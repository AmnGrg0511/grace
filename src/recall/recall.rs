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

/// Words too common to carry any signal. Without this list, *every* stored
/// fact matches *every* prompt: "what is the capital of Peru" shares "the"
/// with "the deployment pipeline runs on jenkins", which was enough to score
/// above zero and inject a completely irrelevant fact into the prompt.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "her", "was", "one", "our",
    "out", "day", "get", "has", "him", "his", "how", "its", "new", "now", "old", "see", "two",
    "who", "did", "yes", "let", "put", "say", "she", "too", "use", "that", "with", "this", "from",
    "they", "have", "what", "were", "when", "your", "said", "each", "which", "their", "will",
    "about", "would", "there", "could", "other", "into", "than", "then", "them", "these", "some",
    "does", "just", "like", "make", "over", "such", "only", "also", "back", "even", "want", "way",
];

/// Tokenize into lowercase alphanumeric words, dropping very short tokens and
/// stopwords. Intentionally simple — this is keyword overlap, not NLP.
fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(w))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Minimum overlap before a candidate is considered relevant at all.
///
/// A single incidental word in common is not a reason to spend context on a
/// fact. Requiring a real fraction of the query to match is what keeps recall
/// from injecting noise into every prompt.
const MIN_SCORE: f32 = 0.25;

/// Score `candidate` against `query_tokens` by the fraction of query tokens
/// that appear in it as *whole words*.
///
/// Whole-word matching matters: plain substring containment made "per" match
/// inside "operator" and "ram" inside "parameter", so unrelated facts scored
/// as hits. Deterministic, no external deps, no embedding call.
fn overlap_score(query_tokens: &[String], candidate: &str) -> f32 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let candidate_words: std::collections::HashSet<String> = candidate
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(std::string::ToString::to_string)
        .collect();
    let hits = query_tokens
        .iter()
        .filter(|t| candidate_words.contains(t.as_str()))
        .count();
    hits as f32 / query_tokens.len() as f32
}

