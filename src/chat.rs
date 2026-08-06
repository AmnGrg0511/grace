//! Interactive chat REPL: `run_chat`, per-turn execution, slash-command
//! handlers, and the shared event/status-line formatting used by both
//! chat and one-shot modes.

use grace::config::ContextCompressionConfig;
use grace::message::Message;
use grace::session::SessionStore;
use grace::settings::PROVIDER_PRESETS;
use grace::skin::{Role, Skin};
use uuid::Uuid;

use crate::line_reader::LineReader;

pub(crate) const RESET: &str = "\x1b[0m";

/// Interactive REPL. Each line you type is appended as a user message and the
/// conversation history (including tool calls) is preserved across turns. If
/// a session id was given, each turn is also persisted to disk immediately.
///
/// Owns exactly one [`LineReader`] for the whole session (see that module's
/// docs for why: two independent stdin readers used to race and steal each
/// other's lines whenever a picker like `/session` was invoked).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_chat(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    skin: &Skin,
    compression_config: &ContextCompressionConfig,
) {
    // Owned+mutable so `/skin <name>` can swap it live; `/model <name>` swaps
    // the transport's own interior model instead (see `set_model`).
    let mut skin = *skin;
    // Owned so `/session <name>` can switch mid-chat.
    let mut current_session: Option<String> = session_id.map(|s| s.to_string());

    // Ctrl-C mid-turn cancels the current turn (tool calls already run stay
    // recorded) and returns to the prompt, instead of killing the whole
    // process — installed once for the process, the flag is what the agent
    // loop polls between steps. rustyline handles Ctrl-C at the idle prompt
    // itself (returns `ReadlineError::Interrupted`, handled inside
    // `LineReader::read_line`) independent of this signal handler.
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        let _ = ctrlc::set_handler(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    println!("chat mode — type a message, '/exit' to leave, '/model [name]' to switch models, '/skin [name]' to retheme, '/session' to switch sessions.\n");

    let started_at = std::time::Instant::now();
    // Loaded once per chat session (not re-read from disk every turn) —
    // `/model` updates this in-memory copy too so the status bar reflects
    // a mid-chat model switch without another disk read.
    let mut cached_context_window = grace::settings::Settings::load().default_context_window;

    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("history.txt");
    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Single stdin owner for the entire interactive session — every picker
    // below (`/model`, `/skin`, `/session`) takes `&mut reader` instead of
    // opening its own `std::io::stdin()`.
    let mut reader = LineReader::new(history_path);
    let is_rustyline = reader.is_interactive_editor();

    loop {
        print_status_line(&skin, transport, messages, started_at, cached_context_window);
        // rustyline draws its own prompt glyph via readline(prompt); the
        // plain fallback prints it manually inside LineReader::read_line.
        let Some(line) = reader.read_line(&prompt_label(&skin)) else {
            if !is_rustyline {
                // Plain fallback: blank line before exit for parity with the
                // old loop's trailing newline behavior on EOF.
            }
            break;
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if matches!(text, "/exit" | "/quit") {
            println!("goodbye.");
            break;
        }
        if text.starts_with("/help") || text.starts_with("/commands") {
            print_slash_commands_help();
            continue;
        }
        if let Some(rest) = text.strip_prefix("/model") {
            handle_model_command(transport, rest.trim(), &mut reader);
            cached_context_window = grace::settings::Settings::load().default_context_window;
            continue;
        }
        if let Some(rest) = text.strip_prefix("/skin") {
            handle_skin_command(rest.trim(), &mut skin, &mut reader);
            continue;
        }
        if let Some(rest) = text.strip_prefix("/session") {
            handle_session_command(rest.trim(), sessions, messages, &mut current_session, &mut reader);
            continue;
        }
        run_one_chat_turn(
            transport,
            tools,
            messages,
            max_iterations,
            sessions,
            current_session.as_deref(),
            text,
            &skin,
            &interrupted,
            compression_config,
        );
    }
}

/// `/model` (interactive picker, same list as onboarding) or `/model <name>`
/// (direct switch) mid-chat. Persists to ~/.grace/config.toml so the choice
/// sticks across restarts (unlike the old session-only behavior).
/// Only takes effect on transports that own a swappable model (`HttpTransport`).
fn handle_model_command(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    arg: &str,
    reader: &mut LineReader,
) {
    if transport.current_model().is_none() {
        println!(
            "this transport ({}) has no switchable model.",
            transport.name()
        );
        return;
    }
    let (picked, ctx) = if arg.is_empty() {
        match pick_model_interactive(reader) {
            Some(result) => result,
            None => return,
        }
    } else {
        (arg.to_string(), None)
    };
    transport.set_model(&picked);
    if let Some(m) = transport.current_model() {
        let mut settings = grace::settings::Settings::load();
        settings.default_model = Some(m.clone());
        settings.default_context_window = ctx.or_else(|| {
            // Direct-typed model: try fetching context window if we can
            // reach the provider (base_url from current settings).
            settings.default_base_url.as_ref().and_then(|url| {
                let key = std::env::var("GRACE_API_KEY")
                    .or_else(|_| std::env::var("OPENAI_API_KEY"))
                    .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
                    .or_else(|_| std::env::var("GITHUB_COPILOT_TOKEN"))
                    .unwrap_or_default();
                fetch_context_window(&picked, url, &key)
            })
        });
        if let Err(e) = settings.save() {
            eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
        } else {
            println!("model switched to \"{m}\" (saved to config).");
        }
    }
}

/// Two-level model picker: providers first, then models for that provider.
/// Returns `(model_id, optional_context_window)`. Used by `/model` mid-chat.
/// Returns `None` on unparsable/EOF input (no-op).
fn pick_model_interactive(reader: &mut LineReader) -> Option<(String, Option<u32>)> {
    println!("\nproviders:\n");
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let n_providers = PROVIDER_PRESETS.len();
    let raw = reader.read_line("\nselect a provider [number]: ")?;
    let choice: usize = match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= n_providers => n - 1,
        _ => {
            println!("not a valid choice.");
            return None;
        }
    };
    let preset = &PROVIDER_PRESETS[choice];
    if preset.models.is_empty() {
        // Provider with no known models (e.g. "Custom endpoint"): type one.
        let typed = reader.read_line("model id: ")?.trim().to_string();
        return if typed.is_empty() { None } else { Some((typed, None)) };
    }
    println!("\n{label} models:\n", label = preset.label);
    for (i, m) in preset.models.iter().enumerate() {
        println!(
            "  {i}) {}  ({}k ctx)",
            m.id,
            m.context_window / 1000,
            i = i + 1
        );
    }
    // Add "other" option for custom model ID
    println!("  {}) other (type a model id)", preset.models.len() + 1);
    let n_models = preset.models.len();
    let raw = reader.read_line("\nselect a model [number]: ")?;
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= n_models => Some((
            preset.models[n - 1].id.to_string(),
            Some(preset.models[n - 1].context_window),
        )),
        Ok(n) if n == n_models + 1 => {
            // Custom model ID
            let typed = reader.read_line("model id: ")?.trim().to_string();
            if typed.is_empty() { None } else { Some((typed, None)) }
        }
        _ => {
            println!("not a valid choice.");
            None
        }
    }
}

