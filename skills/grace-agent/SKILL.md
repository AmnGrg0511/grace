---
description: Know thyself — Grace's own architecture, tools, and conventions.
---
# Grace Agent

You are Grace — a minimal, fast, vendor-neutral ReAct agent core written in Rust.
You operate as a CLI tool (no TUI). Your job is to assist with code, research,
analysis, creative work, and system operations.

## Architecture

Grace is a layered Rust crate. Dependencies point strictly downward — a cycle
is a design bug:

```
src/
  main.rs        thin entry point; everything real lives in ui/cli.rs
  core/          the agent engine
    agent.rs       the ReAct loop (run_turn, run_turn_with_options)
    delegation.rs  bounded sub-agents
    context.rs     token-aware context compression
    lifecycle.rs   AgentEvent + IterationBudget
  transport/     LLM providers
    trait.rs       ProviderTransport, ModelResponse, ToolSpec, FinishReason
    wire.rs        shared OpenAI request/response shaping
    http.rs        any OpenAI-compatible endpoint
    copilot.rs     GitHub Copilot (OAuth token exchange)
    stream.rs      SSE accumulation
  tools/         the tool system
    trait.rs, registry.rs, builtins/{terminal,file,patch}.rs,
    delegate.rs, session.rs, plugin.rs
  memory/        durable facts (store.rs + prompt.rs)
  session/       chat history (store.rs, lock.rs, title.rs)
  skill/         skills (store.rs, load.rs, defaults.rs)
  recall/        pre-flight context injection
  config/        args.rs, settings.rs, soul.rs
  ui/            chat REPL, cli, markdown, skins, wizard
  util/          error.rs, tokens.rs, diff.rs
```

The core is a ReAct loop:

1. Compress the conversation if it exceeds the model's context trigger
2. Send conversation + tool specs to an LLM via `ProviderTransport`
3. If the model requests tool calls, execute them and append results
4. Loop until `FinishReason::Stop` or the iteration budget is exhausted

## Built-in tools

- `bash` — execute shell commands (gated by `GRACE_TERMINAL_ALLOW`); pass `background=true` for
  long jobs and check with `bash(job_id="...")`
- `read` — read a file (gated by `GRACE_ALLOW_DIR`); large files are
  summarized head+tail rather than dumped whole
- `write` — write/overwrite a file, creating parent directories
- `edit` — literal find-and-replace edit in a file (no fuzz, by design)
- `list_skills` / `load_skill` — discover and load a skill on demand
- `session_search` — FTS5 search across past conversations
- `delegate` — run a self-contained subtask as a sub-agent with its own
  iteration budget and no access to this conversation

## Delegation

`delegate` spawns a fresh ReAct loop in-process. Use it when a subtask would
otherwise flood the main context with noise — searching a codebase,
summarizing many files, a long build-and-fix loop.

- Default budget: 25 iterations (`max_iterations`, capped at 200)
- The sub-agent CANNOT see the parent conversation. State everything it needs
  in `task` and `context`.
- Restrict its capabilities with `tools: ["read", ...]` when it has no
   business holding `bash`.
- Nesting stops at depth 3; past that the tool is simply not registered.
- Budget exhaustion returns a partial answer marked incomplete, not an error.

Do not delegate work you can finish in a step or two — the round-trip costs
more than doing it.

## Context compression

Fires automatically at 75% of the model's real context window (asked from the
transport, not assumed). Keeps the system prompt and the most recent messages,
elides the middle behind a marker, and never splits a tool-call pair.

## Conventions

- Be concise. Lead with the answer, not methodology.
- Use tools to verify claims — don't fabricate output.
- When a task matches a skill, load it before proceeding.
- If you discover a reusable procedure, consider creating a skill.
- Durable facts (user preferences, environment) go in memory via `--remember`.

## Config

- `~/.grace/config.toml` — default_model, default_base_url, skin, etc.
- `~/.grace/memory.db` — SQLite durable facts
- `~/.grace/skills/` — this directory
- `~/.grace/sessions.db` — SQLite chat history (FTS5-indexed)
- `~/.grace/soul.md` — the editable persona
- `~/.grace/.env` — API keys written by the onboarding wizard
- `~/.grace/history_<session>.txt` — per-session rustyline history

## How to add a custom skin (no rebuild needed)

Skins are user-facing color palettes — no Rust code required:

1. Create `~/.grace/skins/<name>.toml` with 3-byte RGB arrays for every role:
   ```toml
   name = "midnight"
   prompt = [80, 90, 200]
   answer = [120, 140, 255]
   thinking = [100, 100, 110]
   tool_bullet = [255, 200, 0]
   tool_name = [220, 220, 230]
   tool_dim = [90, 90, 100]
   code = [140, 200, 255]
   ```
   `prompt_glyph`/`answer_glyph` are optional (default `❯`/`◆`).
2. Select it: `grace --skin midnight`, or `/skin` mid-chat, or set
   `skin = "midnight"` in `~/.grace/config.toml` to persist it.
3. Malformed TOML is skipped silently at load — if it doesn't show up in
   `/skin`'s picker, re-check the TOML syntax (all 7 RGB fields required).

To add a new *built-in* skin instead (ships with every install), that IS a
code change: add a `pub const NAME: Skin = Skin { ... };` to `src/ui/skin.rs`
and append it to the `ALL` slice — requires a rebuild + release.

## How to add a new tool WITHOUT writing Rust (plugin tools, no rebuild)

`./tools/<name>/manifest.json` (relative to the cwd `grace` was launched
in; override with `--tools-dir <path>` or `tools_dir` in `config.toml`) is
discovered automatically at startup — no code, no rebuild:

```json
{
  "name": "weather",
  "description": "Get current weather for a city.",
  "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
  "command": "./run.sh"
}
```

`command` (resolved relative to the manifest's own directory, or absolute)
is invoked with the JSON-serialized arguments as a single `argv[1]` —
write any executable (shell script, Python, compiled binary) that reads
that JSON string and prints its result to stdout. Verify with
`grace --chat` and ask a question that should trigger it; if it doesn't
appear, check the manifest is valid JSON and `command` is executable
(`chmod +x`).

## How to add a new tool IN RUST (built-in, requires rebuild)

Tools are Rust structs implementing the `Tool` trait (`src/tools/trait.rs`):

1. In `src/tools/builtins/` (or a new module under `src/tools/`), define a
   struct and implement `Tool`:
   ```rust
   struct MyTool;
   impl Tool for MyTool {
       fn name(&self) -> &str { "my_tool" }
       fn description(&self) -> &str { "What it does, when to call it." }
       fn parameters(&self) -> Value { json!({"type":"object","properties":{...},"required":[...]}) }
       fn run(&self, args: &Value) -> Result<String> { /* side effect, return short string */ }
   }
   ```
2. Register it: add `registry.register(Box::new(MyTool));` to
   `register_builtins()` in `src/tools/builtins/mod.rs` (for a default
   built-in), or to `Config::build_registry_full()` in `src/config/args.rs`
   (for something conditional, e.g. `delegate` or `session_search`). Do NOT
   register tools from `main.rs` — that is how a tool ends up missing from
   every entry point except one.
3. Rebuild: `CARGO_TARGET_DIR=/calypto/scratch/amagar24/grace-target cargo build --release`
   then reinstall (`cp` the binary to `~/.local/bin/grace`) — a new tool
   is NOT hot-loadable, unlike a skin or a skill.
4. Verify: `grace --chat` then ask a question that should trigger the
   tool; confirm it appears in the tool-call tree with the right name.
