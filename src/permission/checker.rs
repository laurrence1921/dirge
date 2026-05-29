// Phase 2b routes runtime decisions through the engine; the legacy
// `check`/`check_path` facade + their private helpers and rule fields
// remain only for the `/allow` display surface, the `semantic-bash`
// dev-null soft-allow, and the test oracle. They are bin-unused in the
// default build. Phase 4 deletes this legacy decision code wholesale,
// at which point this allow goes too.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::permission::allowlist;
use crate::permission::engine;
use crate::permission::path;
use crate::permission::pattern::Pattern;
use crate::permission::{Action, PermissionConfig, SecurityMode, ToolPerm};

pub type PermCheck = Arc<Mutex<PermissionChecker>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allowed,
    Ask,
    Denied(String),
}

/// Render a decision's audit trail for the `/why` command: the final
/// effect + deciding policy, then every applicable policy's vote in
/// evaluation order (and the skipped ones, so it's clear what did and
/// didn't apply).
fn format_decision(tool: &str, input: &str, decision: &engine::types::Decision) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "why: {tool} {input:?}");
    let _ = writeln!(out, "  → {:?}  ({})", decision.effect, decision.reason());
    for e in &decision.trace {
        if e.applied {
            let eff = e
                .effect
                .map(|x| format!("{x:?}"))
                .unwrap_or_else(|| "—".to_string());
            let _ = writeln!(out, "  · {:<16} {eff:<6} {}", e.policy, e.why);
        } else {
            let _ = writeln!(out, "  · {:<16} (n/a)  {}", e.policy, e.why);
        }
    }
    out
}

/// Map an engine [`Decision`](engine::types::Decision) onto the legacy
/// `CheckResult` returned by the `check`/`check_path` facade.
fn effect_to_result(decision: engine::types::Decision) -> CheckResult {
    use engine::types::Effect;
    match decision.effect {
        Effect::Allow => CheckResult::Allowed,
        Effect::Ask => CheckResult::Ask,
        Effect::Deny => CheckResult::Denied(decision.reason()),
    }
}

pub struct PermissionChecker {
    rules: HashMap<String, Vec<(Pattern, Action)>>,
    default_action: Action,
    ext_dir_rules: Vec<(Pattern, Action)>,
    doom_loop_action: Action,
    working_dir: String,
    /// Cached canonical form of `working_dir`, computed once at
    /// construction (and refreshed by `set_working_dir`). Used by
    /// `is_external_path` to compare canonical paths without
    /// hitting the filesystem on every permission check — the
    /// canonicalize syscall is otherwise called once per
    /// read/write/edit/grep call, accumulating to hundreds of
    /// stat()s per session.
    working_dir_canonical: String,
    /// The currently-installed CWD-scoped allow-glob (e.g.
    /// `/Users/foo/proj/**`) used by `install_cwd_allow_rules` and
    /// `set_working_dir`. Recorded so that on cd we can find and
    /// remove the stale entries from `rules` before installing
    /// fresh ones, without touching user-configured rules pushed
    /// onto the same Vec. `None` when no CWD-allow was installable
    /// (degenerate working_dir, e.g. empty or `/`).
    cwd_allow_pattern: Option<String>,
    session_allowlist: Vec<(String, Pattern)>,
    recent_calls: VecDeque<(String, String)>,
    /// PERM-1: per-key repeat counter. Tracks how many times each
    /// (tool, input) pair has been seen. Uses a HashMap keyed by
    /// "{tool}\x00{input}" so the lookup is O(1) instead of scanning
    /// the FIFO window. Counts persist until evicted by the FIFO
    /// ring (window 32) — a 14-call decoy-gap attack can't flush a
    /// specific key because the ring is 2× the old window.
    repeat_counts: HashMap<String, u32>,
    mode: SecurityMode,
    /// Tools denied by the currently-active prompt's frontmatter
    /// `deny_tools` list. Enforced at the top of every `check` /
    /// `check_path` call — even before Yolo mode's blanket allow.
    /// This is the permission-layer enforcement of plan/review/etc.
    /// modes; previously plan mode relied on prose ("don't write
    /// code") + inline `is_plan_file` gates in edit/write/apply_patch,
    /// which an adversarial / confused LLM could route around via
    /// `bash` or by bypassing the gate name-check.
    ///
    /// Updated by `set_prompt_deny_tools` whenever the active prompt
    /// changes (slash `/prompt <name>`, session load, startup). Empty
    /// when no prompt is active or the active prompt has no
    /// frontmatter.
    prompt_deny_tools: Vec<String>,
    /// The unified authorization engine. Phase 2b routes the live
    /// `enforce` chokepoint through this (via `authorize_scope`); the
    /// legacy `rules`/`check`/`check_path` fields above remain only for
    /// the `/allow` display surface and the old test suite, and are
    /// deleted in Phase 4. The engine is the source of truth for
    /// runtime decisions; the session allowlist + prompt-deny are kept
    /// in sync with it on every write so the two views never diverge.
    engine: engine::Engine,
}

/// Tools that execute external code with broad effects. Accept mode
/// does NOT coerce `Ask → Allow` for these — the "I trust the agent
/// inside cwd" rationale that justifies the coercion for other
/// non-path tools doesn't generalize to shell + MCP servers.
fn is_high_risk_non_path_tool(tool: &str) -> bool {
    engine::is_high_risk_non_path_tool(tool)
}