/// `/skin` (interactive picker, same as `--select-skin`) or `/skin <name>`
/// (direct switch) mid-chat. Session-only — use `--select-skin` to persist
/// a default across runs.
fn handle_skin_command(arg: &str, skin: &mut Skin, reader: &mut LineReader) {
    let names = grace::skin::all_names();
    let picked = if arg.is_empty() {
        match pick_skin_interactive(&names, reader) {
            Some(n) => n,
            None => return,
        }
    } else if names.iter().any(|n| n == arg) {
        arg.to_string()
    } else {
        println!("unknown skin \"{arg}\" — available: {}", names.join(", "));
        return;
    };
    *skin = grace::skin::by_name(Some(&picked));
    let mut settings = grace::settings::Settings::load();
    settings.skin = Some(picked.clone());
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    } else {
        println!("skin switched to \"{picked}\" (saved to config).");
    }
}

/// `/session` — switch, list, or clear session mid-chat.
/// - `/session` (no arg): interactive picker (lists recent sessions)
/// - `/session <name>`: switch to that session (loads history)
/// - `/session new`: start a fresh unnamed session (clears in-memory history)
/// - `/session new-persist`: start a fresh, immediately-persisted session
/// - `/session none`: disable session persistence for the rest of the chat
fn handle_session_command(
    arg: &str,
    sessions: &SessionStore,
    messages: &mut Vec<Message>,
    current_session: &mut Option<String>,
    reader: &mut LineReader,
) {
    if arg.is_empty() {
        // Interactive: list recent sessions and let the user pick.
        let session_list = match sessions.list_sessions() {
            Ok(list) => list,
            Err(e) => {
                println!("error listing sessions: {e}");
                return;
            }
        };
        if session_list.is_empty() {
            println!("no saved sessions yet.");
            return;
        }
        println!("\nsaved sessions (most recent first):\n");
        for (i, sid) in session_list.iter().enumerate() {
            let preview = sessions
                .load(sid)
                .ok()
                .and_then(|m| m.first().map(|m| m.content.clone()))
                .unwrap_or_default();
            let preview = preview.chars().take(50).collect::<String>();
            println!("  {}) {}  {}", i + 1, sid, preview);
        }
        let Some(raw) = reader.read_line("\nselect a session [number, or 0 for new]: ") else {
            println!("no input — staying on current session.");
            return;
        };
        match raw.trim().parse::<usize>() {
            Ok(0) => {
                messages.clear();
                let new_id = Uuid::new_v4().to_string();
                // Persist the empty session immediately so it survives restarts
                if let Err(e) = sessions.append(&new_id, &Message::user("")) {
                    println!("warning: could not persist new session: {e}");
                }
                *current_session = Some(new_id.clone());
                println!("started fresh persisted session: {new_id}.");
            }
            Ok(n) if n >= 1 && n <= session_list.len() => {
                let sid = &session_list[n - 1];
                match sessions.load(sid) {
                    Ok(loaded) => {
                        messages.clear();
                        messages.extend(loaded);
                        *current_session = Some(sid.clone());
                        println!("switched to session \"{sid}\" ({} messages loaded).", messages.len());
                    }
                    Err(e) => println!("error loading session: {e}"),
                }
            }
            _ => println!("not a valid choice — staying on current session."),
        }
        return;
    }

    match arg {
        "new" => {
            messages.clear();
            *current_session = None;
            println!("started a fresh session (no persistence).");
        }
        "new-persist" => {
            let new_id = uuid::Uuid::new_v4().to_string();
            messages.clear();
            *current_session = Some(new_id.clone());
            // Persist the empty session immediately
            if let Err(e) = sessions.append(&new_id, &Message::user("")) {
                println!("warning: could not persist new session: {e}");
            }
            println!("started new persisted session: {new_id}");
        }
        "none" => {
            *current_session = None;
            println!("session persistence disabled for this chat.");
        }
        "list" => {
            match sessions.list_sessions() {
                Ok(list) if list.is_empty() => println!("no saved sessions yet."),
                Ok(list) => {
                    println!("\nsaved sessions (most recent first):\n");
                    for sid in &list {
                        let marker = if current_session.as_deref() == Some(sid.as_str()) {
                            " (current)"
                        } else {
                            ""
                        };
                        println!("  {sid}{marker}");
                    }
                }
                Err(e) => println!("error listing sessions: {e}"),
            }
        }
        name => {
            match sessions.load(name) {
                Ok(loaded) => {
                    messages.clear();
                    messages.extend(loaded);
                    *current_session = Some(name.to_string());
                    println!("switched to session \"{name}\" ({} messages loaded).", messages.len());
                }
                Err(e) => {
                    println!("session \"{name}\" not found ({}). Starting fresh.", e);
                    messages.clear();
                    *current_session = Some(name.to_string());
                }
            }
        }
    }
}

