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

/// The skills shipped with Grace, as `(name, SKILL.md contents)`.
pub const DEFAULT_SKILLS: &[(&str, &str)] = &[
    ("grace-agent", GRACE_AGENT),
    ("memory-update", MEMORY_UPDATE),
    ("skill-author", SKILL_AUTHOR),
];

/// Seed the default skills if `~/.grace/skills/` doesn't exist yet.
///
/// Idempotent, and keyed on the *directory* rather than each file: once the
/// root exists the user owns it, so a skill they deliberately deleted must
/// not silently reappear on the next run.
pub fn ensure_default_skills() -> PathBuf {
    let root = default_root();
    seed_into(&root);
    root
}

/// [`ensure_default_skills`] against an explicit root — the seam that makes
/// seeding testable without writing into the real `~/.grace`.
///
/// Returns `true` if it actually seeded, `false` if the root already existed.
pub fn seed_into(root: &std::path::Path) -> bool {
    if root.exists() {
        return false;
    }
    if std::fs::create_dir_all(root).is_err() {
        return false;
    }
    for (name, body) in DEFAULT_SKILLS {
        let dir = root.join(name);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("SKILL.md"), body);
    }
    true
}

const GRACE_AGENT: &str = r#"---
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

Or if the `write` tool is available and `~/.grace/memory.db` is within
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::SkillStore;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "grace_defaults_test_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn seeding_writes_all_three_default_skills() {
        let root = scratch("seed");
        assert!(seed_into(&root));
        for (name, _) in DEFAULT_SKILLS {
            assert!(
                root.join(name).join("SKILL.md").is_file(),
                "{name} was not seeded"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seeded_skills_are_discoverable_by_the_skill_store() {
        // Writing files nobody can find would be a silent no-op.
        let root = scratch("discover");
        seed_into(&root);
        let names = SkillStore::new(&root).list();
        assert_eq!(names, vec!["grace-agent", "memory-update", "skill-author"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_default_skill_has_a_parseable_description() {
        // Recall matches on descriptions; one missing means that skill is
        // effectively invisible to the pre-flight pass.
        let root = scratch("descriptions");
        seed_into(&root);
        for meta in SkillStore::new(&root).list_meta() {
            assert_ne!(
                meta.description, meta.name,
                "{} fell back to its name — frontmatter is missing or malformed",
                meta.name
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seeding_is_idempotent_and_never_resurrects_a_deleted_skill() {
        // Once the root exists the user owns it. A skill they deliberately
        // removed must stay removed.
        let root = scratch("idempotent");
        assert!(seed_into(&root));
        std::fs::remove_dir_all(root.join("skill-author")).unwrap();

        assert!(!seed_into(&root), "second seed should be a no-op");
        assert!(
            !root.join("skill-author").exists(),
            "a deleted skill must not reappear"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn seeding_does_not_overwrite_user_edits() {
        let root = scratch("useredits");
        seed_into(&root);
        let edited = root.join("grace-agent").join("SKILL.md");
        std::fs::write(&edited, "my own version").unwrap();
        seed_into(&root);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "my own version");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_default_root_lives_under_dot_grace() {
        assert!(default_root().ends_with(".grace/skills"));
    }
}
