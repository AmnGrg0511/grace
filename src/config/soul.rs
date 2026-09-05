//! Grace's persona — the default system prompt and the user-editable
//! `~/.grace/soul.md` that overrides it.
//!
//! The persona lives in a file rather than only in the binary so it is
//! something you can open and edit, not a string you have to recompile to
//! change. The in-binary default is the fallback, and is written out on first
//! run so the file always exists to be edited.

/// Default system identity. Grace is a calm, composed, capable agent. This is
/// seeded into every conversation unless overridden by `soul.md` or `--system`.
pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are Grace — a calm, composed, and capable AI agent. You address the user as \
\"Sir\". You are precise, warm but restrained, and you do real work via your tools \
(bash, read, write, edit) rather than only talking about it. \
When a task needs a tool, call it. Keep responses concise and purposeful.\n\
\n\
Skills: Use list_skills to discover available skills, then load_skill to load one \
when a task matches. Three default skills ship with Grace:\n\
- grace-agent: your own architecture and conventions\n\
- memory-update: when to persist a durable fact and how\n\
- skill-author: when and how to create a new skill\n\
\n\
Delegation: For a large, self-contained subtask that would flood this \
conversation with noise (searching a codebase, summarizing many files, a long \
build-and-fix loop), use the delegate tool. The sub-agent runs with its own \
iteration budget and cannot see this conversation, so state everything it needs \
in the task description. Do not delegate work you can finish in a step or two — \
 the round-trip costs more than doing it.\n\
\n\
Asking: When a decision is genuinely the user's — a preference, an approval \
for a consequential or hard-to-reverse action, or information you cannot \
obtain yourself — use the ask tool and wait for their answer instead of \
guessing. Never ask what you can determine from the task or your tools, and \
never ask the same thing twice: if the answer comes back declined, proceed \
with your best judgment and say what you assumed.\n\
\n\
Auto-identify: After completing a complex task (5+ tool calls, errors overcome, \
a reusable workflow), proactively load the skill-author skill and offer to save \
the approach. When the user states a stable preference or correction, proactively \
load the memory-update skill and offer to persist it. Do not ask to create skills \
or update memory for trivial tasks.";

/// Path to the user-editable persona file: `~/.grace/soul.md`.
pub fn soul_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("soul.md")
}

/// Load the persona from `soul.md`, creating it with the default if missing.
///
/// I/O errors fall back to the in-binary default rather than propagating: a
/// filesystem hiccup should never leave the agent with no identity at all.
pub fn load_soul() -> String {
    load_soul_from(&soul_path())
}

/// [`load_soul`] against an explicit path — the seam that makes this testable
/// without writing into the real `~/.grace`.
pub fn load_soul_from(path: &std::path::Path) -> String {
    if let Ok(text) = std::fs::read_to_string(path) {
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, DEFAULT_SYSTEM_PROMPT);
    DEFAULT_SYSTEM_PROMPT.to_string()
}

/// Environment facts the agent should know, appended as a block to the
/// system prompt so it can reason about the machine it runs on (paths to
/// edit, what OS it is on, what model it is). Derived once at prompt-build
/// time; a model/provider switch re-probes via the same struct.
#[derive(Default)]
pub struct SystemEnv {
    /// The process's current working directory.
    pub cwd: Option<String>,
    /// Target OS triple (e.g. "Linux").
    pub os: Option<String>,
    /// CPU architecture (e.g. "x86_64").
    pub arch: Option<String>,
    /// The OS username, if determinable.
    pub user: Option<String>,
    /// The user's home directory, if determinable.
    pub home: Option<String>,
    /// The git branch of the repository under `cwd`, when one is present.
    pub git_branch: Option<String>,
    /// The model id the session is running under (or the reason effort
    /// belongs to).
    pub model: Option<String>,
    /// The model's context window in tokens, when known. Always the cached
    /// or static-table value — the prompt must never trigger a network probe
    /// to report this.
    pub context_window: Option<u32>,
    /// The effective reasoning-effort hint, when active.
    pub reasoning: Option<String>,
}

