# Changelog

## v0.2.0

**Fixed**
- Live-endpoint bug: `ToolCall`'s wire-format `type` field was omitted on
  serialization for its only real value (`"function"`), which strict
  OpenAI-compatible servers (vLLM and others) reject with a 422 on any
  follow-up request after a tool call. Every `read_file`/`write_file`/
  `run_terminal` round-trip was broken against such backends. Found via a
  live end-to-end test against a real self-hosted deployment, not a mock.
- `print_status_line` was reloading and re-parsing `~/.grace/config.toml`
  from disk on every single chat turn. Now cached once per session,
  refreshed only on `/model`.
- GitHub Copilot device-flow auth: `expires_in` made optional in the token
  response, response text checked before JSON parsing, onboarding wizard
  skipped when `--copilot` is passed, token saved to `.env` correctly.

**Added**
- `docs/minimalism-audit.md` — module-by-module core/polish/bloat
  classification against the "core developers only, CLI based" philosophy.
- `LICENSE-MIT` + `LICENSE-APACHE` — dual-license files backing the
  `MIT OR Apache-2.0` declaration in `Cargo.toml` (previously undeclared).
- `CONTRIBUTING.md` — build/test/release instructions.
- `tests/live_endpoint.rs` — opt-in live e2e test tier (4 tests: read_file,
  write_file+read_file, run_terminal, session persistence across process
  restarts) against a real OpenAI-compatible endpoint via
  `GRACE_LIVE_BASE_URL`/`GRACE_LIVE_API_KEY`/`GRACE_LIVE_MODEL` env vars.
  `#[ignore]`d by default — the standard `cargo test` run stays hermetic
  and network-free.
- 2 regression unit tests in `message.rs` pinning the tool_call wire format
  (had zero tests before this release).

**Changed**
- `main.rs` split 1424 → 445 lines into `cli.rs` (127), `chat.rs` (685),
  `wizard.rs` (134) — pure code motion, no behavior change.
- `fetch_context_window` moved into `transport_http.rs` (transport layer,
  not CLI).
- `packaging/homebrew/grace.rb` — corrected to musl artifact names (was
  pointing at nonexistent `gnu`-suffixed v0.1.0 assets); sha256 sums
  verified against the real v0.1.9 release assets.
- `transport_copilot.rs` cleaned up: removed dead `CopilotModel` struct and
  unread `api_key` field, collapsed a duplicate if/else branch, removed
  redundant `reqwest::Client` instances.

41 → 47 tests total (43 unit + 4 opt-in live). `cargo clippy --all-targets
--release -- -D warnings` clean throughout.
