# Grace

**A production-grade agent CLI that learns.** ~9,000 lines of Rust. No framework. No async runtime. Single binary. `#![forbid(unsafe_code)]`.

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
| **Context overflow** | Token-aware compression against the model's *real* context window. |
| **Noisy subtasks** | `delegate` runs a sub-agent with its own iteration budget and a clean history. |
| **One-shot skills** | Drop a `SKILL.md` in `~/.grace/skills/` → `load_skill` in chat. Self-improves. |
| **Vendor lock-in** | Works with any OpenAI-compatible endpoint (OpenAI, OpenRouter, Copilot, Ollama, vLLM, LM Studio). |

---

## Quick Start

```bash
# 1. Install (10s, statically linked musl binary — no GLIBC issues)
curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh | bash

# 2. First run: interactive provider setup (OpenRouter, OpenAI, Copilot, Ollama, ...)
grace --chat

# 3. Use it
> "Read the codebase and add a health check endpoint"
> "Refactor the auth module to use JWT"
> "Find and fix the memory leak in the worker pool"
```

**One-shot mode:**
```bash
grace --prompt "Add a health check to main.rs"
grace --stream --prompt "Explain this codebase"   # tokens as they arrive
```

---

## The Loop

```
Your prompt
   │
   ├─► compress context if it exceeds the model's trigger
   ├─► transport.complete(messages, tools)
   ├─► if tool_calls: execute each, append results ──┐
   │                                                 │
   └────────────────────── loop ◄────────────────────┘
           until FinishReason::Stop or the budget is spent
```

- **Bounded iterations** (default 256, configurable) — the only guaranteed termination condition
- **Ctrl-C interrupts mid-turn** — completed tool calls are kept, nothing is stuck
- **Streaming** in both chat and one-shot mode, with tool calls still working
- **Typical task: 3–8 tool calls, 10–30 seconds**

---

## Sub-Agent Delegation

Some subtasks are large, noisy, and self-contained: searching a codebase,
summarizing twenty files, a long build-and-fix loop. Running them inline
floods the main conversation with output nobody needs afterwards.

`delegate` runs them as a **sub-agent** — a fresh ReAct loop, in-process:

```
> "Audit every module for missing error handling, then fix main.rs"

  ⇢ delegating (budget 25): audit every module for missing error handling
      · sub-agent done after 11 iterations
  ● patch(src/main.rs)
```

- **Its own iteration budget** (default 25, max 200). A runaway subtask fails
  *its own* budget; the parent keeps every round it had left and can react.
- **A clean history.** The sub-agent cannot see the parent conversation — which
  is the entire point. State what it needs in `task` and `context`.
- **A narrowable tool set.** `tools: ["read_file"]` for a sub-agent that has no
  business holding `run_terminal`.
- **Bounded nesting.** Past depth 3 the `delegate` tool is simply not
  registered, so recursion terminates structurally rather than by asking the
  model nicely.
- **Honest truncation.** Budget exhaustion returns the partial answer marked
  incomplete, not an error — partial findings are usually still useful.

---

## Context Compression

Long sessions used to hit the model's limit and die mid-turn. Compression now
fires automatically at 75% of the window:

- **The window comes from the transport**, not a hardcoded constant. An 8k
  model is budgeted as 8k.
- **Tokens are estimated properly.** Not `len()/4` — a segment-aware BPE
  approximation that accounts for message framing, tool-call JSON, digit runs,
  and CJK. Under-counting is what lets a "safe" estimate overflow.
- **The system prompt always survives.** It carries identity and durable facts.
- **Tool-call pairs are never split.** An orphaned `tool` message is rejected
  outright by providers, turning a size problem into an immediate 400.
- **The elision is visible.** A marker tells the model history was cut, and the
  CLI tells you.

---

## Self-Learning & Skills

### 1. Persistent Memory
```bash
> "I prefer 2-space tabs and early returns"
# Stored in SQLite, injected into every future prompt automatically
```

### 2. Skills
```bash
# Drop a skill file once:
~/.grace/skills/my-refactor/SKILL.md

# Use it forever:
> "Apply my-refactor to the auth module"
```

Skills are discovered by name + description and loaded **on demand** — a dozen
skills concatenated into every system prompt would be a dozen skills' worth of
tokens spent per turn, nearly all irrelevant.

**3 default skills ship on first run:**
- `grace-agent` — knows its own architecture
- `memory-update` — when to persist facts
- `skill-author` — creates new skills from workflows

### 3. Pre-Flight Recall
Before each turn, Grace searches durable facts, skill descriptions, and past
sessions (FTS5) for genuine keyword overlap and injects what matches. You don't
say "remember X" — it just knows. Deterministic, free, no embedding call.

---

## Architecture

Dependencies point strictly downward. A cycle is a design bug.

