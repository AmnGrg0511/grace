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
        for tool in ["bash", "read", "write", "edit"] {
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
    fn soul_path_lives_under_the_dot_grace_directory() {
        let p = soul_path();
        assert!(p.ends_with(".grace/soul.md"), "got {}", p.display());
    }
}
