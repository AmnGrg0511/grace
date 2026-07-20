//! Pre-flight recall — the fix for "it didn't know what it already knew".
//!
//! Before the first LLM call each turn, this does a cheap, deterministic,
//! zero-cost (no embedding API call) keyword-overlap pass across three
//! sources Grace already has locally:
//!   - durable facts (memory.rs)
//!   - skill descriptions (skill.rs, via frontmatter `description:` or the
//!     skill name as a fallback)
//!   - past session turns (session.rs, via SQLite FTS5)
//!
//! and injects the top hits into the system prompt automatically, instead of
//! requiring the user to manually say "look at this skill" / "check this
//! file". This is deliberately *not* semantic search: it's free, instant,
//! and auditable — good enough to catch "the user asked about X and we have
//! a fact/skill that mentions X" without an extra network round-trip. An
//! opt-in `--semantic` mode (embedding-based) can layer on top later without
//! changing this path.

use crate::memory::Memory;
use crate::session::SessionStore;
use crate::skill::SkillStore;

/// One recalled candidate worth surfacing to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub kind: &'static str, // "fact" | "skill" | "session"
    pub label: String,
    pub snippet: String,
}

/// Tokenize into lowercase alphanumeric words, dropping short stopword-ish
/// tokens. Intentionally simple — this is keyword overlap, not NLP.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_string())
        .collect()
}

/// Score `candidate` against `query_tokens` by fraction of query tokens
/// present in the candidate (simple, deterministic, no external deps).
fn overlap_score(query_tokens: &[String], candidate: &str) -> f32 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let candidate_lower = candidate.to_lowercase();
    let hits = query_tokens.iter().filter(|t| candidate_lower.contains(t.as_str())).count();
    hits as f32 / query_tokens.len() as f32
}

/// Run the pre-flight recall pass for `user_prompt`, returning up to
/// `limit` total hits across facts + skills + sessions, ranked by overlap
/// score (highest first), score > 0 only.
pub fn recall(
    user_prompt: &str,
    memory: &Memory,
    skills: &SkillStore,
    sessions: Option<&SessionStore>,
    limit: usize,
) -> Vec<RecallHit> {
    let tokens = tokenize(user_prompt);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(f32, RecallHit)> = Vec::new();

    if let Ok(facts) = memory.all() {
        for f in facts {
            let score = overlap_score(&tokens, &f.content);
            if score > 0.0 {
                scored.push((
                    score,
                    RecallHit { kind: "fact", label: format!("fact#{}", f.id), snippet: f.content },
                ));
            }
        }
    }

    for meta in skills.list_meta() {
        let score = overlap_score(&tokens, &meta.description).max(overlap_score(&tokens, &meta.name));
        if score > 0.0 {
            scored.push((
                score,
                RecallHit {
                    kind: "skill",
                    label: meta.name.clone(),
                    snippet: meta.description,
                },
            ));
        }
    }

    if let Some(store) = sessions {
        // FTS5 needs at least one real token; use the longest token as the
        // query to keep this a single cheap lookup rather than one query per
        // token.
        if let Some(best_token) = tokens.iter().max_by_key(|t| t.len()) {
            if let Ok(hits) = store.search(best_token, 5) {
                for (session_id, content) in hits {
                    let score = overlap_score(&tokens, &content);
                    if score > 0.0 {
                        scored.push((
                            score,
                            RecallHit { kind: "session", label: session_id, snippet: content },
                        ));
                    }
                }
            }
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(limit).map(|(_, hit)| hit).collect()
}

/// Render recall hits as a block to prepend/append to the system prompt.
/// Returns `None` if there are no hits (so callers don't inject empty noise).
pub fn as_prompt_block(hits: &[RecallHit]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut s = String::from("\n\nPossibly relevant, recalled automatically before this turn (verify before relying on it):\n");
    for h in hits {
        s.push_str(&format!("- [{}] {}: {}\n", h.kind, h.label, h.snippet));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("grace_recall_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        (base.join("memory.db"), base.join("skills"), base.join("sessions.db"))
    }

    #[test]
    fn recalls_matching_fact_and_skill_but_not_unrelated_ones() {
        let (mem_path, skills_dir, _) = scratch_paths("basic");
        let memory = Memory::open(&mem_path).unwrap();
        memory.remember("user works on PowerPro QoR regression triage").unwrap();
        memory.remember("user prefers concise answers").unwrap();

        std::fs::create_dir_all(skills_dir.join("powerpro-regold")).unwrap();
        std::fs::write(
            skills_dir.join("powerpro-regold").join("SKILL.md"),
            "---\ndescription: Attribute and regold PowerPro QoR regression failures\n---\n# body",
        )
        .unwrap();
        std::fs::create_dir_all(skills_dir.join("unrelated")).unwrap();
        std::fs::write(
            skills_dir.join("unrelated").join("SKILL.md"),
            "---\ndescription: Bake a cake\n---\n# body",
        )
        .unwrap();
        let skills = SkillStore::new(&skills_dir);

        let hits = recall("help me triage a PowerPro QoR regression failure", &memory, &skills, None, 10);

        assert!(hits.iter().any(|h| h.kind == "fact" && h.snippet.contains("QoR regression")));
        assert!(hits.iter().any(|h| h.kind == "skill" && h.label == "powerpro-regold"));
        assert!(!hits.iter().any(|h| h.label == "unrelated"));
        assert!(!hits.iter().any(|h| h.snippet.contains("concise answers")));

        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn empty_prompt_yields_no_hits() {
        let (mem_path, skills_dir, _) = scratch_paths("empty");
        let memory = Memory::open(&mem_path).unwrap();
        memory.remember("something").unwrap();
        let skills = SkillStore::new(&skills_dir);
        let hits = recall("", &memory, &skills, None, 10);
        assert!(hits.is_empty());
        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn prompt_block_is_none_for_empty_hits() {
        assert!(as_prompt_block(&[]).is_none());
    }
}
