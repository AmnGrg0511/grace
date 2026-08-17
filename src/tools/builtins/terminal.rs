//! `bash` — shell execution and background job management.
//!
//! Bounded by a kill-timeout so a hung command cannot freeze an agent turn
//! forever. Every spawn becomes its own process-group leader so a timeout kill
//! reaches the whole tree. Supports background jobs and checking them via a
//! unified interface.
//!
//! Safety: unguarded by default. Opt-in guardrails via `GRACE_TERMINAL_DENY`,
//! `GRACE_TERMINAL_ALLOW`, `GRACE_TERMINAL_ALLOW_DIR`, and
//! `GRACE_TERMINAL_TIMEOUT` (foreground) / `GRACE_TERMINAL_BG_TIMEOUT`
//! (background).

use crate::tools::r#trait::{str_prop, Tool};
use crate::util::{AgentError, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

// ---- process-group signalling ----------------------------------------------
// `Command::process_group` (std, stable since 1.64) puts the child in a new
// process group without any unsafe pre_exec/setsid FFI. To kill the whole
// group on timeout we shell out to the `kill` binary rather than calling
// libc's kill(2) directly — this crate forbids unsafe code, and spawning a
// process is not unsafe.

/// Send SIGKILL to an entire process group (negative pid == group signal).
fn killpg(pgid: i32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pgid}"))
        .status();
}

/// Read `r` to EOF, but if `deadline` is set, give up after it (returning
/// whatever was read so far) rather than blocking indefinitely — used after
/// a timeout-kill where a stray descendant could otherwise still be holding
/// the write end of the pipe open. Takes ownership of `r` (rather than a
/// borrow) so the read can be handed to a helper thread and simply
/// abandoned — no unsafe aliasing — if it overruns the deadline.
fn read_bounded<R: std::io::Read + Send + 'static>(r: R, deadline: Option<std::time::Duration>) -> Vec<u8> {
    match deadline {
        None => {
            let mut r = r;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            buf
        }
        Some(d) => {
            // std::io::Read has no built-in timeout; run the blocking read
            // on a helper thread and abandon it (not join) if it overruns —
            // it dies with the process, no leak beyond one dangling read
            // that unblocks the moment the last pipe writer exits.
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = std::thread::Builder::new().spawn(move || {
                let mut r = r;
                let mut local = Vec::new();
                let _ = r.read_to_end(&mut local);
                let _ = tx.send(local);
            });
            rx.recv_timeout(d).unwrap_or_default()
        }
    }
}

// ---- background job registry -----------------------------------------------
//
// `bash(background=true)` hands a command off to its own thread and returns immediately
// with a job id; `bash(job_id="...")` polls or blocks on it. This lets the model kick
// off a long build/test/server and keep working (or deliberately wait) instead of
// every command being bound by `bash`'s own kill-timeout.

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
/// model can hand the string straight back to `bash(job_id="...")` without
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

// ---- bash (merged terminal + check_background) -----------------------------
/// Unified tool: runs a shell command, manages background jobs, and checks their status.
///
/// Optional guardrails, opt-in via environment variables (default: no
/// restrictions, matching the pre-hardening behavior):
///   - `GRACE_TERMINAL_DENY`: comma-separated substrings; a command containing
///     any of them is refused before spawning.
///   - `GRACE_TERMINAL_ALLOW_DIR`: if set, commands run with this directory as
///     their cwd (a simple jail, not a full sandbox).
pub struct BashTool;

