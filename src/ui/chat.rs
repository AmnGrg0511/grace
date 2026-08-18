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
        "{}\nchat mode — type a message, \"/\" for the command palette, '/exit' to leave, '/model [name]' to switch models, '/skin [name]' to retheme, '/session' to switch sessions, '/verbose' to toggle tool output.\n",
        chat_banner(&skin)
    );

    let started_at = std::time::Instant::now();
    // Loaded once per chat session (not re-read from disk every turn) —
    // `/model` updates this in-memory copy too so the status bar reflects
    // a mid-chat model switch without another disk read.
    let mut cached_context_window = crate::config::settings::Settings::load().default_context_window;
    // The provider's token count from the last completed turn — the status
    // line's context bar prefers this real number over the local estimate.
    // Seeded from the session store so a resumed session starts at the real
    // count instead of an estimate that "jumps" once the first turn lands a
    // provider-measured number. `None` for a brand-new session or a provider
    // that omits usage.
    let mut last_usage: Option<crate::transport::TokenUsage> = current_session
        .as_deref()
        .and_then(|sid| sessions.load_usage(sid).ok().flatten());

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
    // Input queued by the command palette (a bare `/` line): consumed by the
    // next iteration as if it had been typed.
    let mut pending_input: Option<String> = None;

    loop {
        print_status_line(
            &skin,
            transport,
            messages,
            started_at,
            cached_context_window,
            last_usage,
            compression_config,
            tools.is_read_only(),
        );
        let line = if let Some(queued) = pending_input.take() {
            queued
        } else {
            // rustyline draws its own prompt glyph via readline(prompt); the
            // plain fallback prints it manually inside LineReader::read_line.
            let Some(line) = reader.read_line(&prompt_label(&skin)) else {
                if !is_rustyline {
                    // Plain fallback: blank line before exit for parity with
                    // the old loop's trailing newline behavior on EOF.
                }
                break;
            };
            line
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // A bare `/` opens the command palette over the single registry; the
        // chosen command is re-run as if the user had typed it.
        if text == "/" {
            let items: Vec<crate::ui::picker::Pick> = crate::ui::commands::SLASH_COMMANDS
                .iter()
                .map(|c| crate::ui::picker::Pick {
                    id: c.name.to_string(),
                    label: format!("/{}", c.name),
                    sublabel: Some(c.summary.to_string()),
                })
                .collect();
            if let Some(name) = crate::ui::picker::pick(&items, &skin, "pick a command (type to filter)")
            {
                pending_input = Some(format!("/{name} "));
            }
            continue;
        }
        // A slash command is its first *word*: `/modelxyz` is model text, not
        // a mistyped `/model`.
        let cmd_word = text.split_whitespace().next().unwrap_or(text);
        let rest = text[cmd_word.len()..].trim();
        match cmd_word.strip_prefix('/') {
            Some("exit" | "quit") => {
                println!("goodbye.");
                break;
            }
            Some("help" | "commands") => {
                print_slash_commands_help();
                continue;
            }
            Some("model") => {
                handle_model_command(transport, rest, &mut reader, &skin, &mut cached_context_window);
                continue;
            }
            Some("skin") => {
                handle_skin_command(rest, &mut skin, &mut reader);
                reader.set_skin(skin);
                continue;
            }
            Some("session") => {
                handle_session_command(sessions, messages, &mut current_session, &mut reader, memory, &mut session_lock, system_override, skills, &mut last_usage);
                continue;
            }
            Some("verbose") => {
                verbose = !verbose;
                println!(
                    "tool output {} (read/bash bodies {}; edit diffs always show).",
                    if verbose { "shown" } else { "hidden" },
                    if verbose { "visible" } else { "hidden" }
                );
                continue;
            }
            Some("readonly") => {
                // Read-only posture: the registry hides write/edit/bash/
                // delegate from the model's specs and refuses them in
                // execute. The flag is shared, so delegated sub-agents and
                // their later builds inherit whatever we set here.
                match rest.split_whitespace().next() {
                    Some("on") => tools.set_read_only(true),
                    Some("off") => tools.set_read_only(false),
                    Some(other) => {
                        println!(
                            "usage: /readonly [on|off] (no argument toggles) — got '{other}'"
                        );
                        continue;
                    }
                    None => tools.set_read_only(!tools.is_read_only()),
                }
                println!(
                    "read-only mode {}.",
                    if tools.is_read_only() {
                        "on — write, edit, bash, and delegate are unavailable until /readonly off"
                    } else {
                        "off — all tools restored"
                    }
                );
                continue;
            }
            Some(_) | None => {
                // Not a known command: unknown `/xyz` goes to the model as
                // typed (it may be asking about a command or a path).
            }
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
        last_usage = run_one_chat_turn(
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
        // Persist the provider's real count so a later resume of this session
        // starts from it (see the startup seed above) instead of an estimate.
        if let (Some(sid), Some(usage)) = (current_session.as_deref(), last_usage) {
            if let Err(e) = sessions.save_usage(sid, usage) {
                println!("warning: could not persist usage: {e}");
            }
        }
    }
}

/// One destination `/model` can switch to: a named endpoint with the model
/// id to point it at, plus display bookkeeping.
#[derive(Debug, Clone)]
struct ModelCandidate {
    name: String,
    model: String,
    base_url: String,
    key_env: String,
    context_window: Option<u32>,
}

/// Everything `/model` can target right now: the endpoints the user has
/// previously switched to (persisted in settings) plus the known provider
/// presets' models. De-duplicated by (base_url, model) — a persisted entry
/// for the same target keeps its (custom) name.
fn model_candidates(settings: &crate::config::settings::Settings) -> Vec<ModelCandidate> {
    let mut out: Vec<ModelCandidate> = Vec::new();
    let mut known: Vec<(String, String)> = Vec::new();
    // The endpoint the user is currently configured on, so `/model` can
    // always offer "switch back to where I am now" without the user typing
    // its URL as a custom endpoint.
    if let (Some(url), Some(model)) = (&settings.default_base_url, &settings.default_model) {
        out.push(ModelCandidate {
            name: friendly_endpoint_name(url),
            model: model.clone(),
            base_url: url.clone(),
            key_env: crate::config::settings::env_var_for_base_url(url).to_string(),
            context_window: settings
                .default_context_window
                .or_else(|| crate::config::settings::context_window_for(model)),
        });
        known.push((url.clone(), model.clone()));
    }
    for e in &settings.endpoints {
        if known.contains(&(e.base_url.clone(), e.model.clone())) {
            continue;
        }
        out.push(ModelCandidate {
            name: e.name.clone(),
            model: e.model.clone(),
            base_url: e.base_url.clone(),
            key_env: e.key_env.clone(),
            context_window: None,
        });
        known.push((e.base_url.clone(), e.model.clone()));
    }
    for preset in PROVIDER_PRESETS {
        for m in preset.models {
            if known.contains(&(preset.base_url.to_string(), m.id.to_string())) {
                continue;
            }
            out.push(ModelCandidate {
                name: preset.label.to_string(),
                model: m.id.to_string(),
                base_url: preset.base_url.to_string(),
                key_env: preset.env_var.to_string(),
                context_window: Some(m.context_window),
            });
        }
    }
    out
}

/// `/model <query>` resolution, pure so it is testable: an exact
/// (case-insensitive) match on endpoint name or model id wins outright;
/// otherwise a case-insensitive substring match. Returns 0 (no match), 1
/// (unambiguous), or N (ambiguous — the caller shows the list).
fn match_model_query(items: &[ModelCandidate], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    let exact: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name.eq_ignore_ascii_case(&q) || c.model.eq_ignore_ascii_case(&q))
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    items
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.name.to_lowercase().contains(&q) || c.model.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Print a candidate compactly: `name — model (base_url)`.
fn describe_candidate(c: &ModelCandidate) -> String {
    let ctx = match c.context_window {
        Some(w) => format!("{}k ctx", w / 1000),
        None => String::new(),
    };
    if c.base_url.is_empty() {
        format!("{} — {} (custom endpoint)", c.name, c.model)
    } else {
        format!(
            "{} — {} ({} · {})",
            c.name,
            c.model,
            short_url(&c.base_url),
            if ctx.is_empty() { "?" } else { ctx.as_str() }
        )
    }
}

/// Host-only view of a base URL for compact picker rows.
fn short_url(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// A human label for an endpoint URL: the host without scheme, port, common
/// `api.`/`www.` prefix, and TLD. `https://api.siemens.com/llm/v1` →
/// `siemens`; `http://localhost:11434/v1` → `localhost`.
fn friendly_endpoint_name(url: &str) -> String {
    let host = short_url(url);
    let host = host.split(':').next().unwrap_or(&host);
    let host = host
        .strip_prefix("api.")
        .or_else(|| host.strip_prefix("www."))
        .unwrap_or(host);
    let mut labels = host.split('.');
    let first = labels.next().unwrap_or(host);
    // Drop the TLD for multi-label hosts (`api.siemens.com` → `siemens`),
    // keep single-label hosts (`localhost`) as-is.
    if labels.next().is_some() {
        first.to_string()
    } else {
        host.to_string()
    }
}

/// Fetch the target endpoint's own model list (best-effort; a slow or
/// keyless endpoint simply yields the static list).
fn fetched_models(base_url: &str, key_env: &str) -> Vec<crate::transport::ModelInfo> {
    if base_url.is_empty() {
        return Vec::new();
    }
    if base_url == crate::transport::copilot::BASE_URL {
        return Vec::new(); // copilot enumerates via its own session flow
    }
    let key = std::env::var(key_env).unwrap_or_default();
    let transport = crate::transport::http::HttpTransport::new(base_url, key);
    crate::transport::ProviderTransport::list_models(&transport).unwrap_or_default()
}

/// `/model` — pick an endpoint (no arg) or resolve one (`<name>`), then
/// switch the transport to it. Persists the choice (model, endpoint, and
/// context window) to ~/.grace/config.toml so it sticks across restarts.
#[allow(clippy::too_many_arguments)]
fn handle_model_command(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    arg: &str,
    reader: &mut LineReader,
    skin: &Skin,
    cached_context_window: &mut Option<u32>,
) {
    if transport.current_model().is_none() {
        println!(
            "this transport ({}) has no switchable model.",
            transport.name()
        );
        return;
    }
    let settings = crate::config::settings::Settings::load();
    let candidates = model_candidates(&settings);

    let target = if arg.is_empty() {
        match pick_model_interactive(&candidates, reader, skin) {
            Some(t) => t,
            None => return,
        }
    } else {
        match match_model_query(&candidates, arg) {
            m if m.len() == 1 => candidates[m[0]].clone(),
            m if m.is_empty() => {
                println!("no model or endpoint matching \"{arg}\". Available:");
                for c in &candidates {
                    println!("  {}", describe_candidate(c));
                }
                println!("(or /model with no argument for the picker)");
                return;
            }
            m => {
                println!("\"{arg}\" matches more than one target:");
                for &i in &m {
                    println!("  {}", describe_candidate(&candidates[i]));
                }
                println!("be more specific, or /model with no argument for the picker.");
                return;
            }
        }
    };

    apply_model_switch(transport, target, reader, cached_context_window);
}

/// Execute a resolved switch: key handling (env/OAuth/prompt), endpoint
/// re-point, model swap, context-window re-resolution + change notice, and
/// persistence (config.toml endpoint upsert + .env key upsert).
fn apply_model_switch(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    target: ModelCandidate,
    reader: &mut LineReader,
    cached_context_window: &mut Option<u32>,
) {
    let is_copilot_target = target.base_url == crate::transport::copilot::BASE_URL;
    let same_endpoint = transport
        .current_base_url()
        .as_deref()
        .map(|b| short_url(b) == short_url(&target.base_url))
        .unwrap_or(false);

    // A transport whose endpoint is fixed (Copilot) can swap models for its
    // own endpoint but never re-point mid-session. Persist the choice so the
    // *next* launch starts on the new endpoint, and say exactly that.
    if !same_endpoint && !transport.can_repoint_endpoint() {
        let window = target
            .context_window
            .or_else(|| crate::config::settings::context_window_for(&target.model));
        persist_model_choice(&target, window);
        println!(
            "this session is bound to the {} endpoint and cannot re-point mid-chat — this session keeps running; your next `grace` launch starts on {name}.",
            transport.name(),
            name = target.name
        );
        return;
    }

    if !same_endpoint {
        let key = if is_copilot_target {
            // Copilot uses the OAuth device flow, not a typed API key —
            // same path as the onboarding wizard.
            match crate::transport::copilot::get_or_create_token() {
                Ok(t) => Some(t),
                Err(e) => {
                    println!("copilot auth failed: {e}");
                    None
                }
            }
        } else {
            std::env::var(&target.key_env)
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .or_else(|| {
                    // Re-prompt on empty input (an empty key would be
                    // persisted and sent as an empty bearer); bail only on EOF.
                    loop {
                        let Some(raw) = reader
                            .read_line(&format!(
                                "API key for {} (${env_var} not set): ",
                                target.name,
                                env_var = target.key_env
                            ))
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
            if !is_copilot_target {
                println!("no key provided — staying on the current endpoint.");
            }
            return;
        };
        transport.set_endpoint(&target.base_url, &key);
        if !is_copilot_target {
            // Per-key upsert: a whole-file rewrite would wipe the other
            // providers' keys every time `/model` switched providers.
            if let Err(e) = crate::ui::cli::upsert_env_file(&target.key_env, &key) {
                eprintln!(
                    "[grace] warning: could not save {}: {e}",
                    crate::ui::cli::env_file_path().display()
                );
            }
        }
    }

    transport.set_model(&target.model);
    // Re-resolve the window now that model/endpoint changed (the swap
    // invalidated the transport's cache). The static-list value, if any,
    // avoids a network probe the picker already answered.
    let resolved = target.context_window.or(transport.context_window());
    if let (Some(old_w), Some(new_w)) = (*cached_context_window, resolved) {
        if old_w != new_w {
            println!(
                "note: context window changed {} -> {} (compaction budget follows).",
                human_tokens(old_w as u64),
                human_tokens(new_w as u64)
            );
        }
    }
    *cached_context_window = resolved;

    let mut settings = crate::config::settings::Settings::load();
    commit_model_choice(&mut settings, &target, resolved);
    if settings.save().is_err() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml");
        return;
    }
    println!("model switched to \"{}\" (saved to config).", target.model);
}

/// Write the chosen model/endpoint/window into settings (the endpoint list
/// stays one-entry-per-endpoint so the picker's list follows usage).
fn commit_model_choice(
    settings: &mut crate::config::settings::Settings,
    target: &ModelCandidate,
    context_window: Option<u32>,
) {
    settings.default_model = Some(target.model.clone());
    settings.default_context_window = context_window;
    settings.default_base_url = Some(target.base_url.clone());
    crate::config::settings::upsert_endpoint(
        &mut settings.endpoints,
        crate::config::settings::Endpoint {
            name: target.name.clone(),
            base_url: target.base_url.clone(),
            model: target.model.clone(),
            key_env: target.key_env.clone(),
        },
    );
}

/// Persist-for-next-launch variant used when the running transport can't
/// re-point: the change lands on the next startup, not this session.
fn persist_model_choice(target: &ModelCandidate, context_window: Option<u32>) {
    let mut settings = crate::config::settings::Settings::load();
    commit_model_choice(&mut settings, target, context_window);
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    }
}

/// `/model` (no arg) — crossterm picker over the endpoints the candidates
/// know about (persisted ones first), then a model list: what the endpoint
/// reports, merged with the known static models, plus an "other" escape
/// hatch for ids we don't know. Returns `None` on cancel/EOF (no-op).
fn pick_model_interactive(
    candidates: &[ModelCandidate],
    reader: &mut LineReader,
    skin: &Skin,
) -> Option<ModelCandidate> {
    use crate::ui::picker::{pick, Pick};

    // Stage 1: which endpoint — one row per distinct (name, base_url).
    let mut endpoints: Vec<(String, String, String)> = Vec::new(); // name, base_url, model
    for c in candidates {
        if !endpoints
            .iter()
            .any(|(n, u, _)| *n == c.name && *u == c.base_url)
        {
            endpoints.push((c.name.clone(), c.base_url.clone(), c.model.clone()));
        }
    }
    let items: Vec<Pick> = endpoints
        .iter()
        .map(|(name, base_url, model)| Pick {
            id: format!("{name}\u{1}{base_url}"),
            label: name.clone(),
            sublabel: Some(format!("{model} · {}", short_url(base_url))),
        })
        .chain(std::iter::once(Pick {
            id: "\u{1}custom".to_string(),
            label: "Custom OpenAI-compatible endpoint".to_string(),
            sublabel: Some("type a base URL".to_string()),
        }))
        .collect();
    let choice = pick(&items, skin, "pick an endpoint to switch to")?;

    let (name, base_url) = if choice == "\u{1}custom" {
        let raw = reader.read_line("base URL (e.g. https://host/v1): ")?;
        let url = raw.trim().to_string();
        if url.is_empty() {
            return None;
        }
        (friendly_endpoint_name(&url), url)
    } else {
        let mut it = choice.splitn(2, '\u{1}');
        (
            it.next().unwrap_or("").to_string(),
            it.next().unwrap_or("").to_string(),
        )
    };

    // Which key var + known models does this endpoint have?
    let (key_env, known): (String, Vec<(String, Option<u32>)>) = candidates
        .iter()
        .filter(|c| c.name == name && short_url(&c.base_url) == short_url(&base_url))
        .fold(
        (String::new(), Vec::new()),
        |(mut ke, mut known), c| {
            if ke.is_empty() && !c.key_env.is_empty() {
                ke = c.key_env.clone();
            }
            known.push((c.model.clone(), c.context_window));
            (ke, known)
        },
    );
    let key_env = if key_env.is_empty() {
        crate::config::settings::env_var_for_base_url(&base_url).to_string()
    } else {
        key_env
    };

    let (model, ctx) = pick_model_for_endpoint(&base_url, &key_env, &known, reader, skin)?;
    Some(ModelCandidate {
        name,
        model,
        base_url,
        key_env,
        context_window: ctx,
    })
}

/// Stage 2 of the `/model` picker: the endpoint's reported models (best-
/// effort) merged with its known static models, plus "other". Returns
/// `(model_id, context_window_if_known)`.
fn pick_model_for_endpoint(
    base_url: &str,
    key_env: &str,
    known: &[(String, Option<u32>)],
    reader: &mut LineReader,
    skin: &Skin,
) -> Option<(String, Option<u32>)> {
    use crate::ui::picker::{pick, Pick};

    let mut models: Vec<(String, Option<u32>)> = Vec::new();
    for info in fetched_models(base_url, key_env) {
        models.push((info.id, info.context_window));
    }
    for (id, ctx) in known {
        if !models.iter().any(|(m, _)| m == id) {
            models.push((id.clone(), *ctx));
        }
    }
    if models.is_empty() {
        let typed = reader.read_line("model id: ")?;
        let id = typed.trim().to_string();
        return if id.is_empty() { None } else { Some((id, None)) };
    }

    let items: Vec<Pick> = models
        .iter()
        .enumerate()
        .map(|(i, (id, ctx))| Pick {
            id: format!("m{i}"),
            label: id.clone(),
            sublabel: ctx.map(|w| format!("{}k ctx", w / 1000)),
        })
        .chain(std::iter::once(Pick {
            id: "other".to_string(),
            label: "other — type a model id".to_string(),
            sublabel: None,
        }))
        .collect();
    let choice = pick(&items, skin, "pick a model")?;
    if choice == "other" {
        let typed = reader.read_line("model id: ")?;
        let id = typed.trim().to_string();
        return if id.is_empty() { None } else { Some((id, None)) };
    }
    let idx = choice.strip_prefix('m')?.parse::<usize>().ok()?;
    Some(models[idx].clone())
}

const SPINNER_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// The idle status line text: `· model · [context bar] · elapsed`.
fn status_line_content(
    model: &str,
    count: usize,
    measured: bool,
    window: Option<u32>,
    trigger_fraction: f32,
    started: std::time::Instant,
    read_only: bool,
) -> String {
    let secs = started.elapsed().as_secs();
    let time = if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    };
    let bar = context_bar(count, measured, window, trigger_fraction);
    // `[RO]` sits right after the model: the posture is a property of what
    // the model may do, and the line already leads with the model.
    let ro = if read_only { " [RO]" } else { "" };
    format!("· {model}{ro} · {bar} · {time}")
}

/// The thinking spinner: one transient line — `{frame} thinking {m:ss}` —
/// ticking while the turn waits on the model and nothing else is being
/// printed. It shows only in the silent gaps: from turn start until the
/// first event, and after each tool result until the model answers again;
/// [`Spinner::pause`] suppresses it the moment real output flows.
///
/// The line is never newline-terminated, so it always sits exactly on the
/// cursor's line and needs no cursor travel to replace — only column-home +
/// clear-line. Every event write erases it first under the same lock the
/// ticker holds while rendering, so the two can never interleave bytes.
/// Piped (non-TTY) output never sees it at all: everything stays
/// append-only there.
struct SpinnerCore {
    stop: bool,
    /// True while the spinner line is suppressed (real output flowing).
    paused: bool,
    /// True while the cursor sits one line below a live spinner line.
    visible: bool,
    /// True while the cursor sits at column 0 of an empty line. The claim
    /// path may only start there; otherwise it must break the line first.
    at_line_start: bool,
    frame: usize,
    started: std::time::Instant,
    dim: String,
}

pub(crate) struct Spinner {
    core: std::sync::Arc<std::sync::Mutex<SpinnerCore>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Holds the core lock for the duration of one event write, so the ticker
/// thread can never interleave its rewrite inside a multi-line event. Drop
/// is the conservative fallback (treats the write as line-breaking) in case
/// `end_write` is skipped by an early return.
pub(crate) struct SpinnerWriteGuard<'a> {
    core: std::sync::MutexGuard<'a, SpinnerCore>,
}

impl Drop for SpinnerWriteGuard<'_> {
    fn drop(&mut self) {
        // Conservative: an unterminated write is possible, so the next claim
        // breaks the line first. (Normal end_write reports the exact state.)
        self.core.at_line_start = false;
    }
}

impl Spinner {
    const UP: &'static str = "\x1b[F";
    const CLR: &'static str = "\x1b[2K";

    /// Arm the ticker. Without a terminal the thread simply never spawns,
    /// so every method below is a no-op — piped output stays append-only.
    pub(crate) fn start(skin: &Skin) -> Self {
        use std::io::IsTerminal;
        let dim = if no_color() {
            String::new()
        } else {
            skin.style(Role::ToolDim).to_string()
        };
        let core = std::sync::Arc::new(std::sync::Mutex::new(SpinnerCore {
            stop: false,
            paused: false,
            visible: false,
            // The input echo that ended the prompt carried a newline, so the
            // cursor starts on a fresh empty line.
            at_line_start: true,
            frame: 0,
            started: std::time::Instant::now(),
            dim,
        }));
        let thread = if std::io::stdout().is_terminal() {
            let shared = core.clone();
            Some(std::thread::spawn(move || {
            use std::io::Write;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut core = shared.lock().unwrap();
                if core.stop {
                    break;
                }
                if core.paused {
                    continue;
                }
                core.frame = (core.frame + 1) % SPINNER_FRAMES.len();
                let text = Self::render(&core);
                let mut out = std::io::stdout().lock();
                if core.visible {
                    // Redraw in place: the cursor sits on the empty line
                    // BELOW the spinner (the previous write ended \n).
                    let _ = write!(out, "{}{}", Self::UP, Self::CLR);
                } else if !core.at_line_start {
                    // A partial (newline-less) fragment owns the rest of
                    // this line — break it first so the spinner never glues
                    // onto other output.
                    let _ = writeln!(out);
                }
                let _ = writeln!(out, "{text}");
                let _ = out.flush();
                drop(out);
                core.visible = true;
                core.at_line_start = true;
            }
            }))
        } else {
            None
        };
        Self { core, thread }
    }

    fn render(core: &SpinnerCore) -> String {
        let secs = core.started.elapsed().as_secs();
        let time = format!("{}:{:02}", secs / 60, secs % 60);
        let line = format!("{} thinking {time}", SPINNER_FRAMES[core.frame]);
        if core.dim.is_empty() {
            line
        } else {
            format!("{}{line}{}", core.dim, reset())
        }
    }

    /// Suppress the spinner and erase it now: real output is about to land.
    pub(crate) fn pause(&self) {
        use std::io::Write;
        let mut core = self.core.lock().unwrap();
        core.paused = true;
        if core.visible {
            core.visible = false;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{}{}", Self::UP, Self::CLR);
            let _ = out.flush();
            core.at_line_start = true;
        }
    }

    /// Allow the ticker to claim the line again (a tool result just printed;
    /// the model is about to think once more).
    pub(crate) fn resume(&self) {
        self.core.lock().unwrap().paused = false;
    }

    /// Begin an event write: erases the spinner (when visible) so the event
    /// lands on a clean line, and blocks the ticker for the write's
    /// duration. Call [`end_write`](Spinner::end_write) when done.
    pub(crate) fn begin_write(&self) -> SpinnerWriteGuard<'_> {
        use std::io::Write;
        let mut core = self.core.lock().unwrap();
        if !core.paused && core.visible {
            core.visible = false;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{}{}", Self::UP, Self::CLR);
            let _ = out.flush();
        }
        SpinnerWriteGuard { core }
    }

    /// Report how the guarded event write left the cursor: at column 0 of an
    /// empty line, or mid-line (a raw fragment that was not newline-
    /// terminated). The next claim uses this to decide about the break.
    pub(crate) fn end_write(&self, mut guard: SpinnerWriteGuard<'_>, at_line_start: bool) {
        guard.core.at_line_start = at_line_start;
    }

    /// Stop the thread and clear the spinner so the turn's closing output
    /// starts on a clean line.
    pub(crate) fn finish(&mut self) {
        self.pause();
        {
            let mut core = self.core.lock().unwrap();
            core.stop = true;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // A turn that returns early (interrupt, error) must not leak the
        // ticker thread or leave a half-line on screen.
        self.finish();
    }
}

/// Whether one event's terminal output leaves the cursor at the start of an
/// empty line. Everything except the raw (non-colored) stream fragment
/// newline-terminates its output; a raw fragment may end mid-line.
pub(crate) fn event_ends_at_line_start(
    event: &crate::core::lifecycle::AgentEvent<'_>,
    disable_color: bool,
) -> bool {
    match event {
        crate::core::lifecycle::AgentEvent::ContentFragment(f) if disable_color => {
            f.is_empty() || f.ends_with('\n')
        }
        _ => true,
    }
}

#[cfg(test)]
mod spinner_tests {
    use super::*;

    #[test]
    fn spinner_renders_frame_label_and_elapsed() {
        let core = SpinnerCore {
            stop: false,
            paused: false,
            visible: false,
            at_line_start: true,
            frame: 3,
            started: std::time::Instant::now(),
            dim: String::new(),
        };
        let line = Spinner::render(&core);
        assert!(
            line.starts_with("⠸ thinking 0:00"),
            "unexpected line: {line}"
        );
    }

    #[test]
    fn the_idle_line_keeps_one_format() {
        let started = std::time::Instant::now();
        let line = status_line_content("gpt-4o", 4_000, false, Some(10_000), 0.75, started, false);
        // `· model · [context bar] · elapsed`. Heuristic count (measured=
        // false) marks the percent with `~`; a fresh start reads 0:00.
        assert!(
            line.starts_with("· gpt-4o · [███░░░░░] 40% · "),
            "unexpected line: {line}"
        );
        assert!(line.ends_with("0:00"), "unexpected line: {line}");
    }

    #[test]
    fn read_only_mode_marks_the_line_with_an_ro_badge() {
        let started = std::time::Instant::now();
        let line = status_line_content("gpt-4o", 4_000, true, Some(10_000), 0.75, started, true);
        assert!(line.starts_with("· gpt-4o [RO] · "), "unexpected line: {line}");
        // The badge must not disturb the rest of the format.
        let off = status_line_content("gpt-4o", 4_000, true, Some(10_000), 0.75, started, false);
        assert!(line.replacen(" [RO]", "", 1) == off, "only the badge differs: {line:?} vs {off:?}");
    }
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
    last_usage: &mut Option<crate::transport::TokenUsage>,
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
            // A brand-new session has no measured usage yet — the status bar
            // falls back to the estimate until the first turn reports one.
            *last_usage = None;
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
                    // Pick up the switched-to session's own measured usage so
                    // the status bar doesn't keep showing the outgoing
                    // session's count (or an estimate) for a session that has
                    // real usage on record.
                    *last_usage = sessions.load_usage(sid).ok().flatten();
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
) -> Option<crate::transport::TokenUsage> {
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
    // Thinking spinner for the silent waits (TTY only; a no-op when piped).
    let mut spinner = Spinner::start(skin);
    let mut stream_state = StreamState::default();
    let outcome = {
        let mut sink =
            |event: crate::core::lifecycle::AgentEvent<'_>| {
                // Any event is real output: keep the spinner off from here
                // on — except right after a tool result, where the model
                // goes quiet again and the spinner covers that wait.
                let tool_end = matches!(
                    &event,
                    crate::core::lifecycle::AgentEvent::ToolCallEnd { .. }
                );
                if !tool_end {
                    spinner.pause();
                }
                let at_start = event_ends_at_line_start(&event, no_color());
                let guard = spinner.begin_write();
                print_agent_event(event, skin, verbose, &mut stream_state);
                spinner.end_write(guard, at_start);
                if tool_end {
                    spinner.resume();
                }
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
    spinner.finish();
    flush_stream_to_stdout(&mut stream_state, skin);
    match outcome {
        Ok(crate::core::TurnOutcome {
            answer,
            streamed,
            last_usage,
            ..
        }) => {
            if streamed {
                // Already printed live, fragment by fragment. Re-rendering it
                // here would show the whole answer a second time; just close
                // off the streamed block.
                println!("\n");
            } else {
                let glyph = skin.paint(Role::Answer, skin.answer_glyph);
                println!(
                    "\n{} {}\n",
                    glyph,
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
            // Hands the provider's token count to the next status line.
            last_usage
        }
        Err(crate::util::AgentError::Interrupted) => {
            // Tool calls up to this point already ran and are recorded in
            // `messages`/the session — only the final answer is missing.
            // Don't pop the user message: unlike a hard error, there's real
            // partial progress worth keeping in context for the next turn.
            println!("\n(interrupted — back to prompt)\n");
            None
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
            None
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
            // Name in the ToolName role (same as the `/skin` preview's
            // mini-transcript), args in the skin's ToolDim color so the
            // call recedes behind the undimmed answer text above/below it.
            let call = format!(
                "{}{}",
                skin.paint(Role::ToolName, name),
                skin.paint(Role::ToolDim, &format!("({compact})"))
            );
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
                    writeln!(out, "{}{}", skin.paint(Role::ToolDim, prefix), line).ok();
                }
            }
            let tokens = estimate_tokens(result);
            let secs = elapsed.as_secs_f64();
            let timing = if secs >= 1.0 {
                format!("{secs:.1}s")
            } else {
                format!("{}ms", (secs * 1000.0) as u64)
            };
            let prefix = format!(
                "    {} {}",
                skin.paint(Role::ToolDim, "·"),
                skin.paint(Role::ToolBullet, "Σ")
            );
            let rest = dim(&format!(" ~{tokens} tok · {timing}"));
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

#[cfg(test)]
mod context_bar_tests {
    use super::*;

    #[test]
    fn measured_provider_count_gets_no_approx_prefix() {
        assert_eq!(context_bar(5_000, true, None, 0.75), "5000 tok · compacts near ~24k tok");
        assert_eq!(context_bar(5_000, false, None, 0.75), "~5000 tok · compacts near ~24k tok");
    }

    #[test]
    fn a_known_window_renders_the_percent_bar() {
        // 40% of a 10k window.
        let bar = context_bar(4_000, true, Some(10_000), 0.75);
        assert_eq!(bar, "[███░░░░░] 40%");
    }

    #[test]
    fn the_bar_never_clips_above_eight_segments() {
        assert_eq!(context_bar(99_999, true, Some(1_000), 0.75), "[████████] 9999%");
    }

    #[test]
    fn compaction_point_follows_the_trigger_fraction() {
        // 32k fallback window at 0.25 → 8.0k.
        assert!(context_bar(10, true, None, 0.25).contains("compacts near ~8.0k tok"));
    }

    #[test]
    fn a_zero_window_shows_only_the_count() {
        assert_eq!(context_bar(123, false, Some(0), 0.75), "~123 tok");
    }

    #[test]
    fn human_tokens_scales_readably() {
        assert_eq!(human_tokens(950), "950");
        assert_eq!(human_tokens(1_200), "1.2k");
        assert_eq!(human_tokens(32_000), "32k");
    }
}

#[cfg(test)]
mod model_command_tests {
    use super::*;

    fn cand(name: &str, model: &str, base_url: &str, ctx: Option<u32>) -> ModelCandidate {
        ModelCandidate {
            name: name.into(),
            model: model.into(),
            base_url: base_url.into(),
            key_env: "SOME_KEY".into(),
            context_window: ctx,
        }
    }

    #[test]
    fn exact_model_match_wins_case_insensitively() {
        let items = vec![cand("OpenAI", "gpt-4o", "https://api.openai.com/v1", Some(128_000))];
        assert_eq!(match_model_query(&items, "GPT-4O"), vec![0]);
    }

    #[test]
    fn exact_endpoint_name_matches_only_that_endpoint() {
        let items = vec![
            cand(
                "OpenRouter",
                "openai/gpt-4o-mini",
                "https://openrouter.ai/api/v1",
                Some(128_000),
            ),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", Some(128_000)),
        ];
        assert_eq!(match_model_query(&items, "openai"), vec![1]);
    }

    #[test]
    fn substring_match_is_the_fallback_and_can_be_ambiguous() {
        let items = vec![
            cand("OpenRouter", "openai/gpt-4o-mini", "https://openrouter.ai/api/v1", None),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", None),
        ];
        // "gpt-4o" is no exact name/model; it substrings both models.
        assert_eq!(match_model_query(&items, "gpt-4o"), vec![0, 1]);
        assert!(match_model_query(&items, "flux").is_empty());
    }

    #[test]
    fn a_persisted_endpoint_dedups_the_preset_model() {
        let mut settings = crate::config::settings::Settings::default();
        settings.endpoints.push(crate::config::settings::Endpoint {
            name: "MyOpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            key_env: "OPENAI_API_KEY".into(),
        });
        let candidates = model_candidates(&settings);
        let hits: Vec<_> = candidates
            .iter()
            .filter(|c| c.model == "gpt-4o" && c.base_url == "https://api.openai.com/v1")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "MyOpenAI");
    }

    #[test]
    fn upsert_keeps_one_entry_per_base_url() {
        let mut list = Vec::new();
        crate::config::settings::upsert_endpoint(
            &mut list,
            crate::config::settings::Endpoint {
                name: "a".into(),
                base_url: "u".into(),
                model: "m1".into(),
                key_env: "K".into(),
            },
        );
        crate::config::settings::upsert_endpoint(
            &mut list,
            crate::config::settings::Endpoint {
                name: "b".into(),
                base_url: "u".into(),
                model: "m2".into(),
                key_env: "K".into(),
            },
        );
        crate::config::settings::upsert_endpoint(
            &mut list,
            crate::config::settings::Endpoint {
                name: "c".into(),
                base_url: "v".into(),
                model: "m3".into(),
                key_env: "K".into(),
            },
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "b");
        assert_eq!(list[1].name, "c");
    }

    #[test]
    fn short_url_keeps_only_the_host() {
        assert_eq!(short_url("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(short_url("http://localhost:11434/v1"), "localhost:11434");
    }

    #[test]
    fn friendly_endpoint_name_strips_scheme_prefix_and_port() {
        assert_eq!(friendly_endpoint_name("https://api.siemens.com/llm/v1"), "siemens");
        assert_eq!(friendly_endpoint_name("http://localhost:11434/v1"), "localhost");
        assert_eq!(friendly_endpoint_name("https://api.openai.com/v1"), "openai");
    }

    #[test]
    fn the_currently_configured_endpoint_is_a_candidate() {
        let settings = crate::config::settings::Settings {
            default_model: Some("qwen-3.8-27b".into()),
            default_base_url: Some("https://api.siemens.com/llm/v1".into()),
            default_context_window: Some(262_144),
            ..Default::default()
        };
        let candidates = model_candidates(&settings);
        let cur = candidates
            .iter()
            .find(|c| c.base_url == "https://api.siemens.com/llm/v1")
            .expect("configured endpoint is a candidate");
        assert_eq!(cur.name, "siemens");
        assert_eq!(cur.model, "qwen-3.8-27b");
        assert_eq!(cur.context_window, Some(262_144));
    }
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
///
/// The bar prefers the provider's own `prompt_tokens` from the last turn
/// (`measured`); only when the provider didn't report usage does it fall back
/// to the same byte-based estimate the compressor uses (prefix `~`). When the
/// context window is unknown, no bar is drawn (a % against a guessed window
/// would be worse than nothing) — instead the line shows the running count
/// and *where auto-compaction will fire*, so the hidden 32k fallback trigger
/// is never silent.
#[allow(clippy::too_many_arguments)]
pub fn print_status_line(
    skin: &Skin,
    transport: &(dyn crate::transport::ProviderTransport + '_),
    messages: &[crate::message::Message],
    started_at: std::time::Instant,
    cached_context_window: Option<u32>,
    last_usage: Option<crate::transport::TokenUsage>,
    compression_config: &ContextCompressionConfig,
    read_only: bool,
) {
    let model = transport
        .current_model()
        .unwrap_or_else(|| transport.name().to_string());

    // Prefer the provider's real count; fall back to the same estimator the
    // compressor uses, so the bar and the trigger never disagree.
    let (count, measured) = match last_usage {
        Some(u) => (u.prompt_tokens as usize, true),
        None => {
            use crate::util::tokens::TokenCounter;
            let est = crate::util::tokens::default_counter()
                .count_messages(messages)
                .max(1);
            (est, false)
        }
    };
    // Saved context window (loaded once per chat session, not re-read from
    // disk every turn) beats the static lookup table — it covers models
    // only known at runtime.
    let ctx = cached_context_window.or_else(|| crate::config::settings::context_window_for(&model));

    let line = status_line_content(
        &model,
        count,
        measured,
        ctx,
        compression_config.normalized().trigger_fraction,
        started_at,
        read_only,
    );
    if no_color() {
        println!("{line}");
    } else {
        // skin's muted tool-dim color, single dim.
        println!("{}", skin.paint(Role::ToolDim, &line));
    }
}

/// Human-readable token count for the status line: `950`, `1.2k`, `32k`.
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

/// The context portion of the status line, computed without touching stdout
/// so the rule is unit-testable.
///
/// * `count` — tokens in use (provider-measured or locally estimated).
/// * `measured` — whether `count` came from the provider (no `~` prefix).
/// * `ctx` — the context window, if known. Known → a percent bar. Unknown
///   (`None`) → the raw count plus the auto-compaction point, so the
///   fallback-window trigger is never silent. `Some(0)` → the raw count only.
fn context_bar(count: usize, measured: bool, ctx: Option<u32>, trigger_fraction: f32) -> String {
    let approx = if measured { "" } else { "~" };
    match ctx {
        Some(limit) if limit > 0 => {
            let pct = ((count as f64) / (limit as f64) * 100.0) as usize;
            let filled = (pct * 8 / 100).min(8);
            let empty = 8 - filled;
            format!("[{}] {pct}%", "█".repeat(filled) + &"░".repeat(empty))
        }
        None => {
            // No trustworthy window: show the running count and the point at
            // which the compressor (budgeted against its fallback window)
            // will start eliding history.
            let trigger = (crate::core::context::FALLBACK_CONTEXT_WINDOW as f64
                * f64::from(trigger_fraction)) as u64;
            format!("{approx}{count} tok · compacts near ~{} tok", human_tokens(trigger))
        }
        // `Some(0)`: a window the caller explicitly set to zero — just the
        // count, no bar and no compaction claim.
        _ => format!("{approx}{count} tok"),
    }
}

/// Shrink a JSON tool-arguments string to a single readable line for the
/// `⏺ name(args)` header — whitespace-collapsed only, never truncated (the
/// user wants the full call visible; length isn't cause to hide content).
pub fn compact_args(arguments: &str) -> String {
    arguments.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Print available slash commands for the chat REPL.
/// `/help` — rendered from the single command registry in two columns, so
/// the help screen can never list (or omit) a command whose dispatching
/// differs.
fn print_slash_commands_help() {
    use crate::ui::commands;

    let cmds = commands::SLASH_COMMANDS;
    let fmt = |c: &commands::SlashCommand| {
        let names: Vec<String> = c.all_names().map(|n| format!("/{n}")).collect();
        format!("{:>w$}  {}", names.join(", "), c.summary, w = 12)
    };
    let (left, right) = {
        let half = cmds.len().div_ceil(2);
        cmds.split_at(half)
    };
    let width = left.iter().map(|c| fmt(c).len()).max().unwrap_or(0);
    println!("\nAvailable slash commands (type a bare \"/\" for the picker):");
    for (i, c) in left.iter().enumerate() {
        let l = fmt(c);
        match right.get(i) {
            Some(r) => println!("  {:<w$}    {}", l, fmt(r), w = width),
            None => println!("  {l}"),
        }
    }
    println!();
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::should_show_tool_output;
    use super::{print_agent_event_to, StreamState};

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

    #[test]
    fn tool_end_line_has_a_single_sigma_and_the_timing() {
        use crate::core::lifecycle::AgentEvent;
        let skin = crate::ui::skin::by_name(None);
        let mut out = Vec::new();
        let mut stream = StreamState::default();
        print_agent_event_to(
            AgentEvent::ToolCallEnd {
                name: "read",
                result: "x",
                elapsed: std::time::Duration::from_millis(10),
            },
            &skin,
            true, // disable_color
            &mut stream,
            &mut out,
        );
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.matches('Σ').count(), 1, "exactly one Σ glyph: {s:?}");
        assert!(s.contains("10ms"), "timing present: {s:?}");
    }
}

#[cfg(test)]
mod slash_command_agreement_tests {
    /// The words the dispatch `match` in `run_chat` handles. This list and
    /// the registry are checked against each other in both directions: a
    /// handler without a registry entry is an undiscoverable command (audit
    /// G14), and a registry entry without a handler is a phantom. Update
    /// this constant whenever an arm in the dispatch match changes.
    const DISPATCHED: &[&str] = &[
        "exit", "quit", "help", "commands", "model", "skin", "session", "verbose", "readonly",
    ];

    #[test]
    fn every_dispatched_word_is_registered() {
        for w in DISPATCHED {
            assert!(
                crate::ui::commands::resolve(w).is_some(),
                "dispatched but not registered: {w}"
            );
        }
    }

    #[test]
    fn every_registered_word_is_dispatched() {
        for c in crate::ui::commands::SLASH_COMMANDS {
            for n in c.all_names() {
                assert!(
                    DISPATCHED.contains(&n),
                    "registered but not dispatched: {n}"
                );
            }
        }
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
