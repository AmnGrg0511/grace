//! Turning durable facts into prompt text.
//!
//! Memory is only useful if it reaches the model. This module owns that
//! translation — fact rows in, a system-prompt block out — separately from
//! [`super::store`], which owns the SQLite side. Keeping them apart means the
//! injection format can change without touching the schema, and the schema can
//! gain columns without every caller re-learning the prompt shape.

use super::store::{Fact, Memory};
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

    /// Extract `[[wikilink]]` targets referenced in `content` (one hop, no
    /// vault dependency — just a marker inside a fact's own text).
    pub fn extract_links(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = content;
        while let Some(start) = rest.find("[[") {
            let after = &rest[start + 2..];
            if let Some(end) = after.find("]]") {
                out.push(after[..end].trim().to_string());
                rest = &after[end + 2..];
            } else {
                break;
            }
        }
        out
    }

    /// Resolve one hop of `[[wikilink]]`s inside `content` by keyword match
    /// against other facts' content (case-insensitive substring). Used by
    /// recall to pull in a linked fact without a vault or embeddings.
    pub fn resolve_links(&self, content: &str) -> Result<Vec<Fact>> {
        let links = Self::extract_links(content);
        if links.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.all()?;
        let mut out = Vec::new();
        for link in &links {
            let link_lower = link.to_lowercase();
            for f in &all {
                if f.content.to_lowercase().contains(&link_lower) && f.content != content {
                    out.push(f.clone());
                }
            }
        }
        Ok(out)
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

    #[test]
    fn extract_links_finds_wikilink_targets() {
        let links = Memory::extract_links("see [[acme]] and [[grace core]]");
        assert_eq!(links, vec!["acme", "grace core"]);
    }

    #[test]
    fn extract_links_ignores_an_unterminated_marker() {
        // An unclosed `[[` must not loop forever or panic on the slice.
        assert!(Memory::extract_links("dangling [[oops").is_empty());
    }

    #[test]
    fn extract_links_returns_empty_for_plain_text() {
        assert!(Memory::extract_links("no links here").is_empty());
    }

    #[test]
    fn resolve_links_pulls_in_a_referenced_fact() {
        let (dir, mem) = scratch("resolve");
        mem.remember("acme is the build tool we work on").unwrap();
        mem.remember("today we debug [[acme]]").unwrap();
        let resolved = mem.resolve_links("today we debug [[acme]]").unwrap();
        assert!(resolved.iter().any(|f| f.content.contains("build tool")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_links_does_not_return_the_source_fact_itself() {
        let (dir, mem) = scratch("selfref");
        let text = "the [[topic]] topic note";
        mem.remember(text).unwrap();
        let resolved = mem.resolve_links(text).unwrap();
        assert!(
            resolved.iter().all(|f| f.content != text),
            "a fact must not resolve to itself"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_links_on_unlinked_text_is_empty_without_a_query() {
        let (dir, mem) = scratch("nolinks");
        assert!(mem.resolve_links("plain text").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
