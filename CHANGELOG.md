# Changelog

## v0.3.3

**Fixed**
- The answer appeared **all at once at the end** of generation in chat
  mode and one-shot `--stream` instead of progressively.  The streaming
  transport read the entire HTTP body before parsing it, so every
  fragment fired in one burst after the model finished.  The SSE body is
  now read incrementally from the socket and each fragment reaches the
  renderer as it is produced; first visible token arrives within one
  round trip of generation start.
- A mid-stream network failure now surfaces as an io error naming the
  read failure (previously a transport error from the buffered read);
  it still aborts the turn with a message, never silently.

**Tests**
- `parse_sse_stream` is now guarded against its two load-bearing
  properties: chunk-boundary agnosticism (byte-at-a-time delivery parses
  identically to a one-shot buffer) and terminating at `data: [DONE]`
  even when the connection stays open (keep-alive) afterward.

## v0.3.2

**Fixed**
- Wide tables rendered at their natural width (hundreds of columns), so
  every row soft-wrapped and the box was unreadable.  Tables now fit the
  terminal: columns shrink proportionally to the render width (floors of
  1–2 columns so two-cell glyphs like ✅/CJK always fit) and cell text
  word-wraps ANSI-aware, hard-breaking unbroken tokens.  Tables that
  already fit render byte-identically to before.
- The width source was previously none at all: a `COLUMNS`-only fallback
  would not see a width in shells that don't export it (the common case
  here), silently keeping the old overflow behavior.

**Added**
- The render width now comes from the live TTY size (`terminal_size` —
  std has no size API on stable), with `$COLUMNS` as the fallback for
  piped output (e.g. `grace | less -R`).  Resizes are picked up between
  turns; the stream path pins the width per stream so a mid-stream resize
  can't re-wrap an already-committed table.
- Chat startup wordmark (skin rebrands it via the answer/tool-dim tiers)
  with a clean-screen open; the tool-call line is dimmed so it recedes
  behind the streamed answer.

## v0.3.1

**Fixed**
- Streaming output was **silently truncated** when a list contained
  nested items with styled text (e.g. `**bold**` in a nested bullet):
  inline style escapes were emitted at the tag-open position, and in
  tight lists that position can sit on the *parent* item's line — so
  rendering the extended prefix changed bytes already emitted, and the
  duplication guard dropped the rest of the answer.  Inline styles are
  now emitted together with the styled text, keeping every render a
  true byte-prefix of the next.
- Strong/emphasis/link inside table cells wrote escapes into the wrong
  buffer; cells now render their styling correctly.

**Tests**
- `stream_sim_driver` is now self-contained (committed real capture
  fixture, OS-temp outputs) and asserts the full answer reaches the
  terminal at 20/40/120 columns from a real 37 KB thinking-model turn —
  the regression above is caught in CI, not just locally.

## v0.3.0

**Added**
- Append-only streaming: long answers now stream past the terminal
  viewport with no duplication and no cursor re-rendering. Each
  completed markdown block (paragraphs, headings, lists, code fences,
  tables) is rendered exactly once and emitted exactly once.
- Tables commit atomically (like code fences), so a table box is never
  exposed half-sized mid-stream.
- VT replay driver test (`stream_sim_driver`) replays the bytes grace
  actually emits through a terminal screen model.

**Fixed**
- Streaming replies from thinking models (gateways that stream
  reasoning tokens first, then content beginning with blank lines)
  rendered **nothing at all** — the renderer's unconditional trailing
  newline broke the byte-prefix invariant the duplication guard
  enforces, so every delta after the first was silently dropped.
  Empty/whitespace-only input now renders to nothing.
- Syntax highlighting data reloaded on every streamed line; now cached
  for the process lifetime.

## v0.2.1

**Fixed**
- `--chat` default session id collided across unrelated terminals (all
  landed in the same session); now derived from the controlling tty so
  concurrent terminals never share a session. Live-verified against a real
  endpoint with two concurrently running named sessions.
- GitHub Copilot: removed as a special-cased transport and CLI flag
  entirely. Picking "GitHub Copilot" in onboarding now runs the OAuth
  device flow as the picker's key-entry step and wires the result up as an
  ordinary `Http` transport (base_url + bearer token) — identical code path
  to every other provider from that point on. No `--copilot` flag exists
  anywhere in the CLI surface.
- Onboarding model selection now queries the provider's live `/models`
  endpoint (context windows included) instead of only showing the
  hardcoded preset list; falls back to presets if the live call fails.
  Live-verified against a real self-hosted endpoint (11 real models
  returned, not presets).

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
