# Grace Minimalism Audit (v0.1.9 → v0.2.0)

Classification of all 23 modules against the standing directive: *"a fast
performant agent core without the unnecessary features, for core developers
only, CLI based."*

## Core (essential ReAct loop, transport, tools, config) — keep

| Module | LOC | Why core |
|---|---|---|
| `agent.rs` | 347 | The ReAct loop itself. Irreducible. |
| `transport.rs` | 169 | `ProviderTransport` trait — the vendor-neutral abstraction. |
| `transport_http.rs` | 245 | OpenAI-compatible HTTPS — the one transport every user needs. |
| `tool.rs` | 72 | `Tool` trait + registry. |
| `tools.rs` | 414 | terminal/read/write/patch — the actual working tools. |
| `message.rs` | 143 | Conversation record — source of truth. |
| `config.rs` | 264 | Runtime wiring (transport selection, system prompt). |
| `error.rs` | 74 | Single error type. |
| `lib.rs` | 56 | Module wiring / doc root. |
| `main.rs` (CLI parsing + one-shot + chat loop skeleton) | ~400 of 1584 | Entry point. |

## Polish (nice, not essential) — flag for removal or de-scope

| Module | LOC | Verdict |
|---|---|---|
| `markdown.rs` | 586 | Biggest single "nice to have." A CLI for *core developers* doesn't need syntax-highlighted markdown rendering with box-drawing tables — plain text or a much smaller renderer suffices. **Recommend: cut to a ~80-line renderer (bold/italic/code fences only, drop tables+box-drawing+syntect) or make it an opt-in feature flag.** |
| `skin.rs` + 4 built-in skins | 249 | 4 skins + custom TOML loader is UI polish for a dev tool that's typically read via `less`/logs. **Recommend: cut to one fixed palette (no picker, no custom TOML).** Removes `anstyle-query` dependency surface too. |
| `completer.rs` | 101 | rustyline tab-completion for slash commands — nice, skippable. **Recommend: keep only if `/`-command surface stays; otherwise cut with the slash-command layer.** |
| Two-level model/skin/session pickers in `main.rs` (~350 LOC) | — | Onboarding wizard + `/model`, `/skin`, `/session` interactive pickers. A core-developer CLI is more naturally configured via flags/config.toml than an interactive wizard. **Recommend: keep flags/config.toml, cut the multi-step interactive prompts** (or gate behind `--wizard`). |

## Bloat (violates "core developers only, CLI based") — recommend cut

| Module | LOC | Why bloat |
|---|---|---|
| `session.rs` (SQLite+FTS5 chat history) | 261 | A durable multi-session chat history with full-text search is agent-*framework* territory, not a minimal core. Core devs re-run one-shot prompts; they don't need `/session` switching. **Recommend: cut**, or reduce to "last N messages in a flat file" if any persistence is kept. |
| `memory.rs` (SQLite durable facts) | 237 | Same category — durable cross-run memory + markdown mirror + wikilink resolution is a product feature, not core-loop plumbing. **Recommend: cut.** |
| `recall.rs` (pre-flight keyword recall over memory+skills+sessions) | 217 | Only exists to serve `memory.rs`/`session.rs` — cut together with them. |
| `default_skills.rs` (seeds 3 skills on first run) | 188 | Depends on `skill.rs` staying, but the *seeding* behavior (writing markdown to `~/.grace/skills/` automatically) is unnecessary magic for a minimal core. **Recommend: cut the auto-seed; ship skill docs in the README instead.** |
| `skill.rs` (skill loading itself) | 207 | Borderline — filesystem-convention skill loading is genuinely useful and small. **Recommend: keep**, but drop `default_skills.rs`'s auto-seeding. |
| `plugin_tool.rs` (external tool plugins via manifest.json) | 195 | A plugin system is real framework surface. Core developers can add a tool by writing a `Tool` impl and recompiling — that's the "core" way. **Recommend: cut** unless there's a concrete external-tool use case today. |
| `delegate_tool.rs` (subagent spawning) | 117 | Spawns a fresh `grace` subprocess to delegate a subtask. Useful, but it's meta-orchestration on top of the core loop, not the loop itself. **Recommend: cut**, or keep only if subagent delegation is an active daily workflow. |
| `transport_copilot.rs` (GitHub Copilot device flow) | 379 | A second full transport implementation (OAuth device flow, token caching, model listing) just for one vendor. Vendor-neutral core should have exactly one HTTP transport; GitHub-Copilot-specific auth flow is vendor lock-in in the other direction. **Recommend: cut**, or fold into `transport_http.rs` as a thin auth-header variant if Copilot support must stay. |
| `transport_stream.rs` (SSE streaming, one-shot only) | 228 | Only wired to one-shot mode (`--stream`), not chat — half-finished feature surface. **Recommend: cut until it's wired to chat mode too**, or finish the wiring (Phase 3 item 7) and keep. |
| Context compression (`ContextCompressionConfig` in `agent.rs`/`config.rs`) | ~60 | Uses a hardcoded `128_000` fallback context window (agent.rs:158) instead of the real model's window — currently half-correct. Compression logic itself is reasonable but adds a knob most core-dev usage (bounded one-shot prompts) never needs. **Recommend: cut**, or fix the hardcoded window and keep as opt-in. |

## Net effect if all "Bloat" cuts are taken

Removes: `session.rs`, `memory.rs`, `recall.rs`, `default_skills.rs`,
`plugin_tool.rs`, `delegate_tool.rs`, `transport_copilot.rs`,
`transport_stream.rs`, plus the compression path in `agent.rs`/`config.rs`.

Estimated LOC removed: ~2,100 of 6,640 (≈32%). Remaining core: ReAct loop,
one HTTP transport, 4 built-in tools, skill loading (no auto-seed),
markdown rendering trimmed, one fixed skin — a CLI that does exactly what
"agent core for developers" promises, nothing else.

**This is a recommendation, not applied** — cutting `session.rs`/`memory.rs`
would break `session_search`, `--remember`, and the durable-memory pitch in
the README, which the user may still want. Flagging for an explicit decision
rather than deleting working, tested code.
