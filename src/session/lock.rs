//! Cross-terminal session locking.
//!
//! One file per live session under `~/.grace/locks/<id>.lock`, holding the
//! owning PID plus the lock's creation time (Unix seconds). This is what
//! lets a bare `grace --chat` resume the most recently active session that
//! is *not* already open in another terminal, instead of two terminals
//! silently interleaving turns into the same history.
//!
//! The obvious alternative — one session per tty — was tried and is worse:
//! it avoids collisions but orphans history every time a terminal is closed
//! and reopened under a different tty path.
//!
//! Acquisition is atomic (`create_new`, the O_EXCL create); an existing lock
//! is only reclaimed if its holder is provably gone (dead PID, or a recycled
//! PID whose start time postdates the lock). A live foreign holder is an
//! error, not a clobber — the old read-then-write let two starts both "win".
//!
//! A lock whose PID is no longer alive is stale and does not count as held,
//! so a crashed process never permanently strands a session.

use super::store::SessionStore;
use crate::util::{AgentError, Result};
use std::io::Write;
use std::path::PathBuf;

/// A cross-process "this session is live in some terminal" marker: one file
/// per session under `~/.grace/locks/<id>.lock`, containing the owning PID
/// and the Unix seconds the lock was written. The timestamp is what lets us
/// tell a recycled PID (stale lock) from a genuine live holder (held lock):
/// a process that started *after* the lock was written cannot have written
/// it. Lets a fresh `grace` invocation with no explicit `--session` pick the
/// most recently active session that ISN'T already open elsewhere, instead
/// of two terminals silently colliding on the same session (or the naive fix
/// of one session per tty, which orphaned history every time a terminal was
/// closed and reopened under a different tty path).
pub struct SessionLock {
    pub(crate) path: PathBuf,
}

/// Session ids land in file names (`<id>.lock`, `history_<id>.txt`), so an
/// id is an opaque token, not a path: no empty, no separators, no `.` (`..`
/// traversal), no drive colon, no NUL, and a length cap. Returns `Err` for
/// anything else so a hostile `--session` cannot smuggle a path in.
pub fn validate_session_id(id: &str) -> Result<()> {
    const MAX_LEN: usize = 64;
    let bad = id.is_empty()
        || id.len() > MAX_LEN
        || id.contains(['/', '\\', '.', ':', '\0']);
    if bad {
        return Err(AgentError::Config(format!(
            "invalid session id {id:?} — must be 1-{MAX_LEN} characters and contain no '/', '\\', '.', ':' or NUL"
        )));
    }
    Ok(())
}

impl SessionLock {
    fn dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("locks")
    }

    /// The lock file for `session_id`, or `None` if the id would build a
    /// path we must not touch. Defense in depth: callers validate up front,
    /// but no id — even an internal one — may reach the filesystem invalid.
    fn path_for(session_id: &str) -> Option<PathBuf> {
        if validate_session_id(session_id).is_err() {
            return None;
        }
        Some(Self::dir().join(format!("{session_id}.lock")))
    }

    /// True if `session_id` has a live lock held by a still-running process
    /// that is NOT this one. A lock file whose PID belongs to the current
    /// process does NOT count as held — it's our own session, not a
    /// collision. A lock whose PID is no longer alive is stale and does NOT
    /// count as held either (nor does a recycled PID that postdates the
    /// lock's timestamp).
    pub fn is_held(session_id: &str) -> bool {
        let Some(path) = Self::path_for(session_id) else {
            return false;
        };
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return false;
        };
        let mut fields = contents.split_whitespace();
        let Some(pid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            return false;
        };
        // Second field is the lock's creation time; legacy single-field
        // lock files have none, in which case liveness alone decides.
        let lock_secs: Option<u64> = fields.next().and_then(|s| s.parse().ok());
        // Our own lock is not a collision — we're already in this session.
        if pid == std::process::id() {
            return false;
        }
        process_holds_lock(pid, lock_secs)
    }

    /// Try to take the lock for `session_id`, recording this process's PID
    /// and the current time. Succeeds via an atomic create; an existing file
    /// is only reclaimed when its holder is provably gone (stale by
    /// `is_held`). A live foreign holder means the session is open in
    /// another terminal and is an `Err` for the caller — never a silent
    /// clobber, never a warning-and-steal.
    pub fn acquire(session_id: &str) -> Result<Self> {
        validate_session_id(session_id)?;
        let path = Self::path_for(session_id).unwrap();
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| AgentError::Tool(format!("create lock dir: {e}")))?;
        // Bounded retry: each pass is either a clean atomic create or a
        // provably-stale reclaim (remove + try again). Two terminals racing
        // from the same second converge within a few passes; a persistent
        // conflict means someone live holds the lock.
        for attempt in 0..3 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let line = format!("{} {now}\n", std::process::id(), now = now_unix());
                    f.write_all(line.as_bytes()).map_err(|e| {
                        AgentError::Tool(format!("write lock {}: {e}", path.display()))
                    })?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 2 => {
                    if Self::is_held(session_id) {
                        return Err(AgentError::Config(format!(
                            "session \"{session_id}\" is already open in another terminal — close it there or use --session <other>"
                        )));
                    }
                    // Stale (dead holder, recycled pid, garbage): reclaim
                    // and race the create again — the next pass re-checks
                    // atomically, so a third starter can't be clobbered.
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => {
                    return Err(AgentError::Tool(format!(
                        "write lock {}: {e}",
                        path.display()
                    )));
                }
            }
        }
        Err(AgentError::Config(format!(
            "could not take session lock {}: still contended",
            path.display()
        )))
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Only remove a lock we still own: if ours was reclaimed mid-session
        // (stale-lock race), the file now holds someone else's live lock and
        // removing it would yank it out from under them.
        let still_ours = std::fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|t| {
                t.split_whitespace()
                    .next()
                    == Some(std::process::id().to_string().as_str())
            });
        if still_ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Does the PID recorded in the lock actually hold it? On Linux the process
