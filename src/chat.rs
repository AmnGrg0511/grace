//! Interactive chat REPL: `run_chat`, per-turn execution, slash-command
//! handlers, and the shared event/status-line formatting used by both
//! chat and one-shot modes.

use grace::config::ContextCompressionConfig;
use grace::message::Message;
use grace::session::SessionStore;
use grace::settings::PROVIDER_PRESETS;
use grace::skin::{Role, Skin};

use crate::line_reader::LineReader;

pub(crate) const RESET: &str = "\x1b[0m";

/// A short, readable session id — e.g. `s-4kq9`. Full UUIDs are needless
/// noise for something the user only ever sees in a picker (never types by
/// hand for these auto-created sessions); a 4-char base36 suffix still has
/// ~1.6M combinations, plenty to avoid collision in one user's session
/// store while actually fitting on one line next to a title.
pub(crate) fn short_session_id() -> String {
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
pub(crate) fn run_chat(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    skin: &Skin,
    compression_config: &ContextCompressionConfig,
    verbose: bool,
    memory: &grace::memory::Memory,
    skills: &grace::skill::SkillStore,
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
    let mut session_lock: Option<grace::session::SessionLock> =
        current_session.as_deref().and_then(|s| grace::session::SessionLock::acquire(s).ok());
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
    let mut cached_context_window = grace::settings::Settings::load().default_context_window;

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
            cached_context_window = grace::settings::Settings::load().default_context_window;
            continue;
        }
        if let Some(rest) = text.strip_prefix("/skin") {
            handle_skin_command(rest.trim(), &mut skin, &mut reader);
            reader.set_skin(skin);
            continue;
        }
        if let Some(rest) = text.strip_prefix("/session") {
            handle_session_command(rest.trim(), sessions, messages, &mut current_session, &mut reader, memory, &mut session_lock);
            continue;
        }
        if text.starts_with("/verbose") {
            verbose = !verbose;
            println!(
                "tool output {} (read_file/run_terminal bodies {}; patch diffs always show).",
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
        let recall_hint = grace::recall::as_prompt_block(&grace::recall::recall(
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
    let (picked, ctx, new_endpoint) = if arg.is_empty() {
        match pick_model_interactive(reader) {
            Some(result) => result,
            None => return,
        }
    } else {
        (arg.to_string(), None, None)
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
            let key = std::env::var(env_var)
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .or_else(|| {
                    reader
                        .read_line(&format!("API key for this provider (${env_var} not set): "))
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty())
                });
            let Some(key) = key else {
                println!("no key provided — staying on current provider.");
                return;
            };
            transport.set_endpoint(&base_url, &key);
            let mut settings = grace::settings::Settings::load();
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
    memory: &grace::memory::Memory,
    session_lock: &mut Option<grace::session::SessionLock>,
) {
    // `sessions.load()` never returns a system message (see session.rs —
    // only user/assistant rows are persisted), and every `messages.clear()`
    // below wipes whatever system message was already in memory. Without
    // this, `/session new`/`/session <name>` silently dropped the persona +
    // durable-facts block for the rest of the process: the model would
    // answer with no identity and no memory of facts told to it in an
    // *earlier* session, even though `--remember` had genuinely saved them.
    // Re-derive fresh every time rather than caching it once, since
    // `--remember` can add facts mid-process (via a different terminal) and
    // a stale cached block would miss those.
    let fresh_system = || {
        let mut sp = grace::config::load_soul();
        if let Ok(Some(block)) = memory.as_prompt_block() {
            sp.push_str(&block);
        }
        Message::system(sp)
    };

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
        let titles = sessions.get_titles(&session_list).unwrap_or_default();
        for (i, sid) in session_list.iter().enumerate() {
            // Prefer the auto-generated title (a real description of what
            // the chat is about) over the raw id — the id itself is never
            // shown here at all now, since a bare UUID/tty-path conveyed
            // nothing and the old "first message" preview was almost
            // always just "hi".
            let label = titles.get(sid).cloned().unwrap_or_else(|| {
                sessions
                    .load(sid)
                    .ok()
                    .and_then(|m| m.first().map(|m| m.content.clone()))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().take(50).collect())
                    .unwrap_or_else(|| "(untitled)".to_string())
            });
            println!("  {}) {}", i + 1, label);
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
                // Persist the empty session immediately so it survives restarts
                if let Err(e) = sessions.append(&new_id, &Message::user("")) {
                    println!("warning: could not persist new session: {e}");
                }
                *current_session = Some(new_id.clone());
                *session_lock = grace::session::SessionLock::acquire(&new_id).ok();
                reader.set_history_scope(Some(&new_id));
                println!("started fresh session: {new_id}.");
            }
            Ok(n) if n >= 1 && n <= session_list.len() => {
                let sid = &session_list[n - 1];
                match sessions.load(sid) {
                    Ok(loaded) => {
                        messages.clear();
                        messages.push(fresh_system());
                        messages.extend(loaded);
                        *current_session = Some(sid.clone());
                        *session_lock = grace::session::SessionLock::acquire(sid).ok();
                        reader.set_history_scope(Some(sid));
                        let label = titles.get(sid).cloned().unwrap_or_else(|| sid.clone());
                        println!("switched to session \"{label}\" ({} messages loaded).", messages.len());
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
            messages.push(fresh_system());
            *current_session = None;
            *session_lock = None;
            reader.set_history_scope(None);
            println!("started a fresh session (no persistence).");
        }
        "new-persist" => {
            let new_id = short_session_id();
            messages.clear();
            messages.push(fresh_system());
            *current_session = Some(new_id.clone());
            *session_lock = grace::session::SessionLock::acquire(&new_id).ok();
            reader.set_history_scope(Some(&new_id));
            // Persist the empty session immediately
            if let Err(e) = sessions.append(&new_id, &Message::user("")) {
                println!("warning: could not persist new session: {e}");
            }
            println!("started new persisted session: {new_id}");
        }
        "none" => {
            *current_session = None;
            *session_lock = None;
            reader.set_history_scope(None);
            println!("session persistence disabled for this chat.");
        }
        "list" => {
            match sessions.list_sessions() {
                Ok(list) if list.is_empty() => println!("no saved sessions yet."),
                Ok(list) => {
                    println!("\nsaved sessions (most recent first):\n");
                    let titles = sessions.get_titles(&list).unwrap_or_default();
                    for sid in &list {
                        let marker = if current_session.as_deref() == Some(sid.as_str()) {
                            " (current)"
                        } else {
                            ""
                        };
                        let label = titles.get(sid).cloned().unwrap_or_else(|| sid.clone());
                        println!("  {label}{marker}");
                    }
                }
                Err(e) => println!("error listing sessions: {e}"),
            }
        }
        name => {
            match sessions.load(name) {
                Ok(loaded) => {
                    messages.clear();
                    messages.push(fresh_system());
                    messages.extend(loaded);
                    *current_session = Some(name.to_string());
                    *session_lock = grace::session::SessionLock::acquire(name).ok();
                    reader.set_history_scope(Some(name));
                    println!("switched to session \"{name}\" ({} messages loaded).", messages.len());
                }
                Err(e) => {
                    println!("session \"{name}\" not found ({}). Starting fresh.", e);
                    messages.clear();
                    messages.push(fresh_system());
                    *current_session = Some(name.to_string());
                    *session_lock = grace::session::SessionLock::acquire(name).ok();
                    reader.set_history_scope(Some(name));
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
    match grace::agent::run_turn_with_events(
        transport,
        tools,
        messages,
        max_iterations,
        Some(&mut |event| print_agent_event(event, skin, verbose)),
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
                        if let Some(title) = grace::session::generate_title(transport, &model, &transcript) {
                            let _ = sessions.set_title(sid, &title);
                        }
                    }
                }
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
/// Whether a tool's result body should be printed: `patch` (diffs) always
/// shows since seeing the diff is the point; every other tool
/// (read_file/run_terminal/etc.) is noise by default, gated behind
/// `--verbose`/`/verbose`. Pulled out as its own function (not inlined in
/// `print_agent_event`) so it's directly unit-testable without touching
/// stdout at all.
fn should_show_tool_output(tool_name: &str, verbose: bool) -> bool {
    verbose || tool_name == "patch"
}

pub(crate) fn print_agent_event(event: grace::agent::AgentEvent, skin: &Skin, verbose: bool) {
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
        grace::agent::AgentEvent::ToolCallEnd { name, result, elapsed } => {
            // Tool output is noisy by default — `patch` is the one tool
            // whose result (a diff) is the point of looking at it, so it
            // always shows; everything else (read_file/run_terminal/etc.)
            // is hidden unless `--verbose`/`/verbose` is on. The one-line
            // "call + timing" summary below still always prints, so you
            // always see *that* something ran, just not its full body.
            if should_show_tool_output(name, verbose) {
                let rendered = grace::markdown::render_terminal(result, skin);
                for (i, line) in rendered.lines().enumerate() {
                    let prefix = if i == 0 { "  ⎿ " } else { "    " };
                    println!("{}{}{}", skin.paint(Role::ToolDim, ""), prefix, line);
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
    println!("  /verbose              - Toggle tool-output visibility (patch diffs always show)");
    println!();
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::should_show_tool_output;

    #[test]
    fn non_patch_tools_hidden_unless_verbose() {
        assert!(!should_show_tool_output("read_file", false));
        assert!(!should_show_tool_output("run_terminal", false));
        assert!(should_show_tool_output("read_file", true));
        assert!(should_show_tool_output("run_terminal", true));
    }

    #[test]
    fn patch_always_shown_regardless_of_verbose() {
        assert!(should_show_tool_output("patch", false));
        assert!(should_show_tool_output("patch", true));
    }
}