/// Tool names where the input is a filesystem path. For these, `*` keeps
/// classic glob semantics (one segment, doesn't cross `/`). Everything else
/// is treated as shell/text where `*` means "any chars including /".
pub(crate) fn is_path_tool_name(tool: &str) -> bool {
    engine::is_path_tool_name(tool)
}

/// Build a Pattern with the right `*` semantics for the given tool.
pub(crate) fn pattern_for_tool(tool: &str, pat: &str) -> Pattern {
    engine::pattern_for_tool(tool, pat)
}

impl PermissionChecker {
    pub fn new(
        config: &PermissionConfig,
        mode: SecurityMode,
        working_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // M4 (dirge-ojn): default flipped Allow → Ask. Unconfigured
        // tools now prompt the user instead of silently executing.
        // Read-only tools that should NOT prompt get explicit Allow
        // rules installed below (see `install_default_allow_rules`).
        //
        // Why: dirge previously defaulted every unmatched tool to
        // Allow — e.g. `write` had no rules installed, so write to
        // any cwd path executed silently. Combined with the bash
        // redirect-target bug closed in M3 (fbcc09b), the practical
        // posture was "anything runs unless an explicit rule says no",
        // the opposite of what users expect from a coding agent.
        //
        // Mirrors maki's posture (`maki-agent/src/permissions.rs:199`:
        // bash, write, edit, MCP all default to Ask; an explicit
        // BUILTIN_ALLOW_RULES list opens specific safe tools) and
        // opencode's (`evaluate.ts:14`: `return match ?? { action:
        // "ask" }` — Ask is the universal fallback).
        let default_action = config.default.unwrap_or(Action::Ask);
        let doom_loop_action = config.doom_loop.unwrap_or(Action::Ask);

        // Resolve `working_dir` UP-FRONT so the CWD-scoped builtin
        // allow rules installed below can embed it in their
        // patterns. The actual struct field is populated from this
        // same value at the bottom of `new`.
        let working_dir = working_dir
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
            .to_string_lossy()
            .to_string();

        let mut rules: HashMap<String, Vec<(Pattern, Action)>> = HashMap::new();

        // M4 (dirge-ojn): install the builtin-allow list FIRST so user
        // rules added later (last-match-wins per check_path's
        // `matched.last()`) can override specific patterns while the
        // tool's overall posture stays Allow-by-default for safety.
        //
        // Example: user writes `read: { "/etc/**": "deny" }`. With the
        // builtin already installed as `read: { "**": allow }`, the
        // user's specific deny appends to the same Vec. On lookup the
        // last matching pattern wins:
        //   - `/etc/passwd` → both rules match → user's deny wins ✓
        //   - `/tmp/safe.txt` → only `**` matches → builtin allow ✓
        //
        // Tools NOT in this list (write/edit/apply_patch/bash/webfetch/
        // websearch/task) fall to the global default Ask unless the
        // user installs explicit rules. NOTE: `memory` and `skill` ARE
        // in this list (added below, per dirge-sm9w) — auto-allowed in
        // Standard/Accept and demoted to Ask only in Restrictive.
        //
        // Adapts maki's `BUILTIN_ALLOW_RULES`
        // (`maki-agent/src/permissions.rs:16-24`) for dirge's tool set.
        // Maki includes write/edit/multiedit in its allow list — a
        // different posture choice that doesn't suit dirge given the
        // audit history (C1/C8/etc.).
        for tool in [
            "read",
            "glob",
            "grep",
            "find_files",
            "list_dir",
            "list_symbols",
            "find_definition",
            "find_callers",
            "find_callees",
            "get_symbol_body",
            "repo_overview",
            "lsp",
            "write_todo_list", // Internal-only TODO tracking; no side effects
            "task_status",     // Read-only status query for background tasks
            "question",        // Interactive by definition; gating it just adds friction
            // dirge-sm9w: memory writes are scoped to `~/.dirge/memories/`
            // (no arbitrary filesystem access) and the tool can only
            // add/edit/delete its own entries. The per-action prompt
            // is friction without security value in Standard/Accept
            // modes. Restrictive mode still demotes this back to Ask
            // in the mode switch below — its contract is "every
            // action confirms".
            "memory",
            // skill follows the same contract as memory: its actions
            // (load/list/create/edit/patch) operate only on the
            // agent's own scoped skills directory, not arbitrary
            // filesystem paths. Prompting per action is friction with
            // no security value in Standard/Accept. Restrictive still
            // demotes the WRITE actions (create/edit/patch) back to
            // Ask in the mode switch below; the read actions
            // (load/list) pass through.
            "skill",
        ] {
            rules
                .entry(tool.to_string())
                .or_default()
                .push((pattern_for_tool(tool, "**"), Action::Allow));
        }

        // CWD-scoped builtin-allow for mutating filesystem tools.
        // Helper handles canonicalization + safety guards; see
        // `install_cwd_allow_rules` for the contract.
        let cwd_allow_pattern = install_cwd_allow_rules(&mut rules, &working_dir);

        // /dev/null is a harmless bit-bucket — writes silently
        // discard data, reads return immediate EOF. It must be
        // allowed for ALL tools without prompting, regardless of
        // security mode. Without this, every `> /dev/null` bash
        // redirect and every `write /dev/null` call triggers an
        // unnecessary permission dialog.
        install_dev_null_allow(&mut rules);

        // Helper: append a `ToolPerm` (Simple or Granular) onto a
        // tool's rule vec. Used by both the legacy per-tool fields and
        // the M2 `tools` map. The legacy fields are syntactic sugar
        // for `tools.{name}` — same code path.
        fn append_tool_perm(
            rules: &mut HashMap<String, Vec<(Pattern, Action)>>,
            tool_name: &str,
            tp: &ToolPerm,
        ) {
            let entries = rules.entry(tool_name.to_string()).or_default();
            match tp {
                ToolPerm::Simple(action) => {
                    entries.push((pattern_for_tool(tool_name, "*"), *action));
                }
                ToolPerm::Granular(map) => {
                    for (pat, action) in map {
                        entries.push((pattern_for_tool(tool_name, pat), *action));
                    }
                }
            }
        }

        // Track which tools the user explicitly configured (legacy
        // OR via `tools` map) so the bash / MCP default-installers
        // below can decide whether to skip themselves.
        let mut user_configured: std::collections::HashSet<&str> = std::collections::HashSet::new();

        for (tool_name, tool_perm) in [
            ("bash", &config.bash),
            ("read", &config.read),
            ("write", &config.write),
            ("edit", &config.edit),
            ("grep", &config.grep),
            ("find_files", &config.find_files),
            ("list_dir", &config.list_dir),
            // Adversarial-review #5 added; both are read-only walkers.
            ("glob", &config.glob),
            ("repo_overview", &config.repo_overview),
            ("write_todo_list", &config.write_todo_list),
            ("apply_patch", &config.apply_patch),
            ("lsp", &config.lsp),
            ("question", &config.question),
            // Newly-configurable tools (previously the perm checker
            // had no rules for them, so they always fell through to
            // the `*` default and couldn't be individually gated).
            ("webfetch", &config.webfetch),
            ("websearch", &config.websearch),
            ("task", &config.task),
            ("task_status", &config.task_status),
            ("memory", &config.memory),
            ("skill", &config.skill),
            ("list_symbols", &config.list_symbols),
            ("get_symbol_body", &config.get_symbol_body),
            ("find_definition", &config.find_definition),
            ("find_callers", &config.find_callers),
            ("find_callees", &config.find_callees),
            ("mcp_tool", &config.mcp_tool),
        ] {
            if let Some(tp) = tool_perm {
                append_tool_perm(&mut rules, tool_name, tp);
                user_configured.insert(tool_name);
            }
        }

        // M2 (dirge-cep): merge the unified `tools` map. New configs
        // declare rules for ANY tool name (including plugin / MCP /
        // future tools) without extending `PermissionConfig`. Same
        // append semantics as the legacy fields: tools-map rules are
        // pushed after legacy rules so last-match-wins.
        if let Some(tools_map) = &config.tools {
            for (tool_name, tp) in tools_map {
                append_tool_perm(&mut rules, tool_name, tp);
                // Static lifetime needed for HashSet entry —
                // restrict to the known tool name set; unknown tool
                // names (plugin/MCP) don't gate the bash/MCP
                // defaults below anyway.
                if matches!(tool_name.as_str(), "bash" | "mcp_tool") {
                    user_configured.insert(match tool_name.as_str() {
                        "bash" => "bash",
                        "mcp_tool" => "mcp_tool",
                        _ => unreachable!(),
                    });
                }
            }
        }

        // Bash defaults: only install if the user didn't supply ANY
        // bash rules (legacy or `tools` map). Bash's defaults are
        // specific allow + deny patterns that don't compose well
        // with arbitrary user rules — a `cargo *: deny` from the
        // user shouldn't have to co-exist with the default
        // `cargo build: allow`.
        if !user_configured.contains("bash") {
            let mut defaults = Vec::new();
            for (pat, action) in crate::permission::default_bash_rules() {
                defaults.push((pattern_for_tool("bash", pat), action));
            }
            // Replace any builtin-allow entry (bash isn't in the
            // builtin-allow list anyway, but be explicit).
            rules.insert("bash".to_string(), defaults);
        }

        // MCP tools execute external code (the MCP server's
        // implementation, plus whatever effects the server has on
        // the filesystem / network / API services). The previous
        // default was the inherited `default_action` (Allow) since
        // `mcp_tool` had no rule installed; that let an entire
        // sequence of MCP calls execute silently, with only the
        // doom-loop detector eventually prompting on the 3rd
        // identical call. User reported running through several
        // MCP queries without ever being asked. Install a default
        // `Ask` rule when no explicit config exists. Users who
        // trust a specific MCP server can pin it with config:
        //
        //   "permission": {
        //     "mcp_tool": {
        //       "mcp_tool:lattice:*": "allow"
        //     }
        //   }
        //
        // …or accept once and pick "allow always" for the same
        // effect via the session allowlist.
        if !user_configured.contains("mcp_tool") {
            rules.insert(
                "mcp_tool".to_string(),
                vec![(pattern_for_tool("mcp_tool", "*"), Action::Ask)],
            );
        }

        // External-directory rules are always path patterns by definition.
        let ext_dir_rules = config
            .external_directory
            .as_ref()
            .map(|map| {
                map.iter()
                    .map(|(pat, action)| (Pattern::new(pat), *action))
                    .collect()
            })
            .unwrap_or_default();

        // `working_dir` was already resolved earlier in this fn (used
        // by the CWD-scoped builtin allow installer above).
        let working_dir_canonical = canonicalize_for_cache(&working_dir);

        // The unified engine, built from the same config. Runtime
        // decisions (the `enforce` chokepoint) flow through this.
        let engine = engine::Engine::from_config(config);

        PermissionChecker {
            rules,
            default_action,
            ext_dir_rules,
            doom_loop_action,
            working_dir,
            working_dir_canonical,
            cwd_allow_pattern,
            session_allowlist: Vec::new(),
            recent_calls: VecDeque::with_capacity(32),
            repeat_counts: HashMap::new(),
            mode,
            prompt_deny_tools: Vec::new(),
            engine,
        }
    }

