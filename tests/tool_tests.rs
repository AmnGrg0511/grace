//! Integration tests for the tool system: the registry contract, the built-in
//! tools, and how tool failures surface back to the model.
//!
//! Env-var-dependent guardrails (`GRACE_ALLOW_DIR`, `GRACE_TERMINAL_*`) are
//! deliberately *not* retested here — those mutate process-global state and
//! are covered by the serialized unit tests inside the crate, where a
//! crate-wide lock keeps them from racing.

use grace::tools::{register_builtins, Tool, ToolRegistry};
use grace::util::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grace_tool_it_{}_{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_builtins(&mut reg);
    reg
}

// ---- the registry contract --------------------------------------------------

#[test]
fn the_builtin_set_is_exactly_what_is_documented() {
    assert_eq!(
        registry().names(),
        vec![
            "bash",
            "edit",
            "read",
            "write"
        ]
    );
}

#[test]
fn every_tool_spec_is_valid_for_an_openai_compatible_provider() {
    // A malformed schema surfaces as an opaque provider 400 that names no
    // tool, so it is worth catching structurally here.
    for spec in registry().specs() {
        assert!(!spec.name.is_empty());
        assert!(
            !spec.description.is_empty(),
            "{} has no description — the model has nothing to decide on",
            spec.name
        );
        assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        assert!(
            spec.parameters.get("properties").is_some(),
            "{} declares no properties object",
            spec.name
        );
    }
}

