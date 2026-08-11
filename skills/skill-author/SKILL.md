---
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
