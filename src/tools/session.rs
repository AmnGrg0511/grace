//! `session_search` — lets the model search its own past conversations.
//!
//! Without this, prior context only ever reached the model through the
//! invisible pre-flight recall pass (see [`crate::recall`]), which fires once
//! per turn on the user's opening prompt. That is the wrong shape for "what
//! did we decide about X last time" surfacing three tool calls deep. Backed by
//! the same FTS5 index `--search-sessions` uses.

use crate::session::SessionStore;
use crate::tools::r#trait::{arg_str, str_prop, Tool};
use crate::util::{AgentError, Result};
use serde_json::{json, Value};
use std::sync::Arc;

/// Full-text search over persisted chat history.
pub struct SessionSearchTool {
    store: Arc<SessionStore>,
}

impl SessionSearchTool {
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self { store }
    }

    /// Cap on how much of each hit is echoed back. A broad query against a
    /// long history can otherwise return more text than the context window
    /// holds — the same failure mode `read_file` guards against.
    const PREVIEW_CHARS: usize = 200;
}

impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Full-text search past chat sessions (persisted across restarts) for prior turns matching a query. Use this when the user references something from an earlier conversation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": str_prop("Search terms (FTS5 syntax: AND/OR/quoted phrases supported)."),
                "limit": {"type": "integer", "description": "Max results (default 10)."}
            },
            "required": ["query"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let query = arg_str(args, "query")?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as u32;
        let hits = self
            .store
            .search(&query, limit)
            .map_err(|e| AgentError::Tool(format!("session search failed: {e}")))?;
        if hits.is_empty() {
            return Ok(format!("no session history matches {query:?}."));
        }
        let mut out = String::new();
        for (session_id, content) in hits {
            let preview: String = content.chars().take(Self::PREVIEW_CHARS).collect();
            out.push_str(&format!("[{session_id}] {preview}\n"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    fn store_with(tag: &str, turns: &[(&str, &str)]) -> Arc<SessionStore> {
        let path = std::env::temp_dir().join(format!(
            "grace_session_tool_{}_{tag}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        for (sid, text) in turns {
            store.append(sid, &Message::user(*text)).unwrap();
        }
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(store)
    }

    #[test]
    fn finds_a_matching_prior_turn() {
        let store = store_with("hit", &[("s1", "we decided to use rustls for TLS")]);
        let tool = SessionSearchTool::new(store);
        let out = tool.run(&json!({"query": "rustls"})).unwrap();
        assert!(out.contains("rustls"));
        assert!(out.contains("[s1]"));
    }

    #[test]
    fn reports_no_matches_without_erroring() {
        // "no results" is information for the model, not a failure — an error
        // here would make it think the tool is broken and retry.
        let store = store_with("miss", &[("s1", "something unrelated")]);
        let tool = SessionSearchTool::new(store);
        let out = tool.run(&json!({"query": "quantumfoo"})).unwrap();
        assert!(out.contains("no session history matches"));
    }

    #[test]
    fn limit_is_respected() {
        let store = store_with(
            "limit",
            &[
                ("s1", "alpha marker"),
                ("s1", "beta marker"),
                ("s1", "gamma marker"),
            ],
        );
        let tool = SessionSearchTool::new(store);
        let out = tool.run(&json!({"query": "marker", "limit": 2})).unwrap();
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn long_hits_are_previewed_not_dumped_whole() {
        let long = "z".repeat(5000);
        let store = store_with("preview", &[("s1", &format!("needle {long}"))]);
        let tool = SessionSearchTool::new(store);
        let out = tool.run(&json!({"query": "needle"})).unwrap();
        assert!(
            out.len() < 400,
            "hits must be truncated to a preview, got {} chars",
            out.len()
        );
    }

    #[test]
    fn missing_query_argument_is_reported() {
        let store = store_with("noargs", &[]);
        let tool = SessionSearchTool::new(store);
        let err = tool.run(&json!({})).unwrap_err();
        assert!(err.to_string().contains("missing string argument 'query'"));
    }

    #[test]
    fn schema_declares_query_required() {
        let store = store_with("schema", &[]);
        let tool = SessionSearchTool::new(store);
        let p = tool.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "query");
    }
}