#[test]
fn a_custom_tool_can_be_registered_and_invoked_through_the_registry() {
    struct Doubler;
    impl Tool for Doubler {
        fn name(&self) -> &str {
            "double"
        }
        fn description(&self) -> &str {
            "doubles a number"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"n": {"type": "integer"}}})
        }
        fn run(&self, args: &Value) -> Result<String> {
            let n = args.get("n").and_then(Value::as_i64).unwrap_or(0);
            Ok((n * 2).to_string())
        }
    }
    let mut reg = registry();
    reg.register(Box::new(Doubler));
    assert_eq!(reg.execute("double", r#"{"n":21}"#).unwrap(), "42");
}

#[test]
fn a_custom_tool_can_deliberately_override_a_builtin() {
    struct SafeTerminal;
    impl Tool for SafeTerminal {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "refuses everything"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn run(&self, _args: &Value) -> Result<String> {
            Ok("refused".into())
        }
    }
    let mut reg = registry();
    let before = reg.len();
    reg.register(Box::new(SafeTerminal));
    assert_eq!(reg.len(), before, "override, not addition");
    assert_eq!(reg.execute("bash", "{}").unwrap(), "refused");
}

#[test]
fn calling_an_unregistered_tool_names_it_in_the_error() {
    let err = registry().execute("teleport", "{}").unwrap_err();
    assert!(err.to_string().contains("teleport"));
}

#[test]
fn malformed_argument_json_is_rejected_with_a_readable_message() {
    let err = registry().execute("read", "{not json}").unwrap_err();
    assert!(err.to_string().contains("bad arguments json"));
}

// ---- file tools -------------------------------------------------------------

#[test]
fn write_read_and_patch_compose_into_an_edit_workflow() {
    // The realistic sequence an agent actually performs.
    let dir = scratch("workflow");
    let path = dir.join("code.rs");
    let p = path.to_str().unwrap();
    let reg = registry();

    reg.execute(
        "write",
        &json!({"path": p, "content": "fn main() {\n    old();\n}\n"}).to_string(),
    )
    .unwrap();

    let read = reg
        .execute("read", &json!({"path": p}).to_string())
        .unwrap();
    assert!(read.contains("old();"));

    reg.execute(
        "edit",
        &json!({"path": p, "old_string": "old()", "new_string": "new()"}).to_string(),
    )
    .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("new();") && !after.contains("old();"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_patch_whose_anchor_is_absent_leaves_the_file_untouched() {
    // A fuzzy apply here would silently corrupt a file the model cannot see.
    let dir = scratch("nomatch");
    let path = dir.join("f.txt");
    std::fs::write(&path, "original content").unwrap();
    let err = registry()
        .execute(
            "edit",
            &json!({
                "path": path.to_str().unwrap(),
                "old_string": "not present",
                "new_string": "x"
            })
            .to_string(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("old_string not found"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original content");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reading_a_very_large_file_returns_a_bounded_summary() {
    // An unbounded read can consume the whole context window in one result.
    let dir = scratch("big");
    let path = dir.join("big.log");
    let content: String = (0..5_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&path, &content).unwrap();

    let out = registry()
        .execute(
            "read",
            &json!({"path": path.to_str().unwrap()}).to_string(),
        )
        .unwrap();

    assert!(out.contains("5000 lines"));
    assert!(out.contains("lines omitted"));
    assert!(
        out.len() < content.len() / 4,
        "summary must be far smaller than the file: {} vs {}",
        out.len(),
        content.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_file_creates_intermediate_directories() {
    let dir = scratch("mkdirs");
    let path = dir.join("a/b/c/file.txt");
    registry()
        .execute(
            "write",
            &json!({"path": path.to_str().unwrap(), "content": "hi"}).to_string(),
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- terminal ---------------------------------------------------------------

#[test]
fn run_terminal_returns_stdout_and_an_exit_code() {
    let out = registry()
        .execute("bash", r#"{"command":"echo integration-marker"}"#)
        .unwrap();
    assert!(out.contains("integration-marker"));
    assert!(out.contains("[exit code 0]"));
}

#[test]
fn a_failing_command_reports_its_nonzero_exit_code_rather_than_erroring() {
    // A non-zero exit is information for the model, not a tool failure.
    let out = registry()
        .execute("bash", r#"{"command":"exit 3"}"#)
        .unwrap();
    assert!(out.contains("[exit code 3]"), "got: {out}");
}

#[test]
fn stderr_is_captured_alongside_stdout() {
    let out = registry()
        .execute(
            "bash",
            r#"{"command":"echo out; echo err 1>&2"}"#,
        )
        .unwrap();
    assert!(out.contains("out"));
    assert!(out.contains("err"));
}

#[test]
fn a_background_job_returns_immediately_and_can_be_waited_on() {
    let reg = registry();
    let started = std::time::Instant::now();
    let dispatch = reg
        .execute(
            "bash",
            r#"{"command":"sleep 1; echo finished-marker","background":true}"#,
        )
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "background dispatch must not block"
    );

    let job_id = dispatch
        .split('\'')
        .nth(1)
        .expect("a quoted job id in the dispatch message");

    let done = reg
        .execute(
            "bash",
            &json!({"job_id": job_id, "wait": true}).to_string(),
        )
        .unwrap();
    assert!(done.contains("finished-marker"), "got: {done}");
    assert!(done.contains("[status: exited, code 0]"), "got: {done}");
}

#[test]
fn polling_an_unknown_background_job_is_an_error() {
    let err = registry()
        .execute("bash", r#"{"job_id":"bg-nope"}"#)
        .unwrap_err();
    assert!(err.to_string().contains("no background job"));
}

// ---- skill tools ------------------------------------------------------------

#[test]
fn skill_tools_discover_and_load_from_a_directory() {
    let dir = scratch("skills");
    std::fs::create_dir_all(dir.join("greet")).unwrap();
    std::fs::write(
        dir.join("greet").join("SKILL.md"),
        "---\ndescription: Says hello.\n---\n# Greet\nSay hello warmly.",
    )
    .unwrap();

    let reg = grace::config::Config::build_registry_with_skills(&dir);
    assert_eq!(reg.execute("list_skills", "{}").unwrap(), "greet");
    let loaded = reg
        .execute("load_skill", r#"{"name":"greet"}"#)
        .unwrap();
    assert!(loaded.contains("Say hello warmly."));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_skill_refuses_to_escape_the_skills_root() {
    // Otherwise a discovery tool becomes an arbitrary file read.
    let dir = scratch("traversal");
    let reg = grace::config::Config::build_registry_with_skills(&dir);
    assert!(reg
        .execute("load_skill", r#"{"name":"../../../etc/passwd"}"#)
        .is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
