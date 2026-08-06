# Contributing to Grace

Grace is a minimal, vendor-neutral ReAct agent core, built for core developers
who want a fast CLI agent without unnecessary features. Contributions that
shrink the core or fix a real bug are especially welcome; contributions that
add optional surface area need a concrete use case.

## Build

```bash
CARGO_TARGET_DIR=/path/to/target cargo build --release
```

(`CARGO_TARGET_DIR` is only needed if your home/repo partition has quota
limits — set it to any writable scratch path.)

## Test

```bash
CARGO_TARGET_DIR=/path/to/target cargo test --release
```

All 41 tests must pass before a PR is considered.

## Lint

```bash
CARGO_TARGET_DIR=/path/to/target cargo clippy --all-targets --release -- -D warnings
```

Clippy must be clean with `-D warnings` — no exceptions, no `#[allow]` without
a comment justifying it (see `transport_copilot.rs` for the pattern: allow
dead_code on wire-format structs with a comment explaining why).

## Code conventions

- `#![forbid(unsafe_code)]` stays in every crate root — never removed.
- Use official/native crates only; don't reinvent standard functionality.
  Every new dependency must earn its place — justify it in the PR description.
- No vendor lock-in: the core (`agent.rs`, `transport.rs`, `tool.rs`) must stay
  provider-agnostic. Vendor-specific code lives in its own `transport_*.rs`
  module and must not leak into the agent loop.
- Match existing style; no drive-by reformatting in a functional PR.

## Release process

1. Bump `version` in `Cargo.toml`.
2. Update `CHANGELOG` section in README (or a `CHANGELOG.md` if one exists).
3. `cargo build --release` + `cargo test --release` + `cargo clippy -- -D warnings`
   must all be clean.
4. Tag: `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. Cross-compile and verify all 4 release targets build:
   - `x86_64-unknown-linux-musl`
   - `aarch64-unknown-linux-musl`
   - `x86_64-apple-darwin`
   - `aarch64-apple-darwin`
6. Update `packaging/homebrew/grace.rb` with the new version's release
   tarball sha256 sums.
7. Update `scripts/install.sh` if the release asset naming changed.

## Reporting issues

Open a GitHub issue with: Grace version (`grace --help` header), OS/arch,
the exact command that failed, and full stderr output.
