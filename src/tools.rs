//! Built-in tools: terminal, file read/write, and patch.
//!
//! These are intentionally thin wrappers over `std` I/O. Each tool:
//!   1. declares its name/description/parameters,
//!   2. pulls typed fields out of the JSON args,
//!   3. performs the side effect,
//!   4. returns a short string result (fed back to the model).
//!
//! Safety note: a real deployment must guard `run_terminal` (command
//! allow-list / sandbox) and `write_file`/`patch` (path allow-list). We keep
//! the minimal core unguarded but document the gap in the README.

use crate::error::{AgentError, Result};
use crate::tool::Tool;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, OnceLock};

// ---- helpers ---------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AgentError::Tool(format!("missing string argument '{key}'")))
}

fn str_prop(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

/// Check if a path is allowed under the configured allow-list.
/// If `GRACE_ALLOW_DIR` is set, only paths under that directory are permitted.
/// If not set, all paths are allowed (backward-compatible default).
fn check_path_allowed(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| AgentError::Tool(format!("cwd error: {e}")))?
            .join(path)
    };

    if let Ok(allow_dir) = std::env::var("GRACE_ALLOW_DIR") {
        let allow_root = Path::new(&allow_dir).canonicalize()
            .map_err(|e| AgentError::Tool(format!("invalid GRACE_ALLOW_DIR: {e}")))?;
        let canonical = absolute.canonicalize()
            .map_err(|e| AgentError::Tool(format!("path resolve error: {e}")))?;
        if !canonical.starts_with(&allow_root) {
            return Err(AgentError::Tool(format!(
                "path '{}' outside allowed directory '{}'",
                canonical.display(), allow_root.display()
            )));
        }
    }
    Ok(absolute)
}

// ---- session_search ---------------------------------------------------------

/// Lets the model search past conversation history on its own initiative —
/// e.g. "what did we decide about X last time" — instead of that only
/// happening as an invisible pre-flight recall pass (see `recall.rs`). Backed
/// by the same `SessionStore` FTS5 index `--search-sessions` uses.
pub struct SessionSearchTool {
    store: Arc<crate::session::SessionStore>,
}

impl SessionSearchTool {
    pub fn new(store: Arc<crate::session::SessionStore>) -> Self {
        Self { store }
    }
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
            "query": str_prop("Search terms (FTS5 syntax: AND/OR/quoted phrases supported)."),
            "limit": {"type": "integer", "description": "Max results (default 10)."}
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
            let preview: String = content.chars().take(200).collect();
            out.push_str(&format!("[{session_id}] {preview}\n"));
        }
        Ok(out)
    }
}

// ---- background job registry -----------------------------------------------
//
// `run_terminal(background=true)` hands a command off to its own thread and
// returns immediately with a job id; `check_background` polls or blocks on
// it. This is what lets the model kick off a long build/test/server and keep
// working (or deliberately wait) instead of every command being bound by
// `run_terminal`'s own kill-timeout.

struct BgJob {
    /// Combined stdout+stderr, tailable while still running.
    output_path: PathBuf,
    /// `None` while running; set once by the monitor thread.
    result: Mutex<Option<(Option<i32>, bool)>>, // (exit_code, timed_out)
    child: Mutex<Option<Child>>,
}

fn bg_jobs() -> &'static Mutex<HashMap<String, Arc<BgJob>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<BgJob>>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Short, readable job id — same scheme as `chat.rs`'s session ids, so the
/// model can hand the string straight back to `check_background` without
/// needing to copy a UUID verbatim.
fn short_job_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut n = nanos;
    let mut suffix = [0u8; 6];
    for slot in suffix.iter_mut().rev() {
        *slot = ALPHABET[(n % ALPHABET.len() as u128) as usize];
        n /= ALPHABET.len() as u128;
    }
    format!("bg-{}", std::str::from_utf8(&suffix).unwrap())
}

// ---- run_terminal ----------------------------------------------------------
/// Executes a shell command and returns its stdout (or stderr + exit code).
///
/// Optional guardrails, opt-in via environment variables (default: no
/// restrictions, matching the pre-hardening behavior):
///   - `GRACE_TERMINAL_DENY`: comma-separated substrings; a command containing
///     any of them is refused before spawning.
///   - `GRACE_TERMINAL_ALLOW_DIR`: if set, commands run with this directory as
///     their cwd (a simple jail, not a full sandbox).
pub struct TerminalTool;

/// (stdout, stderr, exit_code, timed_out) — see `TerminalTool::run_with_timeout`.
type SpawnResult = (Vec<u8>, Vec<u8>, Option<i32>, bool);

