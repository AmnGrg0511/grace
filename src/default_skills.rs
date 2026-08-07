//! Default skills — seeded into `~/.grace/skills/` on first run.
//!
//! These give the agent self-awareness (grace-agent), the ability to
//! consolidate memory from past sessions (memory-update), and a procedure
//! for creating clean new skills (skill-author). They are written as plain
//! markdown files the first time grace starts and `~/.grace/skills/` doesn't
//! exist yet — after that, the user owns them and can edit freely.

use std::path::PathBuf;

/// The default skills root: `~/.grace/skills/`.
pub fn default_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grace")
        .join("skills")
}

/// Seed the three default skills if `~/.grace/skills/` doesn't exist yet.
/// Idempotent — if the directory exists (even if empty), does nothing.
/// Returns the path to the skills root (created if needed).
pub fn ensure_default_skills() -> PathBuf {
    let root = default_root();
    if root.exists() {
        return root;
    }
    let _ = std::fs::create_dir_all(&root);

    let _ = std::fs::create_dir_all(root.join("grace-agent"));
    let _ = std::fs::write(root.join("grace-agent").join("SKILL.md"), GRACE_AGENT);

    let _ = std::fs::create_dir_all(root.join("memory-update"));
    let _ = std::fs::write(
        root.join("memory-update").join("SKILL.md"),
        MEMORY_UPDATE,
    );

    let _ = std::fs::create_dir_all(root.join("skill-author"));
    let _ = std::fs::write(
        root.join("skill-author").join("SKILL.md"),
        SKILL_AUTHOR,
    );

    root
}

const GRACE_AGENT: &str = r#"---
description: Know thyself — Grace's own architecture, tools, and conventions.
---
# Grace Agent

You are Grace — a minimal, fast, vendor-neutral ReAct agent core written in Rust.
You operate as a CLI tool (no TUI). Your job is to assist with code, research,
analysis, creative work, and system operations.

## Architecture

Grace is ~5,400 lines of Rust across 22 modules. The core is a ReAct loop:

1. Send conversation + tool specs to an LLM via `ProviderTransport`
2. If the model requests tool calls, execute them and append results
3. Loop until `FinishReason::Stop` or `max_iterations` is exhausted

## Built-in tools

- `run_terminal` — execute shell commands (gated by `GRACE_TERMINAL_ALLOW`)
- `read_file` — read a file (gated by `GRACE_ALLOW_DIR`)
- `write_file` — write/overwrite a file
- `patch` — find-and-replace edit in a file
- `list_skills` — list available skill names
- `load_skill` — load a skill's SKILL.md into context
- `session_search` — FTS5 search across past conversations

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
- `~/.grace/history.txt` — rustyline chat history

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
code change: add a `pub const NAME: Skin = Skin { ... };` to `src/skin.rs`
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

Tools are Rust structs implementing the `Tool` trait (`src/tool.rs`):

1. In `src/tools.rs` (or a new module), define a struct and implement `Tool`:
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
   `register_builtins()` in `tools.rs` (for a default built-in) or wherever
   the registry is built in `main.rs`/`config.rs` (for something conditional,
   e.g. plugin-style tools like `delegate_tool.rs`/`plugin_tool.rs`).
3. Rebuild: `CARGO_TARGET_DIR=/calypto/scratch/amagar24/grace-target cargo build --release`
   then reinstall (`cp` the binary to `~/.local/bin/grace`) — a new tool
   is NOT hot-loadable, unlike a skin or a skill.
4. Verify: `grace --chat` then ask a question that should trigger the
   tool; confirm it appears in the tool-call tree with the right name.
"#;

const MEMORY_UPDATE: &str = r#"---
description: Consolidate durable facts from the current session into memory.
---
# Memory Update

Use this skill when you detect that the user has shared a stable fact that should
persist across sessions — a preference, a correction, an environment detail, or
a convention.

## When to update memory

- User states a preference: "I prefer concise responses"
- User corrects your behavior: "Don't use sed, use patch"
- User reveals an environment fact: "My project uses pytest with xdist"
- User establishes a convention: "Always run tests before committing"

## When NOT to update memory

- Temporary task state (what we're doing right now)
- Ephemeral context (the current file we're editing)
- Things that will be stale in a week (PR numbers, commit SHAs)

## How

The CLI has a `--remember "<fact>"` flag that persists a fact to the SQLite
memory DB at `~/.grace/memory.db`. Those facts are injected into every system
prompt automatically.

In chat mode, you can suggest the user run:
```
grace --remember "user prefers X"
```

Or if the `write_file` tool is available and `~/.grace/memory.db` is within
the allow-listed path, you can note the fact in the conversation and remind
the user to persist it.

## Procedure

1. Identify the durable fact from the conversation
2. Phrase it declaratively: "User prefers concise responses" (not "Always...")
3. Suggest the user persist it, or persist it yourself if tools allow
4. Confirm it was saved
"#;

const SKILL_AUTHOR: &str = r#"---
description: Create clean, well-structured new skills from a procedure or workflow.
---
# Skill Author

Use this skill when the user has completed a non-trivial task (5+ tool calls,
errors overcome, a workflow discovered) and you think the approach is reusable.

## When to create a skill

- A complex task succeeded after 5+ tool calls
- An error was overcome with a specific fix or workaround
- A non-obvious workflow was discovered
- The user explicitly asks to "remember how to do this"

## When NOT to create a skill

- Simple one-off tasks (single tool call)
- Tasks that are trivially discoverable
- Things specific to a single file or project

## Skill format

Each skill is a directory under `~/.grace/skills/<name>/SKILL.md`:

```markdown
---
description: One-line summary of what the skill does.
---
# Skill Name

## Trigger conditions
When to use this skill.

## Steps
1. Numbered steps with exact commands
2. Each step is actionable, not descriptive

## Pitfalls
- Common mistakes and how to avoid them

## Verification
How to confirm the task was done correctly.
```

## Procedure

1. Confirm the task is worth saving as a skill
2. Determine the skill name (lowercase, hyphenated, max 64 chars)
3. Write the SKILL.md following the format above
4. Create the directory: `~/.grace/skills/<name>/`
5. Write the file: `~/.grace/skills/<name>/SKILL.md`
6. Confirm it loads: the user can verify with `grace --chat` then `load_skill <name>`
"#;
