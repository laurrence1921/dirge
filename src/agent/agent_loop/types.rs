//! Value types for the agent loop. Faithful port of `pi/packages/agent/src/types.ts`.
//!
//! Phase 0: enums + plain shape structs. No behavior yet — phase 1+
//! consume these.

use serde::{Deserialize, Serialize};

/// How a batch of tool calls from one assistant message is executed.
///
/// Port of pi `ToolExecutionMode` (types.ts:36):
///   `"sequential" | "parallel"`
///
/// - `Sequential`: each tool call is prepared, executed, and finalized
///   before the next one starts.
/// - `Parallel`: tool calls are prepared sequentially, then allowed
///   tools execute concurrently. `tool_execution_end` events emit in
///   completion order; tool-result message artifacts emit later in
///   assistant source order.
///
/// Wire format is lowercase to match pi's TypeScript literal union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    /// Default per pi: `toolExecution?: ToolExecutionMode` defaults to
    /// `"parallel"` when omitted (types.ts:252 comment).
    #[default]
    Parallel,
}

/// How many queued user messages are injected at a queue drain point.
///
/// Port of pi `QueueMode` (types.ts:44):
///   `"all" | "one-at-a-time"`
///
/// - `All`: drain and inject every queued message at the drain point.
/// - `OneAtATime`: drain only the oldest queued message; the rest
///   stay queued for later drain points.
///
/// Wire format is kebab-case ("one-at-a-time") to match pi exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    All,
    OneAtATime,
}

/// Reasoning effort / thinking budget for models that support it.
///
/// Port of pi `ThinkingLevel` (types.ts:284):
///   `"off" | "minimal" | "low" | "medium" | "high" | "xhigh"`
///
/// Note from pi: `"xhigh"` is only supported by selected model
/// families. Pi recommends checking model thinking-level metadata
/// from `@earendil-works/pi-ai` to detect support for a concrete
/// model. Dirge will mirror this once provider plumbing lands in
/// phase 1.
///
/// Wire format is lowercase to match pi's literals exactly,
/// including `"xhigh"` (one word, no separator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Reasoning disabled. Pi's `prepareNextTurn` snapshot maps
    /// `"off"` to `reasoning: undefined` on the next request
    /// (agent-loop.ts:235-237).
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

/// Conversation context passed to the loop and threaded through
/// hooks. Port of pi `AgentContext` (types.ts:387).
///
/// `messages` is `Vec<serde_json::Value>` as a phase-0 placeholder;
/// phase 4 will substitute a typed `LoopMessage` enum once the
/// message vocabulary is finalized. We avoid choosing the final
/// shape here because rig's message types and dirge's existing
/// `session::Message` need to be reconciled — that's phase 1 work,
/// not phase 0.
///
/// `tools` is held as `Arc<dyn LoopTool>` so the same tool registry
/// can be shared across turns without cloning. Pi uses
/// `tools?: AgentTool<any>[]` — optional, defaulting to an empty
/// list when no tools are configured.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// System prompt sent with each model request. Pi field
    /// `systemPrompt: string`.
    pub system_prompt: String,
    /// Transcript visible to the model. Pi field `messages:
    /// AgentMessage[]`. Phase 0 placeholder type — see module doc.
    pub messages: Vec<serde_json::Value>,
    /// Tools available for this run. Pi field `tools?:
    /// AgentTool<any>[]`. Empty by default rather than `Option<Vec>`
    /// because empty-vs-absent has no semantic difference for pi's
    /// loop (both produce the same lookup misses).
    pub tools: Vec<std::sync::Arc<dyn super::tool::LoopTool>>,
}

