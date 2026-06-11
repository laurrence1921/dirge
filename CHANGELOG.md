# Changelog

All notable changes to dirge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.2] - 2026-06-10

### Fixed
- **Tool chambers are restored when a session is reloaded.** The scrollback
  replay (`/sessions` switch, `-c`/`--session` resume, `/fork`) rendered only
  message prose and dropped every persisted tool call, so reloading a session
  lost all its edits/bash/reads from the visible transcript and showed
  pure-tool-call turns as a bare handle. Each call is now reconstructed as a
  chamber, matching the live view. (Data was never lost — the model always
  re-saw the calls; this was a display gap.)
- **Idle scroll no longer stalls.** The render loop's 8ms paint throttle could
  drop the final frame of a fast wheel/trackpad scroll, and with the agent
  idle there was no timer to repaint it — so the scrolled position sat
  unpainted until the next event. A pending dirty frame now repaints just past
  the throttle window even when idle.

### Changed
- Mouse-wheel scrolling over the chat moves 3 lines per tick (was 1), matching
  the side panel.
- On interactive exit, dirge prints a dark-gray hint —
  `Resume this session with: dirge --session <id>` — so it's easy to pick the
  session back up (skipped for `--no-session` / empty sessions).

## [0.5.1] - 2026-06-10

### Fixed
- **`sandbox-microvm` builds on macOS without OpenSSL.** The feature pulled
  `ssh2 → libssh2-sys → openssl-sys`, so `cargo build --features
  sandbox-microvm` (and `--all-features`) failed on machines without a
  system OpenSSL. The microVM SSH client is now `russh` (pure Rust, `ring`
  crypto backend) — no OpenSSL, no libssh2, no cmake. Host-key pinning and
  command execution behave as before.

### Added
- **Homebrew install.** `brew install dirge-code/dirge/dirge` installs a
  prebuilt binary (macOS + Linux); on macOS it avoids the Gatekeeper
  quarantine prompt of a downloaded tarball. The release workflow
  auto-bumps the tap formula on each tag.

## [0.5.0] - 2026-06-10

### Added
- **SQLite-backed project memory.** Memory moved off the flat `MEMORY.md` /
  `PITFALLS.md` files into a `memories` table in the per-project session DB
  (`.dirge/sessions/state.db`). Two-tier storage: hot entries inline in the
  prompt verbatim, and once the inline budget fills the least-salient entries
  demote to a searchable breadcrumb index (id + preview) the agent expands
  with `memory(action='expand')` or queries with `memory(action='search')`
  (FTS5). Removal tombstones rather than deletes (`restore` brings entries
  back). Legacy `MEMORY.md` / `PITFALLS.md` files are imported automatically
  on first load and parked as `*.imported`.
- **Cross-turn failure recovery checkpoint.** The repeat-loop guard only
  catches a model repeating the *same* call; a separate tracker counts
  consecutive errored tool results across turns (reset by any success) and,
  at a streak of three distinct failures, injects one structured recovery
  checkpoint asking the model to name the shared root cause and take a
  different next step. When one tool dominates the streak it's named so the
  model re-reads that tool's contract.
- **"Did you mean?" feedback.** An unknown tool name now gets a nearest-match
  suggestion (`ehco` → `echo`), an invalid enum argument lists the valid
  values plus the closest one, and `read` on a mistyped path suggests the
  near-miss neighbour in the same directory (`parserr.rs` → `parser.rs`).
- **Janet compression plugin + `/plugins` command.** Plugins can supply a
  custom compaction summary via an `on-compact` hook, and `/plugins` lists
  the loaded plugins.
- **Interactive DAP REPL** (`/dap-repl`) and a shared JSON-RPC framing layer
  for the debug-adapter client.

### Changed
- **Slash-command tab completion is on by default.** It was gated behind an
  experimental feature that never shipped in the default set; it's now a
  first-class `slash-completion` feature, default-on.
- **Memory guidance explains the frozen snapshot.** The agent is told its
  saved memory is injected as a session-start snapshot, so a fact saved
  mid-session becomes active next session — it no longer re-saves facts it
  doesn't see reappear.
- Oversized `bash` / `webfetch` output now truncates head + tail (was a
  buggier middle-out heuristic).

### Removed
- **Cross-session memory extractor** and the unused memory `confidence`
  field. The post-session orchestrator is now three stages (background
  review → skills curator → memory curator); the extractor re-derived what
  the per-session review already captures.

### Fixed
- A `read` cache hit returns the file content instead of a terse "unchanged"
  message, so a re-read after compaction isn't left empty.
- Compression: git-diff off-by-one and the truncation heuristics.
- Fresh-state panics, an FTS index issue, tool-status lifecycle bugs, and
  duplicate-result handling (DAP review round).
