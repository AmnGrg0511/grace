//! The shared turn-interrupt flag.
//!
//! Lives in `util` — the bottom layer — so transport retry loops, which must
//! not depend on `tools`, can poll it (and cancel a backoff sleep) instead of
//! firing another request the user already Ctrl-C'd. Chat/CLI publish the flag
//! once at startup via [`set_flag`]; everything else reads it through
//! [`poll`]/[`sleep_interruptibly`], never through a cross-module dependency.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

static FLAG: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();

/// Publish the flag Ctrl-C latches. Called once at startup; a later caller
/// replaces the previous flag.
pub fn set_flag(flag: Arc<AtomicBool>) {
    *FLAG.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(flag);
}

/// `true` when the user has pressed Ctrl-C for the current turn. `false` when
/// no flag has been published (single-shot runs without a terminal).
pub fn poll() -> bool {
    FLAG
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|f| f.load(Ordering::SeqCst))
}

/// Sleep `d` in small slices so Ctrl-C lands before the full backoff elapses.
/// Returns `true` if the interrupt flag fired before the duration was up — the
/// caller turns that into `AgentError::Interrupted` instead of sleeping on.
pub fn sleep_interruptibly(d: Duration) -> bool {
    // 100ms slices keep a turn Ctrl-C-responsive without hammering atomic
    // loads on every interval.
    let step = Duration::from_millis(100);
    let mut remaining = d;
    while remaining > Duration::ZERO {
        if poll() {
            return true;
        }
        let slice = remaining.min(step);
        std::thread::sleep(slice);
        remaining -= slice;
    }
    poll()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The FLAG global is shared process-wide (chat/cli and the transport
    // layers all read the same one), so its tests must not interleave; a
    // dedicated test lock serializes them.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn no_flag_never_interrupts() {
        let _g = LOCK.lock().unwrap();
        set_flag(Arc::new(AtomicBool::new(false)));
        assert!(!poll());
        assert!(!sleep_interruptibly(Duration::from_millis(1)));
    }

    #[test]
    fn latched_flag_interrupts_immediately() {
        let _g = LOCK.lock().unwrap();
        set_flag(Arc::new(AtomicBool::new(true)));
        assert!(poll());
        assert!(sleep_interruptibly(Duration::from_millis(1_000)));
    }

    #[test]
    fn cleared_flag_polls_false() {
        let _g = LOCK.lock().unwrap();
        set_flag(Arc::new(AtomicBool::new(false)));
        assert!(!poll());
    }
}
