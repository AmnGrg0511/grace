//! Interactive chat REPL: `run_chat`, per-turn execution, slash-command
//! handlers, and the shared event/status-line formatting used by both
//! chat and one-shot modes.

use crate::core::context::ContextCompressionConfig;
use crate::message::Message;
use crate::session::SessionStore;
use crate::config::settings::PROVIDER_PRESETS;
use crate::ui::skin::{Role, Skin};

use crate::ui::line_reader::LineReader;

pub const RESET: &str = "\x1b[0m";

/// A short, readable session id — e.g. `s-4kq9`. Full UUIDs are needless
/// noise for something the user only ever sees in a picker (never types by
/// hand for these auto-created sessions); a 4-char base36 suffix still has
/// ~1.6M combinations, plenty to avoid collision in one user's session
/// store while actually fitting on one line next to a title.
pub fn short_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut n = nanos;
    let mut suffix = [0u8; 4];
    for slot in suffix.iter_mut().rev() {
        *slot = ALPHABET[(n % ALPHABET.len() as u128) as usize];
        n /= ALPHABET.len() as u128;
    }
    format!("s-{}", std::str::from_utf8(&suffix).unwrap())
}

/// Interactive REPL. Each line you type is appended as a user message and the
/// conversation history (including tool calls) is preserved across turns. If
/// a session id was given, each turn is also persisted to disk immediately.
///
/// Owns exactly one [`LineReader`] for the whole session (see that module's
/// docs for why: two independent stdin readers used to race and steal each
/// other's lines whenever a picker like `/session` was invoked).
#[allow(clippy::too_many_arguments)]
pub fn run_chat(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    tools: &crate::tools::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    skin: &Skin,
    compression_config: &ContextCompressionConfig,
    verbose: bool,
    memory: &crate::memory::Memory,
    skills: &crate::skill::SkillStore,
) {
    // Owned+mutable so `/skin <name>` can swap it live; `/model <name>` swaps
    // the transport's own interior model instead (see `set_model`).
    let mut skin = *skin;
    // Owned so `/session <name>` can switch mid-chat.
    let mut current_session: Option<String> = session_id.map(|s| s.to_string());
    // Cross-terminal lock for whichever session is active — re-claimed on
    // every `/session` switch (see `handle_session_command`) so switching
    // away from a session releases it for other terminals immediately,
    // rather than holding the original --session lock for the whole process.
    let mut session_lock: Option<crate::session::SessionLock> =
        current_session.as_deref().and_then(|s| crate::session::SessionLock::acquire(s).ok());
    // `/verbose` toggles this mid-chat; starts from `--verbose`/`-v`.
    let mut verbose = verbose;

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

    println!("chat mode — type a message, '/exit' to leave, '/model [name]' to switch models, '/skin [name]' to retheme, '/session' to switch sessions, '/verbose' to toggle tool output.\n");

    let started_at = std::time::Instant::now();
    // Loaded once per chat session (not re-read from disk every turn) —
    // `/model` updates this in-memory copy too so the status bar reflects
    // a mid-chat model switch without another disk read.
    let mut cached_context_window = crate::config::settings::Settings::load().default_context_window;

    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join(match session_id {
            // Scope up-arrow history to the active session from the very
            // start, not just after a mid-chat `/session` switch — starting
            // `grace --chat --session <id>` should already show that
            // session's own past inputs, not the global stack's.
            Some(sid) => format!("history_{sid}.txt"),
            None => "history.txt".to_string(),
        });
    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Single stdin owner for the entire interactive session — every picker
    // below (`/model`, `/skin`, `/session`) takes `&mut reader` instead of
    // opening its own `std::io::stdin()`.
    let mut reader = LineReader::new(history_path, skin);
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
            cached_context_window = crate::config::settings::Settings::load().default_context_window;
            continue;
        }
        if let Some(rest) = text.strip_prefix("/skin") {
            handle_skin_command(rest.trim(), &mut skin, &mut reader);
            reader.set_skin(skin);
            continue;
        }
        if let Some(_rest) = text.strip_prefix("/session") {
            handle_session_command(sessions, messages, &mut current_session, &mut reader, memory, &mut session_lock);
            continue;
        }
        if text.starts_with("/verbose") {
            verbose = !verbose;
            println!(
                "tool output {} (read/bash bodies {}; edit diffs always show).",
                if verbose { "shown" } else { "hidden" },
                if verbose { "visible" } else { "hidden" }
            );
            continue;
        }
        // Pre-flight recall: same mechanism one-shot mode gets at startup,
        // but re-run on EVERY turn here since chat is long-lived and each
        // message may need different facts/skills/sessions surfaced. Without
        // this, chat mode never saw memory/skills recall at all — only the
        // durable-facts block from session start, which is what caused
        // "I have to explicitly ask it to load a skill" (recall's whole
        // point is to surface a matching skill without being asked).
        let recall_hint = crate::recall::as_prompt_block(&crate::recall::recall(
            text,
            memory,
            skills,
            Some(sessions),
            5,
        ));
        run_one_chat_turn(
            transport,
            tools,
            messages,
            max_iterations,
            sessions,
            current_session.as_deref(),
            text,
            recall_hint.as_deref(),
            &skin,
            &interrupted,
            compression_config,
            verbose,
        );
    }
}

