use grace::tool::ToolRegistry;
use serde_json::json;

fn reg() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    grace::tools::register_builtins(&mut r);
    r
}

fn obj(pairs: &[(&str, &str)]) -> String {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), json!(v));
    }
    serde_json::Value::Object(m).to_string()
}

#[test]
fn terminal_runs_real_command() {
    let r = reg();
    let out = r.execute("run_terminal", &obj(&[("command", "echo side_effect_ok")])).unwrap();
    assert!(out.contains("side_effect_ok"));
    assert!(out.contains("exit code 0"));
}

#[test]
fn write_read_patch_roundtrip() {
    let r = reg();
    let path = "/tmp/hc_itest.txt";
    r.execute("write_file", &obj(&[("path", path), ("content", "hello world")])).unwrap();
    let read = r.execute("read_file", &obj(&[("path", path)])).unwrap();
    assert_eq!(read, "hello world");
    r.execute("patch", &obj(&[("path", path), ("old_string", "world"), ("new_string", "rust")])).unwrap();
    let read2 = r.execute("read_file", &obj(&[("path", path)])).unwrap();
    assert_eq!(read2, "hello rust");
}

#[test]
fn unknown_tool_errors_cleanly() {
    let r = ToolRegistry::new();
    assert!(r.execute("nope", "{}").is_err());
}
