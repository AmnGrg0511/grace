//! `read` and `write`.
//!
//! Both go through [`check_path_allowed`], which enforces the optional
//! `GRACE_ALLOW_DIR` jail. Reads of large files are summarized (head+tail with
//! an omission count) rather than returned whole: a single 10k-line file
//! dumped verbatim into a tool result can consume the entire context window
//! and kill the session outright.

use crate::tools::r#trait::{arg_str, str_prop, Tool};
use crate::util::{AgentError, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Resolve `path` to an absolute path, refusing anything outside
/// `GRACE_ALLOW_DIR` when that jail is configured. Unset = all paths allowed
/// (backward-compatible default).
pub fn check_path_allowed(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| AgentError::Tool(format!("cwd error: {e}")))?
            .join(path)
    };

    if let Ok(allow_dir) = std::env::var("GRACE_ALLOW_DIR") {
        let allow_root = Path::new(&allow_dir)
            .canonicalize()
            .map_err(|e| AgentError::Tool(format!("invalid GRACE_ALLOW_DIR: {e}")))?;
        // Canonicalize the *existing* portion of the target so a not-yet-created
        // write target is still jailed correctly (canonicalize fails on a
        // missing leaf, which previously made every new-file write error out).
        let canonical = canonicalize_lexically(&absolute)?;
        if !canonical.starts_with(&allow_root) {
            return Err(AgentError::Tool(format!(
                "path '{}' outside allowed directory '{}'",
                canonical.display(),
                allow_root.display()
            )));
        }
    }
    Ok(absolute)
}

/// Canonicalize as much of `path` as exists, then re-append the missing tail.
/// Resolves `..` and symlinks in the existing prefix — so a jail cannot be
/// escaped with `allowed/../../etc/passwd` — without requiring the full path
/// to exist yet.
fn canonicalize_lexically(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        let Some(parent) = existing.parent() else {
            return Err(AgentError::Tool(format!(
                "path resolve error: no existing ancestor for '{}'",
                path.display()
            )));
        };
        if let Some(name) = existing.file_name() {
            tail.push(name);
        }
        existing = parent;
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| AgentError::Tool(format!("path resolve error: {e}")))?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

// ---- read ------------------------------------------------------------------

/// Reads a UTF-8 file and returns its contents.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file and return its contents with line count. For large files (>500 lines), falls back to grep search."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": str_prop("Absolute or relative path to the file."),
                "offset": {"type": "integer", "description": "Starting line number (1-indexed, default 1)."},
                "limit": {"type": "integer", "description": "Max lines to read (default 500, max 2000)."},
            },
            "required": ["path"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let path = arg_str(args, "path")?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
        
        let allowed = check_path_allowed(&path)?;
        
        // Get total line count first
        let content = fs::read_to_string(&allowed)
            .map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        
        let total_lines = content.lines().count();
        
        // If file is large (>500 lines), only show summary + head/tail
        if total_lines > 500 {
            let head: Vec<&str> = content.lines().take(50).collect();
            let tail: Vec<&str> = content.lines().skip(total_lines.saturating_sub(50)).collect();
            
            let mut result = format!("File: {} ({} lines)\n", path, total_lines);
            result.push_str("=== FIRST 50 LINES ===\n");
            result.push_str(&head.join("\n"));
            result.push_str("\n=== LAST 50 LINES ===\n");
            result.push_str(&tail.join("\n"));
            result.push_str(&format!("\n... [{} lines omitted] ...", total_lines - head.len() - tail.len()));
            return Ok(result);
        }
        
        // For smaller files, apply offset/limit
        let lines: Vec<&str> = content.lines().skip(offset.saturating_sub(1)).take(limit).collect();
        let shown = lines.len();
        let mut result = format!("File: {} ({} lines total, showing {} lines from {})\n", path, total_lines, shown, offset);
        result.push_str(&lines.join("\n"));
        Ok(result)
    }
}

// ---- write ------------------------------------------------------------------

