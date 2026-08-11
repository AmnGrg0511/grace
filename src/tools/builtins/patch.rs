//! `edit` — the literal find-and-replace edit primitive.
//!
//! Deliberately not a unified-diff applier: no fuzz, no context matching, no
//! hunk offsets. The model supplies an exact `old_string` and its replacement,
//! and the edit either matches or is refused. A fuzzy patcher that "helpfully"
//! applies a near-miss is how an agent silently corrupts a file it cannot see.

use super::file::check_path_allowed;
use crate::tools::r#trait::{arg_str, str_prop, Tool};
use crate::util::{AgentError, Result};
use serde_json::{json, Value};
use std::fs;

// ---- edit (find and replace) --------------------------------------------

/// Applies a find-and-replace edit to a file. This is the "edit" primitive:
/// no fuzz, no context beyond a literal old block search.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
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
        let allowed = check_path_allowed(&path)?;
        let original = fs::read_to_string(&allowed)
            .map_err(|e| AgentError::Tool(format!("read {}: {e}", path)))?;
        match original.find(&old) {
            Some(idx) => {
                let replaced = format!(
                    "{}{}{}",
                    &original[..idx],
                    new,
                    &original[idx + old.len()..]
                );
                fs::write(&allowed, &replaced)
                    .map_err(|e| AgentError::Tool(format!("write {}: {e}", path)))?;
                let diff = crate::util::diff::unified_snippet(&old, &new, 3);
                Ok(format!("patched {path}\n{diff}"))
            }
            None => Err(AgentError::Tool(format!(
                "old_string not found in {} (exact, case-sensitive match required)",
                path
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_support::EnvVarGuard;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "grace_patch_test_{}_{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn replaces_the_first_occurrence_and_reports_a_diff() {
        let _g = EnvVarGuard::none();
        let dir = scratch("basic");
        let path = dir.join("f.txt");
        fs::write(&path, "alpha beta gamma").unwrap();
        let out = EditTool
            .run(&json!({
                "path": path.to_str().unwrap(),
                "old_string": "beta",
                "new_string": "delta"
            }))
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha delta gamma");
        assert!(out.contains("patched"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_first_occurrence_is_replaced() {
        let _g = EnvVarGuard::none();
        let dir = scratch("first_only");
        let path = dir.join("f.txt");
        fs::write(&path, "x x x").unwrap();
        EditTool
            .run(&json!({
                "path": path.to_str().unwrap(),
                "old_string": "x",
                "new_string": "y"
            }))
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "y x x");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_matching_old_string_refuses_rather_than_guessing() {
        let _g = EnvVarGuard::none();
        // The whole point of a literal patcher: a near-miss must fail loudly.
        // A fuzzy apply here silently corrupts a file the model cannot see.
        let dir = scratch("nomatch");
        let path = dir.join("f.txt");
        fs::write(&path, "hello world").unwrap();
        let err = EditTool
            .run(&json!({
                "path": path.to_str().unwrap(),
                "old_string": "Hello World",
                "new_string": "x"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("old_string not found"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hello world",
            "file must be untouched on a failed match"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_is_case_sensitive() {
        let _g = EnvVarGuard::none();
        let dir = scratch("case");
        let path = dir.join("f.txt");
        fs::write(&path, "Foo").unwrap();
        assert!(EditTool
            .run(&json!({"path": path.to_str().unwrap(), "old_string": "foo", "new_string": "bar"}))
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiline_replacement_works() {
        let _g = EnvVarGuard::none();
        let dir = scratch("multiline");
        let path = dir.join("f.rs");
        fs::write(&path, "fn a() {\n    old();\n}\n").unwrap();
        EditTool
            .run(&json!({
                "path": path.to_str().unwrap(),
                "old_string": "    old();\n",
                "new_string": "    new();\n    extra();\n"
            }))
            .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("new();") && after.contains("extra();"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn patching_a_missing_file_is_an_error() {
        let _g = EnvVarGuard::none();
        let err = EditTool
            .run(&json!({
                "path": "/nonexistent/nope.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .unwrap_err();
        assert!(err.to_string().contains("read"));
    }

    #[test]
    fn missing_arguments_are_reported_by_name() {
        let _g = EnvVarGuard::none();
        let err = EditTool.run(&json!({"path": "/tmp/x"})).unwrap_err();
        assert!(err.to_string().contains("old_string"));
    }
}
