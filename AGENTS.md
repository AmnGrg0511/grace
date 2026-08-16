# AGENTS.md — how to work in this repo

Read this before writing code. Rules beat memory; when in doubt,
smaller and simpler wins. This file is the anti-drift contract: it
exists because agent sessions forget decisions and re-litigate them.

## What grace is

A fast, minimal Rust ReAct agent core. CLI only, for core developers.
No TUI, no web layer. Anything that adds surface area (flags, deps,
commands, files) must serve the specific request at hand — when in
doubt, do not add it.

## Scope — the biggest drift source

- Implement exactly what was asked. No drive-by refactors, no new
  features, no new dependencies (std first; native crates only, never
  for convenience).
- `src/ui/markdown.rs`, `src/ui/skin.rs`, and the completer are
  deliberate product choices. Never flag them as bloat, never remove
  them, in audits or "cleanups".
- Code reads as its own documentation. Comment only where a decision
  is non-obvious. No design docs for what code already says.

## Definition of done — no exceptions

```
cargo test
cargo clippy --all-targets --release -- -D warnings
cargo test --release
cargo build --release
```

- For user-visible fixes: the binary the user will run must be current.
  `stat -c %y` newer than the fix; `nm <bin> | grep <symbol>` proves the
  fix is in it. `~/.local/bin/grace` is a copy — update it if that's
  what they run. Green debug tests with a stale release binary is NOT
  done.

## Hard invariants — breaking these breaks users

- Streaming is **append-only** (`src/ui/chat.rs`): completed markdown
  blocks render once, bytes emit once. Never reintroduce cursor
  movement (cursor-up, clear-to-end, saved positions) in the stream
  path. Guarded by `streaming_markdown_tests` (byte-for-byte
  reassembly).
- `render_terminal_colored` stays **prefix-stable**: render(p) is a
  byte prefix of render(p+more) for line-complete prefixes. Guarded by
  `render_is_prefix_stable_under_line_extension`. Table layout depends
  on the terminal width, so the stream path must use ONE width per
  stream (pinned in `StreamState::width`) — never re-resolve the width
  per render inside the stream path, or a mid-stream resize re-wraps an
  already-committed table and the invariant breaks.
- Never swallow errors in the turn/event path. An empty or failed
  answer must say why — silent no-outputs are the worst failure mode.
- No secrets (keys, endpoint URLs, credentials) in code, comments,
  docs, or commits. Env vars at runtime only.

## Commits

- Verify loop green first. One concern per commit. Imperative subject.
- Stage explicit files — never `git add -A`. Scan the staged diff for
  secrets and stray files before committing.
- `.opencode/` stays gitignored: local agent config, never committed.

## Versions & releases — the sync invariant

- `Cargo.toml` version is the single source of truth.
- Versions move **strictly sequential, one step**: 0.3.0 → 0.3.1 →
  0.3.2 … Patch is the default until 1.0. Step up one minor only for a
  genuine feature/breaking release. Never skip a number.
- Release procedure: CHANGELOG entry → version bump → one
  `chore: release vX.Y.Z` commit (Cargo.toml + Cargo.lock +
  CHANGELOG.md only) → annotated tag → push. CI builds the four
  targets and publishes the GitHub release **from the tag** — the tag
  is the GitHub release. `release.yml` refuses to build if tag and
  Cargo.toml disagree.
- After release, verify all three agree: Cargo.toml, `git describe
  --tags`, `gh release list --limit 1`.