    /// Engine-backed decision for the `enforce` chokepoint. Normalizes
    /// a single (tool, input) into an [`engine::types::AccessRequest`],
    /// authorizes it, commits (loop-guard accounting), and returns the
    /// [`engine::types::Decision`]. `is_path` selects path-resource
    /// classification (resolved + in_cwd + dev_null) vs a raw resource.
    pub fn authorize_scope(
        &mut self,
        tool: &str,
        input: &str,
        is_path: bool,
    ) -> engine::types::Decision {
        let req = self.build_request(tool, input, is_path);
        let decision = self.engine.authorize(&req);
        self.engine.commit(&req, &decision);
        decision
    }

    /// Dry-run a decision and render its full audit trail (which
    /// policy decided and why, plus every applicable policy's vote).
    /// Pure: no commit, no loop-guard accounting. Backs the `/why`
    /// command so the user can see exactly what governs an action.
    pub fn explain(&self, tool: &str, input: &str, is_path: bool) -> String {
        let req = self.build_request(tool, input, is_path);
        let decision = self.engine.authorize(&req);
        format_decision(tool, input, &decision)
    }

    /// Normalize a (tool, input) pair into a one-resource request. The
    /// raw-resource variant picks the resource type from the tool:
    /// shell → Command, mcp_tool → Mcp, webfetch/websearch → Url,
    /// everything else → Bareword (memory/skill action, grep pattern…).
    fn build_request(
        &self,
        tool: &str,
        input: &str,
        is_path: bool,
    ) -> engine::types::AccessRequest {
        use engine::types::Resource;
        let resource = if is_path {
            engine::classify_path(input, &self.working_dir)
        } else {
            match tool {
                "bash" | "shell" => Resource::Command {
                    raw: input.to_string(),
                    head: input.split_whitespace().next().unwrap_or("").to_string(),
                },
                "mcp_tool" => {
                    // input shape: "mcp_tool:<server>:<name>"
                    let mut parts = input.splitn(3, ':');
                    let _umbrella = parts.next();
                    let server = parts.next().unwrap_or("").to_string();
                    let name = parts.next().unwrap_or("").to_string();
                    Resource::Mcp {
                        server,
                        name,
                        raw: input.to_string(),
                    }
                }
                "webfetch" | "websearch" => Resource::Url(input.to_string()),
                _ => Resource::Bareword(input.to_string()),
            }
        };
        engine::types::AccessRequest {
            op: engine::tool_operation(tool),
            tool: tool.to_string(),
            resources: vec![resource],
            mode: self.mode,
            display_input: input.to_string(),
        }
    }