/// Validate a command against the `GRACE_TERMINAL_ALLOW` allow-list.
///
/// Refuses outright anything that lets an allow-listed first token smuggle a
/// second program past the guard: separators (`;`, `|`, `&`) outside quotes,
/// backticks, `$(` substitution, and newlines. A command that survives is a
/// single segment, which is then validated as a whole — `ls; rm -rf /` no
/// longer passes an allow-list that only names `ls`. `pub` so the read-only
/// session posture (features.md W4) can reuse the same check.
pub fn validate_command(command: &str, allow: &[String]) -> Result<()> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for c in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' | '|' | '&' if !in_single && !in_double => {
                return Err(AgentError::Tool(format!(
                    "command refused: '{c}' outside quotes — chaining not allowed"
                )));
            }
            '`' | '\n' => {
                return Err(AgentError::Tool(
                    "command refused: backticks and newlines are not allowed".into(),
                ));
            }
            _ => {}
        }
    }
    if command.contains("$(") {
        return Err(AgentError::Tool(
            "command refused: command substitution is not allowed".into(),
        ));
    }
    let first_token = command.split_whitespace().next().unwrap_or("");
    if !allow.iter().any(|a| a == first_token) {
        return Err(AgentError::Tool(format!(
            "command refused: '{first_token}' not in allow-list"
        )));
    }
    Ok(())
}

/// (stdout, stderr, exit_code, timed_out) — see `BashTool::run_with_timeout`.
type SpawnResult = (Vec<u8>, Vec<u8>, Option<i32>, bool);

