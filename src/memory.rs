//! Persistent memory — durable facts that survive across process runs.
//!
//! This is the thing Hermes gets *conceptually* right (memory injected every
//! turn) but Grace backs with a real embedded database instead of re-reading
//! a markdown file: a single SQLite file via `rusqlite` (bundled, no system
//! libsqlite3 required). Facts are plain rows; there is no LLM-driven
//! "consolidation" happening automatically — that is a deliberate choice
//! (see `dream` module) so memory never mutates itself silently.

use crate::error::{AgentError, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// A single durable fact.
#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub content: String,
    pub created_at: i64,
}

/// Owns the SQLite connection backing persistent memory.
pub struct Memory {
    conn: Connection,
}

impl Memory {
    /// Open (creating if needed) the memory database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AgentError::Tool(format!("create memory dir: {e}")))?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| AgentError::Tool(format!("open memory db {}: {e}", path.display())))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS facts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| AgentError::Tool(format!("init memory schema: {e}")))?;
        Ok(Self { conn })
    }

    /// Default location: `~/.grace/memory.db`.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("memory.db")
    }

    /// Store a new durable fact.
    pub fn remember(&self, content: &str) -> Result<i64> {
        let now = now_unix();
        self.conn
            .execute(
                "INSERT INTO facts (content, created_at) VALUES (?1, ?2)",
                (content, now),
            )
            .map_err(|e| AgentError::Tool(format!("insert fact: {e}")))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All facts, oldest first.
    pub fn all(&self) -> Result<Vec<Fact>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, content, created_at FROM facts ORDER BY id ASC")
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Fact {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
        }
        Ok(out)
    }

    /// Delete a fact by id. Returns true if a row was removed.
    pub fn forget(&self, id: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM facts WHERE id = ?1", [id])
            .map_err(|e| AgentError::Tool(format!("delete fact: {e}")))?;
        Ok(n > 0)
    }

    /// Render all facts as a block suitable for appending to the system
    /// prompt. Returns `None` if there are no facts.
    pub fn as_prompt_block(&self) -> Result<Option<String>> {
        let facts = self.all()?;
        if facts.is_empty() {
            return Ok(None);
        }
        let mut s = String::from("\n\nDurable facts you know about the user/environment:\n");
        for f in &facts {
            s.push_str(&format!("- {}\n", f.content));
        }
        Ok(Some(s))
    }

    /// Extract `[[wikilink]]` targets referenced in `content` (one hop, no
    /// vault dependency — just a marker inside a fact's own text).
    pub fn extract_links(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = content;
        while let Some(start) = rest.find("[[") {
            let after = &rest[start + 2..];
            if let Some(end) = after.find("]]") {
                out.push(after[..end].trim().to_string());
                rest = &after[end + 2..];
            } else {
                break;
            }
        }
        out
    }

    /// Resolve one hop of `[[wikilink]]`s inside `content` by keyword match
    /// against other facts' content (case-insensitive substring). Used by
    /// recall to pull in a linked fact without a vault or embeddings.
    pub fn resolve_links(&self, content: &str) -> Result<Vec<Fact>> {
        let links = Self::extract_links(content);
        if links.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.all()?;
        let mut out = Vec::new();
        for link in &links {
            let link_lower = link.to_lowercase();
            for f in &all {
                if f.content.to_lowercase().contains(&link_lower) && f.content != content {
                    out.push(f.clone());
                }
            }
        }
        Ok(out)
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_recall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("grace_mem_test_{}", std::process::id()));
        let db_path = dir.join("memory.db");
        let mem = Memory::open(&db_path).unwrap();
        assert!(mem.all().unwrap().is_empty());

        let id = mem.remember("user prefers concise answers").unwrap();
        assert!(id > 0);

        let facts = mem.all().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "user prefers concise answers");

        let block = mem.as_prompt_block().unwrap().unwrap();
        assert!(block.contains("user prefers concise answers"));

        assert!(mem.forget(id).unwrap());
        assert!(mem.all().unwrap().is_empty());
        assert!(mem.as_prompt_block().unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wal_mode_is_actually_set() {
        let dir = std::env::temp_dir().join(format!("grace_mem_test_wal_{}", std::process::id()));
        let db_path = dir.join("memory.db");
        let mem = Memory::open(&db_path).unwrap();
        let mode: String = mem
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
