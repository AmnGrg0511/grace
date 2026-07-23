//! `grace` binary: a minimal CLI that drives the agent loop.
//!
//! Usage:
//!   # Offline demo (scripted model + real tools):
//!   grace --mock --prompt "run a terminal command"
//!
//!   # Interactive chat (state persists across turns, and across restarts
//!   # via --session):
//!   grace --mock --chat --session work
//!
//!   # Real OpenAI-compatible endpoint (HTTPS via reqwest/rustls):
//!   grace --base-url https://api.openai.com/v1 \
//!                --api-key "$KEY" --model gpt-4o-mini --prompt "list files"
//!
//!   # OpenRouter (HTTPS via reqwest; key from env or --api-key):
//!   export OPENROUTER_API_KEY=sk-or-...
//!   grace --openrouter --model tencent/hy3:free --prompt "list files"
//!
//!   # Durable memory (survives process restarts, injected into every prompt):
//!   grace --mock --remember "user prefers concise answers"
//!   grace --mock --prompt "what do you know about me?"

use std::process::ExitCode;

use grace::config::{Config, load_soul};
use grace::memory::Memory;
use grace::message::Message;
use grace::session::SessionStore;
use grace::settings::PROVIDER_PRESETS;
use grace::skin::{Role, Skin};

const RESET: &str = "\x1b[0m";

