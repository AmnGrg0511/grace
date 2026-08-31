//! Session persistence — chat history that survives restarts, searchable.
//!
//! Backed by the same rusqlite pattern as `memory`, plus an FTS5 virtual
//! table so past turns are actually searchable (not just replayable). This is
//! the concrete fix for "chat forgets everything on exit": a session id
//! groups messages; `grace --session <id>` (or the default id) resumes.

use crate::util::{AgentError, Result};
use crate::message::{Message, Role};
use crate::transport::TokenUsage;
use rusqlite::Connection;
use std::path::PathBuf;

/// One recorded turn's usage — what `/info` prints per turn.
#[derive(Debug, Clone)]
pub struct TurnUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub created_at: i64,
}

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
                created_at INTEGER NOT NULL,
                tool_call_id TEXT,
                name TEXT,
                tool_calls TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE TABLE IF NOT EXISTS session_titles (
                session_id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_usage (
                session_id TEXT PRIMARY KEY,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session ON turn_usage(session_id);
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
        ensure_message_columns(&conn)
            .map_err(|e| AgentError::Tool(format!("migrate session schema: {e}")))?;
        Ok(Self { conn })
    }

    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("sessions.db")
    }

    /// Append one message to a session's history, faithfully.
    ///
    /// The on-disk row is a verbatim projection of the in-memory
    /// [`Message`]: tool calls, tool results (with their `tool_call_id` and
    /// `name`), and assistant replies that carry `tool_calls` are all stored,
    /// so a resumed session replays the exact transcript the model last saw —
    /// including the tool work it already did — instead of re-issuing calls.
    /// Returns the new row's id (`0` when the message was skipped: a system
    /// prompt, an empty user marker, or an empty assistant turn with no tool
    /// calls has nothing worth replaying).
    pub fn append(&self, session_id: &str, msg: &Message) -> Result<i64> {
        let persist = match msg.role {
            // System prompts are rebuilt from the soul + durable facts on
            // load; replaying a stale one would freeze identity mid-session.
            Role::System => false,
            // A user row is the anchor of a turn — persist it whenever it has
            // text. (The empty `user("")` that seeds a fresh session is a
            // no-op marker, matching the pre-faithful-persistence behavior.)
            Role::User => !msg.content.is_empty(),
            // An assistant row matters if it has text OR requested tools; an
            // empty reply with no tool calls is nothing to replay.
            Role::Assistant => !msg.content.is_empty() || !msg.tool_calls.is_empty(),
            // A tool result always carries a `tool_call_id` answering a call
            // the model is waiting on — dropping it orphans the call.
            Role::Tool => true,
        };
        if !persist {
            return Ok(0);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // The wire-shape `tool_calls` array (id/type/function) as JSON; empty
        // for messages that carry none. Storing the raw JSON (not a
        // re-rendering) keeps a round-trip byte-faithful.
        let tool_calls_json = if msg.tool_calls.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&msg.tool_calls)
                .map_err(|e| AgentError::Tool(format!("serialize tool_calls: {e}")))?
        };
        self.conn
            .execute(
                "INSERT INTO messages (session_id, role, content, created_at, tool_call_id, name, tool_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    session_id,
                    msg.role.as_str(),
                    &msg.content,
                    now,
                    msg.tool_call_id.as_deref(),
                    msg.name.as_deref(),
                    &tool_calls_json,
                ),
            )
            .map_err(|e| AgentError::Tool(format!("append message: {e}")))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Delete a session's row `row_id` and every row after it — the on-disk
    /// half of unwinding a turn that has not completed. One-shot Ctrl-C uses
    /// it to drop the turn's user row together with any partial tool traffic
    /// the turn already recorded (a bare "delete the last user row" would
    /// leave those tool rows dangling with no prompt above them). The
    /// `AFTER DELETE` trigger keeps the FTS index in sync. Returns the number
    /// of rows removed.
    pub fn truncate_from_row_id(&self, session_id: &str, row_id: i64) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1 AND id >= ?2",
                (session_id, row_id),
            )
            .map_err(|e| AgentError::Tool(format!("truncate from row: {e}")))?;
        Ok(n)
    }

    /// A session's user rows as `(row_id, content)`, oldest first — the
    /// transcript a "jump back and re-send" picker lists (user prompts only,
    /// so tool-call noise never clutters the choices).
    pub fn list_user_rows(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, content FROM messages
                 WHERE session_id = ?1 AND role = 'user' ORDER BY id ASC",
            )
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| AgentError::Tool(format!("row: {e}")))?);
        }
        Ok(out)
    }

    /// How many user rows a session has — the turn counter for auto-retitling
    /// (each real turn contributes exactly one user row, so this is the 0-based
    /// index of the turn about to happen, unlike a raw row count which now
    /// also includes tool traffic).
    pub fn count_user_rows(&self, session_id: &str) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND role = 'user'",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|e| AgentError::Tool(format!("count user rows: {e}")))?;
        Ok(n as usize)
    }

    /// Load a session's full transcript as replayable `Message`s — user,
    /// assistant (with any `tool_calls`), and tool results (with their
    /// `tool_call_id`/`name`) — oldest first. The system prompt is not stored
    /// and is rebuilt by the caller.
    ///
    /// An orphan-tool guard drops any `tool` row whose `tool_call_id` was
    /// never requested by an earlier assistant row: a `/jump` that lands
    /// mid tool-pair would otherwise hand the provider a transcript it
    /// hard-rejects (400 on a tool message with no matching call).
    pub fn load(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT role, content, tool_call_id, name, tool_calls
                 FROM messages WHERE session_id = ?1 ORDER BY id ASC",
            )
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| AgentError::Tool(format!("query: {e}")))?;
        let mut loaded = Vec::new();
        for r in rows {
            let (role, content, tool_call_id, name, tool_calls_json) =
                r.map_err(|e| AgentError::Tool(format!("row: {e}")))?;
            let tool_calls = match tool_calls_json {
                Some(json) if !json.is_empty() => {
                    serde_json::from_str(&json).unwrap_or_default()
                }
                _ => Vec::new(),
            };
            let msg = match role.as_str() {
                "user" => Message::user(content),
                "assistant" => Message {
                    role: Role::Assistant,
                    content,
                    tool_calls,
                    ..Default::default()
                },
                "tool" => Message::tool(
                    tool_call_id.unwrap_or_default(),
                    name.unwrap_or_default(),
                    content,
                ),
                // Nothing but user/assistant/tool is ever persisted; this arm
                // only exists so an unexpected role degrades to a no-op
                // system-ish row rather than panicking.
                _ => Message {
                    role: Role::System,
                    content,
                    ..Default::default()
                },
            };
            loaded.push(msg);
        }
        Ok(drop_orphan_tool_results(loaded))
    }

    /// Full-text search across all sessions. Returns (session_id, snippet).
    /// A bare `*` (or empty query) means "everything" — FTS5 rejects `*` as
    /// invalid MATCH syntax, so that case is special-cased to a plain scan
    /// instead of surfacing a raw SQL error to the caller. Any other query
    /// is quoted token-by-token (see [`fts_quote`]) so identifiers like
    /// `GRACE-123` work bare.
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
                "SELECT session_id, content FROM messages_fts \
                 WHERE messages_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2",
            )
            .map_err(|e| AgentError::Tool(format!("prepare search: {e}")))?;
        let rows = stmt
            .query_map((fts_quote(trimmed), limit), |row| {
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

    /// Last activity time (unix seconds) per session id — the newest message
    /// each session has. Missing ids have no messages. Powers the `/session`
    /// picker's recency labels ("current", "14:32", "2d 3h ago").
    pub fn last_active(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, MAX(created_at) AS last_at FROM messages \
                 GROUP BY session_id",
            )
            .map_err(|e| AgentError::Tool(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
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

    /// Persist the provider's measured token usage for a session — the
    /// "how full is the window" number that survives restarts so a resumed
    /// session's status bar starts from the real count instead of an
    /// estimate (which made it look like usage jumped on the first turn).
    /// Upserts by session id: a newer turn's usage replaces the older one.
    pub fn save_usage(&self, session_id: &str, usage: TokenUsage) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO session_usage (session_id, prompt_tokens, completion_tokens, total_tokens)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     prompt_tokens = excluded.prompt_tokens,
                     completion_tokens = excluded.completion_tokens,
                     total_tokens = excluded.total_tokens",
                (
                    session_id,
                    usage.prompt_tokens as i64,
                    usage.completion_tokens as i64,
                    usage.total_tokens as i64,
                ),
            )
            .map_err(|e| AgentError::Tool(format!("save usage: {e}")))?;
        Ok(())
    }

    /// Record one completed turn's token usage for `/info`'s per-turn
    /// report. Unlike [`Self::save_usage`] (which upserts the latest
    /// context-size measurement), rows append — one per turn.
    pub fn record_turn_usage(
        &self,
        session_id: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        created_at: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO turn_usage (session_id, model, input_tokens, output_tokens, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    session_id,
                    model,
                    input_tokens as i64,
                    output_tokens as i64,
                    created_at
                ],
            )
            .map_err(|e| AgentError::Tool(format!("record turn usage: {e}")))?;
        Ok(())
    }

    /// Every recorded turn's usage for a session, oldest first.
    pub fn turn_usage(&self, session_id: &str) -> Result<Vec<TurnUsage>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT model, input_tokens, output_tokens, created_at
                 FROM turn_usage WHERE session_id = ?1 ORDER BY id",
            )
            .map_err(|e| AgentError::Tool(format!("prepare turn usage: {e}")))?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(TurnUsage {
                    model: r.get::<_, String>(0)?,
                    input_tokens: r.get::<_, i64>(1)?.max(0) as u64,
                    output_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                    created_at: r.get(3)?,
                })
            })
            .map_err(|e| AgentError::Tool(format!("query turn usage: {e}")))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::Tool(format!("row: {e}")))
    }

    /// The last measured usage saved for a session, if any. `None` when the
    /// session never produced a provider-reported count (brand-new session,
    /// or a provider that doesn't report usage).
    pub fn load_usage(&self, session_id: &str) -> Result<Option<TokenUsage>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT prompt_tokens, completion_tokens, total_tokens
                 FROM session_usage WHERE session_id = ?1",
            )
            .map_err(|e| AgentError::Tool(format!("prepare load usage: {e}")))?;
        let mut rows = stmt
            .query_map([session_id], |row| {
                Ok(TokenUsage {
                    prompt_tokens: row.get::<_, i64>(0)? as u64,
                    completion_tokens: row.get::<_, i64>(1)? as u64,
                    total_tokens: row.get::<_, i64>(2)? as u64,
                })
            })
            .map_err(|e| AgentError::Tool(format!("query load usage: {e}")))?;
        match rows.next() {
            Some(r) => r.map(Some).map_err(|e| AgentError::Tool(format!("row: {e}"))),
            None => Ok(None),
        }
    }
}

