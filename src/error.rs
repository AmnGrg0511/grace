//! The single error type for the whole crate.
//!
//! A flat `thiserror`-style enum without the dependency: we implement a small
//! `Display` ourselves. Keeping one error type makes the agent loop and tool
//! plumbing uniform.

use std::fmt;

/// Errors produced anywhere in the core.
#[derive(Debug)]
pub enum AgentError {
    /// JSON parse failure.
    Json(String),
    /// I/O error (file/terminal ops).
    Io(std::io::Error),
    /// The transport failed to reach or parse the model endpoint.
    Transport(String),
    /// The model returned a response we could not understand.
    Response(String),
    /// A tool reported a failure (non-fatal; surfaced back to the model).
    Tool(String),
    /// Configuration was invalid.
    Config(String),
    /// The iteration/budget limit was reached before the model stopped.
    BudgetExhausted { iterations: u32 },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Json(s) => write!(f, "json error: {s}"),
            AgentError::Io(e) => write!(f, "io error: {e}"),
            AgentError::Transport(s) => write!(f, "transport error: {s}"),
            AgentError::Response(s) => write!(f, "bad response: {s}"),
            AgentError::Tool(s) => write!(f, "tool error: {s}"),
            AgentError::Config(s) => write!(f, "config error: {s}"),
            AgentError::BudgetExhausted { iterations } => {
                write!(f, "iteration budget exhausted after {iterations} iterations")
            }
        }
    }
}

impl std::error::Error for AgentError {}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        AgentError::Io(e)
    }
}

impl From<String> for AgentError {
    fn from(s: String) -> Self {
        AgentError::Tool(s)
    }
}

/// Convenience `Result` alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AgentError>;