- Fail-closed permissions for the debug-adapter tools.

## [0.4.1] - 2026-06-08

### Fixed
- The `sandbox-microvm` feature now compiles on non-Linux hosts. The reflink
  fast path (`copy_file_range`) and the `dirge-microvm-runner` libkrun calls
  are Linux-only and were ungated, so `--features sandbox-microvm` (and
  `--all-features`) failed to build on macOS. They're now `cfg(target_os =
  "linux")`-gated, with a `std::fs::copy` fallback for file copies and a
  clear "Linux-only" stub for the runner. The sandbox itself still requires
  Linux + KVM at runtime; this only fixes the cross-platform build.

## [0.4.0] - 2026-06-08

### Added
- **Hardware-isolated microVM sandbox** (`--sandbox microvm`, opt-in behind
  the `sandbox-microvm` build feature). A per-session Linux microVM boots
  once via libkrun and runs every `bash` tool call inside it over SSH, with
  the workspace mounted via virtio-fs. Includes a pure-Rust OCI image puller
  (every layer SHA-256-verified against the manifest, no skopeo/buildah
  needed for remote images), ephemeral SSH keys with guest host-key pinning,
  rootfs snapshot/restore, the `/sandbox` slash commands (`attach`/`ssh`,
  `reboot`/`start`, `snapshot save|list|restore|delete`), the
  `dirge sandbox check` / `dirge sandbox setup` subcommands, and a
  `--microvm-image` flag. Requires `/dev/kvm` and `libkrun.so`. See
  [docs/microvm/](docs/microvm/INDEX.md).

  Experimental, and narrower than full isolation: only `bash` runs in the
  VM. The file tools (`read`/`write`/`edit`/`apply_patch`/`list_dir`/
  `find_files`) still operate directly on the host workspace, so the VM is a
  boundary for command execution, not for file access. See
  [docs/microvm/SECURITY.md](docs/microvm/SECURITY.md).
- **Memory entries now carry a UMP kind, identity, and lifecycle metadata.**
  Saved memories are classified by kind (working / semantic / procedural /
  identity / …) at capture time instead of all defaulting to one bucket, and
  record provenance and lifecycle fields used by eviction and recall.

### Changed
- **Memory compaction evicts by salience, not age.** When the store is full
  the least-salient entry is dropped first (ties broken oldest-first, so the
  prior behavior holds under uniform salience). Salience now derives from the
  entry's kind (transient working notes rank well below durable identity /
  semantic facts), so the useful memories survive longer.

### Security
- **Load-time threat scan on memory.** Entries that reach `MEMORY.md` /
  `PITFALLS.md` out-of-band (hand-edit, `git pull`) and so bypassed the
  capture-time check are now scanned on read, closing the rehydration gap for
  prompt-injection content smuggled into the memory files.

## [0.3.1] - 2026-06-05

### Changed
- **The TUI now renders as a single model-driven paint per event.** A
  `UiState` model is the single source of truth, and the screen (status
  line, input area, avatar, panels, scrollback) is painted as one effect
  when the model changes — with dirty-flag coalescing — replacing ~85
  scattered inline paint sites. No change in normal use; this is the
  groundwork for the input fix below.

### Fixed
- **Modal prompts no longer block the UI.** Permission prompts, the
  `question` questionnaire (including custom free-text answers), the
  `/plan` switch confirmation, and plugin confirm/select dialogs each ran
  in their own nested blocking read loop, which could freeze the interface
  while a prompt was up. They now route through a unified input state
  machine driven by the main event loop, so chat scroll, text
  selection/copy, and terminal resize stay live during a prompt, and user
  keystrokes take priority over the agent's output stream. Concurrent
  permission requests from a parallel tool batch queue safely instead of
  clobbering the active prompt.

## [0.3.0] - 2026-06-04

### Security
- **Plugin tools now route through the permission engine.** A Janet
  plugin-registered tool previously ran its handler with **no**
  allow/deny/ask check — arbitrary file/network/shell I/O bypassing
  authorization entirely. Plugin tools now go through the same `enforce`
  chokepoint as built-ins via a new high-risk `Operation::Plugin` (not
  builtin-allowed, not Accept-coerced, like MCP/bash): they prompt by
  default in standard mode, can be denied with `deny_tools`, and are
  allowed under yolo or a `/allow plugin_tool …` grant.

### Added
- **Agent profiles** (opt-in): `/agent <name>` activates a named persona
  that bundles a system prompt, a model, and a tool policy
  (`.dirge/agents/*.md`, `~/.config/dirge/agents/*.md`, or config `agents`,
  layered project > global > config). `/agent off` reverts. The `task` tool
  can spawn a subagent under a profile via `task(agent="<name>")`. See
  [docs/agents.md](docs/agents.md).
- **Shell integration** (the `:` prefix, zsh): an optional plugin to run
  `:<prompt>` from your normal shell — the answer prints and you stay in the
  shell; `:` commands share one session. See
  [shell-plugin/README.md](shell-plugin/README.md).
- **`--session <id>`**: resumes the session with that id if it exists, and
  **creates it under that exact id** otherwise — a stable id for scripts and
  the shell plugin.
- **`--no-color`**: collapses the entire TUI to the terminal's default
  foreground through a single theme chokepoint.
- **`Alt+X`**: drops queued mid-execution interjections without cancelling
  the running agent (`Ctrl+C` still cancels both).
- Role-routing keys `summarization_provider` and `subagent_provider` are now
  actually consumed: the former routes the in-loop compaction summarizer, the
  latter sets the default model for `task`-spawned subagents.

### Changed
- **Automatic compaction now runs LLM summarization.** Proactive
  turn-boundary folds at the high context-ratio threshold now produce a
  structured summary (via `summarization_provider` when configured, else the
  main model) instead of silently degrading to a prune-only pass — the
  in-loop summarizer was declared but never wired. The cheap tool-output
  cap/prune still runs first with no LLM call.
- **`/wt-merge` is now programmatic and conflict-safe.** It performs the
  merge directly (refuses on a dirty tree, `git merge --abort` on conflict,
  never force-deletes the worktree) instead of delegating to an unconstrained
  LLM prompt, and only returns to the main repo on a clean merge.
- **`/prompt` and `/agent` compose as independent layers.** Their
  `deny_tools` now union (an agent can no longer silently re-enable a tool an
  active prompt denied), and `/agent off` restores the prompt layer's prompt +
  denies and the pre-agent model.
- **`allow_tools` now caps MCP and plugin tools too** (via the `mcp_tool` /
  `plugin_tool` umbrellas), not just built-ins.
- **Phased `/plan`**: the plan phase now gets a true context reset
  (findings-only), matching its documented isolation; explore→plan forks run
  off the UI event loop.
- Model-family steering now tracks the active (swapped) model rather than the
  launch-time model, so `/model` and `/agent` swaps steer correctly.

### Fixed
- Compaction could leave an orphaned `tool_use`/`tool_result` pair
  (unbalanced history → provider 400); the fold window now snaps to
  user-message boundaries, and the tool-output prune handles block-array
  content (previously a silent no-op on real tool results).
- `edit_minified` is now classified as a mutating edit, so the
  verify-before-done gate and the repeat-loop/storm breaker treat it like
  `edit`/`apply_patch`.
- `--no-color` no longer leaks hard-coded green/red diff backgrounds.
- `/panel auto` threshold corrected to ≥152 cols (was documented as ≥100,
  where no gutter actually fits).
- The active DAP debug session is now torn down at process exit instead of
  relying on `Drop` of a `static` that never runs.
- Plugin `transform-context` / `on-compact` hooks no longer block a runtime
  worker thread (now `spawn_blocking` + timeout), and `message-end` fires in
  headless (`--print` / `--loop`) mode too.
- Background-shell concurrency cap is enforced atomically; `/mode yolo` warns
  at runtime when configured deny rules are made inert; `/allow add`
  validates against the single tool-name source of truth; a leaked git
  worktree is cleaned up if creation fails after `git worktree add`; an
  ignored plugin-provider re-registration is now surfaced.

## [0.2.4] - 2026-06-03

### Added
- **Debug Adapter Protocol (DAP) integration**: step-through debugging
  alongside the existing LSP client, hardened against the usual adapter
  failure modes (UB, hangs, panics).
- **Configurable terminal background**: themes can set a `background` color,
  and the built-in phosphor theme defaults to a soft charcoal `#222222`. The
  plain theme keeps the terminal's own background (`Reset`).
- **Snap-to-bottom on input**: typing in the prompt or pressing Down on a
  scrolled-up chat jumps straight back to the latest output instead of
  requiring a manual scroll.
- **Phased plan workflow** (`/plan <request>`, opt-in via
  `phased_workflow_enabled`): an explicit per-task command that runs
  explore → plan → implement → reviewer-runs-code loop. The explore and
  plan phases are context-isolated read-only forks; the implement phase
  is a normal streamed turn; a write-disabled reviewer fork then runs the
  code and emits a machine-parsed verdict, with `NEEDS_FIX` feeding a
  punch-list back for a bounded re-implement (`phased_workflow_max_review_cycles`,
  default 2). Ported from [vix](https://github.com/kirby88/vix). See
  [docs/agent-loop.md](docs/agent-loop.md#phased-plan-workflow-plan).
- **Minified tree-sitter read/edit** (`read_minified` / `edit_minified`):
  token-efficient file I/O that collapses a file to its structural
  skeleton — aggressive collapse for Rust/Java/Go, gap-preserving collapse
  for whitespace/ASI-sensitive grammars (Bash, Python, Ruby, Elixir, C/C++,
  TS, Clojure). Each gated on its `semantic-<lang>` feature.
- **Hard read-before-edit gate**: `edit`/`apply_patch` to a file never
  read this session is refused mechanically.
- **Thinking-stall watchdog**: the request-timeout backstop now injects a
  summary-reinjection nudge for graceful recovery from a stalled run.
- **Mandatory reason/intent fields** on the read/grep/glob/find/lsp tools
  (and bash anti-misuse fields), plus a **todo-completion nudge** that
  blocks a premature `end_turn` while todo items remain pending.
- Config keys `phased_workflow_enabled`, `phased_workflow_max_review_cycles`,
  and documentation for the pre-existing `dynamic_tool_search` and
  `context_depth_reminder_threshold` keys.

### Fixed
- **/plan busy indicator**: `/plan` now shows the standard busy state and
  clears the submitted text from the input box while the explore/plan forks
  run (previously it looked idle with the command still lingering), and the
  busy flag can no longer be stranded "running" by a failed repaint.
