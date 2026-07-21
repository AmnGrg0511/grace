# grace

A **minimal, vendor-neutral ReAct agent core** — the irreducible spine of an
agent, written in Rust.

This is the result of extracting what is *core tech* (a normalized LLM loop + a
tool substrate) and dropping the *wrapper* (multi-provider fallback chains,
context compression, tool-safety guardrails, `/steer`, the TUI) — while adding
what was missing: durable memory, real skill loading, solid transports.

## Dependency stance

Grace prefers **official, maintained crates** over hand-rolled
reimplementations of solved problems. Reinventing TCP/TLS framing, a JSON
parser, or an HTTP client from `std` alone is not "minimal" — it's fragile and
unmaintainable. So:

- `reqwest` (rustls-tls, blocking) — real HTTPS, no proxy hacks.
- `serde` / `serde_json` — real JSON, not a hand-rolled parser.
- `rusqlite` (bundled) — real persistent memory, not a text file re-read on
  every turn.

What Grace still avoids: heavy async runtimes when a blocking CLI doesn't need
one, ORMs, config-management frameworks, anything with a large transitive
dependency tree relative to the value it adds. Every dependency has to earn
its place; official/native and actively maintained is the bar, not "zero
deps at all costs."

## What it is

```
Message list  ──►  ProviderTransport (normalized LLM call)
                   │  returns content + optional tool_calls
                   ▼
              if tool_calls: ToolRegistry executes each
                   │  results appended as `tool` messages
                   ▼
              loop until FinishReason::Stop (or budget exhausted)
```

That is the whole agent. Persistent memory and skills are additive state
around it, not a rewrite of the loop.

## Modules

| Module | Role |
|---|---|
| `message` | The unified conversation record (the source of truth). |
| `transport` | `ProviderTransport` trait + normalized `ModelResponse`/`FinishReason`. |
| `transport_http` | OpenAI-compatible `/chat/completions` over real HTTPS via `reqwest`. Also serves OpenRouter (just a base-url preset). |
| `transport_mock` | Scripted offline "model" — proves the loop with zero network. |
| `tool` | `Tool` trait + `ToolRegistry` (name → handler dispatch). |
| `tools` | Built-ins: `run_terminal`, `read_file`, `write_file`, `patch`. |
| `agent` | The ReAct loop. |
| `config` | Runtime wiring (which transport, which model, budget). |
| `memory` | SQLite-backed persistent facts, injected into every system prompt. |
| `skill` | Filesystem-convention skill loading (`skills/<name>/SKILL.md`). |
| `session` | SQLite-backed chat history with FTS search; `--chat` survives restarts. |
| `markdown` | Zero-dep Markdown→ANSI renderer (TTY-gated; no crate needed for this scope). |

## Build & run

Requires Rust ≥ 1.75 and network access to crates.io on first build (fetches
`reqwest`/`serde`/`rusqlite` and their dependency trees; subsequent builds are
offline/cached as usual).

```bash
cargo build --release
cargo test

# Offline demo (scripted model drives the real tools):
./target/release/grace --mock --prompt "run a terminal command"

# Real OpenAI-compatible endpoint:
./target/release/grace \
  --base-url https://api.openai.com/v1 \
  --api-key "$OPENAI_API_KEY" --model gpt-4o-mini \
  --prompt "list the files in the current directory"

# OpenRouter (key from --api-key or $OPENROUTER_API_KEY):
export OPENROUTER_API_KEY=sk-or-...
./target/release/grace \
  --openrouter --model tencent/hy3:free \
  --prompt "list the files in the current directory"

# Interactive chat (state persists across turns, and across restarts once
# session persistence is wired to --chat):
./target/release/grace --openrouter --model tencent/hy3:free --chat
```

## Memory & skills

Grace remembers durable facts across process runs and can load reusable
procedures on demand:

```bash
# Persistent memory (SQLite at ~/.grace/memory.db by default)
./target/release/grace --mock --remember "user prefers concise answers"
./target/release/grace --mock --prompt "what do you know about me?"

# Skills live as plain markdown under ./skills/<name>/SKILL.md and are
# loaded on demand via the built-in `load_skill` tool — no vault required.
```

This is deliberately simple compared to a vault-based skill system: a flat
filesystem convention plus one SQLite file. It is not feature-complete
(no vault, no dreaming yet) — it is the smallest version of "the agent
actually remembers you and can learn a procedure" that is real, not a stub.

## What is intentionally NOT here (yet)

- **Obsidian vault integration** — deferred; the memory/skill primitives above
  are the substrate it will build on later.
- **Multi-provider fallback chains** — `HttpTransport` is one provider at a
  time; chain several behind `ProviderTransport` if you need resilience.
- **Context compression** — long sessions will hit the model's context limit.
- **Tool-safety guardrails** — `run_terminal` is unguarded. Add a command
  allow-list / sandbox before exposing this on a shared host.
- **Streaming, `/steer`** — out of scope by design; the loop is intentionally
  synchronous and easy to reason about.

## Security & safety

Grace executes model-requested shell commands and file writes with **no
sandbox or allow-list**. Safe against the offline `--mock` model or a trusted
endpoint; do **not** point it at an untrusted model or expose it on a shared
host without adding guardrails. The bundled tools are deliberately thin so you
can harden them for your environment.

## License

Licensed under either of **MIT** or **Apache-2.0** at your option (see the
`LICENSE` file). Written by Aman Garg.
