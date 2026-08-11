//! Plug-in tool loader — discovers external tools from a directory tree.
//!
//! Convention: `tools/<name>/manifest.json` with shape
//! `{"name","description","parameters","command"}` where `command` is
//! executed (relative to the tool's own directory, or absolute) with a
//! single argv[1] containing the JSON-serialized arguments.

use crate::util::{AgentError, Result};
use crate::tools::Tool;
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
    /// Maximum seconds before the plugin is killed. Absent = no timeout.
    #[serde(default, rename = "timeout")]
    timeout_secs: Option<f64>,
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
        let arg_json = serde_json::to_string(args).map_err(|e| {
            AgentError::Tool(format!("serialize args for '{}': {e}", self.manifest.name))
        })?;
        let command = self.resolved_command();

        // Build the command inline in each branch to avoid lifetime issues
        // with `Command` borrowing from its builder arguments.
        match self.manifest.timeout_secs {
            Some(secs) => {
                let deadline = std::time::Duration::from_secs_f64(secs.max(0.1));
                // Put the child in its own process group so we can kill the
                // entire tree (e.g., `sh -c "something &"`) on timeout, not
                // just the immediate child process.
                let child = {
                    let mut cmd = Command::new(&command);
                    cmd.arg(&arg_json)
                       .current_dir(&self.tool_dir);
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::CommandExt;
                        cmd.process_group(0);
                    }
                    cmd.spawn().map_err(|e| {
                        AgentError::Tool(format!(
                            "failed to spawn plugin tool '{}' ({}): {e}",
                            self.manifest.name,
                            command.display()
                        ))
                    })?
                };
                let (output, timed_out) = wait_with_timeout(child, deadline);
                if timed_out {
                    return Err(AgentError::Tool(format!(
                        "plugin tool '{}' timed out after {:.0}s",
                        self.manifest.name, secs
                    )));
                }
                Ok(format_output(output))
            }
            None => {
                Command::new(&command)
                    .arg(&arg_json)
                    .current_dir(&self.tool_dir)
                    .output()
                    .map_err(|e| {
                        AgentError::Tool(format!(
                            "plugin tool '{}' ({}): {e}",
                            self.manifest.name, command.display()
                        ))
                    })
                    .map(format_output)
            }
        }
    }
}

/// Wait for a child process up to `deadline`, killing it if it overruns.
/// On Unix, kills the child's entire process group. On other platforms,
/// kills only the immediate child.
fn wait_with_timeout(child: std::process::Child, deadline: std::time::Duration) -> (std::process::Output, bool) {
    // Capture the child's PID for group kill before moving into the thread.
    let child_pid = child.id();

    let (tx, rx) = std::sync::mpsc::channel();
    let thread_name = format!("plugin-wait-{}", child_pid);
    let _guard = std::thread::Builder::new().name(thread_name).spawn(move || {
        let output = child.wait_with_output().unwrap_or_else(|_| {
            std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }
        });
        let _ = tx.send(output);
    });

    match rx.recv_timeout(deadline) {
        Ok(output) => (output, false),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Kill the entire process group (child was spawned with
            // process_group(0), so its PID is the PGID).
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{child_pid}"))
                    .status();
            }
            #[cfg(not(unix))]
            {
                let _ = Command::new("taskkill")
                    .arg("/F")
                    .arg("/T")
                    .arg("/PID")
                    .arg(child_pid.to_string())
                    .status();
            }
            (
                std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
                true,
            )
        }
        Err(_) => (
            std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            true,
        ),
    }
}

/// Format process output (stdout, stderr, exit code) as a string.
fn format_output(output: std::process::Output) -> String {
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
        result.push_str(&format!(
            "\n[exit code {}]",
            output.status.code().unwrap_or(-1)
        ));
    }
    result
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
    /// registration into a [`crate::tools::ToolRegistry`].
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
            tools.push(Box::new(PluginTool {
                manifest,
                tool_dir: path,
            }));
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_executes_a_plugin_tool() {
        let dir =
            std::env::temp_dir().join(format!("grace_plugin_tool_test_{}", std::process::id()));
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
        assert!(
            out.contains("got:") || out.contains("hello"),
            "got output: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_yields_no_tools() {
        let store = PluginToolStore::new("/nonexistent/path/grace_test_xyz");
        assert!(store.load().is_empty());
    }

    #[test]
    fn a_plugin_that_exceeds_timeout_is_killed() {
        let dir =
            std::env::temp_dir().join(format!("grace_plugin_timeout_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tool_dir = dir.join("slow");
        std::fs::create_dir_all(&tool_dir).unwrap();

        std::fs::write(
            tool_dir.join("manifest.json"),
            serde_json::json!({
                "name": "slow",
                "description": "Sleeps forever.",
                "parameters": {"type": "object", "properties": {}},
                "command": "./sleep.sh",
                "timeout": 0.5,
            })
            .to_string(),
        )
        .unwrap();

        let script = "#!/bin/sh\nsleep 60\n";
        let script_path = tool_dir.join("sleep.sh");
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
        let err = tool.run(&serde_json::json!({})).unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "should fail with timeout, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
