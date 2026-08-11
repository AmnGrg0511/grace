//! The single error type for the whole crate.
//!
//! Built with `thiserror` so `Display`/`Error`/`From` impls are derived from
//! the variant definitions themselves — the message and the variant can no
//! longer drift apart the way they could with a hand-written `match` in
//! `fmt::Display`.

use thiserror::Error;

/// Errors produced anywhere in the core.
#[derive(Debug, Error)]
pub enum AgentError {
    /// JSON (de)serialization failure.
    #[error("json error: {0}")]
    Json(String),

    /// I/O error (file/terminal ops).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The transport failed to reach or parse the model endpoint.
    #[error("transport error: {0}")]
    Transport(String),

    /// The model returned a response we could not understand.
    #[error("bad response: {0}")]
    Response(String),

    /// A tool reported a failure (non-fatal; surfaced back to the model).
    #[error("tool error: {0}")]
    Tool(String),

    /// Configuration was invalid.
    #[error("config error: {0}")]
    Config(String),

    /// The iteration/budget limit was reached before the model stopped.
    #[error("iteration budget exhausted after {iterations} iterations")]
    BudgetExhausted { iterations: u32 },

    /// A delegated sub-agent failed. Carries the sub-agent's label so a
    /// failure deep in a delegation tree is attributable to the branch that
    /// produced it rather than surfacing as an anonymous tool error.
    #[error("delegation '{task}' failed: {reason}")]
    Delegation { task: String, reason: String },

    /// The user hit Ctrl-C mid-turn — not a real failure, just "stop now
    /// and give me the prompt back".
    #[error("interrupted")]
    Interrupted,
}

impl From<String> for AgentError {
    fn from(s: String) -> Self {
        AgentError::Tool(s)
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(e: serde_json::Error) -> Self {
        AgentError::Json(e.to_string())
    }
}

impl From<reqwest::Error> for AgentError {
    fn from(e: reqwest::Error) -> Self {
        AgentError::Transport(e.to_string())
    }
}

/// Convenience `Result` alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_stable() {
        assert_eq!(
            AgentError::Tool("boom".into()).to_string(),
            "tool error: boom"
        );
        assert_eq!(
            AgentError::BudgetExhausted { iterations: 7 }.to_string(),
            "iteration budget exhausted after 7 iterations"
        );
        assert_eq!(AgentError::Interrupted.to_string(), "interrupted");
    }

    #[test]
    fn delegation_error_names_the_failing_subtask() {
        let e = AgentError::Delegation {
            task: "audit deps".into(),
            reason: "budget exhausted".into(),
        };
        assert_eq!(e.to_string(), "delegation 'audit deps' failed: budget exhausted");
    }

    #[test]
    fn io_errors_convert_via_from() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let e: AgentError = io.into();
        assert!(matches!(e, AgentError::Io(_)));
        assert!(e.to_string().starts_with("io error:"));
    }

    #[test]
    fn bare_strings_convert_to_tool_errors() {
        let e: AgentError = "something".to_string().into();
        assert!(matches!(e, AgentError::Tool(_)));
    }

    #[test]
    fn json_errors_convert_via_from() {
        let err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let e: AgentError = err.into();
        assert!(matches!(e, AgentError::Json(_)));
    }

    #[test]
    fn source_chain_is_available_for_io() {
        use std::error::Error as _;
        let io = std::io::Error::other("disk on fire");
        let e = AgentError::Io(io);
        assert!(e.source().is_some(), "thiserror #[from] should wire up source()");
    }
}