    /// Install the current prompt's deny-list. Called when the
    /// active prompt changes (startup, session load, `/prompt
    /// <name>`); pass an empty vec to clear.
    pub fn set_prompt_deny_tools(&mut self, denied: Vec<String>) {
        self.engine.ctx_mut().prompt_deny = denied.clone();
        self.prompt_deny_tools = denied;
    }

    /// Returns true when `tool` is in the active prompt's
    /// `deny_tools` frontmatter list. Internal helper so both
    /// `check` and `check_path` share the same gate. Case-insensitive
    /// match (#7 fix): `deny_tools: [Edit]` correctly denies `edit`.
    fn is_prompt_denied(&self, tool: &str) -> bool {
        self.prompt_deny_tools
            .iter()
            .any(|t| t.eq_ignore_ascii_case(tool))
    }

    /// Public deny-list probe, used by code paths that route through
    /// `check_perm` with a UMBRELLA tool name (e.g. MCP tools always
    /// pass `"mcp_tool"`) and need to additionally check the
    /// CONCRETE name the LLM would think of (e.g. an MCP-exported
    /// `edit` should be blocked if the active prompt denies `edit`).
    /// Returns true if ANY of the supplied names hits the deny-list.
    pub fn any_prompt_denied(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.is_prompt_denied(n))
    }

    /// dirge-mzs4: like [`Self::check`] for the `bash` tool, but
    /// upgrades a final `Ask` outcome to `Allowed` when the caller
    /// has established that the segment's ONLY filesystem-touching
    /// effect is a `/dev/null` redirect. Writing to `/dev/null`
    /// discards data with no observable side effect, so there's no
    /// reason to prompt for that subset of commands.
    ///
    /// Deny rules still fire (the default `rm -rf /**` deny will
    /// reject `rm -rf / > /dev/null`), as does the doom-loop tracker;
    /// the only behavioural difference is the post-step that converts
    /// `Ask → Allowed`. Mode coercions, prompt-level deny lists, and
    /// the session allowlist all run through unchanged.
    ///
    /// Gated on `feature = "semantic-bash"` to match the only call
    /// site in `agent::tools::bash` — without that feature the
    /// method is dead code.
    #[cfg(feature = "semantic-bash")]
    pub fn check_bash_dev_null_softallow(&mut self, input: &str) -> CheckResult {
        match self.check("bash", input) {
            CheckResult::Ask => CheckResult::Allowed,
            other => other,
        }
    }

    /// Decision for a non-path tool input (bash command, mcp id,
    /// memory/skill action, grep pattern…). Delegates to the unified
    /// engine. Retained as a convenience wrapper for the `/allow`
    /// surface, `check_bash_dev_null_softallow`, and the test suite;
    /// the engine is the single source of truth.
    pub fn check(&mut self, tool: &str, input: &str) -> CheckResult {
        effect_to_result(self.authorize_scope(tool, input, false))
    }

    /// Decision for a filesystem-path tool input. Path classification
    /// (resolved / in_cwd / dev_null) happens inside `authorize_scope`.
    pub fn check_path(&mut self, tool: &str, path: &str) -> CheckResult {
        // Reject obvious LLM hallucinations ("1", "a") before the
        // engine — preserves the old `validate_path` guard.
        if let Err(reason) = path::validate_path(path) {
            return CheckResult::Denied(reason);
        }
        effect_to_result(self.authorize_scope(tool, path, true))
    }

    fn is_session_allowed(&self, tool: &str, input: &str) -> bool {
        allowlist::is_allowed(&self.session_allowlist, tool, input)
    }

    /// Side-effect-free re-check of ONLY the session allowlist (the
    /// state a fresh "allow always" mutates) for a pending request.
    /// Unlike [`Self::check`] / [`Self::check_path`] it does NOT touch
    /// the doom-loop counters or apply mode coercion — it answers the
    /// narrow question "would the current session allowlist allow this
    /// right now?".
    ///
    /// Used by the UI to coalesce parallel-tool permission prompts:
    /// when the agent fires several tool calls at once, each that needs
    /// permission queues its own request. If the user picks "allow
    /// always" on the first, the queued siblings that the new pattern
    /// now covers should be auto-allowed instead of re-prompting (and
    /// re-flashing the Alert avatar). Mirrors the raw-vs-path dispatch
    /// and the resolve-both-forms logic of the real checks so a
    /// relative allow-always pattern matches an absolute probe.
    pub fn session_allows_now(&self, tool: &str, input: &str) -> bool {
        // Read the ENGINE allowlist (the runtime source of truth that
        // `enforce` consults), op-scoped.
        let op = engine::tool_operation(tool);
        let al = &self.engine.ctx().allowlist;
        if is_path_tool_name(tool) {
            let abs = resolve_absolute(input, &self.working_dir);
            al.allows(op, input) || al.allows(op, &abs)
        } else {
            al.allows(op, input)
        }
    }

    pub fn add_session_allowlist(&mut self, tool: String, pattern_str: &str) {
        // dirge-yevn fix #1: register the pattern AND a
        // canonicalized variant for path-tool entries so the check
        // hits whichever form the upstream path arrives in (raw vs
        // canonical, symlinked vs realpath). The UI's
        // `suggest_pattern` derives the pattern from the input the
        // LLM passed (often the symlinked form), but `check_path`
        // canonicalizes the probe path via `resolve_absolute`. Prior
        // to this fix, a user who "Allow always"'d a write under
        // `/tmp/proj/src/` on macOS got the pattern stored as
        // `/tmp/proj/src/**` while subsequent checks compared against
        // `/private/tmp/proj/src/foo.rs` — no match, re-prompt.
        register_with_canonical_variant(
            &mut self.session_allowlist,
            &tool,
            pattern_str,
            &self.working_dir,
        );
        // F2 write↔edit↔apply_patch aliasing: when the user "always
        // allows" any of these three, also register the pattern under
        // the OTHER TWO so the alias check in enforce() doesn't
        // re-prompt. Without this, a user who "always allows" write
        // gets asked again on the next write because the edit-alias
        // check returns Ask with no allowlist match.
        //
        // dirge-yevn fix #2: previously this only mirrored
        // write→edit and edit→{write,apply_patch}, leaving
        // apply_patch→write unmirrored. Result: an "Allow always" on
        // a write left apply_patch's own rules (in the checker's
        // `check_path("apply_patch", ...)`) with no allowlist entry,
        // so a subsequent apply_patch call re-prompted. The fix is
        // full bidirectional mirroring across the three aliases.
        let aliases: &[&str] = match tool.as_str() {
            "write" => &["edit", "apply_patch"],
            "edit" => &["write", "apply_patch"],
            "apply_patch" => &["write", "edit"],
            _ => &[],
        };
        for alias in aliases {
            register_with_canonical_variant(
                &mut self.session_allowlist,
                alias,
                pattern_str,
                &self.working_dir,
            );
        }

        // Engine (runtime source of truth). Op-scoped: write/edit/
        // apply_patch all map to Operation::Edit, so a single grant
        // covers the trio — no mirroring needed. Add a canonical
        // variant for path tools so a relative "allow always" pattern
        // matches the absolute probe the engine checks against.
        let op = engine::tool_operation(&tool);
        self.engine.allow_always(op, pattern_str);
        if is_path_tool_name(&tool)
            && let Some(canon) = canonicalize_path_pattern(pattern_str, &self.working_dir)
            && canon != pattern_str
        {
            self.engine.allow_always(op, &canon);
        }
    }

    pub fn load_session_allowlist(&mut self, entries: &[(String, String)]) {
        // Route through add_session_allowlist (not allowlist::add
        // directly) so the write↔edit alias mirroring fires for
        // persisted sessions too.
        for (tool, pat) in entries {
            self.add_session_allowlist(tool.clone(), pat);
        }
    }

    pub fn allowlist_entries(&self) -> Vec<(String, String)> {
        allowlist::entries(&self.session_allowlist)
    }

    /// Remove the allowlist entry at the given index (0-based,
    /// matching the display order in `/allow list`). Returns the
    /// removed `(tool, pattern)` on success, or `None` if the
    /// index is out of range. Used by `/allow remove <n>`.
    pub fn remove_session_allowlist_at(&mut self, idx: usize) -> Option<(String, String)> {
        allowlist::remove_at(&mut self.session_allowlist, idx)
    }

    /// Remove ALL allowlist entries. Used by `/allow clear`.
    pub fn clear_session_allowlist(&mut self) {
        allowlist::clear(&mut self.session_allowlist);
        self.engine.ctx_mut().allowlist.clear();
    }

    pub fn set_mode(&mut self, mode: SecurityMode) {
        self.mode = mode;
    }

    /// Resolve a possibly-relative, possibly-symlinked path to its
    /// canonical form using the checker's own working_dir.
    /// Exposes `resolve_absolute` to callers that need the same
    /// canonical path the check ran against (audit H12 — pass this
    /// to `File::open` instead of the raw `args.path` to close the
    /// symlink-swap TOCTOU between check and open).
    pub fn resolve_path_for_tool(&self, path: &str) -> String {
        resolve_absolute(path, &self.working_dir)
    }

    /// Count of explicit `Deny` rules across all tools + the
    /// external-directory ruleset. Used by the host to warn the user
    /// when Yolo mode is active alongside non-empty deny rules —
    /// Yolo unconditionally returns `Allowed` before any rule
    /// lookup, so those deny rules are silently inert (audit H11).
    pub fn deny_rule_count(&self) -> usize {
        let in_tool_rules: usize = self
            .rules
            .values()
            .map(|v| v.iter().filter(|(_, a)| *a == Action::Deny).count())
            .sum();
        let in_ext_dir = self
            .ext_dir_rules
            .iter()
            .filter(|(_, a)| *a == Action::Deny)
            .count();
        in_tool_rules + in_ext_dir
    }

    pub fn mode(&self) -> SecurityMode {
        self.mode
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        self.working_dir = dir.to_string();
        self.working_dir_canonical = canonicalize_for_cache(dir);
        // Refresh the CWD-scoped builtin-allow rules so the new
        // project gets its own auto-allow and the OLD pattern
        // doesn't keep matching after cd. Surgically removes only
        // the previously-installed pattern (identified by
        // `pattern.original`) so user-configured rules pushed onto
        // the same Vec stay intact.
        if let Some(old_pat) = self.cwd_allow_pattern.take() {
            for tool in ["write", "edit", "apply_patch"] {
                if let Some(entries) = self.rules.get_mut(tool) {
                    entries.retain(|(p, _)| p.original != old_pat);
                }
            }
        }
        self.cwd_allow_pattern = install_cwd_allow_rules(&mut self.rules, dir);
        // B3-5 (audit fix): clear session-scoped state that was
        // implicitly tied to the OLD cwd. Two concerns:
        //   1. `recent_calls` is the doom-loop counter — stale
        //      entries from before the cd would falsely trip the
        //      3-identical-calls limiter on the first calls in
        //      the new project.
        //   2. `session_allowlist` holds patterns the user
        //      approved for the prior project (e.g. `cd *`,
        //      `cargo *`). Carrying them silently to a new
        //      project means the user has implicitly granted
        //      those permissions there too — a privilege carry-
        //      over the audit flagged. Pi rebuilds the session
        //      on cwd change.
        self.recent_calls.clear();
        self.repeat_counts.clear();
        self.session_allowlist.clear();
        // Mirror the reset into the engine: a cwd change drops the
        // loop-guard counters and session grants tied to the old
        // project (privilege carry-over guard).
        self.engine.ctx_mut().repeat.clear();
        self.engine.ctx_mut().allowlist.clear();
    }

    fn is_path_tool(&self, tool: &str) -> bool {
        // Must match `is_path_tool_name` — these are the tools that
        // take a filesystem path as their permission input and need
        // `external_directory` rule consultation. `apply_patch` and
        // `lsp` are included because both route filesystem-path
        // strings through `check_perm_path`.
        is_path_tool_name(tool)
    }

    pub fn is_external_path(&self, path_str: &str) -> bool {
        // F18: previously `!is_absolute → return false`, which
        // treated `../../etc/passwd` as "internal" (not external).
        // In Accept mode that bypassed external_directory rules:
        // a relative `../../secret` would auto-allow because it
        // wasn't classified external. Now we resolve relative
        // paths against the working_dir (same logic as
        // `resolve_absolute`) before the starts_with check.
        let resolved = resolve_absolute(path_str, &self.working_dir);
        let p = Path::new(&resolved);
        if !p.is_absolute() {
            // resolve_absolute fell back to lexical join and the
            // result is still relative — usually means working_dir
            // itself is bogus. Treat as not-external; rules will
            // fall through to the default action.
            return false;
        }
        let cwd = Path::new(&self.working_dir);
        // PERM-3: re-canonicalize at check time so a symlink
        // rewrite (or `working_dir_canonical` going stale for
        // any other reason) doesn't misclassify in-tree paths
        // as external (or vice versa). The cached
        // `working_dir_canonical` is kept as a fallback for
        // when the on-disk cwd has been removed/replaced.
        let fresh_canonical = canonicalize_for_cache(&self.working_dir);
        // Comparing against the fresh canonical, the cached
        // canonical, AND the literal form handles symlinked
        // roots like macOS's `/tmp → /private/tmp`: `resolved`
        // is canonical (`/private/tmp/...`) but `cwd` may still
        // be the literal `/tmp` form. Without all three checks
        // every in-tree access in such a setup would classify
        // as external.
        let canonical_cwd_cached = Path::new(&self.working_dir_canonical);
        let canonical_cwd_fresh = Path::new(&fresh_canonical);
        !p.starts_with(canonical_cwd_fresh)
            && !p.starts_with(canonical_cwd_cached)
            && !p.starts_with(cwd)
    }

    fn match_ext_dir(&self, path_str: &str) -> Option<Action> {
        for (pattern, action) in &self.ext_dir_rules {
            if pattern.matches(path_str) {
                return Some(*action);
            }
        }
        None
    }

    fn track_doom_loop(&mut self, tool: &str, input: &str) {
        let key = format!("{}\x00{}", tool, input);
        let count = self.repeat_counts.entry(key).or_insert(0);
        *count = count.saturating_add(1);
        // Maintain a FIFO ring for TTL-based eviction.
        // PERM-1: window 32 (was 16) so a 14-call decoy gap
        // can't flush a specific key before it repeats.
        self.recent_calls
            .push_back((tool.to_string(), input.to_string()));
        if self.recent_calls.len() > 32
            && let Some((t, i)) = self.recent_calls.pop_front()
        {
            let old_key = format!("{}\x00{}", t, i);
            if let Some(c) = self.repeat_counts.get_mut(&old_key) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    self.repeat_counts.remove(&old_key);
                }
            }
        }
    }

    fn is_doom_loop(&self, tool: &str, input: &str) -> bool {
        let key = format!("{}\x00{}", tool, input);
        // PERM-2: threshold is 2 (blocks on the 3rd identical call).
        // `track_doom_loop` fires AFTER this check, so the counter
        // reflects previous calls only — not the current one.
        self.repeat_counts.get(&key).copied().unwrap_or(0) >= 2
    }
}