/// Writes UTF-8 content to a file (creating parent dirs, overwriting).
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
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
        let allowed = check_path_allowed(&path)?;
        if let Some(parent) = allowed.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| AgentError::Tool(format!("create dirs for {}: {e}", path)))?;
            }
        }
        let nbytes = content.len();
        fs::write(&allowed, &content).map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
        // Truncate content display for large files
        let display_content = if content.len() > 200 {
            format!("{}... [truncated {} bytes]", &content[..200], content.len() - 200)
        } else {
            content.to_string()
        };
        Ok(format!("wrote {} bytes to {} (content: {})", nbytes, path, display_content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_support::EnvVarGuard;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "grace_file_test_{}_{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_then_read_roundtrip() {
        let _g = EnvVarGuard::none();
        let dir = scratch("roundtrip");
        let path = dir.join("a.txt");
        let p = path.to_str().unwrap();

        let wrote = WriteTool
            .run(&json!({"path": p, "content": "hello world"}))
            .unwrap();
        assert!(wrote.contains("wrote 11 bytes"));

        let read = ReadTool.run(&json!({"path": p})).unwrap();
        assert!(read.contains("hello world"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_creates_missing_parent_directories() {
        let _g = EnvVarGuard::none();
        let dir = scratch("mkparents");
        let path = dir.join("deep/nested/b.txt");
        WriteTool
            .run(&json!({"path": path.to_str().unwrap(), "content": "x"}))
            .unwrap();
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_a_missing_file_is_a_tool_error_not_a_panic() {
        let _g = EnvVarGuard::none();
        let err = ReadTool
            .run(&json!({"path": "/nonexistent/definitely/not/here.txt"}))
            .unwrap_err();
        assert!(err.to_string().contains("read"));
    }

    #[test]
    fn large_files_are_summarized_rather_than_dumped_whole() {
        let _g = EnvVarGuard::none();
        // A 10k-line file returned verbatim can consume the entire context
        // window in one tool result — the exact failure this head/tail
        // summary exists to prevent.
        let dir = scratch("large");
        let path = dir.join("big.txt");
        let content: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        fs::write(&path, &content).unwrap();

        let out = ReadTool
            .run(&json!({"path": path.to_str().unwrap()}))
            .unwrap();
        assert!(out.contains("1000 lines"));
        assert!(out.contains("FIRST 50 LINES"));
        assert!(out.contains("LAST 50 LINES"));
        assert!(out.contains("lines omitted"));
        assert!(
            out.len() < content.len(),
            "summary must be smaller than the file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn offset_and_limit_window_a_small_file() {
        let _g = EnvVarGuard::none();
        let dir = scratch("window");
        let path = dir.join("small.txt");
        fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let out = ReadTool
            .run(&json!({"path": path.to_str().unwrap(), "offset": 2, "limit": 2}))
            .unwrap();
        assert!(out.contains("showing 2 lines from 2"));
        let body: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(body, vec!["b", "c"], "only the requested window");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allow_dir_jail_permits_paths_inside_it() {
        let dir = scratch("jail_ok");
        let _g = EnvVarGuard::set("GRACE_ALLOW_DIR", dir.to_str().unwrap());
        let inside = dir.join("ok.txt");
        let res = WriteTool.run(&json!({"path": inside.to_str().unwrap(), "content": "y"}));
        assert!(res.is_ok(), "in-jail write must succeed: {res:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allow_dir_jail_refuses_paths_outside_it() {
        let dir = scratch("jail_deny");
        let _g = EnvVarGuard::set("GRACE_ALLOW_DIR", dir.to_str().unwrap());
        let err = ReadTool.run(&json!({"path": "/etc/hostname"})).unwrap_err();
        assert!(
            err.to_string().contains("outside allowed directory"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allow_dir_jail_cannot_be_escaped_with_dotdot() {
        // `allowed/../../etc/passwd` must be resolved before the prefix
        // check, or the jail is decorative.
        let dir = scratch("jail_dotdot");
        let _g = EnvVarGuard::set("GRACE_ALLOW_DIR", dir.to_str().unwrap());
        let escape = dir.join("../../../etc/hostname");
        let res = ReadTool.run(&json!({"path": escape.to_str().unwrap()}));
        assert!(res.is_err(), "dotdot escape must be refused");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_file_inside_the_jail_is_allowed_even_though_it_does_not_exist_yet() {
        // Regression: plain `canonicalize()` fails on a missing leaf, which
        // made every first-time write inside the jail error out.
        let dir = scratch("jail_newfile");
        let _g = EnvVarGuard::set("GRACE_ALLOW_DIR", dir.to_str().unwrap());
        let fresh = dir.join("never/created/before.txt");
        let res = WriteTool.run(&json!({"path": fresh.to_str().unwrap(), "content": "z"}));
        assert!(res.is_ok(), "new file in jail must be writable: {res:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_argument_is_reported() {
        let _g = EnvVarGuard::none();
        let err = ReadTool.run(&json!({})).unwrap_err();
        assert!(err.to_string().contains("missing string argument 'path'"));
    }

    #[test]
    fn write_truncates_the_echoed_content_for_large_writes() {
        let _g = EnvVarGuard::none();
        let dir = scratch("echo_trunc");
        let path = dir.join("big.txt");
        let big = "q".repeat(500);
        let out = WriteTool
            .run(&json!({"path": path.to_str().unwrap(), "content": big}))
            .unwrap();
        assert!(out.contains("truncated 300 bytes"));
        let _ = fs::remove_dir_all(&dir);
    }
}
