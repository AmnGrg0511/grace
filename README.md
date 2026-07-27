# Grace

**An agent that works.** No framework. No async runtime. No bloat.

A single binary. `~5,700` lines of Rust. You read the source, you understand the whole thing.

```
cargo build --release
./target/release/grace --chat
```

## What it does

| Capability | How it works |
|---|---|
| **Agent loop** | ReAct: model calls tools → tools return → model continues. Bounded iterations. Ctrl-C interrupts mid-turn. |
| **Any provider** | OpenAI, OpenRouter, Ollama, vLLM, LM Studio — anything OpenAI-compatible. Switch with `/model` mid-chat. |
| **Real tools** | `run_terminal`, `read_file`, `write_file`, `patch` — with allow-lists so you sleep at night. |
| **Memory that persists** | SQLite-backed facts. Injected automatically. `--remember "prefers concise answers"`. |
| **Sessions you can search** | FTS5 full-text across all history. `--search-sessions "refactor auth"`. |
| **Skills on demand** | Filesystem convention: `~/.grace/skills/<name>/SKILL.md`. Loaded via `load_skill`. Three defaults ship: `grace-agent`, `memory-update`, `skill-author`. |
| **Recall before acting** | Pre-flight search injects relevant past context automatically. |
| **Delegation** | `delegate` tool spawns isolated subprocesses for independent subtasks. |
| **Plugin tools** | Drop `manifest.json` in `~/.grace/tools/` — auto-discovered. |
| **Markdown that looks right** | Tables, syntax-highlighted code blocks, bold, blockquotes, task lists. TTY-gated (pipes pass raw). |
| **4 skins** | `solaris` (default), `royal`, `ocean`, `sakura`. Custom via TOML. |
| **Streaming** | `--stream` for live tokens. |
| **Shell completions** | `--completions bash\|zsh\|fish`. |

## Quick start

```bash
# Build (40s first time, cached after)
cargo build --release

# First run: interactive provider setup
./target/release/grace --chat

# Or point at any OpenAI-compatible endpoint
export OPENROUTER_API_KEY=sk-or-...
./target/release/grace --openrouter --model openai/gpt-4o-mini --chat

# One-shot with streaming
./target/release/grace --openrouter --model openai/gpt-4o-mini --stream \
  --prompt "explain how transformers work"

# Teach it something permanent
./target/release/grace --remember "user prefers concise answers"
./target/release/grace --prompt "what do you know about me?"

# Search history
./target/release/grace --search-sessions "refactor auth"
```

## Security

Two environment variables. That's it.

```bash
# Restrict file access
GRACE_ALLOW_DIR="/home/amagar24/projects" ./target/release/grace --chat

# Restrict commands
GRACE_TERMINAL_ALLOW="ls,cat,rg" ./target/release/grace --chat
```

## Dependencies — only what earns its place

| Crate | Purpose |
|---|---|
| `reqwest` (rustls-tls, blocking) | Real HTTPS. No hand-rolled TLS. |
| `serde` / `serde_json` | Real JSON. |
| `rusqlite` (bundled) | SQLite + FTS5. No system dependency. |
| `pulldown-cmark` | GFM markdown. Tables, task lists. |
| `syntect` | 200+ language syntax highlighting. |
| `anstyle` | Zero-alloc ANSI. Proper NO_COLOR/CLICOLOR. |
| `similar` | Unified diff (same engine as ruff). |
| `rustyline` | Arrow-key history, line editing. |
| `ctrlc` | Graceful mid-turn interrupt. |

No async runtime. No ORM. No config framework. Every dependency pays rent.

## What's not here (by design)

- **Multi-provider fallback** — compose behind `ProviderTransport` if you need it
- **Context compression** — long sessions hit the model's limit
- **TUI** — CLI first. TUI is a layer on top.
- **Sandbox** — allow-lists are a first step. Use a container/VM for untrusted models.

## License

MIT OR Apache-2.0. Written by Aman Garg.

---

*Grace is an agent. Not a framework. If you want to understand it, read `src/main.rs`. The whole thing fits in your head.*