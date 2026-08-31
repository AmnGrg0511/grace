//! Request-level diagnostic logging, gated by the `GRACE_LOG` env var (any
//! non-empty value enables it). Off by default — normal runs pay nothing and
//! print nothing — and goes to stderr so it can never mangle the CLI's stdout
//! (the spinner, answers, or `/dump`'s OSC52 clipboard payload).
//!
//! This is a no-flag, opt-in debug aid: the user decided logging stays an env
//! var rather than a `--verbose`-style flag. It exists so a looping or empty
//! turn can be inspected at the wire level — what model, what messages, what
//! finish reason — instead of guessing from the screen.

/// Is `GRACE_LOG` enabled (present and non-empty)?
pub fn enabled() -> bool {
    std::env::var("GRACE_LOG").map(|v| !v.is_empty()).unwrap_or(false)
}

/// Write one diagnostic line to stderr when `GRACE_LOG` is on; a no-op
/// otherwise.
pub fn line(msg: &str) {
    use std::io::Write as _;
    if enabled() {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "[grace-log] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_gate_follows_the_grace_log_env_var() {
        use crate::util::test_support::EnvVarGuard;
        let _guard = EnvVarGuard::set("GRACE_LOG", "1");
        assert!(enabled(), "non-empty GRACE_LOG turns logging on");
    }

    #[test]
    fn log_gate_is_off_by_default_and_for_empty() {
        use crate::util::test_support::EnvVarGuard;
        let guard = EnvVarGuard::set("GRACE_LOG", "");
        assert!(!enabled(), "an empty GRACE_LOG means off");
        drop(guard);
        // Unset entirely (the clean default) is also off.
        assert!(!enabled());
    }
}
