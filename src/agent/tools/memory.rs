use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::memory_store::MemoryToolStore;

pub struct MemoryTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    store: Arc<MemoryToolStore>,
}

impl MemoryTool {
    pub fn new(
        store: Arc<MemoryToolStore>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            store,
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    action: String,
    #[serde(default = "default_target")]
    target: String,
    content: Option<String>,
    old_text: Option<String>,
}

fn default_target() -> String {
    "memory".to_string()
}

impl Tool for MemoryTool {
    const NAME: &'static str = "memory";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "memory".to_string(),
            description: r#"Persistent long-term memory. Actions: view [target] (read all entries), add (new entry), replace (update by substring match), remove (delete by substring match).

WHEN TO SAVE:
- User corrects you or says "remember this" / "don't do that again"
- You discover build commands, test runners, or project conventions
- You learn architecture patterns, library quirks, or naming conventions
- You identify a pitfall — something tried and failed, with the reason

TARGETS:
- "memory": project facts, conventions, build commands, architecture patterns
- "pitfalls": anti-patterns, things tried and failed, environment-specific issues

ACTIONS:
- view: read all entries in a target (no other args needed)
- add: create a new entry (needs content)
- replace: update existing entry found by old_text substring (needs old_text + content)
- remove: delete entry found by old_text substring (needs old_text)"#
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["view", "add", "replace", "remove"],
                        "description": "The action to perform."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["memory", "pitfalls"],
                        "description": "Which memory store: 'memory' for project facts, 'pitfalls' for anti-patterns."
                    },
                    "content": {
                        "type": "string",
                        "description": "The entry content. Required for 'add' and 'replace'."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Short unique substring identifying the entry to replace or remove."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "memory", &args.action).await?;

        let target = validate_target(&args.target)?;

        match args.action.as_str() {
            "view" => {
                let resp = self.store.view(target);
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "add" => {
                let content = args
                    .content
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| ToolError::Msg("content is required for 'add'".to_string()))?;
                let resp = self
                    .store
                    .add(target, content)
                    .map_err(|e| ToolError::Msg(e))?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "replace" => {
                let old_text = args
                    .old_text
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| ToolError::Msg("old_text is required for 'replace'".to_string()))?;
                let content = args
                    .content
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| {
                        ToolError::Msg("content is required for 'replace'".to_string())
                    })?;
                let resp = self
                    .store
                    .replace(target, old_text, content)
                    .map_err(|e| ToolError::Msg(e))?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            "remove" => {
                let old_text = args
                    .old_text
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| ToolError::Msg("old_text is required for 'remove'".to_string()))?;
                let resp = self
                    .store
                    .remove(target, old_text)
                    .map_err(|e| ToolError::Msg(e))?;
                Ok(serde_json::to_string_pretty(&resp)
                    .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
            }
            _ => Err(ToolError::Msg(format!(
                "Unknown action '{}'. Use: view, add, replace, remove.",
                args.action
            ))),
        }
    }
}

fn validate_target(target: &str) -> Result<&str, ToolError> {
    match target {
        "memory" | "pitfalls" => Ok(target),
        _ => Err(ToolError::Msg(format!(
            "Invalid target '{}'. Use 'memory' or 'pitfalls'.",
            target
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extras::dirge_paths::ProjectPaths;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_store() -> (Arc<MemoryToolStore>, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "dirge-mem-tool-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let paths = ProjectPaths::new(&dir);
        let store = Arc::new(MemoryToolStore::load(&paths).unwrap());
        (store, dir)
    }

    fn make_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn test_add_and_view() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        // Add an entry.
        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("build command: cargo build --release".into()),
            old_text: None,
        }));
        assert!(result.is_ok(), "add failed: {:?}", result);
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["entry_count"], 1);

        // View — should see the entry.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
        }));
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        let entries = resp["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].as_str().unwrap().contains("cargo build"));
    }

    #[test]
    fn test_add_to_pitfalls() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "pitfalls".into(),
            content: Some("Don't use async in the render loop".into()),
            old_text: None,
        }));
        assert!(result.is_ok());
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["target"], "pitfalls");
    }

    #[test]
    fn test_duplicate_rejected() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("same entry".into()),
            old_text: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("same entry".into()),
            old_text: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_replace_by_substring() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("build command: cargo build".into()),
            old_text: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "replace".into(),
            target: "memory".into(),
            content: Some("build command: cargo build --release".into()),
            old_text: Some("cargo build".into()),
        }));
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);

        // Verify the entry was replaced.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
        }));
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        let entries = resp["entries"].as_array().unwrap();
        assert!(entries[0].as_str().unwrap().contains("--release"));
    }

    #[test]
    fn test_remove_entry() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: Some("temp entry to remove".into()),
            old_text: None,
        }))
        .unwrap();

        let result = rt.block_on(tool.call(Args {
            action: "remove".into(),
            target: "memory".into(),
            content: None,
            old_text: Some("temp entry".into()),
        }));
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["success"], true);

        // Verify empty.
        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
        }));
        let resp: serde_json::Value =
            serde_json::from_str(&result.unwrap()).expect("valid JSON");
        assert_eq!(resp["entry_count"], 0);
    }

    #[test]
    fn test_invalid_target_rejected() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "view".into(),
            target: "user".into(),
            content: None,
            old_text: None,
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid target"));
    }

    #[test]
    fn test_missing_content_for_add() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(Args {
            action: "add".into(),
            target: "memory".into(),
            content: None,
            old_text: None,
        }));
        assert!(result.is_err());
    }

    #[test]
    fn test_definition_includes_both_targets() {
        let (store, _dir) = temp_store();
        let tool = MemoryTool::new(store, None, None);
        let rt = make_runtime();
        let def = rt.block_on(tool.definition(String::new()));
        assert!(def.description.contains("memory"));
        assert!(def.description.contains("pitfalls"));
    }
}
