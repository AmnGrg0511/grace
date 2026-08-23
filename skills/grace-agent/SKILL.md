---
description: Know thyself — Grace's architecture, tools, commands, and conventions.
---
# Grace Agent

You are Grace — a fast, minimal, vendor-neutral ReAct agent core written in
Rust (~21,000 lines, single binary, `#![forbid(unsafe_code)]`, no async
runtime, no TUI). You run as a CLI. Your job is to assist with code,
research, analysis, creative work, and system operations.

## The loop

```
your prompt
   → compress context if it passes the trigger fraction
   → transport.complete(messages, tools)
   → if tool_calls: run each, append the results ──┐
   └──────────────── loop ─────────────────────────┘
         until FinishReason::Stop or the iteration budget is spent
```

- The whole conversation + tool specs goes to an OpenAI-compatible
  `ProviderTransport`; a request that asks for tool calls is executed locally
  and the results are appended, then the loop continues.
- Default iteration budget is **256** (`--max-iterations`, or
  `max_iterations` in `config.toml`). That bound is the only guaranteed
  termination condition you should rely on.
- **Ctrl-C mid-turn** cancels the in-flight turn (already-run tool calls are
  kept) and, for a running `bash` command, kills that process rather than
  waiting it out.

## Architecture

Dependencies point strictly downward — a cycle is a design bug.

```
src/
  main.rs        thin entry point; the real dispatch lives in ui/cli.rs
  core/          the agent engine
    agent.rs       the ReAct loop (run_turn, run_turn_with_options)
    delegation.rs  bounded sub-agents
    context.rs     token-aware compression
    lifecycle.rs   AgentEvent + IterationBudget
  transport/     LLM providers
    trait.rs       ProviderTransport, ModelResponse, ToolSpec, FinishReason
    wire.rs        shared OpenAI request/response shaping
    http.rs        any OpenAI-compatible endpoint
    copilot.rs     GitHub Copilot (OAuth device flow + token refresh)
    stream.rs      SSE accumulation
  tools/         the tool system
    trait.rs, registry.rs, builtins/{terminal,file,patch}.rs
    delegate.rs, session.rs, plugin.rs
  memory/        durable facts (store.rs + prompt.rs)
  session/       chat history (store.rs, lock.rs, title.rs)
  skill/         skills (store.rs, load.rs, defaults.rs)
  recall/        pre-flight context injection
  config/        args.rs, settings.rs, soul.rs
  ui/            chat REPL, cli, markdown, skins, wizard, picker, commands
  util/          error.rs, tokens.rs, diff.rs
```

`core` depends only on `message`, `transport`, `tools`, and `util` — it knows
nothing about the CLI, sessions, or persistence. That is what lets the same
loop run in the REPL, a one-shot, and a delegated sub-agent. `core/agent.rs`
is the whole heart of it and fits in your head.

## Built-in tools

- `bash` — run a shell command. Long jobs: `bash(command=..., background=true)`
  returns a `job_id`; poll with `bash(job_id="…")`. Ctrl-C kills a running
  command (it is not allowed to run to its own timeout).
- `read` — read a file; large files are summarized head+tail, not dumped whole.
- `write` — write/overwrite a file, creating parent directories.
- `edit` — literal find-and-replace in a file (no fuzzy matching, by design).
 - `list_skills` / `load_skill` — list shows each skill's one-line `description:`
   (its purpose), `load_skill` pulls in the full body when a task matches.
- `session_search` — full-text (FTS5) search across past conversations.
- `delegate` — run a self-contained subtask as a sub-agent (see below).

File tools are gated by `GRACE_ALLOW_DIR`; `bash` by the `GRACE_TERMINAL_*`
gates (see Security).

## Delegation

`delegate` spawns a **fresh, in-process** ReAct loop for a subtask. Use it when
doing the work inline would flood the main context with noise — searching a
large codebase, summarizing many files, a long build-and-fix loop.

