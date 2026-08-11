//! Cross-terminal session locking.
//!
//! One file per live session under `~/.grace/locks/<id>.lock`, holding the
//! owning PID. This is what lets a bare `grace --chat` resume the most
//! recently active session that is *not* already open in another terminal,
//! instead of two terminals silently interleaving turns into the same history.
//!
//! The obvious alternative — one session per tty — was tried and is worse: it
//! avoids collisions but orphans history every time a terminal is closed and
//! reopened under a different tty path.
//!
//! A lock whose PID is no longer alive is stale and does not count as held, so
//! a crashed process never permanently strands a session.

use super::store::SessionStore;
use crate::util::{AgentError, Result};
use std::path::PathBuf;

/// A cross-process "this session is live in some terminal" marker: one file
/// per session under `~/.grace/locks/<id>.lock`, containing the owning PID.
/// Lets a fresh `grace` invocation with no explicit `--session` pick the most
/// recently active session that ISN'T already open elsewhere, instead of two
/// terminals silently colliding on the same session (or the naive fix of one
/// session per tty, which orphaned history every time a terminal was closed
/// and reopened under a different tty path).
pub struct SessionLock {
    pub(crate) path: PathBuf,
}

impl SessionLock {
    fn dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("locks")
    }

    fn path_for(session_id: &str) -> PathBuf {
        Self::dir().join(format!("{session_id}.lock"))
    }

    /// True if `session_id` has a live lock held by a still-running process
    /// that is NOT this one. A lock file whose PID belongs to the current
    /// process does NOT count as held — it's our own session, not a
    /// collision. A lock file whose PID is no longer alive is stale and
    /// does NOT count as held either.
    pub fn is_held(session_id: &str) -> bool {
        let path = Self::path_for(session_id);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return false;
        };
        let Ok(pid) = contents.trim().parse::<u32>() else {
            return false;
        };
        // Our own lock is not a collision — we're already in this session.
        if pid == std::process::id() {
            return false;
        }
        pid_is_alive(pid)
    }

    /// Try to acquire the lock for `session_id`, writing this process's PID.
    /// Always succeeds (overwrites stale locks) — callers are expected to have
    /// checked `is_held` before calling this. Prints a warning if another
    /// live process's lock is clobbered.
    pub fn acquire(session_id: &str) -> Result<Self> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| AgentError::Tool(format!("create lock dir: {e}")))?;
        let path = Self::path_for(session_id);
        // Record the old lock's PID before overwriting, so we can warn if
        // we're clobbering a live session held by another terminal.
        let old_pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        std::fs::write(&path, std::process::id().to_string())
            .map_err(|e| AgentError::Tool(format!("write lock {}: {e}", path.display())))?;
        if let Some(pid) = old_pid {
            if pid != std::process::id() && pid_is_alive(pid) {
                eprintln!(
                    "warning: session \"{}\" was locked by PID {} — lock claimed by current process.",
                    session_id, pid
                );
            }
        }
        Ok(Self { path })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "linux")]
fn pid_is_alive(pid: u32) -> bool {
    // Cheaper and dependency-free vs. a libc kill(pid, 0) FFI call: Linux
    // exposes process liveness directly via /proc.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn pid_is_alive(pid: u32) -> bool {
    // macOS/BSD have no /proc by default; `ps -p` is a portable enough
    // fallback (single fork, no libc FFI dependency to add for this).
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    // No cheap liveness check off Unix; treat as alive (conservative — a
    // stale lock just means the default-session picker skips one candidate
    // and falls further down the list or creates a new session, never a
    // crash).
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
        // The lock file exists and contains our PID.
        assert!(lock.path.exists());
        let contents = std::fs::read_to_string(&lock.path).unwrap();
        assert_eq!(contents.trim(), std::process::id().to_string());
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
    fn a_stale_lock_from_a_dead_process_does_not_count_as_held() {
        // A crashed grace must not permanently lock a session out.
        let id = unique_id("stale");
        let lock = SessionLock::acquire(&id).unwrap();
        let path = lock.path.clone();
        std::mem::forget(lock); // skip Drop so the file survives
        // PID 0 is never a live user process on Linux.
        std::fs::write(&path, "0").unwrap();
        assert!(!SessionLock::is_held(&id));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_garbage_lock_file_is_treated_as_unheld_not_a_panic() {
        let id = unique_id("garbage");
        let lock = SessionLock::acquire(&id).unwrap();
        let path = lock.path.clone();
        std::mem::forget(lock);
        std::fs::write(&path, "not-a-pid").unwrap();
        assert!(!SessionLock::is_held(&id));
        let _ = std::fs::remove_file(&path);
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
