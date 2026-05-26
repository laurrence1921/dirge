//! Phase 4.5c — translate `LoopEvent` → `AgentEvent`.
//!
//! Dirge's UI and ACP consume `AgentEvent` (the legacy event
//! vocabulary). The new loop emits `LoopEvent` (the pi-style
//! vocabulary). This module bridges the two so existing
//! consumers can drink from the new loop without rewrites.
//!
//! Translation table:
//!
//! | `LoopEvent`                               | `AgentEvent`(s) emitted          |
//! |-------------------------------------------|----------------------------------|
//! | `AgentStart`                              | (none — dirge has no start event)|
//! | `AgentEnd { messages }`                   | `Done { response, tokens, cost }`|
//! | `TurnStart`                               | `TurnStart { index: counter }`   |
//! | `TurnEnd { message, tool_results }`       | `TurnEnd { index: counter }`     |
//! | `MessageStart { User }`                   | `UserMessage { content }`        |
//! | `MessageStart { Assistant }`              | (none — tokens flow from Update) |
//! | `MessageStart { ToolResult }`             | (none — already via ToolExecutionEnd) |
//! | `MessageStart { Custom }`                 | (none)                           |
//! | `MessageUpdate { TextStart/TextDelta }`   | `Token(delta_chunk)`             |
//! | `MessageUpdate { ThinkingStart/Delta }`   | `Reasoning(delta_chunk)`         |
//! | `MessageUpdate { ToolCall* / *End }`      | (none — covered elsewhere)       |
//! | `MessageEnd`                              | (none — Done finalizes)          |
//! | `ToolExecutionStart { id, name, args }`   | `ToolCall { id, name, args }`    |
//! |                                           | + `ToolStarted { id }`           |
//! | `ToolExecutionUpdate`                     | (none — no AgentEvent equivalent)|
//! | `ToolExecutionEnd { id, name, result }`   | `ToolResult { id, output, kind }`|
//!
//! **State maintained**:
//!   - `turn_index`: increments on each `TurnStart`; used to label
//!     `TurnStart` / `TurnEnd` events.
//!   - `last_text_emitted` / `last_reasoning_emitted`: concatenated
//!     text seen so far per kind. Each `MessageUpdate` carries the
//!     FULL `partial` message; we extract the concatenated text /
//!     reasoning, compute the delta vs last-seen, and emit only the
//!     new chunk. Mirrors how dirge's existing runner emits Token /
//!     Reasoning incrementally.
//!   - `tool_name_by_id`: records tool names at
//!     `ToolExecutionStart` so the matching `ToolResult` can pick
//!     the right `ToolContent` classification (`Text` vs `File`).
//!
//! The bridge is **stateful per-run** — instantiate one per
//! `run_agent_loop` invocation. Feeding events from multiple runs
//! through the same bridge would scramble turn indices and delta
//! tracking.

use std::collections::HashMap;

use compact_str::CompactString;

use crate::event::{AgentEvent, ToolContent};

use super::message::{ContentBlock, DeltaPhase, LoopEvent, LoopMessage, StopReason};

/// Bridges `LoopEvent` stream to `AgentEvent` stream. Stateful
/// per-run.
pub struct EventBridge {
    /// Turn counter. Incremented on each `LoopEvent::TurnStart`.
    /// `AgentEvent::TurnStart`/`TurnEnd` label themselves with
    /// this value.
    turn_index: u32,
    /// Concatenated text content emitted so far across all text
    /// blocks in the current run. Used to compute delta chunks
    /// for `Token` events from `MessageUpdate { TextDelta }`.
    last_text_emitted: String,
    /// Same as `last_text_emitted` but for reasoning / thinking
    /// content. Used for `Reasoning` events.
    last_reasoning_emitted: String,
    /// Tool name lookup at `ToolExecutionStart` time so the
    /// matching `ToolExecutionEnd` can classify the output's
    /// `ToolContent` (Text vs File). Matches the per-name
    /// classification dirge's existing runner uses (read /
    /// find_files / list_dir → File).
    tool_name_by_id: HashMap<String, String>,
}

