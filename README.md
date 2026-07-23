# grace

A **minimal, fast, vendor-neutral ReAct agent core** — the irreducible spine of
an AI agent, written in Rust. No bloat. No async runtime. No TUI. Just the loop.

```
Message list  ──►  ProviderTransport (normalized LLM call)
                   │  returns content + optional tool_calls
                   ▼
              if tool_calls: ToolRegistry executes each
                   │  results appended as `tool` messages
                   ▼
              loop until FinishReason::Stop (or budget exhausted)
```

That is the whole agent. Everything else — memory, skills, skins, streaming,
session history — is additive state around that loop, not a rewrite of it.

## Why

Most agent frameworks are either:
- **Thick orchestrators** (LangChain, AutoGen) — layers of abstraction,
  context compression, multi-provider routing, plugins, hooks, config
  DSLs. You spend more time fighting the framework than writing your agent.
- **Thin wrappers** (basic API clients) — no tool loop, no memory, no
  persistence. Just a glorified `curl`.

Grace is the **third path**: a production-quality ReAct loop with real tools,
real memory, real transports, real rendering — in ~5,000 lines of Rust. It
compiles in 40 seconds, runs as a single binary, and doesn't require you to
learn a framework. You read the source and you understand the whole thing.

## Features

| Feature | What it does |
|---|---|
| **ReAct loop** | Model calls tools → tools return results → model continues, until it answers. Bounded by `max_iterations` (default 256). |
| **Vendor-neutral transport** | One trait (`ProviderTransport`), three implementations: `HttpTransport` (any OpenAI-compatible endpoint), `MockTransport` (offline), `SseStream` (streaming). Switch providers with `/model` mid-chat. |
| **Built-in tools** | `run_terminal`, `read_file`, `write_file`, `patch` — with path allow-lists (`GRACE_ALLOW_DIR`) and command allow-lists (`GRACE_TERMINAL_ALLOW`). |
| **Durable memory** | SQLite-backed facts that survive process restarts. Injected into every system prompt. |
| **Session history** | SQLite + FTS5 full-text search across past conversations. `--list-sessions`, `--search-sessions "query"`. |
| **Skills** | Filesystem convention: `skills/<name>/SKILL.md`. Loaded on demand via the `load_skill` tool. |
| **Markdown rendering** | pulldown-cmark + syntect. Tables, code blocks (with syntax highlighting), bold, inline code, blockquotes, task lists, horizontal rules. TTY-gated (pipes pass through raw). |
| **4 built-in skins** | `solaris` (amber, default), `royal` (violet), `ocean` (teal), `sakura` (pink). Custom skins via `~/.grace/skins/<name>.toml`. |
| **Streaming** | `--stream` flag for one-shot mode. SSE parsing with live token printing. |
| **Shell completions** | `--completions bash\|zsh\|fish` prints installable completion scripts. |
| **Interactive chat** | `--chat` mode with rustyline (arrow-key history, line editing), `/model`, `/skin`, `/exit` commands. |
| **Ctrl-C handling** | Mid-turn interrupt cancels the current turn without killing the process. |

## Quick start

```bash
# Build (first run fetches crates, subsequent runs are cached)
cargo build --release

# Offline demo (scripted model drives real tools):
./target/release/grace --mock --prompt "run a terminal command"

# Real OpenAI-compatible endpoint:
./target/release/grace \
  --base-url https://api.openai.com/v1 \
  --api-key "$OPENAI_API_KEY" --model gpt-4o-mini \
  --prompt "list the files in the current directory"

# OpenRouter:
export OPENROUTER_API_KEY=sk-or-...
./target/release/grace --openrouter --model tencent/hy3:free --chat

# Stream tokens as they arrive:
./target/release/grace --openrouter --model tencent/hy3:free --stream \
  --prompt "explain how transformers work"

# Persistent memory:
./target/release/grace --mock --remember "user prefers concise answers"
./target/release/grace --mock --prompt "what do you know about me?"

# Search past conversations:
./target/release/grace --search-sessions "rust async"

# Shell completions:
eval "$(./target/release/grace --completions bash)"
```

## Security

Grace executes model-requested shell commands and file writes. Two
environment variables harden it:

- **`GRACE_ALLOW_DIR`** — path allow-list for `read_file`/`write_file`/`patch`.
  Defaults to the current working directory. Set to `*` to allow all paths.
- **`GRACE_TERMINAL_ALLOW`** — command allow-list for `run_terminal`.
  Default-deny (empty = no commands allowed). Set to `ls,cat,echo` or `*` to
  allow. Commands are matched by their first token (the executable name).

```bash
# Only allow ls and cat:
GRACE_TERMINAL_ALLOW="ls,cat" ./target/release/grace --mock --chat

# Only allow file access under /home/user/projects:
GRACE_ALLOW_DIR="/home/user/projects" ./target/release/grace --mock --chat
```

## Dependency stance

Grace prefers **official, maintained crates** over hand-rolled
reimplementations of solved problems:

| Crate | Why |
|---|---|
| `reqwest` (rustls-tls, blocking) | Real HTTPS. No proxy hacks, no hand-rolled TLS. |
| `serde` / `serde_json` | Real JSON. Not a hand-rolled parser. |
| `rusqlite` (bundled) | Real persistent memory + FTS5. Not a text file. |
| `pulldown-cmark` | GFM markdown parsing. Tables, task lists, strikethrough. |
| `syntect` | Syntax highlighting for code blocks. 200+ languages. |
| `anstyle` | Zero-alloc ANSI styling with proper NO_COLOR/CLICOLOR support. |
| `similar` | Unified diff for the `patch` tool. Same engine as ruff. |
| `rustyline` | Arrow-key history, line editing for chat mode. |

What Grace avoids: heavy async runtimes (blocking CLI doesn't need one), ORMs,
config frameworks, anything with a large transitive tree relative to its value.
Every dependency earns its place.

## Architecture

```
~5,400 lines across 22 modules:

message.rs          143  — unified conversation record
transport.rs        155  — ProviderTransport trait + ModelResponse/FinishReason
transport_http.rs   183  — OpenAI-compatible HTTPS via reqwest
transport_mock.rs   104  — offline scripted model (zero network)
transport_stream.rs 228  — SSE streaming with tool-call accumulation
tool.rs              72  — Tool trait + ToolRegistry dispatch
tools.rs            379  — built-ins: terminal, read_file, write_file, patch
agent.rs            244  — the ReAct loop (bounded, interruptible)
config.rs           216  — runtime wiring (transport, model, budget)
settings.rs        264  — ~/.grace/config.toml persistence
memory.rs           237  — SQLite durable facts (injected into system prompt)
session.rs          261  — SQLite chat history with FTS5 search
skill.rs            207  — filesystem-convention skill loading
skin.rs             249  — 4 built-in skins + custom skins from TOML
markdown.rs         453  — pulldown-cmark + syntect rendering
diff.rs              50  — similar-based diff rendering
main.rs          1,267  — CLI, chat loop, model/skin pickers, completions
```

## What's intentionally not here

- **Multi-provider fallback chains** — one transport at a time. Compose
  multiple behind `ProviderTransport` if you need resilience.
- **Context compression** — long sessions hit the model's context limit.
- **TUI** — Grace is a CLI. A TUI is a separate layer on top, not a core
  concern.
- **Sandboxing** — the allow-lists are a first step, not a sandbox. Use a
  container or VM for untrusted models.

## Build & test

```bash
# Standard:
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings

# 35 tests, 0 warnings, 0 clippy violations.
```

## License

MIT OR Apache-2.0, at your option. Written by Aman Garg.
