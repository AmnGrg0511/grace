//! Session titles.
//!
//! A session id is meaningless to a human, and "the first user message" is
//! almost always "hi". A short model-generated title is what makes the
//! `/session` picker legible — "debugging the stdin race" instead of `s-a1b2`.
//!
//! Regenerated as a session grows (see the retitle schedule in the chat REPL)
//! so a long conversation does not stay frozen under its opening greeting.

use crate::message::Message;
use crate::transport::ProviderTransport;

/// Ask the model for a short (3-6 word) title summarizing a conversation
/// transcript so far. Deliberately tiny: no tools, no system prompt, one
/// cheap round-trip — this is what replaces "the id is just whatever the
/// user's first message was" (almost always "hi") with an actual
/// description of what the chat is about. Called repeatedly as a session
/// grows (see `run_one_chat_turn`'s retitle schedule) so long sessions
/// don't freeze on their opening "hi".
///
/// Best-effort: any transport error just means no title this time; the
/// picker falls back to the previous title (or the raw session id).
pub fn generate_title(
    transport: &dyn ProviderTransport,
    model: &str,
    transcript: &str,
) -> Option<String> {
    let prompt = format!(
        "Write a specific 3-6 word title for this conversation, distinct \
         enough that it wouldn't be confused with a different conversation. \
         Base it on the concrete topic, question, or task the user raised — \
         if the exchange so far is just a greeting with no real topic yet \
         (e.g. \"hi\" / \"hello\"), title it using the user's exact opening \
         words instead of a generic summary like \"assistant greets user\". \
         No punctuation, no quotes, plain text only — just the title:\n\n{transcript}"
    );
    let messages = [Message::user(prompt)];
    let resp = transport.complete(&messages, &[], model).ok()?;
    let title = resp.content.trim().trim_matches('"').to_string();
    if title.is_empty() {
        None
    } else {
        // Defensively cap length — a misbehaving model ignoring the
        // word-count instruction shouldn't be able to blow up the picker's
        // layout.
        Some(title.chars().take(60).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FinishReason, ModelResponse, ToolSpec};
    use crate::util::{AgentError, Result};

    struct Fixed(&'static str);
    impl ProviderTransport for Fixed {
        fn name(&self) -> &str {
            "fixed"
        }
        fn complete(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: self.0.to_string(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }
    }

    struct Broken;
    impl ProviderTransport for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        fn complete(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            Err(AgentError::Transport("down".into()))
        }
    }

    #[test]
    fn returns_the_models_title() {
        let t = generate_title(&Fixed("debugging the stdin race"), "m", "...");
        assert_eq!(t.as_deref(), Some("debugging the stdin race"));
    }

    #[test]
    fn surrounding_quotes_are_stripped() {
        let t = generate_title(&Fixed("\"quoted title\""), "m", "...");
        assert_eq!(t.as_deref(), Some("quoted title"));
    }

    #[test]
    fn whitespace_is_trimmed() {
        let t = generate_title(&Fixed("  spaced out  "), "m", "...");
        assert_eq!(t.as_deref(), Some("spaced out"));
    }

    #[test]
    fn an_empty_response_yields_no_title_rather_than_a_blank_one() {
        assert!(generate_title(&Fixed("   "), "m", "...").is_none());
    }

    #[test]
    fn a_transport_failure_is_not_fatal() {
        // Titling is cosmetic; a provider hiccup must never break the chat.
        assert!(generate_title(&Broken, "m", "...").is_none());
    }

    #[test]
    fn an_overlong_title_is_capped_so_it_cannot_break_the_picker_layout() {
        let long: &'static str = Box::leak("x".repeat(500).into_boxed_str());
        let t = generate_title(&Fixed(long), "m", "...").unwrap();
        assert!(t.chars().count() <= 60, "got {} chars", t.chars().count());
    }
}
