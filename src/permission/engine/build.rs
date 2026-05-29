//! Construction of the standard dirge [`Engine`] from a
//! [`PermissionConfig`], plus the tool→[`Operation`] mapping and the
//! path classifier used to normalize a tool call into an
//! [`AccessRequest`].
//!
//! Translation is deliberately mechanical: only USER-supplied rules
//! (legacy per-tool fields + the `tools` map), the bash/mcp defaults,
//! and `external_directory` become [`Rule`]s. The transparent allows
//! (read-only tools, memory/skill, dev-null, in-cwd writes) are NOT
//! translated — they live in [`BuiltinAllowPolicy`] as code, so there
//! is no double-install. User rules outrank builtin-allow by virtue of
//! `ConfiguredRulePolicy` sitting above it in the decider order.

use std::path::PathBuf;

use super::policies::{
    AcceptModePolicy, BuiltinAllowPolicy, ConfiguredRulePolicy, DefaultActionPolicy,
    ExternalDirPolicy, LoopGuardPolicy, OpMatch, PromptDenyPolicy, Rule, SessionAllowlistPolicy,
    YoloPolicy,
};
use super::policy::{Decider, Modifier, PolicyCtx};
use super::types::{Effect, Operation, Resource};
use super::{Engine, classify::pattern_for_tool};
use crate::permission::path::{canonicalize_for_cache, resolve_absolute};
use crate::permission::{Action, PermissionConfig, ToolPerm};

/// Default retry-loop threshold: the Nth identical *prompted* request
/// is hard-denied. (The breaking config in Phase 4 makes this tunable.)
const LOOP_GUARD_THRESHOLD: u32 = 3;

impl From<Action> for Effect {
    fn from(a: Action) -> Effect {
        match a {
            Action::Allow => Effect::Allow,
            Action::Ask => Effect::Ask,
            Action::Deny => Effect::Deny,
        }
    }
}

/// Map a concrete tool name to its coarse [`Operation`].
pub fn tool_operation(tool: &str) -> Operation {
    match tool {
        "read" | "grep" | "find_files" | "glob" | "list_dir" | "repo_overview" | "lsp"
        | "list_symbols" | "get_symbol_body" | "find_definition" | "find_callers"
        | "find_callees" => Operation::Read,
        "write" => Operation::Write,
        "edit" | "apply_patch" => Operation::Edit,
        "bash" | "shell" => Operation::Execute,
        "webfetch" | "websearch" => Operation::Network,
        "mcp_tool" => Operation::Mcp,
        "memory" => Operation::Memory,
        "skill" => Operation::Skill,
        "task" | "task_status" | "question" | "write_todo_list" => Operation::Meta,
        // Unknown (plugin) tools: treat as meta — they fall to the
        // configured rules / default like anything else.
        _ => Operation::Meta,
    }
}

/// Build a `Path` resource from a raw path string, computing the
/// canonical form, whether it is inside `working_dir`, and whether it
/// is `/dev/null`. This is the single place path classification
/// happens (replacing the scattered `install_cwd_allow_rules` /
/// `install_dev_null_allow` / `is_external_path` logic).
pub fn classify_path(raw: &str, working_dir: &str) -> Resource {
    let resolved_str = resolve_absolute(raw, working_dir);
    let dev_null = resolved_str == "/dev/null" || raw == "/dev/null";
    let cwd_canonical = canonicalize_for_cache(working_dir);
    let trimmed = cwd_canonical.trim_end_matches('/');
    let in_cwd = !trimmed.is_empty()
        && trimmed != "/"
        && (resolved_str == trimmed || resolved_str.starts_with(&format!("{trimmed}/")));
    Resource::Path {
        raw: raw.to_string(),
        resolved: PathBuf::from(resolved_str),
        in_cwd,
        dev_null,
    }
}

