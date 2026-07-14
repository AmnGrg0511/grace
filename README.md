# grace

A **minimal, vendor-neutral ReAct agent core** — the irreducible spine of an
agent (Hermes-inspired), written in Rust with best practices and **zero
dependencies** (only `std`).

This is the result of a careful analysis of the Hermes engine: we extracted
what is *core tech* (a normalized LLM loop + a tool substrate) and dropped the
*wrapper* (multi-provider fallback chains, context compression, tool-safety
guardrails, `/steer`, skills/vault knowledge, the TUI). Those are real value in
production; they are not the core.

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

That is the whole agent. Everything else is configuration.

## Modules

| Module | Role |
|---|---|
| `message` | The unified conversation record (the source of truth). |
| `transport` | `ProviderTransport` trait + normalized `ModelResponse`/`FinishReason`. |
| `transport_http` | OpenAI-compatible `/chat/completions` over plain `http://` (no TLS). |
| `transport_openrouter` | OpenRouter (HTTPS) via an auto-spawned python3 TLS proxy. |
| `transport_mock` | Scripted offline "model" — proves the loop with zero network. |
| `tool` | `Tool` trait + `ToolRegistry` (name → handler dispatch). |
| `tools` | Built-ins: `run_terminal`, `read_file`, `write_file`, `patch`. |
| `agent` | The ReAct loop. |
| `config` | Runtime wiring (which transport, which model, budget). |
| `json` | A tiny dependency-free JSON value/parser/serializer. |
| `error` | One flat error type. |

## Build & run

Requires Rust ≥ 1.75 (no external crates — builds fully offline).

```bash
cargo build --release
cargo test                 # unit + integration tests (no network)

# Offline demo (scripted model drives the real tools):
./target/release/grace --mock --prompt "run a terminal command"

# Real OpenAI-compatible endpoint (see TLS note below):
./target/release/grace \
  --base-url http://127.0.0.1:8080/v1 \
  --api-key "$KEY" --model grace-1 \
  --prompt "list the files in the current directory"

# OpenRouter (HTTPS) — key from --api-key or $OPENROUTER_API_KEY.
# Grace auto-spawns a tiny python3 TLS proxy and talks to it over
# plaintext, so the crate stays std-only (see TLS note below).
#
# NOTE: OpenRouter keys are often restricted to FREE models only. Use a
# ":free" model id — e.g. tencent/hy3:free (or the openrouter/free router).
# Paid ids like openai/gpt-4o-mini return HTTP 403 "Key limit exceeded".
export OPENROUTER_API_KEY=sk-or-...
./target/release/grace \
  --openrouter --model tencent/hy3:free \
  --prompt "list the files in the current directory"

# Interactive chat against OpenRouter (state persists across turns):
./target/release/grace --openrouter --model tencent/hy3:free --chat
```

## Why std-only (and the TLS caveat)

`std` has no TLS, so `transport_http` speaks **plaintext `http://`**. To reach a
TLS provider (OpenAI, Nous, etc.), front it with a local proxy
(`nginx`/`mitmproxy`/a one-liner) and point `--base-url` at
`http://127.0.0.1:PORT`. This is a deliberate, common production pattern and it
keeps the crate **dependency-free and supply-chain-free**.

**OpenRouter is wired this way by default.** `transport_openrouter` embeds a
~40-line pure-stdlib `python3` proxy that terminates TLS to
`https://openrouter.ai` and exposes `http://127.0.0.1:PORT/api/v1` to the
plaintext `transport_http`. Grace spawns it as a child process, waits until it
is listening, runs the whole conversation through it, and force-kills it on
exit. No Rust TLS crate, no `crates.io` dependency, no build changes — a single
`grace --openrouter --model ... --prompt ...` command works. The only external
requirement is `python3 >= 3.7` on `PATH`.

The `ProviderTransport` seam means you can add a real TLS transport (e.g. via
`hyper`/`rustls`) without touching the loop.

## What is intentionally NOT here

The analysis concluded these are *wrapper*, not core. They belong in a
production agent; omitting them is the point of the minimal rewrite:

- **Multi-provider fallback** — `HttpTransport` is one provider. Chain several
  behind a `ProviderTransport` if you need resilience.
- **Context compression** — long sessions will hit the model's context limit.
- **Tool-safety guardrails** — `run_terminal` is unguarded. In production, add a
  command allow-list / sandbox and a path allow-list for file tools.
- **Streaming, retries, `/steer`, skills** — out of scope by design.

## Security & safety

Grace executes model-requested shell commands and file writes with **no
sandbox or allow-list**. It is safe to run against the offline `--mock` model
or a trusted endpoint; do **not** point it at an untrusted model or expose it
on a shared host without adding the guardrails above. The bundled tools are
deliberately thin so you can harden them for your environment.

## Lines of code

~2,240 lines of Rust across the modules above (tests separate). The **agent
loop itself is ~60 lines**; the bulk is the dependency-free JSON parser (with
tests) and the HTTP/OpenRouter transports. The point stands: the *core logic*
is tiny; the volume is plumbing you can drop or swap.

## License

Licensed under either of **MIT** or **Apache-2.0** at your option (see the
`LICENSE` file). Written by Aman Garg.
