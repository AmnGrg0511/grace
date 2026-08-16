//! Opt-in live end-to-end tests against a real OpenAI-compatible endpoint.
//!
//! These are NOT part of the default `cargo test` run — every test here is
//! `#[ignore]`d so CI and other contributors' machines stay hermetic and
//! network-free by default. Run them explicitly when you have a live
//! endpoint to burn against (self-hosted vLLM/Ollama/LM Studio, or any
//! OpenAI-compatible API — no cost concern on self-hosted infra):
//!
//! ```bash
//! export GRACE_LIVE_BASE_URL="https://your-endpoint/v1"
//! export GRACE_LIVE_API_KEY="..."
//! export GRACE_LIVE_MODEL="your-model-id"
//! cargo test --release --test live_endpoint -- --ignored --test-threads=1
//! ```
//!
//! Credentials are read ONLY from environment variables at run time — never
//! hardcoded, logged, or written to disk by these tests. No endpoint URL,
//! key, or vendor name belongs in this file or anywhere else in the repo.

use std::path::Path;
use std::process::Command;

/// Resolves the 3 required env vars, or returns `None` (test then skips
/// itself with a clear message instead of failing — there's no live
/// endpoint configured in a normal `cargo test` run).
fn live_config() -> Option<(String, String, String)> {
    let base_url = std::env::var("GRACE_LIVE_BASE_URL").ok()?;
    let api_key = std::env::var("GRACE_LIVE_API_KEY").ok()?;
    let model = std::env::var("GRACE_LIVE_MODEL").ok()?;
    Some((base_url, api_key, model))
}

