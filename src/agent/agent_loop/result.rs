//! Tool-result types. Port of pi `AgentToolResult<T>`,
//! `BeforeToolCallResult`, `AfterToolCallResult` (types.ts:344-81).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Final or partial result produced by a tool.
///
/// Port of pi `AgentToolResult<T>` (types.ts:345):
///   `{ content: (TextContent | ImageContent)[]; details: T; terminate?: boolean }`
///
/// `content` is the LLM-visible payload (text or image blocks
/// shipped back to the model). `details` is structured metadata
/// for UI / log consumers; pi types it generically per-tool, we
/// use opaque `Value` to keep the trait object homogeneous.
///
/// `terminate` is the early-stop hint. Per agent-loop.ts:544 the
/// loop only terminates when EVERY tool result in the batch has
/// `terminate: true` — one tool can't unilaterally end the run.
/// `None` and `Some(false)` are equivalent at the loop level.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoopToolResult {
    /// LLM-visible content blocks. Phase 0 uses `Value` as
    /// placeholder for `(TextContent | ImageContent)[]`; phase 1
    /// will introduce a typed `Content` enum once the rig
    /// message vocabulary is reconciled.
    pub content: Vec<Value>,
    /// Structured details for UI / logs. Pi types this
    /// generically; we use opaque `Value` for trait-object
    /// uniformity.
    pub details: Value,
    /// Early-termination hint. See doc above for the batch-AND
    /// semantics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

/// Result returned from `beforeToolCall`.
///
/// Port of pi `BeforeToolCallResult` (types.ts:55):
///   `{ block?: boolean; reason?: string }`
///
/// Returning `block: true` prevents tool execution; the loop
/// emits an error tool result whose text is `reason` (or a
/// default "Tool execution was blocked" message if omitted —
/// agent-loop.ts:601).
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: Option<bool>,
    pub reason: Option<String>,
}

/// Partial override returned from `afterToolCall`.
///
/// Port of pi `AfterToolCallResult` (types.ts:72):
///   `{ content?; details?; isError?; terminate? }`
///
/// Per pi's merge semantics (types.ts:62-71): each field that's
/// `Some(...)` replaces the executed result's corresponding
/// field IN FULL. Omitted fields keep the original values. No
/// deep merge — `content` and `details` are atomic.
///
/// `is_error` defaults to the executed result's flag if not
/// overridden. `terminate` similarly.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<Value>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LoopToolResult::default()` produces an empty content/
    /// details with no terminate hint. This is the "ok but
    /// empty" shape; not used as an error sentinel (errors get
    /// their own `content` text via the dispatcher).
    #[test]
    fn loop_tool_result_default() {
        let r = LoopToolResult::default();
        assert!(r.content.is_empty());
        assert_eq!(r.details, Value::Null);
        assert!(r.terminate.is_none());
    }

    /// `terminate` serialization skips when None to keep wire
    /// payloads slim — matches pi where `terminate?` is omitted
    /// from JSON when undefined.
    #[test]
    fn terminate_omitted_when_none() {
        let r = LoopToolResult {
            content: vec![],
            details: Value::Null,
            terminate: None,
        };
        let encoded = serde_json::to_string(&r).unwrap();
        assert!(!encoded.contains("terminate"), "got: {encoded}");
    }

    /// `terminate: true` emits as `"terminate":true`.
    #[test]
    fn terminate_serializes_when_some() {
        let r = LoopToolResult {
            content: vec![],
            details: Value::Null,
            terminate: Some(true),
        };
        let encoded = serde_json::to_string(&r).unwrap();
        assert!(encoded.contains("\"terminate\":true"), "got: {encoded}");
    }

    /// `BeforeToolCallResult` default is "no block, no reason" —
    /// the no-op outcome from a hook that ran but didn't object.
    #[test]
    fn before_default_is_no_block() {
        let r = BeforeToolCallResult::default();
        assert!(r.block.is_none());
        assert!(r.reason.is_none());
    }

    /// `AfterToolCallResult` default is all-None — keeps the
    /// executed result's content/details/isError/terminate
    /// verbatim per pi's merge semantics.
    #[test]
    fn after_default_is_passthrough() {
        let r = AfterToolCallResult::default();
        assert!(r.content.is_none());
        assert!(r.details.is_none());
        assert!(r.is_error.is_none());
        assert!(r.terminate.is_none());
    }
}