impl TerminalTool {
    fn deny_list() -> Vec<String> {
        std::env::var("GRACE_TERMINAL_DENY")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Allow-list: if `GRACE_TERMINAL_ALLOW` is set, only commands whose
    /// first token matches an entry are permitted. Empty/unset = allow all
    /// (backward-compatible default).
    fn allow_list() -> Vec<String> {
        std::env::var("GRACE_TERMINAL_ALLOW")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn allow_dir() -> Option<String> {
        std::env::var("GRACE_TERMINAL_ALLOW_DIR")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// A hung/slow command (e.g. a server that never exits, a prompt
    /// waiting on stdin that's never coming) must not freeze the whole
    /// agent turn forever. Default 30s, overridable via
    /// `GRACE_TERMINAL_TIMEOUT` (seconds; `0` disables the cap).
    fn timeout() -> Option<std::time::Duration> {
        let secs = std::env::var("GRACE_TERMINAL_TIMEOUT")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(30);
        (secs > 0).then(|| std::time::Duration::from_secs(secs))
    }

    /// Run `cmd`, killing it if it outlives `timeout()`. Polls
    /// `try_wait()` rather than blocking on `.output()`, since a blocking
    /// wait gives no way to intervene once the timeout has elapsed.
    /// Returns (stdout, stderr, exit_code, timed_out).
    fn run_with_timeout(mut cmd: Command) -> Result<SpawnResult> {
        use std::io::Read;
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::Tool(format!("failed to spawn 'sh': {e}")))?;
        let mut out = child.stdout.take();
        let mut err = child.stderr.take();
        let deadline = Self::timeout().map(|d| std::time::Instant::now() + d);
        let poll = std::time::Duration::from_millis(50);
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| AgentError::Tool(format!("wait failed: {e}")))?
            {
                break Some(status);
            }
            if let Some(dl) = deadline {
                if std::time::Instant::now() >= dl {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
            std::thread::sleep(poll);
        };
        let timed_out = status.is_none();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut o) = out.take() {
            let _ = o.read_to_end(&mut stdout);
        }
        if let Some(mut e) = err.take() {
            let _ = e.read_to_end(&mut stderr);
        }
        Ok((stdout, stderr, status.and_then(|s| s.code()), timed_out))
    }

    /// Cap: past this many bytes a command's output can blow the model's
    /// whole context window in one tool result (real incident: `p4 changes`
    /// on a busy depot returned megabytes, no truncation, next request
    /// exceeded context and the session died). Same head+tail-with-omission-
    /// count shape as `read_file`'s >500-line path, but byte-keyed since
    /// arbitrary command output has no reliable line-length assumption.
    const MAX_OUTPUT_BYTES: usize = 20_000;

    fn cap_output(s: &str) -> String {
        if s.len() <= Self::MAX_OUTPUT_BYTES {
            return s.to_string();
        }
        let half = Self::MAX_OUTPUT_BYTES / 2;
        // Slice on char boundaries so we never split a multi-byte UTF-8 char.
        let head_end = (0..=half).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        let tail_start = (s.len() - half..=s.len())
            .find(|&i| s.is_char_boundary(i))
            .unwrap_or(s.len());
        format!(
            "{}\n... [{} bytes omitted — output too large, re-run with a narrower command] ...\n{}",
            &s[..head_end],
            s.len() - head_end - (s.len() - tail_start),
            &s[tail_start..]
        )
    }

    /// Spawn `command` detached, redirecting combined stdout+stderr to a temp
    /// file a monitor thread tails, and register it under a short job id.
    /// Returns immediately — no `run_with_timeout` wait — since the whole
    /// point is not blocking the calling turn.
    fn spawn_background(command: &str) -> Result<String> {
        let id = short_job_id();
        let output_path = std::env::temp_dir().join(format!("grace-bg-{id}.log"));
        let out_file = fs::File::create(&output_path)
            .map_err(|e| AgentError::Tool(format!("create bg log {}: {e}", output_path.display())))?;
        let err_file = out_file
            .try_clone()
            .map_err(|e| AgentError::Tool(format!("clone bg log handle: {e}")))?;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.stdout(out_file);
        cmd.stderr(err_file);
        if let Some(dir) = Self::allow_dir() {
            cmd.current_dir(dir);
        }
        let child = cmd
            .spawn()
            .map_err(|e| AgentError::Tool(format!("failed to spawn background command: {e}")))?;
        let job = Arc::new(BgJob {
            output_path: output_path.clone(),
            result: Mutex::new(None),
            child: Mutex::new(Some(child)),
        });
        bg_jobs().lock().unwrap().insert(id.clone(), job.clone());
        // Monitor thread: waits (with the same timeout policy as foreground
        // runs) so a background job can't leak forever either, then records
        // the outcome for `check_background` to observe.
        std::thread::spawn(move || {
            let deadline = Self::timeout().map(|d| std::time::Instant::now() + d);
            let poll = std::time::Duration::from_millis(200);
            let (code, timed_out) = loop {
                let mut guard = job.child.lock().unwrap();
                let done = guard.as_mut().and_then(|c| c.try_wait().ok().flatten());
                if let Some(status) = done {
                    break (status.code(), false);
                }
                if let Some(dl) = deadline {
                    if std::time::Instant::now() >= dl {
                        if let Some(c) = guard.as_mut() {
                            let _ = c.kill();
                            let _ = c.wait();
                        }
                        break (None, true);
                    }
                }
                drop(guard);
                std::thread::sleep(poll);
            };
            *job.result.lock().unwrap() = Some((code, timed_out));
        });
        Ok(format!(
            "started background job '{id}' (pid detached). Use check_background(job_id=\"{id}\") \
             to poll output/status, or wait=true to block until it finishes."
        ))
    }
}

// ---- check_background -------------------------------------------------------

/// Companion to `run_terminal(background=true)`: polls a job's tailed output
/// and, once it exits, its final status. Optionally blocks (`wait=true`) so
/// the model can deliberately wait out a known-length background task
/// instead of polling in a loop.
pub struct CheckBackgroundTool;

impl Tool for CheckBackgroundTool {
    fn name(&self) -> &str {
        "check_background"
    }

