//! `grace` binary: a minimal CLI that drives the agent loop.
//!
//! Usage:
//!   # Offline demo (scripted model + real tools):
//!   grace --mock --prompt "run a terminal command"
//!
//!   # Interactive chat (state persists across turns):
//!   grace --mock --chat
//!
//!   # Real OpenAI-compatible endpoint (plaintext http://, front TLS w/ proxy):
//!   grace --base-url http://127.0.0.1:8080/v1 \
//!                --api-key "$KEY" --model grace-1 --prompt "list files"
//!
//!   # OpenRouter (HTTPS via auto-spawned python3 TLS proxy; key from env):
//!   export OPENROUTER_API_KEY=sk-or-...
//!   grace --openrouter --model openai/gpt-4o-mini --prompt "list files"

use std::process::ExitCode;

use grace::agent::run_turn;
use grace::config::Config;
use grace::message::Message;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

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
                max_iterations = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(16);
                i += 2;
            }
            "--system" => {
                system_prompt = args.get(i + 1).cloned();
                i += 2;
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

    if !chat && prompt.is_none() {
        eprintln!("error: --prompt is required unless --chat is given (or use --help)");
        return Ok(ExitCode::FAILURE);
    }

    let config = Config::from_args(base_url, api_key, model, mock, openrouter, max_iterations, system_prompt)
        .map_err(|e| e.to_string())?;

    let transport = config.build_transport().map_err(|e| e.to_string())?;
    let tools = Config::build_registry();

    let mut messages: Vec<Message> = Vec::new();
    let sp = config
        .system_prompt
        .clone()
        .unwrap_or_else(|| grace::config::DEFAULT_SYSTEM_PROMPT.to_string());
    messages.push(Message::system(sp));

    println!(
        "[grace] transport={} model={} tools={}",
        transport.name(),
        config.model(),
        tools.specs().len()
    );

    if chat {
        run_chat(transport.as_ref(), &tools, &mut messages, config.max_iterations);
        return Ok(ExitCode::SUCCESS);
    }

    // One-shot mode.
    messages.push(Message::user(prompt.unwrap()));
    let answer = run_turn(transport.as_ref(), &tools, &mut messages, config.max_iterations)
        .map_err(|e| e.to_string())?;
    println!("\n--- answer ---\n{}", grace::markdown::render_terminal(&answer));
    Ok(ExitCode::SUCCESS)
}

/// Interactive REPL. Each line you type is appended as a user message and the
/// conversation history (including tool calls) is preserved across turns.
fn run_chat(
    transport: &(dyn grace::transport::ProviderTransport + '_),
    tools: &grace::tool::ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
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
        match run_turn(transport, tools, messages, max_iterations) {
            Ok(answer) => println!("\ngrace: {}\n", grace::markdown::render_terminal(&answer)),
            Err(e) => {
                eprintln!("error: {e}");
                // Drop the last user message so a failed turn can be retried.
                messages.pop();
            }
        }
    }
}

fn print_help() {
    let help = r#"grace — minimal vendor-neutral ReAct agent (std-only, zero deps)

Usage:
  grace --mock --prompt "run a terminal command"
  grace --mock --chat
  grace --base-url http://127.0.0.1:8080/v1 --api-key KEY --model M --prompt "..."
  grace --openrouter --model tencent/hy3:free --prompt "..."   (key from --api-key or $OPENROUTER_API_KEY; free-only keys need a :free model)

Flags:
  --prompt <text>        The user instruction (one-shot mode)
  --chat                 Interactive REPL (state persists across turns)
  --mock                 Use the offline scripted model (no network)
  --openrouter           Use OpenRouter (HTTPS; auto-spawns a python3 TLS proxy)
  --base-url <url>       OpenAI-compatible endpoint (http:// only)
  --api-key <key>        Bearer token (default empty; for OpenRouter uses $OPENROUTER_API_KEY)
  --model <name>         Model id (required for http/openrouter mode)
  --max-iterations <n>   Tool-call round cap (default 16)
  --system <text>        Optional system prompt
  -h, --help             Show this help"#;
    println!("{help}");
}
