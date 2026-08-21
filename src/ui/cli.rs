//! The CLI: argument parsing, help text, completions, and dispatch.
//!
//! `main.rs` is deliberately a stub that calls [`run`]. Everything that used to
//! make it a 500-line file — flag parsing, settings layering, store opening,
//! registry assembly, one-shot vs chat dispatch — lives here, where it is
//! reachable from a test instead of only from a real process invocation.
//!
//! ```text
//! CliArgs::parse   pure: argv -> a struct, no I/O, no side effects
//! Action           what the parsed args mean (help, list, run a turn, ...)
//! run              performs the action
//! ```
//!
//! Parsing is separated from performing precisely so the former can be tested
//! exhaustively without a network, a database, or a terminal.

use crate::config::{Config, RegistryOptions, Settings};
use crate::core::delegation::DelegationDepth;
use crate::memory::Memory;
use crate::message::Message;
use crate::session::{SessionLock, SessionStore};
use crate::ui::skin::Skin;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;

/// Everything the CLI accepts, after parsing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliArgs {
    pub prompt: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub chat: bool,
    pub max_iterations: Option<u32>,
    pub system_prompt: Option<String>,
    pub remember: Option<String>,
    pub session_id: Option<String>,
    pub skills_dir: Option<String>,
    pub memory_path: Option<String>,
    pub tools_dir: Option<String>,
    pub stream: bool,
    pub skin: Option<String>,
    pub verbose: bool,
    /// A terminal action that short-circuits everything else.
    pub action: Option<Action>,
}

/// A flag that means "do this one thing and exit".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Help,
    Version,
    ListSessions,
    SearchSessions(String),
    ListSkins,
    SelectSkin,
    Completions(String),
    /// An unrecognized flag. Carried rather than printed at parse time so
    /// parsing stays pure.
    Unknown(String),
}

impl CliArgs {
    /// Parse `argv` (excluding the program name).
    ///
    /// Pure: no I/O, no environment reads, no exits. URLs, keys, and model ids
    /// are trimmed — a value pasted from a password manager or shell variable
    /// routinely carries a trailing newline, which would otherwise reach the
    /// HTTP client verbatim and fail with an opaque auth error instead of just
    /// working.
    pub fn parse<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = argv.into_iter().map(|s| s.as_ref().to_string()).collect();
        let mut out = CliArgs::default();
        let mut i = 0;

        // Consume the value following a flag, if there is one.
        let value = |args: &Vec<String>, i: usize| args.get(i + 1).cloned();
        let trimmed = |args: &Vec<String>, i: usize| {
            args.get(i + 1).map(|s| s.trim().to_string())
        };

