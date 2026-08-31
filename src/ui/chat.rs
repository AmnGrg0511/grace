//! The interactive chat REPL: `run_chat`, per-turn execution, and the
//! slash-command dispatch. The streaming engine lives in [`crate::ui::stream`],
//! the banner/status line in [`crate::ui::status`], and the model, provider,
//! and effort commands in [`crate::ui::model`].

use crate::core::context::ContextCompressionConfig;
use crate::message::Message;
use crate::session::SessionStore;
use crate::ui::skin::{reset, Role, Skin};

use crate::ui::line_reader::LineReader;
use crate::ui::model::{handle_effort_command, handle_model_command, handle_provider_command, short_url};
use crate::ui::status::{chat_banner, prompt_label, print_status_line};


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


/// One picker row per slash command: the `/name` label with the one-line
/// summary as the dim sublabel in its own quieter voice — the shared row
/// shape of the command palette and the ambiguous-abbreviation picker.
fn slash_command_rows(
    cmds: impl IntoIterator<Item = &'static crate::ui::commands::SlashCommand>,
) -> Vec<crate::ui::picker::Pick> {
    cmds.into_iter()
        .map(|c| {
            crate::ui::picker::Pick::new(c.name, format!("/{}", c.name))
                .hint(c.summary)
                .hint_role(crate::ui::skin::Role::Hint)
        })
        .collect()
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
    // Hidden by default; the `/verbose` command toggles it mid-chat.
    let mut verbose = false;

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

    // Long-running tools (bash) watch the same latch so a ctrl-c kills the
    // running command instead of waiting for it to finish on its own.
    crate::tools::builtins::terminal::set_turn_interrupt(interrupted.clone());

    // Start on a clean screen so the wordmark sits at the top of the view.
    // One-shot, pre-conversation: not part of the append-only stream path.
    // TTY only — piped/one-shot stdout never receives screen-control escapes.
    use std::io::{IsTerminal, Write};
    if std::io::stdout().is_terminal() {
        let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
        let _ = std::io::stdout().flush();
    }

    println!("{}", chat_banner(&skin));
    // The transport/resume summary sits directly under the wordmark so it's
    // visible at the very start of the session (one-shot prints it up top,
    // where there's no banner for it to sit under).
    let prior_turns = messages.len().saturating_sub(1); // exclude the system prompt
    println!(
        "[grace] transport={} model={} ctx={} tools={}",
        transport.name(),
        transport.current_model().unwrap_or_else(|| "?".into()),
        transport
            .context_window()
            .map_or_else(|| "?".to_string(), |n| n.to_string()),
        tools.len(),
    );
    if prior_turns > 0 {
        let label = current_session
            .as_deref()
            .map(|sid| session_label(sessions, sid))
            .unwrap_or_else(|| "?".to_string());
        println!("[grace] resumed \"{label}\" ({prior_turns} prior turns)");
    }
    // The persona greeting is skin-controlled (see `Skin::welcome`), so a
    // custom skin sets its own tone without a code change.
    println!("{}\n", skin.paint(Role::Accent, skin.welcome));
    println!(
        "{}\n",
        skin.paint_badge(
            Role::Hint,
            "\"/\" for commands · /model the model list · /provider the endpoint · /effort the reasoning · /skin the theme · /info this session · /verbose tool output · /exit to leave"
        )
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
            // `grace --session <id>` should already show that
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
            let items = slash_command_rows(crate::ui::commands::SLASH_COMMANDS.iter());
            if let Some(name) = crate::ui::picker::pick(&items, &skin, "pick a command") {
                pending_input = Some(format!("/{name} "));
            }
            continue;
        }
        // A slash command is its first *word*: `/modelxyz` is model text, not
        // a mistyped `/model`. An unambiguous abbreviation (`/e`, `/m`) is
        // rewritten to the command it names; an ambiguous one (`/s`) opens a
        // picker between the candidates; no match falls through to the model.
        let mut cmd_word = text.split_whitespace().next().unwrap().to_string();
        let rest = text[cmd_word.len()..].trim();
        if cmd_word.len() > 1 && cmd_word.starts_with('/') {
            let word = cmd_word[1..].to_string();
            if crate::ui::commands::resolve(&word).is_none() {
                let candidates = crate::ui::commands::resolve_prefix(&word);
                match candidates.as_slice() {
                    [] => {}
                    [only] => cmd_word = format!("/{}", only.name),
                    many => {
                        let items = slash_command_rows(many.iter().copied());
                        if let Some(name) = crate::ui::picker::pick(&items, &skin, "pick a command") {
                            pending_input = Some(format!("/{name} {rest}"));
                        }
                        continue;
                    }
                }
            }
        }
        match cmd_word.strip_prefix('/') {
            Some("exit") => {
                println!("{}", skin.paint(Role::Accent, skin.goodbye));
                break;
            }
            Some("model") => {
                handle_model_command(transport, rest, &mut reader, &skin, &mut cached_context_window);
                continue;
            }
            Some("skin") => {
                handle_skin_command(rest, &mut skin);
                reader.set_skin(skin);
                continue;
            }
            Some("session") => {
                handle_session_command(sessions, messages, &mut current_session, &mut reader, memory, &mut session_lock, system_override, skills, &mut last_usage, &skin);
                continue;
            }
            Some("jump") => {
                // A jump returns the re-sent prompt; queue it so the next
                // iteration runs it as a fresh turn (the session was already
                // rewound by the command).
                if let Some(queued) =
                    handle_jump_command(sessions, messages, &current_session, &mut reader, &skin)
                {
                    pending_input = Some(queued);
                }
                continue;
            }
            Some("compact") => {
                handle_compact_command(messages, transport, compression_config, &skin);
                continue;
            }
            Some("dump") => {
                handle_dump_command(messages, rest);
                continue;
            }
            Some("rename") => {
                handle_rename_command(sessions, current_session.as_deref(), rest, &mut reader, &skin);
                continue;
            }
            Some("provider") => {
                handle_provider_command(transport, &mut reader, &skin, &mut cached_context_window);
                continue;
            }
            Some("effort") => {
                handle_effort_command(transport, rest, &skin);
                continue;
            }
            Some("info") => {
                handle_info_command(
                    InfoView {
                        sessions,
                        current_session: current_session.as_deref(),
                        transport,
                        context_entries: messages.len(),
                        window: cached_context_window,
                        last_usage,
                        started_at,
                        read_only: tools.is_read_only(),
                    },
                    &skin,
                );
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
            Some("clear") => {
                // Purely visual: same clear+home as startup, then the banner
                // again. Context, history file, and usage stay untouched.
                if std::io::stdout().is_terminal() {
                    let _ = write!(std::io::stdout(), "\x1b[2J\x1b[H");
                    let _ = std::io::stdout().flush();
                }
                println!("{}\n", chat_banner(&skin));
                println!(
                    "{}",
                    skin.paint_badge(Role::Hint, "(view cleared — conversation context unchanged)")
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

/// `/rename [name]` — set the open session's title. With no argument it asks
/// interactively. The title is what `/session` shows, so renaming is how a
/// cryptic auto-title becomes a findable one.
fn handle_rename_command(
    sessions: &crate::session::store::SessionStore,
    current_session: Option<&str>,
    rest: &str,
    reader: &mut LineReader,
    skin: &Skin,
) {
    use crate::ui::skin::Role;
    let Some(sid) = current_session else {
        println!(
            "{}",
            skin.paint_badge(Role::Error, "no open session to rename — start one with a message or /session")
        );
        return;
    };
    // A bare `/rename` opens on the current name already typed in, so a user
    // tweaking it edits in place; an explicit `/rename <name>` replaces it.
    let current_title = sessions.get_title(sid).ok().flatten().unwrap_or_default();
    let mut title = rest.trim().to_string();
    if title.is_empty() {
        let Some(asked) = reader.read_line_default("new name: ", &current_title) else {
            return;
        };
        title = asked.trim().to_string();
    }
    if title.is_empty() {
        println!("{}", skin.paint_badge(Role::Error, "a name needs at least one character"));
        return;
    }
    // Same cap as auto-titles so picker rows never blow their layout budget.
    let title: String = title.chars().take(80).collect();
    match sessions.set_title(sid, &title) {
        Ok(()) => println!(
            "{}",
            skin.paint_badge(Role::Success, &format!("renamed to \"{title}\""))
        ),
        Err(e) => println!("{}", skin.paint_badge(Role::Error, &format!("rename failed: {e}"))),
    }
}

/// `/info` — the session report: what model/provider posture this session
/// runs under, how full the context window is, and per-turn token usage as
/// recorded by the provider. Spend in currency is deliberately absent: grace
/// ships no pricing tables, so any $ figure would be fabricated.
struct InfoView<'a> {
    sessions: &'a crate::session::store::SessionStore,
    current_session: Option<&'a str>,
    transport: &'a (dyn crate::transport::ProviderTransport + 'a),
    context_entries: usize,
    window: Option<u32>,
    last_usage: Option<crate::transport::TokenUsage>,
    started_at: std::time::Instant,
    read_only: bool,
}

fn handle_info_command(view: InfoView<'_>, skin: &Skin) {
    let InfoView {
        sessions,
        current_session,
        transport,
        context_entries,
        window,
        last_usage,
        started_at,
        read_only,
    } = view;
    use crate::ui::skin::Role;
    let accent = |t: &str| skin.paint(Role::Accent, t);
    let dim = |t: &str| skin.paint(Role::Hint, t);
    let group = |n: u64| -> String {
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        out
    };
    let secs = started_at.elapsed().as_secs();
    let elapsed = if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    };

    println!("{}", accent("session"));
    match current_session {
        Some(sid) => println!(
            "  {}{}",
            dim("name     "),
            match sessions.get_title(sid).ok().flatten() {
                Some(t) => t,
                None => sid.to_string(),
            }
        ),
        None => println!(
            "  {}{}",
            dim("name     "),
            dim("(unsaved — send a message to start one)")
        ),
    }
    println!("  {}{}", dim("elapsed  "), elapsed);
    println!(
        "  {}{}",
        dim("readonly "),
        if read_only { "on" } else { "off" }
    );

    println!("{}", accent("model"));
    let model = transport.current_model().unwrap_or_else(|| "(unset)".into());
    let effort = transport
        .current_reasoning()
        .unwrap_or_else(|| "off".into());
    println!(
        "  {}{}",
        dim("provider "),
        transport
            .current_base_url()
            .as_deref()
            .map(short_url)
            .unwrap_or_else(|| "(unknown)".to_string())
    );
    println!("  {}{}", dim("model    "), model);
    println!("  {}{}", dim("effort   "), effort);
    println!(
        "  {}{}",
        dim("window   "),
        match window {
            Some(w) => format!("{} tokens", crate::ui::status::human_tokens(w as u64)),
            None => "unknown".to_string(),
        }
    );
    println!(
        "  {}{}",
        dim("context  "),
        match last_usage {
            Some(u) => format!(
                "{} tokens measured (provider)",
                group(u.prompt_tokens)
            ),
            None => format!("{context_entries} entries, not yet measured"),
        }
    );

    if let Some(sid) = current_session {
        if let Ok(rows) = sessions.turn_usage(sid) {
            println!("{}", accent("turns"));
            if rows.is_empty() {
                println!("  {}", dim("no usage recorded yet (or provider omits it)"));
                return;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let tin: u64 = rows.iter().map(|r| r.input_tokens).sum();
            let tout: u64 = rows.iter().map(|r| r.output_tokens).sum();
            // Measure before painting: recency labels ("2h 47m ago" vs "3m
            // ago") and model names are variable width, so the in/out
            // columns only line up if every row — and the total — shares
            // their true maxes. Pad the plain text, then paint: ANSI
            // escapes carry no display width and must not count.
            let recs: Vec<String> = rows
                .iter()
                .map(|r| recency_label(r.created_at, now))
                .collect();
            let in_s: Vec<String> = rows.iter().map(|r| group(r.input_tokens)).collect();
            let out_s: Vec<String> = rows.iter().map(|r| group(r.output_tokens)).collect();
            let recency_w = recs.iter().map(|s| s.len()).max().unwrap_or(0);
            let model_w = rows.iter().map(|r| r.model.len()).max().unwrap_or(0);
            let total_in = group(tin);
            let total_out = group(tout);
            let in_w = in_s.iter().map(|s| s.len()).max().unwrap_or(0).max(total_in.len());
            let out_w = out_s.iter().map(|s| s.len()).max().unwrap_or(0).max(total_out.len());
            for (i, r) in rows.iter().enumerate() {
                let rec = format!("{:<recency_w$}", recs[i]);
                let mdl = format!("{:<model_w$}", r.model);
                let inp = format!("{:>in_w$}", in_s[i]);
                let outp = format!("{:>out_w$}", out_s[i]);
                println!(
                    "  {:>5}  {}  {}  {} in · {} out",
                    i + 1,
                    dim(&rec),
                    mdl,
                    inp,
                    outp
                );
            }
            println!(
                "  {total:>5}  {blank:<recency_w$}  {blank:<model_w$}  {in_cell:>in_w$} in · {out_cell:>out_w$} out across {count} turns",
                total = dim("total"),
                blank = "",
                in_cell = accent(&total_in),
                out_cell = accent(&total_out),
                count = rows.len()
            );
        }
    }
}



/// A compact color snapshot of `skin` for picker rows: prompt glyph, a
/// tool-call line, and an answer with inline code — every role color at
/// once, using real Grace shapes. Kept under ~70 columns so it never wraps.
pub(crate) fn skin_preview_lines(s: &Skin) -> Vec<String> {
    vec![
        format!(
            "{p}{glyph} you{r}",
            p = s.style(Role::Prompt).render(),
            glyph = s.prompt_glyph,
            r = reset(),
        ),
        format!(
            "{tb}● {tn}terminal{r} {td}⎿ file1.txt  file2.txt{r}",
            tb = s.style(Role::ToolBullet).render(),
            tn = s.style(Role::ToolName).render(),
            td = s.style(Role::ToolDim).render(),
            r = reset(),
        ),
        format!(
            "{a}{ag}{r} Answer with {c}`ls`{r}.",
            a = s.style(Role::Answer).render(),
            ag = s.answer_glyph,
            c = s.style(Role::Code).render(),
            r = reset(),
        ),
    ]
}

/// `/skin` (interactive picker with color previews) or `/skin <name>`
/// (direct switch) mid-chat. The choice is persisted to
/// ~/.grace/config.toml so the next run starts on the same skin.
fn handle_skin_command(arg: &str, skin: &mut Skin) {
    let names = crate::ui::skin::all_names();
    let picked = if arg.is_empty() {
        let items: Vec<crate::ui::picker::Pick> = names
            .iter()
            .map(|n| {
                let mut row =
                    crate::ui::picker::Pick::new(n.clone(), n.clone())
                        .extra_lines(skin_preview_lines(&crate::ui::skin::by_name(Some(n))));
                if skin.name == n.as_str() {
                    row = row.disabled().hint("already active");
                }
                row
            })
            .collect();
        match crate::ui::picker::pick(&items, skin, "pick a skin") {
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

/// Human recency for a session's last activity, unix seconds in/out.
/// Pure and timezone-free by design: "<1m ago", "45m ago", "3h 12m ago",
/// "2d 3h ago" — the shape the session picker shows next to each title.
pub(crate) fn recency_label(last_active: i64, now: i64) -> String {
    let delta = (now - last_active).max(0);
    if delta < 60 {
        return "now".to_string();
    }
    let mins = delta / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    let rem = mins % 60;
    if hours < 24 {
        if rem == 0 {
            return format!("{hours}h ago");
        }
        return format!("{hours}h {rem}m ago");
    }
    let days = hours / 24;
    let rem_h = hours % 24;
    if rem_h == 0 {
        format!("{days}d ago")
    } else {
        format!("{days}d {rem_h}h ago")
    }
}

/// The human face of a session: its generated title, else (never titled)
/// its first message truncated to a line, else the raw id. The switch
/// message, the startup resume line, and the one-shot resume line all
/// name sessions through this one chain so a session is always called by
/// the same name everywhere.
pub(crate) fn session_label(sessions: &SessionStore, sid: &str) -> String {
    if let Some(t) = sessions.get_title(sid).ok().flatten() {
        return t;
    }
    // The first stored row is persisted as sent (auto-recalled block included
    // for a stable resume prefix) — a label shows the user's words, not the block.
    let first = sessions
        .load(sid)
        .ok()
        .and_then(|m| m.first().map(|m| m.content.clone()));
    match first {
        Some(content) => {
            let words = crate::recall::strip_recall_block(&content).trim().to_string();
            if words.is_empty() {
                sid.to_string()
            } else {
                words.chars().take(50).collect()
            }
        }
        None => sid.to_string(),
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
    skin: &Skin,
) {
    use crate::ui::picker::{pick, Pick};

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

    let titles = sessions.get_titles(&session_list).unwrap_or_default();
    let recency: std::collections::HashMap<String, i64> = sessions
        .last_active()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let labels: Vec<String> = session_list
        .iter()
        .map(|sid| {
            titles.get(sid).cloned().unwrap_or_else(|| {
                sessions
                    .load(sid)
                    .ok()
                    .and_then(|m| m.first().map(|m| m.content.clone()))
                    .map(|s| crate::recall::strip_recall_block(&s))
                    .filter(|s| !s.trim().is_empty())
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
    let mut items: Vec<Pick> = Vec::with_capacity(session_list.len() + 1);
    items.push(Pick::new("\u{1}new", "start a fresh session"));
    for (i, sid) in session_list.iter().enumerate() {
        let label = &labels[i];
        let disambiguated = if counts.get(label.as_str()).copied().unwrap_or(0) > 1 {
            format!("{label} ({sid})")
        } else {
            label.clone()
        };
        let is_current = current_session.as_deref() == Some(sid.as_str());
        // Recency in a skin accent, visually distinct from the dim title —
        // "current" for the live session, else how long ago it was active.
        // No brackets; the color alone separates title from timestamp.
        let accent = skin.paint(Role::Code, &if is_current {
            "current".to_string()
        } else {
            match recency.get(sid) {
                Some(&ts) => recency_label(ts, now),
                None => "—".to_string(),
            }
        });
        let lock_marker = if crate::session::SessionLock::is_held(sid) {
            format!("  {}", skin.paint(Role::ToolDim, "· open in another terminal"))
        } else {
            String::new()
        };
        let mut row = Pick::new(sid.clone(), disambiguated)
            .hint(format!("{accent}{lock_marker}"))
            .verbatim_sub();
        if is_current {
            row = row.disabled();
        }
        items.push(row);
    }
    let Some(choice) = pick(&items, skin, "pick a session") else {
        println!("staying on current session.");
        return;
    };
    if choice == "\u{1}new" {
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
        return;
    }
    let sid = &choice;
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
            // The recall query is the session's first prompt — the user's
            // words, not the auto-recalled block that was appended when it was sent.
            let query = loaded
                .iter()
                .find(|m| m.role == crate::message::Role::User)
                .map(|m| crate::recall::strip_recall_block(&m.content));
            messages.clear();
            messages.push(fresh_system(query.as_deref()));
            messages.extend(loaded);
            *current_session = Some(sid.clone());
            *session_lock = crate::session::SessionLock::acquire(sid).ok();
            reader.set_history_scope(Some(sid));
            // Pick up the switched-to session's own measured usage so
            // the status bar doesn't keep showing the outgoing
            // session's count (or an estimate) for a session that has
            // real usage on record.
            *last_usage = sessions.load_usage(sid).ok().flatten();
            let label = session_label(sessions, sid);
            println!("switched to session \"{label}\" ({} messages loaded).", messages.len());
        }
        Err(e) => println!("error loading session: {e}"),
    }
}

/// The visible "why" for a turn whose model answer has no text: an empty turn
/// must still say so, or the user is left staring at a silent no-output — the
/// worst failure mode. Shared by the interactive and one-shot paths, both of
/// which print this after their (empty) answer. An answer with any text gets
/// an empty string (nothing printed).
pub(crate) fn empty_answer_note(skin: &Skin, answer: &str) -> String {
    if answer.trim().is_empty() {
        skin.paint_badge(
            Role::Hint,
            "(no output — the model returned nothing; try rephrasing)",
        )
    } else {
        String::new()
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
    // Snapshot the turn count BEFORE appending — drives auto-(re)titling
    // below. Counted as user rows (not total rows, which now also include
    // tool traffic): each real turn contributes exactly one, so this is the
    // 0-based index of the turn about to happen.
    let turn_index = session_id
        .and_then(|sid| sessions.count_user_rows(sid).ok())
        .unwrap_or(0);
    // The user row is persisted exactly as sent — recall hint included — so
    // the on-disk prefix is byte-identical to the request the model actually
    // saw. A resumed session replays it verbatim (recall is only re-derived
    // for fresh input, never for loaded history), keeping the provider's
    // prompt-cache prefix warm.
    let user_msg = Message::user(match recall_hint {
        Some(hint) => format!("{text}\n{hint}"),
        None => text.to_string(),
    });
    messages.push(user_msg.clone());
    if let Some(sid) = session_id {
        let _ = sessions.append(sid, &user_msg);
    }
    // Clear any interrupt latched from a previous turn before starting a new
    // one — otherwise a stale Ctrl-C would abort every turn from here on.
    interrupted.store(false, std::sync::atomic::Ordering::SeqCst);
    // Persist every transcript message the turn creates (assistant replies,
    // tool results, interrupt close-outs) the moment it is created — the
    // faithful-transcript hook. A no-op when no session is open.
    let mut persist_message = |msg: &Message| {
        if let Some(sid) = session_id {
            let _ = sessions.append(sid, msg);
        }
    };
    let outcome = crate::ui::stream::run_turn_displayed(
        transport,
        tools,
        messages,
        max_iterations,
        skin,
        verbose,
        // Chat mode streams AND renders markdown: ContentFragment appends
        // each finalized line as markdown (append-only, no re-rendering).
        // One-shot runs never stream (see cli.rs).
        transport.supports_streaming(),
        interrupted,
        compression_config,
        // Chat is where a tool may ask the user something mid-turn.
        true,
        Some(&mut persist_message),
    );
    match outcome {
        Ok(crate::core::TurnOutcome {
            answer,
            streamed,
            last_usage,
            total_usage,
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
            let note = empty_answer_note(skin, &answer);
            if !note.is_empty() {
                println!("{note}");
            }
            if let Some(sid) = session_id {
                // The final answer row was already persisted by the
                // `persist_message` hook when the loop pushed it — persisting
                // it again here would duplicate the row.
                // Per-turn token record for /info. Uses the whole turn's
                // summed usage (a tool-using turn makes several provider
                // calls); providers that omit usage record a zero row so the
                // turn still counts.
                let (inp, out) = match total_usage {
                    Some(u) => (u.prompt_tokens, u.completion_tokens),
                    None => (0, 0),
                };
                let _ = sessions.record_turn_usage(
                    sid,
                    &transport.current_model().unwrap_or_else(|| "unknown".into()),
                    inp,
                    out,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                );
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
                        // this turn — the picker title should reflect what the
                        // *session* is about, not just its latest message.
                        // User + assistant text only: tool results are noise
                        // for naming a session and would dominate the transcript.
                        let transcript = sessions
                            .load(sid)
                            .unwrap_or_default()
                            .iter()
                            .filter(|m| {
                                matches!(
                                    m.role,
                                    crate::message::Role::User | crate::message::Role::Assistant
                                ) && !m.content.is_empty()
                            })
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
            // Keep the partial progress in context so the next turn builds
            // on it.
            println!("\n(interrupted — back to prompt)\n");
            None
        }
        Err(e) => {
            eprintln!("error: {e}");
            // Same treatment as a Ctrl-C: the completed iterations
            // (assistant messages, tool results) and the on-disk user row
            // stay put, so the next turn continues from real progress
            // instead of the failed turn evaporating. Rewinding used to
            // discard everything the turn had learned before the failure —
            // with flaky endpoints that meant losing long tool-heavy
            // analyses to a single transient drop.
            None
        }
    }
}



#[cfg(test)]
mod endpoint_and_recency_tests {
    use super::*;

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
    fn recency_labels_read_like_a_humans_clock() {
        let now = 1_000_000_000;
        assert_eq!(recency_label(now, now), "now");
        assert_eq!(recency_label(now - 30, now), "now");
        assert_eq!(recency_label(now - 45 * 60, now), "45m ago");
        assert_eq!(recency_label(now - 3 * 3600, now), "3h ago");
        assert_eq!(recency_label(now - 3 * 3600 - 12 * 60, now), "3h 12m ago");
        assert_eq!(recency_label(now - 2 * 86400 - 3 * 3600, now), "2d 3h ago");
        assert_eq!(recency_label(now - 5 * 86400, now), "5d ago");
        // A future timestamp (clock skew) is clamped to "now", never negative.
        assert_eq!(recency_label(now + 500, now), "now");
    }
}

#[cfg(test)]
mod failed_turn_tests {
    use super::*;
    use crate::message::Role;
    use crate::transport::r#trait::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
    use crate::tools::r#trait::Tool;
    use crate::util::AgentError;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;

    /// First call returns a tool call (real progress), second call dies with
    /// the production mid-stream drop ("error decoding response body").
    struct FailAfterProgress {
        calls: Cell<u32>,
    }

    impl ProviderTransport for FailAfterProgress {
        fn name(&self) -> &str {
            "fail-after-progress"
        }
        fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _model: &str,
        ) -> crate::util::Result<ModelResponse> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            if n == 0 {
                Ok(ModelResponse {
                    content: "Let me look into that.".into(),
                    tool_calls: vec![crate::message::ToolCall::new("call_1", "noop", "{}")],
                    finish_reason: FinishReason::ToolCalls,
                    usage: None,
                })
            } else {
                Err(AgentError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "error decoding response body",
                )))
            }
        }
    }

    struct NoopTool;

    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "does nothing"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn run(&self, _args: &serde_json::Value) -> crate::util::Result<String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn a_failed_turn_keeps_its_progress_and_the_user_row() {
        let path = std::env::temp_dir().join(format!(
            "grace_failed_turn_test_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let sessions = SessionStore::open(&path).unwrap();
        let sid = "s-failturn";

        // This turn installs the mid-turn prompt slot (chat is interactive);
        // hold the slot lock so the slot-asserting tests can't race it.
        let _slot_lock = crate::util::test_support::prompt_slot_guard();
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Box::new(NoopTool));

        let mut messages = vec![Message::system("you are grace")];
        let interrupted = std::sync::Arc::new(AtomicBool::new(false));
        let transport = FailAfterProgress { calls: Cell::new(0) };

        run_one_chat_turn(
            &transport,
            &tools,
            &mut messages,
            8,
            &sessions,
            Some(sid),
            "figure out the cyclic valid scenario",
            None,
            &crate::ui::skin::SOLARIS,
            &interrupted,
            &ContextCompressionConfig::default(),
            false,
        );

        // The in-memory transcript keeps the completed iteration —
        // system, user, assistant (tool call), tool result — instead of
        // rewinding to just before the prompt.
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::System, Role::User, Role::Assistant, Role::Tool],
            "a failed turn must keep its progress in context"
        );

        // And the turn's traffic — the user's prompt, the assistant's tool
        // call, and the tool result — all stay on disk, so a resume sees the
        // work already done instead of re-issuing the call (the old behavior
        // persisted only the prompt).
        let loaded = sessions.load(sid).unwrap();
        assert_eq!(
            loaded.len(),
            3,
            "user + assistant tool-call + tool result: {loaded:?}"
        );
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[0].content, "figure out the cyclic valid scenario");
        assert_eq!(loaded[1].role, Role::Assistant);
        assert_eq!(loaded[1].tool_calls.len(), 1);
        assert_eq!(loaded[1].tool_calls[0].id, "call_1");
        assert_eq!(loaded[2].role, Role::Tool);
        assert_eq!(loaded[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(loaded[2].content, "ok");

        drop(sessions);
        let _ = std::fs::remove_file(&path);
    }

    /// A turn that succeeds after a tool round must persist the full faithful
    /// transcript — user, the assistant's tool call, the tool result, and the
    /// final answer — so a resume continues from real progress.
    #[test]
    fn a_completed_tool_turn_persists_the_full_transcript() {
        let path = std::env::temp_dir().join(format!(
            "grace_completed_tool_turn_test_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let sessions = SessionStore::open(&path).unwrap();
        let sid = "s-toolturn";

        let _slot_lock = crate::util::test_support::prompt_slot_guard();
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Box::new(NoopTool));

        let mut messages = vec![Message::system("you are grace")];
        let interrupted = std::sync::Arc::new(AtomicBool::new(false));
        // First call: a tool call. Second call: the final answer.
        struct TwoRound {
            calls: Cell<u32>,
        }
        impl crate::transport::r#trait::ProviderTransport for TwoRound {
            fn name(&self) -> &str {
                "two-round"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[crate::transport::r#trait::ToolSpec],
                _model: &str,
            ) -> crate::util::Result<crate::transport::r#trait::ModelResponse> {
                let n = self.calls.get();
                self.calls.set(n + 1);
                if n == 0 {
                    Ok(crate::transport::r#trait::ModelResponse {
                        content: String::new(),
                        tool_calls: vec![crate::message::ToolCall::new("c1", "noop", "{}")],
                        finish_reason: crate::transport::r#trait::FinishReason::ToolCalls,
                        usage: None,
                    })
                } else {
                    Ok(crate::transport::r#trait::ModelResponse {
                        content: "All done.".into(),
                        tool_calls: vec![],
                        finish_reason: crate::transport::r#trait::FinishReason::Stop,
                        usage: None,
                    })
                }
            }
        }
        let transport = TwoRound {
            calls: Cell::new(0),
        };

        run_one_chat_turn(
            &transport,
            &tools,
            &mut messages,
            8,
            &sessions,
            Some(sid),
            "run the noop tool",
            None,
            &crate::ui::skin::SOLARIS,
            &interrupted,
            &ContextCompressionConfig::default(),
            false,
        );

        let loaded = sessions.load(sid).unwrap();
        let roles: Vec<Role> = loaded.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant],
            "user, tool-call, tool result, final answer: {loaded:?}"
        );
        assert_eq!(loaded[3].content, "All done.");
        assert_eq!(loaded[2].tool_call_id.as_deref(), Some("c1"));

        drop(sessions);
        let _ = std::fs::remove_file(&path);
    }
}





/// One row per user prompt for the `/jump` picker: the prompt's own words
/// (the auto-recalled block stripped) as the label, its position as the dim
/// sublabel. Tool traffic never appears — "re-send this prompt" is about the
/// user's asks, not the model's tool calls. The renderer wraps and applies
/// its own row budget, so the only cap here is a generous character bound to
/// keep absurd prompts from dominating the window; overflow is marked with `…`.
fn jump_rows(user_rows: &[(i64, String)]) -> Vec<crate::ui::picker::Pick> {
    user_rows
        .iter()
        .enumerate()
        .map(|(i, (_row_id, content))| {
            let words = crate::recall::strip_recall_block(content);
            let flat = words.trim().replace('\n', " ");
            let snippet = if flat.chars().count() > 240 {
                format!("{}…", flat.chars().take(240).collect::<String>())
            } else {
                flat
            };
            crate::ui::picker::Pick::new(i.to_string(), snippet).hint(format!("prompt {}", i + 1))
        })
        .collect()
}

/// `/jump` — go back to an earlier prompt and re-send it. A picker over the
/// current session's user prompts (tool traffic excluded); picking one drops
/// that prompt and everything after it — from both the on-disk session and
/// the in-memory context — then preloads the prompt's own words into the
/// editor so you can re-send it verbatim or tweak it first. The re-sent line
/// is returned for the caller to queue as the next turn (`None` = the user
/// picked nothing, or hit EOF at the edit prompt: the session is untouched).
///
/// `messages[0]` is the system prompt; after a jump the in-memory context is
/// rebuilt from the (now-truncated) on-disk transcript so the two agree, the
/// same way a `/session` switch re-derives context from the store.
fn handle_jump_command(
    sessions: &SessionStore,
    messages: &mut Vec<Message>,
    current_session: &Option<String>,
    reader: &mut LineReader,
    skin: &Skin,
) -> Option<String> {
    let Some(sid) = current_session else {
        println!("no session in use — nothing to jump within.");
        return None;
    };
    let user_rows = match sessions.list_user_rows(sid) {
        Ok(rows) => rows,
        Err(e) => {
            println!("error listing the session: {e}");
            return None;
        }
    };
    if user_rows.is_empty() {
        println!("no messages yet in this session.");
        return None;
    }
    let items = jump_rows(&user_rows);
    let Some(id) = crate::ui::picker::pick(
        &items,
        skin,
        "pick a prompt to re-send (drops it and everything after)",
    ) else {
        println!("staying put.");
        return None;
    };
    let Ok(picked) = id.parse::<usize>() else {
        return None;
    };
    let (row_id, stored) = user_rows[picked].clone();
    // Preload the prompt's own words (the recalled block stripped) so an
    // unedited re-send is one keystroke and an edit starts from clean text.
    // Re-sending verbatim would otherwise stack the stale block under the
    // fresh one the new turn derives.
    let words = crate::recall::strip_recall_block(&stored);
    let Some(edited) = reader.read_line_default("re-send: ", &words) else {
        return None; // EOF (Ctrl-D) — leave the session exactly as it was
    };
    let edited = edited.trim().to_string();
    if edited.is_empty() {
        // `read_line_default` keeps the prefill on an empty submit, so this
        // only fires if the prefill itself was empty — nothing to re-send.
        println!("staying put.");
        return None;
    }
    // Drop the picked prompt and everything after it. The on-disk delete
    // happens first; the in-memory context is then rebuilt from what remains
    // so both agree (the system prompt is kept in place).
    match sessions.truncate_from_row_id(sid, row_id) {
        Ok(removed) => {
            let system = messages.first().cloned();
            let remaining = sessions.load(sid).unwrap_or_default();
            messages.clear();
            if let Some(sys) = system {
                messages.push(sys);
            }
            messages.extend(remaining);
            println!("dropped {removed} message(s) — re-sending from the picked prompt.");
            Some(edited)
        }
        Err(e) => {
            println!("error rewinding the session: {e}");
            None
        }
    }
}

/// `/compact` — compress the context on demand instead of waiting for the
/// automatic trigger. Same model-summarized pass as the automatic path, minus
/// the trigger gate; still refuses a pass that would not strictly help.
fn handle_compact_command(
    messages: &mut Vec<Message>,
    transport: &(dyn crate::transport::ProviderTransport + '_),
    compression_config: &ContextCompressionConfig,
    skin: &Skin,
) {
    if messages
        .iter()
        .all(|m| m.role == crate::message::Role::System)
    {
        println!("nothing to compact yet.");
        return;
    }
    let compressor = crate::core::context::Compressor::for_transport(compression_config, transport);
    match compressor.force_compress_in_place_with_model(messages, transport) {
        Some(res) => {
            let o = &res.outcome;
            let summary_suffix = match res.summary.as_deref() {
                Some(s) if !s.is_empty() => format!("\n  · summary: {s}"),
                _ => String::new(),
            };
            println!(
                "{}",
                skin.paint(
                    Role::ToolDim,
                    &format!(
                        "  · context compacted: {} → {} tok ({} message(s) summarized){summary_suffix}",
                        o.before_tokens, o.after_tokens, o.dropped_messages
                    )
                )
            );
        }
        None => println!("nothing to compact — the context is already minimal."),
    }
}

/// `/dump [FILE]` — serialize the whole context (system prompt included, so
/// the dump is a complete picture of what the model sees). With an argument
/// it writes a file; without one it copies through the OSC52 clipboard
/// escape, which needs no external clipboard binary and works over SSH —
/// but only if the terminal permits it.
fn handle_dump_command(messages: &[Message], arg: &str) {
    if messages.iter().all(|m| m.role == crate::message::Role::System) {
        println!("nothing to dump yet.");
        return;
    }
    let mut text = String::new();
    for m in messages {
        text.push_str(&m.render());
        text.push_str("\n\n");
    }
    if let Some(path) = arg.split_whitespace().next() {
        match std::fs::write(path, &text) {
            Ok(()) => println!("wrote {} bytes to {path}", text.len()),
            Err(e) => println!("could not write {path}: {e}"),
        }
        return;
    }
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        println!("the clipboard needs a TTY — use /dump FILE instead.");
        return;
    }
    // Terminals cap OSC52 payloads (tmux defaults to ~74 KB); beyond that,
    // silently truncating or dropping is worse than pointing at /dump FILE.
    let b64 = crate::util::base64_encode(text.as_bytes());
    if b64.len() > 100_000 {
        println!(
            "context too large for the clipboard escape ({} KB encoded) — use /dump FILE.",
            b64.len() / 1024
        );
        return;
    }
    use std::io::Write as _;
    print!("\x1b]52;c;{b64}\x07");
    let _ = std::io::stdout().flush();
    println!(
        "copied {} chars to the clipboard (OSC52 — nothing appeared if your terminal refuses it).",
        text.chars().count()
    );
}

#[cfg(test)]
mod slash_command_agreement_tests {
    /// The words the dispatch `match` in `run_chat` handles. This list and
    /// the registry are checked against each other in both directions: a
    /// handler without a registry entry is an undiscoverable command (audit
    /// G14), and a registry entry without a handler is a phantom. Update
    /// this constant whenever an arm in the dispatch match changes.
    const DISPATCHED: &[&str] = &[
        "exit", "model", "skin", "session", "jump", "compact", "dump", "rename", "clear",
        "info", "effort", "provider", "verbose", "readonly",
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
mod picker_row_tests {
    use super::*;

    #[test]
    fn slash_command_rows_pair_the_command_with_its_summary() {
        let cmds = crate::ui::commands::resolve_prefix("m");
        assert!(!cmds.is_empty());
        let rows = slash_command_rows(cmds.iter().copied());
        assert_eq!(rows.len(), cmds.len());
        for (row, c) in rows.iter().zip(&cmds) {
            assert_eq!(row.label, format!("/{}", c.name));
            assert_eq!(row.sublabel.as_deref(), Some(c.summary));
            assert!(!row.disabled);
        }
    }

    #[test]
    fn jump_rows_show_user_prompts_only_with_position() {
        // The picker lists the user's prompts (the stored as-sent text),
        // flattening whitespace and labelling each by position.
        let rows = vec![
            (1i64, "  hello\nworld  ".to_string()),
            (2, "second prompt".to_string()),
        ];
        let rows = jump_rows(&rows);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "0");
        assert_eq!(rows[0].label, "hello world");
        assert_eq!(rows[0].sublabel.as_deref(), Some("prompt 1"));
        assert_eq!(rows[1].label, "second prompt");
        assert_eq!(rows[1].sublabel.as_deref(), Some("prompt 2"));
    }

    #[test]
    fn jump_rows_strip_the_recalled_block_from_the_label() {
        // A stored prompt is persisted as sent (prompt + auto-recalled block);
        // the picker must show only the user's words, not the block.
        let as_sent = format!(
            "fix the parser\n\n{marker} (verify before relying on it):\n- [fact] acme: \"x\"\n",
            marker = crate::recall::RECALL_BLOCK_MARKER
        );
        let rows = jump_rows(&[(1, as_sent)]);
        assert_eq!(rows[0].label, "fix the parser");
    }

    #[test]
    fn jump_rows_cap_long_snippets_with_an_ellipsis() {
        let long = "w ".repeat(200);
        let rows = jump_rows(&[(1, long)]);
        assert_eq!(rows[0].label.chars().count(), 241, "240 chars + the ellipsis");
        assert!(rows[0].label.ends_with('…'));
    }

    #[test]
    fn empty_answer_note_is_visible_only_for_a_blank_answer() {
        // The invariant: a turn that produced no text must not be silent. An
        // answer with any content prints nothing extra; a blank or
        // whitespace-only answer gets a visible "why".
        let _lock = crate::util::test_support::env_guard();
        let skin = crate::ui::skin::SOLARIS;
        let plain = |s: String| crate::ui::markdown::strip_ansi(&s).trim().to_string();

        let note = empty_answer_note(&skin, "");
        assert!(
            plain(note).contains("no output"),
            "a blank answer must say so, not be silent"
        );
        assert!(
            !plain(empty_answer_note(&skin, "   \n\t ")).is_empty(),
            "whitespace-only still counts as no output"
        );
        assert!(
            empty_answer_note(&skin, "I did the thing.").is_empty(),
            "a real answer gets no extra note"
        );
    }

    #[test]
    fn dump_writes_the_faithful_transcript_including_tool_traffic() {
        // `/dump FILE` must be a complete picture of what the model saw (commit
        // 1's faithful persistence), not a bare `[role] content` that drops the
        // tool calls and their result linkage.
        let messages = vec![
            Message::system("sys prompt"),
            Message::user("read the file"),
            Message {
                role: crate::message::Role::Assistant,
                content: String::new(),
                tool_calls: vec![crate::message::ToolCall::new(
                    "call_1",
                    "read",
                    "{\"path\":\"/tmp/a.txt\"}",
                )],
                tool_call_id: None,
                name: None,
            },
            Message::tool("call_1", "read", "file text"),
            Message::assistant("here it is"),
        ];
        let path = std::env::temp_dir().join(format!("grace_dump_test_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        handle_dump_command(&messages, path.to_str().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[user] read the file"), "user row: {text}");
        assert!(
            text.contains("→ read({\"path\":\"/tmp/a.txt\"}) #call_1"),
            "the tool call must appear (faithful): {text}"
        );
        assert!(
            text.contains("[tool · read #call_1] file text"),
            "the tool result must keep its linkage: {text}"
        );
        assert!(text.contains("[assistant] here it is"), "final answer: {text}");
        let _ = std::fs::remove_file(&path);
    }
}