/// Replacement runtime state returned by `prepareNextTurn`.
///
/// Port of pi `AgentLoopTurnUpdate` (types.ts:124):
///   `{ context?, model?, thinkingLevel? }`
///
/// All fields optional; omitted fields keep the current value
/// (loop.rs phase 4 will mirror pi's `?? config.X` fallback).
///
/// `model` is `Option<String>` here as the phase-0 placeholder.
/// Phase 4 will substitute the rig `CompletionModel` trait object
/// or an opaque model handle once the model-swap mechanism lands.
/// We don't pick the type now because the rig API for runtime
/// model swap may require its own wrapper type.
#[derive(Debug, Clone, Default)]
pub struct TurnUpdate {
    pub context: Option<Context>,
    pub model: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ToolExecutionMode` round-trips as lowercase, matching pi's
    /// TypeScript literal union. Verifies the serde rename rule.
    #[test]
    fn tool_execution_mode_wire_format() {
        assert_eq!(
            serde_json::to_string(&ToolExecutionMode::Sequential).unwrap(),
            "\"sequential\""
        );
        assert_eq!(
            serde_json::to_string(&ToolExecutionMode::Parallel).unwrap(),
            "\"parallel\""
        );
        assert_eq!(
            serde_json::from_str::<ToolExecutionMode>("\"sequential\"").unwrap(),
            ToolExecutionMode::Sequential
        );
        assert_eq!(
            serde_json::from_str::<ToolExecutionMode>("\"parallel\"").unwrap(),
            ToolExecutionMode::Parallel
        );
    }

    /// Default for `ToolExecutionMode` is `Parallel` per pi
    /// (`toolExecution?` defaults to `"parallel"` per types.ts:252).
    #[test]
    fn tool_execution_mode_default_is_parallel() {
        assert_eq!(ToolExecutionMode::default(), ToolExecutionMode::Parallel);
    }

    /// `QueueMode` uses kebab-case for `OneAtATime` to match pi's
    /// literal `"one-at-a-time"`. Easy to break if a future edit
    /// changes the `rename_all` rule.
    #[test]
    fn queue_mode_wire_format() {
        assert_eq!(serde_json::to_string(&QueueMode::All).unwrap(), "\"all\"");
        assert_eq!(
            serde_json::to_string(&QueueMode::OneAtATime).unwrap(),
            "\"one-at-a-time\""
        );
        assert_eq!(
            serde_json::from_str::<QueueMode>("\"one-at-a-time\"").unwrap(),
            QueueMode::OneAtATime
        );
    }

    /// Every `ThinkingLevel` variant round-trips at its expected
    /// lowercase string. `"xhigh"` is one word, no separator — pi
    /// uses this exact spelling and we must match it.
    #[test]
    fn thinking_level_wire_format() {
        let pairs = [
            (ThinkingLevel::Off, "\"off\""),
            (ThinkingLevel::Minimal, "\"minimal\""),
            (ThinkingLevel::Low, "\"low\""),
            (ThinkingLevel::Medium, "\"medium\""),
            (ThinkingLevel::High, "\"high\""),
            (ThinkingLevel::Xhigh, "\"xhigh\""),
        ];
        for (variant, wire) in pairs {
            let encoded = serde_json::to_string(&variant).unwrap();
            assert_eq!(encoded, wire, "encode mismatch: {variant:?}");
            let decoded: ThinkingLevel = serde_json::from_str(wire).unwrap();
            assert_eq!(decoded, variant, "decode mismatch: {wire}");
        }
    }

    /// Default for `ThinkingLevel` is `Off`. Aligns with pi's
    /// AgentState default `thinkingLevel: "off"` (agent.ts:75).
    #[test]
    fn thinking_level_default_is_off() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Off);
    }

    /// `Context::default()` produces an empty transcript and empty
    /// tool list. Matches pi's "no context yet" starting state.
    #[test]
    fn context_default_is_empty() {
        let ctx = Context::default();
        assert!(ctx.system_prompt.is_empty());
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }

    /// `TurnUpdate::default()` is the "no-op" snapshot — every
    /// field None. Pi's `if (nextTurnSnapshot)` check at
    /// agent-loop.ts:227 treats this case (technically `undefined`
    /// in pi, but a struct of all-None matches the semantics) as
    /// "keep current state for the next turn".
    #[test]
    fn turn_update_default_is_no_op() {
        let upd = TurnUpdate::default();
        assert!(upd.context.is_none());
        assert!(upd.model.is_none());
        assert!(upd.thinking_level.is_none());
    }
}
