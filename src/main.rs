//! `grace` binary: a minimal CLI that drives the agent loop.
//!
//! Usage:
//!   # Interactive chat (state persists across turns, and across restarts
//!   # via --session):
//!   grace --openrouter --model openai/gpt-4o-mini --chat --session work
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
//!   grace --openrouter --model openai/gpt-4o-mini --remember "user prefers concise answers"
//!   grace --openrouter --model openai/gpt-4o-mini --prompt "what do you know about me?"

use std::process::ExitCode;

use grace::config::{Config, load_soul};
use grace::memory::Memory;
use grace::message::Message;
use grace::session::SessionStore;

mod cli;
mod chat;
mod wizard;

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

/// Auto session id for `--chat` runs with no explicit `--session`: derived
/// from the controlling tty so distinct terminals don't collide on a shared
/// "default" session, while a given terminal still resumes its own history
/// across restarts (the tty path stays stable within one terminal). Falls
/// back to a plain "default" when there's no real tty (piped stdin, CI,
/// non-Unix) — a single shared session in that case is the correct
/// behavior, since there's no "which terminal" to disambiguate.
fn default_session_id() -> String {
    #[cfg(unix)]
    {
        if let Ok(path) = std::fs::read_link("/proc/self/fd/0") {
            let s = path.to_string_lossy();
            if s.starts_with("/dev/") {
                return format!("default-{}", s.replace('/', "-"));
            }
        }
    }
    "default".to_string()
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
                // Trimmed: a copy-pasted URL/key with a trailing newline or
                // space (common when piping from a password manager or
                // shell var) would otherwise reach the HTTP client verbatim
                // and fail with an opaque connection/auth error instead of
                // just working.
                base_url = args.get(i + 1).map(|s| s.trim().to_string());
                i += 2;
            }
            "--api-key" => {
                api_key = args.get(i + 1).map(|s| s.trim().to_string());
                i += 2;
            }
            "--model" => {
                model = args.get(i + 1).map(|s| s.trim().to_string());
                i += 2;
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
                wizard::run_skin_picker()?;
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
                cli::print_completions(&shell);
                return Ok(ExitCode::SUCCESS);
            }
            "--help" | "-h" => {
                cli::print_help();
                return Ok(ExitCode::SUCCESS);
            }
            other => {
                eprintln!("unknown argument: {other}");
                cli::print_help();
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
    // `--session`) persisted nothing at all. `--session <id>` still
    // overrides for named/deliberately-shared sessions.
    //
    // The auto id is derived from the controlling tty (not a bare
    // "default"), so two *different* terminals running `grace --chat`
    // simultaneously each get their own history instead of silently
    // reading/appending to the same "default" session — a real
    // cross-terminal contamination bug found via live testing (terminal B
    // could answer questions about facts only ever told to terminal A).
    // Re-running in the *same* terminal still resumes its own history,
    // since the tty path is stable across restarts of that terminal.
    if chat && session_id.is_none() {
        session_id = Some(default_session_id());
    }

    // Onboarding: if we're headed for a real network transport but have no
    // model and no resolvable API key anywhere (config, CLI, known env
    // vars), stop and run the interactive picker instead of failing with a
    // terse "missing --model" error. Runs once; picks are persisted to
    // ~/.grace/config.toml (plus the key to ~/.grace/.env — including
    // Copilot's OAuth-minted token, same file, same shape as any other
    // provider's key) so this never asks twice.
    if model.is_none() {
        let (picked_model, picked_base_url, picked_key) = wizard::run_onboarding_wizard()?;
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
        chat::run_chat(
            transport.as_ref(),
            &tools,
            &mut messages,
            config.max_iterations,
            &sessions,
            session_id.as_deref(),
            &skin,
            &config.context_compression,
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
    // falls back to the normal (non-streaming) path when tool calls are
    // needed, since streaming here is a single direct completion call (no
    // tool-loop), matching the task's scope.
    if stream {
        let (base_url, api_key, model) = match &config.transport {
            grace::config::TransportConfig::Http {
                base_url,
                api_key,
                model,
            } => (base_url.clone(), api_key.clone(), model.clone()),
        };

        print!("\n--- answer (streaming) ---\n");
        use std::io::Write;
        let response = grace::transport_stream::stream_complete(
            &base_url,
            &api_key,
            &model,
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

    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let answer = grace::agent::run_turn_with_events(
        transport.as_ref(),
        &tools,
        &mut messages,
        config.max_iterations,
        Some(&mut |event| chat::print_agent_event(event, &skin)),
        Some(interrupted.as_ref()),
        Some(&config.context_compression),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_session_id_is_deterministic_and_nonempty() {
        // Under test, stdin is not a tty (it's piped/redirected), so this
        // exercises the "default" fallback path — but the important
        // invariant either way is: same process env -> same id every call
        // (a session name that changes turn-to-turn would defeat resuming).
        let a = default_session_id();
        let b = default_session_id();
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }
}

