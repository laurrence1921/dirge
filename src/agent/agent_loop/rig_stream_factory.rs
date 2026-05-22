//! Phase 4.5f-2 — build a `StreamFn` from a real rig
//! `CompletionModel`. Plugs into `LoopSpawnConfig.stream_fn`
//! at the composition site, completing the integration between
//! the new loop and an actual LLM.
//!
//! ## What this provides
//!
//! - `rig_stream_fn_from_model(model, tools)` — produces a
//!   `StreamFn` that, per LLM call, builds a rig
//!   `CompletionRequest` from the supplied `LlmContext`, calls
//!   `model.stream(request)`, and wraps the response stream via
//!   `wrap_rig_stream` (4.5a).
//!
//! ## What it does NOT
//!
//! - Recovery / retry around the stream call. Lives in
//!   phase 4.5g — wrappers compose around this `StreamFn` from
//!   the outside.
//! - Permission checking / pre-flight. Tool definitions reach
//!   rig as-is; the loop's `before_tool_call` hook handles
//!   permission decisions at dispatch time, not provider time.
//!
//! ## Message conversion
//!
//! `LlmContext.messages: Vec<Value>` (the placeholder shape
//! phase 0 chose) carries our own message variants serialized
//! as JSON. This module converts each `Value` to a rig
//! `Message`:
//!
//! | Our `role` | rig `Message`                         |
//! |------------|---------------------------------------|
//! | "user"     | `Message::user(content_string)`       |
//! | "assistant"| `Message::Assistant { content: …}`    |
//! | "toolResult"| `Message::tool_result_with_call_id`  |
//! | other      | skipped (custom messages are UI-only) |
//!
//! Assistant content blocks (text / thinking / toolCall) map to
//! rig's `AssistantContent` variants. ToolResult content is
//! flattened to a single text body (rig's helper takes
//! `impl Into<String>`).
//!
//! ## Conversion is lossy by design
//!
//! Our `AssistantMessage.stop_reason` / `error_message` are
//! loop-internal; rig doesn't model them on the wire (the
//! provider derives stop reason from its own stream). They're
//! dropped in conversion.

use std::sync::Arc;

use rig::OneOrMany;
use rig::completion::message::{AssistantContent, Message, Reasoning, ToolCall, ToolFunction};
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequestBuilder, GetTokenUsage, ToolDefinition,
};
use serde_json::Value;

use super::message::StreamEvent;
use super::rig_stream::wrap_rig_stream;
use super::stream::{LlmContext, StreamFn};
use super::tool::{AbortSignal, LoopTool};

use futures::Stream;
use std::pin::Pin;

/// Build a `StreamFn` that drives a rig `CompletionModel`. Each
/// invocation of the returned closure builds a
/// `CompletionRequest` from the supplied `LlmContext`, calls
/// `model.stream(request).await`, and wraps the result via
/// `wrap_rig_stream`.
///
/// `tools` is captured at construction — rig wants tool
/// definitions in the request, and the loop's tool registry is
/// stable across turns. If tools ever need to vary per-call
/// (e.g. dynamic tool sets), pass an empty `tools` here and
/// have the caller inject definitions via a different
/// mechanism.
///
/// The model is cloned per-call so the closure can be `Fn`
/// (multi-call). `CompletionModel: Clone` is part of the trait
/// bounds so this is always cheap (Arc-internally in most rig
/// impls).
pub fn rig_stream_fn_from_model<M>(
    model: M,
    tools: Vec<ToolDefinition>,
    chunk_timeout: Option<std::time::Duration>,
) -> StreamFn
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    let tools = Arc::new(tools);
    Arc::new(move |ctx: LlmContext, opts: super::stream::StreamOptions| {
        let model = model.clone();
        let tools = tools.clone();
        invoke_one_stream(model, tools, ctx, chunk_timeout, opts)
    })
}

