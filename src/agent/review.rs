//! Background review at session end.
//!
//! Port of Hermes's `agent/background_review.py`. After every session,
//! a forked agent with limited tools (memory + skill only) reviews the
//! transcript and writes project learnings to MEMORY.md, PITFALLS.md,
//! and skills.
//!
//! The review runs as a fire-and-forget tokio task — it never blocks
//! the main session. If it fails, the error is logged and the session
//! continues unaffected.
//!
//! Key design decisions from Hermes preserved:
//! - Fork, don't inline (separate agent instance, no prompt-cache pollution)
//! - Tool whitelist (only memory + skill tools)
//! - Same credentials as parent session
//! - Frozen conversation snapshot
//! - Fire-and-forget (daemon thread pattern)

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extras::dirge_paths::ProjectPaths;
use crate::provider::AnyAgent;

/// Minimum interval between background reviews (seconds).
const MIN_REVIEW_INTERVAL_SECS: u64 = 900; // 15 minutes

/// Last review timestamp (Unix seconds).
static LAST_REVIEW: AtomicU64 = AtomicU64::new(0);

/// Review prompt focused on project memory and pitfalls.
/// Port of Hermes's `_MEMORY_REVIEW_PROMPT` adapted for coding context.
#[allow(dead_code)]
const MEMORY_REVIEW_PROMPT: &str = r#"Review the conversation above and update project memory.

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools — they are not loaded and will fail.

**MEMORY.md** (project facts, conventions, architecture):
- What build/test commands were discovered or confirmed?
- What naming conventions, file layout patterns, or import styles were used?
- What architecture patterns emerged (how modules relate, error handling style)?
- What library quirks or tool behaviors were discovered?
- Were there any user corrections about how things should be done?

**PITFALLS.md** (anti-patterns and things to avoid):
- Was something tried and failed? Capture what was attempted and WHY it failed.
- Were there environment-specific issues that need documentation?
- Were there test fixtures or mocks that behaved unexpectedly?
- Were there any "gotchas" discovered with the build system or tooling?

For each finding, use the `memory` tool to add an entry. Be specific and actionable — a future session should benefit from what you learned.

"Nothing to save." is valid but should not be the default. Most coding sessions produce at least one learning."#;

/// Review prompt focused on procedural skills.
/// Port of Hermes's `_SKILL_REVIEW_PROMPT` adapted for coding context.
#[allow(dead_code)]
const SKILL_REVIEW_PROMPT: &str = r#"Review the conversation and improve project skills.

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools.

**SKILLS**: procedural improvements.
- Did a skill that was loaded turn out wrong, outdated, or missing steps? PATCH IT NOW using the `skill` tool.
- Did a non-trivial technique, workaround, or debugging workflow emerge from the session?
- Did the user correct your style, approach, or workflow? Embed the lesson.
- Were there test patterns or debugging strategies used successfully?

Preference order for skills:
1. UPDATE a currently-loaded skill (the one in play)
2. UPDATE an existing umbrella skill
3. CREATE a new class-level skill

Start by listing existing skills, then decide what to update or create.

"Nothing to update." is valid but should not be the default."#;

/// Combined review prompt — reviews both memory and skills in one pass.
const COMBINED_REVIEW_PROMPT: &str = r#"Review the conversation above and do TWO things:

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools.

**1. Update MEMORY:**
- What project facts, conventions, or build commands were confirmed?
- What pitfalls or anti-patterns were discovered?
- Any user corrections about how things should be done?

**2. Update SKILLS:**
- Did any loaded skills turn out wrong or outdated? PATCH them.
- Did a non-trivial workflow or debugging strategy emerge? CREATE a skill.
- Did the user correct your approach? Embed that lesson.

Use the `memory` tool to add entries to MEMORY.md (facts) or PITFALLS.md (pitfalls).
Use the `skill` tool to list, view, patch, or create skills.

Be specific and actionable. Future sessions should benefit from what you learned.
"Nothing to save." is valid but should not be the default."#;

/// Spawn a background review task that evaluates the just-completed
/// session and writes learnings to project memory and skills.
///
/// This is fire-and-forget — it runs in a `tokio::spawn` task and
/// returns immediately. Failures are logged to stderr and never
/// block the user.
pub fn spawn_background_review(agent: AnyAgent, _paths: ProjectPaths, transcript: String) {
    // Rate-limit: skip if a review ran recently. Uses atomic
    // compare-and-swap so concurrent Done events from different
    // sessions don't race — only the first one wins.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_REVIEW.load(Ordering::Relaxed);
    if now.saturating_sub(last) < MIN_REVIEW_INTERVAL_SECS {
        tracing::debug!(
            target: "dirge::review",
            elapsed_secs = %(now - last),
            "Skipping background review — last review was too recent"
        );
        return;
    }
    LAST_REVIEW.store(now, Ordering::Relaxed);

    tokio::spawn(async move {
        // Build a review runner with only memory + skill tools.
        let review_runner =
            agent.spawn_review_runner(COMBINED_REVIEW_PROMPT.to_string(), transcript);

        // Drain events. We don't render them — the review runs
        // silently in the background.
        let mut rx = review_runner.event_rx;
        let mut had_error = false;
        while let Some(event) = rx.recv().await {
            use crate::event::AgentEvent;
            match event {
                AgentEvent::Error(msg) => {
                    tracing::warn!(
                        target: "dirge::review",
                        error = %msg,
                        "Background review encountered an error"
                    );
                    had_error = true;
                }
                AgentEvent::Done { .. } => {
                    break;
                }
                _ => {
                    // Tokens, tool calls, etc. — consumed silently.
                }
            }
        }

        if !had_error {
            tracing::info!(
                target: "dirge::review",
                "Background review completed — project knowledge updated"
            );
        }
    });
}

