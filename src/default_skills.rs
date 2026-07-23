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
6. Confirm it loads: the user can verify with `grace --mock --chat` then `load_skill <name>`
"#;
