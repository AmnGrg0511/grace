//! Built-in tools: terminal, file read/write, and patch.
//!
//! These are intentionally thin wrappers over `std` I/O. Each tool:
//!   1. declares its name/description/parameters,
//!   2. pulls typed fields out of the JSON args,
//!   3. performs the side effect,
//!   4. returns a short string result (fed back to the model).
//!
//! Safety note: a real deployment must guard `run_terminal` (command
//! allow-list / sandbox) and `write_file`/`patch` (path allow-list). We keep
//! the minimal core unguarded but document the gap in the README.

use crate::error::{AgentError, Result};
use crate::tool::Tool;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

// ---- helpers ---------------------------------------------------------------

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| AgentError::Tool(format!("missing string argument '{key}'")))
}

fn str_prop(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}

// ---- run_terminal ----------------------------------------------------------

/// Executes a shell command and returns its stdout (or stderr + exit code).
///
/// Optional guardrails, opt-in via environment variables (default: no
/// restrictions, matching the pre-hardening behavior):
///   - `GRACE_TERMINAL_DENY`: comma-separated substrings; a command containing
///     any of them is refused before spawning.
///   - `GRACE_TERMINAL_ALLOW_DIR`: if set, commands run with this directory as
///     their cwd (a simple jail, not a full sandbox).
pub struct TerminalTool;

impl TerminalTool {
    fn deny_list() -> Vec<String> {
        std::env::var("GRACE_TERMINAL_DENY")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn allow_dir() -> Option<String> {
        std::env::var("GRACE_TERMINAL_ALLOW_DIR").ok().filter(|s| !s.is_empty())
    }
}

impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "run_terminal"
    }

    fn description(&self) -> &str {
        "Run a shell command and return its combined stdout/stderr and exit code."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": str_prop("The shell command to execute."),
            },
            "required": ["command"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let command = arg_str(args, "command")?;
        let deny = Self::deny_list();
        if let Some(hit) = deny.iter().find(|d| command.contains(d.as_str())) {
            return Err(AgentError::Tool(format!(
                "command refused: contains denied pattern '{hit}'"
            )));
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        if let Some(dir) = Self::allow_dir() {
            cmd.current_dir(dir);
        }
        let output = cmd
            .output()
            .map_err(|e| AgentError::Tool(format!("failed to spawn 'sh': {e}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(stdout.trim_end());
        }
        if !stderr.is_empty() {
            result.push_str(&format!("\n[stderr] {}", stderr.trim_end()));
        }
        result.push_str(&format!("\n[exit code {}]", output.status.code().unwrap_or(-1)));
        Ok(result)
    }
}

// ---- read_file -------------------------------------------------------------

/// Reads a UTF-8 file and returns its contents.
pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file and return its contents."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("Absolute or relative path to the file."),
            },
            "required": ["path"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let content = fs::read_to_string(&path).map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        Ok(content)
    }
}

// ---- write_file ------------------------------------------------------------

/// Writes UTF-8 content to a file (creating parent dirs, overwriting).
pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write text content to a file, creating parent directories as needed. Overwrites."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("Path to write."),
                "content": str_prop("Text to write."),
            },
            "required": ["path", "content"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| AgentError::Tool(format!("create dirs for {}: {e}", path)))?;
            }
        }
        let nbytes = content.len();
        fs::write(&path, &content).map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
        Ok(format!("wrote {nbytes} bytes to {}", path))
    }
}

// ---- patch (unified diff apply) --------------------------------------------

/// Applies a small unified diff to a file. This is the "edit" primitive: we
/// implement a minimal `patch` (no fuzz, no context beyond a literal old block
/// search) so the core can modify files without shelling out to GNU patch.
pub struct PatchTool;

impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Replace the first occurrence of `old_string` with `new_string` in a file (case-sensitive, literal)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("File to edit."),
                "old_string": str_prop("Exact text to find and replace."),
                "new_string": str_prop("Replacement text."),
            },
            "required": ["path", "old_string", "new_string"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let old = arg_str(args, "old_string")?;
        let new = arg_str(args, "new_string")?;
        let original = fs::read_to_string(&path).map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        match original.find(&old) {
            Some(idx) => {
                let replaced = format!("{}{}{}", &original[..idx], new, &original[idx + old.len()..]);
                fs::write(&path, &replaced).map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
                Ok(format!(
                    "patched {} (replaced {}-byte block with {}-byte block)",
                    path,
                    old.len(),
                    new.len()
                ))
            }
            None => Err(AgentError::Tool(format!(
                "old_string not found in {} (exact, case-sensitive match required)",
                path
            ))),
        }
    }
}

/// Register the default built-in tool set into a registry.
pub fn register_builtins(registry: &mut crate::tool::ToolRegistry) {
    registry.register(Box::new(TerminalTool));
    registry.register(Box::new(ReadFileTool));
    registry.register(Box::new(WriteFileTool));
    registry.register(Box::new(PatchTool));
}

#[cfg(test)]
mod tools_hardening_tests {
    use super::*;

    #[test]
    fn terminal_deny_list_rejects_matching_command() {
        std::env::set_var("GRACE_TERMINAL_DENY", "rm -rf,shutdown");
        let tool = TerminalTool;
        let err = tool.run(&json!({"command": "rm -rf /"})).unwrap_err();
        assert!(err.to_string().contains("denied"));
        std::env::remove_var("GRACE_TERMINAL_DENY");
    }

    #[test]
    fn terminal_allow_dir_jails_cwd() {
        let dir = std::env::temp_dir().join(format!("grace_terminal_jail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GRACE_TERMINAL_ALLOW_DIR", dir.to_str().unwrap());
        let tool = TerminalTool;
        let out = tool.run(&json!({"command": "pwd"})).unwrap();
        // Canonicalize both sides: /tmp is often a symlink (e.g. to /private/tmp).
        let canon_dir = std::fs::canonicalize(&dir).unwrap();
        assert!(out.contains(canon_dir.to_str().unwrap()) || out.contains(dir.to_str().unwrap()));
        std::env::remove_var("GRACE_TERMINAL_ALLOW_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