/// `/model` (interactive picker, same list as onboarding) or `/model <name>`
/// (direct switch) mid-chat. Persists to ~/.grace/config.toml so the choice
/// sticks across restarts (unlike the old session-only behavior).
/// Only takes effect on transports that own a swappable model (`HttpTransport`).
fn handle_model_command(
    transport: &(dyn crate::transport::ProviderTransport + '_),
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
    let (picked, ctx, new_endpoint) = if arg.is_empty() {
        match pick_model_interactive(reader) {
            Some(result) => result,
            None => return,
        }
    } else {
        let endpoint = find_provider_for_model(arg);
        (arg.to_string(), None, endpoint)
    };

    // A different provider was picked: re-point the transport at its
    // base_url/api_key instead of silently keeping the old endpoint with a
    // model id it was never meant for (the original bug: /model listed
    // providers but only ever swapped the model string, so picking
    // "OpenAI" mid-OpenRouter-session sent OpenAI model ids to OpenRouter
    // with the OpenRouter key and never asked for anything).
    if let Some((base_url, env_var)) = new_endpoint {
        let same_endpoint = transport.current_base_url().as_deref() == Some(base_url.as_str());
        if !same_endpoint {
            let is_copilot = base_url == crate::transport::copilot::BASE_URL;
            let key = if is_copilot {
                // Copilot uses OAuth device flow, not a typed API key.
                // Same path as the onboarding wizard — get_or_create_token()
                // handles the "open browser" prompt itself.
                crate::transport::copilot::get_or_create_token()
                    .map_err(|e| { println!("copilot auth failed: {e}"); e })
                    .ok()
            } else {
                std::env::var(env_var)
                    .ok()
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        reader
                            .read_line(&format!("API key for this provider (${env_var} not set): "))
                            .map(|k| k.trim().to_string())
                            .filter(|k| !k.is_empty())
                    })
            };
            let Some(key) = key else {
                if !is_copilot {
                    println!("no key provided — staying on current provider.");
                }
                return;
            };
            transport.set_endpoint(&base_url, &key);
            let mut settings = crate::config::settings::Settings::load();
            settings.default_base_url = Some(base_url.clone());
            if let Err(e) = settings.save() {
                eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
            }
            let env_path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".grace")
                .join(".env");
            if let Some(parent) = env_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&env_path, format!("{env_var}={key}\n")) {
                eprintln!("[grace] warning: could not save {}: {e}", env_path.display());
            }
        }
    }

    transport.set_model(&picked);
    if let Some(m) = transport.current_model() {
        let mut settings = crate::config::settings::Settings::load();
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
/// `PickedModel` = `(model_id, optional_context_window, optional (base_url,
/// env_var) when a different provider than the transport's current one
/// was picked)`. Used by `/model` mid-chat. Returns `None` on
/// unparsable/EOF input (no-op).
type PickedModel = (String, Option<u32>, Option<(String, &'static str)>);

fn pick_model_interactive(reader: &mut LineReader) -> Option<PickedModel> {
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
    let endpoint = if preset.base_url.is_empty() {
        None
    } else {
        Some((preset.base_url.to_string(), preset.env_var))
    };
    if preset.models.is_empty() {
        // Provider with no known models (e.g. "Custom endpoint"): type one.
        let typed = reader.read_line("model id: ")?.trim().to_string();
        return if typed.is_empty() { None } else { Some((typed, None, endpoint)) };
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
            endpoint,
        )),
        Ok(n) if n == n_models + 1 => {
            // Custom model ID
            let typed = reader.read_line("model id: ")?.trim().to_string();
            if typed.is_empty() { None } else { Some((typed, None, endpoint)) }
        }
        _ => {
            println!("not a valid choice.");
            None
        }
    }
}

/// Look up which provider preset a model ID belongs to, returning its
/// `(base_url, env_var)` so a direct `/model <name>` switch can re-point the
/// transport at the correct endpoint (not just swap the model string).
fn find_provider_for_model(model_id: &str) -> Option<(String, &'static str)> {
    for preset in PROVIDER_PRESETS {
        if !preset.base_url.is_empty()
            && preset.models.iter().any(|m| m.id == model_id)
        {
            return Some((preset.base_url.to_string(), preset.env_var));
        }
    }
    None
}

/// `/skin` (interactive picker, same as `--select-skin`) or `/skin <name>`
/// (direct switch) mid-chat. Session-only — use `--select-skin` to persist
/// a default across runs.
fn handle_skin_command(arg: &str, skin: &mut Skin, reader: &mut LineReader) {
    let names = crate::ui::skin::all_names();
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
    *skin = crate::ui::skin::by_name(Some(&picked));
    let mut settings = crate::config::settings::Settings::load();
    settings.skin = Some(picked.clone());
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    } else {
        println!("skin switched to \"{picked}\" (saved to config).");
    }
}