    fn description(&self) -> &str {
        "Poll or wait on a background job started via run_terminal(background=true). \
         Returns current output plus 'running' or the final exit status."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": str_prop("Job id returned by run_terminal(background=true)."),
                "wait": {
                    "type": "boolean",
                    "description": "Block until the job finishes instead of returning immediately (default false). Still bounded by the job's own kill-timeout."
                },
            },
            "required": ["job_id"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let job_id = arg_str(args, "job_id")?;
        let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
        let job = bg_jobs()
            .lock()
            .unwrap()
            .get(&job_id)
            .cloned()
            .ok_or_else(|| AgentError::Tool(format!("no background job '{job_id}' (unknown id, or process already reaped)")))?;

        if wait {
            loop {
                if job.result.lock().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }

        let mut output = String::new();
        if let Ok(mut f) = fs::File::open(&job.output_path) {
            let _ = f.read_to_string(&mut output);
        }
        let output = TerminalTool::cap_output(output.trim_end());

        let status = *job.result.lock().unwrap();
        let tail = match status {
            None => "\n[status: running]".to_string(),
            Some((code, true)) => format!(
                "\n[status: TIMED OUT — process killed, last exit code n/a (was {:?})]",
                code
            ),
            Some((code, false)) => format!("\n[status: exited, code {}]", code.unwrap_or(-1)),
        };
        Ok(format!("{output}{tail}"))
    }
}

impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "run_terminal"
    }

    fn description(&self) -> &str {
        "Run a shell command and return its combined stdout/stderr and exit code. \
         Killed after 30s by default (GRACE_TERMINAL_TIMEOUT to override) — for \
         long-running processes (servers, watchers, builds), pass background=true \
         instead of backgrounding with shell `&`: it returns a job id immediately, \
         and `check_background` polls or blocks on it later."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": str_prop("The shell command to execute."),
                "background": {
                    "type": "boolean",
                    "description": "Run detached and return a job id immediately instead of waiting (default false). Use for servers, watchers, or long builds/tests you want to poll later with check_background."
                },
            },
            "required": ["command"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let command = arg_str(args, "command")?;
        let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
        let deny = Self::deny_list();
        if let Some(hit) = deny.iter().find(|d| command.contains(d.as_str())) {
            return Err(AgentError::Tool(format!(
                "command refused: contains denied pattern '{hit}'"
            )));
        }
        let allow = Self::allow_list();
        if !allow.is_empty() {
            let first_token = command.split_whitespace().next().unwrap_or("");
            if !allow.iter().any(|a| a == first_token) {
                return Err(AgentError::Tool(format!(
                    "command refused: '{first_token}' not in allow-list"
                )));
            }
        }
        if background {
            return Self::spawn_background(&command);
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        if let Some(dir) = Self::allow_dir() {
            cmd.current_dir(dir);
        }
        let (raw_stdout, raw_stderr, code, timed_out) = Self::run_with_timeout(cmd)?;
        let stdout = String::from_utf8_lossy(&raw_stdout);
        let stderr = String::from_utf8_lossy(&raw_stderr);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&Self::cap_output(stdout.trim_end()));
        }
        if !stderr.is_empty() {
            result.push_str(&format!("\n[stderr] {}", Self::cap_output(stderr.trim_end())));
        }
        if timed_out {
            result.push_str(&format!(
                "\n[TIMED OUT after {:?} — process killed. Long-running commands \
                 (servers, watchers) should be backgrounded with `&` and polled, \
                 or GRACE_TERMINAL_TIMEOUT raised for a known-slow one-off.]",
                Self::timeout().unwrap_or_default()
            ));
        } else {
            result.push_str(&format!("\n[exit code {}]", code.unwrap_or(-1)));
        }
        Ok(result)
    }
}