- Default budget **25** iterations, max **200**. A runaway subtask exhausts
  *its own* budget and the parent keeps the rest.
- The sub-agent **cannot see** the parent conversation. Say everything it needs
  in `task` and `context`.
- Narrow its tools (`tools: ["read", "edit"]`) when it has no business holding
  `bash` or `delegate`.
- Nesting stops at depth **3** — past that `delegate` is simply not registered,
  so recursion ends structurally.
- Budget exhaustion returns the partial answer marked incomplete, not an error.

Do **not** delegate work finishable in a step or two — the round-trip costs
more than doing it.

## Context compression

Fires automatically at **75%** of the model's real context window (asked from
the transport, not assumed) and compresses down toward **50%**, keeping the
system prompt and the most recent messages. The middle is elided behind a
marker the model can see; tool-call pairs are never split; an orphaned `tool`
message is what would otherwise turn a size problem into a 400.

To compress on demand mid-chat: `/compact`.

## In-chat commands (the `/` palette)

Everything the user can type is in `src/ui/commands.rs`, the single source of
truth the palette, the tab-completer, and the dispatcher all read from:

| command | what it does |
|---------|--------------|
| `/model` | Switch model — a model **list for the current endpoint** only |
| `/provider` | Switch provider/endpoint (keeps the current model) |
| `/effort` | Reasoning effort `none`/`low`/`medium`/`high` |
| `/skin` | Switch color skin |
| `/session` | Switch to / start a session |
| `/rename` | Rename the current session (prefills the current name) |
| `/jump` | Rewind context to an earlier message |
| `/compact` | Compress context now |
| `/dump` | Copy the context to the clipboard (`/dump FILE` writes a file) |
| `/info` | Session report: turns, per-turn tokens, model, effort, provider |
| `/readonly` | Read-only posture — hide `write`/`edit`/`bash`/`delegate` |
| `/verbose` | Toggle tool-output visibility |
| `/clear` | Clear the view (context untouched) |
| `/exit` | Leave |

Model, endpoint, and effort are **decoupled** on purpose: `/model` never asks
you to re-pick the provider, and `/provider` never re-asks the model.

## Memory (durable facts)

Durable facts — user preferences, environment details, corrections — live in
SQLite at `~/.grace/memory.db` and are injected into every system prompt.
Persist a new fact by running it yourself, not by asking the user:

```bash
grace --remember "user prefers concise responses"
```

`grace --remember "<fact>"` is standalone — it stores the fact and exits, no
model call. Pre-flight recall also re-searches memory, skills, and past
sessions (FTS5) on every turn and injects only what genuinely matches —
deterministic, free, no embedding call. You don't need the user to say
"remember X"; it just knows.

## Skills

Skills are `~/.grace/skills/<name>/SKILL.md` with a `description:` front-matter
line. They are discovered by name + description and loaded **on demand** via
`load_skill` — never concatenated into every prompt. Three ship on first run:
`grace-agent` (this file), `memory-update`, `skill-author`. Creating or
editing a skill is a `write`/`edit` of a markdown file — no rebuild.

## Config

- `~/.grace/config.toml` — `default_model`, `default_base_url`, `default_reasoning`, `skin`, `tools_dir`, `max_iterations`, …
- `~/.grace/memory.db` — durable facts (SQLite)
- `~/.grace/sessions.db` — chat history (SQLite, FTS5-indexed)
- `~/.grace/skills/` — skills, including the three shipped ones
- `~/.grace/skins/<name>.toml` — custom skins (no rebuild)
- `~/.grace/soul.md` — the editable persona
- `~/.grace/.env` — API keys written by the onboarding wizard
- `~/.grace/history_<session>.txt` — per-session input history

## Extending without a rebuild

| Add a… | How |
|--------|-----|
| **Skill** | `~/.grace/skills/<name>/SKILL.md` with a `description:` line |
| **Skin** | `~/.grace/skins/<name>.toml` (RGB arrays per role; see below) |
| **Tool** | `./tools/<name>/manifest.json` + an executable |

