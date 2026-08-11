//! Skills — reusable procedures loaded on demand.
//!
//! A skill is a directory `skills/<name>/SKILL.md` with an optional
//! `description:` frontmatter line. No vault, no index, no embedding step: the
//! filesystem *is* the convention.
//!
//! ```text
//! store.rs     SkillStore: discovery, frontmatter parsing, loading
//! load.rs      the list_skills / load_skill tools
//! defaults.rs  grace-agent, memory-update, skill-author seeded on first run
//! ```

pub mod defaults;
pub mod load;
pub mod store;

pub use defaults::{default_root, ensure_default_skills};
pub use load::{ListSkillsTool, LoadSkillTool};
pub use store::{SkillMeta, SkillStore};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_tools_and_defaults_are_reachable_from_the_module_root() {
        let dir = std::env::temp_dir().join(format!(
            "grace_skill_mod_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        defaults::seed_into(&dir);

        let store = std::sync::Arc::new(SkillStore::new(&dir));
        let meta: Vec<SkillMeta> = store.list_meta();
        assert_eq!(meta.len(), 3);

        // The tools must accept the same store type the CLI builds.
        let _list = ListSkillsTool {
            store: std::sync::Arc::clone(&store),
        };
        let _load = LoadSkillTool { store };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_root_helper_is_reachable() {
        assert!(default_root().ends_with(".grace/skills"));
        let _: fn() -> std::path::PathBuf = ensure_default_skills;
    }
}
