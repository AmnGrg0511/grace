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
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Grace — a calm, composed, and capable AI agent. You address the user as \
\"Sir\". You are precise, warm but restrained, and you do real work via your tools \
(run_terminal, read_file, write_file, patch) rather than only talking about it. \
When a task needs a tool, call it. Keep responses concise and purposeful.";

/// Path to the user-editable persona file: `~/.grace/soul.md`. If present,
/// its content REPLACES [`DEFAULT_SYSTEM_PROMPT`] (still overridable by
/// `--system`) — this is what makes Grace's identity something you can
/// actually open and edit, not a string baked into the binary. Created with
/// the default persona on first run so it always exists.
pub fn soul_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("soul.md")
}

/// Load the persona from `soul.md`, creating it with the default persona if
/// missing. I/O errors fall back to the in-binary default so a filesystem
/// hiccup never breaks the agent's identity.
pub fn load_soul() -> String {
    let path = soul_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, DEFAULT_SYSTEM_PROMPT);
    DEFAULT_SYSTEM_PROMPT.to_string()
}

/// How the agent reaches a model.
#[derive(Debug, Clone)]
pub enum TransportConfig {
    /// Scripted, offline model (tests/demos). No network.
    Mock { max_tool_rounds: u32 },
    /// Real OpenAI-compatible endpoint (any base_url, including OpenRouter's).
    Http {
        base_url: String,
        api_key: String,
        model: String,
    },
}

impl TransportConfig {
    /// Re-derive the CLI flags that would reproduce this transport, so a
    /// delegated subagent subprocess inherits the *real* configured
    /// provider/model instead of silently falling back to `--mock`.
    pub fn to_cli_args(&self) -> Vec<String> {
        match self {
            TransportConfig::Mock { .. } => vec!["--mock".to_string()],
            TransportConfig::Http {
                base_url,
                api_key,
                model,
            } => vec![
                "--base-url".to_string(),
                base_url.clone(),
                "--api-key".to_string(),
                api_key.clone(),
                "--model".to_string(),
                model.clone(),
            ],
        }
    }
}

/// Full agent configuration.
pub struct Config {
    pub transport: TransportConfig,
    /// Hard cap on LLM round-trips per turn.
    pub max_iterations: u32,
    /// Optional system prompt prepended to the conversation.
    pub system_prompt: Option<String>,
}

/// OpenRouter's OpenAI-compatible base URL preset.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

impl Config {
    /// Build the configured transport as a `dyn ProviderTransport`.
    pub fn build_transport(&self) -> Result<Box<dyn ProviderTransport>> {
        match &self.transport {
            TransportConfig::Mock { max_tool_rounds } => Ok(Box::new(
                crate::transport_mock::MockTransport::new(*max_tool_rounds),
            )),
            TransportConfig::Http {
                base_url,
                api_key,
                model,
            } => Ok(Box::new(crate::transport_http::HttpTransport::with_model(
                base_url.clone(),
                api_key.clone(),
                model.clone(),
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

    /// Tool set plus skill discovery/loading tools bound to `skills_root`.
    pub fn build_registry_with_skills(skills_root: impl Into<std::path::PathBuf>) -> ToolRegistry {
        let mut reg = Self::build_registry();
        let store = std::sync::Arc::new(crate::skill::SkillStore::new(skills_root.into()));
        reg.register(Box::new(crate::skill::ListSkillsTool {
            store: store.clone(),
        }));
        reg.register(Box::new(crate::skill::LoadSkillTool { store }));
        reg
    }

    /// Tool set plus skill tools plus any plugin tools discovered under
    /// `tools_root` (see [`crate::plugin_tool::PluginToolStore`]). Additive
    /// on top of [`Config::build_registry_with_skills`] so callers can opt in
    /// without changing existing wiring.
    pub fn build_registry_with_plugins(
        skills_root: impl Into<std::path::PathBuf>,
        tools_root: impl Into<std::path::PathBuf>,
    ) -> ToolRegistry {
        let mut reg = Self::build_registry_with_skills(skills_root);
        let store = crate::plugin_tool::PluginToolStore::new(tools_root.into());
        for tool in store.load() {
            reg.register(tool);
        }
        reg
    }
}

/// Helper so `main` can turn CLI flags into a [`Config`].
impl Config {
    #[allow(clippy::too_many_arguments)]
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
            TransportConfig::Mock { max_tool_rounds: 2 }
        } else if openrouter {
            let model = model.ok_or_else(|| {
                AgentError::Config(
                    "missing --model (OpenRouter needs e.g. openai/gpt-4o-mini)".into(),
                )
            })?;
            // Prefer explicit --api-key, else the OPENROUTER_API_KEY env var.
            let api_key = api_key
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    AgentError::Config(
                        "missing OpenRouter API key: pass --api-key or set OPENROUTER_API_KEY"
                            .into(),
                    )
                })?;
            TransportConfig::Http {
                base_url: OPENROUTER_BASE_URL.to_string(),
                api_key,
                model,
            }
        } else {
            let base_url =
                base_url.ok_or_else(|| AgentError::Config("missing --base-url".into()))?;
            // Fall back to OPENROUTER_API_KEY (or any generic key already in
            // the environment via ~/.grace/.env) — not just the
            // --openrouter preset branch. Without this, config.toml-driven
            // custom base URLs silently send an empty bearer token.
            let api_key = api_key
                .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
                .unwrap_or_default();
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
