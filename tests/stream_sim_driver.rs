//! Capture/replay driver for the append-only stream path.
//!
//! Feeds a REAL captured fragment sequence (thinking model: content led by
//! "\n\n", 2923 fragments, mixed paragraphs/headings/lists/fences/tables)
//! through the renderer and the live `print_agent_event_to` stream path at
//! several terminal sizes, and dumps the exact bytes emitted.  The dumps are
//! replayable through any VT screen model to prove zero loss / zero
//! duplication.
//!
//! Self-contained: the fixture is committed under `tests/fixtures/`, and the
//! output goes to the OS temp dir — no machine-local paths.  The tests also
//! assert the streamed text ends with the full answer, because a silent
//! early stop is exactly the failure class this suite exists to catch.
//!
//! All tests mutate process-wide env vars (COLUMNS/LINES/CLICOLOR_FORCE),
//! so they run under one process-local lock regardless of
//! `--test-threads`.  (The crate-wide `util::test_support` lock is
//! `#[cfg(test)]` and not visible to integration tests.)

use std::io::Write;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the env lock, recovering from a poisoned mutex: one panicking
/// test must not cascade into every other test in this binary.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/real_frags.json");

fn load_frags() -> Vec<String> {
    let data = std::fs::read_to_string(FIXTURE)
        .expect("committed fixture tests/fixtures/real_frags.json must exist");
    let v: serde_json::Value = serde_json::from_str(&data).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn temp_out(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("grace_stream_sim_{name}"))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip an escape parameter sequence ending in a final byte.
            for f in chars.by_ref() {
                if f == 'm' || f == 'B' || f == 'b' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn run_sequence(cols: &str, lines: &str, out_name: &str) {
    let frags = load_frags();
    let _guard = env_guard();
    std::env::set_var("CLICOLOR_FORCE", "1");
    std::env::set_var("COLUMNS", cols);
    std::env::set_var("LINES", lines);

    let skin = grace::ui::skin::SOLARIS;
    let mut buf = String::new();
    let mut out = std::fs::File::create(temp_out(out_name)).unwrap();
    for f in &frags {
        buf.push_str(f);
        let rendered = grace::ui::markdown::render_terminal_colored(&buf, &skin, true);
        let rows = grace::ui::chat::test_support_visual_rows(&rendered, cols.parse().ok());
        writeln!(out, "__RENDER__{rows}").unwrap();
        write!(out, "{}", rendered.replace('\x1b', "<ESC>")).unwrap();
    }
}

#[test]
fn dump_progressive_renders_20() {
    run_sequence("20", "10", "prog_20x10.txt");
}

#[test]
fn dump_progressive_renders_40() {
    run_sequence("40", "10", "prog_40x10.txt");
}

fn emit_stream(lines: &str, cols: &str, out_name: &str) {
    let frags = load_frags();
    let _guard = env_guard();
    std::env::set_var("CLICOLOR_FORCE", "1");
    std::env::set_var("COLUMNS", cols);
    std::env::set_var("LINES", lines);

    let skin = grace::ui::skin::SOLARIS;
    let mut stream = grace::ui::chat::StreamState::default();
    let mut out = Vec::new();
    for f in &frags {
        let mut one = Vec::new();
        grace::ui::chat::print_agent_event_to(
            grace::core::lifecycle::AgentEvent::ContentFragment(f),
            &skin,
            false,
            &mut stream,
            &mut one,
        );
        out.extend_from_slice(&one);
    }
    // End-of-turn flush of the in-progress tail.
    let mut fin = Vec::new();
    grace::ui::chat::finalize_stream(&mut stream, &skin, false, &mut fin).unwrap();
    out.extend_from_slice(&fin);
    std::fs::write(temp_out(out_name), &out).unwrap();

    // The streamed bytes must contain the whole answer — a silent early
    // stop (the historical total-loss bug) fails here at every width.
    let doc: String = frags.concat();
    let tail = "early chapters of its longest transformation";
    let plain = strip_ansi(&String::from_utf8(out).unwrap());
    assert!(
        plain.contains(tail),
        "streamed output must end with the full answer (doc len {})",
        doc.chars().count()
    );
}

#[test]
fn capture_emitted_stream_40() {
    emit_stream("10", "40", "emitted_40.bin");
}

#[test]
fn capture_emitted_stream_20() {
    emit_stream("10", "20", "emitted_20.bin");
}

#[test]
fn capture_emitted_stream_120() {
    emit_stream("30", "120", "emitted_120.bin");
}
