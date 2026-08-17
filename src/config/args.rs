//! Runtime configuration: how the agent is wired, and what tools it gets.
//!
//! This is the only place that knows *how* the agent reaches a model and which
//! tools exist. The agent loop itself is transport-agnostic and registry-
//! agnostic — it is handed both.
//!
//! [`Config::build_registry_full`] is the single assembly point for a complete
//! tool set. Delegation registration used to live in `main.rs`, which meant any
//! other entry point silently built a Grace that could not delegate.

use crate::core::context::ContextCompressionConfig;
use crate::core::delegation::DelegationDepth;
use crate::session::SessionStore;
use crate::tools::{DelegateTool, SessionSearchTool, ToolRegistry};
use crate::transport::ProviderTransport;
use crate::util::{AgentError, Result};
use std::rc::Rc;
use std::sync::Arc;

/// OpenRouter's OpenAI-compatible base URL preset.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// How the agent reaches a model.
#[derive(Debug, Clone)]
pub enum TransportConfig {
    /// Real OpenAI-compatible endpoint (any base_url, including OpenRouter's
    /// and GitHub Copilot's — Copilot is just an `Http` transport whose key
    /// happens to be minted via OAuth device flow instead of pasted).
    Http {
        base_url: String,
        api_key: String,
        model: String,
    },
}

impl TransportConfig {
    /// Re-derive the CLI flags that would reproduce this transport.
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

    /// The configured model id.
    pub fn model(&self) -> &str {
        match self {
            TransportConfig::Http { model, .. } => model,
        }
    }
}

/// Which optional tool groups a registry should include.
///
/// Explicit rather than a pile of `Option` parameters, so a caller cannot
/// accidentally build a registry that is missing delegation (the exact bug
/// that arose when registration lived in `main.rs`).
#[derive(Clone)]
pub struct RegistryOptions {
    /// Root directory for skill discovery.
    pub skills_root: std::path::PathBuf,
    /// Root directory for executable plugin tools.
    pub tools_root: std::path::PathBuf,
    /// Session store for `session_search`. `None` omits the tool.
    pub sessions: Option<Arc<SessionStore>>,
    /// Transport for `delegate`. `None` omits the tool.
    pub transport: Option<Rc<dyn ProviderTransport>>,
    /// Compression policy applied to sub-agent conversations.
    pub compression: ContextCompressionConfig,
}