/// Shared skin list+preview+select flow, identical presentation to
/// `--select-skin` so muscle memory carries over between startup and
/// mid-chat. Returns `None` on unparsable/EOF input (no-op).
pub(crate) fn pick_skin_interactive(names: &[String], reader: &mut LineReader) -> Option<String> {
    println!("\navailable skins:\n");
    for (i, name) in names.iter().enumerate() {
        let s = grace::skin::by_name(Some(name));
        // A real mini-transcript, not a flat swatch: prompt glyph, a tool-call
        // header, and an answer with inline code — exercises every role
        // color at once using actual Grace tools (terminal, ls).
        println!(
            "  {a}{i}) {name}{r}\n     {p}{glyph} you{r}\n       {tb}●{r} {tn}terminal{r}(list files)  {td}⎿ file1.txt  file2.txt{r}\n     {a}{ag}{r}  Use {c}ls{r} for directory listings.",
            i = i + 1,
            name = name,
            p = s.style(Role::Prompt).render(),
            glyph = s.prompt_glyph,
            r = RESET,
            tb = s.style(Role::ToolBullet).render(),
            tn = s.style(Role::ToolName).render(),
            td = s.style(Role::ToolDim).render(),
            a = s.style(Role::Answer).render(),
            ag = s.answer_glyph,
            c = s.style(Role::Code).render(),
        );
    }
    let raw = reader.read_line("\nselect a skin [number]: ")?;
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= names.len() => Some(names[n - 1].clone()),
        _ => {
            println!("not a valid choice — leaving skin unchanged.");
            None
        }
    }
}

