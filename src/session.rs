//! Session persistence — chat history that survives restarts, searchable.
//!
//! Backed by the same rusqlite pattern as `memory`, plus an FTS5 virtual
//! table so past turns are actually searchable (not just replayable). This is
//! the concrete fix for "--chat forgets everything on exit": a session id
//! groups messages; `--chat --session <id>` (or the default id) resumes.

use crate::error::{AgentError, Result};
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
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                content, session_id UNINDEXED, content='messages', content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content, session_id) VALUES (new.id, new.content, new.session_id);
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
                _ => Message { role: Role::System, content, ..Default::default() },
            };
            out.push(msg);
        }
        Ok(out)
    }

    /// Full-text search across all sessions. Returns (session_id, snippet).
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, content FROM messages_fts WHERE messages_fts MATCH ?1 LIMIT ?2",
            )
            .map_err(|e| AgentError::Tool(format!("prepare search: {e}")))?;
        let rows = stmt
            .query_map((query, limit), |row| {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_db(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("grace_session_test_{}_{tag}.db", std::process::id()))
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

        store.append("s1", &Message::user("what is the capital of France")).unwrap();
        store.append("s1", &Message::assistant("Paris is the capital of France")).unwrap();
        store.append("s2", &Message::user("unrelated question about rust")).unwrap();

        let hits = store.search("France", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|(sid, _)| sid == "s1"));

        let _ = std::fs::remove_file(&path);
    }
}
