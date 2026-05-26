pub(crate) mod apply_patch;
pub(crate) mod background;
mod bash;
pub(crate) mod cache;
pub(crate) mod edit;
mod find_files;
mod glob;
mod grep;
mod list_dir;
#[cfg(feature = "lsp")]
mod lsp;
mod memory;
pub(crate) mod modified;
pub(crate) mod plan;
pub(crate) mod question;
mod read;
mod repo_overview;
#[cfg(feature = "semantic")]
pub mod semantic;
mod session_search;
mod skill;
pub mod task;
mod task_status;
pub(crate) mod todo;
mod webfetch;
mod websearch;
pub(crate) mod write;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use cache::ToolCache;
pub use edit::EditTool;
pub use find_files::FindFilesTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
#[cfg(feature = "lsp")]
pub use lsp::LspTool;
pub use memory::MemoryTool;
pub use plan::{PlanEnterTool, PlanExitTool};
pub use question::QuestionTool;
pub use read::ReadTool;
pub use repo_overview::RepoOverviewTool;
pub use session_search::SessionSearchTool;
pub use skill::SkillTool;
pub use task::TaskTool;
pub use task_status::TaskStatusTool;
pub use todo::WriteTodoList;
pub use webfetch::WebFetchTool;
pub use websearch::WebSearchTool;
pub use write::WriteTool;

use std::io;

use serde::Deserialize;

use crate::permission::ask::{AskRequest, AskSender, UserDecision};
use crate::permission::checker::{CheckResult, PermCheck};

pub const MAX_GREP_RESULTS: usize = 200;
pub const MAX_FIND_RESULTS: usize = 200;

/// Single source of truth for every built-in tool name dirge ships.
/// Used by:
///   - `agent/builder.rs` MCP collision filter — refuses to register
///     an MCP-exported tool with a colliding name.
///   - `context/prompts.rs` `deny_tools` validation — warns when a
///     prompt's frontmatter names something not in this set.
/// Previously these two sites maintained independent lists; review-
/// batch #7 unified them so adding a new tool only requires one edit.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find_files",
    "glob",
    "list_dir",
    "write_todo_list",
    "apply_patch",
    "memory",
    "skill",
    "task",
    "task_status",
    "question",
    "webfetch",
    "websearch",
    "lsp",
    "repo_overview",
    "session_search",
    "list_symbols",
    "get_symbol_body",
    "find_definition",
    "find_callers",
    "find_callees",
    // plan_enter / plan_exit are unconditionally added when plan_tx
    // is in scope (they manage the plan mode state via plan_tx). An
    // MCP server exporting either name would shadow them and could
    // disable / hijack plan mode.
    "plan_enter",
    "plan_exit",
    // `mcp_tool` is the umbrella name McpTool calls go through.
    // Including it lets a prompt's `deny_tools: [mcp_tool]` deny
    // every MCP server's tools wholesale; the warn-on-unknown gate
    // in `context/prompts.rs` then accepts that entry.
    "mcp_tool",
];

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Msg(String),
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

pub fn is_skip_dir(name: &str) -> bool {
    matches!(name, "node_modules" | "target")
}

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub replace_all: Option<bool>,
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<u64>,
}

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_lines: Option<usize>,
    /// Include dotfiles / hidden files in the search. Default
    /// `false` — F2 carryover from find_files/glob/list_dir: grep
    /// also walks the filesystem and should not silently surface
    /// `.env`, `.git/` internals, etc. by default.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct FindFilesArgs {
    pub pattern: String,
    pub path: Option<String>,
    /// Include dotfiles / hidden files (e.g. `.env`, `.gitignore`).
    /// Default `false` — by default the listing skips hidden files
    /// so secrets in `.env` or `.git/` internals don't get pulled
    /// into LLM context inadvertently. Set `true` when the agent
    /// explicitly needs to inspect dotfiles.
    #[serde(default)]
    pub include_hidden: bool,
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    pub path: Option<String>,
    /// Include dotfiles in the listing. See `FindFilesArgs::include_hidden`
    /// for the rationale; default `false` for safety.
    #[serde(default)]
    pub include_hidden: bool,
}

