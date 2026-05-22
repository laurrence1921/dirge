//! `run_loop`, `run_agent_loop`, `run_agent_loop_continue` —
//! THE KEYSTONE.
//!
//! Faithful port of pi's `runLoop` (agent-loop.ts:155-269) plus
//! the two public entry points `runAgentLoop` (95-118) and
//! `runAgentLoopContinue` (120-143).
//!
//! Pi's algorithm in one pass (the bones we replicate):
//!
//! ```text
//! runLoop(currentContext, newMessages, config, signal, emit, streamFn):
//!   first_turn = true
//!   pending_messages = getSteeringMessages?() || []
//!
//!   OUTER:
//!     has_more_tool_calls = true
//!     INNER while has_more_tool_calls OR pending_messages not empty:
//!       if !first_turn: emit turn_start; else first_turn = false
//!       inject pending_messages into context + newMessages; emit
//!         message_start + message_end for each
//!       msg = streamAssistantResponse(...)
//!       newMessages.push(msg)
//!       if msg.stopReason in [error, aborted]:
//!         emit turn_end (toolResults=[]); emit agent_end; return
//!       tool_calls = filter msg.content for type=toolCall
//!       tool_results = []; has_more_tool_calls = false
//!       if tool_calls non-empty:
//!         batch = executeToolCalls(...)
//!         tool_results = batch.messages
//!         has_more_tool_calls = !batch.terminate
//!         push each tool_result to context + newMessages
//!       emit turn_end (msg, tool_results)
//!       snapshot = prepareNextTurn?(ctx)
//!       if snapshot: context = ?? newCtx, model = ?? newModel, ...
//!       if shouldStopAfterTurn?(ctx): emit agent_end; return
//!       pending_messages = getSteeringMessages?() || []
//!     // INNER end
//!     follow_up = getFollowUpMessages?() || []
//!     if follow_up non-empty: pending_messages = follow_up; continue OUTER
//!     break OUTER
//!   emit agent_end
//! ```

use serde_json::Value;
use tokio::sync::mpsc;

use super::message::{
    AssistantMessage, ContentBlock, LoopEvent, LoopMessage, StopReason, ToolResultMessage,
};
use super::stream::{StreamFn, stream_assistant_response};
use super::tool::AbortSignal;
use super::tools::execute_tool_calls;
use super::types::{Context, LoopConfig};

/// Errors from `run_agent_loop_continue`. Pi throws synchronously
/// (agent-loop.ts:71-76, 128-133) — we return `Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopError {
    /// `agentLoopContinue` invoked with no context messages. Pi
    /// throws "Cannot continue: no messages in context".
    NoMessages,
    /// `agentLoopContinue` invoked with the LAST context message
    /// having role=assistant. Pi throws "Cannot continue from
    /// message role: assistant".
    CannotContinueFromAssistant,
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopError::NoMessages => write!(f, "Cannot continue: no messages in context"),
            LoopError::CannotContinueFromAssistant => {
                write!(f, "Cannot continue from message role: assistant")
            }
        }
    }
}

impl std::error::Error for LoopError {}

/// Public entry point: start a new run from one or more prompt
/// messages. Faithful port of pi `runAgentLoop` (agent-loop.ts:95).
///
/// Emits `agent_start` + `turn_start`, then `message_start` /
/// `message_end` for each prompt, THEN enters `run_loop`. Returns
/// the full list of messages produced by this run (prompts + every
/// assistant turn + every tool result).
pub async fn run_agent_loop(
    prompts: Vec<LoopMessage>,
    mut context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    // Pi line 103: `newMessages = [...prompts]`.
    let new_messages = prompts.clone();
    // Pi line 105: `currentContext.messages = [...context.messages, ...prompts]`.
    for prompt in &prompts {
        context.messages.push(loop_message_to_value(prompt));
    }

    // Pi lines 109-114: emit agent_start + turn_start + per-prompt
    // start/end pair.
    let _ = emit.send(LoopEvent::AgentStart).await;
    let _ = emit.send(LoopEvent::TurnStart).await;
    for prompt in &prompts {
        let _ = emit
            .send(LoopEvent::MessageStart {
                message: prompt.clone(),
            })
            .await;
        let _ = emit
            .send(LoopEvent::MessageEnd {
                message: prompt.clone(),
            })
            .await;
    }

    run_loop(context, new_messages, config, signal, emit, stream_fn).await
}