/// `/session` — interactive session picker.
/// The picker lists saved sessions, lets you switch to one, or start a fresh session.
/// No explicit subcommands: the interactive picker covers everything.
fn handle_session_command(
    sessions: &SessionStore,
    messages: &mut Vec<Message>,
    current_session: &mut Option<String>,
    reader: &mut LineReader,
    memory: &crate::memory::Memory,
    session_lock: &mut Option<crate::session::SessionLock>,
) {
    // `sessions.load()` never returns a system message (see session.rs —
    // only user/assistant rows are persisted), and `messages.clear()` below
    // wipes whatever system message was already in memory. Re-derive fresh
    // every time instead of caching, since `--remember` can add facts
    // mid-process (via a different terminal) and a stale cached block would
    // miss those.
    let fresh_system = || {
        let mut sp = crate::config::load_soul();
        if let Ok(Some(block)) = memory.as_prompt_block() {
            sp.push_str(&block);
        }
        Message::system(sp)
    };

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
    let titles = sessions.get_titles(&session_list).unwrap_or_default();
    let labels: Vec<String> = session_list
        .iter()
        .map(|sid| {
            titles.get(sid).cloned().unwrap_or_else(|| {
                sessions
                    .load(sid)
                    .ok()
                    .and_then(|m| m.first().map(|m| m.content.clone()))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().take(50).collect())
                    .unwrap_or_else(|| "(untitled)".to_string())
            })
        })
        .collect();

    // Identical titles are common (a "hi"-only opener always titles the same
    // way) and made entries impossible to tell apart. Disambiguate any label
    // that repeats by suffixing the short session id — unique labels are left
    // exactly as generated.
    let mut counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for l in &labels {
        *counts.entry(l.as_str()).or_insert(0) += 1;
    }
    for (i, sid) in session_list.iter().enumerate() {
        let label = &labels[i];
        let disambiguated = if counts.get(label.as_str()).copied().unwrap_or(0) > 1 {
            format!("{label} ({sid})")
        } else {
            label.clone()
        };
        let lock_marker = if crate::session::SessionLock::is_held(sid) {
            " [open in another terminal]"
        } else {
            ""
        };
        println!("  {}) {}{}", i + 1, disambiguated, lock_marker);
    }
    let Some(raw) = reader.read_line("\nselect a session [number, or 0 for new]: ") else {
        println!("no input — staying on current session.");
        return;
    };
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            messages.clear();
            messages.push(fresh_system());
            let new_id = short_session_id();
            if let Err(e) = sessions.append(&new_id, &Message::user("")) {
                println!("warning: could not persist new session: {e}");
            }
            *current_session = Some(new_id.clone());
            *session_lock = crate::session::SessionLock::acquire(&new_id).ok();
            reader.set_history_scope(Some(&new_id));
            println!("started a fresh session.");
        }
        Ok(n) if n >= 1 && n <= session_list.len() => {
            let sid = &session_list[n - 1];
            if crate::session::SessionLock::is_held(sid) {
                println!(
                    "session is open in another terminal — switching here would co-own it (concurrent writes may interleave). switch anyway? [y/N]"
                );
                match reader.read_line("") {
                    Some(ans) if ans.trim().eq_ignore_ascii_case("y") => {}
                    _ => {
                        println!("staying on current session.");
                        return;
                    }
                }
            }
            match sessions.load(sid) {
                Ok(loaded) => {
                    messages.clear();
                    messages.push(fresh_system());
                    messages.extend(loaded);
                    *current_session = Some(sid.clone());
                    *session_lock = crate::session::SessionLock::acquire(sid).ok();
                    reader.set_history_scope(Some(sid));
                    let label = titles.get(sid).cloned().unwrap_or_else(|| sid.clone());
                    println!("switched to session \"{label}\" ({} messages loaded).", messages.len());
                }
                Err(e) => println!("error loading session: {e}"),
            }
        }
        _ => println!("not a valid choice — staying on current session."),
    }
}

