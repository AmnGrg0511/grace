//! Durable memory — facts that survive across process runs.
//!
//! Backed by a single bundled-SQLite file (`~/.grace/memory.db`) rather than a
//! markdown file that has to be re-read and re-parsed every run. Facts are
//! plain rows; nothing consolidates or rewrites them automatically, which is
//! deliberate — memory that mutates itself silently is memory you cannot trust.
//!
//! ```text
//! store.rs   the SQLite side: open, remember, all, forget, export
//! prompt.rs  the model-facing side: as_prompt_block, wikilink resolution
//! ```

pub mod prompt;
pub mod store;

pub use store::{Fact, Memory};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_and_fact_types_are_reachable_from_the_module_root() {
        let dir = std::env::temp_dir().join(format!(
            "grace_memory_mod_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mem = Memory::open(dir.join("memory.db")).unwrap();
        mem.remember("a fact").unwrap();
        let facts: Vec<Fact> = mem.all().unwrap();
        assert_eq!(facts.len(), 1);
        // The prompt-side impl lives in `prompt.rs` but must be callable on
        // the same type, not a separate wrapper.
        assert!(mem.as_prompt_block().unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
