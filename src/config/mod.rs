//! Configuration — how the agent is wired, and who it is.
//!
//! ```text
//! args.rs      Config / TransportConfig / registry assembly
//! settings.rs  ~/.grace/config.toml, provider presets, model context windows
//! soul.rs      the persona (DEFAULT_SYSTEM_PROMPT + ~/.grace/soul.md)
//! ```
//!
//! Layering is defaults -> `config.toml` -> CLI flags, with CLI winning.

pub mod args;
pub mod settings;
pub mod soul;

pub use args::{Config, RegistryOptions, TransportConfig, OPENROUTER_BASE_URL};
pub use settings::{context_window_for, KnownModel, ProviderPreset, Settings, PROVIDER_PRESETS};
pub use soul::{build_system_prompt, load_soul, soul_path, DEFAULT_SYSTEM_PROMPT};

/// Re-exported for backwards compatibility: compression config conceptually
/// belongs to the agent engine, but callers reach for it as configuration.
pub use crate::core::context::ContextCompressionConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_surface_is_reachable_from_the_module_root() {
        let cfg = Config::from_args(
            Some("https://x/v1".into()),
            Some("k".into()),
            Some("m".into()),
            8,
            None,
        )
        .unwrap();
        assert_eq!(cfg.model(), "m");
        assert!(matches!(cfg.transport, TransportConfig::Http { .. }));

        let _opts = RegistryOptions::new("/s", "/t");
        let _settings = Settings::default();
        assert!(OPENROUTER_BASE_URL.starts_with("https://"));
    }

    #[test]
    fn the_persona_and_model_table_are_reachable() {
        assert!(!DEFAULT_SYSTEM_PROMPT.is_empty());
        assert!(soul_path().ends_with(".grace/soul.md"));
        let _: fn() -> String = load_soul;
        #[allow(clippy::type_complexity)]
        let _: fn(
            Option<&str>,
            &crate::memory::Memory,
            &crate::skill::SkillStore,
            &crate::session::SessionStore,
            Option<&str>,
        ) -> crate::util::Result<String> = build_system_prompt;
        let _: fn(&str) -> Option<u32> = context_window_for;
        assert!(!PROVIDER_PRESETS.is_empty());
    }

    #[test]
    fn compression_config_is_re_exported_where_callers_look_for_it() {
        // It lives in `core` but is reached for as configuration.
        let cfg = ContextCompressionConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.target_fraction < cfg.trigger_fraction);
    }
}