// ---- read_file -------------------------------------------------------------

/// Reads a UTF-8 file and returns its contents.
pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file and return its contents with line count. For large files (>500 lines), falls back to grep search."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("Absolute or relative path to the file."),
                "offset": {"type": "integer", "description": "Starting line number (1-indexed, default 1)."},
                "limit": {"type": "integer", "description": "Max lines to read (default 500, max 2000)."},
            },
            "required": ["path"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
        
        let allowed = check_path_allowed(&path)?;
        
        // Get total line count first
        let content = fs::read_to_string(&allowed)
            .map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        
        let total_lines = content.lines().count();
        
        // If file is large (>500 lines), only show summary + head/tail
        if total_lines > 500 {
            let head: Vec<&str> = content.lines().take(50).collect();
            let tail: Vec<&str> = content.lines().skip(total_lines.saturating_sub(50)).collect();
            
            let mut result = format!("File: {} ({} lines)\n", path, total_lines);
            result.push_str("=== FIRST 50 LINES ===\n");
            result.push_str(&head.join("\n"));
            result.push_str("\n=== LAST 50 LINES ===\n");
            result.push_str(&tail.join("\n"));
            result.push_str(&format!("\n... [{} lines omitted] ...", total_lines - head.len() - tail.len()));
            return Ok(result);
        }
        
        // For smaller files, apply offset/limit
        let lines: Vec<&str> = content.lines().skip(offset.saturating_sub(1)).take(limit).collect();
        let shown = lines.len();
        let mut result = format!("File: {} ({} lines total, showing {} lines from {})\n", path, total_lines, shown, offset);
        result.push_str(&lines.join("\n"));
        Ok(result)
    }
}

// ---- write_file ------------------------------------------------------------

/// Writes UTF-8 content to a file (creating parent dirs, overwriting).
pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write text content to a file, creating parent directories as needed. Overwrites."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("Path to write."),
                "content": str_prop("Text to write."),
            },
            "required": ["path", "content"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let allowed = check_path_allowed(&path)?;
        if let Some(parent) = allowed.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| AgentError::Tool(format!("create dirs for {}: {e}", path)))?;
            }
        }
        let nbytes = content.len();
        fs::write(&allowed, &content).map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
        // Truncate content display for large files
        let display_content = if content.len() > 200 {
            format!("{}... [truncated {} bytes]", &content[..200], content.len() - 200)
        } else {
            content.to_string()
        };
        Ok(format!("wrote {} bytes to {} (content: {})", nbytes, path, display_content))
    }
}

// ---- patch (unified diff apply) --------------------------------------------

/// Applies a small unified diff to a file. This is the "edit" primitive: we
/// implement a minimal `patch` (no fuzz, no context beyond a literal old block
/// search) so the core can modify files without shelling out to GNU patch.
pub struct PatchTool;

impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Replace the first occurrence of `old_string` with `new_string` in a file (case-sensitive, literal)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("File to edit."),
                "old_string": str_prop("Exact text to find and replace."),
                "new_string": str_prop("Replacement text."),
            },
            "required": ["path", "old_string", "new_string"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let old = arg_str(args, "old_string")?;
        let new = arg_str(args, "new_string")?;
        let allowed = check_path_allowed(&path)?;
        let original = fs::read_to_string(&allowed)
            .map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        match original.find(&old) {
            Some(idx) => {
                let replaced = format!(
                    "{}{}{}",
                    &original[..idx],
                    new,
                    &original[idx + old.len()..]
                );
                fs::write(&allowed, &replaced)
                    .map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
                let diff = crate::diff::unified_snippet(&old, &new, 3);
                Ok(format!("patched {path}\n{diff}"))
            }
            None => Err(AgentError::Tool(format!(
                "old_string not found in {} (exact, case-sensitive match required)",
                path
            ))),
        }
    }
}

