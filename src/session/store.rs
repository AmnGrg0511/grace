//! Session persistence — chat history that survives restarts, searchable.
//!
//! Backed by the same rusqlite pattern as `memory`, plus an FTS5 virtual
//! table so past turns are actually searchable (not just replayable). This is
//! the concrete fix for "--chat forgets everything on exit": a session id
//! groups messages; `--chat --session <id>` (or the default id) resumes.

use crate::util::{AgentError, Result};
use crate::message::{Message, Role};
use rusqlite::Connection;
use std::path::PathBuf;

pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AgentError::Tool(format!("create session dir: {e}")))?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| AgentError::Tool(format!("open session db {}: {e}", path.display())))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE TABLE IF NOT EXISTS session_titles (
                session_id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content, session_id UNINDEXED, content='messages', content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content, session_id) VALUES (new.id, new.content, new.session_id);
            END;
            CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, content, session_id)
                    VALUES('delete', old.id, old.content, old.session_id);
            END;",
        )
        .map_err(|e| AgentError::Tool(format!("init session schema: {e}")))?;
        Ok(Self { conn })
    }

    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("sessions.db")
    }

    /// Append one message to a session's history.
    pub fn append(&self, session_id: &str, msg: &Message) -> Result<()> {
        // Only persist user/assistant text turns; tool/system noise is
        // reconstructed fresh each run rather than replayed verbatim.
        if msg.content.is_empty() {
            return Ok(());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
                (session_id, msg.role.as_str(), &msg.content, now),
            )
            .map_err(|e| AgentError::Tool(format!("append message: {e}")))?;
        Ok(())
    }

    /// Remove the most recent `user` row of a session — the prompt of a turn
    /// that failed before producing an answer. Without this, every failed
    /// (or Ctrl-C'd-into-a-hard-error) turn leaves a dangling user message
    /// in the on-disk history. Returns the number of rows removed (0 or 1);
    /// the `AFTER DELETE` trigger keeps the FTS index in sync.
    pub fn delete_last_user_row(&self, session_id: &str) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "DELETE FROM messages WHERE id = (
                    SELECT id FROM messages WHERE session_id = ?1 AND role = 'user'
                    ORDER BY id DESC LIMIT 1
                 )",
                [session_id],
            )
            .map_err(|e| AgentError::Tool(format!("delete last user row: {e}")))?;
        Ok(n)
    }

    /// Load a session's prior turns as replayable `Message`s (user/assistant
    /// only), oldest first.
    pub fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self
            .conn
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id ASC")
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([session_id], |row| {
                let role: String = row.get(0)?;
                let content: String = row.get(1)?;
                Ok((role, content))
            })
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            let (role, content) = r.map_err(|e| AgentError::Tool(format!("row: {e}")))?;
            let msg = match role.as_str() {
                "user" => Message::user(content),
                "assistant" => Message::assistant(content),
                _ => Message {
                    role: Role::System,
                    content,
                    ..Default::default()
                },
            };
            out.push(msg);
        }
        Ok(out)
    }

    /// Full-text search across all sessions. Returns (session_id, snippet).
    /// A bare `*` (or empty query) means "everything" — FTS5 rejects `*` as
    /// invalid MATCH syntax, so that case is special-cased to a plain scan
    /// instead of surfacing a raw SQL error to the caller.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<(String, String)>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || trimmed == "*" {
            let mut stmt = self
                .conn
                .prepare("SELECT session_id, content FROM messages ORDER BY id DESC LIMIT ?1")
                .map_err(|e| AgentError::Tool(format!("prepare search: {e}")))?;
            let rows = stmt
                .query_map([limit], |row| {
                    let sid: String = row.get(0)?;
                    let content: String = row.get(1)?;
                    Ok((sid, content))
                })
                .map_err(|e| AgentError::Tool(format!("search query: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
            }
            return Ok(out);
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, content FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2",
            )
            .map_err(|e| AgentError::Tool(format!("prepare search: {e}")))?;
        let rows = stmt
            .query_map((trimmed, limit), |row| {
                let sid: String = row.get(0)?;
                let content: String = row.get(1)?;
                Ok((sid, content))
            })
            .map_err(|e| AgentError::Tool(format!("search query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
        }
        Ok(out)
    }
    /// Distinct session ids that have at least one message, most recently
    /// active first. Powers `grace --list-sessions`.
    pub fn list_sessions(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, MAX(created_at) AS last_at FROM messages \
                 GROUP BY session_id ORDER BY last_at DESC",
            )
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
        }
        Ok(out)
    }

    /// Set (or overwrite) a session's human-readable title — a short
    /// LLM-generated summary of what the conversation is about, so the
    /// `/session` picker shows "debugging the stdin race" instead of a raw
    /// UUID or a truncated first message that's often just "hi".
    pub fn set_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO session_titles (session_id, title) VALUES (?1, ?2) \
                 ON CONFLICT(session_id) DO UPDATE SET title = excluded.title",
                (session_id, title),
            )
            .map_err(|e| AgentError::Tool(format!("set title: {e}")))?;
        Ok(())
    }

    /// This session's title, if one has been generated yet.
    pub fn get_title(&self, session_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT title FROM session_titles WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        ) {
            Ok(title) => Ok(Some(title)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AgentError::Tool(format!("get title: {e}"))),
        }
    }

    /// Titles for a batch of session ids in one call (avoids N+1 queries in
    /// the interactive picker). Missing entries are simply absent from the
    /// returned map.
    pub fn get_titles(&self, session_ids: &[String]) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT session_id, title FROM session_titles WHERE session_id IN ({placeholders})");
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| AgentError::Tool(format!("prepare titles: {e}")))?;
        let params = rusqlite::params_from_iter(session_ids.iter());
        let rows = stmt
            .query_map(params, |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AgentError::Tool(format!("query titles: {e}")))?;
        for r in rows {
            let (sid, title) = r.map_err(|e| AgentError::Tool(format!("row: {e}")))?;
            out.insert(sid, title);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_db(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "grace_session_test_{}_{tag}.db",
            std::process::id()
        ))
    }

    #[test]
    fn append_and_resume_roundtrip() {
        let path = scratch_db("roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store.append("s1", &Message::user("hello there")).unwrap();
        store.append("s1", &Message::assistant("hi, Sir")).unwrap();

        let history = store.load("s1").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "hello there");
        assert_eq!(history[1].content, "hi, Sir");

        // A different session id must not see s1's history.
        assert!(store.load("s2").unwrap().is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn full_text_search_finds_prior_turns() {
        let path = scratch_db("fts");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store
            .append("s1", &Message::user("what is the capital of France"))
            .unwrap();
        store
            .append("s1", &Message::assistant("Paris is the capital of France"))
            .unwrap();
        store
            .append("s2", &Message::user("unrelated question about rust"))
            .unwrap();

        let hits = store.search("France", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|(sid, _)| sid == "s1"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_sessions_returns_distinct_ids_most_recent_first() {
        let path = scratch_db("list");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store.append("alpha", &Message::user("hi")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store.append("beta", &Message::user("hi")).unwrap();

        let ids = store.list_sessions().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "beta"); // most recently active
        assert!(ids.contains(&"alpha".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_last_user_row_removes_only_the_prompt_and_syncs_fts() {
        let path = scratch_db("delrow");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store.append("s1", &Message::user("first question zebra")).unwrap();
        store.append("s1", &Message::assistant("first answer")).unwrap();
        store.append("s1", &Message::user("second question zebra")).unwrap();
        // No assistant row after that — a failed turn.

        assert_eq!(store.delete_last_user_row("s1").unwrap(), 1);
        assert_eq!(store.load("s1").unwrap().len(), 2);

        // The FTS index must no longer see the deleted row.
        let hits = store.search("zebra", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "first question zebra");

        // Other sessions are untouched; nothing to delete -> 0 rows.
        assert_eq!(store.delete_last_user_row("s2").unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wildcard_search_returns_everything_without_fts_syntax_error() {
        let path = scratch_db("wildcard");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store.append("s1", &Message::user("hello there")).unwrap();
        store.append("s2", &Message::user("goodbye now")).unwrap();

        // Previously `messages_fts MATCH "*"` was invalid FTS5 syntax and
        // errored; both "*" and "" must now return everything instead.
        let star = store.search("*", 10).unwrap();
        assert_eq!(star.len(), 2);
        let empty = store.search("", 10).unwrap();
        assert_eq!(empty.len(), 2);

        let _ = std::fs::remove_file(&path);
    }
}
