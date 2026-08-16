use serde_json::Value;
use std::io::Write;

fn run_sequence(cols: &str, lines: &str, out_file: &str) {
    std::env::set_var("CLICOLOR_FORCE", "1");
    std::env::set_var("COLUMNS", cols);
    std::env::set_var("LINES", lines);
    let frags: Vec<String> = {
        let data = std::fs::read_to_string("/tmp/opencode/real_frags.json").unwrap();
        let v: Value = serde_json::from_str(&data).unwrap();
        v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
    };
    let skin = grace::ui::skin::SOLARIS;
    let mut buf = String::new();
    let mut out = std::fs::File::create(out_file).unwrap();
    for f in &frags {
        buf.push_str(f);
        let rendered = grace::ui::markdown::render_terminal_colored(&buf, &skin, true);
        let rows = grace::ui::chat::test_support_visual_rows(&rendered, cols.parse().ok());
        writeln!(out, "__RENDER__{}", rows).unwrap();
        write!(out, "{}", rendered.replace('\x1b', "<ESC>")).unwrap();
    }
}

#[test]
fn dump_progressive_renders_20() {
    run_sequence("20", "10", "/tmp/opencode/prog_20x10.txt");
}

#[test]
fn dump_progressive_renders_40() {
    run_sequence("40", "10", "/tmp/opencode/prog_40x10.txt");
}

fn emit_stream(lines: &str, cols: &str, out_file: &str) {
    std::env::set_var("CLICOLOR_FORCE", "1");
    std::env::set_var("COLUMNS", cols);
    std::env::set_var("LINES", lines);
    let frags: Vec<String> = {
        let data = std::fs::read_to_string("/tmp/opencode/real_frags.json").unwrap();
        let v: Value = serde_json::from_str(&data).unwrap();
        v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
    };
    let skin = grace::ui::skin::SOLARIS;
    let mut stream = grace::ui::chat::StreamState::default();
    let mut out = Vec::new();
    for f in &frags {
        let mut buf = Vec::new();
        grace::ui::chat::print_agent_event_to(
            grace::core::lifecycle::AgentEvent::ContentFragment(f),
            &skin,
            false,
            &mut stream,
            &mut buf,
        );
        out.extend_from_slice(&buf);
    }
    std::fs::write(out_file, &out).unwrap();
}

#[test]
fn capture_emitted_stream_40() {
    emit_stream("10", "40", "/tmp/opencode/emitted_40.bin");
}

#[test]
fn capture_emitted_stream_20() {
    emit_stream("10", "20", "/tmp/opencode/emitted_20.bin");
}

#[test]
fn capture_emitted_stream_120() {
    emit_stream("30", "120", "/tmp/opencode/emitted_120.bin");
}

