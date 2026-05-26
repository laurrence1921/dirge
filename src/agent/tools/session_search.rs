use std::path::PathBuf;

use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::extras::session_db::SessionDb;
use crate::extras::session_search::SessionSearch;

/// Tool wrapping `SessionSearch` — search past sessions on this project.
/// Three shapes: discover (query), scroll (session_id + message_id), browse (no args).
pub struct SessionSearchTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    db_path: PathBuf,
    /// Exclude the current session from results.
    current_session_id: Option<String>,
}

impl SessionSearchTool {
    pub fn new(
        db_path: PathBuf,
        current_session_id: Option<String>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            db_path,
            current_session_id,
        }
    }

    fn open_search(&self) -> Result<SessionSearch, String> {
        let db = SessionDb::open(&self.db_path)?;
        let mut search = SessionSearch::new(db);
        if let Some(ref id) = self.current_session_id {
            search = search.with_current_session(id);
        }
        Ok(search)
    }
}

#[derive(Deserialize)]
pub struct SearchArgs {
    /// FTS5 query for DISCOVERY mode. Omit for BROWSE.
    #[serde(default)]
    query: Option<String>,
    /// Session id for SCROLL mode.
    #[serde(default)]
    session_id: Option<String>,
    /// Message id anchor for SCROLL mode.
    #[serde(default)]
    around_message_id: Option<i64>,
    /// Window size for SCROLL (default 5).
    #[serde(default = "default_window")]
    window: usize,
}

fn default_window() -> usize {
    5
}

impl Tool for SessionSearchTool {
    const NAME: &'static str = "session_search";

    type Error = ToolError;
    type Args = SearchArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "session_search".to_string(),
            description: r#"Search past sessions on this project. Three calling modes (inferred from args):

1. DISCOVERY: pass `query` — FTS5 full-text search. Returns top sessions with snippets, message windows around matches, and bookends (first/last messages). Deduped by session lineage. Zero LLM cost — pure DB queries.

2. SCROLL: pass `session_id` + `around_message_id` — returns a window of ±N messages centered on the anchor. No FTS5, no bookends. Re-anchor on last/first message id to scroll forward/backward.

3. BROWSE: no args — returns recent sessions chronologically (titles, previews, timestamps).

FTS5 syntax: AND (default), OR, NOT, "quoted phrases", * prefix wildcards."#
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "FTS5 query for DISCOVERY mode. Omit for BROWSE or SCROLL."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session id for SCROLL mode."
                    },
                    "around_message_id": {
                        "type": "integer",
                        "description": "Message id anchor for SCROLL mode."
                    },
                    "window": {
                        "type": "integer",
                        "description": "Window size for SCROLL (default 5, max 20)."
                    }
                },
                "required": []
            }),
        }
    }

    async fn call(&self, args: SearchArgs) -> Result<String, ToolError> {
        check_perm(&self.permission, &self.ask_tx, "session_search", "search").await?;

        let search = self.open_search().map_err(|e| ToolError::Msg(e))?;

        // Mode inference: query → DISCOVERY, session_id + message_id → SCROLL, else → BROWSE
        if let Some(ref query) = args.query.filter(|q| !q.trim().is_empty()) {
            let hits = search.discover(query).map_err(|e| ToolError::Msg(e))?;
            Ok(serde_json::to_string_pretty(&hits)
                .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
        } else if let (Some(sid), Some(msg_id)) = (&args.session_id, args.around_message_id) {
            let window = args.window.min(20).max(1);
            let result = search
                .scroll(sid, msg_id, window)
                .map_err(|e| ToolError::Msg(e))?;
            Ok(serde_json::to_string_pretty(&result)
                .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
        } else {
            let sessions = search.browse().map_err(|e| ToolError::Msg(e))?;
            Ok(serde_json::to_string_pretty(&sessions)
                .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_db_path() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "dirge-search-tool-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("state.db")
    }

    fn seed_session(db_path: &PathBuf, id: &str) {
        let db = SessionDb::open(db_path).unwrap();
        db.insert_session(id, "cli", "gpt-5", "openai", "2025-01-15T10:00:00Z")
            .unwrap();
        for i in 0..5 {
            db.insert_message(
                id,
                if i % 2 == 0 { "user" } else { "assistant" },
                &format!("message {} about database migrations", i),
                None,
                None,
                None,
                &format!("2025-01-15T10:{:02}:00Z", i),
            )
            .unwrap();
        }
    }

    fn make_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn test_browse_returns_empty_when_no_sessions() {
        let db_path = temp_db_path();
        let tool = SessionSearchTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(SearchArgs {
            query: None,
            session_id: None,
            around_message_id: None,
            window: 5,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.trim(), "[]");
    }

    #[test]
    fn test_browse_returns_seeded_sessions() {
        let db_path = temp_db_path();
        seed_session(&db_path, "sess-1");

        let tool = SessionSearchTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(SearchArgs {
            query: None,
            session_id: None,
            around_message_id: None,
            window: 5,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("sess-1"));
    }

    #[test]
    fn test_discover_finds_matching_sessions() {
        let db_path = temp_db_path();
        seed_session(&db_path, "sess-1");

        let tool = SessionSearchTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(SearchArgs {
            query: Some("database migrations".into()),
            session_id: None,
            around_message_id: None,
            window: 5,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("sess-1"), "should find sess-1: {output}");
    }

    #[test]
    fn test_discover_empty_for_no_match() {
        let db_path = temp_db_path();
        seed_session(&db_path, "sess-1");

        let tool = SessionSearchTool::new(db_path, None, None, None);
        let rt = make_runtime();

        let result = rt.block_on(tool.call(SearchArgs {
            query: Some("zzzzz_nonexistent_xyz".into()),
            session_id: None,
            around_message_id: None,
            window: 5,
        }));
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.trim(), "[]");
    }

    #[test]
    fn test_definition_includes_modes() {
        let db_path = temp_db_path();
        let tool = SessionSearchTool::new(db_path, None, None, None);
        let rt = make_runtime();
        let def = rt.block_on(tool.definition(String::new()));
        assert!(def.description.contains("DISCOVERY"));
        assert!(def.description.contains("SCROLL"));
        assert!(def.description.contains("BROWSE"));
    }
}
