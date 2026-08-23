//! Persistent memory — durable facts that survive across process runs.
//!
//! This is the thing Grace gets *conceptually* right (memory injected every
//! turn) but Grace backs with a real embedded database instead of re-reading
//! a markdown file: a single SQLite file via `rusqlite` (bundled, no system
//! libsqlite3 required). Facts are plain rows; there is no LLM-driven
//! "consolidation" happening automatically — that is a deliberate choice so
//! memory never mutates itself silently.

use crate::util::{AgentError, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// A single durable fact.
#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub content: String,
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
            .prepare("SELECT id, content FROM facts ORDER BY id ASC")
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Fact {
                    id: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
        }
        Ok(out)
    }

    /// Default location for the read-only markdown mirror: `~/.grace/memory.md`.
    pub fn default_markdown_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("memory.md")
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