/// One-shot canonicalize for the working-directory cache. Best
/// effort: if canonicalize fails (cwd doesn't exist on disk, e.g.
/// in tests that pass a fixture path), fall back to the literal
/// string so the `starts_with` comparisons in `is_external_path`
/// still work for the literal form.
fn canonicalize_for_cache(working_dir: &str) -> String {
    path::canonicalize_for_cache(working_dir)
}

/// Install the CWD-scoped builtin-allow rule on `rules` for the
/// mutating filesystem tools (write/edit/apply_patch). Returns the
/// pattern string installed (`Some`) so `set_working_dir` can find
/// and remove it on cd; `None` when the working_dir is too
/// degenerate to install safely.
///
/// Refuses to install when:
///   - `working_dir` is empty (config-only init w/o cwd resolution).
///   - The canonical form is `/` or shorter than 2 chars — the
///     resulting pattern (`/**`) would silently allow writes anywhere
///     on the filesystem, defeating the "permissive only inside the
///     project" intent.
///   - `working_dir` contains glob metacharacters (`*`, `?`, `[`,
///     `{`). Such characters would be re-interpreted by the glob
///     compiler rather than matched literally; a user starting dirge
///     from `/tmp/[odd]` would get a character-class pattern matching
///     unintended paths.
///
/// Uses `canonicalize_for_cache` so the pattern matches the canonical
/// form `resolve_absolute` produces. Without this, macOS users whose
/// `/var` / `/tmp` resolve to `/private/var` / `/private/tmp` would
/// see the rule silently fail to match for any abs_path the checker
/// computed.
fn install_cwd_allow_rules(
    rules: &mut HashMap<String, Vec<(Pattern, Action)>>,
    working_dir: &str,
) -> Option<String> {
    path::install_cwd_allow_rules(rules, working_dir)
}