/// Run the pre-flight recall pass for `user_prompt`, returning up to
/// `limit` total hits across facts + skills + sessions, ranked by overlap
/// score (highest first), keeping only those at or above [`MIN_SCORE`].
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
            if score >= MIN_SCORE {
                scored.push((
                    score,
                    RecallHit {
                        kind: "fact",
                        label: format!("fact#{}", f.id),
                        snippet: f.content,
                    },
                ));
            }
        }
    }

    for meta in skills.list_meta() {
        let score =
            overlap_score(&tokens, &meta.description).max(overlap_score(&tokens, &meta.name));
        if score >= MIN_SCORE {
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
                    if score >= MIN_SCORE {
                        scored.push((
                            score,
                            RecallHit {
                                kind: "session",
                                label: session_id,
                                snippet: content,
                            },
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
        let base =
            std::env::temp_dir().join(format!("grace_recall_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        (
            base.join("memory.db"),
            base.join("skills"),
            base.join("sessions.db"),
        )
    }

    #[test]
    fn recalls_matching_fact_and_skill_but_not_unrelated_ones() {
        let (mem_path, skills_dir, _) = scratch_paths("basic");
        let memory = Memory::open(&mem_path).unwrap();
        memory
            .remember("user works on acme regression triage")
            .unwrap();
        memory.remember("user prefers concise answers").unwrap();

        std::fs::create_dir_all(skills_dir.join("acme-triage")).unwrap();
        std::fs::write(
            skills_dir.join("acme-triage").join("SKILL.md"),
            "---\ndescription: Triage and attribute acme regression failures\n---\n# body",
        )
        .unwrap();
        std::fs::create_dir_all(skills_dir.join("unrelated")).unwrap();
        std::fs::write(
            skills_dir.join("unrelated").join("SKILL.md"),
            "---\ndescription: Bake a cake\n---\n# body",
        )
        .unwrap();
        let skills = SkillStore::new(&skills_dir);

        let hits = recall(
            "help me triage an acme regression failure",
            &memory,
            &skills,
            None,
            10,
        );

        assert!(hits
            .iter()
            .any(|h| h.kind == "fact" && h.snippet.contains("regression triage")));
        assert!(hits
            .iter()
            .any(|h| h.kind == "skill" && h.label == "acme-triage"));
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

    #[test]
    fn stopwords_alone_never_produce_a_hit() {
        // Regression: substring overlap on common words made *every* stored
        // fact score above zero for *every* prompt — "what is the capital of
        // Peru" matched "the deployment pipeline runs on jenkins" via "the",
        // and an irrelevant fact got injected into the prompt.
        let (mem_path, skills_dir, _) = scratch_paths("stopwords");
        let memory = Memory::open(&mem_path).unwrap();
        memory
            .remember("the deployment pipeline runs on jenkins")
            .unwrap();
        let skills = SkillStore::new(&skills_dir);

        let hits = recall("what is the capital of Peru", &memory, &skills, None, 10);
        assert!(hits.is_empty(), "expected no hits, got {hits:?}");
        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn matching_is_whole_word_not_substring() {
        // "per" must not match inside "operator"; substring containment made
        // unrelated facts score as hits.
        let tokens = tokenize("per ram");
        assert_eq!(overlap_score(&tokens, "the operator parameter"), 0.0);
        assert_eq!(overlap_score(&tokens, "per ram values"), 1.0);
    }

    #[test]
    fn tokenize_drops_stopwords_and_short_words() {
        let tokens = tokenize("the quick and a fox");
        assert_eq!(tokens, vec!["quick", "fox"]);
    }

    #[test]
    fn a_single_incidental_word_falls_below_the_relevance_floor() {
        // One word in common out of many is not worth spending context on.
        let (mem_path, skills_dir, _) = scratch_paths("floor");
        let memory = Memory::open(&mem_path).unwrap();
        memory.remember("pipeline notes about jenkins").unwrap();
        let skills = SkillStore::new(&skills_dir);

        let hits = recall(
            "explain rust lifetimes ownership borrowing traits pipeline",
            &memory,
            &skills,
            None,
            10,
        );
        assert!(hits.is_empty(), "1/6 overlap should not qualify: {hits:?}");
        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn hits_are_ranked_with_the_strongest_match_first() {
        let (mem_path, skills_dir, _) = scratch_paths("ranking");
        let memory = Memory::open(&mem_path).unwrap();
        memory.remember("jenkins pipeline deployment staging").unwrap();
        memory.remember("jenkins notes").unwrap();
        let skills = SkillStore::new(&skills_dir);

        let hits = recall(
            "jenkins pipeline deployment staging",
            &memory,
            &skills,
            None,
            10,
        );
        assert!(hits.len() >= 2);
        assert!(
            hits[0].snippet.contains("staging"),
            "the fuller match should rank first, got {hits:?}"
        );
        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn the_limit_caps_how_much_context_recall_can_inject() {
        let (mem_path, skills_dir, _) = scratch_paths("limit");
        let memory = Memory::open(&mem_path).unwrap();
        for i in 0..10 {
            memory
                .remember(&format!("jenkins pipeline note number {i}"))
                .unwrap();
        }
        let skills = SkillStore::new(&skills_dir);
        let hits = recall("jenkins pipeline note", &memory, &skills, None, 3);
        assert_eq!(hits.len(), 3);
        let _ = std::fs::remove_dir_all(mem_path.parent().unwrap());
    }

    #[test]
    fn a_prompt_block_labels_each_hit_by_kind() {
        let hits = vec![
            RecallHit {
                kind: "fact",
                label: "fact#1".into(),
                snippet: "a durable fact".into(),
            },
            RecallHit {
                kind: "skill",
                label: "greet".into(),
                snippet: "says hello".into(),
            },
        ];
        let block = as_prompt_block(&hits).unwrap();
        assert!(block.contains("fact#1"));
        assert!(block.contains("greet"));
    }
}
