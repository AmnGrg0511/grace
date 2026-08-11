---
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
