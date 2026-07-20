//! Delegation tool — spawns a fresh isolated `grace` subprocess to work a
//! subtask, with no shared message history. Useful for splitting work into
//! independent subagent calls.

use crate::error::{AgentError, Result};
use crate::tool::Tool;
use serde_json::Value;
use std::process::Command;

/// A tool named `delegate`: runs `{task, model?}` as a brand-new `grace
/// --prompt <task>` subprocess and returns its cleaned answer text.
pub struct DelegateTool {
    /// Extra flags to pass through to the child (e.g. `--mock` or
    /// `--base-url ... --api-key ... --model ...`), so the child inherits the
    /// parent's transport instead of guessing.
    pub transport_args: Vec<String>,
}

impl DelegateTool {
    /// Build a delegate tool that spawns children in `--mock` mode
    /// (deterministic, no network — used by tests and safe default).
    pub fn mock() -> Self {
        Self {
            transport_args: vec!["--mock".to_string()],
        }
    }

    /// Build a delegate tool whose children inherit the parent's real,
    /// currently-configured transport (base-url/api-key/model, or --mock)
    /// instead of guessing — this is what makes `delegate` actually usable
    /// against a live provider rather than silently downgrading to mock.
    pub fn for_transport(transport: &crate::config::TransportConfig) -> Self {
        Self {
            transport_args: transport.to_cli_args(),
        }
    }

    /// Strip the `[grace] transport=...` banner line and the
    /// `--- answer ---` marker from a child's stdout, leaving just the answer
    /// text.
    fn clean_output(stdout: &str) -> String {
        let mut lines: Vec<&str> = stdout.lines().collect();
        // Drop the banner line(s) that start with "[grace]".
        lines.retain(|l| !l.starts_with("[grace]"));
        let joined = lines.join("\n");
        match joined.find("--- answer ---") {
            Some(idx) => joined[idx + "--- answer ---".len()..].trim().to_string(),
            None => joined.trim().to_string(),
        }
    }
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a subtask to a fresh, isolated grace subagent (no shared history) and return its final answer."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The instruction to hand to the subagent."},
                "model": {"type": "string", "description": "Optional model override for the subagent."},
            },
            "required": ["task"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::Tool("missing string argument 'task'".to_string()))?;

        let current_exe = std::env::current_exe()
            .map_err(|e| AgentError::Tool(format!("could not locate current executable: {e}")))?;

        let mut cmd = Command::new(&current_exe);
        cmd.args(&self.transport_args);
        if let Some(model) = args.get("model").and_then(Value::as_str) {
            cmd.arg("--model").arg(model);
        }
        cmd.arg("--prompt").arg(task);

        let output = cmd
            .output()
            .map_err(|e| AgentError::Tool(format!("failed to spawn delegate subprocess: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AgentError::Tool(format!(
                "delegate subprocess exited with {:?}: {}",
                output.status.code(),
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Self::clean_output(&stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_output_strips_banner_and_marker() {
        let raw = "[grace] transport=mock model=mock tools=4\n\n--- answer ---\nUnderstood. (mock response after 0 tool round(s))\n";
        let cleaned = DelegateTool::clean_output(raw);
        assert_eq!(cleaned, "Understood. (mock response after 0 tool round(s))");
    }

    /// Spawns the real release binary in --mock mode, if present. Skips
    /// gracefully (does not fail the suite) when the binary hasn't been
    /// built yet at test time.
    #[test]
    fn delegate_spawns_real_mock_subprocess_if_binary_present() {
        let release_bin =
            std::path::PathBuf::from("/calypto/scratch/amagar24/grace-target/release/grace");
        if !release_bin.exists() {
            eprintln!(
                "skipping: release binary not built at {}",
                release_bin.display()
            );
            return;
        }

        // We can't swap `current_exe()` easily inside the test, so directly
        // exercise the subprocess+clean_output path against the known binary.
        let output = Command::new(&release_bin)
            .args(["--mock", "--prompt", "say hello"])
            .output()
            .expect("spawn release binary");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let cleaned = DelegateTool::clean_output(&stdout);
        assert!(
            cleaned.contains("Understood."),
            "unexpected output: {cleaned}"
        );
    }
}