/// Public entry point: continue an EXISTING run from the current
/// context (last message must be a user / toolResult / custom —
/// NOT assistant). Faithful port of pi `runAgentLoopContinue`
/// (agent-loop.ts:120).
///
/// Pi semantics:
///   - empty context → throw
///   - last message is assistant → throw
///   - otherwise → emit agent_start + turn_start, enter loop with
///     newMessages = [] (does NOT re-emit user-message events)
pub async fn run_agent_loop_continue(
    context: Context,
    config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Result<Vec<LoopMessage>, LoopError> {
    // Pi lines 71-76: empty context check.
    if context.messages.is_empty() {
        return Err(LoopError::NoMessages);
    }
    // Pi lines 131-133: last-message role check. Phase 4 reads
    // the role string from the placeholder `Vec<Value>` shape
    // since that's what context.messages carries. Phase ??? may
    // substitute typed messages.
    let last_role = context
        .messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("");
    if last_role == "assistant" {
        return Err(LoopError::CannotContinueFromAssistant);
    }

    // Pi lines 135-139: newMessages = []; emit agent_start +
    // turn_start; enter loop.
    let new_messages: Vec<LoopMessage> = Vec::new();
    let _ = emit.send(LoopEvent::AgentStart).await;
    let _ = emit.send(LoopEvent::TurnStart).await;

    Ok(run_loop(context, new_messages, config, signal, emit, stream_fn).await)
}

/// The actual loop. Faithful port of pi `runLoop` (agent-loop.ts:155-269).
///
/// Owns `current_context`, `new_messages`, `config` — pi mutates
/// these as the run proceeds; in Rust we own them by value and
/// return `new_messages` at the end.
pub async fn run_loop(
    mut current_context: Context,
    mut new_messages: Vec<LoopMessage>,
    // `config` is `mut` even though phase 4 only reads it. Pi
    // mutates it at agent-loop.ts:229 (`config = { ...config,
    // model: ..., reasoning: ... }`) for the prepareNextTurn
    // model/thinking swap. Phase 4 lands the hook signature and
    // the placeholder fields; phase 4.5 will actually assign
    // through this binding. Keeping `mut` here matches pi's
    // shape and avoids needing to retype the parameter when the
    // assignment site activates.
    #[allow(unused_mut)] mut config: LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> Vec<LoopMessage> {
    let mut first_turn = true;

    // Pi line 167: initial steering poll.
    let mut pending_messages: Vec<LoopMessage> = match &config.get_steering_messages {
        Some(get) => get().await,
        None => Vec::new(),
    };

    'outer: loop {
        let mut has_more_tool_calls = true;

        // Pi line 174: INNER LOOP.
        while has_more_tool_calls || !pending_messages.is_empty() {
            // Pi lines 175-179: turn_start (skipped on very first
            // iteration — the outer wrapper already emitted it).
            if !first_turn {
                let _ = emit.send(LoopEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // Pi lines 181-189: inject pending steering / follow-up
            // messages.
            if !pending_messages.is_empty() {
                for msg in &pending_messages {
                    let _ = emit
                        .send(LoopEvent::MessageStart {
                            message: msg.clone(),
                        })
                        .await;
                    let _ = emit
                        .send(LoopEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                    current_context.messages.push(loop_message_to_value(msg));
                    new_messages.push(msg.clone());
                }
                pending_messages.clear();
            }

            // Pi lines 192-194: LLM call.
            let assistant_msg = stream_assistant_response(
                &mut current_context,
                &config,
                signal.clone(),
                emit,
                stream_fn,
            )
            .await;
            new_messages.push(LoopMessage::Assistant(assistant_msg.clone()));

            // Pi lines 196-200: error / aborted short-circuit.
            if matches!(
                assistant_msg.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                let _ = emit
                    .send(LoopEvent::TurnEnd {
                        message: assistant_msg.clone(),
                        tool_results: Vec::new(),
                    })
                    .await;
                let _ = emit
                    .send(LoopEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                return new_messages;
            }

            // Pi lines 202-216: tool calls + results.
            let tool_calls = extract_tool_calls_from(&assistant_msg);
            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch =
                    execute_tool_calls(&current_context, &assistant_msg, &config, &signal, emit)
                        .await;
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;
                for result in &tool_results {
                    current_context.messages.push(tool_result_to_value(result));
                    new_messages.push(LoopMessage::ToolResult(result.clone()));
                }
            }

            // Pi line 218: turn_end.
            let _ = emit
                .send(LoopEvent::TurnEnd {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                })
                .await;

            // Pi lines 220-239: prepareNextTurn.
            if let Some(hook) = &config.prepare_next_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if let Some(update) = hook(hook_ctx).await {
                    // Pi line 228: `context: snapshot.context ??
                    // currentContext`. Apply only `Some`.
                    if let Some(new_ctx) = update.context {
                        current_context = new_ctx;
                    }
                    // Pi lines 229-238 rebuild config with the
                    // new model / reasoning. Doing that in Rust
                    // requires re-building the `StreamFn` closure
                    // (which has the CompletionModel baked in at
                    // construction by `rig_stream_fn_from_model`).
                    // The StreamFn isn't part of LoopConfig — it's
                    // passed to `run_loop` separately — so we
                    // can't swap it mid-run without restructuring
                    // the loop's surface.
                    //
                    // Surface a warning so users wiring this hook
                    // know their swap was ignored. Code-review
                    // gap #3: lift this when a real consumer
                    // needs mid-run model swap; the fix is to
                    // accept a `Fn(Context) -> StreamFn` factory
                    // instead of a single StreamFn.
                    if let Some(model) = &update.model {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            requested_model = %model,
                            "prepareNextTurn returned a new model but mid-run swap is not yet wired — ignoring",
                        );
                    }
                    if let Some(level) = &update.thinking_level {
                        tracing::warn!(
                            target: "dirge::agent_loop",
                            requested_thinking = ?level,
                            "prepareNextTurn returned a new thinking_level but mid-run swap is not yet wired — ignoring",
                        );
                    }
                }
            }

            // Pi lines 241-251: shouldStopAfterTurn.
            if let Some(hook) = &config.should_stop_after_turn {
                let hook_ctx = super::hooks::TurnHookContext {
                    message: assistant_msg.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                };
                if hook(hook_ctx).await {
                    let _ = emit
                        .send(LoopEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await;
                    return new_messages;
                }
            }

            // Pi line 253: refresh steering for next iteration.
            pending_messages = match &config.get_steering_messages {
                Some(get) => get().await,
                None => Vec::new(),
            };
        }
        // INNER END

        // Pi lines 256-262: outer-loop follow-up poll.
        let follow_up = match &config.get_followup_messages {
            Some(get) => get().await,
            None => Vec::new(),
        };
        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue 'outer;
        }
        break;
    }

    // Pi line 268: final agent_end.
    let _ = emit
        .send(LoopEvent::AgentEnd {
            messages: new_messages.clone(),
        })
        .await;
    new_messages
}

/// Local extract — same as `tools::extract_tool_calls`. Kept
/// inline so `run.rs` doesn't reach into `tools` for tiny helpers.
fn extract_tool_calls_from(msg: &AssistantMessage) -> Vec<super::tools::ToolCall> {
    super::tools::extract_tool_calls(msg)
}

/// Convert a `LoopMessage` to the placeholder `Value` shape used
/// in `Context.messages`. Mirrors `serialize_assistant` from
/// stream.rs but covers every variant.
///
/// Phase 4 placeholder — phase ??? swaps the Vec<Value> for typed
/// messages and this helper goes away.
fn loop_message_to_value(msg: &LoopMessage) -> Value {
    match msg {
        LoopMessage::User(u) => serde_json::json!({
            "role": "user",
            "content": u.content,
        }),
        LoopMessage::Assistant(a) => serde_json::json!({
            "role": "assistant",
            "content": a.content,
            "stopReason": a.stop_reason,
            "errorMessage": a.error_message,
        }),
        LoopMessage::ToolResult(t) => tool_result_to_value(t),
        LoopMessage::Custom(v) => v.clone(),
    }
}

fn tool_result_to_value(t: &ToolResultMessage) -> Value {
    serde_json::json!({
        "role": "toolResult",
        "toolCallId": t.tool_call_id,
        "toolName": t.tool_name,
        "content": t.content,
        "details": t.details,
        "isError": t.is_error,
    })
}

// =====================================================================
// Tests — ported from pi/test/agent-loop.test.ts
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::hooks::{
        AfterToolCallContext, AfterToolCallFn, GetSteeringMessagesFn, PrepareNextTurnFn,
        ShouldStopAfterTurnFn,
    };
    use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
    use crate::agent::agent_loop::result::AfterToolCallResult;
    use crate::agent::agent_loop::stream::StreamFn;
    use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
    use crate::agent::agent_loop::types::{
        ConvertToLlmFn, LoopConfig, ToolExecutionMode, TurnUpdate,
    };
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Build a stream factory that returns canned assistant
    /// messages in sequence. Mirrors pi's typical test mock —
    /// `callIndex` increments per invocation; each call returns
    /// the next canned response.
    ///
    /// `responses` is a Vec; index N is returned on the (N+1)th
    /// call. Past the end → final fallback message with
    /// stopReason=Stop.
    fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let responses = std::sync::Arc::new(responses);
        std::sync::Arc::new(move |_ctx, _opts| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = responses.get(n).cloned().unwrap_or_else(|| {
                AssistantMessage::new(
                    vec![ContentBlock::Text {
                        text: "end".to_string(),
                    }],
                    StopReason::Stop,
                )
            });
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
            }]))
        })
    }

    fn identity_converter() -> ConvertToLlmFn {
        std::sync::Arc::new(|messages: &[Value]| {
            messages
                .iter()
                .filter(|m| {
                    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    matches!(role, "user" | "assistant" | "toolResult")
                })
                .cloned()
                .collect()
        })
    }

    fn build_config() -> LoopConfig {
        LoopConfig {
            convert_to_llm: identity_converter(),
            transform_context: None,
            get_api_key: None,
            api_key: None,
            tool_execution: ToolExecutionMode::Sequential,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            get_steering_messages: None,
            get_followup_messages: None,
            reasoning: None,
            thinking_budgets: None,
            headers: std::collections::HashMap::new(),
            metadata: std::collections::HashMap::new(),
            request_timeout: None,
            provider_name: None,
        }
    }

    fn empty_context() -> Context {
        Context {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Mock echo tool for run-loop tests. Records executed args
    /// per call so test setups can detect terminate-flag flow.
    #[derive(Debug)]
    struct EchoTool {
        terminate: bool,
        executed: std::sync::Arc<Mutex<Vec<Value>>>,
    }
    impl EchoTool {
        fn new() -> Self {
            Self {
                terminate: false,
                executed: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn with_terminate(mut self) -> Self {
            self.terminate = true;
            self
        }
    }
    impl LoopTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo tool"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            args: Value,
            _signal: AbortSignal,
            _on_update: LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
        {
            let executed = self.executed.clone();
            let terminate = self.terminate;
            Box::pin(async move {
                executed.lock().unwrap().push(args.clone());
                Ok(super::super::LoopToolResult {
                    content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                    details: args,
                    terminate: if terminate { Some(true) } else { None },
                })
            })
        }
    }

    fn user(text: &str) -> LoopMessage {
        LoopMessage::User(UserMessage {
            content: text.to_string(),
        })
    }

    fn text_response(text: &str) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            StopReason::Stop,
        )
    }

    fn tool_use_response(id: &str, name: &str, args: Value) -> AssistantMessage {
        AssistantMessage::new(
            vec![ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
            }],
            StopReason::ToolUse,
        )
    }

    /// Drain channel into a Vec.
    async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
        let mut out = Vec::new();
        while let Some(e) = rx.recv().await {
            out.push(e);
        }
        out
    }

    /// Port of pi test "should emit events with AgentMessage types"
    /// (agent-loop.test.ts:84). Full agent loop run — assistant
    /// response, no tools.
    #[tokio::test]
    async fn test_emits_full_agent_loop_event_sequence() {
        let factory = canned_factory(vec![text_response("Hi there!")]);
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("Hello")],
            empty_context(),
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        // Must contain all pi-required events.
        for required in [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ] {
            assert!(kinds.contains(&required), "missing {required}: {kinds:?}");
        }
        // Return value: user + assistant message.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[1].role(), "assistant");
    }

    /// Port of pi test "should handle tool calls and results"
    /// (agent-loop.test.ts:239). Full-loop scope: assistant emits
    /// tool call → loop dispatches → next assistant emits final
    /// text.
    #[tokio::test]
    async fn test_full_loop_with_tool_then_final_text() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo.clone());

        let factory = canned_factory(vec![
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1})),
            text_response("done"),
        ]);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
        let messages = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        // Tool actually executed.
        assert_eq!(echo.executed.lock().unwrap().len(), 1);

        // Roles: user, assistant (tool use), toolResult, assistant (text).
        let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);

        // Stream of events should contain tool_execution_start +
        // tool_execution_end.
        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        assert!(kinds.contains(&"tool_execution_start"));
        assert!(kinds.contains(&"tool_execution_end"));
    }

    /// Port of pi test "should use prepareNextTurn snapshot before
    /// continuing" (agent-loop.test.ts:897). The hook returns a
    /// snapshot mutating `context`; subsequent turn observes the
    /// mutation.
    #[tokio::test]
    async fn test_prepare_next_turn_snapshot_applied() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.system_prompt = "first prompt".to_string();
        ctx.tools.push(echo.clone());

        // Track the system_prompt seen at each LLM call.
        let observed_prompts = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_clone = observed_prompts.clone();
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
            observed_clone.lock().unwrap().push(llm_ctx.system_prompt);
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
            } else {
                text_response("done")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
            }]))
        });

        // Hook fires once: returns a new context with a different
        // system prompt.
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let fired_clone = fired.clone();
        let hook: PrepareNextTurnFn = std::sync::Arc::new(move |ctx| {
            let fired = fired_clone.clone();
            Box::pin(async move {
                if fired.fetch_add(1, Ordering::SeqCst) > 0 {
                    return None; // only on the first invocation
                }
                Some(TurnUpdate {
                    context: Some(Context {
                        system_prompt: "second prompt".to_string(),
                        messages: ctx.context.messages.clone(),
                        tools: ctx.context.tools.clone(),
                    }),
                    ..Default::default()
                })
            })
        });

        let mut config = build_config();
        config.prepare_next_turn = Some(hook);

        let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
        let _ = run_agent_loop(
            vec![user("echo something")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        let observed = observed_prompts.lock().unwrap().clone();
        assert_eq!(observed.len(), 2, "expected 2 LLM calls");
        assert_eq!(observed[0], "first prompt");
        assert_eq!(
            observed[1], "second prompt",
            "second LLM call should see the mutated context"
        );
    }

    /// Port of pi test "should stop after the current turn when
    /// shouldStopAfterTurn returns true" (agent-loop.test.ts:970).
    #[tokio::test]
    async fn test_should_stop_after_turn_stops_loop() {
        let factory = canned_factory(vec![
            text_response("turn one"),
            // Second response should NEVER be requested — hook
            // stops the loop after turn one.
            text_response("should not appear"),
        ]);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        // Wrap factory to count invocations.
        let factory_counted: StreamFn = std::sync::Arc::new(move |ctx, opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            factory(ctx, opts)
        });

        let hook: ShouldStopAfterTurnFn = std::sync::Arc::new(|_ctx| Box::pin(async move { true }));

        let mut config = build_config();
        config.should_stop_after_turn = Some(hook);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("hi")],
            empty_context(),
            config,
            AbortSignal::new(),
            &tx,
            &factory_counted,
        )
        .await;
        drop(tx);

        // Only one LLM call.
        assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
        // Messages: user + one assistant.
        assert_eq!(messages.len(), 2);
        // Loop emitted agent_end.
        let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
        assert!(kinds.contains(&"agent_end"));
    }

    /// Port of pi test "should stop after a tool batch when every
    /// tool result sets terminate=true" (agent-loop.test.ts:1067).
    /// LOOP-LEVEL: only one LLM call (the tool dispatch terminates).
    #[tokio::test]
    async fn test_terminate_stops_loop_after_tool_batch() {
        let echo = std::sync::Arc::new(EchoTool::new().with_terminate());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: msg,
            }]))
        });

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let messages = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
        // user + assistant(tool use) + toolResult — no second
        // assistant text turn.
        let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, vec!["user", "assistant", "toolResult"]);
    }

    /// Port of pi test "should allow afterToolCall to mark a tool
    /// batch as terminating" (agent-loop.test.ts:1184). LOOP-LEVEL.
    #[tokio::test]
    async fn test_after_tool_call_terminate_stops_loop() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::ToolUse,
                message: msg,
            }]))
        });

        let after: AfterToolCallFn = std::sync::Arc::new(|_ctx: AfterToolCallContext| {
            Box::pin(async move {
                Some(AfterToolCallResult {
                    content: None,
                    details: None,
                    is_error: None,
                    terminate: Some(true),
                })
            })
        });
        let mut config = build_config();
        config.after_tool_call = Some(after);

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let _ = run_agent_loop(
            vec![user("echo")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
    }

    /// Port of pi test "should continue after parallel tool calls
    /// when not all tool results terminate" (agent-loop.test.ts:1119).
    /// LOOP-LEVEL: two LLM calls.
    #[tokio::test]
    async fn test_continue_when_not_all_terminate() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let llm_calls_clone = llm_calls.clone();
        let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
            let n = llm_calls_clone.fetch_add(1, Ordering::SeqCst);
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
            } else {
                text_response("done")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
            }]))
        });

        let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
        let _ = run_agent_loop(
            vec![user("echo")],
            ctx,
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;

        assert_eq!(
            llm_calls.load(Ordering::SeqCst),
            2,
            "two LLM calls expected"
        );
    }

    /// Port of pi test "should inject queued messages after all
    /// tool calls complete" (agent-loop.test.ts:547).
    ///
    /// Setup: assistant emits a tool call. After tool dispatch
    /// the loop polls `getSteeringMessages` which returns a user
    /// message ONCE. That message is injected before the next
    /// assistant call; the second LLM call sees it in its context.
    #[tokio::test]
    async fn test_steering_messages_injected_after_tool_calls() {
        let echo = std::sync::Arc::new(EchoTool::new());
        let mut ctx = empty_context();
        ctx.tools.push(echo);

        // Steering hook delivers once on the SECOND call (so
        // not on initial poll).
        let poll_count = std::sync::Arc::new(AtomicUsize::new(0));
        let poll_clone = poll_count.clone();
        let steering: GetSteeringMessagesFn = std::sync::Arc::new(move || {
            let poll = poll_clone.clone();
            Box::pin(async move {
                let n = poll.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    vec![user("interrupt")]
                } else {
                    Vec::new()
                }
            })
        });

        // Inspector: record what each LLM call sees in its
        // converted message list.
        let saw_interrupt_on_second = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_clone = saw_interrupt_on_second.clone();
        let call_counter = std::sync::Arc::new(AtomicUsize::new(0));

        let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
            let n = call_counter.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                // Second call: check for "interrupt" in messages.
                let found = llm_ctx.messages.iter().any(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("content").and_then(|c| c.as_str()) == Some("interrupt")
                });
                *saw_clone.lock().unwrap() = found;
            }
            let msg = if n == 0 {
                tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
            } else {
                text_response("done")
            };
            let reason = msg.stop_reason;
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason,
                message: msg,
            }]))
        });

        let mut config = build_config();
        config.get_steering_messages = Some(steering);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
        let messages = run_agent_loop(
            vec![user("start")],
            ctx,
            config,
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        drop(tx);

        assert!(
            *saw_interrupt_on_second.lock().unwrap(),
            "second LLM call should see the injected interrupt"
        );

        // Returned messages include the injected interrupt.
        let user_contents: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                LoopMessage::User(u) => Some(u.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(user_contents, vec!["start", "interrupt"]);

        // The interrupt's message_start fires AFTER the tool
        // result's message_end. We verify by event ordering.
        let events = drain(&mut rx).await;
        let interrupt_idx = events.iter().position(|e| match e {
            LoopEvent::MessageStart {
                message: LoopMessage::User(u),
            } => u.content == "interrupt",
            _ => false,
        });
        let last_tool_result_end_idx = events.iter().rposition(|e| {
            matches!(
                e,
                LoopEvent::MessageEnd {
                    message: LoopMessage::ToolResult(_)
                }
            )
        });
        assert!(
            interrupt_idx.unwrap() > last_tool_result_end_idx.unwrap(),
            "interrupt should appear AFTER the tool result message_end"
        );
    }

    // ---- agentLoopContinue tests ----

    /// Port of pi test "should throw when context has no messages"
    /// (agent-loop.test.ts:1234). Pi throws synchronously; we
    /// return Err.
    #[tokio::test]
    async fn test_continue_errors_on_empty_context() {
        let factory = canned_factory(vec![]);
        let (tx, _rx) = mpsc::channel::<LoopEvent>(4);
        let result = run_agent_loop_continue(
            empty_context(),
            build_config(),
            AbortSignal::new(),
            &tx,
            &factory,
        )
        .await;
        assert_eq!(result.unwrap_err(), LoopError::NoMessages);
    }

    /// Defensive: last message is assistant → error (pi:131-133
    /// — `runAgentLoopContinue` checks this before entering
    /// the loop).
    #[tokio::test]
    async fn test_continue_errors_when_last_is_assistant() {
        let mut ctx = empty_context();
        ctx.messages
            .push(serde_json::json!({"role": "assistant", "content": "hello"}));
        let factory = canned_factory(vec![]);
        let (tx, _rx) = mpsc::channel::<LoopEvent>(4);
        let result =
            run_agent_loop_continue(ctx, build_config(), AbortSignal::new(), &tx, &factory).await;
        assert_eq!(result.unwrap_err(), LoopError::CannotContinueFromAssistant);
    }

    /// Port of pi test "should continue from existing context
    /// without emitting user message events" (agent-loop.test.ts:1249).
    /// Continue does NOT re-emit message_start/message_end for the
    /// existing user message — only for the new assistant turn.
    #[tokio::test]
    async fn test_continue_does_not_reemit_user_message_events() {
        let mut ctx = empty_context();
        ctx.messages
            .push(serde_json::json!({"role": "user", "content": "Hello"}));

        let factory = canned_factory(vec![text_response("Response")]);

        let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
        let messages =
            run_agent_loop_continue(ctx, build_config(), AbortSignal::new(), &tx, &factory)
                .await
                .expect("continue");
        drop(tx);

        // Returned messages: ONLY the new assistant. The
        // pre-existing user does NOT appear.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), "assistant");

        // No message_end for a user message.
        let user_message_ends = drain(&mut rx)
            .await
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    LoopEvent::MessageEnd {
                        message: LoopMessage::User(_)
                    }
                )
            })
            .count();
        assert_eq!(user_message_ends, 0);
    }
}
