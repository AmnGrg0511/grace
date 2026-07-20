//! Settings — layered configuration: defaults -> ~/.grace/config.toml -> CLI flags.
//!
//! This is deliberately separate from [`crate::config::Config`] (the runtime
//! transport wiring) to avoid touching that file. `Settings` only fills in
//! `None` CLI values before `Config::from_args` is called.

use serde::Deserialize;
use std::path::PathBuf;

/// Optional overrides loaded from `~/.grace/config.toml`, then overridden by
/// explicit CLI flags at the call site.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Settings {
    pub default_model: Option<String>,
    pub default_base_url: Option<String>,
    pub memory_path: Option<String>,
    pub skills_dir: Option<String>,
    pub tools_dir: Option<String>,
    pub max_iterations: Option<u32>,
    pub request_timeout_secs: Option<u64>,
}

impl Settings {
    /// Default config file location: `~/.grace/config.toml`.
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("config.toml")
    }

    /// Load settings: start from defaults, then merge in `~/.grace/config.toml`
    /// if it exists and parses. Any I/O or parse error is treated as "no file"
    /// (falls back to defaults) so a missing/broken config never blocks startup.
    pub fn load() -> Settings {
        Self::load_from(&Self::default_path())
    }

    /// Same as [`Settings::load`] but reading from an explicit path — used by
    /// tests and by anyone that wants to point at a non-default location.
    pub fn load_from(path: &std::path::Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// Fill `None` values in the given CLI-provided `Option`s with settings
    /// values. Each field is only used if the CLI slot is currently `None`,
    /// so CLI flags always win.
    #[allow(clippy::too_many_arguments)]
    pub fn merge_into_args(
        &self,
        base_url: &mut Option<String>,
        model: &mut Option<String>,
        memory_path: &mut Option<String>,
        skills_dir: &mut Option<String>,
        tools_dir: &mut Option<String>,
        max_iterations: &mut Option<u32>,
    ) {
        if base_url.is_none() {
            *base_url = self.default_base_url.clone();
        }
        if model.is_none() {
            *model = self.default_model.clone();
        }
        if memory_path.is_none() {
            *memory_path = self.memory_path.clone();
        }
        if skills_dir.is_none() {
            *skills_dir = self.skills_dir.clone();
        }
        if tools_dir.is_none() {
            *tools_dir = self.tools_dir.clone();
        }
        if max_iterations.is_none() {
            *max_iterations = self.max_iterations;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_from_temp_config_toml() {
        let dir = std::env::temp_dir().join(format!("grace_settings_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
default_model = "gpt-4o-mini"
default_base_url = "https://api.openai.com/v1"
memory_path = "/tmp/mem.db"
skills_dir = "myskills"
tools_dir = "mytools"
max_iterations = 42
request_timeout_secs = 30
"#,
        )
        .unwrap();

        let settings = Settings::load_from(&path);
        assert_eq!(settings.default_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(settings.default_base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(settings.memory_path.as_deref(), Some("/tmp/mem.db"));
        assert_eq!(settings.skills_dir.as_deref(), Some("myskills"));
        assert_eq!(settings.tools_dir.as_deref(), Some("mytools"));
        assert_eq!(settings.max_iterations, Some(42));
        assert_eq!(settings.request_timeout_secs, Some(30));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = std::env::temp_dir().join("grace_settings_test_missing_does_not_exist.toml");
        let _ = std::fs::remove_file(&path);
        let settings = Settings::load_from(&path);
        assert!(settings.default_model.is_none());
    }

    #[test]
    fn merge_into_args_prefers_cli_values() {
        let settings = Settings {
            default_model: Some("from-settings".to_string()),
            ..Default::default()
        };
        let mut base_url = None;
        let mut model = Some("from-cli".to_string());
        let mut memory_path = None;
        let mut skills_dir = None;
        let mut tools_dir = None;
        let mut max_iterations = None;

        settings.merge_into_args(
            &mut base_url,
            &mut model,
            &mut memory_path,
            &mut skills_dir,
            &mut tools_dir,
            &mut max_iterations,
        );

        // CLI value wins.
        assert_eq!(model.as_deref(), Some("from-cli"));
    }
}
