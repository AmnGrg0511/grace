//! Interactive chat REPL: `run_chat`, per-turn execution, slash-command
//! handlers, and the shared event/status-line formatting used by both
//! chat and one-shot modes.

use crate::core::context::ContextCompressionConfig;
use crate::message::Message;
use crate::session::SessionStore;
use crate::config::settings::PROVIDER_PRESETS;
use crate::ui::skin::{no_color, reset, Role, Skin};

use crate::ui::line_reader::LineReader;

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

/// The startup wordmark printed once above the "chat mode —" hint.
///
/// 6 rows: "GRACE" in dense box-drawing capitals (ANSI Shadow style). The
/// lower half stands out in the skin's main color (the answer role —
/// grace's own output color, so each skin rebrands the mark at once); the
/// upper half recedes in the muted `tool_dim` tier (same as the status bar)
/// so it never competes with the conversation. `Skin::paint` is a no-op when
/// stdout is not a TTY, so piped output gets the plain art with no escapes.
pub fn chat_banner(skin: &Skin) -> String {
    const ART: [&str; 6] = [
        " ██████╗ ██████╗  █████╗  ██████╗███████╗",
        "██╔════╝ ██╔══██╗██╔══██╗██╔════╝██╔════╝",
        "██║  ███╗██████╔╝███████║██║     █████╗",
        "██║   ██║██╔══██╗██╔══██║██║     ██╔══╝",
        "╚██████╔╝██║  ██║██║  ██║╚██████╗███████╗",
        " ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚══════╝",
    ];
    const MAIN_COLOR_START: usize = 3; // rows 3–5 (the lower half) get the main color
    ART.iter()
        .enumerate()
        .map(|(i, line)| {
            let role = if i >= MAIN_COLOR_START {
                Role::Answer
            } else {
                Role::ToolDim
            };
            skin.paint(role, line)
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    system_override: Option<&str>,
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

    // Start on a clean screen so the wordmark sits at the top of the view.
    // One-shot, pre-conversation: not part of the append-only stream path.
    // TTY only — piped/one-shot stdout never receives screen-control escapes.
    use std::io::{IsTerminal, Write};
    if std::io::stdout().is_terminal() {
        let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
    }

    println!(
        "{}\nchat mode — type a message, '/exit' to leave, '/model [name]' to switch models, '/skin [name]' to retheme, '/session' to switch sessions, '/verbose' to toggle tool output.\n",
        chat_banner(&skin)
    );

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
            // After model switch, get context window from the transport first
            // (which resolved it during handle_model_command), falling back to
            // settings and the static table.
            cached_context_window = transport.context_window()
                .or(crate::config::settings::Settings::load().default_context_window)
                .or_else(|| crate::config::settings::context_window_for(
                    transport.current_model().as_deref().unwrap_or(""),
                ));
            continue;
        }
        if let Some(rest) = text.strip_prefix("/skin") {
            handle_skin_command(rest.trim(), &mut skin, &mut reader);
            reader.set_skin(skin);
            continue;
        }
        if let Some(_rest) = text.strip_prefix("/session") {
            handle_session_command(sessions, messages, &mut current_session, &mut reader, memory, &mut session_lock, system_override, skills);
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
                        // Re-prompt on empty input (an empty key would be
                        // persisted and sent as an empty bearer); bail only
                        // on EOF.
                        loop {
                            let Some(raw) = reader
                                .read_line(&format!("API key for this provider (${env_var} not set): "))
                            else {
                                break None;
                            };
                            let trimmed = raw.trim().to_string();
                            if trimmed.is_empty() {
                                println!("key is empty — type it again");
                                continue;
                            }
                            break Some(trimmed);
                        }
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
            // Per-key upsert: a whole-file rewrite would wipe the other
            // providers' keys every time `/model` switched providers.
            if let Err(e) = crate::ui::cli::upsert_env_file(env_var, &key) {
                eprintln!(
                    "[grace] warning: could not save {}: {e}",
                    crate::ui::cli::env_file_path().display()
                );
            }
        }
    }

    transport.set_model(&picked);
    if let Some(m) = transport.current_model() {
        // Force the transport to re-resolve its context window now that the
        // model has changed (set_model above invalidated the cache). The
        // transport's context_window() method fetches from the provider API,
        // falls back to the /models listing, then to the static table.
        let resolved = ctx.or(transport.context_window());
        let mut settings = crate::config::settings::Settings::load();
        settings.default_model = Some(m.clone());
        settings.default_context_window = resolved;
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
#[allow(clippy::too_many_arguments)]
fn handle_session_command(
    sessions: &SessionStore,
    messages: &mut Vec<Message>,
    current_session: &mut Option<String>,
    reader: &mut LineReader,
    memory: &crate::memory::Memory,
    session_lock: &mut Option<crate::session::SessionLock>,
    system_override: Option<&str>,
    skills: &crate::skill::SkillStore,
) {
    // The same assembly path as startup (`config::build_system_prompt`): the
    // `--system` override is honored, durable facts are appended, and a
    // memory error is printed rather than swallowed. `query` is the
    // switched-to session's first user message, which feeds the pre-flight
    // recall — at startup the interactive path can't know what you'll type,
    // but a switch targets a session whose opener is already on record.
    let fresh_system = |query: Option<&str>| {
        match crate::config::build_system_prompt(system_override, memory, skills, sessions, query) {
            Ok(sp) => Message::system(sp),
            Err(e) => {
                println!("error building the system prompt: {e} — using the bare soul");
                Message::system(crate::config::load_soul())
            }
        }
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
            messages.push(fresh_system(None));
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
                    let query = loaded
                        .iter()
                        .find(|m| m.role == crate::message::Role::User)
                        .map(|m| m.content.as_str());
                    messages.clear();
                    messages.push(fresh_system(query));
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
            r = reset(),
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
    let outcome = {
        let mut sink =
            |event: crate::core::lifecycle::AgentEvent<'_>| {
                print_agent_event(event, skin, verbose, &mut stream_state);
            };
        crate::core::run_turn_with_options(
            transport,
            tools,
            messages,
            max_iterations,
            crate::core::TurnOptions::new()
                .with_events(&mut sink)
                .with_interrupt(interrupted.as_ref())
                .with_compression(compression_config)
                // Chat mode streams AND renders markdown: ContentFragment
                // appends each finalized line as markdown (append-only, no
                // re-rendering).  One-shot `--stream` does the same (cli.rs).
                .streaming(transport.supports_streaming()),
        )
    };
    // The agent emits no terminal event after its last ContentFragment, so a
    // trailing line that arrived without a newline would otherwise stay
    // buffered.  Flush it now, before anything else is printed for the turn.
    // (The closure that held the mutable borrow has gone out of scope.)
    flush_stream_to_stdout(&mut stream_state, skin);
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
                    reset(),
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
            // Rewind the transcript to before this turn's user message so a
            // failed turn can be retried cleanly. A blind `pop()` is wrong:
            // by the time a turn errors, assistant/tool messages have usually
            // been pushed already (and the compressor may have rewritten the
            // whole list), so the last message is not the user message.
            if let Some(pos) = messages
                .iter()
                .rposition(|m| m.role == crate::message::Role::User)
            {
                messages.truncate(pos);
            }
            // The user row was persisted before the turn ran; without this a
            // failed turn leaves a user message with no answer in the
            // on-disk history.
            if let Some(sid) = session_id {
                let _ = sessions.delete_last_user_row(sid);
            }
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

/// State for streaming markdown rendering.
///
/// Fragments are accumulated in [`StreamState::buf`].  On each fragment we
/// find the *stable commit boundary* — the largest prefix of the buffer that
/// is a set of fully-finalized markdown blocks which no future fragment can
/// change (complete lines, whole fenced blocks) — render that prefix through
/// [`crate::ui::markdown::render_terminal_colored`], and **append only the
/// newly-rendered bytes** to the terminal.
///
/// This is append-only: nothing is ever cursor-up or erased, so a streamed
/// response renders markdown on *every* terminal, keeps rendering well past
/// the viewport height, and cannot duplicate — each rendered byte is written
/// exactly once.  The in-progress tail (a line with no newline yet, an open
/// code fence, a half-seen table) is held back and flushed when the block
/// completes or the stream finalizes.
#[derive(Default)]
pub struct StreamState {
    /// All raw fragments accumulated so far for the current stream.
    buf: String,
    /// How many bytes of the rendered committed prefix have already been
    /// written to the terminal.  Only the suffix beyond this is emitted.
    emitted: usize,
    /// The rendered form of the committed prefix as of the last emit.  Kept so
    /// we can verify the new render is a byte-prefix extension of it (the
    /// invariant that makes delta-emission duplication-free).
    last_rendered: String,
    /// The render width for this stream, captured once, on first use —
    /// `Some` after the first render, the inner value `None` when there is no
    /// width (no terminal size and no `COLUMNS`).  Pinning it is load-bearing: a
    /// table's wrapped form depends on the width, so a fresh per-render width
    /// would let a mid-stream resize re-wrap an already-committed table and
    /// break the byte-prefix invariant the delta emitter checks.  One-shot
    /// renders (answers, tool results) resolve a fresh width instead.
    width: Option<Option<usize>>,
    /// Set once a commit fault has been made visible (invariant broken or a
    /// write error) so the one-line stderr note is not repeated for every
    /// subsequent delta of the same stream.
    commit_failed: bool,
}

/// The byte offset into `buf` up to which the buffer consists of fully
/// finalized markdown blocks — the safe point to commit and render.
///
/// A block is finalized once it can no longer be altered by later text.  For
/// prose that is a complete line (a line whose newline has arrived); for a
/// fenced code block it is only when the *closing* fence is seen (the box is
/// rendered whole, sized to its widest line, so we cannot commit a partial
/// fence).  Everything from the returned offset to the end is an in-progress
/// tail and must be held back.
///
/// The scan is fence-aware: a blank line or line boundary *inside* an open
/// fence is code content, not a block separator, so it never advances the
/// commit point.
fn stable_commit_endpoint(buf: &str) -> usize {
    let mut commit = 0usize;
    let mut in_fence = false;
    let mut fence_char = '\0';
    let mut fence_len = 0usize;
    let mut line_start = 0usize;

    for (i, b) in buf.as_bytes().iter().enumerate() {
        if *b != b'\n' {
            continue;
        }
        let line = &buf[line_start..i];
        let line_end = i + 1; // include the terminating newline

        if in_fence {
            if is_closing_fence(line, fence_char, fence_len) {
                in_fence = false;
                commit = line_end; // the whole block is finalized now
            }
            // interior line: commit stays put
        } else if let Some((ch, len)) = open_fence(line) {
            in_fence = true;
            fence_char = ch;
            fence_len = len;
            // do NOT advance commit past the opening fence: the block's
            // content is not finalized until the closing fence arrives.
        } else if is_table_row(line) {
            // A table's rendered box depends on every row of it (columns are
            // sized to the whole table, then fitted to the terminal width),
            // so every already-rendered row — borders included — could
            // change if another row arrives: the same prefix-instability as
            // an open fence.  Hold the table's rows until a non-row line
            // ends it.
        } else {
            commit = line_end; // finalizes any held table rows, too
        }
        line_start = i + 1;
    }
    // A trailing line without a newline is in progress — not committed.
    commit
}

/// Whether a complete source line looks like a GFM table row (a line whose
/// trimmed form opens with a pipe).  Erring toward "yes" merely delays that
/// line's commit until the next non-row line — it can never expose a
/// half-sized table.
fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// If `line` (a complete source line, no newline) opens a fenced code block,
/// return the fence character and its run length.  Matches CommonMark: up to
/// three leading spaces, then three or more backticks/tildes.
fn open_fence(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    let mut chars = t.chars();
    let fence = chars.next()?;
    if fence != '`' && fence != '~' {
        return None;
    }
    let run = 1 + chars.take_while(|&c| c == fence).count();
    if run < 3 {
        return None;
    }
    Some((fence, run))
}

/// Whether `line` closes a fence opened with `fence_char` repeated `min_len`
/// times: the line (ignoring trailing whitespace) must be the same character
/// repeated at least `min_len` times and nothing else.
fn is_closing_fence(line: &str, fence_char: char, min_len: usize) -> bool {
    let t = line.trim_end();
    let mut chars = t.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != fence_char {
        return false;
    }
    let run = 1 + chars.take_while(|&c| c == fence_char).count();
    run >= min_len && run == t.chars().count()
}

/// Emit any portion of `rendered` not yet written, then remember it.
///
/// The committed prefix only ever grows by appending *complete* blocks, so a
/// fresh render is always a byte-prefix extension of the previous one; we
/// therefore emit just the suffix.  That is what makes streaming duplication-
/// free: each rendered byte reaches the terminal exactly once, and because we
/// never move the cursor, nothing can be duplicated past the viewport either.
fn emit_committed(stream: &mut StreamState, rendered: &str, out: &mut dyn std::io::Write) {
    // Fail-safe: if the prefix invariant were ever broken, skip this delta
    // rather than emit a corrupted/duplicated slice — but say so, once. A
    // silent skip here would be an invisible loss of output; the note goes
    // to stderr, never into `out`, so the stream's bytes stay untouched.
    if !stream.last_rendered.is_empty() && !rendered.starts_with(&stream.last_rendered) {
        if !stream.commit_failed {
            stream.commit_failed = true;
            eprintln!(
                "[grace] stream error: the append-only prefix invariant broke; \
                 the rest of this block was dropped (output may be incomplete)"
            );
        }
        return;
    }
    let end = rendered.len();
    if end > stream.emitted {
        if let Err(e) = out.write_all(&rendered.as_bytes()[stream.emitted..]) {
            if !stream.commit_failed {
                stream.commit_failed = true;
                eprintln!("[grace] stream error: writing stream output failed: {e}; remaining output may be missing");
            }
        }
        // Keep the same bookkeeping as before (advance past the attempted
        // slice) so a broken destination isn't retried delta after delta.
        stream.emitted = end;
    }
    stream.last_rendered = rendered.to_string();
}

/// Flush the in-progress tail of the current stream to `out`, then reset the
/// streaming state for the next block.
///
/// Called when a non-fragment event (tool call, compression, …) interrupts the
/// stream, and again at end of turn — since no terminal event fires after the
/// last fragment, that final flush is what guarantees a trailing line that
/// arrives without a newline is still emitted.  With color disabled (piped
/// output) the fragments were already written raw as they arrived, so there is
/// nothing buffered to flush.
pub fn finalize_stream(
    stream: &mut StreamState,
    skin: &Skin,
    disable_color: bool,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    if !disable_color && !stream.buf.is_empty() {
        let width = *stream
            .width
            .get_or_insert_with(crate::ui::markdown::terminal_width);
        let rendered = crate::ui::markdown::render_terminal_width(&stream.buf, skin, true, width);
        emit_committed(stream, &rendered, out);
        out.flush()?;
    }
    stream.buf.clear();
    stream.emitted = 0;
    stream.last_rendered.clear();
    Ok(())
}

/// Flush the in-progress tail of a stream to **stdout** — the end-of-turn
/// counterpart of [`print_agent_event`], needed because the agent emits no
/// terminal event after its last [`crate::core::lifecycle::AgentEvent::ContentFragment`].
pub fn flush_stream_to_stdout(stream: &mut StreamState, skin: &Skin) {
    let mut stdout = std::io::stdout();
    let _ = finalize_stream(stream, skin, no_color(), &mut stdout);
}

/// Strip ANSI SGR escape sequences (everything from `\x1b[` to the
/// terminating `m`) so display-width math sees only visible characters.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// How many visual terminal rows `text` occupies when printed at `cols`
/// columns wide.  ANSI escapes are zero-width; wide (CJK) characters count
/// as two columns; a line exactly `cols` wide still fits on one row.  When
/// the terminal width is unknown (piped/tests) falls back to the logical
/// line count, which is the best available approximation.
fn visual_rows(text: &str, cols: Option<usize>) -> usize {
    let cols = match cols {
        Some(c) if c > 0 => c,
        _ => return text.lines().count().max(1),
    };
    text.lines()
        .map(|line| {
            use unicode_width::UnicodeWidthStr;
            let width = strip_ansi(line).width();
            width.div_ceil(cols).max(1)
        })
        .sum::<usize>()
        .max(1)
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

/// Test-support shim so integration tests can compute the exact visual-row
/// count the streaming renderer uses, for replaying captures faithfully.
#[doc(hidden)]
pub fn test_support_visual_rows(text: &str, cols: Option<usize>) -> usize {
    visual_rows(text, cols)
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
            let _ = finalize_stream(stream, skin, disable_color, out);
            let bullet = skin.paint(Role::ToolBullet, "▾");
            let thinking = skin.paint(Role::Thinking, "Thinking");
            writeln!(out, "{} {}", bullet, thinking).ok();
            for line in text.lines() {
                writeln!(out, "  {}", skin.paint(Role::Thinking, line)).ok();
            }
        }
        crate::core::lifecycle::AgentEvent::ToolCallStart { name, arguments } => {
            let _ = finalize_stream(stream, skin, disable_color, out);
            let compact = compact_args(arguments);
            let bullet = skin.paint(Role::ToolBullet, "●");
            // Dim the call itself so it recedes behind the undimmed answer
            // text above/below it — bright name+args read as "response".
            let call = dim(&format!("{name}({compact})"));
            writeln!(out, "{} {call}", bullet).ok();
        }
        crate::core::lifecycle::AgentEvent::ToolCallEnd { name, result, elapsed } => {
            let _ = finalize_stream(stream, skin, disable_color, out);
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
            stream.buf.push_str(fragment);
            if disable_color {
                // Piped output: print fragments raw, no markdown pass.  This is
                // append-only (each fragment written exactly once), so it is
                // already duplication-free and viewport-independent.
                let _ = write!(out, "{fragment}");
            } else {
                // Commit the largest finalized prefix and append only the
                // newly-rendered bytes.  Append-only — no cursor movement — so
                // it renders markdown on any terminal, keeps rendering past the
                // viewport, and cannot duplicate.
                // Pin the width for this stream on first use (see
                // StreamState::width): a mid-stream resize must not re-wrap an
                // already-committed table.
                let width = *stream
                    .width
                    .get_or_insert_with(crate::ui::markdown::terminal_width);
                let commit = stable_commit_endpoint(&stream.buf);
                if commit > 0 {
                    let committed = &stream.buf[..commit];
                    let rendered =
                        crate::ui::markdown::render_terminal_width(committed, skin, true, width);
                    emit_committed(stream, &rendered, out);
                }
            }
            let _ = out.flush();
        }
         crate::core::lifecycle::AgentEvent::ContextCompressed {
            before_tokens,
            after_tokens,
            dropped_messages,
            summary,
        } => {
            let _ = finalize_stream(stream, skin, disable_color, out);
            // Always surfaced, not gated behind --verbose: history silently
            // disappearing is exactly the kind of thing a user needs told.
            let summary_suffix = match summary {
                Some(s) if !s.is_empty() => format!("\n  · summary: {s}"),
                _ => String::new(),
            };
            writeln!(
                out,
                "{}",
                skin.paint(
                    Role::ToolDim,
                    &format!(
                        "  · context compressed: {before_tokens} → {after_tokens} tok \
                         ({dropped_messages} older messages elided){summary_suffix}"
                    )
                )
            )
            .ok();
        }
        crate::core::lifecycle::AgentEvent::DelegationStart { task, budget } => {
            let _ = finalize_stream(stream, skin, disable_color, out);
            let bullet = skin.paint(Role::ToolBullet, "⇢");
            writeln!(out, "{bullet} delegating (budget {budget}): {}", compact_args(task)).ok();
        }
        crate::core::lifecycle::AgentEvent::DelegationEnd {
            task: _,
            iterations,
            ok,
        } => {
            let _ = finalize_stream(stream, skin, disable_color, out);
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
mod banner_tests {
    use super::chat_banner;
    use crate::ui::skin::SOLARIS;

    #[test]
    fn banner_is_a_compact_box_wordmark() {
        // Structural invariants, not a copy of the constant: the wordmark
        // must stay 6 compact rows (it prints above the status line on every
        // chat start), stay within a 43-col budget (well under a third of a
        // terminal — no wall of art), use only box-drawing/block characters,
        // and — with no TTY in tests — be free of any leaked ANSI escapes.
        let banner = chat_banner(&SOLARIS);
        assert!(!banner.contains('\x1b'), "no ANSI escapes in non-TTY banner: {banner:?}");
        let lines: Vec<&str> = banner.lines().collect();
        assert_eq!(lines.len(), 6, "expected 6 art rows: {banner:?}");
        for line in &lines {
            // chars, not bytes: the box glyphs are 3 bytes each in UTF-8.
            assert!(line.chars().count() <= 43, "wordmark must stay within 43 cols: {line:?}");
            assert!(
                line.chars().all(|c| matches!(c, ' ' | '█' | '═' | '║' | '╗' | '╔' | '╚' | '╝')),
                "wordmark must use only box/block characters: {line:?}"
            );
        }
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
        // Flush the in-progress tail so the captured bytes represent a fully
        // completed stream (as a real turn is finalized at its end).
        let _ = finalize_stream(&mut stream, &skin, !color, &mut out);
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
        // The emitted output must contain styled bold and heading — and since
        // only the *rendered* committed text is ever appended (never the raw
        // fragments), the RAW markdown ("**bold**") must NOT appear in the
        // captured output, and no byte is ever written twice.
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
        // A tool call finalizes (flushes + resets) the current stream, so the
        // second stream starts from a completely blank slate — and because no
        // content is ever re-emitted, the first stream's text stays exactly once.
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        // Force color on (tests have no PTY) so the color/commit path runs.
        std::env::set_var("CLICOLOR_FORCE", "1");

        // First stream — a complete line commits the moment its newline lands.
        print_agent_event_to(
            AgentEvent::ContentFragment("first "),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        print_agent_event_to(
            AgentEvent::ContentFragment("stream\n"),
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

        // Tool call interrupts (finalizes) the stream
        print_agent_event_to(
            AgentEvent::ToolCallEnd {
                name: "read",
                result: "file contents",
                elapsed: std::time::Duration::from_millis(10),
            },
            &skin,
            false,
            &mut stream,
            &mut out,
        );

        // Second stream
        print_agent_event_to(
            AgentEvent::ContentFragment("second stream\n"),
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
        // …and the first stream is still exactly once (nothing re-emitted).
        let first_count = final_out.matches("first stream").count();
        assert_eq!(
            first_count, 1,
            "first stream must not be re-emitted, got {first_count} in: {final_out:?}"
        );

        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn raw_fragments_not_duplicated_in_color_output() {
        // Append-only regression test: fragments are only ever rendered and
        // committed as they form complete lines; the trailing partial line is
        // flushed once when the stream finalizes.  So the intermediate partial
        // "**bold" (before " text**" completed the line) is never emitted as a
        // line on its own, and no byte is written more than once.
        let out = render_events(
            &[
                AgentEvent::ContentFragment("**bold"),
                AgentEvent::ContentFragment(" text**"),
            ],
            true,
        );
        // The completed buffer renders as styled "bold text".  We should NOT
        // see the incomplete intermediate "**bold" as a line on its own — that
        // would indicate the open line was emitted before it completed.
        let lines: Vec<&str> = out.lines().collect();
        let standalone_partial = lines.contains(&"**bold");
        assert!(
            !standalone_partial,
            "incomplete open line leaked as standalone line: {lines:?}"
        );
    }

    #[test]
    fn visual_rows_counts_wrapped_lines() {
        // A single logical line spanning multiple terminal rows must count
        // as multiple visual rows — this is the number the cursor-up uses,
        // so under-counting is exactly what leaves stale content on screen.
        assert_eq!(visual_rows("abc", Some(3)), 1);
        assert_eq!(visual_rows("abcd", Some(3)), 2);
        assert_eq!(visual_rows("abcdef", Some(3)), 2);
        assert_eq!(visual_rows("abcdefg", Some(3)), 3);
        assert_eq!(visual_rows("a\nb", Some(3)), 2);
        assert_eq!(visual_rows("", Some(3)), 1);
    }

    #[test]
    fn visual_rows_falls_back_to_logical_lines_without_a_terminal() {
        assert_eq!(visual_rows("abc\ndef", None), 2);
    }

    #[test]
    fn visual_rows_ignores_ansi_escapes_for_width() {
        // The render adds styling escapes that occupy zero visual columns;
        // they must not inflate the wrap count.
        let styled = "\x1b[1mabcdef\x1b[0m";
        assert_eq!(visual_rows(styled, Some(3)), 2);
    }

    #[test]
    fn wrapped_content_emitted_once_via_append() {
        // A single logical line that wraps to several visual rows at a narrow
        // width must still be emitted exactly once, with NO cursor movement.
        // Append-only rendering has no cursor-up to get wrong: the line is
        // written once (as its newline arrives) and the terminal soft-wraps it.
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        std::env::set_var("CLICOLOR_FORCE", "1");
        std::env::set_var("COLUMNS", "10");
        std::env::set_var("LINES", "24");

        let long = "abcdefghijklmnopqrstuvwxyz";
        // Arrives in two fragments; the line commits only once its newline lands.
        print_agent_event_to(
            AgentEvent::ContentFragment(long),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        // Nothing is emitted yet — the line is still open (no newline).
        assert!(out.is_empty(), "an open line must not be emitted yet: {out:?}");
        print_agent_event_to(
            AgentEvent::ContentFragment("!\n"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        let s = String::from_utf8(out.clone()).unwrap();

        // The full line is present exactly once …
        assert_eq!(
            s.matches("abcdefghijklmnopqrstuvwxyz!").count(),
            1,
            "wrapped line must be emitted once: {s:?}"
        );
        // … and no cursor movement of any kind was used.
        assert!(!s.contains("\x1b[J"), "append mode must not clear the screen: {s:?}");
        for n in 1..=24 {
            assert!(
                !s.contains(&format!("\x1b[{n}A")),
                "append mode must not cursor-up: {s:?}"
            );
        }

        std::env::remove_var("COLUMNS");
        std::env::remove_var("LINES");
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn oversized_block_keeps_rendering_past_viewport() {
        // Content as tall as (or taller than) the viewport keeps rendering in
        // markdown and is appended — no freeze, no cursor movement, no
        // duplication.  Nothing is re-rendered in place: each line is written
        // exactly once as it commits, and the terminal's own scrollback handles
        // the overflow.
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        std::env::set_var("CLICOLOR_FORCE", "1");
        std::env::set_var("COLUMNS", "20");
        std::env::set_var("LINES", "6");

        for chunk in ["aaa\nbbb\n", "ccc\nddd\n", "eee\nfff\n"] {
            print_agent_event_to(
                AgentEvent::ContentFragment(chunk),
                &skin,
                false,
                &mut stream,
                &mut out,
            );
        }
        let s = String::from_utf8(out.clone()).unwrap();

        // Every line is present …
        for word in ["aaa", "bbb", "ccc", "ddd", "eee", "fff"] {
            assert!(s.contains(word), "missing {word} in: {s:?}");
        }
        // … exactly once (no duplication from re-rendering).
        for word in ["aaa", "bbb", "ccc", "ddd", "eee", "fff"] {
            assert_eq!(s.matches(word).count(), 1, "{word} duplicated in: {s:?}");
        }
        // And no cursor movement of any kind was used.
        assert!(!s.contains("\x1b[J"), "append mode must not clear: {s:?}");
        for n in 1..=24 {
            assert!(!s.contains(&format!("\x1b[{n}A")), "no cursor-up: {s:?}");
        }

        std::env::remove_var("COLUMNS");
        std::env::remove_var("LINES");
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn stable_commit_endpoint_tracks_complete_lines_and_fences() {
        // A complete line commits at its terminating newline …
        assert_eq!(stable_commit_endpoint("hello\n"), 6);
        // … but a trailing line with no newline is in progress and held back.
        assert_eq!(stable_commit_endpoint("hello\nworld"), 6);

        // A fenced block is NOT committed until its closing fence arrives: the
        // commit point stays just before the opening fence.
        assert_eq!(stable_commit_endpoint("text\n\n```rust\nlet x = 1;\n"), 6);
        // … and once the closing fence lands, the whole block is committed.
        assert_eq!(
            stable_commit_endpoint("text\n\n```rust\nlet x = 1;\n```\n"),
            "text\n\n```rust\nlet x = 1;\n```\n".len()
        );

        // A blank line INSIDE an open fence is code content, not a boundary —
        // and the never-closed fence commits nothing at all.
        assert_eq!(stable_commit_endpoint("```python\n\ndef f(): pass\n"), 0);

        // A table is held like a fence: the rendered box is sized to the
        // widest line, so a row is not final until a non-row line ends it.
        let intro = "text\n";
        let rows = "| a | bb |\n| --- | --- |\n| ccc | dd |\n";
        assert_eq!(stable_commit_endpoint(&format!("{intro}{rows}")), intro.len());
        // A following non-row line finalizes the table and commits it whole.
        assert_eq!(
            stable_commit_endpoint(&format!("{intro}{rows}after\n")),
            intro.len() + rows.len() + "after\n".len()
        );
    }

    #[test]
    fn emit_committed_reports_a_commit_fault_instead_of_dropping_silently() {
        // Regression (G9): the fail-safe used to `return` without saying a
        // word — a broken prefix invariant, or a dead write destination,
        // meant silently missing output. The delta is still skipped
        // (emitting a corrupted slice would be worse), but the fault must be
        // flagged exactly once per stream and stay flagged.
        let mut stream = StreamState::default();
        let mut out: Vec<u8> = Vec::new();
        emit_committed(&mut stream, "first line\n", &mut out);
        assert_eq!(out, b"first line\n");
        assert!(!stream.commit_failed);

        // A render that does not extend the previous one breaks the prefix
        // invariant: nothing may be emitted, and no later delta either.
        let mut broken: Vec<u8> = Vec::new();
        emit_committed(&mut stream, "something else entirely\n", &mut broken);
        assert!(broken.is_empty(), "a corrupted slice must never be emitted: {broken:?}");
        assert!(stream.commit_failed, "the fault must be flagged");

        let mut after: Vec<u8> = Vec::new();
        emit_committed(&mut stream, "something else entirely\nmore\n", &mut after);
        assert!(after.is_empty(), "after a fault the delta stays skipped: {after:?}");
        assert!(stream.commit_failed, "the flag stays set for the rest of the stream");
    }

    #[test]
    fn emit_committed_flags_a_failed_write_without_retrying_forever() {
        struct FailWrite;
        impl std::io::Write for FailWrite {
            fn write(&mut self, _b: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe is gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // A dead destination is reported once; the bookkeeping still
        // advances past the attempted slice so the same bytes are not
        // retried on every subsequent delta.
        let mut stream = StreamState::default();
        emit_committed(&mut stream, "some output\n", &mut FailWrite);
        assert!(stream.commit_failed, "a write error must be flagged");
        assert_eq!(stream.emitted, "some output\n".len(), "bookkeeping advances past the failed slice");
    }

    #[test]
    fn fenced_code_block_emitted_once_when_closed() {
        // A fenced code block must not be emitted until it is complete (its
        // closing fence has arrived) — the box is rendered whole, sized to its
        // widest line — and it must then be emitted exactly once (no dupes).
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        std::env::set_var("CLICOLOR_FORCE", "1");

        // Opening fence + a content line: the fence is still open, so its
        // interior is held back, but the prose line before it has committed.
        print_agent_event_to(
            AgentEvent::ContentFragment("Intro line.\n\n```rust\n"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        print_agent_event_to(
            AgentEvent::ContentFragment("let x = 41 + 1;\n"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        let mid = String::from_utf8(out.clone()).unwrap();
        assert!(
            mid.contains("Intro line."),
            "complete prose before the fence should commit: {mid:?}"
        );
        assert!(
            !mid.contains("let x = 41 + 1;"),
            "code must not be emitted before its fence closes: {mid:?}"
        );

        // Closing fence: the whole block commits and renders exactly once.
        print_agent_event_to(
            AgentEvent::ContentFragment("```\n"),
            &skin,
            false,
            &mut stream,
            &mut out,
        );
        let s = String::from_utf8(out.clone()).unwrap();
        // The code is syntax-highlighted (ANSI breaks the text into runs), so
        // inspect the plain, escape-stripped text for content.
        let plain = strip_ansi(&s);
        assert!(
            plain.contains("let x = 41 + 1;"),
            "code should render once the fence closes: {plain:?}"
        );
        assert!(
            plain.contains('┌') && plain.contains('└'),
            "code box borders expected: {plain:?}"
        );
        assert_eq!(
            plain.matches("let x = 41 + 1;").count(),
            1,
            "code line must be emitted exactly once: {plain:?}"
        );
        assert_eq!(
            plain.matches('┌').count(),
            1,
            "box top border must appear exactly once: {plain:?}"
        );

        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn render_is_prefix_stable_under_line_extension() {
        // The streaming invariant: rendering a line-complete prefix must be a
        // byte prefix of rendering that prefix plus more complete lines.
        // Violating it makes the duplication guard drop output entirely.
        let skin = crate::ui::skin::SOLARIS;
        let r = |s: &str| crate::ui::markdown::render_terminal_colored(s, &skin, true);

        // The thinking-model case that broke streaming: content leading with
        // blank lines (emitted after the reasoning tokens).
        assert_eq!(r("\n\n"), "");
        assert!(r("\n\nPONG\n").starts_with(&r("\n\n")));
        assert!(r("PONG\n").starts_with(&r("\n\n")));

        // Prose, then a table whose later row is wider (box width changes
        // mid-table — stable only because the table commits atomically).
        let base = "intro\n\n";
        let table = "| a | bb |\n| --- | --- |\n";
        let wider = "| ccc | dd |\n";
        assert!(r(base).starts_with(""));
        assert!(r(&format!("{base}{table}")).starts_with(&r(base)));
        assert!(r(&format!("{base}{table}{wider}")).starts_with(&r(base)));

        // Code fences stay atomic; the paragraph before renders stably.
        let pre = "before the code:\n";
        let fence = "```rust\nlet x = 1;\n```\n";
        assert!(r(&format!("{pre}{fence}")).starts_with(&r(pre)));
    }

    #[test]
    fn short_answer_with_leading_blank_lines_still_flushes() {
        // A thinking model can answer "\n\nPONG": two blank lines then a
        // short final line with no trailing newline.  No line ever commits
        // anything that renders non-empty until the turn ends, so the
        // end-of-turn flush is the ONLY thing that can print the answer.
        let _lock = crate::util::test_support::env_guard();
        std::env::set_var("CLICOLOR_FORCE", "1");
        let skin = crate::ui::skin::SOLARIS;
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        for frag in ["\n\n", "PONG"] {
            print_agent_event_to(
                AgentEvent::ContentFragment(frag),
                &skin,
                false,
                &mut stream,
                &mut out,
            );
        }
        let _ = finalize_stream(&mut stream, &skin, false, &mut out);
        let s = String::from_utf8(out).unwrap();
        let plain = strip_ansi(&s);
        assert!(plain.contains("PONG"), "answer text must reach the terminal: {s:?}");
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn long_mixed_document_streams_without_raw_or_dupes() {
        // Regression: a markdown document past the old viewport-height limit
        // must stream as fully rendered output — the append-only deltas must
        // reassemble into exactly what a plain full render would produce
        // (byte for byte, so no raw fences, missing lines, or duplicates).
        let _lock = crate::util::test_support::env_guard();
        std::env::set_var("CLICOLOR_FORCE", "1");
        let skin = crate::ui::skin::SOLARIS;
        // Note the LEADING "\n\n": thinking-model gateways emit reasoning
        // tokens first, and the assistant content then starts with blank
        // lines.  That exact shape used to freeze the whole stream.
        let doc = "\n\n\
                   Opening paragraph with **bold** and `inline code`.\n\n\
                   A table, then code:\n\n\
                   | name | score |\n\
                   | --- | --- |\n\
                   | alice | 10 |\n\
                   | bob | 7 |\n\n\
                   First section has a code block:\n\
                   ```rust\n\
                   let mut v = vec![1, 2, 3];\n\
                   v.push(4);\n\
                   ```\n\
                   \n\
                   ### Heading after the block\n\
                   \n\
                    - bullet one\n\
                    - bullet two with `code` inside\n\
                    \n\
                    A nested list with styling in the children:\n\
                    \n\
                    * Parent item text here:\n\
                    \x20\x20* **Abacus** and bead counting frames\n\
                    \x20\x20* A nested item that is deliberately much longer than the parent line above it so the box and lines must stay stable while it arrives\n\
                    \x20\x20* Mixed **bold** and *italic* in one item\n\
                    \n\
                    A second fence, longer lines:   \n\
                   ```cpp\n\
                   std::vector<int> v = {1, 2, 3};\n\
                   v.push_back(4);\n\
                   ```\n\
                   \n\
                   Trailing line without newline";
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        // Character chunks of 7: splits mid-word, mid-fence, mid-escape —
        // the nastiest realistic fragment boundaries, and many re-renders.
        let chunks: Vec<String> = doc
            .chars()
            .collect::<Vec<char>>()
            .chunks(7)
            .map(|c| c.iter().collect())
            .collect();
        for ch in &chunks {
            print_agent_event_to(
                AgentEvent::ContentFragment(ch.as_str()),
                &skin,
                false,
                &mut stream,
                &mut out,
            );
        }
        let _ = finalize_stream(&mut stream, &skin, false, &mut out);
        let s = String::from_utf8(out).unwrap();

        assert!(!s.contains("```"), "raw fences must never reach the terminal: {s:?}");
        let expected = crate::ui::markdown::render_terminal_colored(doc, &skin, true);
        assert_eq!(s, expected, "streamed deltas must reassemble into the full render");

        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }

    #[test]
    fn streamed_wide_table_fits_the_terminal_width() {
        // A table whose natural width exceeds the terminal must stream as
        // wrapped rows that fit the width (no soft-wrap at the edge), keep
        // every byte of content, and reassemble into the full render.
        let _lock = crate::util::test_support::env_guard();
        std::env::set_var("CLICOLOR_FORCE", "1");
        std::env::set_var("COLUMNS", "60");
        let skin = crate::ui::skin::SOLARIS;
        let doc = format!(
            "| name | score |\n| --- | --- |\n| {} | 10 |\n",
            "a".repeat(120)
        );
        let mut stream = StreamState::default();
        let mut out = Vec::new();
        for frag in doc.as_bytes().chunks(13) {
            let frag = std::str::from_utf8(frag).unwrap();
            print_agent_event_to(
                AgentEvent::ContentFragment(frag),
                &skin,
                false,
                &mut stream,
                &mut out,
            );
        }
        let _ = finalize_stream(&mut stream, &skin, false, &mut out);
        let s = String::from_utf8(out).unwrap();

        // No table row may exceed the terminal width the stream pinned.
        let width = crate::ui::markdown::terminal_width().unwrap();
        use unicode_width::UnicodeWidthStr;
        let wide_rows: Vec<String> = s.lines().map(strip_ansi).filter(|l| l.contains('│')).collect();
        assert!(!wide_rows.is_empty(), "expected a rendered table: {s:?}");
        assert!(
            wide_rows.iter().all(|l| l.width() <= width),
            "every table row must fit the terminal: {s:?} width={width}"
        );
        // No content is lost to the wrap.
        let plain: String = s.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert!(
            plain.matches('a').count() >= 120,
            "table content lost in the wrap: {plain:?}"
        );
        assert_eq!(
            s,
            crate::ui::markdown::render_terminal_colored(&doc, &skin, true),
            "streamed deltas must reassemble into the full render"
        );

        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");
    }
}