/// start time is compared against the lock's creation time: a live PID that
/// postdates the lock is a recycled pid, not the original holder. Elsewhere
/// (macOS/BSD) only liveness is available; off-unix, conservatively assume
/// alive (a stale lock just skips a candidate, never crashes — and no
/// non-Unix target is built).
///
/// Note: `/proc/<pid>/stat`'s start_time is counted in clock ticks (100/s)
/// since system BOOT, while the lock records Unix epoch seconds — so the
/// start time is shifted into epoch space via `/proc/uptime` before
/// comparing. Comparing the two raw time bases would mark every live lock
/// as held (and recycled pids could never be reclaimed).
#[cfg(target_os = "linux")]
fn process_holds_lock(pid: u32, lock_secs: Option<u64>) -> bool {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(_) => return false, // no process -> stale
    };
    // Field 1 is the pid, field 2 is the comm in parens (which may itself
    // contain spaces and parens), so fields are counted after the last ')'.
    let rest = match stat.rsplit_once(')') {
        Some((_, rest)) => rest,
        None => return true, // unreadable process state: conservatively held
    };
    // `rest` starts at field 3 (state); start_time is field 22 -> index 19,
    // in clock ticks (Linux USER_HZ is 100) counted since boot.
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let Some(start_ticks) = fields.get(19).and_then(|s| s.parse::<u64>().ok()) else {
        return true; // conservatively held
    };
    let since_boot = start_ticks / 100;
    let Some(boot_offset) = secs_since_boot() else {
        return true; // conservatively held
    };
    let start_epoch = now_unix().saturating_sub(boot_offset) + since_boot;
    match lock_secs {
        Some(secs) => start_epoch <= secs,
        None => true, // legacy single-field lock: liveness alone
    }
}

/// Seconds since system boot (`/proc/uptime` first field, fractional).
#[cfg(target_os = "linux")]
fn secs_since_boot() -> Option<u64> {
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    Some(uptime.split_whitespace().next()?.parse::<f64>().ok()? as u64)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_holds_lock(pid: u32, lock_secs: Option<u64>) -> bool {
    let _ = lock_secs;
    // No /proc start-time to verify the pid against; `ps -p` liveness is
    // the portable fallback (single fork, no libc FFI dependency to add).
    // A long-lived macOS lock held by a live process stays held — the
    // reclaim path (dead pid) covers crashes, which is the real case.
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true)
}

#[cfg(not(unix))]
fn process_holds_lock(pid: u32, lock_secs: Option<u64>) -> bool {
    let _ = (pid, lock_secs);
    true
}