- **Resize scroll clamp**: enlarging the terminal while scrolled up no longer
  leaves a stale scroll offset that hid the newest output behind blank rows.
- **Idle Ctrl+C** now clears a typed-but-unsent draft instead of quitting
  outright; only an empty input line exits.
- **Home/End** moved to **Ctrl+Home/End** for chat scroll, freeing bare
  Home/End for the input editor's line-start/line-end as documented.
- Ctrl+J inside reverse-i-search no longer desyncs the search buffer.

### Acknowledgements
- Added [vix](https://github.com/kirby88/vix) — the battle-tested Go coding
  agent the above agentic-loop features were ported from.

## [0.2.3] - 2026-06-02

### Added
- Unified permission/authorization engine (single Policy Decision Point):
  op-based rules, `/why` decision-trace command, atomic multi-claim bash.
- Input box scrolls to keep the cursor visible past the height cap, and
  Up/Down navigate across soft-wrapped display rows.
- Cohesive low-saturation phosphor palette (hue = action, brightness =
  importance), a dedicated soft "thinking" color, and syntax-highlighted
  `read` boxes. Critic/thinking colors are config-themeable.
- Config-driven plugin toggles (`plugins.<name>.{enabled, auto_start}`)
  and a bundled `backpressured` validation-gated loop plugin.

### Changed
- Lighter terminal UI: the heavy double-line frame is now light
  single-line/rounded, the side panels follow the main frame's theme
  color, and the input prompt is a simple `> `.
- Reasoning/thinking is suppressed by default (spinner + Ctrl+O to
  expand) to keep the conversation focused.

### Fixed
- Secrets in tool output are redacted before reaching the LLM / session
  transcript.
- Transient LLM connection failures ("error sending request") now retry
  with exponential backoff.
- Questionnaire custom answers soft-wrap instead of running off-screen.
- Edit results collapse the appended LSP diagnostics into a one-line
  summary (Ctrl+O to expand); diagnostic floods are summarized and the
  per-file cap tightened, so an unsupported language server no longer
  floods the chat.
- A configured `deny` rule is now terminal above a session allowlist.
- Resumed sessions keep persisting after a save (loaded-mtime refresh).

### Packaging
- Published to crates.io as the **`dirge-agent`** crate (the short
  `dirge` name was taken); the installed binary is still `dirge`:
  `cargo install dirge-agent`.

## [1.0.0]

First tagged release. dirge is a minimalistic, memory-efficient coding
agent in Rust with:

- A terminal UI with markdown rendering, scrollback, and an info panel.
- Configurable permission modes (standard / restrictive / accept / yolo)
  with op-based rules and session allowlists.
- Tree-sitter bash permission parsing and semantic code tools for
  TypeScript, Python, Clojure, Go, Ruby, Rust, Java, C, and C++.
- Claude-compatible skills, persistent project memory, subagents, MCP and
  LSP integration, and a Janet plugin system.
- Session save/load/resume with LLM-summarization compaction.

[Unreleased]: https://github.com/dirge-code/dirge/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/dirge-code/dirge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dirge-code/dirge/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/dirge-code/dirge/compare/v0.3.0...v0.3.1
[1.0.0]: https://github.com/dirge-code/dirge/releases/tag/v1.0.0
