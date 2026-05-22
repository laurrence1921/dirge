use compact_str::CompactString;

/// Structured classification of tool output for richer downstream
/// rendering. Most tools return plain text and use `Text`; tools
/// that surface file references (`read`, `find_files`,
/// `list_dir`) can opt into `File` so consumers (ACP, future UI
/// features) can render file refs as resource links rather than
/// blobs of text.
///
/// The classification is currently coarse — assigned by the
/// runner based on tool NAME rather than via per-tool plumbing —
/// since that's enough to drive opencode/ACP-style file-link
/// surfaces without touching every tool's `type Output = String`
/// contract. A future refactor could thread the variant through
/// the rig `Tool` trait for finer control.
#[derive(Debug, Clone, Default)]
pub enum ToolContent {
    /// Plain text output — the default for every tool that
    /// returns prose, JSON, command output, diffs, etc.
    #[default]
    Text,
    /// Tool surfaced one or more file paths (read returned the
    /// content of a specific file; find_files returned a listing).
    /// Consumers can render as a clickable resource link instead
    /// of a text blob.
    File,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Token(CompactString),
    Reasoning(CompactString),
    ToolCall {
        /// Provider call id (rig's `ToolCall.id`). Empty for older
        /// rig versions or providers that don't emit one; the UI
        /// uses it to pair this call with the corresponding
        /// `ToolResult` event for structured persistence (Phase 3).
        id: CompactString,
        name: CompactString,
        args: serde_json::Value,
    },
    /// Fired immediately AFTER `ToolCall` — marks the transition
    /// from "LLM has emitted this call" to "dispatch is imminent".
    /// Semantically: between this event and the matching
    /// `ToolResult`, the tool is *running*. Consumers use it to:
    ///   - Show per-tool spinners / status badges
    ///   - Emit ACP `ToolCallStatus::InProgress` updates (the
    ///     ACP protocol distinguishes pending / in_progress /
    ///     completed; without this dirge skipped the in_progress
    ///     transition)
    ///   - Plugin observability hooks that need a "started" tick
    ///     distinct from "LLM decided to call"
    ///
    /// The id matches the corresponding `ToolCall.id` so consumers
    /// can pair them. UI consumers that already track in-flight
    /// state via "saw ToolCall, no matching ToolResult" can ignore
    /// this event safely — it's purely additive.
    ///
    /// `name` is intentionally omitted — consumers correlate by
    /// `id` against the immediately-prior `ToolCall` which already
    /// carries the name. Keeping the variant lean (one field)
    /// keeps the per-event allocation cheap; the runner emits
    /// many of these per turn.
    ToolStarted {
        // Only consumed by feature-gated paths (ACP) at present;
        // UI arm uses `{ .. }`. The field is part of the variant's
        // documented contract — kept regardless of which features
        // are compiled.
        #[allow(dead_code)]
        id: CompactString,
    },
    ToolResult {
        /// Matching call id from the `ToolCall` event. Empty if the
        /// provider didn't emit one — the UI falls back to
        /// positional pairing (this result belongs to the most-
        /// recent unanswered ToolCall in the same turn).
        id: CompactString,
        output: CompactString,
        /// Structured classification of `output`. Additive — the
        /// existing `output: CompactString` remains the
        /// authoritative payload for the LLM and the default UI
        /// rendering path. Consumers that want richer rendering
        /// (ACP resource links, future UI file-card components)
        /// dispatch on `kind`.
        ///
        /// Defaults to `Text`; the runner classifies as `File`
        /// when the producing tool name is known to surface a
        /// file reference (`read`, `find_files`, `list_dir`).
        /// Coarse on purpose — no per-tool plumbing required.
        #[allow(dead_code)]
        kind: ToolContent,
    },
    Error(CompactString),
    /// The streaming run failed with a context-length error. Audit
    /// H17: the UI used to render this as a hard `Error` and stop;
    /// users had to manually `/compress` then re-issue. Now the
    /// runner emits `ContextOverflow` carrying the prompt it was
    /// trying to send so the UI can auto-compact the session and
    /// respawn the run with the same prompt against the compacted
    /// history.
    ContextOverflow {
        prompt: CompactString,
        error: CompactString,
    },
    Done {
        response: CompactString,
        tokens: u64,
        cost: f64,
    },
    /// Marks the start of one turn within an agent run. A "turn" is one
    /// LLM call + any tool calls it dispatched + the tool results
    /// returning. A pure-text response has exactly one turn (TurnStart 0
    /// → TurnEnd 0 → Done). A run with tool calls has multiple turns,
    /// with turn boundaries straddling tool-result/next-assistant
    /// content. Plugin hook authors (P3) consume these to bracket
    /// per-turn observability.
    TurnStart {
        index: u32,
    },
    /// Marks the end of one turn. Fires immediately before the next
    /// turn's TurnStart, or just before `Done` for the final turn.
    /// Empty runs (stream ended without any assistant content) emit
    /// neither TurnStart nor TurnEnd.
    TurnEnd {
        index: u32,
    },
    /// The runner observed an interjection request at a tool-result boundary
    /// and stopped the stream cleanly. Whatever assistant text had streamed
    /// so far is captured in `partial_response`. The UI is expected to
    /// commit it as an assistant message and then drain its interjection
    /// queue as the next user turn.
    Interjected {
        partial_response: CompactString,
        tokens: u64,
    },
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    ScrollUp,
    ScrollDown,
    MouseDown {
        row: u16,
        col: u16,
    },
    MouseDrag {
        row: u16,
        col: u16,
    },
    MouseUp {
        row: u16,
        col: u16,
    },
    Paste(String),
    /// Terminal was resized. Carries no payload — the renderer queries
    /// `crossterm::terminal::size()` directly; the variant is just a kick
    /// to repaint at the new dimensions.
    Resize,
}