impl Default for EventBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBridge {
    pub fn new() -> Self {
        Self {
            turn_index: 0,
            last_text_emitted: String::new(),
            last_reasoning_emitted: String::new(),
            tool_name_by_id: HashMap::new(),
        }
    }

    /// Translate one `LoopEvent` to zero-or-more `AgentEvent`s.
    /// Returns a `Vec` because some loop events expand to multiple
    /// dirge events (e.g. `ToolExecutionStart` → `ToolCall` +
    /// `ToolStarted`) and some expand to none (e.g. `AgentStart`).
    pub fn translate(&mut self, event: LoopEvent) -> Vec<AgentEvent> {
        match event {
            LoopEvent::AgentStart => Vec::new(),

            // Context compaction: log the event but no AgentEvent
            // conversion needed — the UI shows a status line when it
            // sees the event. In the future this could emit a
            // dedicated AgentEvent variant for richer UI feedback.
            LoopEvent::ContextCompacted {
                ref new_session_id,
                tokens_before,
                tokens_after,
            } => {
                tracing::info!(
                    target: "dirge::agent_loop",
                    session_id = %new_session_id,
                    tokens_before,
                    tokens_after,
                    "context compacted — session rotated"
                );
                Vec::new()
            }

            LoopEvent::AgentEnd { messages } => {
                // Phase 4.5h-1: classify the run's terminal state
                // by inspecting the LAST assistant message:
                //   - stop_reason=Error + context-length signal
                //     → AgentEvent::ContextOverflow (UI auto-
                //       compacts and respawns)
                //   - stop_reason=Error otherwise → AgentEvent::Error
                //   - stop_reason=Aborted → AgentEvent::Done (the
                //     UI's existing Interjected event covers
                //     graceful aborts elsewhere; emit Done with
                //     empty response so consumers see uniform
                //     terminal events)
                //   - any other stop_reason (Stop, ToolUse, Length)
                //     → AgentEvent::Done with the assembled final
                //     text
                //
                // Pi's agent_end carries newMessages; dirge's Done
                // carries the final response string for the UI's
                // terminal render. Tokens / cost not yet tracked
                // through the loop — surfaced as 0. A future phase
                // could populate from rig usage metadata.
                let last_assistant = messages.iter().rev().find_map(|m| match m {
                    LoopMessage::Assistant(a) => Some(a),
                    _ => None,
                });
                if let Some(a) = last_assistant
                    && matches!(a.stop_reason, StopReason::Error)
                {
                    let error_text = a
                        .error_message
                        .as_deref()
                        .unwrap_or("agent loop produced an error with no message");
                    // Cancellation via the interject channel
                    // (`AbortSignal::cancel()` from
                    // `LoopRunner::into_agent_runner`'s bridge task)
                    // surfaces as an error with this exact message
                    // from `rig_stream.rs:124`. It's NOT a real
                    // failure — it's the user asking to stop. Emit
                    // `Interjected` instead so the UI drains its
                    // queued messages and respawns, rather than
                    // `Error` which would drop them.
                    if error_text.contains("stream aborted by cancellation signal") {
                        let partial_response = a
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        return vec![AgentEvent::Interjected {
                            partial_response: CompactString::from(partial_response),
                            tokens: 0,
                        }];
                    }
                    let kind = crate::agent::recovery::classify_error(error_text);
                    return if matches!(kind, crate::agent::recovery::ErrorKind::ContextLength) {
                        // Extract the user prompt that triggered
                        // this run. Pi's runAgentLoop puts prompts
                        // FIRST in newMessages; new_messages[0]
                        // is the user prompt (or the start of one)
                        // for typical runs. agentLoopContinue
                        // starts new_messages empty — fall back to
                        // empty prompt in that case.
                        let prompt_text = messages
                            .iter()
                            .find_map(|m| match m {
                                LoopMessage::User(u) => Some(u.content.as_str()),
                                _ => None,
                            })
                            .unwrap_or("");
                        vec![AgentEvent::ContextOverflow {
                            prompt: CompactString::from(prompt_text),
                            error: CompactString::from(error_text),
                        }]
                    } else {
                        vec![AgentEvent::Error(CompactString::from(error_text))]
                    };
                }
                let response = messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        LoopMessage::Assistant(a) => Some(
                            a.content
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join(""),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                vec![AgentEvent::Done {
                    response: CompactString::from(response),
                    tokens: 0,
                    cost: 0.0,
                }]
            }

            LoopEvent::TurnStart => {
                let evt = AgentEvent::TurnStart {
                    index: self.turn_index,
                };
                self.turn_index += 1;
                vec![evt]
            }

            LoopEvent::TurnEnd { .. } => {
                // `turn_index` was bumped at TurnStart; current
                // value is the NEXT turn's index. Use
                // `turn_index - 1` for the closing TurnEnd.
                let idx = self.turn_index.saturating_sub(1);
                vec![AgentEvent::TurnEnd { index: idx }]
            }

            LoopEvent::MessageStart { message } => {
                match message {
                    LoopMessage::User(u) => {
                        vec![AgentEvent::UserMessage {
                            content: CompactString::from(u.content),
                        }]
                    }
                    LoopMessage::Custom(payload) => {
                        vec![AgentEvent::CustomMessage {
                            payload: payload.clone(),
                        }]
                    }
                    // Assistant / ToolResult starts don't map to
                    // AgentEvents — token streaming flows from
                    // MessageUpdate, tool results flow from
                    // ToolExecutionEnd.
                    _ => Vec::new(),
                }
            }

            LoopEvent::MessageEnd { message } => {
                let _ = message;
                Vec::new()
            }

            LoopEvent::MessageUpdate { message, phase } => {
                match phase {
                    DeltaPhase::TextStart | DeltaPhase::TextDelta => {
                        // Concatenate all text content across the
                        // partial; compute delta vs last-emitted.
                        let concat: String = message
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if concat.len() > self.last_text_emitted.len()
                            && concat.starts_with(&self.last_text_emitted)
                        {
                            let new_chunk = &concat[self.last_text_emitted.len()..];
                            let chunk = CompactString::from(new_chunk);
                            self.last_text_emitted = concat;
                            vec![AgentEvent::Token(chunk)]
                        } else if concat != self.last_text_emitted {
                            // Defensive: provider re-emitted text
                            // out of order. Emit the FULL concat
                            // as a single Token and resync.
                            let chunk = CompactString::from(concat.as_str());
                            self.last_text_emitted = concat;
                            vec![AgentEvent::Token(chunk)]
                        } else {
                            // No new text in this update.
                            Vec::new()
                        }
                    }
                    DeltaPhase::ThinkingStart | DeltaPhase::ThinkingDelta => {
                        let concat: String = message
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Thinking { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if concat.len() > self.last_reasoning_emitted.len()
                            && concat.starts_with(&self.last_reasoning_emitted)
                        {
                            let new_chunk = &concat[self.last_reasoning_emitted.len()..];
                            let chunk = CompactString::from(new_chunk);
                            self.last_reasoning_emitted = concat;
                            vec![AgentEvent::Reasoning(chunk)]
                        } else if concat != self.last_reasoning_emitted {
                            let chunk = CompactString::from(concat.as_str());
                            self.last_reasoning_emitted = concat;
                            vec![AgentEvent::Reasoning(chunk)]
                        } else {
                            Vec::new()
                        }
                    }
                    // *End markers: content already emitted via
                    // the corresponding Delta events. No-op.
                    DeltaPhase::TextEnd
                    | DeltaPhase::ThinkingEnd
                    | DeltaPhase::ToolCallStart
                    | DeltaPhase::ToolCallDelta
                    | DeltaPhase::ToolCallEnd => Vec::new(),
                }
            }

            LoopEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                // Remember the name so the matching
                // ToolExecutionEnd can classify the output.
                self.tool_name_by_id
                    .insert(tool_call_id.clone(), tool_name.clone());
                // Pi/dirge fires both `ToolCall` AND `ToolStarted`
                // consecutively. ToolCall = "LLM emitted the
                // call", ToolStarted = "dispatch is imminent".
                // For the new loop these collapse to one event
                // since `tool_execution_start` IS dispatch-imminent.
                // We still emit both for back-compat with existing
                // UI / ACP consumers that distinguish them.
                vec![
                    AgentEvent::ToolCall {
                        id: CompactString::from(tool_call_id.clone()),
                        name: CompactString::from(tool_name),
                        args,
                    },
                    AgentEvent::ToolStarted {
                        id: CompactString::from(tool_call_id),
                    },
                ]
            }

            LoopEvent::ToolExecutionUpdate { .. } => {
                // Dirge has no `ToolUpdate` AgentEvent. Phase 6
                // hardening could add one if a real UI use case
                // emerges. For now: silent.
                Vec::new()
            }

            LoopEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name: _,
                result,
                is_error: _,
            } => {
                // Convert the LoopToolResult's content to a
                // single string (the LLM-facing payload). Pi's
                // content is `Vec<Value>` with text/image blocks;
                // dirge's AgentEvent::ToolResult is a flat
                // CompactString.
                let output = flatten_content(&result.content);
                let name = self.tool_name_by_id.remove(&tool_call_id);
                let kind = classify_tool(name.as_deref());
                vec![AgentEvent::ToolResult {
                    id: CompactString::from(tool_call_id),
                    output: CompactString::from(output),
                    kind,
                }]
            }
        }
    }
}