/// Idempotently add the faithful-transcript columns (`tool_call_id`, `name`,
/// `tool_calls`) to databases created before they existed. SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, so the existing columns are read from
/// `PRAGMA table_info` and only the missing ones are added. A brand-new
/// database already has them from `CREATE TABLE`, making this a no-op.
fn ensure_message_columns(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for column in ["tool_call_id", "name", "tool_calls"] {
        if !existing.iter().any(|c| c == column) {
            conn.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {column} TEXT;"))?;
        }
    }
    Ok(())
}

/// Drop any `tool` message whose `tool_call_id` was never requested by an
/// earlier assistant message. Providers reject a transcript containing a tool
/// result with no matching call, and the only way one reaches the store is a
/// rewind (`/jump`) that truncates a tool-pair to just its result half. A
/// single forward pass with a set of seen call ids is enough: a result always
/// follows the assistant message that requested it.
fn drop_orphan_tool_results(messages: Vec<Message>) -> Vec<Message> {
    // Owned ids (not borrows into the messages) so a message can be moved into
    // the output while the set of requested call ids outlives it.
    let mut requested: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if m.role == Role::Tool {
            let Some(id) = m.tool_call_id.as_deref() else {
                continue; // a result with no call id can never match
            };
            if !requested.contains(id) {
                continue;
            }
        }
        if m.role == Role::Assistant {
            for call in &m.tool_calls {
                requested.insert(call.id.clone());
            }
        }
        out.push(m);
    }
    out
}

