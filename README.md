# Grace

A **production-grade agentic CLI** — a ReAct agent core with durable memory,
session history, skill loading, syntax-highlighted markdown rendering, and a
multi-provider transport layer. Written in ~5,700 lines of Rust. No async
runtime, no framework, no bloat.

```
┌─────────────────────────────────────────────────────────────┐
│                      Grace Architecture                      │
│                                                             │
│  CLI args → Config → Transport (OpenAI-compatible HTTP/SSE)  │
│                    ↓                                        │
│  ┌─ Agent Loop (bounded, interruptible) ──────────────┐     │
│  │  Transport.complete() → ModelResponse              │     │
│  │  ├─ Stop? → render answer, done                    │     │
│  │  ├─ ToolCalls? → execute each via ToolRegistry     │     │
│  │  │   └─ results appended as tool messages          │     │
│  │  └─ Length? → continue                             │     │
│  └────────────────────────────────────────────────────┘     │
│                    ↓                                        │
│  Memory (SQLite) · Session (SQLite + FTS5) · Skills (FS)    │
│  Recall (pre-flight context injection)                      │
│  Markdown renderer (pulldown-cmark + syntect) · 4 skins     │
└─────────────────────────────────────────────────────────────┘
```

## Why

Most agent frameworks are either **thick orchestrators** (LangChain, AutoGen) —
layers of abstraction, context compression, plugin ecosystems, config DSLs — or
**thin wrappers** (basic API clients) with no tool loop, no memory, no
persistence.

Grace is the third path: a production-quality agent with real tools, durable
memory, session history, skills, streaming, and syntax-highlighted rendering —
in ~5,700 lines of Rust. It compiles in 40 seconds, runs as a single binary,
and doesn't require you to learn a framework. You read the source and you
understand the whole thing.

## Features

| Feature | What it does |
|---|---|
| **Agent loop** | Model calls tools → tools return results → model continues, until it answers. Bounded by `max_iterations` (default 256). Ctrl-C interrupts mid-turn. |
| **Vendor-neutral transport** | One trait (`ProviderTransport`), HTTP + SSE implementations. Works with any OpenAI-compatible endpoint (OpenAI, OpenRouter, Ollama, vLLM, LM Studio, etc.). Switch models with `/model` mid-chat. |
| **Built-in tools** | `run_terminal`, `read_file`, `write_file`, `patch` — with path allow-lists (`GRACE_ALLOW_DIR`) and command allow-lists (`GRACE_TERMINAL_ALLOW`). |
| **Durable memory** | SQLite-backed facts that survive process restarts. Injected into every system prompt automatically. `--remember "fact"` to store, recall is automatic. |
| **Session history** | SQLite + FTS5 full-text search across past conversations. `--session <name>`, `--list-sessions`, `--search-sessions "query"`. `/session` switcher in chat mode. |
| **Skills** | Filesystem convention: `~/.grace/skills/<name>/SKILL.md`. Loaded on demand via `load_skill` tool. Three default skills seeded on first run: `grace-agent`, `memory-update`, `skill-author`. |
| **Pre-flight recall** | Searches past sessions before each turn and injects relevant context — the agent "remembers" what it already knew. |
| **Delegation** | `delegate` tool spawns a fresh isolated `grace` subprocess for independent subtasks. |
| **Plugin tools** | Discover external tools from `~/.grace/tools/<name>/manifest.json`. |
| **Markdown rendering** | pulldown-cmark + syntect. Tables, code blocks (200+ language syntax highlighting), bold, italic, inline code, blockquotes, task lists, horizontal rules. TTY-gated (pipes pass through raw). |
| **4 built-in skins** | `solaris` (amber, default), `royal` (violet), `ocean` (teal), `sakura` (pink). Custom skins via `~/.grace/skins/<name>.toml`. `/skin` switcher in chat. |
| **Streaming** | `--stream` flag for one-shot mode. SSE parsing with live token printing. |
| **Shell completions** | `--completions bash\|zsh\|fish` prints installable completion scripts. |
| **Interactive chat** | `--chat` mode with rustyline (arrow-key history, line editing), `/model`, `/skin`, `/session`, `/exit` commands. |
| **Settings persistence** | `~/.grace/config.toml` — model, API key, skin, max-iterations all persist. First-run onboarding wizard picks a provider. |

