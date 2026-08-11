//! Skill loading — reusable procedures on demand, no vault required.
//!
//! A "skill" is just a directory `skills/<name>/SKILL.md`: a plain markdown
//! file the model can load into context via the `load_skill` tool when a task
//! matches. No frontmatter parser, no vault indexing yet — the smallest thing
//! that gives the agent "I can look up a known procedure" behavior.

use crate::util::{AgentError, Result};
use std::path::{Path, PathBuf};

/// A skill's metadata: name plus an optional one-line description parsed
/// from an optional frontmatter block at the top of SKILL.md:
/// ```md
/// ---
/// description: Reviews a pending Perforce changelist for correctness.
/// ---
/// # Perforce CL Review
/// ...
/// ```
/// Skills without frontmatter still work (description falls back to the
/// name) — this is additive, not a breaking convention change.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
}

/// Parse an optional `---\nkey: value\n---` frontmatter block from the top
/// of a SKILL.md body. Returns the `description` field if present.
fn parse_description(content: &str) -> Option<String> {
    let content = content.trim_start();
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Where skills are read from.
pub struct SkillStore {
    root: PathBuf,
}

impl SkillStore {
    /// `root` is the directory containing one subdirectory per skill.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Default location: `./skills` relative to the current working directory.
    pub fn default_root() -> PathBuf {
        PathBuf::from("skills")
    }

    /// List available skill names (directories under root containing a
    /// SKILL.md), sorted alphabetically.
    pub fn list(&self) -> Vec<String> {
        self.list_meta().into_iter().map(|m| m.name).collect()
    }

    /// List available skills with their descriptions (frontmatter
    /// `description:` if present, else the skill name), sorted by name.
    /// This is what recall matches against — the thing that fixes "it
    /// didn't know what skill to look for" without requiring the model to
    /// load every SKILL.md speculatively.
    pub fn list_meta(&self) -> Vec<SkillMeta> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let skill_md = path.join("SKILL.md");
            if path.is_dir() && skill_md.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let description = std::fs::read_to_string(&skill_md)
                        .ok()
                        .and_then(|c| parse_description(&c))
                        .unwrap_or_else(|| name.to_string());
                    out.push(SkillMeta {
                        name: name.to_string(),
                        description,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Load a skill's SKILL.md content by name.
    pub fn load(&self, name: &str) -> Result<String> {
        if name.is_empty() || name.contains(['/', '\\', '.']) {
            return Err(AgentError::Tool(format!("invalid skill name '{name}'")));
        }
        let path: PathBuf = Path::new(&self.root).join(name).join("SKILL.md");
        std::fs::read_to_string(&path)
            .map_err(|e| AgentError::Tool(format!("load skill '{name}' ({}): {e}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn scratch_skills_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("grace_skill_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("greet")).unwrap();
        std::fs::write(dir.join("greet").join("SKILL.md"), "# Greet\nSay hello.").unwrap();
        dir
    }

    #[test]
    fn list_and_load_roundtrip() {
        let dir = scratch_skills_dir("roundtrip");
        let store = std::sync::Arc::new(SkillStore::new(&dir));
        assert_eq!(store.list(), vec!["greet".to_string()]);

        let content = store.load("greet").unwrap();
        assert!(content.contains("Say hello."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_path_traversal() {
        let dir = scratch_skills_dir("traversal");
        let store = SkillStore::new(&dir);
        assert!(store.load("../etc").is_err());
        assert!(store.load("a/b").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frontmatter_description_is_parsed_when_present() {
        let dir = std::env::temp_dir().join(format!(
            "grace_skill_fm_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("review")).unwrap();
        std::fs::write(
            dir.join("review").join("SKILL.md"),
            "---\ndescription: Reviews a pending changelist.\n---\n# Review\n",
        )
        .unwrap();
        let store = SkillStore::new(&dir);
        let meta = store.list_meta();
        assert_eq!(meta[0].description, "Reviews a pending changelist.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skill_without_frontmatter_falls_back_to_its_name() {
        // Frontmatter is additive; skills predating it must keep working.
        let dir = scratch_skills_dir("nofm");
        let meta = SkillStore::new(&dir).list_meta();
        assert_eq!(meta[0].description, "greet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_skills_root_lists_empty_rather_than_erroring() {
        // First run, before any skill has been seeded.
        let store = SkillStore::new("/nonexistent/skills/root");
        assert!(store.list().is_empty());
    }

    #[test]
    fn directories_without_a_skill_md_are_ignored() {
        let dir = scratch_skills_dir("notaskill");
        std::fs::create_dir_all(dir.join("random_dir")).unwrap();
        assert_eq!(SkillStore::new(&dir).list(), vec!["greet".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skills_are_listed_in_a_stable_sorted_order() {
        let dir = scratch_skills_dir("sorted");
        for name in ["zebra", "apple"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("SKILL.md"), "x").unwrap();
        }
        assert_eq!(SkillStore::new(&dir).list(), vec!["apple", "greet", "zebra"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_an_unknown_skill_is_an_error_naming_it() {
        let dir = scratch_skills_dir("unknown");
        let err = SkillStore::new(&dir).load("nope").unwrap_err();
        assert!(err.to_string().contains("nope"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_skill_name_is_rejected() {
        let dir = scratch_skills_dir("emptyname");
        assert!(SkillStore::new(&dir).load("").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