impl SystemEnv {
    /// Probe the machine and repo for the fields that don't depend on the
    /// active transport: `cwd`, `os`, `arch`, `user`, `home`, and — when
    /// `cwd` is a git worktree — `git_branch`. Model-dependent fields
    /// (`model`, `reasoning`) are filled in by the caller.
    pub fn probe() -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string());
        let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string());
        Self {
            cwd,
            os: Some(std::env::consts::OS.to_string()),
            arch: Some(std::env::consts::ARCH.to_string()),
            user: std::env::var("USER").ok().filter(|s| !s.is_empty()),
            home,
            git_branch: git_branch(),
            ..Default::default()
        }
    }

    /// Render as a prompt block, or `None` when every interesting field is
    /// empty (nothing worth telling the model). Model/window/reasoning are
    /// only shown when set — "unknown" noise is worse than silence.
    pub fn as_prompt_block(&self) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        if let Some(cwd) = &self.cwd {
            lines.push(format!("- cwd: `{cwd}`"));
        }
        if let (Some(os), Some(arch)) = (&self.os, &self.arch) {
            lines.push(format!("- machine: {os} / {arch}"));
        } else if let Some(os) = &self.os {
            lines.push(format!("- machine: {os}"));
        }
        if let Some(user) = &self.user {
            lines.push(format!("- user: `{user}`"));
        }
        if let Some(home) = &self.home {
            lines.push(format!("- home: `{home}`"));
        }
        if let Some(branch) = &self.git_branch {
            lines.push(format!("- git branch: `{branch}`"));
        }
        if let Some(model) = &self.model {
            lines.push(format!("- model: {model}"));
        }
        if let Some(window) = self.context_window {
            lines.push(format!(
                "- context window: ~{} tokens",
                crate::ui::status::human_tokens(window as u64)
            ));
        }
        if let Some(reasoning) = &self.reasoning {
            lines.push(format!("- reasoning effort: {reasoning}"));
        }
        if lines.is_empty() {
            return None;
        }
        let mut out = String::from("\n\nEnvironment (read-only context for this session):\n");
        out.push_str(&lines.join("\n"));
        out.push('\n');
        Some(out)
    }
}

/// Current UTC date as `YYYY-MM-DD`, computed with std-only civil-date math
/// (no `time` crate). Folded into the persona so the agent can reason about
/// "today" without a separate clock tool.
fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_date_utc(secs)
}

