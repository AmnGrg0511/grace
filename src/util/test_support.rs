//! Test-only helpers. Not compiled into the binary.

/// Serializes every test that mutates a process-global environment variable.
///
/// `cargo test` runs tests in parallel threads within one process, so
/// `set_var`/`remove_var` are shared mutable state across the *whole crate*,
/// not just the module doing it. A per-module mutex is not enough: the
/// `GRACE_ALLOW_DIR` jail tests in `tools::builtins::file` and the unrelated
/// file/patch tests in sibling modules raced each other, and the jail leaked
/// into tests that never asked for one — producing failures that looked like
/// real path-handling bugs.
///
/// One crate-wide lock makes env mutation effectively sequential, which is
/// what real single-process usage always was anyway.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the env lock, recovering from a poisoned mutex.
///
/// A panicking test poisons the lock; without recovery, one genuine failure
/// cascades into every other env-touching test failing for an unrelated
/// reason, which buries the actual signal.
pub fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Set an environment variable for the lifetime of the returned guard,
/// restoring the previous value (or absence) on drop — even if the test
/// panics. This is what stops a failing test from leaking a jail into
/// everything that runs after it.
pub struct EnvVarGuard {
    key: String,
    previous: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    pub fn set(key: &str, value: &str) -> Self {
        let lock = env_guard();
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self {
            key: key.to_string(),
            previous,
            _lock: lock,
        }
    }

    /// Hold the lock without setting anything — for a test that must merely
    /// be sure no *other* test's variable is visible.
    pub fn none() -> Self {
        let lock = env_guard();
        Self {
            key: String::new(),
            previous: None,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if self.key.is_empty() {
            return;
        }
        match &self.previous {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variable_is_visible_while_the_guard_lives() {
        let _g = EnvVarGuard::set("GRACE_TEST_SUPPORT_VAR", "yes");
        assert_eq!(
            std::env::var("GRACE_TEST_SUPPORT_VAR").as_deref(),
            Ok("yes")
        );
    }

    #[test]
    fn the_variable_is_removed_again_on_drop() {
        {
            let _g = EnvVarGuard::set("GRACE_TEST_SUPPORT_DROP", "yes");
        }
        assert!(std::env::var("GRACE_TEST_SUPPORT_DROP").is_err());
    }

    #[test]
    fn a_previous_value_is_restored_rather_than_removed() {
        std::env::set_var("GRACE_TEST_SUPPORT_PREV", "original");
        {
            let _g = EnvVarGuard::set("GRACE_TEST_SUPPORT_PREV", "temporary");
            assert_eq!(
                std::env::var("GRACE_TEST_SUPPORT_PREV").as_deref(),
                Ok("temporary")
            );
        }
        assert_eq!(
            std::env::var("GRACE_TEST_SUPPORT_PREV").as_deref(),
            Ok("original")
        );
        std::env::remove_var("GRACE_TEST_SUPPORT_PREV");
    }

    #[test]
    fn a_panic_still_restores_the_environment() {
        // The whole reason this is RAII: a failing test must not leak a jail
        // into every test that runs after it.
        let _ = std::panic::catch_unwind(|| {
            let _g = EnvVarGuard::set("GRACE_TEST_SUPPORT_PANIC", "leaked");
            panic!("boom");
        });
        assert!(std::env::var("GRACE_TEST_SUPPORT_PANIC").is_err());
    }
}