## Quick start

```bash
# Build
cargo build --release

# Interactive chat (first run walks you through provider setup):
./target/release/grace --chat

# OpenRouter:
export OPENROUTER_API_KEY=sk-or-...
./target/release/grace --openrouter --model openai/gpt-4o-mini --chat

# One-shot prompt:
./target/release/grace --openrouter --model openai/gpt-4o-mini \
  --prompt "list the files in the current directory"

# Stream tokens as they arrive:
./target/release/grace --openrouter --model openai/gpt-4o-mini --stream \
  --prompt "explain how transformers work"

# Durable memory:
./target/release/grace --remember "user prefers concise answers"
./target/release/grace --prompt "what do you know about me?"

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
  Default-deny (empty = allow all). Set to `ls,cat,echo` to restrict.
  Commands are matched by their first token.

```bash
# Only allow ls and cat:
GRACE_TERMINAL_ALLOW="ls,cat" ./target/release/grace --chat

# Only allow file access under /home/user/projects:
GRACE_ALLOW_DIR="/home/user/projects" ./target/release/grace --chat
```

## Dependency stance

Grace prefers **official, maintained crates** over hand-rolled reimplementations:

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
| `ctrlc` | Graceful mid-turn interrupt without killing the process. |

What Grace avoids: heavy async runtimes (blocking CLI doesn't need one), ORMs,
config frameworks, anything with a large transitive tree relative to its value.
Every dependency earns its place.

## Architecture

```
~5,700 lines across 22 modules:

message.rs           144  — unified conversation record
transport.rs         154  — ProviderTransport trait + ModelResponse/FinishReason
transport_http.rs    184  — OpenAI-compatible HTTPS via reqwest
transport_stream.rs  229  — SSE streaming with tool-call accumulation
tool.rs               73  — Tool trait + ToolRegistry dispatch
tools.rs             380  — built-ins: terminal, read_file, write_file, patch, search_sessions
agent.rs             278  — the agent loop (bounded, interruptible)
config.rs            219  — runtime wiring (transport, model, budget)
settings.rs          265  — ~/.grace/config.toml persistence
memory.rs            238  — SQLite durable facts (injected into system prompt)
session.rs           262  — SQLite chat history with FTS5 search
recall.rs            218  — pre-flight recall (injects past context before each turn)
skill.rs             208  — filesystem-convention skill loading + load_skill tool
default_skills.rs    189  — seeds 3 default skills on first run
delegate_tool.rs      118  — spawns isolated grace subprocess for subtasks
plugin_tool.rs       196  — discovers external tools from manifest.json
skin.rs              250  — 4 built-in skins + custom skins from TOML
markdown.rs          566  — pulldown-cmark + syntect rendering
diff.rs               51  — similar-based diff rendering
error.rs              75  — unified error type
main.rs            1,361  — CLI, chat loop, model/skin/session pickers, completions
```

## What's intentionally not here

- **Multi-provider fallback chains** — one transport at a time. Compose
  multiple behind `ProviderTransport` if you need resilience.
- **Context compression** — long sessions hit the model's context limit.
- **TUI** — Grace is a CLI. A TUI is a separate layer on top.
- **Sandboxing** — the allow-lists are a first step, not a sandbox. Use a
  container or VM for untrusted models.

## Roadmap

- **Intelligent skill/memory loading** — the agent loop (not the system prompt)
  decides when to load skills or persist memory after each turn, based on
  iteration count and task complexity.
- **Streaming in chat mode** — live token display during interactive sessions.
- **Context window management** — automatic summarization or trimming when
  conversations approach the model's context limit.

## Build & test

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings

# 38 tests, 0 warnings, 0 clippy violations.
```

## License

MIT OR Apache-2.0, at your option. Written by Aman Garg.
