# Grace

**A production-grade agent CLI that learns.** ~5,700 lines of Rust. No framework. No async runtime. Single binary.

```bash
# Install in 10 seconds
curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh | bash
```

---

## Why Grace?

| Problem | Grace |
|---------|-------|
| **Slow setup** | `curl ... \| bash` → works in 10s. No Rust, no Docker, no config. |
| **Fragile tools** | Built-in `run_terminal`, `read_file`, `write_file`, `patch` with allow-lists. |
| **Amnesia** | Persistent SQLite memory + FTS5 session search. Remembers across restarts. |
| **One-shot skills** | Drop a `SKILL.md` in `~/.grace/skills/` → `load_skill` in chat. Self-improves. |
| **Vendor lock-in** | Works with any OpenAI-compatible endpoint (OpenAI, OpenRouter, Ollama, vLLM, LM Studio). |

---

## Quick Start

```bash
# 1. Install (10s, statically linked musl binary — no GLIBC issues)
curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh | bash

# 2. First run: interactive provider setup (OpenRouter, OpenAI, Ollama, etc.)
grace --chat

# 3. Use it
> "Read the codebase and add a health check endpoint"
> "Refactor the auth module to use JWT"
> "Find and fix the memory leak in the worker pool"
```

**One-shot mode:**
```bash
grace --prompt "Add a health check to main.rs"
```

---

## The Loop (Why It Feels Fast)

```
Your prompt → Grace plans → Tools execute → Results feed back → Repeat until done
```

- **Bounded iterations** (default 256, configurable)
- **Ctrl-C interrupts mid-turn** — no stuck processes
- **Streaming tokens** with `--stream` (one-shot mode)
- **Typical task: 3-8 tool calls, 10-30 seconds**

---

## Self-Learning & Skills (The Differentiator)

### 1. Persistent Memory (Auto)
```bash
# You say:
> "I prefer 2-space tabs and early returns"

# Grace stores it in SQLite, injects into every future prompt automatically
```

### 2. Skills That Self-Improve
```bash
# Drop a skill file once:
~/.grace/skills/my-refactor/SKILL.md

# Use it forever:
> "Apply my-refactor to the auth module"

# Grace loads it, executes, and the skill evolves with each use
```

**3 default skills ship on first run:**
- `grace-agent` — knows its own architecture
- `memory-update` — when to persist facts
- `skill-author` — creates new skills from workflows

### 3. Pre-Flight Recall (Before Every Turn)
Before each turn, Grace searches past sessions (FTS5) for relevant context and injects it. You don't ask "remember X" — it just knows.

---

## Security (Default-Deny)

```bash
# File access: only within allowed dirs
GRACE_ALLOW_DIR="/home/user/projects" grace --chat

# Commands: allow-list only
GRACE_TERMINAL_ALLOW="ls,cat,rg,cargo" grace --chat
```

---

## Install Options

| Method | Command |
|--------|---------|
| **Install script** | `curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh &#124; bash` |
| **Homebrew** | `brew install grace` *(coming soon)* |
| **Binary** | Download from [releases](https://github.com/AmnGrg0511/grace/releases) |
| **Cargo** | `cargo install grace` |

---

## What's Not Here (By Design)

- **Multi-provider fallback** — compose behind `ProviderTransport` if needed
- **Context compression** — long sessions hit the model's limit
- **TUI** — CLI first. TUI is a layer on top.
- **Sandbox** — allow-lists are a first step. Use a container/VM for untrusted models.

---

## Architecture

```
Message list ──► ProviderTransport (normalized LLM call)
                   │  returns content + optional tool_calls
                   ▼
              if tool_calls: ToolRegistry executes each
                   │  results appended as `tool` messages
                   ▼
              loop until FinishReason::Stop (or budget exhausted)
```

23 modules, ~6,600 lines, `#![forbid(unsafe_code)]`. See `docs/minimalism-audit.md`
for a module-by-module core/polish/bloat classification.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build/test/release instructions.

---

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
Written by Aman Garg.

---

*Grace is an agent. Not a framework. If you want to understand it, read `src/main.rs`. The whole thing fits in your head.*