/// Translate one legacy `ToolPerm` (or `tools`-map entry) into rules
/// narrowed to `tool`.
fn rules_from_tool_perm(tool: &str, tp: &ToolPerm) -> Vec<Rule> {
    let op = OpMatch::One(tool_operation(tool));
    match tp {
        ToolPerm::Simple(action) => vec![Rule {
            op,
            tool: Some(tool.to_string()),
            pattern: pattern_for_tool(tool, "*"),
            effect: (*action).into(),
            original: format!("{tool}:*"),
        }],
        ToolPerm::Granular(map) => map
            .iter()
            .map(|(pat, action)| Rule {
                op,
                tool: Some(tool.to_string()),
                pattern: pattern_for_tool(tool, pat),
                effect: (*action).into(),
                original: format!("{tool}:{pat}"),
            })
            .collect(),
    }
}

impl Engine {
    /// Assemble the standard dirge policy set from configuration. The
    /// decider order encodes precedence; see `policies.rs`.
    pub fn from_config(config: &PermissionConfig) -> Engine {
        let mut rules: Vec<Rule> = Vec::new();
        let mut user_configured: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Legacy per-tool fields, in a stable order.
        let legacy: [(&str, &Option<ToolPerm>); 25] = [
            ("bash", &config.bash),
            ("read", &config.read),
            ("write", &config.write),
            ("edit", &config.edit),
            ("grep", &config.grep),
            ("find_files", &config.find_files),
            ("list_dir", &config.list_dir),
            ("glob", &config.glob),
            ("repo_overview", &config.repo_overview),
            ("write_todo_list", &config.write_todo_list),
            ("apply_patch", &config.apply_patch),
            ("lsp", &config.lsp),
            ("question", &config.question),
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
        ];
        for (tool, tp) in legacy {
            if let Some(tp) = tp {
                rules.extend(rules_from_tool_perm(tool, tp));
                user_configured.insert(tool.to_string());
            }
        }

        // M2 `tools` map (appended after legacy → last-match-wins).
        if let Some(tools_map) = &config.tools {
            for (tool, tp) in tools_map {
                rules.extend(rules_from_tool_perm(tool, tp));
                user_configured.insert(tool.clone());
            }
        }

        // Bash defaults — only if the user supplied no bash rules.
        if !user_configured.contains("bash") {
            for (pat, action) in crate::permission::default_bash_rules() {
                rules.push(Rule {
                    op: OpMatch::One(Operation::Execute),
                    tool: Some("bash".to_string()),
                    pattern: pattern_for_tool("bash", pat),
                    effect: action.into(),
                    original: format!("bash:{pat}"),
                });
            }
        }

        // MCP default: prompt unless the user pinned a server.
        if !user_configured.contains("mcp_tool") {
            rules.push(Rule {
                op: OpMatch::One(Operation::Mcp),
                tool: Some("mcp_tool".to_string()),
                pattern: pattern_for_tool("mcp_tool", "*"),
                effect: Effect::Ask,
                original: "mcp_tool:*".to_string(),
            });
        }

        // external_directory rules (path-style patterns).
        let ext_rules: Vec<Rule> = config
            .external_directory
            .as_ref()
            .map(|m| {
                m.iter()
                    .map(|(pat, action)| Rule {
                        op: OpMatch::Any,
                        tool: None,
                        pattern: crate::permission::pattern::Pattern::new(pat),
                        effect: (*action).into(),
                        original: pat.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let default: Effect = config.default.unwrap_or(Action::Ask).into();

        let deciders: Vec<Box<dyn Decider>> = vec![
            Box::new(PromptDenyPolicy),
            Box::new(YoloPolicy),
            Box::new(SessionAllowlistPolicy),
            Box::new(ConfiguredRulePolicy { rules }),
            Box::new(BuiltinAllowPolicy),
            Box::new(ExternalDirPolicy { rules: ext_rules }),
            Box::new(AcceptModePolicy),
            Box::new(DefaultActionPolicy { default }),
        ];
        let modifiers: Vec<Box<dyn Modifier>> = vec![Box::new(LoopGuardPolicy {
            threshold: LOOP_GUARD_THRESHOLD,
        })];

        Engine::new(deciders, modifiers, PolicyCtx::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::SecurityMode;
    use crate::permission::engine::types::AccessRequest;
    use std::collections::HashMap;

    fn req(
        op: Operation,
        tool: &str,
        mode: SecurityMode,
        resources: Vec<Resource>,
    ) -> AccessRequest {
        AccessRequest {
            op,
            tool: tool.to_string(),
            resources,
            mode,
            display_input: String::new(),
        }
    }

    #[test]
    fn tool_operation_mapping() {
        assert_eq!(tool_operation("read"), Operation::Read);
        assert_eq!(tool_operation("grep"), Operation::Read);
        assert_eq!(tool_operation("write"), Operation::Write);
        assert_eq!(tool_operation("edit"), Operation::Edit);
        assert_eq!(tool_operation("apply_patch"), Operation::Edit);
        assert_eq!(tool_operation("bash"), Operation::Execute);
        assert_eq!(tool_operation("webfetch"), Operation::Network);
        assert_eq!(tool_operation("mcp_tool"), Operation::Mcp);
        assert_eq!(tool_operation("memory"), Operation::Memory);
        assert_eq!(tool_operation("skill"), Operation::Skill);
        assert_eq!(tool_operation("question"), Operation::Meta);
    }

    #[test]
    fn classify_path_in_cwd_dev_null_external() {
        let p = classify_path("/proj/src/x.rs", "/proj");
        assert!(matches!(
            p,
            Resource::Path {
                in_cwd: true,
                dev_null: false,
                ..
            }
        ));
        let p = classify_path("/dev/null", "/proj");
        assert!(matches!(p, Resource::Path { dev_null: true, .. }));
        let p = classify_path("/etc/passwd", "/proj");
        assert!(matches!(
            p,
            Resource::Path {
                in_cwd: false,
                dev_null: false,
                ..
            }
        ));
    }

    #[test]
    fn default_config_bash_defaults_present() {
        let e = Engine::from_config(&PermissionConfig::default());
        // A safe default bash command (git status) should be allowed;
        // an unfamiliar one falls to default Ask.
        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "git status -s".into(),
                head: "git".into(),
            }],
        ));
        assert_eq!(
            d.effect,
            Effect::Allow,
            "git status -s is a default-allowed bash command"
        );

        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "frobnicate --hard".into(),
                head: "frobnicate".into(),
            }],
        ));
        assert_eq!(d.effect, Effect::Ask, "unknown bash command prompts");
    }

    #[test]
    fn user_bash_rule_suppresses_defaults() {
        let mut tools = HashMap::new();
        tools.insert("bash".to_string(), ToolPerm::Simple(Action::Allow));
        let cfg = PermissionConfig {
            tools: Some(tools),
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        // With a blanket user `bash: allow`, even an unknown command is allowed
        let d = e.authorize(&req(
            Operation::Execute,
            "bash",
            SecurityMode::Standard,
            vec![Resource::Command {
                raw: "frobnicate".into(),
                head: "frobnicate".into(),
            }],
        ));
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn user_rule_overrides_builtin_allow() {
        // read is builtin-allowed; a user deny rule must win.
        let mut read = HashMap::new();
        read.insert("/secret/**".to_string(), Action::Deny);
        let cfg = PermissionConfig {
            read: Some(ToolPerm::Granular(read)),
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        let d = e.authorize(&req(
            Operation::Read,
            "read",
            SecurityMode::Standard,
            vec![classify_path("/secret/k", "/proj")],
        ));
        assert_eq!(
            d.effect,
            Effect::Deny,
            "user read deny rule beats builtin-allow"
        );
        // a non-secret read is still allowed
        let d = e.authorize(&req(
            Operation::Read,
            "read",
            SecurityMode::Standard,
            vec![classify_path("/proj/ok.rs", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Allow);
    }

    #[test]
    fn external_directory_rule_allows_outside_path() {
        let mut ext = HashMap::new();
        ext.insert("/shared/**".to_string(), Action::Allow);
        let cfg = PermissionConfig {
            external_directory: Some(ext),
            ..Default::default()
        };
        let e = Engine::from_config(&cfg);
        // external write to /shared is allowed by the ext-dir rule
        let d = e.authorize(&req(
            Operation::Write,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/shared/lib/x", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Allow);
        // external write elsewhere still asks
        let d = e.authorize(&req(
            Operation::Write,
            "write",
            SecurityMode::Standard,
            vec![classify_path("/etc/x", "/proj")],
        ));
        assert_eq!(d.effect, Effect::Ask);
    }
}