        while i < args.len() {
            match args[i].as_str() {
                "--prompt" => {
                    out.prompt = value(&args, i);
                    i += 2;
                }
                "--base-url" => {
                    out.base_url = trimmed(&args, i);
                    i += 2;
                }
                "--api-key" => {
                    out.api_key = trimmed(&args, i);
                    i += 2;
                }
                "--model" => {
                    out.model = trimmed(&args, i);
                    i += 2;
                }
                "--verbose" | "-v" => {
                    out.verbose = true;
                    i += 1;
                }
                "--openrouter" => {
                    // Sugar for `--base-url https://openrouter.ai/api/v1`.
                    // OpenRouter is just another HTTP provider; this flag only
                    // saves typing, it creates no separate code path.
                    out.base_url = Some(crate::config::OPENROUTER_BASE_URL.to_string());
                    i += 1;
                }
                "--chat" => {
                    out.chat = true;
                    i += 1;
                }
                "--max-iterations" => {
                    out.max_iterations = args.get(i + 1).and_then(|s| s.trim().parse().ok());
                    i += 2;
                }
                "--system" => {
                    out.system_prompt = value(&args, i);
                    i += 2;
                }
                "--remember" => {
                    out.remember = value(&args, i);
                    i += 2;
                }
                "--session" => {
                    out.session_id = value(&args, i);
                    i += 2;
                }
                "--skills-dir" => {
                    out.skills_dir = value(&args, i);
                    i += 2;
                }
                "--skin" => {
                    out.skin = value(&args, i);
                    i += 2;
                }
                "--memory-path" => {
                    out.memory_path = value(&args, i);
                    i += 2;
                }
                "--tools-dir" => {
                    out.tools_dir = value(&args, i);
                    i += 2;
                }
                "--stream" => {
                    out.stream = true;
                    i += 1;
                }
                "--list-sessions" => {
                    out.action = Some(Action::ListSessions);
                    i += 1;
                }
                "--search-sessions" => {
                    out.action = Some(Action::SearchSessions(
                        value(&args, i).unwrap_or_default(),
                    ));
                    i += 2;
                }
                "--list-skins" => {
                    out.action = Some(Action::ListSkins);
                    i += 1;
                }
                "--select-skin" => {
                    out.action = Some(Action::SelectSkin);
                    i += 1;
                }
                "--completions" => {
                    out.action = Some(Action::Completions(value(&args, i).unwrap_or_default()));
                    i += 2;
                }
                "--help" | "-h" => {
                    out.action = Some(Action::Help);
                    i += 1;
                }
                "--version" | "-V" => {
                    out.action = Some(Action::Version);
                    i += 1;
                }
                other => {
                    out.action = Some(Action::Unknown(other.to_string()));
                    i += 1;
                }
            }
        }
        out
    }

    /// Whether this invocation should land in the interactive REPL.
    ///
    /// A bare `grace` with nothing else means chat — matching the "just run
    /// it" expectation from other agent CLIs, rather than a hard error.
    pub fn wants_chat(&self) -> bool {
        self.chat || (self.prompt.is_none() && self.remember.is_none())
    }
}

type BoxedError = Box<dyn std::error::Error>;

/// Entry point. Loads `~/.grace/.env`, parses argv, and dispatches.
pub fn main() -> ExitCode {
    load_dotenv();
    let args = CliArgs::parse(std::env::args().skip(1));
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Where the onboarding wizard and `/model` persist API keys: one
/// `KEY=value` per line, so they survive across invocations without ever
/// touching shell rc files.
pub fn env_file_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join(".env")
}

/// Parse one `.env` line into `(key, value)`, tolerating an `export `
/// prefix and matched single or double quotes around the value. `None` for
/// blank and comment lines, for lines without `=`, and for empty values —
/// empty means absent, not "set to nothing".
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let (key, value) = line.split_once('=')?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(str::to_string)
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .map(str::to_string)
        })
        .unwrap_or_else(|| value.to_string());
    if value.is_empty() {
        return None;
    }
    Some((key, value))
}