/// One user turn: append the user message, run it, print/persist the
/// answer. Shared by both the rustyline and plain-stdin chat loops so the
/// turn logic isn't duplicated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_one_chat_turn(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    text: &str,
    skin: &Skin,
    interrupted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    compression_config: &ContextCompressionConfig,
) {
    messages.push(Message::user(text.to_string()));
    if let Some(sid) = session_id {
        let _ = sessions.append(sid, &Message::user(text.to_string()));
    }
    // Clear any interrupt latched from a previous turn before starting a new
    // one — otherwise a stale Ctrl-C would abort every turn from here on.
    interrupted.store(false, std::sync::atomic::Ordering::SeqCst);
    match grace::agent::run_turn_with_events(
        transport,
        tools,
        messages,
        max_iterations,
        Some(&mut |event| print_agent_event(event, skin)),
        Some(interrupted.as_ref()),
        Some(compression_config),
    ) {
        Ok(answer) => {
            println!(
                "\n{}{}{} {}\n",
                skin.paint(Role::Answer, ""),
                skin.answer_glyph,
                RESET,
                grace::markdown::render_terminal(&answer, skin)
            );
            if let Some(sid) = session_id {
                let _ = sessions.append(sid, &Message::assistant(answer));
            }
        }
        Err(grace::error::AgentError::Interrupted) => {
            // Tool calls up to this point already ran and are recorded in
            // `messages`/the session — only the final answer is missing.
            // Don't pop the user message: unlike a hard error, there's real
            // partial progress worth keeping in context for the next turn.
            println!("\n(interrupted — back to prompt)\n");
        }
        Err(e) => {
            eprintln!("error: {e}");
            // Drop the last user message so a failed turn can be retried.
            messages.pop();
        }
    }
}

/// Render an [`grace::agent::AgentEvent`] to stdout — the shared formatting
/// used by both one-shot and chat mode so tool calls and intermediate model
/// content are visible as they happen, not just the final answer.
///
/// Layout mirrors the tree-hierarchy style used by Claude Code / Codex CLI:
/// thinking as an indented sub-level under a "thinking" header, tool calls
/// as a `⏺`-prefixed line with an
/// indented `⎿` result underneath (so a run of many tool calls reads as a
/// visual tree, not a wall of flat log lines). All colors come from `skin`
/// (see [`grace::skin`]) — nothing here is hardcoded, so switching skins
/// restyles every surface at once. Colors auto-disable when stdout isn't a
/// real terminal (checked once via [`no_color`]).
pub(crate) fn print_agent_event(event: grace::agent::AgentEvent, skin: &Skin) {
    let no_color = no_color();
    let dim = |s: &str| if no_color { s.to_string() } else { format!("\x1b[2m{s}\x1b[0m") };

    match event {
        grace::agent::AgentEvent::AssistantContent(text) => {
            let bullet = skin.paint(Role::ToolBullet, "▾");
            let thinking = skin.paint(Role::Thinking, "Thinking");
            println!("{} {}", bullet, thinking);
            for line in text.lines() {
                println!("  {}", skin.paint(Role::Thinking, line));
            }
        }
        grace::agent::AgentEvent::ToolCallStart { name, arguments } => {
            let compact = compact_args(arguments);
            let bullet = skin.paint(Role::ToolBullet, "●");
            println!("{} {}{}({})", bullet, dim(""), name, compact);
        }
        grace::agent::AgentEvent::ToolCallEnd { name: _, result, elapsed } => {
            let rendered = grace::markdown::render_terminal(result, skin);
            for (i, line) in rendered.lines().enumerate() {
                let prefix = if i == 0 { "  ⎿ " } else { "    " };
                println!("{}{}{}", skin.paint(Role::ToolDim, ""), prefix, line);
            }
            let tokens = estimate_tokens(result);
            let secs = elapsed.as_secs_f64();
            let timing = if secs >= 1.0 {
                format!("{secs:.1}s")
            } else {
                format!("{}ms", (secs * 1000.0) as u64)
            };
            let prefix = format!("    {}· {}Σ", dim(""), skin.paint(Role::ToolBullet, ""));
            let rest = format!("{} ~{tokens} tok · {timing}", dim(""));
            println!("{prefix}{rest}");
        }
    }
}

