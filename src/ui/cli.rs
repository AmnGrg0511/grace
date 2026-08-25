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
    pub max_iterations: Option<u32>,
    pub system_prompt: Option<String>,
    pub remember: Option<String>,
    pub session_id: Option<String>,
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
        self.prompt.is_none() && self.remember.is_none()
    }
}

type BoxedError = Box<dyn std::error::Error>;

/// Entry point. Loads `~/.grace/.env`, parses argv, and dispatches.
pub fn main() -> ExitCode {
    crate::config::env::load_dotenv();
    let args = CliArgs::parse(std::env::args().skip(1));
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Perform whatever `args` asks for.
pub fn run(mut args: CliArgs) -> Result<ExitCode, BoxedError> {
    if let Some(action) = args.action.take() {
        return run_action(&action);
    }

    // Layered settings: defaults -> ~/.grace/config.toml -> CLI flags.
    // The CLI no longer carries path flags: ~/.grace/config.toml is the sole
    // override for where things live.
    let settings = Settings::load();
    let skin = crate::ui::skin::by_name(settings.skin.as_deref());

    let mut base_url = args.base_url.clone();
    let mut model = args.model.clone();
    let mut settings_iterations: Option<u32> = None;
    settings.merge_into_args(&mut base_url, &mut model, &mut settings_iterations);
    // CLI wins over the config file, which wins over the built-in default.
    let max_iterations = args
        .max_iterations
        .or(settings_iterations)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Memory is always opened — it is a cheap local file, not a network dep.
    let memory = Memory::open(
        settings
            .memory_path
            .clone()
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
    // The persisted reasoning-effort hint rides along at startup, same as the
    // model and endpoint themselves.
    if let Some(level) = settings.default_reasoning.clone() {
        transport.set_reasoning(Some(&level));
    }

    // Seed the default skills into ~/.grace/skills on first run.
    let skills_root = settings
        .skills_dir
        .clone()
        .unwrap_or_else(|| crate::skill::default_root().to_string_lossy().to_string());
    let _ = crate::skill::ensure_default_skills();
    let tools_root = settings
        .tools_dir
        .clone()
        .unwrap_or_else(|| crate::tools::PluginToolStore::default_root().to_string_lossy().to_string());
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

    // In chat mode these run up top under the wordmark (see `run_chat`), so
    // they only print here for one-shot runs, which have no banner.
    if !chat {
        println!(
            "[grace] transport={} model={} ctx={} tools={}",
            transport.name(),
            config.model(),
            transport
                .context_window()
                .map_or_else(|| "?".to_string(), |n| n.to_string()),
            tools.len()
        );
    }

    if let Some(sid) = &session_id {
        let prior = sessions.load(sid)?;
        if !prior.is_empty() && !chat {
            let label = crate::ui::chat::session_label(&sessions, sid);
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
        Action::Completions(shell) => {
            print_completions(shell);
            Ok(ExitCode::SUCCESS)
        }
        Action::ListSessions => {
            let sessions = SessionStore::open(SessionStore::default_path())?;
            let ids = sessions.list_sessions()?;
            if ids.is_empty() {
                println!("no sessions yet — use --session <id> to start one.");
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
    // One-shot turns run tools too — same kill-the-command contract.
    crate::tools::builtins::terminal::set_turn_interrupt(interrupted.clone());
    let turn = crate::ui::stream::run_turn_displayed(
        transport,
        tools,
        messages,
        config.max_iterations,
        skin,
        false,
        // One-shot runs are not streamed: tool events still print live
        // through the sink, and the answer is rendered once at the end.
        false,
        &interrupted,
        &config.context_compression,
    );
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

/// Pick the session for a bare `grace`: the most recently active one
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
  grace                        Interactive REPL (bare grace means chat)
  grace --session work         Chat in a named, resumable session
  grace --prompt "..."         One-shot: run a turn, print the answer, exit
  grace --base-url https://api.openai.com/v1 --api-key KEY --model M --prompt "..."
  grace --remember "user prefers concise answers"

Flags:
  --prompt <text>        The user instruction (one-shot mode)
  --session <id>         Persist/resume chat history across process restarts (SQLite)
  --list-sessions        List saved session ids, most recently active first, and exit
  --search-sessions <q>  Full-text search past session turns (SQLite FTS5) and exit
  --remember <fact>      Store a durable fact (SQLite memory) and exit
  --base-url <url>       OpenAI-compatible endpoint (http:// or https://) — GitHub Copilot's
                          is https://api.githubcopilot.com; the picker (no --model given) will
                          mint its key via OAuth device flow instead of asking you to paste one
                          (OpenRouter: https://openrouter.ai/api/v1, key from --api-key
                          or $OPENROUTER_API_KEY)
  --api-key <key>        Bearer token (default: the env var matching the base URL's host)
  --model <name>         Model id (required for http mode)
  --max-iterations <n>   Tool-call round cap (default 256)
  --system <text>        Optional system prompt
  --completions <shell>  Print shell completions (bash, zsh, fish) and exit
  -h, --help             Show this help
  --version, -V          Show version number and exit

Config file (optional, CLI flags always win):
  ~/.grace/config.toml   default_model, default_base_url, skin, memory_path, skills_dir,
                          tools_dir (default ~/.grace/tools), max_iterations,
                          request_timeout_secs"#;

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
    "--max-iterations",
    "--system",
    "--remember",
    "--session",
    "--list-sessions",
    "--search-sessions",
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
    fn terminal_actions_are_recognized() {
        assert_eq!(parse(&["--help"]).action, Some(Action::Help));
        assert_eq!(parse(&["-h"]).action, Some(Action::Help));
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
    fn session_and_system_parse() {
        let a = parse(&["--session", "work", "--system", "be terse"]);
        assert_eq!(a.session_id.as_deref(), Some("work"));
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