/// Make arbitrary user text safe for an FTS5 MATCH query: each
/// whitespace-separated token becomes a quoted phrase (embedded quotes
/// doubled), so identifiers and punctuation are searched literally instead
/// of parsed as query syntax — `GRACE-123` matches its tokens adjacent,
/// `50%` doesn't error, `AND`/`NOT`/`NEAR` lose their operator meaning.
fn fts_quote(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

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
    fn a_tool_transcript_round_trips_faithfully() {
        // The whole point of faithful persistence: an assistant reply that
        // requested tools, and the tool results that answered them, survive a
        // store round-trip with their ids/names/calls intact — so a resumed
        // session sees the work already done instead of re-issuing it.
        let path = scratch_db("toolroundtrip");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store.append("s1", &Message::user("read the config file")).unwrap();
        let call = ToolCall::new("call_1", "read", r#"{"path":"config.toml"}"#);
        let asst = Message {
            role: Role::Assistant,
            content: String::new(), // empty text, but it requested a tool
            tool_calls: vec![call.clone()],
            ..Default::default()
        };
        let asst_rowid = store.append("s1", &asst).unwrap();
        assert_ne!(asst_rowid, 0, "an assistant row with tool_calls must persist");
        let tool = Message::tool("call_1", "read", "key = \"value\"");
        store.append("s1", &tool).unwrap();
        store.append("s1", &Message::assistant("The config has one key.")).unwrap();

        let loaded = store.load("s1").unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[0].role, Role::User);
        // The assistant tool-call row: empty content, one call, intact args.
        assert_eq!(loaded[1].role, Role::Assistant);
        assert!(loaded[1].content.is_empty());
        assert_eq!(loaded[1].tool_calls.len(), 1);
        assert_eq!(loaded[1].tool_calls[0].id, "call_1");
        assert_eq!(loaded[1].tool_calls[0].name(), "read");
        assert_eq!(loaded[1].tool_calls[0].arguments(), r#"{"path":"config.toml"}"#);
        // The tool result row: id + name + body all preserved.
        assert_eq!(loaded[2].role, Role::Tool);
        assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(loaded[2].name.as_deref(), Some("read"));
        assert_eq!(loaded[2].content, "key = \"value\"");
        assert_eq!(loaded[3].role, Role::Assistant);
        assert_eq!(loaded[3].content, "The config has one key.");

        // Reopen: the transcript is durable across a restart.
        drop(store);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.load("s1").unwrap().len(), 4);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_assistant_reply_without_tool_calls_is_skipped() {
        let path = scratch_db("emptyasst");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        store.append("s1", &Message::user("hi")).unwrap();
        // Empty text, no tool calls: nothing to replay.
        assert_eq!(store.append("s1", &Message::assistant("")).unwrap(), 0);
        assert_eq!(store.load("s1").unwrap().len(), 1);
        // A system prompt is never persisted.
        assert_eq!(store.append("s1", &Message::system("you are grace")).unwrap(), 0);
        assert_eq!(store.load("s1").unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_drops_a_tool_result_orphaned_from_its_call() {
        // A rewind that keeps only a tool result (no requesting assistant row)
        // must not hand the provider a transcript it would reject.
        let path = scratch_db("orphan");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        store.append("s1", &Message::user("do the thing")).unwrap();
        // A tool result whose call id was never requested: orphaned.
        store
            .append("s1", &Message::tool("ghost", "read", "orphan body"))
            .unwrap();
        // A matched pair survives.
        let call = ToolCall::new("call_9", "read", "{}");
        store
            .append(
                "s1",
                &Message {
                    role: Role::Assistant,
                    tool_calls: vec![call],
                    ..Default::default()
                },
            )
            .unwrap();
        store.append("s1", &Message::tool("call_9", "read", "matched")).unwrap();

        let loaded = store.load("s1").unwrap();
        let tool_rows: Vec<&Message> = loaded.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tool_rows.len(), 1, "the orphan must be dropped: {loaded:?}");
        assert_eq!(tool_rows[0].tool_call_id.as_deref(), Some("call_9"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn count_user_rows_counts_only_user_rows() {
        let path = scratch_db("countuser");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(store.count_user_rows("s1").unwrap(), 0);
        store.append("s1", &Message::user("q1")).unwrap();
        store
            .append("s1", &Message::assistant("a1 with a call".to_string()))
            .unwrap();
        store.append("s1", &Message::tool("c", "read", "r")).unwrap();
        store.append("s1", &Message::user("q2")).unwrap();
        // Two user rows, not four (tool/assistant don't count).
        assert_eq!(store.count_user_rows("s1").unwrap(), 2);
        // Other sessions are independent.
        assert_eq!(store.count_user_rows("s2").unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_user_rows_returns_prompts_oldest_first() {
        let path = scratch_db("listuser");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        store.append("s1", &Message::user("first prompt")).unwrap();
        store.append("s1", &Message::assistant("answer")).unwrap();
        store.append("s1", &Message::user("second prompt")).unwrap();
        store.append("s1", &Message::tool("c", "read", "r")).unwrap();

        let rows = store.list_user_rows("s1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "first prompt");
        assert_eq!(rows[1].1, "second prompt");
        assert!(rows[0].0 < rows[1].0, "row ids must be ascending");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncate_from_row_id_drops_the_row_and_everything_after() {
        let path = scratch_db("truncrow");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        store.append("s1", &Message::user("keep me")).unwrap();
        store.append("s1", &Message::assistant("keep me too")).unwrap();
        let cut = store.append("s1", &Message::user("drop me")).unwrap();
        store
            .append("s1", &Message::tool("c", "read", "drop me as well"))
            .unwrap();
        store.append("s2", &Message::user("other session")).unwrap();

        let removed = store.truncate_from_row_id("s1", cut).unwrap();
        assert_eq!(removed, 2, "the user row and the tool row after it");
        let remaining = store.load("s1").unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[1].content, "keep me too");

        // The FTS index must no longer see the deleted tool row (the
        // `AFTER DELETE` trigger keeps it in sync) — a stale index would let
        // `--search-sessions` surface a result from a rewound turn.
        let hits = store.search("drop me as well", 10).unwrap();
        assert!(hits.is_empty(), "deleted tool row must leave the FTS index: {hits:?}");

        // Other sessions are untouched; a row id past the end removes nothing.
        assert_eq!(store.load("s2").unwrap().len(), 1);
        assert_eq!(store.truncate_from_row_id("s1", i64::MAX).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_legacy_db_gains_the_faithful_columns_on_open() {
        // A database created before the tool columns existed must be migrated
        // in place on the next open, not error or lose its rows.
        let path = scratch_db("legacy");
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, role, content, created_at)
                 VALUES ('s1', 'user', 'old row', 0)",
                [],
            )
            .unwrap();
        }
        // Opening through SessionStore migrates the schema and preserves the row.
        let store = SessionStore::open(&path).unwrap();
        let loaded = store.load("s1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "old row");
        // And the new columns are now writable: a faithful tool round-trips
        // (a matched assistant tool-call + its result, so the orphan guard keeps it).
        store
            .append(
                "s1",
                &Message {
                    role: Role::Assistant,
                    tool_calls: vec![ToolCall::new("c", "read", "{}")],
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .append("s1", &Message::tool("c", "read", "r"))
            .unwrap();
        let after = store.load("s1").unwrap();
        assert_eq!(after.len(), 3, "old row + assistant tool-call + tool result");
        assert_eq!(after[2].tool_call_id.as_deref(), Some("c"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn turn_usage_appends_per_turn_and_reads_back_in_order() {
        let path = scratch_db("turn_usage");
        let store = SessionStore::open(&path).unwrap();
        assert!(store.turn_usage("s1").unwrap().is_empty());
        for (model, i, o) in [("m1", 100, 10), ("m1", 200, 20), ("m2", 300, 30)] {
            store.record_turn_usage("s1", model, i, o, 1_700_000_000).unwrap();
        }
        let rows = store.turn_usage("s1").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].model, "m1");
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[2].model, "m2");
        // Sessions are independent.
        assert!(store.turn_usage("s2").unwrap().is_empty());
    }

    #[test]
    fn usage_survives_a_reopen_and_upserts_in_place() {
        let path = scratch_db("usage");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        // Nothing saved yet → None, not Some(zero).
        assert!(store.load_usage("s1").unwrap().is_none());

        let first = TokenUsage {
            prompt_tokens: 900,
            completion_tokens: 40,
            total_tokens: 940,
        };
        store.save_usage("s1", first).unwrap();
        assert_eq!(store.load_usage("s1").unwrap(), Some(first));

        // A newer turn replaces the older count (upsert, not accumulate).
        let second = TokenUsage {
            prompt_tokens: 1_200,
            completion_tokens: 55,
            total_tokens: 1_255,
        };
        store.save_usage("s1", second).unwrap();
        assert_eq!(store.load_usage("s1").unwrap(), Some(second));

        // Reopen the same db file → the count is durable across a restart,
        // which is the whole point (resume shouldn't fall back to an estimate).
        drop(store);
        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(reopened.load_usage("s1").unwrap(), Some(second));

        // Sessions are independent: s2 never had usage recorded.
        assert!(reopened.load_usage("s2").unwrap().is_none());

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
    fn full_text_search_returns_most_recent_rows_first() {
        // Regression: the FTS MATCH branch had no ORDER BY, so recall
        // surfaced the *oldest* matches first. Newest first matches the
        // plain-scan branch (`ORDER BY id DESC`) and what recall wants.
        let path = scratch_db("fts_order");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store
            .append("s1", &Message::user("the first occurence of codex"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store
            .append("s1", &Message::assistant("a later occurence of codex"))
            .unwrap();

        let hits = store.search("codex", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].1.contains("later"), "newest first: {hits:?}");
        assert!(hits[1].1.contains("first"), "oldest last: {hits:?}");

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

    #[test]
    fn fts_quote_wraps_each_token_and_doubles_embedded_quotes() {
        assert_eq!(fts_quote("GRACE-123"), "\"GRACE-123\"");
        assert_eq!(fts_quote("fix  GRACE-123  now"), "\"fix\" \"GRACE-123\" \"now\"");
        assert_eq!(fts_quote("say \"hi\""), "\"say\" \"\"\"hi\"\"\"");
    }

    #[test]
    fn a_bare_jira_id_is_findable_without_quoting() {
        let path = scratch_db("jira");
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();

        store
            .append("s1", &Message::user("please fix GRACE-123 before the release"))
            .unwrap();
        store.append("s2", &Message::user("unrelated chatter")).unwrap();

        let hits = store.search("GRACE-123", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "s1");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn punctuation_and_reserved_words_no_longer_error_the_match_query() {
        // Each of these used to be raw MATCH input: `c++`/`50%`/parens are
        // syntax errors, a bare AND/OR/NOT is an operator misuse error.
        for q in ["c++ (urgent)", "50% off", "fish AND chips", "NOT this", "a OR b"] {
            let path = scratch_db("syntax");
            let _ = std::fs::remove_file(&path);
            let store = SessionStore::open(&path).unwrap();
            store.append("s1", &Message::user("plain text body")).unwrap();
            let hits = store.search(q, 10);
            assert!(hits.is_ok(), "{q:?} errored: {:?}", hits.err());
            let _ = std::fs::remove_file(&path);
        }
    }
}