/// Build a human-readable transcript from session messages for
/// background review. Includes user text, assistant text, tool
/// call names+args, and tool results. Compaction summaries are
/// included as system context.
pub fn build_transcript(session: &crate::session::Session) -> String {
    let mut out = String::new();
    for msg in &session.messages {
        match msg.role {
            crate::session::MessageRole::User => {
                out.push_str(&format!("User: {}\n\n", msg.content));
            }
            crate::session::MessageRole::Assistant => {
                if !msg.content.is_empty() {
                    out.push_str(&format!("Assistant: {}\n", msg.content));
                }
                for tc in &msg.tool_calls {
                    let args_str =
                        serde_json::to_string(&tc.args).unwrap_or_else(|_| "{}".to_string());
                    out.push_str(&format!("  [Tool: {}({})]\n", tc.name, args_str));
                    match &tc.state {
                        crate::session::ToolCallState::Completed { result } => {
                            let truncated = truncate_tool_result(result);
                            out.push_str(&format!("  [Result: {}]\n", truncated));
                        }
                        crate::session::ToolCallState::Interrupted => {
                            out.push_str("  [Result: <interrupted>]\n");
                        }
                        crate::session::ToolCallState::Failed { error } => {
                            out.push_str(&format!("  [Result: <failed: {}>]\n", error));
                        }
                    }
                }
                if !msg.content.is_empty() || !msg.tool_calls.is_empty() {
                    out.push('\n');
                }
            }
            crate::session::MessageRole::System => {
                out.push_str(&format!("[System: {}]\n\n", msg.content));
            }
        }
    }
    out
}

fn truncate_tool_result(result: &str) -> String {
    const MAX_TOOL_RESULT: usize = 2000;
    if result.len() <= MAX_TOOL_RESULT {
        result.to_string()
    } else {
        let truncated: String = result.chars().take(MAX_TOOL_RESULT).collect();
        format!("{}… (truncated, {} bytes total)", truncated, result.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{MessageRole, Session, ToolCallEntry, ToolCallState};

    fn make_session() -> Session {
        Session::new("test-provider", "test-model", 128_000)
    }

    #[test]
    fn transcript_includes_user_and_assistant() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "how do I build this?");
        s.add_message(MessageRole::Assistant, "Run cargo build");

        let t = build_transcript(&s);
        assert!(t.contains("User: how do I build this?"));
        assert!(t.contains("Assistant: Run cargo build"));
    }

    #[test]
    fn transcript_includes_tool_calls_and_results() {
        let mut s = make_session();
        s.add_message(MessageRole::User, "read the file");
        let tc = ToolCallEntry {
            id: "call-1".to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/x"}),
            state: ToolCallState::Completed {
                result: "file contents here".to_string(),
            },
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "Let me read that.", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("[Tool: read("));
        assert!(t.contains("[Result: file contents here]"));
    }

    #[test]
    fn transcript_truncates_large_tool_results() {
        let mut s = make_session();
        let big = "x".repeat(3000);
        let tc = ToolCallEntry {
            id: "c1".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({"cmd": "cat big.txt"}),
            state: ToolCallState::Completed {
                result: big.clone(),
            },
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("truncated"));
        assert!(!t.contains(&big));
    }

    #[test]
    fn transcript_includes_system_messages() {
        let mut s = make_session();
        s.add_message(
            MessageRole::System,
            "compaction summary: previous work on auth module",
        );
        s.add_message(MessageRole::User, "continue");

        let t = build_transcript(&s);
        assert!(t.contains("[System: compaction summary"));
        assert!(t.contains("User: continue"));
    }

    #[test]
    fn transcript_handles_interrupted_tool() {
        let mut s = make_session();
        let tc = ToolCallEntry {
            id: "ci".to_string(),
            name: "bash".to_string(),
            args: serde_json::json!({}),
            state: ToolCallState::Interrupted,
        };
        s.add_message_with_tool_calls(MessageRole::Assistant, "", vec![tc]);

        let t = build_transcript(&s);
        assert!(t.contains("<interrupted>"));
    }
}
