//! Shared, dependency-light utilities used across every other module.
//!
//! Nothing here may depend on `core`, `transport`, `tools`, or `ui` — this is
//! the bottom layer of the crate, so a cycle here would be a layering bug.

pub mod diff;
pub mod error;
pub mod tokens;

#[cfg(test)]
pub mod test_support;

pub use error::{AgentError, Result};

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for any fallible crate function, so the assertions below
    /// exercise the real `Result` alias rather than a literal clippy can see
    /// through and flag as pointless.
    fn fallible(ok: bool) -> Result<u8> {
        if ok {
            Ok(7)
        } else {
            Err(AgentError::Tool("x".into()))
        }
    }

    #[test]
    fn the_error_type_and_alias_are_reachable_from_the_module_root() {
        assert_eq!(fallible(true).unwrap(), 7);
        assert_eq!(fallible(false).unwrap_err().to_string(), "tool error: x");
    }

    #[test]
    fn the_token_counter_and_diff_helpers_are_reachable() {
        use tokens::TokenCounter;
        assert!(tokens::default_counter().count_text("hello world") > 0);
        assert!(diff::unified_snippet("a", "b", 1).contains('b'));
    }
}