/// Load the .env into the process environment, only where not already set
/// — the real environment always wins.
pub fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(env_file_path()) else {
        return;
    };
    for line in text.lines() {
        if let Some((key, value)) = parse_env_line(line) {
            if std::env::var(&key).is_err() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Replace-or-append one key in a .env file, touching only that key's line
/// so the other keys (other providers' tokens) survive a rewrite. A line
/// counts as the same key with or without an `export ` prefix; the new line
/// is written plain, which the reader handles.
///
/// The file is created, and left, mode 0600: it holds API keys, and a
/// group/world-readable file is a leak.
pub fn upsert_env_file_at(
    path: &std::path::Path,
    key: &str,
    value: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        let key_of_line = line
            .trim()
            .strip_prefix("export ")
            .map(str::trim)
            .unwrap_or(line.trim())
            .split('=')
            .next()
            .unwrap_or("")
            .trim();
        let is_comment = line.trim_start().starts_with('#');
        if !replaced
            && !line.trim().is_empty()
            && !is_comment
            && key_of_line == key
        {
            out.push(format!("{key}={value}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !replaced {
        out.push(format!("{key}={value}"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    std::fs::write(path, text)?;
    // A key file is 0600 whether it pre-existed or not.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Upsert into the standard `~/.grace/.env` (see [`upsert_env_file_at`]).
pub fn upsert_env_file(key: &str, value: &str) -> std::io::Result<()> {
    upsert_env_file_at(&env_file_path(), key, value)
}

/// Perform whatever `args` asks for.
pub fn run(mut args: CliArgs) -> Result<ExitCode, BoxedError> {
    if let Some(action) = args.action.take() {
        return run_action(&action);
    }

    // Layered settings: defaults -> ~/.grace/config.toml -> CLI flags.
    let settings = Settings::load();
    let skin = crate::ui::skin::by_name(args.skin.as_deref().or(settings.skin.as_deref()));

    let mut base_url = args.base_url.clone();
    let mut model = args.model.clone();
    let mut memory_path = args.memory_path.clone();
    let mut skills_dir = args.skills_dir.clone();
    let mut tools_dir = args.tools_dir.clone();
    let mut settings_iterations: Option<u32> = None;
    settings.merge_into_args(
        &mut base_url,
        &mut model,
        &mut memory_path,
        &mut skills_dir,
        &mut tools_dir,
        &mut settings_iterations,
    );
    // CLI wins over the config file, which wins over the built-in default.
    let max_iterations = args
        .max_iterations
        .or(settings_iterations)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Memory is always opened — it is a cheap local file, not a network dep.
    let memory = Memory::open(
        memory_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(Memory::default_path),
    )?;

    // `--remember` is a standalone action: store the fact and exit.
    if let Some(fact) = &args.remember {
        let id = memory.remember(fact)?;
        println!("remembered (id {id}): {fact}");
        let _ = memory.export_markdown();
        return Ok(ExitCode::SUCCESS);
    }

    let chat = args.wants_chat();

    // The id goes into file names (lock file, history file); a path-shaped
    // id is a traversal, not a session.
    if let Some(sid) = &args.session_id {
        if let Err(e) = crate::session::lock::validate_session_id(sid) {
            eprintln!("error: {e}");
            return Err(e.into());
        }
    }

    #[allow(clippy::arc_with_non_send_sync)]
    let sessions = Arc::new(SessionStore::open(SessionStore::default_path())?);
    let mut session_id = args.session_id.clone();
    if chat && session_id.is_none() {
        session_id = Some(pick_or_create_default_session(&sessions));
    }
    // Acquire lock immediately — before the onboarding wizard and transport
    // setup — so a second terminal sees this session as taken, not a race
    // window of hundreds of lines of code. A session open in another
    // terminal is a hard error: silently co-owning it interleaves two
    // terminals' turns into one history.
    let _lock = match session_id.as_deref() {
        Some(sid) => Some(SessionLock::acquire(sid)?),
        None => None,
    };

    // Onboarding: with no model and no resolvable key anywhere, run the
    // interactive picker rather than failing with a terse "missing --model".
    // Picks persist to ~/.grace/config.toml (and the key to ~/.grace/.env), so
    // this never asks twice.
    let mut api_key = args.api_key.clone();
    if model.is_none() {
        let (picked_model, picked_base_url, picked_key) =
            crate::ui::wizard::run_onboarding_wizard()?;
        model = Some(picked_model);
        base_url = Some(picked_base_url);
        api_key = api_key.or(Some(picked_key));
    }

    let mut config = Config::from_args(
        base_url,
        api_key,
        model,
        max_iterations,
        args.system_prompt.clone(),
    )?;
    // The request timeout is a config-file knob (`request_timeout_secs`); the
    // CLI has no flag for it. Until this lands in build_transport, honoring
    // the value is what "advertised" means.
    config.request_timeout_secs = settings.request_timeout_secs;
    let transport: Rc<dyn crate::transport::ProviderTransport> = Rc::from(config.build_transport()?);

    // Seed the default skills into ~/.grace/skills on first run.
    let skills_root = skills_dir
        .unwrap_or_else(|| crate::skill::default_root().to_string_lossy().to_string());
    let _ = crate::skill::ensure_default_skills();
    let tools_root = tools_dir.unwrap_or_else(|| "tools".to_string());
    let skills = crate::skill::SkillStore::new(&skills_root);

    // One assembly point for the whole tool set — including `delegate`, whose
    // registration used to be duplicated here in main.rs.
    let registry_options = RegistryOptions::new(&skills_root, &tools_root)
        .with_sessions(Arc::clone(&sessions))
        .with_transport(Rc::clone(&transport))
        .with_compression(config.context_compression.clone());
    let tools = Config::build_registry_full(&registry_options, DelegationDepth::ROOT);

    let mut messages = vec![
        Message::system(crate::config::build_system_prompt(
            config.system_prompt.as_deref(),
            &memory,
            &skills,
            &sessions,
            args.prompt.as_deref(),
        )?)
    ];

    println!(
        "[grace] transport={} model={} ctx={} tools={}",
        transport.name(),
        config.model(),
        transport
            .context_window()
            .map_or_else(|| "?".to_string(), |n| n.to_string()),
        tools.len()
    );

    if let Some(sid) = &session_id {
        let prior = sessions.load(sid)?;
        if !prior.is_empty() {
            let label = sessions
                .get_title(sid)
                .ok()
                .flatten()
                .unwrap_or_else(|| sid.clone());
            println!("[grace] resumed \"{label}\" ({} prior turns)", prior.len());
        }
        messages.extend(prior);
    }

    if chat {
        crate::ui::chat::run_chat(
            transport.as_ref(),
            &tools,
            &mut messages,
            config.max_iterations,
            &sessions,
            session_id.as_deref(),
            &skin,
            &config.context_compression,
            args.verbose,
            &memory,
            &skills,
            args.system_prompt.as_deref(),
        );
        return Ok(ExitCode::SUCCESS);
    }

    run_one_shot(
        &args,
        &config,
        transport.as_ref(),
        &tools,
        &mut messages,
        &sessions,
        session_id.as_deref(),
        &skin,
    )
}

/// Default tool-call round cap when neither the CLI nor the config file says.
pub const DEFAULT_MAX_ITERATIONS: u32 = 256;

/// Handle a flag that means "do this and exit".
fn run_action(action: &Action) -> Result<ExitCode, BoxedError> {
    match action {
        Action::Help => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Action::Version => {
            println!("grace {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        Action::ListSkins => {
            println!("available skins:");
            for name in crate::ui::skin::all_names() {
                println!("  {name}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::SelectSkin => {
            crate::ui::wizard::run_skin_picker()?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Completions(shell) => {
            print_completions(shell);
            Ok(ExitCode::SUCCESS)
        }
        Action::ListSessions => {
            let sessions = SessionStore::open(SessionStore::default_path())?;
            let ids = sessions.list_sessions()?;
            if ids.is_empty() {
                println!("no sessions yet — use --session <id> --chat to start one.");
            } else {
                println!("sessions (most recently active first):");
                let titles = sessions.get_titles(&ids).unwrap_or_default();
                for id in &ids {
                    match titles.get(id) {
                        Some(title) => println!("  {id}  —  {title}"),
                        None => println!("  {id}"),
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::SearchSessions(query) => {
            if query.is_empty() {
                eprintln!("--search-sessions requires a query, e.g. --search-sessions \"refactor\"");
                return Ok(ExitCode::FAILURE);
            }
            let sessions = SessionStore::open(SessionStore::default_path())?;
            let hits = sessions.search(query, 20)?;
            if hits.is_empty() {
                println!("no matches for {query:?}.");
            } else {
                for (session_id, content) in hits {
                    let preview: String = content.chars().take(200).collect();
                    println!("[{session_id}] {preview}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Action::Unknown(flag) => {
            eprintln!("unknown argument: {flag}");
            print_help();
            Ok(ExitCode::FAILURE)
        }
    }
}

/// One-shot `--prompt` mode.
#[allow(clippy::too_many_arguments)]
fn run_one_shot(
    args: &CliArgs,
    config: &Config,
    transport: &(dyn crate::transport::ProviderTransport + '_),
    tools: &crate::tools::ToolRegistry,
    messages: &mut Vec<Message>,
    sessions: &Arc<SessionStore>,
    session_id: Option<&str>,
    skin: &Skin,
) -> Result<ExitCode, BoxedError> {
    let user_text = args.prompt.clone().unwrap_or_default();
    messages.push(Message::user(user_text.clone()));
    if let Some(sid) = session_id {
        let _ = sessions.append(sid, &Message::user(user_text));
    }

    // Same Ctrl-C semantics as chat mode: the handler (installed once) sets
    // the flag the agent loop polls; the loop unwinds with `Interrupted`
    // instead of the default SIGINT killing the process mid-request. A
    // bash child running in the same foreground process group still gets
    // the tty's SIGINT and dies, so an interrupt lands even mid-tool.
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = interrupted.clone();
        let _ = ctrlc::set_handler(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    let mut stream_state = crate::ui::chat::StreamState::default();

    // `--stream` is no longer a separate code path that skips the tool loop.
    // It is a flag on the same loop, so a streamed run can still call tools —
    // the old one-shot-only streaming silently dropped that ability.
    let turn = {
        let mut sink = |event: crate::core::AgentEvent<'_>| {
            crate::ui::chat::print_agent_event(event, skin, args.verbose, &mut stream_state);
        };
        let options = crate::core::TurnOptions::new()
            .with_events(&mut sink)
            .with_interrupt(&interrupted)
            .with_compression(&config.context_compression)
            .streaming(args.stream);
        crate::core::run_turn_with_options(
            transport,
            tools,
            messages,
            config.max_iterations,
            options,
        )
    };
    let outcome = match turn {
        Ok(outcome) => outcome,
        // Ctrl-C: not an error report — the same "stop, I'm going" as a
        // shell would give. Rewind the user row (one-shot never persists
        // the partial tool traffic a chat turn would keep) and exit with
        // the conventional SIGINT status.
        Err(crate::util::AgentError::Interrupted) => {
            if let Some(sid) = session_id {
                let _ = sessions.delete_last_user_row(sid);
            }
            eprintln!("interrupted");
            return Ok(ExitCode::from(130));
        }
        Err(e) => {
            // The user row is persisted before the turn runs; a failed one
            // must not leave it dangling in the session history.
            if let Some(sid) = session_id {
                let _ = sessions.delete_last_user_row(sid);
            }
            return Err(e.into());
        }
    };

    // The agent emits no terminal event after its last ContentFragment, so a
    // trailing line that arrived without a newline would stay buffered. Flush
    // it here, before printing anything else for the turn.  (The closure that
    // held the mutable borrow on stream_state has gone out of scope.)
    crate::ui::chat::flush_stream_to_stdout(&mut stream_state, skin);

    if let Some(sid) = session_id {
        let _ = sessions.append(sid, &Message::assistant(outcome.answer.clone()));
    }
    if outcome.streamed {
        // The answer already appeared live as it was produced; printing the
        // rendered copy underneath would duplicate the whole thing.
        println!();
    } else {
        println!(
            "\n--- answer ---\n{}",
            crate::ui::markdown::render_terminal(&outcome.answer, skin)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Pick the session for a bare `grace --chat`: the most recently active one
/// not already open in another terminal, else a fresh short id.
///
/// The message when everything is locked is deliberate — silently minting a
/// new session looked identical to "resuming your usual session".
pub fn pick_or_create_default_session(sessions: &SessionStore) -> String {
    match crate::session::pick_default_session(sessions) {
        Ok(Some(id)) => id,
        Ok(None) => {
            if !sessions.list_sessions().unwrap_or_default().is_empty() {
                println!(
                    "[grace] every existing session is already open in another terminal — starting a new session."
                );
            }
            crate::ui::chat::short_session_id()
        }
        Err(_) => crate::ui::chat::short_session_id(),
    }
}

/// The `--help` body. A const rather than an inline literal so a test can
/// assert every advertised flag is documented without capturing stdout.
pub const HELP_TEXT: &str = r#"grace — minimal vendor-neutral ReAct agent

Usage:
  grace --chat --session work
  grace --base-url https://api.openai.com/v1 --api-key KEY --model M --prompt "..."
  grace --openrouter --model tencent/hy3:free --prompt "..."   (key from --api-key or $OPENROUTER_API_KEY; free-only keys need a :free model)
  grace --remember "user prefers concise answers"

Flags:
  --prompt <text>        The user instruction (one-shot mode)
  --chat                 Interactive REPL (state persists across turns)
  --session <id>         Persist/resume chat history across process restarts (SQLite)
  --list-sessions        List saved session ids, most recently active first, and exit
  --search-sessions <q>  Full-text search past session turns (SQLite FTS5) and exit
  --skin <name>          Use a named skin for this run (solaris/royal/ocean/sakura, or a custom one)
  --list-skins           List every available skin name and exit
  --select-skin          Interactive skin picker with color previews; saves the choice to ~/.grace/config.toml
  --remember <fact>      Store a durable fact (SQLite memory) and exit
  --memory-path <path>   Override memory DB path (default ~/.grace/memory.db)
  --skills-dir <path>    Directory of skills/<name>/SKILL.md (default ~/.grace/skills)
  --openrouter           Use OpenRouter (HTTPS via reqwest/rustls)
  --base-url <url>       OpenAI-compatible endpoint (http:// or https://) — GitHub Copilot's
                         is https://api.githubcopilot.com; the picker (no --model given) will
                         mint its key via OAuth device flow instead of asking you to paste one
  --api-key <key>        Bearer token (default empty; for OpenRouter uses $OPENROUTER_API_KEY)
  --model <name>         Model id (required for http/openrouter mode)
  --max-iterations <n>   Tool-call round cap (default 256)
  --system <text>        Optional system prompt
   --tools-dir <path>     Directory of tools/<name>/manifest.json plugins (default ./tools)
   --stream               Stream tokens as they arrive (chat and one-shot; tool calls still work)
   --verbose, -v          Show full tool output (patch diffs always show)
   --completions <shell>  Print shell completions (bash, zsh, fish) and exit
   -h, --help             Show this help
   --version, -V          Show version number and exit

Config file (optional, CLI flags always win):
  ~/.grace/config.toml   default_model, default_base_url, memory_path, skills_dir,
                         tools_dir, max_iterations, request_timeout_secs"#;

pub fn print_help() {
    println!("{HELP_TEXT}");
}

/// Print shell completion scripts for bash, zsh, or fish.
pub fn print_completions(shell: &str) {
    match shell.trim() {
        "bash" => print!("{}", bash_completions()),
        "zsh" => print!("{}", zsh_completions()),
        "fish" => print!("{}", fish_completions()),
        other => {
            eprintln!("unknown shell: {other} (supported: bash, zsh, fish)");
        }
    }
}

const FLAGS: &[&str] = &[
    "--prompt",
    "--base-url",
    "--api-key",
    "--model",
    "--openrouter",
    "--chat",
    "--max-iterations",
    "--system",
    "--remember",
    "--session",
    "--list-sessions",
    "--search-sessions",
    "--skills-dir",
    "--skin",
    "--list-skins",
    "--select-skin",
    "--memory-path",
    "--tools-dir",
    "--stream",
    "--verbose",
    "--completions",
    "--help",
    "-h",
    "--version",
    "-V",
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
        out.push_str(&format!(
            "complete -c grace -l {} -d 'grace flag'\n",
            f.trim_start_matches('-')
        ));
    }
    out.push_str("complete -c grace -l skin -d 'skin name' -a 'solaris royal ocean sakura'\n");
    out.push_str("complete -c grace -l completions -d 'shell' -a 'bash zsh fish'\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> CliArgs {
        CliArgs::parse(argv.iter().copied())
    }

    #[test]
    fn no_arguments_means_chat() {
        // A bare `grace` should drop into the REPL, not error out.
        let a = parse(&[]);
        assert!(a.wants_chat());
        assert!(a.action.is_none());
    }

    #[test]
    fn a_prompt_alone_is_one_shot_not_chat() {
        let a = parse(&["--prompt", "hello"]);
        assert_eq!(a.prompt.as_deref(), Some("hello"));
        assert!(!a.wants_chat());
    }

    #[test]
    fn remember_alone_is_not_chat() {
        let a = parse(&["--remember", "a fact"]);
        assert_eq!(a.remember.as_deref(), Some("a fact"));
        assert!(!a.wants_chat());
    }

    #[test]
    fn explicit_chat_with_a_prompt_still_means_chat() {
        assert!(parse(&["--chat", "--prompt", "x"]).wants_chat());
    }

    #[test]
    fn openrouter_is_sugar_for_its_base_url() {
        let a = parse(&["--openrouter"]);
        assert_eq!(
            a.base_url.as_deref(),
            Some(crate::config::OPENROUTER_BASE_URL)
        );
    }

    #[test]
    fn env_line_parsing_tolerates_export_prefix_and_quotes() {
        assert_eq!(
            parse_env_line("KEY=value"),
            Some(("KEY".to_string(), "value".to_string()))
        );
        assert_eq!(
            parse_env_line("  export KEY=value  "),
            Some(("KEY".to_string(), "value".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY=\"double quoted\""),
            Some(("KEY".to_string(), "double quoted".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY='single quoted'"),
            Some(("KEY".to_string(), "single quoted".to_string()))
        );
        assert_eq!(parse_env_line("KEY=keep  inner  spaces"), Some(("KEY".to_string(), "keep  inner  spaces".to_string())));
        // Empty values are absent, not "set to nothing".
        assert_eq!(parse_env_line("KEY="), None);
        assert_eq!(parse_env_line("KEY=''"), None);
        // Comments, blanks, and valueless lines are skipped.
        assert_eq!(parse_env_line("# KEY=commented"), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("NOEQUALS"), None);
        assert_eq!(parse_env_line("=orphan value"), None);
    }

    fn scratch_env(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "grace_env_test_{}_{tag}.env",
            std::process::id()
        ))
    }

    #[test]
    fn upsert_touches_only_the_target_key_and_forces_0600() {
        use std::path::Path;
        let path = scratch_env("upsert");
        let _ = std::fs::remove_file(&path);
        // Existing file with another provider's key in the export form,
        // plus a comment and a blank line that must all survive.
        std::fs::write(
            &path,
            "# comment\nexport GITHUB_COPILOT_TOKEN=old-token\n\nOPENROUTER_API_KEY=or-key\n",
        )
        .unwrap();
        // Pre-existing world-readable mode must be tightened, not kept.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        upsert_env_file_at(Path::new(&path), "OPENROUTER_API_KEY", "or-key-v2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after,
            "# comment\nexport GITHUB_COPILOT_TOKEN=old-token\n\nOPENROUTER_API_KEY=or-key-v2\n"
        );
        assert!(
            after.contains("GITHUB_COPILOT_TOKEN=old-token"),
            "an unrelated key must survive an upsert of a different key"
        );

        // Upserting a key in its `export ` form rewrites that very line.
        upsert_env_file_at(Path::new(&path), "GITHUB_COPILOT_TOKEN", "gh-key-v2").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("GITHUB_COPILOT_TOKEN=gh-key-v2"), "{after}");
        assert!(!after.contains("old-token"), "{after}");
        assert_eq!(after.lines().count(), 4, "no line may be duplicated: {after}");

        // The file must be 0600 (keys inside).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be 0600, got {mode:o}");
        }

        // A brand-new key is appended.
        upsert_env_file_at(Path::new(&path), "GRACE_API_KEY", "gk").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("GRACE_API_KEY=gk"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn urls_keys_and_models_are_trimmed() {
        // A value pasted from a password manager carries a trailing newline;
        // sending that verbatim fails with an opaque auth error.
        let a = parse(&[
            "--base-url",
            " https://x/v1\n",
            "--api-key",
            "  sk-abc \n",
            "--model",
            " gpt-4o ",
        ]);
        assert_eq!(a.base_url.as_deref(), Some("https://x/v1"));
        assert_eq!(a.api_key.as_deref(), Some("sk-abc"));
        assert_eq!(a.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn prompts_are_not_trimmed_because_whitespace_can_be_meaningful() {
        let a = parse(&["--prompt", "  indented code  "]);
        assert_eq!(a.prompt.as_deref(), Some("  indented code  "));
    }

    #[test]
    fn max_iterations_is_none_when_absent_so_the_config_file_can_win() {
        // Regression: the old code defaulted to 16 and then compared against
        // 16 to decide whether the config file may override — so a user who
        // explicitly passed `--max-iterations 16` was silently overridden.
        assert_eq!(parse(&[]).max_iterations, None);
        assert_eq!(parse(&["--max-iterations", "16"]).max_iterations, Some(16));
    }

    #[test]
    fn a_non_numeric_iteration_count_is_ignored_rather_than_panicking() {
        assert_eq!(parse(&["--max-iterations", "lots"]).max_iterations, None);
    }

    #[test]
    fn boolean_flags_parse() {
        let a = parse(&["--chat", "--stream", "--verbose"]);
        assert!(a.chat && a.stream && a.verbose);
        assert!(parse(&["-v"]).verbose);
    }

    #[test]
    fn terminal_actions_are_recognized() {
        assert_eq!(parse(&["--help"]).action, Some(Action::Help));
        assert_eq!(parse(&["-h"]).action, Some(Action::Help));
        assert_eq!(parse(&["--list-skins"]).action, Some(Action::ListSkins));
        assert_eq!(parse(&["--select-skin"]).action, Some(Action::SelectSkin));
        assert_eq!(
            parse(&["--list-sessions"]).action,
            Some(Action::ListSessions)
        );
        assert_eq!(
            parse(&["--search-sessions", "rust"]).action,
            Some(Action::SearchSessions("rust".into()))
        );
        assert_eq!(
            parse(&["--completions", "zsh"]).action,
            Some(Action::Completions("zsh".into()))
        );
        assert_eq!(parse(&["--version"]).action, Some(Action::Version));
        assert_eq!(parse(&["-V"]).action, Some(Action::Version));
    }

    #[test]
    fn an_unknown_flag_is_captured_rather_than_exiting_during_parse() {
        // Parsing stays pure; the caller decides what to do about it.
        assert_eq!(
            parse(&["--frobnicate"]).action,
            Some(Action::Unknown("--frobnicate".into()))
        );
    }

    #[test]
    fn a_flag_missing_its_value_does_not_panic() {
        // `grace --model` with nothing after it must parse, then fail later
        // with a clear "missing --model", not index out of bounds.
        assert_eq!(parse(&["--model"]).model, None);
        assert_eq!(parse(&["--prompt"]).prompt, None);
        assert_eq!(
            parse(&["--search-sessions"]).action,
            Some(Action::SearchSessions(String::new()))
        );
    }

    #[test]
    fn all_paths_and_dirs_parse() {
        let a = parse(&[
            "--session",
            "work",
            "--skills-dir",
            "/s",
            "--memory-path",
            "/m.db",
            "--tools-dir",
            "/t",
            "--skin",
            "ocean",
            "--system",
            "be terse",
        ]);
        assert_eq!(a.session_id.as_deref(), Some("work"));
        assert_eq!(a.skills_dir.as_deref(), Some("/s"));
        assert_eq!(a.memory_path.as_deref(), Some("/m.db"));
        assert_eq!(a.tools_dir.as_deref(), Some("/t"));
        assert_eq!(a.skin.as_deref(), Some("ocean"));
        assert_eq!(a.system_prompt.as_deref(), Some("be terse"));
    }

    #[test]
    fn a_later_flag_wins_over_an_earlier_one() {
        let a = parse(&["--model", "a", "--model", "b"]);
        assert_eq!(a.model.as_deref(), Some("b"));
    }

    #[test]
    fn help_text_documents_every_advertised_flag() {
        // A flag that exists but is undocumented is a flag nobody uses.
        for flag in FLAGS {
            assert!(HELP_TEXT.contains(flag), "help text omits {flag}");
        }
    }

    #[test]
    fn completions_are_generated_for_each_supported_shell() {
        assert!(bash_completions().contains("complete -F _grace"));
        assert!(zsh_completions().contains("#compdef grace"));
        assert!(fish_completions().contains("complete -c grace"));
    }

    #[test]
    fn every_flag_appears_in_the_bash_completion_list() {
        let bash = bash_completions();
        for flag in FLAGS {
            assert!(bash.contains(flag), "bash completions omit {flag}");
        }
    }

    #[test]
    fn the_default_iteration_cap_is_generous_enough_for_real_work() {
        assert_eq!(DEFAULT_MAX_ITERATIONS, 256);
    }
}
