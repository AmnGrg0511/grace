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
When a task needs a tool, call it. Keep responses concise and purposeful.\n\
\n\
Skills: Use list_skills to discover available skills, then load_skill to load one \
when a task matches. Three default skills ship with Grace:\n\
- grace-agent: your own architecture and conventions\n\
- memory-update: when to persist a durable fact and how\n\
- skill-author: when and how to create a new skill\n\
\n\
Auto-identify: After completing a complex task (5+ tool calls, errors overcome, \
a reusable workflow), proactively load the skill-author skill and offer to save \
the approach. When the user states a stable preference or correction, proactively \
load the memory-update skill and offer to persist it. Do not ask to create skills \
or update memory for trivial tasks.";

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
    /// Real OpenAI-compatible endpoint (any base_url, including OpenRouter's
    /// and GitHub Copilot's — Copilot is just an `Http` transport whose key
    /// happens to be minted via OAuth device flow instead of pasted; see
    /// `transport_copilot::get_or_create_token`).
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
    /// Context compression settings.
    pub context_compression: ContextCompressionConfig,
}

/// OpenRouter's OpenAI-compatible base URL preset.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Context compression configuration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ContextCompressionConfig {
    /// Enable automatic context compression when threshold is reached.
    pub enabled: bool,
    /// Fraction of context window that triggers compression (0.0 to 1.0).
    /// e.g., 0.75 means compress when 75% of context window is used.
    pub trigger_fraction: f32,
    /// Target fraction after compression (0.0 to 1.0).
    /// e.g., 0.5 means compress down to 50% of context window.
    pub target_fraction: f32,
}

impl Default for ContextCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_fraction: 0.75,
            target_fraction: 0.5,
        }
    }
}

impl Config {
    /// Build the configured transport as a `dyn ProviderTransport`.
    pub fn build_transport(&self) -> Result<Box<dyn ProviderTransport>> {
        match &self.transport {
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

    /// The model name.
    pub fn model(&self) -> &str {
        match &self.transport {
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
    pub fn from_args(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        max_iterations: u32,
        system_prompt: Option<String>,
    ) -> Result<Config> {
        let base_url = base_url.ok_or_else(|| AgentError::Config("missing --base-url".into()))?;
        // Fall back to whichever env var the onboarding wizard actually
        // wrote this key under (~/.grace/.env, loaded into the process
        // env at startup) — not just OPENROUTER_API_KEY. Without this,
        // a restarted session with a custom/OpenAI/Copilot base_url
        // silently sends an empty bearer token (a real regression: the
        // wizard persists custom keys as GRACE_API_KEY, but this only
        // ever checked OPENROUTER_API_KEY).
        let api_key = api_key
            .or_else(|| std::env::var("GRACE_API_KEY").ok())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .or_else(|| std::env::var("GITHUB_COPILOT_TOKEN").ok())
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_default();
        let model = model.ok_or_else(|| AgentError::Config("missing --model".into()))?;
        let transport = TransportConfig::Http {
            base_url,
            api_key,
            model,
        };
        Ok(Config {
            transport,
            max_iterations: max_iterations.max(1),
            system_prompt,
            context_compression: ContextCompressionConfig::default(),
        })
    }
}