```
  ui ─────────────────┐
  config ─────────────┤
  core (agent engine) ┤──►  transport ──►  message
  tools ──────────────┤          │
  memory/session/     │          ▼
  skill/recall  ──────┴───────► util
```

```
src/
├── main.rs             28 lines — arg parsing + dispatch lives in ui/cli.rs
├── core/               the agent engine
│   ├── agent.rs          the ReAct loop
│   ├── delegation.rs     bounded sub-agents
│   ├── context.rs        token-aware compression
│   └── lifecycle.rs      AgentEvent + IterationBudget
├── transport/          LLM providers
│   ├── trait.rs          ProviderTransport, ModelResponse, ToolSpec, FinishReason
│   ├── wire.rs           shared OpenAI request/response shaping
│   ├── http.rs           any OpenAI-compatible endpoint
│   ├── copilot.rs        GitHub Copilot (OAuth device flow + token refresh)
│   └── stream.rs         SSE accumulation
├── tools/              the tool system
│   ├── trait.rs, registry.rs
│   ├── builtins/         terminal, file, patch
│   ├── delegate.rs       sub-agent delegation
│   ├── session.rs        FTS search over past chats
│   └── plugin.rs         executable tools, no rebuild
├── memory/             store.rs (SQLite) + prompt.rs (injection)
├── session/            store.rs, lock.rs (cross-terminal), title.rs
├── skill/              store.rs, load.rs, defaults.rs
├── recall/             pre-flight context injection
├── config/             args.rs, settings.rs, soul.rs
├── ui/                 chat REPL, cli, markdown, skins, wizard
└── util/               error.rs, tokens.rs, diff.rs
```

`core` depends only on `message`, `transport`, `tools`, and `util` — it knows
nothing about the CLI, sessions, or persistence. That is what makes the same
loop usable from a REPL, a one-shot run, and a delegated sub-agent.

---

## Extending Grace

| Add a... | Rebuild? | How |
|----------|----------|-----|
| **Skill** | No | `~/.grace/skills/<name>/SKILL.md` with a `description:` frontmatter line |
| **Skin** | No | `~/.grace/skins/<name>.toml` with 7 RGB fields |
| **Tool** | No | `./tools/<name>/manifest.json` + an executable — see `grace-agent` skill |
| **Built-in tool** | Yes | Implement `Tool` in `src/tools/builtins/`, register in `builtins/mod.rs` |
| **Provider** | Yes | Implement `ProviderTransport` in `src/transport/` |

Tools are registered in exactly one place — `Config::build_registry_full()` —
so every entry point gets the same capability set. Registering from `main.rs`
is how a tool ends up missing everywhere except one code path.

---

## Security (Opt-In Guardrails)

```bash
# File access: only within allowed dirs (resolves `..` before checking)
GRACE_ALLOW_DIR="/home/user/projects" grace --chat

# Commands: allow-list only
GRACE_TERMINAL_ALLOW="ls,cat,rg,cargo" grace --chat

# Or deny specific patterns
GRACE_TERMINAL_DENY="rm -rf,shutdown" grace --chat

# Kill hung commands (default 30s; 0 disables)
GRACE_TERMINAL_TIMEOUT=60 grace --chat
```

Defaults are permissive for local use. Allow-lists are a first step, not a
sandbox — use a container or VM for untrusted models.

---

## Install Options

| Method | Command |
|--------|---------|
| **Install script** | `curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh &#124; bash` |
| **Homebrew** | `brew install grace` *(coming soon)* |
| **Binary** | Download from [releases](https://github.com/AmnGrg0511/grace/releases) |
| **Cargo** | `cargo install grace` |

---

## Development

```bash
cargo test                                  # ~380 tests, no network required
cargo clippy --all-targets -- -D warnings   # clean
cargo build --release
```

Every module carries its own unit tests; `tests/` holds the cross-module
integration suites (`agent_tests`, `tool_tests`, `session_tests`,
`integration`). `tests/live_endpoint.rs` exercises a real provider and is
ignored unless `GRACE_LIVE_BASE_URL` / `GRACE_LIVE_API_KEY` / `GRACE_LIVE_MODEL`
are set.

---

## What's Not Here (By Design)

- **Multi-provider fallback** — compose behind `ProviderTransport` if needed
- **Async runtime** — a single-user CLI doesn't need one; blocking I/O keeps
  the control flow readable
- **TUI** — CLI first. A TUI is a layer on top of `AgentEvent`.
- **Real sandboxing** — allow-lists only. Use a container/VM for untrusted models.
- **Summarizing compression** — compression is structural (drop the middle,
  mark the gap). An LLM summarization pass costs a round-trip and can silently
  rewrite history.

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build/test/release instructions.

---

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
Written by Aman Garg.

---

*Grace is an agent. Not a framework. If you want to understand it, read
`src/core/agent.rs` — the whole loop fits in your head.*
