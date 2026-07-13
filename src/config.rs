//! Runtime configuration for the agent.
//!
//! One small struct, built from CLI args in `main`. This is the only place
//! that knows about *how* the agent is wired (which transport, which model).
//! The agent loop itself is transport-agnostic.

use crate::error::{AgentError, Result};
use crate::tool::ToolRegistry;
use crate::transport::ProviderTransport;

/// How the agent reaches a model.
#[derive(Debug, Clone)]
pub enum TransportConfig {
    /// Scripted, offline model (tests/demos). No network.
    Mock { max_tool_rounds: u32 },
    /// Real OpenAI-compatible endpoint. `base_url` should be `http://`.
    Http {
        base_url: String,
        api_key: String,
        model: String,
    },
}

/// Full agent configuration.
pub struct Config {
    pub transport: TransportConfig,
    /// Hard cap on LLM round-trips per turn.
    pub max_iterations: u32,
    /// Optional system prompt prepended to the conversation.
    pub system_prompt: Option<String>,
}

impl Config {
    /// Build the configured transport as a `dyn ProviderTransport`.
    pub fn build_transport(&self) -> Result<Box<dyn ProviderTransport>> {
        match &self.transport {
            TransportConfig::Mock { max_tool_rounds } => {
                Ok(Box::new(crate::transport_mock::MockTransport::new(*max_tool_rounds)))
            }
            TransportConfig::Http {
                base_url,
                api_key,
                model: _,
            } => Ok(Box::new(crate::transport_http::HttpTransport::new(
                base_url.clone(),
                api_key.clone(),
            ))),
        }
    }

    /// The model name (empty for Mock).
    pub fn model(&self) -> &str {
        match &self.transport {
            TransportConfig::Mock { .. } => "mock",
            TransportConfig::Http { model, .. } => model,
        }
    }

    /// Default tool set. Centralizes "which tools exist".
    pub fn build_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        crate::tools::register_builtins(&mut reg);
        reg
    }
}

/// Helper so `main` can turn CLI flags into a [`Config`].
impl Config {
    pub fn from_args(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        mock: bool,
        max_iterations: u32,
        system_prompt: Option<String>,
    ) -> Result<Config> {
        let transport = if mock {
            TransportConfig::Mock {
                max_tool_rounds: 3,
            }
        } else {
            let base_url =
                base_url.ok_or_else(|| AgentError::Config("missing --base-url".into()))?;
            let api_key = api_key.unwrap_or_default();
            let model = model.ok_or_else(|| AgentError::Config("missing --model".into()))?;
            TransportConfig::Http {
                base_url,
                api_key,
                model,
            }
        };
        Ok(Config {
            transport,
            max_iterations: max_iterations.max(1),
            system_prompt,
        })
    }
}