async fn handle_ask_inner(
    ask_tx: &AskSender,
    permission: &PermCheck,
    tool: &str,
    input: &str,
) -> Result<(), ToolError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    ask_tx
        .send(AskRequest {
            tool: tool.to_string(),
            input: input.to_string(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| ToolError::Msg("Permission system unavailable".to_string()))?;
    match reply_rx.await {
        Ok(UserDecision::AllowOnce) => Ok(()),
        Ok(UserDecision::AllowAlways(pattern)) => {
            permission
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_session_allowlist(tool.to_string(), &pattern);
            Ok(())
        }
        _ => Err(ToolError::Msg("Permission denied by user".to_string())),
    }
}

/// Scope arg passed to the [`enforce`] chokepoint. Discriminates
/// path-style checks (`Path` / `PathResolve`, route through
/// `PermissionChecker::check_path`, glob with `*` excluding `/`) from
/// raw checks (`Raw`, route through `PermissionChecker::check`, shell-
/// style patterns where `*` matches across `/`).
///
/// `PathResolve` additionally canonicalizes the path (resolving
/// symlinks, normalizing `..`) and returns the resolved path so the
/// calling tool can open EXACTLY the path the user authorized
/// (audit H12 — TOCTOU symlink swap defense).
pub enum Scope<'a> {
    /// Non-path tool input. Examples: a bash command string, an MCP
    /// `server:tool` identifier, a grep pattern, a URL.
    Raw(&'a str),
    /// Filesystem path; check_path-style rule matching.
    Path(&'a str),
    /// Filesystem path with canonical resolution returned in the
    /// `Ok` value of [`enforce`]. Use this from tools that follow
    /// the permission check with a file open (read / write / edit /
    /// apply_patch) — the resolved path pins the file across the
    /// check↔open window.
    PathResolve(&'a str),
}

/// **Single chokepoint for all tool permission decisions in dirge.**
///
/// Ported from maki's `PermissionManager::enforce`
/// (`maki-agent/src/permissions.rs:283-350`): one function, one
/// signature, internal dispatch based on [`Scope`]. The legacy
/// `check_perm` / `check_perm_path` / `check_perm_path_resolve`
/// trio are retained as thin back-compat wrappers that delegate
/// here, so existing call sites continue to compile unchanged.
///
/// Returns the (possibly canonicalized) scope string on success.
/// `Raw` and `Path` scopes echo their input; `PathResolve` returns
/// the canonical path. Callers that don't need the return value
/// can discard with `enforce(...).await?;`.
///
/// Future milestones planning to compose against this chokepoint:
///   - **M2 (dirge-cep)**: replace per-tool `PermissionConfig`
///     fields with a uniform rule schema. `enforce` keeps its
///     signature; only the underlying checker changes.
///   - **M3 (dirge-6ab)**: tree-sitter-parse bash commands inside
///     `enforce` and recurse per-segment so `git diff && rm -rf /`
///     gets BOTH `git` AND `rm` checked. Currently the bash tool
///     does its own segmenting in [`crate::agent::tools::bash`];
///     M3 collapses that into the chokepoint.
///   - **M4 (dirge-ojn)**: flip unmatched-tool default from Allow
///     to Ask. Pure config change inside the underlying checker.
pub async fn enforce(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    scope: Scope<'_>,
) -> Result<String, ToolError> {
    let raw_scope: &str = match &scope {
        Scope::Raw(s) | Scope::Path(s) | Scope::PathResolve(s) => s,
    };

    let Some(perm) = permission else {
        // No checker installed (e.g. ACP / --no-tools paths). Pass
        // through with the original scope text — matches the legacy
        // `check_perm_path_resolve` fallback. Raw/Path callers
        // discard the return; PathResolve callers see the
        // unchanged input.
        return Ok(raw_scope.to_string());
    };

    // Inner pure-lookup helper. Reads the checker, returns
    // (CheckResult, resolved-path). Doesn't touch the ask flow —
    // that's separate so the F2 alias check can MERGE results
    // before any prompting fires.
    fn inner_check(
        guard: &mut crate::permission::checker::PermissionChecker,
        tool: &str,
        scope: &Scope<'_>,
    ) -> (CheckResult, String) {
        match scope {
            Scope::Raw(key) => (guard.check(tool, key), (*key).to_string()),
            Scope::Path(path) => (guard.check_path(tool, path), (*path).to_string()),
            Scope::PathResolve(path) => {
                let resolved = guard.resolve_path_for_tool(path);
                let r = guard.check_path(tool, path);
                (r, resolved)
            }
        }
    }

    // F2 (dirge-jlj): write / apply_patch alias to the `edit`
    // permission. Mirrors opencode's `EDIT_TOOLS` aliasing
    // (`permission/index.ts:291-301`): a user writing
    // `edit: { "**": "deny" }` blocks all three uniformly.
    //
    // Strategy: take the MOST RESTRICTIVE outcome between the
    // tool's own rules and the edit rules. Deny > Ask > Allow.
    // If the tool's specific rule allows but `edit` denies, the
    // edit deny wins — broader deny-list semantics. If both Ask,
    // we prompt ONCE (avoids double-prompting).
    let (result, resolved) = {
        let mut guard = perm.lock().unwrap_or_else(|e| e.into_inner());
        let primary = inner_check(&mut guard, tool, &scope);
        if matches!(tool, "write" | "apply_patch") {
            let alias = inner_check(&mut guard, "edit", &scope);
            // Combine: most restrictive wins. Use primary's
            // resolved-path (the tool's own canonicalization
            // anchored to its rule set, not edit's).
            let combined = match (&primary.0, &alias.0) {
                (CheckResult::Denied(reason), _) => CheckResult::Denied(reason.clone()),
                (_, CheckResult::Denied(reason)) => CheckResult::Denied(reason.clone()),
                (CheckResult::Ask, _) | (_, CheckResult::Ask) => CheckResult::Ask,
                _ => CheckResult::Allowed,
            };
            (combined, primary.1)
        } else {
            primary
        }
    };

    match result {
        CheckResult::Allowed => Ok(resolved),
        CheckResult::Denied(reason) => {
            Err(ToolError::Msg(format!("Permission denied: {}", reason)))
        }
        CheckResult::Ask => {
            let Some(tx) = ask_tx else {
                return Err(ToolError::Msg(
                    "Permission denied (non-interactive mode)".to_string(),
                ));
            };
            handle_ask_inner(tx, perm, tool, raw_scope).await?;
            Ok(resolved)
        }
    }
}

/// Back-compat wrapper for the legacy non-path check. Delegates to
/// [`enforce`] with [`Scope::Raw`]. New code should call `enforce`
/// directly.
pub async fn check_perm(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    input_key: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Raw(input_key))
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy path check. Delegates to
/// [`enforce`] with [`Scope::Path`]. New code should call `enforce`
/// directly.
pub async fn check_perm_path(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<(), ToolError> {
    enforce(permission, ask_tx, tool, Scope::Path(path))
        .await
        .map(|_| ())
}

/// Back-compat wrapper for the legacy resolve-and-check entrypoint.
/// Delegates to [`enforce`] with [`Scope::PathResolve`] and returns
/// the canonical path. New code should call `enforce` directly.
///
/// Tools that perform a follow-up file operation (read/edit/write/
/// apply_patch) MUST pass this canonical path to the file API
/// instead of re-using the original `args.path`. Without this, the
/// OS dereferences the symlink a SECOND time at open, and a swap
/// between check-time and open-time lands the operation on a
/// different file than the one the user authorized (audit H12).
pub async fn check_perm_path_resolve(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    tool: &str,
    path: &str,
) -> Result<String, ToolError> {
    enforce(permission, ask_tx, tool, Scope::PathResolve(path)).await
}

// `is_plan_file` and `canonicalize_or_parent` were removed when the
// prompt-level PLAN.md gate moved into the permission checker via
// `deny_tools` frontmatter. The few historical callers (WriteTool,
// EditTool, ApplyPatchTool) now drop the file-name comparison and
// rely on the prompt's deny-list to refuse the entire tool in plan
// mode.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{
        Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// F2 (dirge-jlj): `enforce(write, ...)` MUST also consult the
    /// `edit` rules. A user writing `edit: { "**": "deny" }`
    /// blocks `write` AND `apply_patch` too — matching opencode's
    /// `EDIT_TOOLS` aliasing.
    #[tokio::test]
    async fn enforce_write_aliases_to_edit_deny() {
        let mut edit_rules = HashMap::new();
        edit_rules.insert("**".to_string(), Action::Deny);
        let config = PermissionConfig {
            edit: Some(ToolPerm::Granular(edit_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(
            &config,
            SecurityMode::Standard,
            Some(std::path::PathBuf::from("/tmp")),
        );
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            matches!(result, Err(_)),
            "edit deny should propagate to write; got {result:?}",
        );

        let result = enforce(
            &Some(perm),
            &None,
            "apply_patch",
            Scope::PathResolve("/tmp/x.rs"),
        )
        .await;
        assert!(
            matches!(result, Err(_)),
            "edit deny should propagate to apply_patch; got {result:?}",
        );
    }

    /// F2: most-restrictive-wins. If `write` is explicitly Allow
    /// but `edit` is Deny, the Deny wins.
    #[tokio::test]
    async fn enforce_write_alias_most_restrictive_wins() {
        let mut edit_rules = HashMap::new();
        edit_rules.insert("/etc/**".to_string(), Action::Deny);
        let mut write_rules = HashMap::new();
        write_rules.insert("**".to_string(), Action::Allow);
        let config = PermissionConfig {
            edit: Some(ToolPerm::Granular(edit_rules)),
            write: Some(ToolPerm::Granular(write_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // `/etc/passwd`: write allows (`**`), edit denies (`/etc/**`).
        // More restrictive (deny) wins.
        let result = enforce(
            &Some(perm.clone()),
            &None,
            "write",
            Scope::PathResolve("/etc/passwd"),
        )
        .await;
        assert!(matches!(result, Err(_)));

        // `/tmp/x.rs`: write allows (`**`), edit's `/etc/**`
        // doesn't match → edit lookup = Ask (default), write = Allow.
        // Combined: Ask (more restrictive). No ask_tx → "non-interactive
        // mode" deny.
        let result = enforce(&Some(perm), &None, "write", Scope::PathResolve("/tmp/x.rs")).await;
        assert!(
            matches!(result, Err(_)),
            "/tmp/x.rs: write Allow + edit Ask → combined Ask → non-interactive deny; got {result:?}",
        );
    }

    /// F2 negative: tools NOT in EDIT_TOOLS aren't aliased.
    /// `read` shouldn't be affected by edit rules.
    #[tokio::test]
    async fn enforce_read_does_not_alias_to_edit() {
        let mut edit_rules = HashMap::new();
        edit_rules.insert("**".to_string(), Action::Deny);
        let config = PermissionConfig {
            edit: Some(ToolPerm::Granular(edit_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm: PermCheck = Arc::new(Mutex::new(checker));

        // read has builtin-allow `**: allow` → succeeds
        // regardless of edit's deny.
        let result = enforce(
            &Some(perm),
            &None,
            "read",
            Scope::PathResolve("anywhere.rs"),
        )
        .await;
        assert!(
            matches!(result, Ok(_)),
            "read isn't aliased to edit; should pass via builtin-allow; got {result:?}",
        );
    }
}