fn grace_bin() -> Option<std::path::PathBuf> {
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let candidates = [
        Path::new(&target_dir).join("release/grace"),
        Path::new(&target_dir).join("debug/grace"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

/// Runs `grace --prompt <prompt> ...extra_args` against the live endpoint
/// and returns stdout. Panics with the full output if the exit code is
/// non-zero, so a broken tool-call round-trip (e.g. the `type`-field
/// regression) shows up as a hard test failure, not a silent pass.
fn run_grace(prompt: &str, workdir: &Path, extra_args: &[&str]) -> String {
    let (base_url, api_key, model) = live_config().expect("live_config checked by caller");
    let bin = grace_bin().expect("build the release binary first: cargo build --release");

    let mut cmd = Command::new(bin);
    cmd.current_dir(workdir)
        .arg("--base-url")
        .arg(&base_url)
        .arg("--api-key")
        .arg(&api_key)
        .arg("--model")
        .arg(&model)
        .arg("--prompt")
        .arg(prompt)
        .arg("--max-iterations")
        .arg("5")
        .args(extra_args);

    let output = cmd.output().expect("failed to spawn grace binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "grace exited non-zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

#[test]
#[ignore = "requires GRACE_LIVE_BASE_URL/GRACE_LIVE_API_KEY/GRACE_LIVE_MODEL"]
fn read_file_tool_round_trips_against_live_endpoint() {
    let Some(_) = live_config() else {
        eprintln!("skipped: GRACE_LIVE_* env vars not set");
        return;
    };
    let dir = tempdir();
    std::fs::write(dir.join("probe.txt"), "grace-live-test-marker-9f3a").unwrap();

    let stdout = run_grace(
        "Read probe.txt and quote its exact contents.",
        &dir,
        &[],
    );
    assert!(
        stdout.contains("grace-live-test-marker-9f3a"),
        "expected file contents echoed back, got:\n{stdout}"
    );
}

#[test]
#[ignore = "requires GRACE_LIVE_BASE_URL/GRACE_LIVE_API_KEY/GRACE_LIVE_MODEL"]
fn write_then_read_file_round_trips_against_live_endpoint() {
    let Some(_) = live_config() else {
        eprintln!("skipped: GRACE_LIVE_* env vars not set");
        return;
    };
    let dir = tempdir();

    let stdout = run_grace(
        "Write a file called probe_out.txt containing exactly: grace-live-write-test. Then read it back to confirm.",
        &dir,
        &["--max-iterations", "6"],
    );
    let on_disk = std::fs::read_to_string(dir.join("probe_out.txt"))
        .expect("grace should have created probe_out.txt");
    assert!(on_disk.contains("grace-live-write-test"));
    assert!(stdout.to_lowercase().contains("probe_out.txt") || stdout.contains("grace-live-write-test"));
}

#[test]
#[ignore = "requires GRACE_LIVE_BASE_URL/GRACE_LIVE_API_KEY/GRACE_LIVE_MODEL"]
fn terminal_tool_executes_against_live_endpoint() {
    let Some(_) = live_config() else {
        eprintln!("skipped: GRACE_LIVE_* env vars not set");
        return;
    };
    let dir = tempdir();
    let stdout = run_grace(
        "Run the shell command 'echo grace-live-terminal-9f3a' via the terminal tool and report its exact stdout.",
        &dir,
        &[],
    );
    assert!(
        stdout.contains("grace-live-terminal-9f3a"),
        "expected terminal echo in answer, got:\n{stdout}"
    );
}

#[test]
#[ignore = "requires GRACE_LIVE_BASE_URL/GRACE_LIVE_API_KEY/GRACE_LIVE_MODEL"]
fn session_persists_across_two_process_invocations_against_live_endpoint() {
    let Some(_) = live_config() else {
        eprintln!("skipped: GRACE_LIVE_* env vars not set");
        return;
    };
    let dir = tempdir();
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (base_url, api_key, model) = live_config().unwrap();
    let bin = grace_bin().expect("build the release binary first: cargo build --release");
    let session_id = format!("live-e2e-{}", uuid::Uuid::new_v4());

    let run = |prompt: &str| -> String {
        let output = Command::new(&bin)
            .current_dir(dir.as_path())
            .env("HOME", &home)
            .args([
                "--base-url", &base_url,
                "--api-key", &api_key,
                "--model", &model,
                "--session", &session_id,
                "--prompt", prompt,
                "--max-iterations", "5",
            ])
            .output()
            .expect("failed to spawn grace binary");
        assert!(output.status.success(), "grace exited non-zero: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).to_string()
    };

    run("My favorite color is magenta-9f3a. Just acknowledge briefly.");
    let second = run("What is my favorite color? One word only.");
    assert!(
        second.to_lowercase().contains("magenta"),
        "session did not persist across process restarts, got:\n{second}"
    );
}

#[test]
#[ignore = "requires GRACE_LIVE_BASE_URL/GRACE_LIVE_API_KEY/GRACE_LIVE_MODEL"]
fn two_different_sessions_never_leak_into_each_other_against_live_endpoint() {
    // Regression for the "multiple terminals share state" class of bug:
    // two DIFFERENT --session ids, hit concurrently, must never see each
    // other's facts. (The other half of that bug — two terminals that
    // *don't* pass --session colliding on an implicit "default" — is
    // covered by main.rs's default_session_id unit test, since it depends
    // on tty identity that Command::output() can't fake realistically.)
    let Some(_) = live_config() else {
        eprintln!("skipped: GRACE_LIVE_* env vars not set");
        return;
    };
    let dir = tempdir();
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (base_url, api_key, model) = live_config().unwrap();
    let bin = grace_bin().expect("build the release binary first: cargo build --release");
    let session_a = format!("live-e2e-a-{}", uuid::Uuid::new_v4());
    let session_b = format!("live-e2e-b-{}", uuid::Uuid::new_v4());

    let run = |session_id: &str, prompt: &str| -> String {
        let output = Command::new(&bin)
            .current_dir(dir.as_path())
            .env("HOME", &home)
            .args([
                "--base-url", &base_url,
                "--api-key", &api_key,
                "--model", &model,
                "--session", session_id,
                "--prompt", prompt,
                "--max-iterations", "5",
            ])
            .output()
            .expect("failed to spawn grace binary");
        assert!(
            output.status.success(),
            "grace exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    };

    // Concurrent-ish: interleave the two sessions' turns rather than fully
    // serializing A-then-B, to exercise the same sqlite WAL path two
    // simultaneous terminals would hit.
    run(&session_a, "My favorite fruit is a kumquat-7a2b. Just acknowledge briefly.");
    run(&session_b, "My favorite fruit is a durian-3f9c. Just acknowledge briefly.");
    let ask_a = run(&session_a, "What is my favorite fruit? One word only.");
    let ask_b = run(&session_b, "What is my favorite fruit? One word only.");

    assert!(
        ask_a.to_lowercase().contains("kumquat"),
        "session A leaked or lost its own fact, got:\n{ask_a}"
    );
    assert!(
        !ask_a.to_lowercase().contains("durian"),
        "session A leaked session B's fact, got:\n{ask_a}"
    );
    assert!(
        ask_b.to_lowercase().contains("durian"),
        "session B leaked or lost its own fact, got:\n{ask_b}"
    );
    assert!(
        !ask_b.to_lowercase().contains("kumquat"),
        "session B leaked session A's fact, got:\n{ask_b}"
    );
}

/// Minimal owned-tempdir helper (avoids pulling in the `tempfile` crate for
/// 4 test cases) — created under `std::env::temp_dir()`, removed on drop
/// via `TempDir`'s `Drop` impl below.
fn tempdir() -> TempDir {
    let path = std::env::temp_dir().join(format!("grace-live-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir(path)
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn as_path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