impl RegistryOptions {
    /// Minimal options: skills and plugins only, no delegation, no session
    /// search.
    pub fn new(
        skills_root: impl Into<std::path::PathBuf>,
        tools_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            skills_root: skills_root.into(),
            tools_root: tools_root.into(),
            sessions: None,
            transport: None,
            compression: ContextCompressionConfig::default(),
        }
    }

    #[must_use]
    pub fn with_sessions(mut self, sessions: Arc<SessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    #[must_use]
    pub fn with_transport(mut self, transport: Rc<dyn ProviderTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    #[must_use]
    pub fn with_compression(mut self, compression: ContextCompressionConfig) -> Self {
        self.compression = compression;
        self
    }
}

/// Full agent configuration.
#[derive(Debug)]
pub struct Config {
    pub transport: TransportConfig,
    /// Hard cap on LLM round-trips per turn.
    pub max_iterations: u32,
    /// Optional system prompt prepended to the conversation.
    pub system_prompt: Option<String>,
    /// Context compression settings.
    pub context_compression: ContextCompressionConfig,
}

impl Config {
    /// Build the configured transport as a `dyn ProviderTransport`.
    pub fn build_transport(&self) -> Result<Box<dyn ProviderTransport>> {
        match &self.transport {
            TransportConfig::Http {
                base_url,
                api_key,
                model,
            } => {
                // Copilot needs its OAuth token exchanged for a short-lived
                // session token, auto-refreshed — a plain HttpTransport
                // silently 404s after ~25 minutes.
                if base_url.trim_end_matches('/') == crate::transport::copilot::BASE_URL {
                    Ok(Box::new(crate::transport::CopilotTransport::new(
                        api_key.clone(),
                        model.clone(),
                    )))
                } else {
                    Ok(Box::new(crate::transport::HttpTransport::with_model(
                        base_url.clone(),
                        api_key.clone(),
                        model.clone(),
                    )))
                }
            }
        }
    }

    /// The model name.
    pub fn model(&self) -> &str {
        self.transport.model()
    }

    /// The built-in tool set only. Centralizes "which tools always exist".
    pub fn build_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        crate::tools::register_builtins(&mut reg);
        reg
    }

    /// Built-ins plus skill discovery/loading bound to `skills_root`.
    pub fn build_registry_with_skills(
        skills_root: impl Into<std::path::PathBuf>,
    ) -> ToolRegistry {
        let mut reg = Self::build_registry();
        let store = Arc::new(crate::skill::SkillStore::new(skills_root.into()));
        reg.register(Box::new(crate::skill::ListSkillsTool {
            store: store.clone(),
        }));
        reg.register(Box::new(crate::skill::LoadSkillTool { store }));
        reg
    }

    /// The above plus executable plugin tools discovered under `tools_root`.
    pub fn build_registry_with_plugins(
        skills_root: impl Into<std::path::PathBuf>,
        tools_root: impl Into<std::path::PathBuf>,
    ) -> ToolRegistry {
        let mut reg = Self::build_registry_with_skills(skills_root);
        let store = crate::tools::PluginToolStore::new(tools_root.into());
        for tool in store.load() {
            reg.register(tool);
        }
        reg
    }

    /// Assemble the complete tool set, including `session_search` and
    /// `delegate`.
    ///
    /// `depth` is the nesting level of the agent this registry is for. At the
    /// depth cap, `delegate` is simply not registered — nesting terminates
    /// structurally rather than depending on the model to stop asking.
    pub fn build_registry_full(options: &RegistryOptions, depth: DelegationDepth) -> ToolRegistry {
        let mut reg =
            Self::build_registry_with_plugins(&options.skills_root, &options.tools_root);

        if let Some(sessions) = &options.sessions {
            reg.register(Box::new(SessionSearchTool::new(Arc::clone(sessions))));
        }

        if let Some(transport) = &options.transport {
            if depth.may_delegate() {
                let sub_options = options.clone();
                let factory = Rc::new(move |child_depth: DelegationDepth| {
                    Self::build_registry_full(&sub_options, child_depth)
                });
                reg.register(Box::new(
                    DelegateTool::new(Rc::clone(transport), depth, factory)
                        .with_compression(options.compression.clone()),
                ));
            }
        }

        reg
    }

    /// Turn CLI flags into a [`Config`].
    pub fn from_args(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        max_iterations: u32,
        system_prompt: Option<String>,
    ) -> Result<Config> {
        let base_url = base_url.ok_or_else(|| AgentError::Config("missing --base-url".into()))?;
        // Fall back to the env var that matches the *resolved* host, not a
        // fixed chain. A fixed chain sent the right key to the wrong host
        // (e.g. an OPENAI_API_KEY onto a Copilot endpoint). The onboarding
        // wizard writes the key under the preset's var, so the host match
        // finds it again after a restart.
        let api_key = api_key
            .or_else(|| {
                let var = crate::config::settings::env_var_for_base_url(&base_url);
                std::env::var(var).ok()
            })
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_default();
        let model = model.ok_or_else(|| AgentError::Config("missing --model".into()))?;
        Ok(Config {
            transport: TransportConfig::Http {
                base_url,
                api_key,
                model,
            },
            max_iterations: max_iterations.max(1),
            system_prompt,
            context_compression: ContextCompressionConfig::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delegation::MAX_DELEGATION_DEPTH;
    use crate::message::Message;
    use crate::transport::{FinishReason, ModelResponse, ToolSpec};

    struct Stub;
    impl ProviderTransport for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn complete(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: "ok".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn options() -> RegistryOptions {
        RegistryOptions::new("/nonexistent/skills", "/nonexistent/tools")
    }

    #[test]
    fn from_args_requires_a_base_url() {
        let err = Config::from_args(None, None, Some("m".into()), 8, None).unwrap_err();
        assert!(err.to_string().contains("missing --base-url"));
    }

    #[test]
    fn from_args_requires_a_model() {
        let err =
            Config::from_args(Some("https://x/v1".into()), None, None, 8, None).unwrap_err();
        assert!(err.to_string().contains("missing --model"));
    }

    #[test]
    fn from_args_clamps_a_zero_iteration_budget_to_one() {
        let c = Config::from_args(
            Some("https://x/v1".into()),
            Some("k".into()),
            Some("m".into()),
            0,
            None,
        )
        .unwrap();
        assert_eq!(c.max_iterations, 1);
    }

    #[test]
    fn from_args_trims_and_keeps_an_explicit_key() {
        let c = Config::from_args(
            Some("https://x/v1".into()),
            Some("secret".into()),
            Some("m".into()),
            4,
            None,
        )
        .unwrap();
        match &c.transport {
            TransportConfig::Http { api_key, .. } => assert_eq!(api_key, "secret"),
        }
    }

    #[test]
    fn from_args_picks_the_key_env_var_of_the_resolved_host() {
        // A Copilot host must read GITHUB_COPILOT_TOKEN, never a stray
        // OPENAI_API_KEY set for some other endpoint. Two EnvVarGuards would
        // deadlock on the crate-wide env lock, so both vars are set and
        // restored under one lock by hand.
        let _lock = crate::util::test_support::env_guard();
        let saved = [
            std::env::var("OPENAI_API_KEY").ok(),
            std::env::var("GITHUB_COPILOT_TOKEN").ok(),
        ];
        std::env::set_var("OPENAI_API_KEY", "openai-key");
        std::env::set_var("GITHUB_COPILOT_TOKEN", "copilot-key");
        let outcome = std::panic::catch_unwind(|| {
            let c = Config::from_args(
                Some("https://api.githubcopilot.com".into()),
                None,
                Some("gpt-4o".into()),
                4,
                None,
            )
            .unwrap();
            match &c.transport {
                TransportConfig::Http { api_key, .. } => assert_eq!(api_key, "copilot-key"),
            }
        });
        for (i, key) in ["OPENAI_API_KEY", "GITHUB_COPILOT_TOKEN"].iter().enumerate() {
            match &saved[i] {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        if outcome.is_err() {
            panic!("from_args picked the wrong key for the Copilot host");
        }
    }

    #[test]
    fn from_args_treats_an_empty_matched_key_env_var_as_absent() {
        // An empty var must not short-circuit into a useless bearer token.
        let _guard = crate::util::test_support::EnvVarGuard::set("GRACE_API_KEY", " ");
        let c = Config::from_args(
            Some("https://custom.example/v1".into()),
            None,
            Some("m".into()),
            4,
            None,
        )
        .unwrap();
        match &c.transport {
            TransportConfig::Http { api_key, .. } => assert_eq!(api_key, ""),
        }
    }

    #[test]
    fn to_cli_args_round_trips_the_transport() {
        let t = TransportConfig::Http {
            base_url: "https://x/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
        };
        assert_eq!(
            t.to_cli_args(),
            vec!["--base-url", "https://x/v1", "--api-key", "k", "--model", "m"]
        );
    }

    #[test]
    fn copilot_base_url_selects_the_copilot_transport() {
        // A plain HttpTransport against Copilot silently 404s once the OAuth
        // token needs refreshing, so this routing matters.
        let c = Config {
            transport: TransportConfig::Http {
                base_url: crate::transport::copilot::BASE_URL.to_string(),
                api_key: "tok".into(),
                model: "gpt-4o".into(),
            },
            max_iterations: 4,
            system_prompt: None,
            context_compression: ContextCompressionConfig::default(),
        };
        assert_eq!(c.build_transport().unwrap().name(), "github-copilot");
    }

    #[test]
    fn a_generic_base_url_selects_the_http_transport() {
        let c = Config::from_args(
            Some("https://api.openai.com/v1".into()),
            Some("k".into()),
            Some("gpt-4o".into()),
            4,
            None,
        )
        .unwrap();
        assert_eq!(c.build_transport().unwrap().name(), "openai-http");
    }

    #[test]
    fn the_base_registry_holds_only_builtins() {
        let reg = Config::build_registry();
        assert_eq!(reg.len(), 4);
        assert!(reg.get("delegate").is_none());
    }

    #[test]
    fn the_skills_registry_adds_the_skill_tools() {
        let reg = Config::build_registry_with_skills("/nonexistent/skills");
        assert!(reg.get("list_skills").is_some());
        assert!(reg.get("load_skill").is_some());
    }

    #[test]
    fn delegate_is_registered_centrally_not_by_the_caller() {
        // Regression: registration used to live in main.rs, so any other entry
        // point silently built a Grace that could not delegate.
        let opts = options().with_transport(Rc::new(Stub));
        let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);
        assert!(reg.get("delegate").is_some());
    }

    #[test]
    fn delegate_is_omitted_without_a_transport() {
        let reg = Config::build_registry_full(&options(), DelegationDepth::ROOT);
        assert!(reg.get("delegate").is_none());
    }

    #[test]
    fn delegate_is_withdrawn_at_the_depth_cap() {
        // Nesting must terminate structurally, not by asking the model nicely.
        let opts = options().with_transport(Rc::new(Stub));
        let reg = Config::build_registry_full(&opts, DelegationDepth(MAX_DELEGATION_DEPTH));
        assert!(
            reg.get("delegate").is_none(),
            "a sub-agent at the cap must not be handed delegate"
        );
        assert!(reg.get("read").is_some(), "other tools remain");
    }

    #[test]
    fn session_search_is_registered_when_a_store_is_supplied() {
        let path = std::env::temp_dir().join(format!(
            "grace_cfg_sessions_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        #[allow(clippy::arc_with_non_send_sync)]
        let store = Arc::new(SessionStore::open(&path).unwrap());
        let opts = options().with_sessions(store);
        let reg = Config::build_registry_full(&opts, DelegationDepth::ROOT);
        assert!(reg.get("session_search").is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_search_is_omitted_without_a_store() {
        let reg = Config::build_registry_full(&options(), DelegationDepth::ROOT);
        assert!(reg.get("session_search").is_none());
    }
}
