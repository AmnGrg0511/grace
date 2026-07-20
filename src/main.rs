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

use grace::agent::run_turn;
use grace::config::Config;
use grace::memory::Memory;
use grace::message::Message;
use grace::session::SessionStore;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
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
    let mut max_iterations: u32 = 16;
    let mut system_prompt: Option<String> = None;
    let mut remember: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut skills_dir: Option<String> = None;
    let mut memory_path: Option<String> = None;
    let mut tools_dir: Option<String> = None;
    let mut stream = false;

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
            "--skills-dir" => {
                skills_dir = args.get(i + 1).cloned();
                i += 2;
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
        return Ok(ExitCode::SUCCESS);
    }

    if !chat && prompt.is_none() {
        eprintln!("error: --prompt is required unless --chat or --remember is given (or use --help)");
        return Ok(ExitCode::FAILURE);
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
    let skills_root = skills_dir.unwrap_or_else(|| "skills".to_string());
    let tools_root = tools_dir.unwrap_or_else(|| "tools".to_string());
    let mut tools = Config::build_registry_with_plugins(skills_root, tools_root);
    tools.register(Box::new(grace::delegate_tool::DelegateTool::mock()));

    let mut messages: Vec<Message> = Vec::new();
    let mut sp = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| grace::config::DEFAULT_SYSTEM_PROMPT.to_string());
    // Ground the persona in durable facts instead of leaving it purely
    // decorative text: whatever Grace has been told to remember is appended
    // to every system prompt, every run.
    if let Some(block) = memory.as_prompt_block().map_err(|e| e.to_string())? {
        sp.push_str(&block);
    }
    messages.push(Message::system(sp));

    println!(
        "[grace] transport={} model={} tools={}",
        transport.name(),
        config.model(),
        tools.specs().len()
    );

    // Session persistence: if --session is given, resume prior history and
    // persist new turns as they happen (survives process restarts).
    let sessions = SessionStore::open(SessionStore::default_path()).map_err(|e| e.to_string())?;
    if let Some(sid) = &session_id {
        let prior = sessions.load(sid).map_err(|e| e.to_string())?;
        if !prior.is_empty() {
            println!("[grace] resumed session '{sid}' ({} prior turns)", prior.len());
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
        if let grace::config::TransportConfig::Http { base_url, api_key, model } = &config.transport {
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

    let answer = run_turn(transport.as_ref(), &tools, &mut messages, config.max_iterations)
        .map_err(|e| e.to_string())?;
    if let Some(sid) = &session_id {
        let _ = sessions.append(sid, &Message::assistant(answer.clone()));
    }
    println!("\n--- answer ---\n{}", grace::markdown::render_terminal(&answer));
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
) {
    use std::io::BufRead;

    println!("chat mode — type a message, or 'exit'/'quit' to leave.\n");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if matches!(text, "exit" | "quit" | "/exit" | "/quit") {
            println!("goodbye.");
            break;
        }
        messages.push(Message::user(text.to_string()));
        if let Some(sid) = session_id {
            let _ = sessions.append(sid, &Message::user(text.to_string()));
        }
        match run_turn(transport, tools, messages, max_iterations) {
            Ok(answer) => {
                println!("\ngrace: {}\n", grace::markdown::render_terminal(&answer));
                if let Some(sid) = session_id {
                    let _ = sessions.append(sid, &Message::assistant(answer));
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                // Drop the last user message so a failed turn can be retried.
                messages.pop();
            }
        }
    }
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
  -h, --help             Show this help

Config file (optional, CLI flags always win):
  ~/.grace/config.toml   default_model, default_base_url, memory_path, skills_dir,
                         tools_dir, max_iterations, request_timeout_secs"#;
    println!("{help}");
}