/// Shared skin list+preview+select flow, identical presentation to
/// `--select-skin` so muscle memory carries over between startup and
/// mid-chat. Returns `None` on unparsable/EOF input (no-op).
pub fn pick_skin_interactive(names: &[String], reader: &mut LineReader) -> Option<String> {
    println!("\navailable skins:\n");
    for (i, name) in names.iter().enumerate() {
        let s = crate::ui::skin::by_name(Some(name));
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
pub fn run_one_chat_turn(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    tools: &crate::tools::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    text: &str,
    recall_hint: Option<&str>,
    skin: &Skin,
    interrupted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    compression_config: &ContextCompressionConfig,
    verbose: bool,
) {
    // Snapshot turn count BEFORE appending — drives auto-(re)titling below.
    // History alternates user/assistant, so history_len/2 is the 0-based
    // index of the turn about to happen.
    let turn_index = session_id
        .and_then(|sid| sessions.load(sid).ok())
        .map(|h| h.len() / 2)
        .unwrap_or(0);
    messages.push(Message::user(match recall_hint {
        Some(hint) => format!("{text}\n{hint}"),
        None => text.to_string(),
    }));
    if let Some(sid) = session_id {
        // Persist the clean user text (no recall noise) — recall is
        // re-derived fresh on replay/resume, so baking it into disk history
        // would only duplicate and stale-date it.
        let _ = sessions.append(sid, &Message::user(text.to_string()));
    }
    // Clear any interrupt latched from a previous turn before starting a new
    // one — otherwise a stale Ctrl-C would abort every turn from here on.
    interrupted.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut stream_state = StreamState::default();
    let mut sink =
        |event: crate::core::lifecycle::AgentEvent<'_>| {
            print_agent_event(event, skin, verbose, &mut stream_state);
        };
    let outcome = crate::core::run_turn_with_options(
        transport,
        tools,
        messages,
        max_iterations,
        crate::core::TurnOptions::new()
            .with_events(&mut sink)
            .with_interrupt(interrupted.as_ref())
            .with_compression(compression_config)
            // Chat mode streams AND renders markdown: ContentFragment
            // buffers the text, re-renders the full buffer through
            // render_terminal, and overwrites the previous render via ANSI
            // cursor movement.  One-shot `--stream` does the same (see
            // cli.rs).
            .streaming(transport.supports_streaming()),
    );
    match outcome {
        Ok(crate::core::TurnOutcome {
            answer, streamed, ..
        }) => {
            if streamed {
                // Already printed live, fragment by fragment. Re-rendering it
                // here would show the whole answer a second time; just close
                // off the streamed block.
                println!("\n");
            } else {
                println!(
                    "\n{}{}{} {}\n",
                    skin.paint(Role::Answer, ""),
                    skin.answer_glyph,
                    RESET,
                    crate::ui::markdown::render_terminal(&answer, skin)
                );
            }
            if let Some(sid) = session_id {
                let _ = sessions.append(sid, &Message::assistant(answer.clone()));
                // Auto-(re)title: retitle at turn 1 (first real content),
                // then again at 5 and 15 as the topic usually solidifies,
                // then every 20 turns after that for long sessions that
                // drift — cheap (one extra completion call, no tools) and
                // keeps the `/session` picker's summary from freezing on
                // "hi" forever. Best-effort: a failed call just leaves the
                // previous title in place.
                let should_retitle = matches!(turn_index, 0 | 4 | 14) || (turn_index > 14 && turn_index.is_multiple_of(20));
                if should_retitle {
                    if let Some(model) = transport.current_model() {
                        // Summarize the whole conversation so far, not just
                        // this turn — the picker title should reflect what
                        // the *session* is about, not just its latest message.
                        let transcript = sessions
                            .load(sid)
                            .unwrap_or_default()
                            .iter()
                            .filter(|m| !m.content.is_empty())
                            .map(|m| format!("{:?}: {}", m.role, m.content))
                            .collect::<Vec<_>>()
                            .join("\n");
                        // Cap what we send — a long-running session's full
                        // history isn't needed to name it, and this keeps
                        // the retitle call itself cheap regardless of turn
                        // count. Tail-biased: the recent topic usually
                        // matters more than the opener by the time we're
                        // retitling at turn 15+.
                        let transcript: String = transcript.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
                        if let Some(title) = crate::session::generate_title(transport, &model, &transcript) {
                            let _ = sessions.set_title(sid, &title);
                        }
                    }
                }
            }
        }
        Err(crate::util::AgentError::Interrupted) => {
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

/// Render an [`crate::core::lifecycle::AgentEvent`] to stdout — the shared formatting
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
/// Whether a tool's result body should be printed: `patch` (diffs) always
/// shows since seeing the diff is the point; every other tool
/// (read/bash/etc.) is noise by default, gated behind
/// `--verbose`/`/verbose`. Pulled out as its own function (not inlined in
/// `print_agent_event`) so it's directly unit-testable without touching
/// stdout at all.
fn should_show_tool_output(tool_name: &str, verbose: bool) -> bool {
    verbose || tool_name == "edit"
}

/// State for streaming markdown rendering.  Fragments are buffered and the
/// full buffer is re‑rendered through `render_terminal` on every fragment;
/// ANSI cursor movement overwrites the previous render in place so the user
/// sees styled output that updates live.
#[derive(Default)]
pub struct StreamState {
    buf: String,
    lines: usize,
}

/// Reset streaming state after a non‑fragment event interrupts the stream
/// (tool call, context compression, etc.).  The cursor is already on a fresh
/// line after the last render, so the new event prints cleanly below it.
fn finalize_stream(stream: &mut StreamState) {
    stream.buf.clear();
    stream.lines = 0;
}

pub fn print_agent_event(
    event: crate::core::lifecycle::AgentEvent,
    skin: &Skin,
    verbose: bool,
    stream: &mut StreamState,
) {
    let mut stdout = std::io::stdout();
    print_agent_event_to(event, skin, verbose, stream, &mut stdout);
}

/// Write an event to `out` — same as [`print_agent_event`] but testable
/// (capturable) because it writes through a `Write` trait object instead of
/// hard‑wiring `stdout`.  All the cursor‑movement and markdown logic lives
/// here; `print_agent_event` is just a thin wrapper.
pub fn print_agent_event_to(
    event: crate::core::lifecycle::AgentEvent,
    skin: &Skin,
    verbose: bool,
    stream: &mut StreamState,
    out: &mut dyn std::io::Write,
) {
    let disable_color = no_color();
    let dim = |s: &str| {
        if disable_color {
            s.to_string()
        } else {
            format!("\x1b[2m{s}\x1b[0m")
        }
    };

    match event {
        crate::core::lifecycle::AgentEvent::AssistantContent(text) => {
            finalize_stream(stream);
            let bullet = skin.paint(Role::ToolBullet, "▾");
            let thinking = skin.paint(Role::Thinking, "Thinking");
            writeln!(out, "{} {}", bullet, thinking).ok();
            for line in text.lines() {
                writeln!(out, "  {}", skin.paint(Role::Thinking, line)).ok();
            }
        }
        crate::core::lifecycle::AgentEvent::ToolCallStart { name, arguments } => {
            finalize_stream(stream);
            let compact = compact_args(arguments);
            let bullet = skin.paint(Role::ToolBullet, "●");
            writeln!(out, "{} {}{}({})", bullet, dim(""), name, compact).ok();
        }
        crate::core::lifecycle::AgentEvent::ToolCallEnd { name, result, elapsed } => {
            finalize_stream(stream);
            // Tool output is noisy by default — `edit` is the one tool
            // whose result (a diff) is the point of looking at it, so it
            // always shows; everything else (read/bash/etc.)
            // is hidden unless `--verbose`/`/verbose` is on. The one-line
            // "call + timing" summary below still always prints, so you
            // always see *that* something ran, just not its full body.
            if should_show_tool_output(name, verbose) {
                let rendered = crate::ui::markdown::render_terminal(result, skin);
                for (i, line) in rendered.lines().enumerate() {
                    let prefix = if i == 0 { "  ⎿ " } else { "    " };
                    writeln!(out, "{}{}{}", skin.paint(Role::ToolDim, ""), prefix, line).ok();
                }
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
            writeln!(out, "{prefix}{rest}").ok();
        }
        crate::core::lifecycle::AgentEvent::ContentFragment(fragment) => {
            stream.buf.push_str(&fragment);
            if disable_color {
                // Piped output: no cursor tricks, print fragments raw.
                write!(out, "{fragment}").ok();
            } else {
                // Overwrite the previous render in place.
                if stream.lines > 0 {
                    write!(out, "\x1b[{}A", stream.lines).ok();
                }
                write!(out, "\x1b[J").ok(); // clear from cursor to end of screen
                let rendered =
                    crate::ui::markdown::render_terminal_colored(&stream.buf, skin, true);
                write!(out, "{rendered}").ok();
                stream.lines = rendered.lines().count();
            }
            let _ = out.flush();
        }
        crate::core::lifecycle::AgentEvent::ContextCompressed {
            before_tokens,
            after_tokens,
            dropped_messages,
        } => {
            finalize_stream(stream);
            // Always surfaced, not gated behind --verbose: history silently
            // disappearing is exactly the kind of thing a user needs told.
            writeln!(
                out,
                "{}",
                skin.paint(
                    Role::ToolDim,
                    &format!(
                        "  · context compressed: {before_tokens} → {after_tokens} tok \
                         ({dropped_messages} older messages elided)"
                    )
                )
            )
            .ok();
        }
        crate::core::lifecycle::AgentEvent::DelegationStart { task, budget } => {
            finalize_stream(stream);
            let bullet = skin.paint(Role::ToolBullet, "⇢");
            writeln!(out, "{bullet} delegating (budget {budget}): {}", compact_args(task)).ok();
        }
        crate::core::lifecycle::AgentEvent::DelegationEnd {
            task: _,
            iterations,
            ok,
        } => {
            finalize_stream(stream);
            let status = if ok { "done" } else { "budget exhausted" };
            writeln!(
                out,
                "{}",
                skin.paint(
                    Role::ToolDim,
                    &format!("    · sub-agent {status} after {iterations} iterations")
                )
            )
            .ok();
        }
    }
}

/// Token-count estimate for display.
///
/// Delegates to [`crate::util::tokens`] rather than re-deriving a chars/4
/// rule of thumb here — the status bar and the compressor must agree, or the
/// bar shows 40% while compression is firing.
pub fn estimate_tokens(text: &str) -> usize {
    use crate::util::tokens::TokenCounter;
    crate::util::tokens::default_counter().count_text(text).max(1)
}

/// Whether ANSI color should be suppressed: not a TTY (unless
/// `CLICOLOR_FORCE` is set), or `NO_COLOR`/`CLICOLOR=0` set.  The
/// `CLICOLOR_FORCE` override lets a user pipe to `less -R` (or similar) and
/// still get color, and lets tests force the color path without a PTY.
pub fn no_color() -> bool {
    use std::io::IsTerminal;
    if std::env::var("CLICOLOR_FORCE").as_deref() == Ok("1") {
        return false;
    }
    !std::io::stdout().is_terminal()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CLICOLOR").as_deref() == Ok("0")
}

/// The interactive-chat input prompt — a skin-colored glyph, never the
/// literal word "you", so the transcript reads as two distinct visual
/// speakers instead of a flat `you:`/`grace:` log.
pub fn prompt_label(skin: &Skin) -> String {
    if no_color() {
        return format!("{} ", skin.prompt_glyph);
    }
    skin.paint(Role::Prompt, &format!("{} ", skin.prompt_glyph))
}

/// Best-effort API fetch to discover a model's context window — thin
/// wrapper around [`crate::transport::http::fetch_context_window`] kept here
/// for call-site compatibility.
pub fn fetch_context_window(model: &str, base_url: &str, api_key: &str) -> Option<u32> {
    crate::transport::http::fetch_context_window(model, base_url, api_key)
}

/// A subtle status line above the prompt: model · context bar · elapsed.
/// All in the skin's muted `tool_dim` color so it recedes behind the prompt
/// glyph and never competes with the conversation itself. Single dim
/// (color only, no extra ANSI dim) — same tier as tool output body.
pub fn print_status_line(
    skin: &Skin,
    transport: &(dyn crate::transport::ProviderTransport + '_),
    messages: &[crate::message::Message],
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

    // Same estimator the compressor uses, so the bar and the compression
    // trigger can never disagree about how full the window is.
    let estimated = {
        use crate::util::tokens::TokenCounter;
        crate::util::tokens::default_counter()
            .count_messages(messages)
            .max(1)
    };
    // Saved context window (loaded once per chat session, not re-read from
    // disk every turn) beats the static lookup table — it covers models
    // only known at runtime.
    let ctx = cached_context_window.or_else(|| crate::config::settings::context_window_for(&model));

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
pub fn compact_args(arguments: &str) -> String {
    arguments.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Print available slash commands for the chat REPL.
fn print_slash_commands_help() {
    println!("\nAvailable slash commands:");
    println!("  /exit, /quit          - Exit the chat");
    println!("  /help, /commands      - Show this help");
    println!("  /model [name]         - Switch model (interactive picker if no arg)");
    println!("  /skin [name]          - Switch color skin (interactive picker if no arg)");
    println!("  /session              - Switch / start session (interactive picker)");
    println!("  /verbose              - Toggle tool-output visibility (patch diffs always show)");
    println!();
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::should_show_tool_output;

    #[test]
    fn non_edit_tools_hidden_unless_verbose() {
        assert!(!should_show_tool_output("read", false));
        assert!(!should_show_tool_output("bash", false));
        assert!(should_show_tool_output("read", true));
        assert!(should_show_tool_output("bash", true));
    }

    #[test]
    fn edit_always_shown_regardless_of_verbose() {
        assert!(should_show_tool_output("edit", false));
        assert!(should_show_tool_output("edit", true));
    }
}

#[cfg(test)]
mod streaming_markdown_tests {
    use super::*;
    use crate::core::lifecycle::AgentEvent;

    fn render_events(events: &[AgentEvent], color: bool) -> String {
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        // Force the color decision via env vars (tests have no PTY, so
        // `no_color()` would otherwise always be true).  The env lock
        // serializes against other tests that mutate the same vars.
        let _lock = crate::util::test_support::env_guard();
        std::env::remove_var("CLICOLOR_FORCE");
        std::env::remove_var("NO_COLOR");
        if color {
            std::env::set_var("CLICOLOR_FORCE", "1");
        } else {
            std::env::set_var("NO_COLOR", "1");
        }
        // `print_agent_event_to` takes the event by value; `ContentFragment`
        // holds only a `&'static str` in these tests, so we can reborrow.
        for event in events {
            let event = match *event {
                AgentEvent::ContentFragment(s) => AgentEvent::ContentFragment(s),
                AgentEvent::AssistantContent(s) => AgentEvent::AssistantContent(s),
                _ => unreachable!(),
            };
            print_agent_event_to(event, &skin, false, &mut stream, &mut out);
        }
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn piped_output_prints_fragments_raw() {
        // NO_COLOR → fragments are concatenated with no markdown rendering.
        let out = render_events(
            &[
                AgentEvent::ContentFragment("Hello "),
                AgentEvent::ContentFragment("**bold**"),
                AgentEvent::ContentFragment(" world"),
            ],
            false,
        );
        assert_eq!(out, "Hello **bold** world");
    }

    #[test]
    fn color_output_renders_markdown_once_at_end() {
        // The final render of the full buffer must contain styled bold and
        // heading — and because the stream is overwritten in place, the RAW
        // fragments ("**bold**") must NOT appear in the captured output.
        let out = render_events(
            &[
                AgentEvent::ContentFragment("Hello "),
                AgentEvent::ContentFragment("**bold**"),
                AgentEvent::ContentFragment("\n\n## Heading\n"),
            ],
            true,
        );
        // The rendered output contains ANSI-styled text, not raw markdown.
        assert!(
            out.contains("\x1b[1m"),
            "expected bold ANSI escape in output, got: {out:?}"
        );
        assert!(
            !out.contains("**bold**"),
            "raw markdown should not leak into final output: {out:?}"
        );
        assert!(
            out.contains("Heading"),
            "heading text should be present: {out:?}"
        );
    }

    #[test]
    fn finalize_stream_resets_between_tool_calls() {
        // Simulate: fragment stream → tool call → fragment stream again.
        // The second stream must start fresh (no leftover cursor movement
        // from the first).
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        // Force color on (tests have no PTY) so the cursor-movement path
        // runs — that's the state we want to verify resets.
        std::env::set_var("CLICOLOR_FORCE", "1");

        // First stream
        print_agent_event_to(
            AgentEvent::ContentFragment("first "),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        print_agent_event_to(
            AgentEvent::ContentFragment("stream"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        let after_first = String::from_utf8(out.clone()).unwrap();
        let first_render_count = after_first.matches("first stream").count();
        assert_eq!(
            first_render_count, 1,
            "first stream should render once, got {first_render_count} in: {after_first:?}"
        );

        // Tool call interrupts the stream
        print_agent_event_to(
            AgentEvent::ToolCallEnd {
                name: "read".into(),
                result: "file contents".into(),
                elapsed: std::time::Duration::from_millis(10),
            },
            &skin,
            false,
            &mut stream,
            &mut out,
        );

        // Second stream
        print_agent_event_to(
            AgentEvent::ContentFragment("second stream"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        let final_out = String::from_utf8(out.clone()).unwrap();
        // "second stream" appears exactly once (no duplication from leftover state)
        let second_count = final_out.matches("second stream").count();
        assert_eq!(
            second_count, 1,
            "second stream should render once, got {second_count} in: {final_out:?}"
        );

        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn raw_fragments_not_duplicated_in_color_output() {
        // Key regression test: as fragments arrive, the full buffer is
        // re-rendered each time.  The captured bytes are the CONCATENATION of
        // all renders (because there's no real terminal to overwrite), so we
        // expect to see repeated content — but each re-render replaces the
        // whole buffer, so the raw fragment text from an EARLIER partial
        // buffer should not appear as a standalone line.
        let out = render_events(
            &[
                AgentEvent::ContentFragment("**bold"),
                AgentEvent::ContentFragment(" text**"),
            ],
            true,
        );
        // The final buffer renders as styled "bold text".  We should NOT see
        // the incomplete intermediate "**bold" as a line on its own (that
        // would indicate the intermediate render leaked).
        let lines: Vec<&str> = out.lines().collect();
        let standalone_partial = lines.iter().any(|l| *l == "**bold");
        assert!(
            !standalone_partial,
            "intermediate partial render leaked as standalone line: {lines:?}"
        );
    }
}