/// `YYYY-MM-DD` for `secs` since the Unix epoch, via Howard Hinnant's
/// days-from-civil algorithm. Pure, so the civil math is testable against
/// fixed epoch anchors without the clock interfering.
fn civil_date_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The branch of the git repository containing `cwd`, if any. Runs
/// `git rev-parse --abbrev-ref HEAD` and returns the name; a non-repo dir,
/// a detached head, or a missing `git` all yield `None` quietly.
fn git_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Assemble the full system prompt — the single assembly path shared by
/// first launch, `/session` switches, and one-shot `--prompt` runs.
///
/// An explicit `override_prompt` (the `--system` flag) replaces the soul
/// file entirely; without it, the soul file is the persona. Durable memory
/// facts are always appended after, and `query`, when given, triggers the
/// pre-flight recall against it (facts/skills/sessions overlapping its
/// keywords). `env`, when given, appends an environment context block so the
/// agent knows where it is and what it is running as.
///
/// Errors are returned, never swallowed: a caller that silently drops them
/// would build a prompt that quietly loses the user's remembered facts.
pub fn build_system_prompt(
    override_prompt: Option<&str>,
    memory: &crate::memory::Memory,
    skills: &crate::skill::SkillStore,
    sessions: &crate::session::SessionStore,
    query: Option<&str>,
    env: Option<&SystemEnv>,
) -> crate::util::Result<String> {
    let mut sp = override_prompt.map(str::to_string).unwrap_or_else(load_soul);

    sp.push_str(&format!("\n\nToday is {} (UTC).", utc_date()));

    if let Some(block) = memory.as_prompt_block()? {
        sp.push_str(&block);
    }

    if let Some(query) = query {
        let hits = crate::recall::recall(query, memory, skills, Some(sessions), 5);
        if let Some(block) = crate::recall::as_prompt_block(&hits) {
            sp.push_str(&block);
        }
    }

    if let Some(env) = env {
        if let Some(block) = env.as_prompt_block() {
            sp.push_str(&block);
        }
    }
    Ok(sp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "grace_soul_test_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_missing_soul_file_is_created_with_the_default() {
        let dir = scratch("create");
        let path = dir.join("soul.md");
        let loaded = load_soul_from(&path);
        assert_eq!(loaded, DEFAULT_SYSTEM_PROMPT);
        assert!(path.exists(), "the file must be written so it can be edited");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_soul_file_overrides_the_default() {
        let dir = scratch("override");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("soul.md");
        std::fs::write(&path, "You are a laconic agent.").unwrap();
        assert_eq!(load_soul_from(&path), "You are a laconic agent.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_soul_file_falls_back_to_the_default() {
        // An accidentally-emptied soul.md must not produce an agent with no
        // system prompt at all.
        let dir = scratch("blank");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("soul.md");
        std::fs::write(&path, "   \n\n").unwrap();
        assert_eq!(load_soul_from(&path), DEFAULT_SYSTEM_PROMPT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_path_still_returns_a_usable_persona() {
        // Filesystem failure must degrade to the in-binary default, not panic.
        let loaded = load_soul_from(std::path::Path::new("/proc/nonexistent/soul.md"));
        assert_eq!(loaded, DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn the_default_prompt_documents_the_tools_the_agent_actually_has() {
        for tool in ["bash", "read", "write", "edit", "ask"] {
            assert!(
                DEFAULT_SYSTEM_PROMPT.contains(tool),
                "persona should mention {tool}"
            );
        }
    }

    #[test]
    fn the_default_prompt_explains_when_to_delegate() {
        // A delegate tool the model never reaches for is dead weight; the
        // persona has to say when it is worth the round-trip.
        assert!(DEFAULT_SYSTEM_PROMPT.contains("delegate"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("iteration budget"));
    }

    #[test]
    fn the_default_prompt_explains_when_to_ask() {
        // An ask tool with no guidance either never fires (the model guesses)
        // or nags (the model asks about everything). The persona has to draw
        // the line — and the decline path, or a no-answer dead-ends the turn.
        assert!(DEFAULT_SYSTEM_PROMPT.contains("ask tool"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("genuinely the user's"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("declined"));
    }

    #[test]
    fn soul_path_lives_under_the_dot_grace_directory() {
        let p = soul_path();
        assert!(p.ends_with(".grace/soul.md"), "got {}", p.display());
    }

    #[test]
    fn build_system_prompt_is_the_single_assembly_path() {
        // Regression (G1/G2): `/session` used to derive its own copy of the
        // system prompt — dropping the `--system` override, skipping
        // pre-flight recall, and swallowing memory read errors via
        // `if let Ok(...)`. Session switches must go through this one
        // builder so startup and switch re-derivation agree.
        let dir = scratch("build_prompt");
        let memory = crate::memory::Memory::open(dir.join("memory.db")).unwrap();
        memory.remember("the user prefers rust").unwrap();
        let skills = crate::skill::SkillStore::new(dir.join("skills"));
        let sessions = crate::session::SessionStore::open(dir.join("sessions.db")).unwrap();

        // The override takes the persona slot entirely …
        let sp =
            build_system_prompt(Some("you are a test persona"), &memory, &skills, &sessions, None, None)
                .unwrap();
        assert!(
            sp.starts_with("you are a test persona"),
            "the --system override must win: {sp:?}"
        );
        // … with durable facts appended on top …
        assert!(sp.contains("prefers rust"), "memory block must be present: {sp:?}");

        // A query triggers pre-flight recall — what a switch into a session
        // feeds (the target session's first user message).
        let sp = build_system_prompt(None, &memory, &skills, &sessions, Some("rust preferences"), None)
            .unwrap();
        assert!(
            sp.contains("recalled automatically"),
            "recall block expected for an overlapping query: {sp:?}"
        );

        // Without a query there is nothing to recall against — no empty
        // recall noise.
        let sp = build_system_prompt(None, &memory, &skills, &sessions, None, None).unwrap();
        assert!(!sp.contains("recalled automatically"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_memory_db_fails_loudly_not_silently() {
        // The old /session copy swallowed this case: an unreadable facts
        // database must surface as an error the caller prints, not as a
        // prompt that quietly lost every remembered fact.
        let dir = scratch("corrupt_memory");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("memory.db");
        std::fs::write(&db, b"this is not a sqlite database").unwrap();
        match crate::memory::Memory::open(&db) {
            Err(e) => assert!(e.to_string().contains("memory"), "got: {e}"),
            Ok(_) => panic!("a corrupt facts database must not open"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_env_block_renders_each_set_field_in_order() {
        let env = SystemEnv {
            cwd: Some("/work".into()),
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            user: Some("dev".into()),
            home: Some("/home/dev".into()),
            git_branch: Some("main".into()),
            model: Some("gpt-4o".into()),
            context_window: Some(128_000),
            reasoning: Some("medium".into()),
        };
        let block = env.as_prompt_block().unwrap();
        assert!(block.contains("cwd: `/work`"), "{block}");
        assert!(block.contains("machine: linux / x86_64"), "{block}");
        assert!(block.contains("user: `dev`"), "{block}");
        assert!(block.contains("home: `/home/dev`"), "{block}");
        assert!(block.contains("git branch: `main`"), "{block}");
        assert!(block.contains("model: gpt-4o"), "{block}");
        assert!(block.contains("context window"), "{block}");
        assert!(block.contains("reasoning effort: medium"), "{block}");
    }

    #[test]
    fn an_empty_env_block_renders_nothing() {
        let env = SystemEnv::default();
        assert!(env.as_prompt_block().is_none());
    }

    #[test]
    fn the_env_block_is_appended_after_memory_and_recall() {
        let dir = scratch("env_block");
        let memory = crate::memory::Memory::open(dir.join("memory.db")).unwrap();
        memory.remember("the user prefers rust").unwrap();
        let skills = crate::skill::SkillStore::new(dir.join("skills"));
        let sessions = crate::session::SessionStore::open(dir.join("sessions.db")).unwrap();
        let env = SystemEnv {
            cwd: Some("/work".into()),
            model: Some("gpt-4o".into()),
            ..SystemEnv::default()
        };

        let sp = build_system_prompt(None, &memory, &skills, &sessions, None, Some(&env)).unwrap();
        assert!(
            sp.find("prefers rust").unwrap() < sp.find("cwd: `/work`").unwrap(),
            "env must come after memory: {sp}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn civil_date_math_matches_fixed_epoch_anchors() {
        // Known-good conversions, the strongest oracle available for
        // hand-rolled calendar math.
        assert_eq!(civil_date_utc(0), "1970-01-01");
        assert_eq!(civil_date_utc(946_684_800), "2000-01-01");
        assert_eq!(civil_date_utc(1_709_222_400), "2024-02-29"); // leap day
        assert_eq!(civil_date_utc(1_778_198_400), "2026-05-08"); // mid-year
        assert_eq!(civil_date_utc(86_399), "1970-01-01"); // just before midnight
        assert_eq!(civil_date_utc(86_400), "1970-01-02");
        // Negative (pre-epoch) dates still convert sensibly.
        assert_eq!(civil_date_utc(-1), "1969-12-31");
        assert_eq!(civil_date_utc(-86_400), "1969-12-31");
    }

    #[test]
    fn utc_date_is_a_well_formed_iso_date_in_a_plausible_range() {
        let date = utc_date();
        assert_eq!(date.len(), 10, "got {date:?}");
        assert_eq!(&date[4..5], "-", "got {date:?}");
        assert_eq!(&date[7..8], "-", "got {date:?}");
        let year: i64 = date[0..4].parse().expect("year is numeric");
        assert!((2000..=2100).contains(&year), "got {date:?}");
        let month: u32 = date[5..7].parse().expect("month is numeric");
        let day: u32 = date[8..10].parse().expect("day is numeric");
        assert!((1..=12).contains(&month), "got {date:?}");
        assert!((1..=31).contains(&day), "got {date:?}");
    }
}