/// Pick the default session for a bare `grace --chat` (no `--session` given):
/// the most recently active session that isn't already locked by a live
/// process in another terminal. Returns `None` if every existing session is
/// currently in use (or there are no sessions yet) — the caller should mint
/// a fresh id in that case.
pub fn pick_default_session(sessions: &SessionStore) -> Result<Option<String>> {
    let ids = sessions.list_sessions()?;
    Ok(ids.into_iter().find(|id| !SessionLock::is_held(id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    fn scratch_store(tag: &str) -> (PathBuf, SessionStore) {
        let path = std::env::temp_dir().join(format!(
            "grace_lock_test_{}_{tag}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::open(&path).unwrap();
        (path, store)
    }

    fn unique_id(tag: &str) -> String {
        format!("lk-{}-{tag}", std::process::id())
    }

    #[test]
    fn an_unlocked_session_is_not_held() {
        assert!(!SessionLock::is_held(&unique_id("never")));
    }

    #[test]
    fn acquiring_writes_a_lock_file() {
        let id = unique_id("held");
        let lock = SessionLock::acquire(&id).unwrap();
        // The lock file exists and starts with our PID (plus a timestamp).
        assert!(lock.path.exists());
        let contents = std::fs::read_to_string(&lock.path).unwrap();
        let mut fields = contents.split_whitespace();
        assert_eq!(fields.next(), Some(std::process::id().to_string().as_str()));
        assert!(fields.next().is_some(), "timestamp field must be written");
        // But `is_held` returns false because it's our own lock.
        assert!(!SessionLock::is_held(&id));
        drop(lock);
    }

    #[test]
    fn dropping_the_lock_releases_it() {
        // Without this, closing a terminal would permanently strand a session.
        let id = unique_id("release");
        {
            let _lock = SessionLock::acquire(&id).unwrap();
            // Our own lock is not considered a collision.
            assert!(!SessionLock::is_held(&id));
        }
        assert!(!SessionLock::is_held(&id));
    }

    #[test]
    fn a_live_foreign_holder_blocks_acquisition() {
        // PID 1 (init/systemd) is always alive and always started before any
        // lock we write now, so "1 <now>" simulates another terminal that
        // genuinely holds the session.
        let id = unique_id("foreign");
        let dir = SessionLock::dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.lock")), format!("1 {}\n", now_unix())).unwrap();
        assert!(SessionLock::is_held(&id), "a live holder must read as held");
        let err = SessionLock::acquire(&id).err();
        assert!(
            err.is_some(),
            "a live foreign holder must not be clobbered: {err:?}"
        );
        let _ = std::fs::remove_file(dir.join(format!("{id}.lock")));
    }

    #[test]
    fn a_stale_lock_from_a_dead_process_is_reclaimed() {
        // A crashed grace must not permanently lock a session out — and the
        // reclaim must still end with a working lock owned by us.
        let id = unique_id("stale");
        let path = SessionLock::dir().join(format!("{id}.lock"));
        std::fs::create_dir_all(SessionLock::dir()).unwrap();
        std::fs::write(&path, format!("0 {}\n", now_unix())).unwrap();
        assert!(!SessionLock::is_held(&id));
        let lock = SessionLock::acquire(&id).unwrap();
        let contents = std::fs::read_to_string(&lock.path).unwrap();
        let mut fields = contents.split_whitespace();
        assert_eq!(fields.next(), Some(std::process::id().to_string().as_str()));
        drop(lock);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_recycled_pid_does_not_count_as_held() {
        // A live PID whose process started AFTER the lock was written is a
        // reused pid, not the original holder — without the timestamp check
        // this scenario would strand the session forever.
        let id = unique_id("recycled");
        let path = SessionLock::dir().join(format!("{id}.lock"));
        std::fs::create_dir_all(SessionLock::dir()).unwrap();
        // PID 1 is alive, but its start time postdates "lock written at
        // Unix epoch" — i.e. the lock predates the process.
        std::fs::write(&path, "1 0\n").unwrap();
        assert!(!SessionLock::is_held(&id), "recycled pid must be stale");
        // ...while the same live PID with a current timestamp is held.
        std::fs::write(&path, format!("1 {}\n", now_unix())).unwrap();
        assert!(SessionLock::is_held(&id), "live holder with current stamp is held");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_garbage_lock_file_is_treated_as_unheld_not_a_panic() {
        let id = unique_id("garbage");
        let path = SessionLock::dir().join(format!("{id}.lock"));
        std::fs::create_dir_all(SessionLock::dir()).unwrap();
        std::fs::write(&path, "not-a-pid").unwrap();
        assert!(!SessionLock::is_held(&id));
        // A stale garbage file can be acquired over.
        SessionLock::acquire(&id).unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_session_id_rejects_path_shaped_ids() {
        for bad in ["", "../x", "..", "a/b", "a\\b", "a.b", "a:b", "a\0b", "s-..data"] {
            assert!(validate_session_id(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(
            validate_session_id(&"x".repeat(65)).is_err(),
            "an over-long id must be rejected"
        );
        for good in ["s-4kq9", "work", "a-b_c9"] {
            assert!(validate_session_id(good).is_ok(), "{good:?} must pass");
        }
        assert!(
            validate_session_id(&"x".repeat(64)).is_ok(),
            "the length cap is inclusive"
        );
    }

    #[test]
    fn acquire_refuses_a_path_shaped_id_before_touching_disk() {
        for bad in ["../evil", "a.b", ""] {
            assert!(
                SessionLock::acquire(bad).is_err(),
                "{bad:?} must not be acquirable"
            );
        }
        assert!(!std::path::Path::new("../evil.lock").exists());
    }

    #[test]
    fn pick_default_session_returns_none_when_there_are_no_sessions() {
        let (path, store) = scratch_store("none");
        assert!(pick_default_session(&store).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pick_default_session_prefers_the_most_recent_unlocked_session() {
        let (path, store) = scratch_store("pick");
        let a = unique_id("pick-a");
        store.append(&a, &Message::user("hi")).unwrap();
        assert_eq!(pick_default_session(&store).unwrap(), Some(a));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pick_default_session_skips_a_session_locked_elsewhere() {
        // Two terminals must never silently collide on the same history.
        let (path, store) = scratch_store("skip");
        let a = unique_id("skip-a");
        store.append(&a, &Message::user("hi")).unwrap();
        // Simulate another process holding the lock by writing PID 1
        // (init/systemd is always alive and never our process).
        let dir = SessionLock::dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{a}.lock")), "1").unwrap();
        assert_eq!(
            pick_default_session(&store).unwrap(),
            None,
            "the only session is locked by PID 1, so nothing is pickable"
        );
        let _ = std::fs::remove_file(&path);
    }
}