/// Install a builtin-allow for `/dev/null` on every tool so the
/// harmless bit-bucket never triggers a permission prompt. Writes
/// to `/dev/null` discard data; reads return immediate EOF — no
/// side effects, no security risk, no reason to ask.
fn install_dev_null_allow(rules: &mut HashMap<String, Vec<(Pattern, Action)>>) {
    path::install_dev_null_allow(rules)
}

pub(crate) fn resolve_absolute(path: &str, working_dir: &str) -> String {
    path::resolve_absolute(path, working_dir)
}

/// Register `pattern_str` under `tool` in the session allowlist,
/// and ALSO register a canonicalized variant when the pattern is a
/// path-tool entry whose literal prefix differs from its canonical
/// form. Closes the symlink-mismatch bug: a pattern derived from
/// the symlinked working_dir (e.g. `/tmp/proj/src/**`) wouldn't
/// otherwise match a canonicalized probe path (e.g.
/// `/private/tmp/proj/src/foo.rs`).
///
/// Non-path tools (`bash`, `mcp_tool`, etc.) skip the second
/// registration since their patterns aren't filesystem paths and
/// canonicalization is meaningless.
///
/// Dedup is handled by `allowlist::add`, so a no-op when the
/// canonical form already equals the original.
fn register_with_canonical_variant(
    allowlist: &mut Vec<(String, crate::permission::pattern::Pattern)>,
    tool: &str,
    pattern_str: &str,
    working_dir: &str,
) {
    allowlist::add(allowlist, tool, pattern_str);
    if !is_path_tool_name(tool) {
        return;
    }
    if let Some(canonical_pat) = canonicalize_path_pattern(pattern_str, working_dir)
        && canonical_pat != pattern_str
    {
        allowlist::add(allowlist, tool, &canonical_pat);
    }
}