impl BashTool {
    fn deny_list() -> Vec<String> {
        std::env::var("GRACE_TERMINAL_DENY")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Allow-list: if `GRACE_TERMINAL_ALLOW` is set, only simple commands
    /// whose first token matches an entry are permitted. Chaining (`; | &`),
    /// substitution and newlines are refused outright — see
    /// [`validate_command`]. Empty/unset = allow all (backward-compatible
    /// default).
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

    /// Timeout for background jobs. Deliberately separate from
    /// [`BashTool::timeout`]: the foreground default (30s) makes no sense for
    /// a detached server/watcher, and silently killing those was the bug.
    /// Default is no cap; `GRACE_TERMINAL_BG_TIMEOUT` (seconds, `0`/unset =
    /// no cap) opts in to one.
    fn bg_timeout() -> Option<std::time::Duration> {
        let secs = std::env::var("GRACE_TERMINAL_BG_TIMEOUT")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        (secs > 0).then(|| std::time::Duration::from_secs(secs))
    }

    /// Run `cmd`, killing it if it outlives `timeout()`. Polls
    /// `try_wait()` rather than blocking on `.output()`, since a blocking
    /// wait gives no way to intervene once the timeout has elapsed.
    ///
    /// `cmd` is spawned as the leader of its own process group (`setsid`)
    /// so a timeout kill reaches the whole tree, not just the immediate
    /// `sh`. Without this, a pipeline/background job (`find / | grep ...`,
    /// `sleep 30 &`) leaves orphaned grandchildren holding thestdout pipe
    /// open — `sh` dies on schedule but the subsequent `read_to_end` then
    /// blocks forever waiting for EOF that never comes, silently hanging
    /// well past the timeout that was supposed to bound this call.
    /// Returns (stdout, stderr, exit_code, timed_out).
    fn run_with_timeout(mut cmd: Command) -> Result<SpawnResult> {
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // New process group, with this child as its leader (pid == pgid,
        // via process_group(0)). Lets us signal the whole group on timeout
        // instead of just the top-level `sh`.
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| AgentError::Tool(format!("failed to spawn 'sh': {e}")))?;
        let pgid = child.id() as i32;
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
                    // SIGKILL the whole group (negative pid), not just the
                    // direct child — reaches pipeline stages and any
                    // backgrounded (`cmd &`) descendants too.
                    killpg(pgid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
            std::thread::sleep(poll);
        };
        let timed_out = status.is_none();
        // After a timeout kill, a lingering pipe writer (rare, but possible
        // if a grandchild dodged the group signal) must not re-hang this
        // read — cap it with its own short, bounded read instead of a
        // blocking read_to_end.
        let read_deadline = std::time::Duration::from_millis(500);
        let stdout = out
            .take()
            .map(|o| read_bounded(o, timed_out.then_some(read_deadline)))
            .unwrap_or_default();
        let stderr = err
            .take()
            .map(|e| read_bounded(e, timed_out.then_some(read_deadline)))
            .unwrap_or_default();
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
        let out_file = std::fs::File::create(&output_path)
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
        // Same process-group leadership as run_with_timeout: a pipeline or
        // `cmd &` background job under `sh` must fully die on timeout, not
        // leave orphaned descendants running past it.
        cmd.process_group(0);
        let child = cmd
            .spawn()
            .map_err(|e| AgentError::Tool(format!("failed to spawn background command: {e}")))?;
        let pgid = child.id() as i32;
        let job = Arc::new(BgJob {
            output_path: output_path.clone(),
            result: Mutex::new(None),
            child: Mutex::new(Some(child)),
        });
        bg_jobs().lock().unwrap().insert(id.clone(), job.clone());
        // Monitor thread: waits (with the same timeout policy as foreground
        // runs) so a background job can't leak forever either, then records
        // the outcome for `bash(job_id="...")` to observe.
        std::thread::spawn(move || {
            let deadline = Self::bg_timeout().map(|d| std::time::Instant::now() + d);
            let poll = std::time::Duration::from_millis(200);
            let (code, timed_out) = loop {
                let mut guard = job.child.lock().unwrap();
                let done = guard.as_mut().and_then(|c| c.try_wait().ok().flatten());
                if let Some(status) = done {
                    break (status.code(), false);
                }
                if let Some(dl) = deadline {
                    if std::time::Instant::now() >= dl {
                        killpg(pgid);
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
            "started background job '{id}' (pid detached). Use bash(job_id=\"{id}\") \
             to poll output/status, or wait=true to block until it finishes."
        ))
    }

    /// Check a background job — replaced the old `check_background` tool.
    /// If `wait` is true, blocks until the job finishes.
    fn check_job(job_id: &str, wait: bool) -> Result<String> {
        let job = bg_jobs()
            .lock()
            .unwrap()
            .get(job_id)
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
        if let Ok(mut f) = std::fs::File::open(&job.output_path) {
            let _ = f.read_to_string(&mut output);
        }
        let output = Self::cap_output(output.trim_end());

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

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command, start a background job, or check a background job's status. \
         For running a command: pass 'command'. For background jobs: pass 'command' + background=true \
         to start, then 'job_id' to check. Killed after 30s by default (GRACE_TERMINAL_TIMEOUT to \
         override). Use background=true for servers, watchers, or long builds."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": str_prop("The shell command to execute."),
                "background": {
                    "type": "boolean",
                    "description": "Run detached and return a job id immediately (default false). Use for servers, watchers, or long builds."
                },
                "job_id": {
                    "type": "string",
                    "description": "Check a background job's status. Mutually exclusive with 'command'."
                },
                "wait": {
                    "type": "boolean",
                    "description": "When using job_id, block until the job finishes (default false)."
                },
            },
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let job_id = args.get("job_id").and_then(|v| v.as_str()).map(|s| s.to_string());
        let command = args.get("command").and_then(|v| v.as_str()).map(|s| s.to_string());

        if let Some(jid) = job_id {
            let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
            return Self::check_job(&jid, wait);
        }

        let command = command.ok_or_else(|| {
            AgentError::Tool("either 'command' or 'job_id' must be provided".to_string())
        })?;

        let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
        let deny = Self::deny_list();
        if let Some(hit) = deny.iter().find(|d| command.contains(d.as_str())) {
            return Err(AgentError::Tool(format!(
                "command refused: contains denied pattern '{hit}'"
            )));
        }
        let allow = Self::allow_list();
        if !allow.is_empty() {
            validate_command(&command, &allow)?;
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
#[cfg(test)]
mod tools_hardening_tests {
    use super::*;

    // `GRACE_TERMINAL_*` are real process-global env vars, but `cargo test`
    // runs tests in parallel threads — any two tests touching the same var
    // race each other (this bit us once: the background-job test's
    // count-on-a-generous-timeout observed the hanging-command test's
    // GRACE_TERMINAL_TIMEOUT=1 leak mid-run). The lock is crate-wide, not
    // module-local, because the file/patch tools' GRACE_ALLOW_DIR jail races
    // these too.
    use crate::util::test_support::env_guard as ENV_GUARD_FN;

    #[test]
    fn bash_deny_list_rejects_matching_command() {
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_DENY", "rm -rf,shutdown");
        let tool = BashTool;
        let err = tool.run(&json!({"command": "rm -rf /"})).unwrap_err();
        assert!(err.to_string().contains("denied"));
        std::env::remove_var("GRACE_TERMINAL_DENY");
    }

    #[test]
    fn bash_allow_dir_jails_cwd() {
        let _g = ENV_GUARD_FN();
        let dir = std::env::temp_dir().join(format!("grace_terminal_jail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GRACE_TERMINAL_ALLOW_DIR", dir.to_str().unwrap());
        let tool = BashTool;
        let out = tool.run(&json!({"command": "pwd"})).unwrap();
        // Canonicalize both sides: /tmp is often a symlink (e.g. to /private/tmp).
        let canon_dir = std::fs::canonicalize(&dir).unwrap();
        assert!(out.contains(canon_dir.to_str().unwrap()) || out.contains(dir.to_str().unwrap()));
        std::env::remove_var("GRACE_TERMINAL_ALLOW_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_kills_hanging_command_instead_of_blocking_forever() {
        // Regression test for the "gets stuck for infinity" bug: a command
        // that never exits on its own (e.g. `sleep`, a server with no
        // input) must be killed at the configured timeout, not hang the
        // whole agent turn. Use a short override so the test itself stays
        // fast.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "1");
        let tool = BashTool;
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
    fn timeout_kills_whole_pipeline_not_just_direct_sh_child() {
        // Real bug hit in production: a pipeline/background-job command
        // (e.g. `find / -iname '*skill*' | grep ...`, or `cmd &`) leaves
        // orphaned grandchildren running after the direct `sh` is killed —
        // and those orphans keep the stdout pipe's write end open, so the
        // subsequent read blocks forever waiting for EOF that never comes.
        // The whole tool call then hangs well past its configured timeout.
        // Fix: spawn `sh` as its own process-group leader and SIGKILL the
        // group (not just the child) on timeout.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "1");
        let tool = BashTool;
        let start = std::time::Instant::now();
        // `sleep 30 &` backgrounds a grandchild sh won't wait on directly;
        // `wait` then blocks sh itself until timeout kills it — but without
        // group-kill the backgrounded sleep survives and holds the pipe.
        // Print the backgrounded sleep's own pid so the assertion below can
        // check precisely whether that pid (not just "some sleep 30
        // somewhere on a shared host") is still alive.
        let out = tool
            .run(&json!({"command": "sleep 30 & echo BGPID=$!; sleep 30; wait"}))
            .unwrap();
        let elapsed = start.elapsed();
        std::env::remove_var("GRACE_TERMINAL_TIMEOUT");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "call should return near the 1s timeout, not hang on orphaned children; took {elapsed:?}"
        );
        assert!(out.contains("TIMED OUT"), "output was: {out}");
        // Note: a killed process's stdout up to the kill point IS still
        // captured (cap_output/read_bounded read whatever was written
        // before EOF), so BGPID=<pid> should be present in `out` even
        // though the overall run timed out.
        let bg_pid = out
            .lines()
            .find_map(|l| l.trim().strip_prefix("BGPID="))
            .map(|s| s.trim().to_string());
        if let Some(pid) = bg_pid {
            // Give the group-kill a moment to land, then confirm that
            // specific backgrounded sleep is actually dead — not a blanket
            // `pgrep sleep` (a shared host may have unrelated sleeps from
            // other users/tests running concurrently).
            std::thread::sleep(std::time::Duration::from_millis(300));
            let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
            assert!(!alive, "backgrounded sleep pid {pid} survived the group-kill");
        }
        // If BGPID wasn't captured (e.g. output truncated before the kill
        // landed), the elapsed-time + TIMED OUT assertions above already
        // prove the core regression (hang past timeout) didn't recur.
    }

    #[test]
    fn background_run_returns_immediately_and_bash_check_job_observes_completion() {
        // The whole point of background=true: bash must NOT block
        // for the command's duration — it hands back a job id right away,
        // and bash(job_id="...") is what later observes output + exit status.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "30");
        let tool = BashTool;
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

        // Immediately after dispatch the 1s sleep hasn't finished yet.
        let early = tool.run(&json!({"job_id": job_id})).unwrap();
        assert!(early.contains("[status: running]"), "early poll was: {early}");

        // wait=true blocks until it actually finishes.
        let finished = tool
            .run(&json!({"job_id": job_id, "wait": true}))
            .unwrap();
        std::env::remove_var("GRACE_TERMINAL_TIMEOUT");
        assert!(finished.contains("done-marker"), "finished output was: {finished}");
        assert!(finished.contains("[status: exited, code 0]"), "finished output was: {finished}");
    }

    #[test]
    fn bash_check_job_reports_unknown_job_id_as_error() {
        let tool = BashTool;
        let err = tool
            .run(&json!({"job_id": "bg-doesnotexist"}))
            .unwrap_err();
        assert!(err.to_string().contains("no background job"));
    }

    #[test]
    fn bash_requires_command_or_job_id() {
        let tool = BashTool;
        let err = tool.run(&json!({})).unwrap_err();
        assert!(err.to_string().contains("either 'command' or 'job_id'"));
    }

    #[test]
    fn allow_list_checks_every_segment_not_just_the_first_token() {
        // Regression: `ls; rm -rf /` used to pass an allow-list naming only
        // `ls`, because only the first whitespace token was checked.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_ALLOW", "ls");
        let tool = BashTool;
        let err = tool
            .run(&json!({"command": "ls; rm -rf /"}))
            .unwrap_err();
        assert!(err.to_string().contains("chaining"), "err was: {err}");
        let err = tool
            .run(&json!({"command": "ls | grep x"}))
            .unwrap_err();
        assert!(err.to_string().contains("chaining"), "err was: {err}");
        std::env::remove_var("GRACE_TERMINAL_ALLOW");
    }

    #[test]
    fn allow_list_refuses_substitution_backticks_and_newlines() {
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_ALLOW", "ls");
        let tool = BashTool;
        for bad in ["ls $(rm -rf /)", "`rm -rf /`", "ls\nrm -rf /"] {
            let err = tool.run(&json!({"command": bad})).unwrap_err();
            assert!(err.to_string().contains("refused"), "command {bad:?} err: {err}");
        }
        std::env::remove_var("GRACE_TERMINAL_ALLOW");
    }

    #[test]
    fn allow_list_keeps_quoted_separators_inside_a_single_segment() {
        // A separator *inside* quotes is data, not chaining — `echo "a;b"`
        // must stay allowed when `echo` is.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_ALLOW", "echo");
        let tool = BashTool;
        let out = tool.run(&json!({"command": "echo \"a;b\""})).unwrap();
        assert!(out.contains("a;b"), "out was: {out}");
        std::env::remove_var("GRACE_TERMINAL_ALLOW");
    }

    #[test]
    fn background_job_ignores_the_foreground_timeout() {
        // Regression: background jobs used to inherit the 30s foreground
        // timeout, so a server/watcher left running `sleep 30` got killed on
        // schedule. With a tight foreground timeout set, a backgrounded
        // longer sleep must still complete — bg timeout is its own knob.
        let _g = ENV_GUARD_FN();
        std::env::set_var("GRACE_TERMINAL_TIMEOUT", "1");
        std::env::remove_var("GRACE_TERMINAL_BG_TIMEOUT");
        let tool = BashTool;
        let out = tool
            .run(&json!({"command": "sleep 3; echo bg-done", "background": true}))
            .unwrap();
        let job_id = out
            .split('\'')
            .nth(1)
            .expect("expected job id quoted in dispatch message")
            .to_string();
        let finished = tool
            .run(&json!({"job_id": job_id, "wait": true}))
            .unwrap();
        assert!(
            finished.contains("bg-done") && finished.contains("[status: exited, code 0]"),
            "background job was wrongly killed by the foreground timeout: {finished}"
        );
        std::env::remove_var("GRACE_TERMINAL_TIMEOUT");
    }
}
