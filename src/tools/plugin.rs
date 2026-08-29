//! Plug-in tool loader — discovers external tools from a directory tree.
//!
//! Convention: `tools/<name>/manifest.json` with shape
//! `{"name","description","parameters","command"}` where `command` is
//! executed (relative to the tool's own directory, or absolute) with a
//! single argv[1] containing the JSON-serialized arguments.
//!
//! Mid-run user prompts: every invocation gets a private mailbox directory
//! via the `GRACE_PROMPT_DIR` environment variable. To ask the user
//! something, write `request.json` — `{"question": "…", "choices":
//! ["a","b"] (optional), "allow_custom": true (optional, default true)}` —
//! and wait for `response.json`: `{"answer": "…"}`, `{"declined": true}`,
//! `{"unavailable": true}` (no interactive channel — e.g. one-shot mode),
//! or `{"error": "…"}` (malformed request). Remove both files before the
//! next prompt; one prompt at a time. grace polls the mailbox every 100 ms
//! while the plugin runs and routes each request through the mid-turn
//! interaction channel (`tools/interaction.rs`).

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
        let timeout_secs = self.manifest.timeout_secs;

        // The invocation's prompt mailbox: created up front so the child
        // sees the directory before it starts, and removed when the run is
        // over (timeout included).
        let prompt_dir = create_prompt_dir(&self.manifest.name)?;
        // Put the child in its own process group so we can kill the entire
        // tree (e.g., `sh -c "something &"`) on timeout, not just the
        // immediate child process.
        let child = {
            let mut cmd = Command::new(&command);
            // Explicit pipes: wait_with_output honors the configured Stdio
            // (default inherit) since the implicit-pipe behavior was removed.
            cmd.arg(&arg_json)
                .current_dir(&self.tool_dir)
                .env("GRACE_PROMPT_DIR", &prompt_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
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
        let child_pid = child.id();

        // The waiter thread owns the child; the run loop polls it every 100
        // ms so a prompt request from the plugin can be answered while it
        // still runs.
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_name = format!("plugin-wait-{child_pid}");
        let _guard = std::thread::Builder::new().name(thread_name).spawn(move || {
            let output = child.wait_with_output().unwrap_or_else(|_| empty_output());
            let _ = tx.send(output);
        });

        let deadline = timeout_secs.map(|secs| {
            std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.max(0.1))
        });
        let outcome = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(output) => break Ok(format_output(output)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    service_prompt_request(&prompt_dir);
                    if let Some(d) = deadline {
                        if std::time::Instant::now() >= d {
                            kill_child(child_pid);
                            break Err(AgentError::Tool(format!(
                                "plugin tool '{}' timed out after {:.0}s",
                                self.manifest.name, timeout_secs.unwrap_or(0.0)
                            )));
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // The waiter died without delivering: report, don't hang.
                    break Ok(format_output(empty_output()));
                }
            }
        };
        let _ = std::fs::remove_dir_all(&prompt_dir);
        outcome
    }
}

