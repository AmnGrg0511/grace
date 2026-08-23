---
description: Persist durable facts from the current session to memory.
---
# Memory Update

Use this skill when the user shares a stable fact that should persist across
sessions — a preference, a correction, an environment detail, or a convention.

## When to save

- User states a preference: "I prefer concise responses"
- User corrects your behavior: "Don't use sed, use patch"
- User reveals an environment fact: "My project uses pytest with xdist"
- User establishes a convention: "Always run tests before committing"

## When NOT to save

- Temporary task state (what we're doing right now)
- Ephemeral context (the current file we're editing)
- Things that will be stale in a week (PR numbers, commit SHAs)

## How — run it yourself

Persist the fact directly with your `bash` tool. Do not just mention it in the
conversation, and do not ask the user to do it:

```bash
grace --remember "user prefers concise responses"
```

`grace --remember "<fact>"` is a standalone action: it writes the fact to the
SQLite memory DB at `~/.grace/memory.db` and exits — no model call, no API key
needed. Facts are injected into every future system prompt, and pre-flight
recall picks them up again from the very next turn, so persist the moment the
fact is stable.

## Procedure

1. Identify the durable fact from the conversation.
2. Phrase it declaratively: "User prefers concise responses" (not "Always…").
3. Run `grace --remember "<fact>"` with your `bash` tool.
4. Confirm on the `remembered (id N): <fact>` line it prints; on a failure
   line, retry once, then say so plainly.