/// Flatten the `Vec<Value>` content blocks of a `LoopToolResult`
/// into a single string. Matches dirge's existing runner shape
/// (`AgentEvent::ToolResult.output: CompactString`).
///
/// Recognises `{type: "text", text: "..."}` blocks. Anything else
/// is JSON-stringified — better than dropping the data.
fn flatten_content(content: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for block in content {
        if let Some(obj) = block.as_object()
            && obj.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = obj.get("text").and_then(|t| t.as_str())
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
            continue;
        }
        // Fallback: stringify. Image / other types end up as
        // JSON for now — opaque but not lost.
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&block.to_string());
    }
    out
}

/// Classify a tool name into `ToolContent::Text` vs
/// `ToolContent::File`. Matches dirge's existing runner.rs
/// classification (read / find_files / list_dir → File; everything
/// else → Text).
fn classify_tool(name: Option<&str>) -> ToolContent {
    match name {
        Some("read") | Some("find_files") | Some("list_dir") => ToolContent::File,
        _ => ToolContent::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::{
        AssistantMessage, ContentBlock, StopReason, ToolResultMessage, UserMessage,
    };
    use crate::agent::agent_loop::result::LoopToolResult;

    /// Convenience: build a partial assistant message with a single
    /// text block.
    fn assistant_with_text(s: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: s.to_string(),
            }],
            StopReason::Stop,
        )
    }

    /// Convenience: assistant with thinking content.
    fn assistant_with_thinking(s: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Thinking {
                text: s.to_string(),
            }],
            StopReason::Stop,
        )
    }

    /// `TurnStart` increments the counter; the emitted event
    /// carries the value PRIOR to the increment (so the first
    /// TurnStart is index 0). `TurnEnd` matches.
    #[test]
    fn turn_start_end_index_round_trips() {
        let mut bridge = EventBridge::new();
        let s0 = bridge.translate(LoopEvent::TurnStart);
        let e0 = bridge.translate(LoopEvent::TurnEnd {
            message: assistant_with_text("hi"),
            tool_results: Vec::new(),
        });
        let s1 = bridge.translate(LoopEvent::TurnStart);
        let e1 = bridge.translate(LoopEvent::TurnEnd {
            message: assistant_with_text("again"),
            tool_results: Vec::new(),
        });

        assert!(matches!(
            s0.as_slice(),
            [AgentEvent::TurnStart { index: 0 }]
        ));
        assert!(matches!(e0.as_slice(), [AgentEvent::TurnEnd { index: 0 }]));
        assert!(matches!(
            s1.as_slice(),
            [AgentEvent::TurnStart { index: 1 }]
        ));
        assert!(matches!(e1.as_slice(), [AgentEvent::TurnEnd { index: 1 }]));
    }

    /// `AgentStart` is a no-op — dirge has no equivalent event.
    /// `AgentEnd` produces `Done` with the final assistant text.
    #[test]
    fn agent_start_no_op_agent_end_emits_done() {
        let mut bridge = EventBridge::new();
        assert!(bridge.translate(LoopEvent::AgentStart).is_empty());

        let messages = vec![
            LoopMessage::User(UserMessage {
                content: "hi".to_string(),
            }),
            LoopMessage::Assistant(assistant_with_text("final answer")),
        ];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Done {
                response,
                tokens,
                cost,
            } => {
                assert_eq!(response.as_str(), "final answer");
                assert_eq!(*tokens, 0);
                assert_eq!(*cost, 0.0);
            }
            _ => panic!("expected Done"),
        }
    }

    /// Phase 4.5h-1: AgentEnd carrying an assistant message with
    /// stop_reason=Error + a context-length error string →
    /// `AgentEvent::ContextOverflow` (not `Done` or `Error`).
    /// UI consumes ContextOverflow by running `/compress` and
    /// respawning a fresh runner with the same prompt against
    /// the compacted history.
    #[test]
    fn agent_end_context_length_error_emits_context_overflow() {
        let mut bridge = EventBridge::new();
        let mut a = assistant_with_text("");
        a.stop_reason = StopReason::Error;
        a.error_message = Some("prompt is too long: maximum context length exceeded".to_string());
        let messages = vec![
            LoopMessage::User(UserMessage {
                content: "summarize this huge doc".to_string(),
            }),
            LoopMessage::Assistant(a),
        ];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::ContextOverflow { prompt, error } => {
                assert_eq!(prompt.as_str(), "summarize this huge doc");
                assert!(
                    error.contains("context length") || error.contains("too long"),
                    "error text should mention context length"
                );
            }
            other => panic!("expected ContextOverflow, got {other:?}"),
        }
    }

    /// Phase 4.5h-1: AgentEnd with stop_reason=Error but NOT a
    /// context-length signal → `AgentEvent::Error`.
    #[test]
    fn agent_end_non_context_error_emits_error() {
        let mut bridge = EventBridge::new();
        let mut a = assistant_with_text("");
        a.stop_reason = StopReason::Error;
        a.error_message = Some("401 unauthorized: invalid api key".to_string());
        let messages = vec![
            LoopMessage::User(UserMessage {
                content: "hi".to_string(),
            }),
            LoopMessage::Assistant(a),
        ];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Error(msg) => {
                assert!(msg.contains("unauthorized"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Interject channel cancellation surfaces as a stream Error
    /// with the message "stream aborted by cancellation signal"
    /// (rig_stream.rs:124). The bridge must recognise this as a
    /// graceful interjection — emit `Interjected` so the UI drains
    /// its queued messages, NOT `Error` which would drop them
    /// (the user's bug report on this).
    #[test]
    fn agent_end_cancellation_emits_interjected_not_error() {
        let mut bridge = EventBridge::new();
        let mut a = assistant_with_text("partial response before interject");
        a.stop_reason = StopReason::Error;
        a.error_message = Some("stream aborted by cancellation signal".to_string());
        let messages = vec![
            LoopMessage::User(UserMessage {
                content: "do a long thing".to_string(),
            }),
            LoopMessage::Assistant(a),
        ];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Interjected {
                partial_response,
                tokens,
            } => {
                assert_eq!(
                    partial_response.as_str(),
                    "partial response before interject"
                );
                assert_eq!(*tokens, 0);
            }
            other => panic!("expected Interjected, got {other:?}"),
        }
    }

    /// Phase 4.5h-1: AgentEnd with stop_reason=Error but no
    /// error_message → still emits Error with a placeholder
    /// message. Defensive — error variant should never produce
    /// a misleading Done.
    #[test]
    fn agent_end_error_without_message_still_emits_error() {
        let mut bridge = EventBridge::new();
        let mut a = assistant_with_text("");
        a.stop_reason = StopReason::Error;
        a.error_message = None;
        let messages = vec![LoopMessage::Assistant(a)];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], AgentEvent::Error(_)));
    }

    /// AgentEnd with stop_reason=Aborted → Done with empty
    /// response. Graceful abort doesn't produce an error event.
    /// Interjected (the UI's "user said stop") is a runner-level
    /// concern handled separately.
    #[test]
    fn agent_end_aborted_emits_done() {
        let mut bridge = EventBridge::new();
        let mut a = assistant_with_text("partial work");
        a.stop_reason = StopReason::Aborted;
        let messages = vec![LoopMessage::Assistant(a)];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        assert_eq!(out.len(), 1);
        // Done — the loop ended (no Error). Response field carries
        // whatever text had assembled.
        match &out[0] {
            AgentEvent::Done { response, .. } => {
                assert_eq!(response.as_str(), "partial work");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// `AgentEnd` with no assistant message → `Done` with empty
    /// response.
    #[test]
    fn agent_end_no_assistant_done_empty_response() {
        let mut bridge = EventBridge::new();
        let messages = vec![LoopMessage::User(UserMessage {
            content: "hi".to_string(),
        })];
        let out = bridge.translate(LoopEvent::AgentEnd { messages });
        match &out[0] {
            AgentEvent::Done { response, .. } => {
                assert_eq!(response.as_str(), "");
            }
            _ => panic!("expected Done"),
        }
    }

    /// `MessageUpdate { TextStart }` emits a single `Token` with
    /// the initial text chunk. The bridge tracks this as
    /// "last_text_emitted" so subsequent deltas emit only the new
    /// portion.
    #[test]
    fn text_delta_emits_token_chunks() {
        let mut bridge = EventBridge::new();
        // First chunk: "Hello"
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("Hello"),
            phase: DeltaPhase::TextStart,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Token(s) => assert_eq!(s.as_str(), "Hello"),
            _ => panic!("expected Token"),
        }
        // Second chunk: "Hello world" (provider appended " world")
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("Hello world"),
            phase: DeltaPhase::TextDelta,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Token(s) => assert_eq!(s.as_str(), " world"),
            _ => panic!("expected Token"),
        }
        // Third update with no new text → no event.
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("Hello world"),
            phase: DeltaPhase::TextDelta,
        });
        assert!(out.is_empty());
    }

    /// `MessageUpdate` for reasoning produces `Reasoning` events
    /// using the same delta tracking.
    #[test]
    fn reasoning_delta_emits_reasoning_chunks() {
        let mut bridge = EventBridge::new();
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_thinking("Let me think"),
            phase: DeltaPhase::ThinkingStart,
        });
        match &out[0] {
            AgentEvent::Reasoning(s) => assert_eq!(s.as_str(), "Let me think"),
            _ => panic!("expected Reasoning"),
        }
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_thinking("Let me think about this"),
            phase: DeltaPhase::ThinkingDelta,
        });
        match &out[0] {
            AgentEvent::Reasoning(s) => assert_eq!(s.as_str(), " about this"),
            _ => panic!("expected Reasoning"),
        }
    }

    /// `MessageUpdate` with text and reasoning interleaved tracks
    /// them independently. Verifies the per-kind delta state.
    #[test]
    fn text_and_reasoning_tracked_independently() {
        let mut bridge = EventBridge::new();
        // Reasoning arrives first.
        let _ = bridge.translate(LoopEvent::MessageUpdate {
            message: AssistantMessage::new(
                vec![ContentBlock::Thinking {
                    text: "thinking".to_string(),
                }],
                StopReason::Stop,
            ),
            phase: DeltaPhase::ThinkingStart,
        });
        // Then text arrives in a separate block.
        let out = bridge.translate(LoopEvent::MessageUpdate {
            message: AssistantMessage::new(
                vec![
                    ContentBlock::Thinking {
                        text: "thinking".to_string(),
                    },
                    ContentBlock::Text {
                        text: "answer".to_string(),
                    },
                ],
                StopReason::Stop,
            ),
            phase: DeltaPhase::TextStart,
        });
        // Token for "answer", not for the previously-emitted
        // "thinking".
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::Token(s) => assert_eq!(s.as_str(), "answer"),
            _ => panic!("expected Token"),
        }
    }

    /// `ToolExecutionStart` produces TWO events: `ToolCall` then
    /// `ToolStarted`. Bridge also records the name for later
    /// classification.
    #[test]
    fn tool_execution_start_emits_call_and_started() {
        let mut bridge = EventBridge::new();
        let out = bridge.translate(LoopEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/x"}),
        });
        assert_eq!(out.len(), 2);
        match &out[0] {
            AgentEvent::ToolCall { id, name, args } => {
                assert_eq!(id.as_str(), "call-1");
                assert_eq!(name.as_str(), "read");
                assert_eq!(args["path"], "/tmp/x");
            }
            _ => panic!("expected ToolCall"),
        }
        match &out[1] {
            AgentEvent::ToolStarted { id } => {
                assert_eq!(id.as_str(), "call-1");
            }
            _ => panic!("expected ToolStarted"),
        }
    }

    /// `ToolExecutionEnd` → `ToolResult` with `ToolContent::File`
    /// for tool names that surface file refs.
    #[test]
    fn tool_execution_end_classifies_file_tools_as_file() {
        let mut bridge = EventBridge::new();
        // Record the tool name first.
        let _ = bridge.translate(LoopEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({}),
        });
        let out = bridge.translate(LoopEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            result: LoopToolResult {
                content: vec![serde_json::json!({
                    "type": "text",
                    "text": "file contents here"
                })],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        });
        assert_eq!(out.len(), 1);
        match &out[0] {
            AgentEvent::ToolResult { id, output, kind } => {
                assert_eq!(id.as_str(), "call-1");
                assert_eq!(output.as_str(), "file contents here");
                assert!(matches!(kind, ToolContent::File));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    /// `ToolExecutionEnd` → `ToolResult` with `ToolContent::Text`
    /// for tools that aren't in the file-classification set.
    #[test]
    fn tool_execution_end_classifies_other_tools_as_text() {
        let mut bridge = EventBridge::new();
        let _ = bridge.translate(LoopEvent::ToolExecutionStart {
            tool_call_id: "call-2".to_string(),
            tool_name: "bash".to_string(),
            args: serde_json::json!({}),
        });
        let out = bridge.translate(LoopEvent::ToolExecutionEnd {
            tool_call_id: "call-2".to_string(),
            tool_name: "bash".to_string(),
            result: LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "stdout"})],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        });
        match &out[0] {
            AgentEvent::ToolResult { kind, .. } => {
                assert!(matches!(kind, ToolContent::Text));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    /// `MessageStart` / `MessageEnd` are no-ops at the AgentEvent
    /// boundary (dirge handles user-message rendering / done
    /// `LoopMessage::Custom` flowing through `MessageStart` becomes
    /// an `AgentEvent::CustomMessage` carrying the same JSON payload
    /// — the bridge keeps the payload opaque so the UI's renderer
    /// lookup gets the full structure.
    #[test]
    fn message_start_custom_emits_custom_message_event() {
        let mut bridge = EventBridge::new();
        let payload = serde_json::json!({"type": "status", "content": "hello"});
        let events = bridge.translate(LoopEvent::MessageStart {
            message: LoopMessage::Custom(payload.clone()),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::CustomMessage { payload: out } => assert_eq!(out, &payload),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// `MessageStart` for User messages now emits `UserMessage`
    /// so steering-injected user messages are displayed in the UI
    /// log. ToolResult and Assistant MessageStart remain no-ops.
    /// `MessageEnd` is always a no-op.
    #[test]
    fn message_start_end_behavior() {
        let mut bridge = EventBridge::new();
        let user_msg = LoopMessage::User(UserMessage {
            content: "hi".to_string(),
        });
        // User messages now emit UserMessage.
        let events = bridge.translate(LoopEvent::MessageStart {
            message: user_msg.clone(),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::UserMessage { content } => assert_eq!(content.as_str(), "hi"),
            other => panic!("expected UserMessage, got {other:?}"),
        }
        // MessageEnd is still a no-op for user messages.
        assert!(bridge
            .translate(LoopEvent::MessageEnd {
                message: user_msg
            })
            .is_empty());

        // ToolResult MessageStart/End remain no-ops.
        let tool_msg = LoopMessage::ToolResult(ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "echo".to_string(),
            content: Vec::new(),
            details: serde_json::Value::Null,
            is_error: false,
        });
        assert!(bridge
            .translate(LoopEvent::MessageStart {
                message: tool_msg.clone()
            })
            .is_empty());
        assert!(bridge
            .translate(LoopEvent::MessageEnd {
                message: tool_msg
            })
            .is_empty());
    }

    /// `MessageUpdate` with End-phase markers are no-ops (the
    /// content was already streamed via the corresponding Delta).
    #[test]
    fn message_update_end_phases_are_no_ops() {
        let mut bridge = EventBridge::new();
        for phase in [
            DeltaPhase::TextEnd,
            DeltaPhase::ThinkingEnd,
            DeltaPhase::ToolCallStart,
            DeltaPhase::ToolCallDelta,
            DeltaPhase::ToolCallEnd,
        ] {
            let out = bridge.translate(LoopEvent::MessageUpdate {
                message: assistant_with_text("any"),
                phase,
            });
            assert!(out.is_empty(), "phase {phase:?} should be no-op");
        }
    }

    /// `flatten_content` joins multiple text blocks with newlines.
    /// Image / other-type blocks fall back to JSON stringify (not
    /// dropped).
    #[test]
    fn flatten_content_joins_text_blocks() {
        let blocks = vec![
            serde_json::json!({"type": "text", "text": "line 1"}),
            serde_json::json!({"type": "text", "text": "line 2"}),
        ];
        assert_eq!(flatten_content(&blocks), "line 1\nline 2");
    }

    /// `flatten_content` falls back to JSON stringify for
    /// non-text blocks. Preserves the data so consumers can
    /// inspect.
    #[test]
    fn flatten_content_stringifies_unknown_blocks() {
        let blocks = vec![
            serde_json::json!({"type": "text", "text": "hello"}),
            serde_json::json!({"type": "image", "url": "https://example/x.png"}),
        ];
        let out = flatten_content(&blocks);
        assert!(out.contains("hello"));
        assert!(out.contains("image"));
    }

    /// Bridge can be reused across hand-crafted event sequences
    /// without polluting state between unrelated calls — except
    /// for the documented per-run state (turn_index, last_text).
    /// This test confirms state is properly threaded through a
    /// realistic full-run event sequence (TurnStart → tokens →
    /// tool call → tool result → tokens → TurnEnd → AgentEnd).
    #[test]
    fn full_run_event_sequence_translates_correctly() {
        let mut bridge = EventBridge::new();
        let mut all = Vec::new();

        all.extend(bridge.translate(LoopEvent::AgentStart));
        all.extend(bridge.translate(LoopEvent::TurnStart));
        all.extend(bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("Sure, "),
            phase: DeltaPhase::TextStart,
        }));
        all.extend(bridge.translate(LoopEvent::MessageUpdate {
            message: assistant_with_text("Sure, I'll help."),
            phase: DeltaPhase::TextDelta,
        }));
        all.extend(bridge.translate(LoopEvent::ToolExecutionStart {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/x"}),
        }));
        all.extend(bridge.translate(LoopEvent::ToolExecutionEnd {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            result: LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "data"})],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        }));
        all.extend(bridge.translate(LoopEvent::TurnEnd {
            message: assistant_with_text("Sure, I'll help."),
            tool_results: Vec::new(),
        }));
        all.extend(bridge.translate(LoopEvent::AgentEnd {
            messages: vec![LoopMessage::Assistant(assistant_with_text(
                "final response",
            ))],
        }));

        // Expected sequence: TurnStart(0), Token("Sure, "),
        // Token("I'll help."), ToolCall, ToolStarted, ToolResult,
        // TurnEnd(0), Done.
        let kinds: Vec<_> = all
            .iter()
            .map(|e| match e {
                AgentEvent::Token(_) => "Token",
                AgentEvent::Reasoning(_) => "Reasoning",
                AgentEvent::ToolCall { .. } => "ToolCall",
                AgentEvent::ToolStarted { .. } => "ToolStarted",
                AgentEvent::ToolResult { .. } => "ToolResult",
                AgentEvent::TurnStart { .. } => "TurnStart",
                AgentEvent::TurnEnd { .. } => "TurnEnd",
                AgentEvent::Done { .. } => "Done",
                AgentEvent::Error(_) => "Error",
                AgentEvent::ContextOverflow { .. } => "ContextOverflow",
                AgentEvent::Interjected { .. } => "Interjected",
                AgentEvent::CustomMessage { .. } => "CustomMessage",
                AgentEvent::UserMessage { .. } => "UserMessage",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "TurnStart",
                "Token",
                "Token",
                "ToolCall",
                "ToolStarted",
                "ToolResult",
                "TurnEnd",
                "Done",
            ]
        );
    }
}