fn empty_output() -> std::process::Output {
    std::process::Output {
        status: std::process::ExitStatus::default(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

/// Kill a spawned plugin: its entire process group on Unix (the child was
/// spawned with `process_group(0)`, so its PID is the PGID), the whole
/// process tree via taskkill elsewhere.
fn kill_child(child_pid: u32) {
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
}

/// One private mailbox directory per invocation — the plugin's
/// `GRACE_PROMPT_DIR`. Unique per process + invocation so concurrent plugin
/// calls never share a mailbox.
fn create_prompt_dir(name: &str) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "grace_prompt_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ));
    std::fs::create_dir_all(&dir).map_err(|e| {
        AgentError::Tool(format!("could not create prompt mailbox for '{}': {e}", name))
    })?;
    Ok(dir)
}

/// One shape of `request.json`: what the plugin wants to ask the user.
#[derive(Debug, Deserialize)]
struct PromptRequest {
    question: String,
    #[serde(default)]
    choices: Option<Vec<String>>,
    #[serde(default = "default_allow_custom")]
    allow_custom: bool,
}

fn default_allow_custom() -> bool {
    true
}

/// If the mailbox holds a `request.json`, ask the user through the mid-turn
/// interaction channel and publish the answer as `response.json` (atomic:
/// written to a temp name then renamed, so the plugin never reads a half
/// file). One prompt at a time; both files are removed afterwards.
fn service_prompt_request(dir: &Path) {
    let request_path = dir.join("request.json");
    let Ok(raw) = std::fs::read_to_string(&request_path) else {
        return;
    };
    let response = match serde_json::from_str::<PromptRequest>(&raw) {
        Err(e) => serde_json::json!({ "error": format!("malformed request.json: {e}") }),
        Ok(req) => match crate::tools::interaction::prompt(&crate::tools::interaction::UserPrompt {
            question: req.question,
            choices: req.choices,
            allow_custom: req.allow_custom,
        }) {
            crate::tools::interaction::PromptResult::Answered(a) => serde_json::json!({ "answer": a }),
            crate::tools::interaction::PromptResult::Declined => serde_json::json!({ "declined": true }),
            crate::tools::interaction::PromptResult::Unavailable => {
                serde_json::json!({ "unavailable": true })
            }
        },
    };
    let tmp = dir.join("response.json.tmp");
    match std::fs::write(&tmp, response.to_string()) {
        Ok(()) => {
            if std::fs::rename(&tmp, dir.join("response.json")).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    let _ = std::fs::remove_file(&request_path);
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

    /// Default location: `~/.grace/tools`, alongside the other grace
    /// state (skills, memory, sessions) — not the working directory, so a
    /// tool installed once is found from every directory.
    pub fn default_root() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grace")
            .join("tools")
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
    fn the_default_root_lives_under_dot_grace() {
        // Plugin tools must be found the same way skills and memory are —
        // from $HOME, not the CWD, so a tool installed once works from any
        // directory.
        assert!(PluginToolStore::default_root().ends_with(".grace/tools"));
    }

    #[test]
    fn a_plugin_prompt_is_routed_through_the_mailbox() {
        // Tests run without a TTY, so the channel reports Unavailable and
        // the plugin must see `{"unavailable": true}` — proving the full
        // loop: env var, request poll, channel call, atomic response. The
        // slot lock keeps the slot tests' prompters out of this window.
        let _slot_lock = crate::util::test_support::prompt_slot_guard();
        let dir =
            std::env::temp_dir().join(format!("grace_plugin_prompt_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tool_dir = dir.join("asker");
        std::fs::create_dir_all(&tool_dir).unwrap();

        std::fs::write(
            tool_dir.join("manifest.json"),
            serde_json::json!({
                "name": "asker",
                "description": "Asks the user a question mid-run.",
                "parameters": {"type": "object", "properties": {}},
                "command": "./ask.sh",
                "timeout": 20,
            })
            .to_string(),
        )
        .unwrap();

        let script = "#!/bin/sh\n\
                      echo '{\"question\": \"proceed?\"}' > \"$GRACE_PROMPT_DIR/request.json\"\n\
                      i=0\n\
                      while [ ! -f \"$GRACE_PROMPT_DIR/response.json\" ]; do\n\
                          i=$((i+1))\n\
                          [ $i -gt 15 ] && { echo \"no response\" >&2; exit 1; }\n\
                          sleep 1\n\
                      done\n\
                      cat \"$GRACE_PROMPT_DIR/response.json\"\n\
                      echo\n";
        let script_path = tool_dir.join("ask.sh");
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
        let out = tools[0].run(&serde_json::json!({})).unwrap();
        assert!(
            out.contains("unavailable"),
            "the non-TTY channel must report unavailable to the plugin: {out:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
