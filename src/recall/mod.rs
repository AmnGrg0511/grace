//! Pre-flight recall — surfacing relevant prior context before the turn runs.
//!
//! Deterministic and free: keyword overlap against durable facts, skill
//! descriptions, and the session FTS index. No embedding call, no vector store,
//! no extra latency. The point is that the user should not have to say "look at
//! the X skill" or "remember what we decided about Y" — if the prompt overlaps
//! with something already known, it gets injected.

#[allow(clippy::module_inception)]
pub mod recall;

pub use recall::{
    as_prompt_block, recall, strip_recall_block, RECALL_BLOCK_MARKER, RecallHit,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recall_api_is_reachable_from_the_module_root() {
        let hits: Vec<RecallHit> = Vec::new();
        assert!(as_prompt_block(&hits).is_none());
        let _: fn(
            &str,
            &crate::memory::Memory,
            &crate::skill::SkillStore,
            Option<&crate::session::SessionStore>,
            usize,
        ) -> Vec<RecallHit> = recall;
        let _: fn(&str) -> String = strip_recall_block;
    }

    #[test]
    fn strip_recall_block_drops_the_block_and_keeps_the_words() {
        // A stored user row is persisted as sent: the prompt plus the
        // auto-recalled block. Stripping must return just the prompt.
        let as_sent = format!(
            "fix the parser\n\n{marker} (verify before relying on it):\n- [fact] acme: \"x\"\n",
            marker = RECALL_BLOCK_MARKER
        );
        assert_eq!(strip_recall_block(&as_sent), "fix the parser");
        // A prompt with no block (fresh input, or pre-recall history) is untouched.
        assert_eq!(strip_recall_block("just a prompt"), "just a prompt");
        assert_eq!(strip_recall_block(""), "");
    }
}
