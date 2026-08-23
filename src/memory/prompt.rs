//! Turning durable facts into prompt text.
//!
//! Memory is only useful if it reaches the model. This module owns that
//! translation — fact rows in, a system-prompt block out — separately from
//! [`super::store`], which owns the SQLite side. Keeping them apart means the
//! injection format can change without touching the schema, and the schema can
//! gain columns without every caller re-learning the prompt shape.

use super::store::Memory;
use crate::util::Result;

impl Memory {
    /// Render all facts as a block suitable for appending to the system
    /// prompt. Returns `None` if there are no facts.
    pub fn as_prompt_block(&self) -> Result<Option<String>> {
        let facts = self.all()?;
        if facts.is_empty() {
            return Ok(None);
        }
        let mut s = String::from("\n\nDurable facts you know about the user/environment:\n");
        for f in &facts {
            s.push_str(&format!("- {}\n", f.content));
        }
        Ok(Some(s))
    }

    /// Mirror all facts to a human-readable `~/.grace/memory.md` (best
    /// effort, non-fatal on I/O error). The SQLite DB stays the source of
    /// truth — this file exists so a fact can be **read** without a SQL
    /// client; editing it does nothing (regenerated every run).
    pub fn export_markdown(&self) -> Result<()> {
        let facts = self.all()?;
        let path = Self::default_markdown_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut s = String::from(
            "# Grace — Durable Memory\n\n\
             Auto-generated from `memory.db` on every run — edits here are \
             NOT persisted. Use `grace --remember \"...\"` to add a fact.\n\n",
        );
        if facts.is_empty() {
            s.push_str("_(no facts yet)_\n");
        } else {
            for f in &facts {
                s.push_str(&format!("- **#{}** {}\n", f.id, f.content));
            }
        }
        let _ = std::fs::write(path, s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> (PathBuf, Memory) {
        let dir = std::env::temp_dir().join(format!(
            "grace_mem_prompt_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mem = Memory::open(dir.join("memory.db")).unwrap();
        (dir, mem)
    }

    #[test]
    fn no_facts_means_no_prompt_block() {
        // An empty block would waste tokens on a header with nothing under it.
        let (dir, mem) = scratch("empty");
        assert!(mem.as_prompt_block().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn facts_render_as_a_labelled_bullet_list() {
        let (dir, mem) = scratch("render");
        mem.remember("user prefers concise answers").unwrap();
        mem.remember("the build uses cargo").unwrap();
        let block = mem.as_prompt_block().unwrap().unwrap();
        assert!(block.contains("Durable facts"));
        assert!(block.contains("- user prefers concise answers"));
        assert!(block.contains("- the build uses cargo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