/// Best-effort canonicalize the literal-prefix portion of a path
/// glob pattern. Splits on the first glob metacharacter (`*`, `?`,
/// `[`, `{`); canonicalizes the prefix; reassembles the pattern.
/// Used by `register_with_canonical_variant` to add a realpath-form
/// twin to a symlink-form session-allowlist pattern.
///
/// Returns `None` when:
///   - the literal prefix is empty (pattern starts with a glob),
///   - `canonicalize` fails AND the prefix doesn't resolve via
///     `resolve_absolute` (relative path that doesn't exist on
///     disk and `working_dir` itself is bogus).
fn canonicalize_path_pattern(pattern_str: &str, working_dir: &str) -> Option<String> {
    let split_idx = pattern_str
        .find(['*', '?', '[', '{'])
        .unwrap_or(pattern_str.len());
    if split_idx == 0 {
        return None;
    }
    let (head, tail) = pattern_str.split_at(split_idx);
    // Trim a trailing `/` from the head so the canonicalize call
    // operates on the directory itself; we re-attach the slash
    // when reassembling. Without this, a head like
    // `/tmp/proj/src/` would round-trip as `/private/tmp/proj/src`
    // (no trailing slash) and the reassembled pattern would lose
    // a slash compared to the original.
    let (head_trimmed, had_trailing_slash) = match head.strip_suffix('/') {
        Some(stripped) => (stripped, true),
        None => (head, false),
    };
    if head_trimmed.is_empty() {
        return None;
    }
    // RELATIVE-HEAD ANCHORING (re-prompt bug): `suggest_pattern`
    // derives a path-tool pattern from the parent of the LLM's input.
    // When the LLM sends a relative path (e.g. `src/main.rs`), the
    // stored pattern is the RELATIVE glob `src/**`, which compiles to
    // `^src(?:/.*)?$`. But `check_path` always matches against the
    // canonical ABSOLUTE form via `resolve_absolute`, so the next call
    // (especially when the LLM sends an absolute path, or the same
    // file resolved through the cwd) never matches the relative
    // pattern and the user is re-prompted despite "allow always".
    //
    // The canonical twin must be anchored at the CHECKER's
    // `working_dir`, not the process cwd. A bare `std::fs::canonicalize`
    // on a relative head resolves against `std::env::current_dir()`,
    // which can differ from the checker's working_dir (the agent may
    // have `cd`'d via `set_working_dir`). For relative heads, anchor at
    // `working_dir` first; this keeps the boundary tight — the twin can
    // only point inside `working_dir` (or wherever the symlink-followed
    // canonical path lands), never escaping to an arbitrary absolute
    // location chosen by the LLM.
    if !std::path::Path::new(head_trimmed).is_absolute() {
        let resolved = resolve_absolute(head_trimmed, working_dir);
        if resolved != head_trimmed {
            let mut out = resolved;
            if had_trailing_slash {
                out.push('/');
            }
            out.push_str(tail);
            return Some(out);
        }
    }
    let canonical_head = std::fs::canonicalize(head_trimmed)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            // Fallback 1: try resolving as a possibly-relative path
            // anchored at working_dir. Only useful when the head
            // exists on disk; resolve_absolute is best-effort.
            let resolved = resolve_absolute(head_trimmed, working_dir);
            if resolved != head_trimmed {
                return Some(resolved);
            }
            // Fallback 2: the literal head doesn't exist (yet) —
            // walk up to the closest existing ancestor, canonicalize
            // THAT, and project the missing suffix back on. Handles
            // "Allow always" on a not-yet-existent path that gets
            // created later (e.g. user opts into a directory that
            // doesn't exist; the next operation creates it; the
            // canonicalised probe would otherwise diverge from the
            // stored symlink-form pattern). See the symlink discussion
            // in `register_with_canonical_variant`.
            project_canonical_from_existing_ancestor(head_trimmed)
        })?;
    let mut out = canonical_head;
    if had_trailing_slash {
        out.push('/');
    }
    out.push_str(tail);
    Some(out)
}

/// Walk up the ancestors of `path` until we find one that exists on
/// disk, canonicalize that, and re-attach the missing-from-disk
/// suffix. Returns `None` when no ancestor canonicalizes (e.g.
/// pathological inputs or filesystem permission errors). Used by
/// `canonicalize_path_pattern` to handle "Allow always" on a
/// not-yet-existent path that's later created.
fn project_canonical_from_existing_ancestor(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let mut tail_components: Vec<&std::ffi::OsStr> = Vec::new();
    let mut anchor = p;
    loop {
        match anchor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                // Cache the component we're stripping so we can
                // re-attach it after canonicalizing the parent.
                if let Some(name) = anchor.file_name() {
                    tail_components.push(name);
                }
                anchor = parent;
                if let Ok(canonical) = std::fs::canonicalize(anchor) {
                    let mut out = canonical;
                    for name in tail_components.iter().rev() {
                        out.push(name);
                    }
                    return Some(out.to_string_lossy().into_owned());
                }
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
#[path = "checker_tests.rs"]
mod tests;