fn main() -> ExitCode {
    load_dotenv();
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Load `KEY=value` lines from `~/.grace/.env` into the process environment
/// (only if not already set — real env always wins). This is where the
/// onboarding wizard persists API keys so they survive across invocations
/// without ever touching shell rc files.
fn load_dotenv() {
    let path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join(".env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let mut prompt: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut model: Option<String> = None;
    let mut mock = false;
    let mut chat = false;
    let mut openrouter = false;
    let mut max_iterations: u32 = 256;
    let mut system_prompt: Option<String> = None;
    let mut remember: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut skills_dir: Option<String> = None;
    let mut memory_path: Option<String> = None;
    let mut tools_dir: Option<String> = None;
    let mut stream = false;
    let mut skin_override: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--prompt" => {
                prompt = args.get(i + 1).cloned();
                i += 2;
            }
            "--base-url" => {
                base_url = args.get(i + 1).cloned();
                i += 2;
            }
            "--api-key" => {
                api_key = args.get(i + 1).cloned();
                i += 2;
            }
            "--model" => {
                model = args.get(i + 1).cloned();
                i += 2;
            }
            "--mock" => {
                mock = true;
                i += 1;
            }
            "--openrouter" => {
                openrouter = true;
                i += 1;
            }
            "--chat" => {
                chat = true;
                i += 1;
            }
            "--max-iterations" => {
                max_iterations = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(16);
                i += 2;
            }
            "--system" => {
                system_prompt = args.get(i + 1).cloned();
                i += 2;
            }
            "--remember" => {
                remember = args.get(i + 1).cloned();
                i += 2;
            }
            "--session" => {
                session_id = args.get(i + 1).cloned();
                i += 2;
            }
            "--list-sessions" => {
                let sessions =
                    SessionStore::open(SessionStore::default_path()).map_err(|e| e.to_string())?;
                let ids = sessions.list_sessions().map_err(|e| e.to_string())?;
                if ids.is_empty() {
                    println!("no sessions yet — use --session <id> --chat to start one.");
                } else {
                    println!("sessions (most recently active first):");
                    for id in ids {
                        println!("  {id}");
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }
            "--search-sessions" => {
                let query = args.get(i + 1).cloned().unwrap_or_default();
                if query.is_empty() {
                    eprintln!(
                        "--search-sessions requires a query, e.g. --search-sessions \"powerpro\""
                    );
                    return Ok(ExitCode::FAILURE);
                }
                let sessions =
                    SessionStore::open(SessionStore::default_path()).map_err(|e| e.to_string())?;
                let hits = sessions.search(&query, 20).map_err(|e| e.to_string())?;
                if hits.is_empty() {
                    println!("no matches for {query:?}.");
                } else {
                    for (session_id, content) in hits {
                        let preview: String = content.chars().take(200).collect();
                        println!("[{session_id}] {preview}");
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }
            "--skills-dir" => {
                skills_dir = args.get(i + 1).cloned();
                i += 2;
            }
            "--skin" => {
                skin_override = args.get(i + 1).cloned();
                i += 2;
            }
            "--list-skins" => {
                println!("available skins:");
                for name in grace::skin::all_names() {
                    println!("  {name}");
                }
                return Ok(ExitCode::SUCCESS);
            }
            "--select-skin" => {
                run_skin_picker()?;
                return Ok(ExitCode::SUCCESS);
            }
            "--memory-path" => {
                memory_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--tools-dir" => {
                tools_dir = args.get(i + 1).cloned();
                i += 2;
            }
            "--stream" => {
                stream = true;
                i += 1;
            }
            "--completions" => {
                let shell = args.get(i + 1).cloned().unwrap_or_default();
                print_completions(&shell);
                return Ok(ExitCode::SUCCESS);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(ExitCode::SUCCESS);
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    // Layered settings: defaults -> ~/.grace/config.toml -> CLI flags (CLI wins).
    let settings = grace::settings::Settings::load();
    let skin = grace::skin::by_name(skin_override.as_deref().or(settings.skin.as_deref()));

    let mut max_iterations_opt: Option<u32> = None;
    settings.merge_into_args(
        &mut base_url,
        &mut model,
        &mut memory_path,
        &mut skills_dir,
        &mut tools_dir,
        &mut max_iterations_opt,
    );
    if max_iterations == 16 {
        if let Some(mi) = max_iterations_opt {
            max_iterations = mi;
        }
    }

    // Open durable memory (always; it's a cheap local file, not a network dep).
    let mem_path = memory_path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(Memory::default_path);
    let memory = Memory::open(&mem_path).map_err(|e| e.to_string())?;

    // --remember is a standalone action: store the fact and exit.
    if let Some(fact) = remember {
        let id = memory.remember(&fact).map_err(|e| e.to_string())?;
        println!("remembered (id {id}): {fact}");
        // Mirror the DB to the human-readable markdown file.
        let _ = memory.export_markdown();
        return Ok(ExitCode::SUCCESS);
    }

    if !chat && prompt.is_none() {
        // Bare `grace` with no --prompt/--chat/--remember: default to chat
        // mode (matches the "just run it" expectation from other CLI
        // agents) instead of a hard error.
        chat = true;
    }

    // Every chat gets a session id so history/session_search actually has
    // something to find — without this, plain `grace --chat` (no explicit
    // `--session`) persisted nothing at all, and every `--search-sessions`/
    // `session_search` call legitimately came back empty. `--session <id>`
    // still overrides for named/multiple sessions.
    if chat && session_id.is_none() {
        session_id = Some("default".to_string());
    }

    // Onboarding: if we're headed for a real network transport but have no
    // model and no resolvable API key anywhere (config, CLI, known env
    // vars), stop and run the interactive picker instead of failing with a
    // terse "missing --model" error. Runs once; picks are persisted to
    // ~/.grace/config.toml and the key to ~/.grace/.env so this never asks
    // twice. Skipped entirely for --mock (no network needed).
    if !mock && model.is_none() {
        let (picked_model, picked_base_url, picked_key) = run_onboarding_wizard()?;
        model = Some(picked_model);
        base_url = Some(picked_base_url);
        if api_key.is_none() {
            api_key = Some(picked_key);
        }
        openrouter = false; // base_url is now explicit, no preset needed
    }

    let config = Config::from_args(
        base_url,
        api_key,
        model,
        mock,
        openrouter,
        max_iterations,
        system_prompt,
    )
    .map_err(|e| e.to_string())?;

    let transport = config.build_transport().map_err(|e| e.to_string())?;

    // Seed default skills (grace-agent, memory-update, skill-author) into
    // ~/.grace/skills/ on first run, and use that as the default skills
    // root unless --skills-dir overrides it.
    let skills_root = skills_dir.unwrap_or_else(|| {
        grace::default_skills::default_root().to_string_lossy().to_string()
    });
    let _ = grace::default_skills::ensure_default_skills();
    let tools_root = tools_dir.unwrap_or_else(|| "tools".to_string());
    let skills = grace::skill::SkillStore::new(&skills_root);
    // Shared, not `Sync` (SQLite `Connection` isn't) — fine since Grace is
    // single-threaded; Arc here is just for cheap ownership sharing between
    // the direct session-store call sites and the session_search tool.
    #[allow(clippy::arc_with_non_send_sync)]
    let sessions = std::sync::Arc::new(
        SessionStore::open(SessionStore::default_path()).map_err(|e| e.to_string())?,
    );
    let mut tools = Config::build_registry_with_plugins(skills_root, tools_root);
    tools.register(Box::new(grace::delegate_tool::DelegateTool::for_transport(
        &config.transport,
    )));
    tools.register(Box::new(grace::tools::SessionSearchTool::new(
        std::sync::Arc::clone(&sessions),
    )));

    let mut messages: Vec<Message> = Vec::new();
    let mut sp = config.system_prompt.clone().unwrap_or_else(load_soul);

    // Ground the persona in durable facts instead of leaving it purely
    // decorative text: whatever Grace has been told to remember is appended
    // to every system prompt, every run.
    if let Some(block) = memory.as_prompt_block().map_err(|e| e.to_string())? {
        sp.push_str(&block);
    }

    // Pre-flight recall: surface facts/skills/sessions that overlap with
    // this prompt's keywords, without requiring the user to say "look at
    // this file/skill" explicitly. Deterministic, free, FTS-first — no
    // embedding call unless --semantic is later added on top.
    if let Some(user_prompt) = prompt.as_deref() {
        let hits = grace::recall::recall(user_prompt, &memory, &skills, Some(&sessions), 5);
        if let Some(block) = grace::recall::as_prompt_block(&hits) {
            sp.push_str(&block);
        }
    }
    messages.push(Message::system(sp));

    println!(
        "[grace] transport={} model={} ctx={} tools={}",
        transport.name(),
        config.model(),
        grace::settings::context_window_for(config.model())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string()),
        tools.specs().len()
    );

    // Session persistence: if --session is given, resume prior history and
    // persist new turns as they happen (survives process restarts).
    if let Some(sid) = &session_id {
        let prior = sessions.load(sid).map_err(|e| e.to_string())?;
        if !prior.is_empty() {
            println!(
                "[grace] resumed session '{sid}' ({} prior turns)",
                prior.len()
            );
        }
        messages.extend(prior);
    }

    if chat {
        run_chat(
            transport.as_ref(),
            &tools,
            &mut messages,
            config.max_iterations,
            &sessions,
            session_id.as_deref(),
            &skin,
        );
        return Ok(ExitCode::SUCCESS);
    }

    // One-shot mode.
    let user_text = prompt.unwrap();
    messages.push(Message::user(user_text.clone()));
    if let Some(sid) = &session_id {
        let _ = sessions.append(sid, &Message::user(user_text));
    }

    // --stream only applies to one-shot mode against a real HTTP endpoint; it
    // falls back to the normal (non-streaming) path for --mock or when tool
    // calls are needed, since streaming here is a single direct completion
    // call (no tool-loop), matching the task's scope.
    if stream {
        if let grace::config::TransportConfig::Http {
            base_url,
            api_key,
            model,
        } = &config.transport
        {
            print!("\n--- answer (streaming) ---\n");
            use std::io::Write;
            let response = grace::transport_stream::stream_complete(
                base_url,
                api_key,
                model,
                &messages,
                &tools.specs(),
                |frag| {
                    print!("{frag}");
                    let _ = std::io::stdout().flush();
                },
            )
            .map_err(|e| e.to_string())?;
            println!();
            if let Some(sid) = &session_id {
                let _ = sessions.append(sid, &Message::assistant(response.content.clone()));
            }
            return Ok(ExitCode::SUCCESS);
        }
        println!("[grace] --stream requested but no HTTP transport configured (mock mode); falling back to non-streaming.");
    }

    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let answer = grace::agent::run_turn_with_events(
        transport.as_ref(),
        &tools,
        &mut messages,
        config.max_iterations,
        Some(&mut |event| print_agent_event(event, &skin)),
        Some(interrupted.as_ref()),
    )
    .map_err(|e| e.to_string())?;
    if let Some(sid) = &session_id {
        let _ = sessions.append(sid, &Message::assistant(answer.clone()));
    }
    println!(
        "\n--- answer ---\n{}",
        grace::markdown::render_terminal(&answer, &skin)
    );
    Ok(ExitCode::SUCCESS)
}

/// Interactive REPL. Each line you type is appended as a user message and the
/// conversation history (including tool calls) is preserved across turns. If
/// a session id was given, each turn is also persisted to disk immediately.
#[allow(clippy::too_many_arguments)]
fn run_chat(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    skin: &Skin,
) {
    use std::io::BufRead;
    use std::io::Write;

    // Owned+mutable so `/skin <name>` can swap it live; `/model <name>` swaps
    // the transport's own interior model instead (see `set_model`).
    let mut skin = *skin;
    // Owned so `/session <name>` can switch mid-chat.
    let mut current_session: Option<String> = session_id.map(|s| s.to_string());

    // Ctrl-C mid-turn cancels the current turn (tool calls already run stay
    // recorded) and returns to the prompt, instead of killing the whole
    // process — installed once for the process, the flag is what the agent
    // loop polls between steps. rustyline handles Ctrl-C at the idle prompt
    // itself (returns `ReadlineError::Interrupted`, handled below to exit
    // cleanly) independent of this signal handler.
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        let _ = ctrlc::set_handler(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }

    println!("chat mode — type a message, '/exit' to leave, '/model [name]' or '/skin [name]' to switch mid-chat.\n");

    let started_at = std::time::Instant::now();

    // Prefer rustyline for arrow-key history/editing; if stdin isn't a real
    // TTY (piped input, tests) it errors on creation, so fall back to plain
    // line reading — same behavior as before, just no history in that case.
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("history.txt");
    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut rl) = rustyline::DefaultEditor::new() {
        let _ = rl.load_history(&history_path);
        loop {
            print_status_line(&skin, transport, messages, started_at);
            let readline = rl.readline(&prompt_label(&skin));
            let line = match readline {
                Ok(l) => l,
                // Ctrl-C at an idle prompt: rustyline itself catches it (the
                // ctrlc handler above only fires while a turn is running, since
                // this readline() call blocks outside that code path). Just
                // redraw the prompt instead of exiting the whole session.
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(_) => break,
            };
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            let _ = rl.add_history_entry(text);
            let _ = rl.save_history(&history_path);
            if matches!(text, "/exit" | "/quit") {
                println!("goodbye.");
                break;
            }
            if let Some(rest) = text.strip_prefix("/model") {
                handle_model_command(transport, rest.trim());
                continue;
            }
            if let Some(rest) = text.strip_prefix("/skin") {
                handle_skin_command(rest.trim(), &mut skin);
                continue;
            }
            if let Some(rest) = text.strip_prefix("/session") {
                handle_session_command(rest.trim(), sessions, messages, &mut current_session);
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
            );
        }
        return;
    }

    // Fallback: plain stdin, no history (piped input / non-TTY, or a
    // terminal rustyline couldn't initialize against). Must still print the
    // prompt glyph explicitly — rustyline normally owns that via its
    // `readline(prompt)` argument, but this path bypasses rustyline entirely.
    let stdin = std::io::stdin();
    print_status_line(&skin, transport, messages, started_at);
    print!("{}", prompt_label(&skin));
    let _ = std::io::stdout().flush();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let text = line.trim();
        if text.is_empty() {
            print!("{}", prompt_label(&skin));
            let _ = std::io::stdout().flush();
            continue;
        }
        if matches!(text, "/exit" | "/quit") {
            println!("goodbye.");
            break;
        }
        if let Some(rest) = text.strip_prefix("/model") {
            handle_model_command(transport, rest.trim());
            print!("{}", prompt_label(&skin));
            let _ = std::io::stdout().flush();
            continue;
        }
        if let Some(rest) = text.strip_prefix("/skin") {
            handle_skin_command(rest.trim(), &mut skin);
            print!("{}", prompt_label(&skin));
            let _ = std::io::stdout().flush();
            continue;
        }
        if let Some(rest) = text.strip_prefix("/session") {
            handle_session_command(rest.trim(), sessions, messages, &mut current_session);
            print!("{}", prompt_label(&skin));
            let _ = std::io::stdout().flush();
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
        );
        print_status_line(&skin, transport, messages, started_at);
        print!("{}", prompt_label(&skin));
        let _ = std::io::stdout().flush();
    }
}

/// `/model` (interactive picker, same list as onboarding) or `/model <name>`
/// (direct switch) mid-chat. Persists to ~/.grace/config.toml so the choice
/// sticks across restarts (unlike the old session-only behavior).
/// Only takes effect on transports that own a swappable model (`HttpTransport`);
/// mock has nothing to switch.
fn handle_model_command(transport: &(dyn grace::transport::ProviderTransport + '_), arg: &str) {
    if transport.current_model().is_none() {
        println!(
            "this transport ({}) has no switchable model.",
            transport.name()
        );
        return;
    }
    let (picked, ctx) = if arg.is_empty() {
        match pick_model_interactive() {
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
fn pick_model_interactive() -> Option<(String, Option<u32>)> {
    use std::io::Write;
    println!("\nproviders:\n");
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let n_providers = PROVIDER_PRESETS.len();
    print!("\nselect a provider [number]: ");
    let _ = std::io::stdout().flush();
    let raw = std::io::stdin().lines().next()?.ok()?;
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
        print!("model id: ");
        let _ = std::io::stdout().flush();
        let typed = std::io::stdin().lines().next()?.ok()?.trim().to_string();
        return if typed.is_empty() {
            None
        } else {
            Some((typed, None))
        };
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
    print!("\nselect a model [number]: ");
    let _ = std::io::stdout().flush();
    let raw = std::io::stdin().lines().next()?.ok()?;
    match raw.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= n_models => Some((
            preset.models[n - 1].id.to_string(),
            Some(preset.models[n - 1].context_window),
        )),
        Ok(n) if n == n_models + 1 => {
            // Custom model ID
            print!("model id: ");
            let _ = std::io::stdout().flush();
            let typed = std::io::stdin().lines().next()?.ok()?.trim().to_string();
            if typed.is_empty() {
                None
            } else {
                Some((typed, None))
            }
        },
        _ => {
            println!("not a valid choice.");
            None
        }
    }
}

/// `/skin` (interactive picker, same as `--select-skin`) or `/skin <name>`
/// (direct switch) mid-chat. Session-only — use `--select-skin` to persist
/// a default across runs.
fn handle_skin_command(arg: &str, skin: &mut Skin) {
    let names = grace::skin::all_names();
    let picked = if arg.is_empty() {
        match pick_skin_interactive(&names) {
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
/// - `/session none`: disable session persistence for the rest of the chat
fn handle_session_command(
    arg: &str,
    sessions: &SessionStore,
    messages: &mut Vec<Message>,
    current_session: &mut Option<String>,
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
        print!("\nselect a session [number, or 0 for new]: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let raw = std::io::stdin().lines().next().and_then(|l| l.ok()).unwrap_or_default();
        match raw.trim().parse::<usize>() {
            Ok(0) => {
                messages.clear();
                *current_session = None;
                println!("started a fresh session (no persistence).");
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
        "none" => {
            *current_session = None;
            println!("session persistence disabled for this chat.");
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
fn pick_skin_interactive(names: &[String]) -> Option<String> {
    use std::io::Write;
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
    print!("\nselect a skin [number]: ");
    let _ = std::io::stdout().flush();
    let raw = std::io::stdin().lines().next()?.ok()?;
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
fn run_one_chat_turn(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    sessions: &SessionStore,
    session_id: Option<&str>,
    text: &str,
    skin: &Skin,
    interrupted: &std::sync::Arc<std::sync::atomic::AtomicBool>,
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

/// Interactive first-run picker: provider -> API key -> model. Persists the
/// choice to `~/.grace/config.toml` (model/base_url) and `~/.grace/.env`
/// (the key, so it's never asked twice and never lives in shell history).
/// Returns (model, base_url, api_key) to use for *this* invocation.
fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut stdin_lines = std::io::stdin().lines();
    let mut prompt_read = |label: &str| -> String {
        print!("{label}");
        let _ = std::io::stdout().flush();
        stdin_lines
            .next()
            .and_then(|l| l.ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    println!(
        "\ngrace needs a model provider — this only runs once, choices are saved to ~/.grace/\n"
    );
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let choice: usize = loop {
        let raw = prompt_read("\nselect a provider [number]: ");
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= PROVIDER_PRESETS.len() => break n - 1,
            _ => println!("enter a number between 1 and {}", PROVIDER_PRESETS.len()),
        }
    };
    let preset = &PROVIDER_PRESETS[choice];

    let base_url = if preset.base_url.is_empty() {
        prompt_read("base URL (OpenAI-compatible /chat/completions endpoint): ")
    } else {
        preset.base_url.to_string()
    };

    // Prefer an already-set env var (e.g. exported this shell session) so we
    // don't re-ask for a key the user already has available.
    let api_key = std::env::var(preset.env_var)
        .ok()
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| {
            prompt_read(&format!(
                "API key for {} (or set ${} and re-run): ",
                preset.label, preset.env_var
            ))
        });

    let (model, ctx_window) = if preset.models.is_empty() {
        (prompt_read("model id: "), None)
    } else {
        println!();
        for (i, m) in preset.models.iter().enumerate() {
            println!("  {}) {} (context: {})", i + 1, m.id, m.context_window);
        }
        println!("  {}) other (type a model id)", preset.models.len() + 1);
        let picked: (String, Option<u32>) = loop {
            let raw = prompt_read("\nselect a model [number]: ");
            if let Ok(n) = raw.parse::<usize>() {
                if n >= 1 && n <= preset.models.len() {
                    let m = &preset.models[n - 1];
                    break (m.id.to_string(), Some(m.context_window));
                }
                if n == preset.models.len() + 1 {
                    let typed = prompt_read("model id: ");
                    let ctx = fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
            }
            println!("enter a valid number");
        };
        picked
    };

    // Persist: model + base_url + context window go to config.toml; the
    // key goes to .env (kept separate so config.toml can be safely shared).
    let mut settings = grace::settings::Settings::load();
    settings.default_model = Some(model.clone());
    settings.default_base_url = Some(base_url.clone());
    settings.default_context_window = ctx_window;
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
    if let Err(e) = std::fs::write(&env_path, format!("{}={}\n", preset.env_var, api_key)) {
        eprintln!(
            "[grace] warning: could not save {}: {e}",
            env_path.display()
        );
    }
    println!("\nsaved — future runs won't ask again. edit ~/.grace/config.toml or ~/.grace/.env to change.\n");

    Ok((model, base_url, api_key))
}

/// Interactive skin picker: same list+preview flow as `/skin` mid-chat
/// ([`pick_skin_interactive`]), but persists the choice to
/// `~/.grace/config.toml` — same "choose once, remembered forever" pattern
/// as [`run_onboarding_wizard`]'s provider pick.
fn run_skin_picker() -> Result<(), Box<dyn std::error::Error>> {
    let names = grace::skin::all_names();
    if names.is_empty() {
        println!("no skins available.");
        return Ok(());
    }
    let Some(picked) = pick_skin_interactive(&names) else {
        return Ok(());
    };

    let mut settings = grace::settings::Settings::load();
    settings.skin = Some(picked.clone());
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    } else {
        println!("\nskin set to \"{picked}\" — saved to ~/.grace/config.toml.\n");
    }
    Ok(())
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
fn print_agent_event(event: grace::agent::AgentEvent, skin: &Skin) {
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
fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

/// Whether ANSI color should be suppressed: not a TTY, or `NO_COLOR`/`CLICOLOR=0` set.
fn no_color() -> bool {
    use std::io::IsTerminal;
    !std::io::stdout().is_terminal()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CLICOLOR").as_deref() == Ok("0")
}

/// The interactive-chat input prompt — a skin-colored glyph, never the
/// literal word "you", so the transcript reads as two distinct visual
/// speakers instead of a flat `you:`/`grace:` log.
fn prompt_label(skin: &Skin) -> String {
    if no_color() {
        return format!("{} ", skin.prompt_glyph);
    }
    skin.paint(Role::Prompt, &format!("{} ", skin.prompt_glyph))
}

/// Best-effort API fetch to discover a model's context window. Covers
/// OpenRouter (GET /api/v1/models) and OpenAI (GET /v1/models/{id});
/// everything else returns `None` silently.
fn fetch_context_window(model: &str, base_url: &str, api_key: &str) -> Option<u32> {
    // OpenRouter: list endpoint returns context_length per model.
    if base_url.contains("openrouter") {
        let url = format!(
            "{}/api/v1/models",
            base_url.trim_end_matches("/v1").trim_end_matches('/')
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(&url).bearer_auth(api_key).send().ok()?;
        let data: serde_json::Value = resp.json().ok()?;
        let arr = data.get("data").and_then(|d| d.as_array())?;
        for entry in arr {
            let id = entry.get("id").and_then(|v| v.as_str())?;
            if id == model {
                return entry
                    .get("context_length")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
            }
            // Also match on model family prefix (e.g. "anthropic/claude-sonnet-4-*")
            if model.starts_with(id) || id.starts_with(model) {
                return entry
                    .get("context_length")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
            }
        }
        return None;
    }
    // OpenAI: the models/{id} endpoint returns max_context_window for some.
    if base_url.contains("openai.com") {
        let url = format!(
            "{}/models/{}",
            base_url.trim_end_matches('/'),
            urlencoding(model)
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(&url).bearer_auth(api_key).send().ok()?;
        let data: serde_json::Value = resp.json().ok()?;
        if let Some(ctx) = data.pointer("/max_context_window") {
            return ctx.as_u64().map(|n| n as u32);
        }
    }
    None
}

/// Encode '/' and other URI-unfriendly chars for model-id path segments.
fn urlencoding(s: &str) -> String {
    s.replace('/', "%2F")
        .replace('.', "%2E")
        .replace(':', "%3A")
}

/// A subtle status line above the prompt: model · context bar · elapsed.
/// All in the skin's muted `tool_dim` color so it recedes behind the prompt
/// glyph and never competes with the conversation itself. Single dim
/// (color only, no extra ANSI dim) — same tier as tool output body.
fn print_status_line(
    skin: &Skin,
    transport: &(dyn grace::transport::ProviderTransport + '_),
    messages: &[grace::message::Message],
    started_at: std::time::Instant,
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
    // Saved context window (from picker/API fetch) beats the static lookup
    // table — it covers models only known at runtime.
    let settings = grace::settings::Settings::load();
    let ctx = settings
        .default_context_window
        .or_else(|| grace::settings::context_window_for(&model));

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
fn compact_args(arguments: &str) -> String {
    arguments.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn print_help() {
    let help = r#"grace — minimal vendor-neutral ReAct agent

Usage:
  grace --mock --prompt "run a terminal command"
  grace --mock --chat --session work
  grace --base-url https://api.openai.com/v1 --api-key KEY --model M --prompt "..."
  grace --openrouter --model tencent/hy3:free --prompt "..."   (key from --api-key or $OPENROUTER_API_KEY; free-only keys need a :free model)
  grace --mock --remember "user prefers concise answers"

Flags:
  --prompt <text>        The user instruction (one-shot mode)
  --chat                 Interactive REPL (state persists across turns)
  --session <id>         Persist/resume chat history across process restarts (SQLite)
  --list-sessions        List saved session ids, most recently active first, and exit
  --search-sessions <q>  Full-text search past session turns (SQLite FTS5) and exit
  --skin <name>          Use a named skin for this run (gilded/royal/ocean/sakura/forest/solaris/midnight, or a custom one)
  --list-skins           List every available skin name and exit
  --select-skin          Interactive skin picker with color previews; saves the choice to ~/.grace/config.toml
  --remember <fact>      Store a durable fact (SQLite memory) and exit
  --memory-path <path>   Override memory DB path (default ~/.grace/memory.db)
  --skills-dir <path>    Directory of skills/<name>/SKILL.md (default ./skills)
  --mock                 Use the offline scripted model (no network)
  --openrouter           Use OpenRouter (HTTPS via reqwest/rustls)
  --base-url <url>       OpenAI-compatible endpoint (http:// or https://)
  --api-key <key>        Bearer token (default empty; for OpenRouter uses $OPENROUTER_API_KEY)
  --model <name>         Model id (required for http/openrouter mode)
  --max-iterations <n>   Tool-call round cap (default 16)
  --system <text>        Optional system prompt
  --tools-dir <path>     Directory of tools/<name>/manifest.json plugins (default ./tools)
  --stream               Stream tokens as they arrive (one-shot HTTP mode only; falls back to
                         non-streaming under --mock)
  --completions <shell>  Print shell completions (bash, zsh, fish) and exit
  -h, --help             Show this help

Config file (optional, CLI flags always win):
  ~/.grace/config.toml   default_model, default_base_url, memory_path, skills_dir,
                         tools_dir, max_iterations, request_timeout_secs"#;
    println!("{help}");
}

/// Print shell completion scripts for bash, zsh, or fish.
fn print_completions(shell: &str) {
    match shell.trim() {
        "bash" => print!("{}", bash_completions()),
        "zsh" => print!("{}", zsh_completions()),
        "fish" => print!("{}", fish_completions()),
        other => {
            eprintln!("unknown shell: {other} (supported: bash, zsh, fish)");
            std::process::exit(1);
        }
    }
}

const FLAGS: &[&str] = &[
    "--prompt", "--base-url", "--api-key", "--model", "--mock", "--openrouter",
    "--chat", "--max-iterations", "--system", "--remember", "--session",
    "--list-sessions", "--search-sessions", "--skills-dir", "--skin",
    "--list-skins", "--select-skin", "--memory-path", "--tools-dir",
    "--stream", "--completions", "--help", "-h",
];

fn bash_completions() -> String {
    format!(
        r#"# bash completions for grace
_grace() {{
    local cur prev
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"

    if [[ $cur == --* ]]; then
        COMPREPLY=($(compgen -W "{flags_str}" -- "$cur"))
        return
    fi

    case "${{COMP_WORDS[COMP_CWORD-1]}}" in
        --skin) COMPREPLY=($(compgen -W "solaris royal ocean sakura" -- "$cur")) ;;
        --completions) COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur")) ;;
        --model) ;;
        --session) ;;
        *) ;;
    esac
}}
complete -F _grace grace
"#,
        flags_str = FLAGS.join(" ")
    )
}

fn zsh_completions() -> String {
    format!(
        r#"#compdef grace

_grace() {{
    local -a flags
    flags=({flags})

    if [[ ${{words[CURRENT]}} == --* ]]; then
        _describe 'flag' flags
        return
    fi

    case ${{words[CURRENT-1]}} in
        --skin) _values 'skin' 'solaris' 'royal' 'ocean' 'sakura' ;;
        --completions) _values 'shell' 'bash' 'zsh' 'fish' ;;
    esac
}}

_grace "$@"
"#,
        flags = FLAGS
            .iter()
            .map(|f| format!("'{f}[flag]'"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn fish_completions() -> String {
    let mut out = String::new();
    for f in FLAGS {
        out.push_str(&format!("complete -c grace -l {} -d 'grace flag'\n", f.trim_start_matches('-')));
    }
    out.push_str("complete -c grace -l skin -d 'skin name' -a 'solaris royal ocean sakura'\n");
    out.push_str("complete -c grace -l completions -d 'shell' -a 'bash zsh fish'\n");
    out
}