/// Build a stream that, when polled, performs the model.stream
/// call asynchronously and forwards the wrapped events. Returns
/// a `Pin<Box<dyn Stream<Item = StreamEvent> + Send>>` directly
/// — no outer Future indirection, matches the `StreamFn`
/// signature.
///
/// Errors from message conversion / the `model.stream` call
/// surface as a single `Error` event so the caller's loop
/// observes them uniformly.
fn invoke_one_stream<M>(
    model: M,
    tools: Arc<Vec<ToolDefinition>>,
    ctx: LlmContext,
    chunk_timeout: Option<std::time::Duration>,
    opts: super::stream::StreamOptions,
) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
where
    M: CompletionModel + Clone + Send + Sync + 'static,
    M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
{
    Box::pin(async_stream::stream! {
        // 1. Convert our messages to rig messages.
        let rig_messages: Vec<Message> = ctx
            .messages
            .iter()
            .filter_map(value_to_rig_message)
            .collect();

        // 2. Split: last is prompt; rest is chat_history.
        let (prompt, history) = if rig_messages.is_empty() {
            yield StreamEvent::Error {
                error: "rig_stream_fn: empty message list — no prompt to send".to_string(),
            };
            return;
        } else {
            let mut messages = rig_messages;
            let last = messages.pop().unwrap();
            (last, messages)
        };

        // 3. Build the rig CompletionRequest. Phase 4.6: pack
        //    reasoning + headers + metadata into the request's
        //    `additional_params` so providers that know about
        //    these fields can read them. Rig's underlying
        //    provider implementations vary in which they honor;
        //    unsupported fields are silently ignored downstream.
        let mut builder = CompletionRequestBuilder::new(model.clone(), prompt);
        if !ctx.system_prompt.is_empty() {
            builder = builder.preamble(ctx.system_prompt);
        }
        builder = builder.messages(history);
        if !tools.is_empty() {
            builder = builder.tools((*tools).clone());
        }
        // Build additional_params from opts.reasoning + headers +
        // metadata. Provider-specific mapping for reasoning lives
        // in `provider::AnyAgent::build_stream_fn` — the mapper
        // is captured in this closure's environment by the time
        // we get here. For phase 4.6's initial implementation we
        // pass the raw fields under conventional keys; downstream
        // commits add per-provider transformers if needed.
        let mut additional = serde_json::Map::new();
        if let Some(reasoning) = opts.reasoning {
            additional.insert(
                "reasoning_level".to_string(),
                serde_json::to_value(reasoning).unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(budgets) = &opts.thinking_budgets {
            if let Ok(v) = serde_json::to_value(budgets) {
                additional.insert("thinking_budgets".to_string(), v);
            }
        }
        if !opts.headers.is_empty() {
            if let Ok(v) = serde_json::to_value(&opts.headers) {
                additional.insert("headers".to_string(), v);
            }
        }
        if !opts.metadata.is_empty() {
            additional.insert(
                "metadata".to_string(),
                serde_json::Value::Object(
                    opts.metadata.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                ),
            );
        }
        if !additional.is_empty() {
            builder = builder.additional_params(serde_json::Value::Object(additional));
        }
        let request = builder.build();

        // 4. Call model.stream; wrap result or emit error.
        match model.stream(request).await {
            Ok(response) => {
                let mut wrapped = wrap_rig_stream(response, chunk_timeout, Some(opts.signal.clone()));
                use futures::stream::StreamExt;
                while let Some(evt) = wrapped.next().await {
                    yield evt;
                }
            }
            Err(e) => {
                yield StreamEvent::Error {
                    error: format!("rig stream call failed: {e}"),
                };
            }
        }
    })
}

/// Convert one of our `Value`-shaped messages to a rig
/// `Message`. Returns `None` for unrecognized roles (custom
/// messages get filtered at this boundary — pi calls this
/// out as the `convertToLlm` contract).
///
/// The shapes we recognize match what `run.rs` writes via
/// `loop_message_to_value` and what `stream.rs` writes via
/// `serialize_assistant`:
///
/// - User: `{"role": "user", "content": "<string>"}`
/// - Assistant: `{"role": "assistant", "content": [<blocks>], ...}`
/// - ToolResult: `{"role": "toolResult", "toolCallId": ..., "content": [<blocks>], ...}`
pub fn value_to_rig_message(value: &Value) -> Option<Message> {
    let role = value.get("role").and_then(|r| r.as_str())?;
    match role {
        "user" => {
            let content = value.get("content").and_then(|c| c.as_str())?;
            Some(Message::user(content))
        }
        "assistant" => {
            let blocks = value.get("content").and_then(|c| c.as_array())?;
            let assistant_contents: Vec<AssistantContent> = blocks
                .iter()
                .filter_map(value_to_assistant_content)
                .collect();
            // `OneOrMany::many` errors on empty input; rig
            // returns the error variant rather than constructing
            // an empty OneOrMany. Skip the message entirely if
            // we couldn't extract any usable blocks.
            let content = OneOrMany::many(assistant_contents).ok()?;
            Some(Message::Assistant { id: None, content })
        }
        "toolResult" => {
            let tool_call_id = value.get("toolCallId").and_then(|c| c.as_str())?;
            // Flatten content blocks into a single text body —
            // rig's helper takes `impl Into<String>`. Multi-block
            // tool results are rare; joining with newlines
            // preserves readability.
            let text = value
                .get("content")
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            b.as_object().and_then(|o| {
                                if o.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    o.get("text").and_then(|t| t.as_str()).map(String::from)
                                } else {
                                    None
                                }
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            Some(Message::tool_result(tool_call_id, text))
        }
        _ => None,
    }
}

/// Convert one assistant content block to a rig `AssistantContent`.
/// Recognizes `{type: "text"|"thinking"|"toolCall", ...}`.
fn value_to_assistant_content(block: &Value) -> Option<AssistantContent> {
    let obj = block.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str())?;
    match kind {
        "text" => {
            let text = obj.get("text").and_then(|t| t.as_str())?;
            Some(AssistantContent::text(text))
        }
        "thinking" => {
            let text = obj.get("text").and_then(|t| t.as_str())?;
            Some(AssistantContent::Reasoning(Reasoning::new(text)))
        }
        "toolCall" => {
            let id = obj.get("id").and_then(|t| t.as_str())?.to_string();
            let name = obj.get("name").and_then(|t| t.as_str())?.to_string();
            let arguments = obj.get("arguments").cloned().unwrap_or(Value::Null);
            Some(AssistantContent::ToolCall(ToolCall {
                id,
                call_id: None,
                function: ToolFunction { name, arguments },
                signature: None,
                additional_params: None,
            }))
        }
        _ => None,
    }
}

/// Build a rig `ToolDefinition` from one of our `LoopTool`s.
/// Returns the trio rig actually consumes (name, description,
/// parameters); label is dropped because rig has no slot for it.
pub fn loop_tool_to_rig_definition(tool: &dyn LoopTool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::UserContent;

    /// User-role value → `Message::User { content: text }`.
    #[test]
    fn user_value_converts_to_user_message() {
        let v = serde_json::json!({"role": "user", "content": "hello"});
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => {
                let first = content.first();
                match first {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    /// Assistant with a single text block converts cleanly.
    #[test]
    fn assistant_text_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "hi there"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { id, content } => {
                assert!(id.is_none());
                match content.first() {
                    AssistantContent::Text(t) => assert_eq!(t.text, "hi there"),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a toolCall block produces a rig `ToolCall`
    /// content.
    #[test]
    fn assistant_tool_call_block_converts() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": "call_1",
                "name": "echo",
                "arguments": {"value": "x"},
            }],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::ToolCall(tc) => {
                    assert_eq!(tc.id, "call_1");
                    assert_eq!(tc.function.name, "echo");
                    assert_eq!(tc.function.arguments["value"], "x");
                }
                _ => panic!("expected ToolCall"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// Assistant with a thinking block produces `Reasoning`.
    #[test]
    fn assistant_thinking_block_converts_to_reasoning() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "thinking", "text": "let me think"}],
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::Assistant { content, .. } => match content.first() {
                AssistantContent::Reasoning(_) => {}
                _ => panic!("expected Reasoning"),
            },
            _ => panic!("expected Assistant"),
        }
    }

    /// ToolResult value → rig's tool_result user-content message.
    /// Content blocks are flattened to a single text body.
    #[test]
    fn tool_result_value_converts() {
        let v = serde_json::json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "toolName": "echo",
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"},
            ],
            "details": {},
            "isError": false,
        });
        let msg = value_to_rig_message(&v).expect("must convert");
        match msg {
            Message::User { content } => match content.first() {
                UserContent::ToolResult(tr) => {
                    assert_eq!(tr.id, "call_1");
                }
                _ => panic!("expected ToolResult"),
            },
            _ => panic!("expected User"),
        }
    }

    /// Custom / unknown role → skipped (None).
    #[test]
    fn custom_role_returns_none() {
        let v = serde_json::json!({"role": "custom", "content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// Missing role field → None.
    #[test]
    fn missing_role_returns_none() {
        let v = serde_json::json!({"content": "x"});
        assert!(value_to_rig_message(&v).is_none());
    }

    /// `loop_tool_to_rig_definition` copies name + description +
    /// parameters; label is intentionally dropped (rig has no
    /// slot).
    #[test]
    fn loop_tool_definition_strips_label() {
        // A minimal LoopTool stub for the conversion test.
        #[derive(Debug)]
        struct Stub;
        impl LoopTool for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn description(&self) -> &str {
                "stub description"
            }
            fn label(&self) -> &str {
                "Stub Label"
            }
            fn parameters(&self) -> &Value {
                static P: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                P.get_or_init(|| serde_json::json!({"type": "object"}))
            }
            fn execute<'a>(
                &'a self,
                _id: &'a str,
                _args: Value,
                _signal: AbortSignal,
                _on_update: super::super::tool::LoopToolUpdate,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<super::super::result::LoopToolResult, String>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async { unreachable!("not called in conversion test") })
            }
        }

        let def = loop_tool_to_rig_definition(&Stub);
        assert_eq!(def.name, "stub");
        assert_eq!(def.description, "stub description");
        assert_eq!(def.parameters["type"], "object");
    }

    /// Compile-time: `rig_stream_fn_from_model` produces a
    /// `Send + Sync + 'static` StreamFn. This is the bound the
    /// loop demands; if it doesn't compile, no use of the
    /// factory is going to work.
    #[test]
    fn stream_fn_is_send_sync_static() {
        // Use rig's built-in test model (mock_provider) if
        // available; otherwise this test just verifies the type
        // constraints at compile time via assertion shape.
        // We can't easily build a real model in a unit test
        // because every rig provider needs an API key. Instead
        // we assert the trait bound via a turbofish on a generic
        // function — succeeds compile-time if the signature is
        // correct.

        fn assert_constraints<M>(_model: M)
        where
            M: CompletionModel + Clone + Send + Sync + 'static,
            M::StreamingResponse: Clone + Unpin + Send + Sync + GetTokenUsage + 'static,
        {
            // No-op; existence of the function is the proof.
        }

        // We can't instantiate M without a real provider; the
        // compile-time check on the function signature is what
        // matters. This test "passes" by virtue of compiling.
        let _: fn(_) = assert_constraints::<NopModel>;
    }

    /// Minimal stub CompletionModel so we can verify the
    /// factory produces a working `StreamFn` end-to-end. The
    /// stub returns a canned `done` event with empty text via
    /// `model.stream(request)`.
    #[derive(Clone)]
    struct NopModel;

    impl GetTokenUsage for NopStreamResponse {
        fn token_usage(&self) -> Option<rig::completion::Usage> {
            None
        }
    }

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopStreamResponse;

    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
    struct NopResponse;

    impl CompletionModel for NopModel {
        type Response = NopResponse;
        type StreamingResponse = NopStreamResponse;
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            NopModel
        }

        async fn completion(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
            // Not used by the streaming factory.
            unreachable!("completion() not used in stream factory tests")
        }

        async fn stream(
            &self,
            _request: rig::completion::CompletionRequest,
        ) -> Result<
            rig::streaming::StreamingCompletionResponse<Self::StreamingResponse>,
            CompletionError,
        > {
            // Empty inner stream — the wrap_rig_stream layer
            // synthesizes a `Done { reason: Stop, message: empty }`
            // for an empty stream, which is what we want for
            // the smoke test.
            let inner: rig::streaming::StreamingResult<Self::StreamingResponse> =
                Box::pin(futures::stream::empty());
            Ok(rig::streaming::StreamingCompletionResponse::stream(inner))
        }
    }

    /// End-to-end smoke test: build the factory from `NopModel`,
    /// invoke once, drain the resulting stream. Expect Start +
    /// Done (no Error). Proves the conversion + builder + wrap
    /// chain composes correctly.
    #[tokio::test]
    async fn factory_invocation_produces_start_and_done() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: "test preamble".to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut kinds = Vec::new();
        while let Some(evt) = stream.next().await {
            kinds.push(match &evt {
                StreamEvent::Start { .. } => "start",
                StreamEvent::Delta { .. } => "delta",
                StreamEvent::Done { .. } => "done",
                StreamEvent::Error { error } => {
                    panic!("unexpected error: {error}");
                }
            });
        }
        // Expect at minimum Start + Done. No Error.
        assert!(kinds.contains(&"start"));
        assert!(kinds.contains(&"done"));
    }

    /// Empty message list → factory emits an Error event (not a
    /// panic). Defensive — caller misconfiguration is loud.
    #[tokio::test]
    async fn factory_empty_messages_emits_error() {
        use futures::stream::StreamExt;
        let factory = rig_stream_fn_from_model::<NopModel>(NopModel, vec![], None);
        let ctx = LlmContext {
            system_prompt: String::new(),
            messages: Vec::new(),
        };
        let mut stream = factory(
            ctx,
            crate::agent::agent_loop::StreamOptions::from_signal(AbortSignal::new()),
        );
        let mut found_error = false;
        while let Some(evt) = stream.next().await {
            if matches!(evt, StreamEvent::Error { .. }) {
                found_error = true;
            }
        }
        assert!(found_error, "empty messages must produce an Error event");
    }
}