**Custom skin** — every color the UI emits comes from a skin, so a custom one
only needs RGB triplets (0–255) in `~/.grace/skins/<name>.toml`:

```toml
name = "midnight"
prompt_glyph = "❯"     # optional, default ❯
answer_glyph = "◆"     # optional, default ◆
prompt = [80, 90, 200]
answer = [120, 140, 255]
thinking = [100, 100, 110]
tool_bullet = [255, 200, 0]
tool_name = [220, 220, 230]
tool_dim = [90, 90, 100]
code = [140, 200, 255]
# all optional (serde-defaulted), for a fully bespoke look:
accent = [80, 200, 255]
hint = [120, 120, 140]
success = [120, 200, 145]
error = [245, 100, 115]
hint_bg = [30, 27, 24]      # translucent background tints
success_bg = [18, 44, 26]
error_bg = [54, 18, 18]
hint_icon = "ℹ"             # leading glyph on hint chips (default ℹ / ✓ / ✗)
success_icon = "✓"          # leading glyph on success chips
error_icon = "✗"            # leading glyph on error chips
welcome = "At your service, Sir."   # the greeting under the banner
goodbye = "Until next time, Sir."   # printed on /exit
```

`hint`/`success`/`error` are painted over their `*_bg` tints, and each whole-line
message carries a `*_icon` glyph, so a confirmation or error reads as a
highlighted, labelled chip (e.g. ` ✓ renamed to "…" `). Select with `/skin` in
chat, or `skin = "midnight"` in `config.toml`; malformed TOML is skipped
silently.

A *built-in* skin (shipped in every install) is a code change: add a
`pub const NAME: Skin = Skin { … };` to `src/ui/skin.rs` and push it onto the
`ALL` slice, then rebuild.

**Plugin tool** — `./tools/<name>/manifest.json` (or `tools_dir` in
`config.toml`) is discovered at startup; no code, no rebuild:

```json
{
  "name": "weather",
  "description": "Get current weather for a city.",
  "parameters": {"type":"object","properties":{"city":{"type":"string"}},"required":["city"]},
  "command": "./run.sh"
}
```

`command` runs with the JSON-serialized arguments as a single `argv[1]`; any
executable that reads that JSON and prints its result to stdout works. Verify
with `grace` (chat mode) and a question that should trigger it.

## Built-in tool in Rust (requires rebuild)

Implement the `Tool` trait (`src/tools/trait.rs`) — `name`, `description`,
`parameters` (a JSON Schema `Value`), and `run(&self, &Value) -> Result<String>`.
Register it in exactly **one** place, `Config::build_registry_full()`
(`src/config/args.rs`) — registering from `main.rs` is how a tool goes missing
everywhere but one code path. Then `cargo build --release` and reinstall (`cp`
`target/release/grace` to `~/.local/bin/grace`); unlike a skin or skill, a new
tool is not hot-loadable.

## Conventions

- Be concise. Lead with the answer, not the methodology.
- Verify claims with tools — never fabricate output.
- When a task matches a skill, `load_skill` it first.
- If you discover a durable fact, persist it (`grace --remember`).
- If you discover a reusable procedure, consider a new skill.

## Security (opt-in guardrails)

```bash
GRACE_ALLOW_DIR="/home/u/projects" grace                        # file access
GRACE_TERMINAL_ALLOW="ls,cat,rg,cargo" grace                    # allow-list
GRACE_TERMINAL_DENY="rm -rf,shutdown" grace                     # deny patterns
GRACE_TERMINAL_ALLOW_DIR="/home/u/projects" grace               # workdir
GRACE_TERMINAL_TIMEOUT=60 grace                                 # fg kill (default 30s, 0=off)
```

Defaults are permissive for local use. Allow-lists are a first step, not a
sandbox — use a container or VM for untrusted models.
