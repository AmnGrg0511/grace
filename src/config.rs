//! Runtime configuration for the agent.
//!
//! One small struct, built from CLI args in `main`. This is the only place
//! that knows about *how* the agent is wired (which transport, which model).
//! The agent loop itself is transport-agnostic.

use crate::error::{AgentError, Result};
use crate::tool::ToolRegistry;
use crate::transport::ProviderTransport;

/// Default system identity. Grace is a calm, composed, capable agent. This is
/// seeded into every conversation unless the user overrides it with `--system`.
/// Kept concise so it does not eat model context.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Grace — a calm, composed, and capable AI agent. You address the user as \
\"Sir\". You are precise, warm but restrained, and you do real work via your tools \
(run_terminal, read_file, write_file, patch) rather than only talking about it. \
When a task needs a tool, call it. Keep responses concise and purposeful.";

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
    /// OpenRouter (HTTPS) via an auto-spawned python3 TLS proxy.
    OpenRouter { api_key: String, model: String },
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
            TransportConfig::OpenRouter { api_key, model } => {
                crate::transport_openrouter::OpenRouterTransport::new(api_key.clone(), model.clone())
                    .map(|t| Box::new(t) as Box<dyn ProviderTransport>)
            }
        }
    }

    /// The model name (empty for Mock).
    pub fn model(&self) -> &str {
        match &self.transport {
            TransportConfig::Mock { .. } => "mock",
            TransportConfig::Http { model, .. } => model,
            TransportConfig::OpenRouter { model, .. } => model,
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
        openrouter: bool,
        max_iterations: u32,
        system_prompt: Option<String>,
    ) -> Result<Config> {
        let transport = if mock {
            TransportConfig::Mock {
                max_tool_rounds: 3,
            }
        } else if openrouter {
            let model = model.ok_or_else(|| {
                AgentError::Config("missing --model (OpenRouter needs e.g. openai/gpt-4o-mini)".into())
            })?;
            // Prefer explicit --api-key, else the OPENROUTER_API_KEY env var.
            let api_key = api_key
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    AgentError::Config(
                        "missing OpenRouter API key: pass --api-key or set OPENROUTER_API_KEY".into(),
                    )
                })?;
            TransportConfig::OpenRouter { api_key, model }
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
