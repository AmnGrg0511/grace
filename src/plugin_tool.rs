//! Plug-in tool loader — discovers external tools from a directory tree.
//!
//! Convention: `tools/<name>/manifest.json` with shape
//! `{"name","description","parameters","command"}` where `command` is
//! executed (relative to the tool's own directory, or absolute) with a
//! single argv[1] containing the JSON-serialized arguments.

use crate::error::{AgentError, Result};
use crate::tool::Tool;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    name: String,
    description: String,
    #[serde(default = "default_params")]
    parameters: Value,
    command: String,
}

fn default_params() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// One externally-defined tool: runs `command` with the JSON args as argv[1].
pub struct PluginTool {
    manifest: Manifest,
    /// Directory containing the manifest; used to resolve a relative `command`.
    tool_dir: PathBuf,
}

impl PluginTool {
    fn resolved_command(&self) -> PathBuf {
        let cmd = Path::new(&self.manifest.command);
        if cmd.is_absolute() {
            cmd.to_path_buf()
        } else {
            self.tool_dir.join(cmd)
        }
    }
}

impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn parameters(&self) -> Value {
        self.manifest.parameters.clone()
    }

    fn run(&self, args: &Value) -> Result<String> {
        let arg_json = serde_json::to_string(args)
            .map_err(|e| AgentError::Tool(format!("serialize args for '{}': {e}", self.manifest.name)))?;
        let command = self.resolved_command();
        let output = Command::new(&command)
            .arg(&arg_json)
            .current_dir(&self.tool_dir)
            .output()
            .map_err(|e| {
                AgentError::Tool(format!(
                    "failed to spawn plugin tool '{}' ({}): {e}",
                    self.manifest.name,
                    command.display()
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(stdout.trim_end());
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&format!("[stderr] {}", stderr.trim_end()));
        }
        if !output.status.success() {
            result.push_str(&format!("\n[exit code {}]", output.status.code().unwrap_or(-1)));
        }
        Ok(result)
    }
}

/// Scans a directory of `<name>/manifest.json` subdirectories and builds
/// [`PluginTool`]s for each valid manifest found. Invalid/missing manifests
/// are skipped silently (best-effort discovery, not a hard requirement).
pub struct PluginToolStore {
    root: PathBuf,
}

impl PluginToolStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default location: `./tools` relative to the current working directory.
    pub fn default_root() -> PathBuf {
        PathBuf::from("tools")
    }

    /// Discover all tools under `root`, returning them boxed for direct
    /// registration into a [`crate::tool::ToolRegistry`].
    pub fn load(&self) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return tools;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
                continue;
            };
            tools.push(Box::new(PluginTool { manifest, tool_dir: path }));
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_executes_a_plugin_tool() {
        let dir = std::env::temp_dir().join(format!("grace_plugin_tool_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tool_dir = dir.join("echoer");
        std::fs::create_dir_all(&tool_dir).unwrap();

        std::fs::write(
            tool_dir.join("manifest.json"),
            serde_json::json!({
                "name": "echoer",
                "description": "Echoes its JSON arg back.",
                "parameters": {"type": "object", "properties": {"text": {"type": "string"}}},
                "command": "./run.sh",
            })
            .to_string(),
        )
        .unwrap();

        let script = "#!/bin/sh\necho \"got: $1\"\n";
        let script_path = tool_dir.join("run.sh");
        std::fs::write(&script_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let store = PluginToolStore::new(&dir);
        let tools = store.load();
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.name(), "echoer");

        let out = tool.run(&serde_json::json!({"text": "hello"})).unwrap();
        assert!(out.contains("got:"));
        assert!(out.contains("hello"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_yields_no_tools() {
        let store = PluginToolStore::new("/nonexistent/path/grace_test_xyz");
        assert!(store.load().is_empty());
    }
}