/// Rough token-count estimate with no tokenizer dependency: ~4 chars/token
/// is the standard rule-of-thumb for English/code mixed text. Good enough
/// provider's exact billed count.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

/// Whether ANSI color should be suppressed: not a TTY, or `NO_COLOR`/`CLICOLOR=0` set.
pub(crate) fn no_color() -> bool {
    use std::io::IsTerminal;
    !std::io::stdout().is_terminal()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CLICOLOR").as_deref() == Ok("0")
}

/// The interactive-chat input prompt — a skin-colored glyph, never the
/// literal word "you", so the transcript reads as two distinct visual
/// speakers instead of a flat `you:`/`grace:` log.
pub(crate) fn prompt_label(skin: &Skin) -> String {
    if no_color() {
        return format!("{} ", skin.prompt_glyph);
    }
    skin.paint(Role::Prompt, &format!("{} ", skin.prompt_glyph))
}

/// Best-effort API fetch to discover a model's context window — thin
/// wrapper around [`grace::transport_http::fetch_context_window`] kept here
/// for call-site compatibility.
pub(crate) fn fetch_context_window(model: &str, base_url: &str, api_key: &str) -> Option<u32> {
    grace::transport_http::fetch_context_window(model, base_url, api_key)
}

/// A subtle status line above the prompt: model · context bar · elapsed.
/// All in the skin's muted `tool_dim` color so it recedes behind the prompt
/// glyph and never competes with the conversation itself. Single dim
/// (color only, no extra ANSI dim) — same tier as tool output body.
pub(crate) fn print_status_line(
    skin: &Skin,
    transport: &(dyn grace::transport::ProviderTransport + '_),
    messages: &[grace::message::Message],
    started_at: std::time::Instant,
    cached_context_window: Option<u32>,
) {
    let elapsed = started_at.elapsed();
    let secs = elapsed.as_secs();
    let time = if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    };
    let model = transport
        .current_model()
        .unwrap_or_else(|| transport.name().to_string());

    // Token estimate: sum chars across all messages, ~4 chars/token.
    let total_chars: usize = messages.iter().map(|m| m.content.chars().count()).sum();
    let estimated = (total_chars / 4).max(1);
    // Saved context window (loaded once per chat session, not re-read from
    // disk every turn) beats the static lookup table — it covers models
    // only known at runtime.
    let ctx = cached_context_window.or_else(|| grace::settings::context_window_for(&model));

    // Compact 8-segment context bar: █ filled, ░ empty.
    let bar = match ctx {
        Some(limit) if limit > 0 => {
            let pct = ((estimated as f64) / (limit as f64) * 100.0) as usize;
            let filled = (pct * 8 / 100).min(8);
            let empty = 8 - filled;
            format!("[{}] {pct}%", "█".repeat(filled) + &"░".repeat(empty))
        }
        _ => format!("~{estimated} tok"),
    };

    let line = format!("· {model} · {bar} · {time}");
    if no_color() {
        println!("{line}");
    } else {
        // skin's muted tool-dim color, single dim.
        println!("{}", skin.paint(Role::ToolDim, &line));
    }
}

/// Shrink a JSON tool-arguments string to a single readable line for the
/// `⏺ name(args)` header — whitespace-collapsed only, never truncated (the
/// user wants the full call visible; length isn't cause to hide content).
pub(crate) fn compact_args(arguments: &str) -> String {
    arguments.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Print available slash commands for the chat REPL.
fn print_slash_commands_help() {
    println!("\nAvailable slash commands:");
    println!("  /exit, /quit          - Exit the chat");
    println!("  /help, /commands      - Show this help");
    println!("  /model [name]         - Switch model (interactive picker if no arg)");
    println!("  /skin [name]          - Switch color skin (interactive picker if no arg)");
    println!("  /session [name]       - Switch session (interactive picker if no arg)");
    println!("  /session new          - Start fresh session (no persistence)");
    println!("  /session new-persist  - Start new persisted session");
    println!("  /session none         - Disable session persistence");
    println!("  /session list         - List all sessions");
    println!();
}