/// Register the default built-in tool set into a registry.
pub fn register_builtins(registry: &mut crate::tool::ToolRegistry) {
    registry.register(Box::new(TerminalTool));
    registry.register(Box::new(CheckBackgroundTool));
    registry.register(Box::new(ReadFileTool));
    registry.register(Box::new(WriteFileTool));
    registry.register(Box::new(PatchTool));
}

#[cfg(test)]
mod tools_hardening_tests {
    use super::*;

    // `GRACE_TERMINAL_TIMEOUT`/`GRACE_TERMINAL_DENY`/`GRACE_TERMINAL_ALLOW_DIR`
    // are real process-global env vars, but `cargo test` runs tests in
    // parallel threads by default — any two tests that both touch the same
    // var race each other (this bit us once: the background-job test's
    // count-on-a-generous-timeout could observe the hanging-command test's
    // GRACE_TERMINAL_TIMEOUT=1 leak in mid-run). Serialize every test here
    // behind one mutex so env mutation is effectively sequential, matching
    // what real (non-test, single-process) usage always was anyway.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn terminal_deny_list_rejects_matching_command() {
        let _g = ENV_GUARD.lock().unwrap();
        std::env::set_var("GRACE_TERMINAL_DENY", "rm -rf,shutdown");
        let tool = TerminalTool;
        let err = tool.run(&json!({"command": "rm -rf /"})).unwrap_err();
        assert!(err.to_string().contains("denied"));
        std::env::remove_var("GRACE_TERMINAL_DENY");
    }

    #[test]
    fn terminal_allow_dir_jails_cwd() {
        let _g = ENV_GUARD.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("grace_terminal_jail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GRACE_TERMINAL_ALLOW_DIR", dir.to_str().unwrap());
        let tool = TerminalTool;
        let out = tool.run(&json!({"command": "pwd"})).unwrap();
        // Canonicalize both sides: /tmp is often a symlink (e.g. to /private/tmp).
        let canon_dir = std::fs::canonicalize(&dir).unwrap();
        assert!(out.contains(canon_dir.to_str().unwrap()) || out.contains(dir.to_str().unwrap()));
        std::env::remove_var("GRACE_TERMINAL_ALLOW_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_kills_hanging_command_instead_of_blocking_forever() {
        // Regression test for the "gets stuck for infinity" bug: a command
        // that never exits on its own (e.g. `sleep`, a server with no
        // input) must be killed at the configured timeout, not hang the
        // whole agent turn. Use a short override so the test itself stays
        // fast.
        let _g = ENV_GUARD.lock().unwrap();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "1");
        let tool = TerminalTool;
        let start = std::time::Instant::now();
        let out = tool.run(&json!({"command": "sleep 30"})).unwrap();
        let elapsed = start.elapsed();
        std::env::remove_var("GRACE_TERMINAL_TIMEOUT");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "command should have been killed near the 1s timeout, took {elapsed:?}"
        );
        assert!(out.contains("TIMED OUT"), "output was: {out}");
    }

    #[test]
    fn background_run_returns_immediately_and_check_background_observes_completion() {
        // The whole point of background=true: run_terminal must NOT block
        // for the command's duration — it hands back a job id right away,
        // and check_background is what later observes output + exit status.
        let _g = ENV_GUARD.lock().unwrap();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "30");
        let tool = TerminalTool;
        let start = std::time::Instant::now();
        let out = tool
            .run(&json!({"command": "sleep 1; echo done-marker", "background": true}))
            .unwrap();
        let dispatch_elapsed = start.elapsed();
        assert!(
            dispatch_elapsed < std::time::Duration::from_millis(500),
            "background dispatch should return near-instantly, took {dispatch_elapsed:?}"
        );
        let job_id = out
            .split('\'')
            .nth(1)
            .expect("expected job id quoted in dispatch message")
            .to_string();

        let checker = CheckBackgroundTool;
        // Immediately after dispatch the 1s sleep hasn't finished yet.
        let early = checker.run(&json!({"job_id": job_id})).unwrap();
        assert!(early.contains("[status: running]"), "early poll was: {early}");

        // wait=true blocks until it actually finishes.
        let finished = checker
            .run(&json!({"job_id": job_id, "wait": true}))
            .unwrap();
        std::env::remove_var("GRACE_TERMINAL_TIMEOUT");
        assert!(finished.contains("done-marker"), "finished output was: {finished}");
        assert!(finished.contains("[status: exited, code 0]"), "finished output was: {finished}");
    }

    #[test]
    fn check_background_reports_unknown_job_id_as_error() {
        let checker = CheckBackgroundTool;
        let err = checker
            .run(&json!({"job_id": "bg-doesnotexist"}))
            .unwrap_err();
        assert!(err.to_string().contains("no background job"));
    }
}
