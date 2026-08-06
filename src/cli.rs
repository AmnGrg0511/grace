//! CLI flag list, `--help` text, and shell-completion generation.

pub(crate) fn print_help() {
    let help = r#"grace — minimal vendor-neutral ReAct agent

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
  --skills-dir <path>    Directory of skills/<name>/SKILL.md (default ./skills)
  --openrouter           Use OpenRouter (HTTPS via reqwest/rustls)
  --copilot              Use GitHub Copilot (device flow auth)
  --base-url <url>       OpenAI-compatible endpoint (http:// or https://)
  --api-key <key>        Bearer token (default empty; for OpenRouter uses $OPENROUTER_API_KEY)
  --model <name>         Model id (required for http/openrouter/copilot mode)
  --max-iterations <n>   Tool-call round cap (default 16)
  --system <text>        Optional system prompt
  --tools-dir <path>     Directory of tools/<name>/manifest.json plugins (default ./tools)
  --stream               Stream tokens as they arrive (one-shot HTTP mode only)
  --completions <shell>  Print shell completions (bash, zsh, fish) and exit
  -h, --help             Show this help

Config file (optional, CLI flags always win):
  ~/.grace/config.toml   default_model, default_base_url, memory_path, skills_dir,
                         tools_dir, max_iterations, request_timeout_secs"#;
    println!("{help}");
}

/// Print shell completion scripts for bash, zsh, or fish.
pub(crate) fn print_completions(shell: &str) {
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
    "--prompt", "--base-url", "--api-key", "--model", "--openrouter",
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
