//! The `ask` builtin — the model's way to reach the human mid-turn. The
//! tool is UI-agnostic: it validates the question, hands it to the mid-turn
//! interaction channel (`tools/interaction.rs`), and maps the outcome to
//! tool-result space — the answer verbatim, a decline as a literal marker
//! (a *result*, not an error), and no channel as a readable error.

use crate::tools::interaction::{prompt, PromptResult, UserPrompt};
use crate::tools::r#trait::{arg_str, str_prop, Tool};
use crate::util::{AgentError, Result};
use serde_json::Value;

/// What the model sees when the user declines: a literal, greppable result
/// that is *not* an error — the model decides what a no-answer means (the
/// persona says: proceed with your best judgment and say what you assumed).
pub const DECLINED_MARKER: &str = "[no answer — user declined]";

pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer mid-task. Use it \
         only when a decision is genuinely theirs — a preference, an approval \
         for a consequential or hard-to-reverse action, or information you \
         cannot obtain yourself — never for anything you can determine from \
         the task or your tools. Offer `choices` when the answer is one of a \
         known set. If the user declines, the result is a decline marker: \
         proceed with your best judgment and say what you assumed."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": str_prop("The question for the user, in one or two sentences."),
                "choices": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" },
                    "description": "Optional fixed answers to offer; the user may still type their own."
                },
                "allow_custom": {
                    "type": "boolean",
                    "description": "With choices: also allow a free-text answer (default true). Ignored without choices."
                }
            },
            "required": ["question"]
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let question = arg_str(args, "question")?;
        if question.trim().is_empty() {
            return Err(AgentError::Tool(
                "argument 'question' must not be empty".into(),
            ));
        }
        let choices = match args.get("choices") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) if items.is_empty() => {
                return Err(AgentError::Tool(
                    "argument 'choices' must have at least one entry".into(),
                ));
            }
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let s = item.as_str().ok_or_else(|| {
                        AgentError::Tool("every entry of 'choices' must be a string".into())
                    })?;
                    if s.trim().is_empty() {
                        return Err(AgentError::Tool(
                            "entries of 'choices' must not be empty".into(),
                        ));
                    }
                    out.push(s.to_string());
                }
                Some(out)
            }
            Some(_) => {
                return Err(AgentError::Tool(
                    "argument 'choices' must be an array of strings".into(),
                ))
            }
        };
        let allow_custom = match args.get("allow_custom") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(AgentError::Tool(
                    "argument 'allow_custom' must be a boolean".into(),
                ))
            }
        };

        match prompt(&UserPrompt {
            question,
            choices,
            allow_custom,
        }) {
            PromptResult::Answered(a) => Ok(a.trim().to_string()),
            PromptResult::Declined => Ok(DECLINED_MARKER.to_string()),
            PromptResult::Unavailable => Err(AgentError::Tool(
                "the user cannot be reached right now (no interactive channel — \
                 one-shot mode or no terminal); proceed with your best judgment \
                 instead of asking"
                    .into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::interaction::{install, Prompter, PromptResult};
    use crate::util::test_support::prompt_slot_guard;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Records what it was asked and answers by a fixed rule.
    struct Recording {
        last: Arc<Mutex<Option<UserPrompt>>>,
        answer: PromptResult,
    }

    impl Prompter for Recording {
        fn prompt(&self, req: &UserPrompt) -> PromptResult {
            *self.last.lock().unwrap() = Some(req.clone());
            self.answer.clone()
        }
    }

    fn recording(answer: PromptResult) -> (Recording, Arc<Mutex<Option<UserPrompt>>>) {
        let last: Arc<Mutex<Option<UserPrompt>>> = Arc::new(Mutex::new(None));
        (
            Recording {
                last: last.clone(),
                answer,
            },
            last,
        )
    }

    fn err_text(args: &Value) -> String {
        AskTool.run(args).unwrap_err().to_string()
    }

    // ---- argument validation (no channel involved) -----------------------

    #[test]
    fn a_missing_question_names_the_argument() {
        let e = err_text(&json!({}));
        assert!(e.contains("missing string argument 'question'"), "{e}");
    }

    #[test]
    fn a_non_string_question_is_rejected() {
        let e = err_text(&json!({"question": 42}));
        assert!(e.contains("missing string argument 'question'"), "{e}");
    }

    #[test]
    fn an_empty_or_blank_question_is_rejected() {
        for q in ["", "   "] {
            let e = err_text(&json!({"question": q}));
            assert!(e.contains("must not be empty"), "{e}");
        }
    }

    #[test]
    fn choices_must_be_an_array_of_nonempty_strings() {
        let e = err_text(&json!({"question": "q?", "choices": "yes"}));
        assert!(e.contains("array of strings"), "{e}");
        let e = err_text(&json!({"question": "q?", "choices": []}));
        assert!(e.contains("at least one entry"), "{e}");
        let e = err_text(&json!({"question": "q?", "choices": ["yes", 1]}));
        assert!(e.contains("must be a string"), "{e}");
        let e = err_text(&json!({"question": "q?", "choices": ["yes", "  "]}));
        assert!(e.contains("must not be empty"), "{e}");
    }

    #[test]
    fn allow_custom_must_be_a_boolean() {
        let e = err_text(&json!({"question": "q?", "allow_custom": "yes"}));
        assert!(e.contains("must be a boolean"), "{e}");
    }

    // ---- channel outcomes (slot locked so no other test's prompter leaks)

    #[test]
    fn an_answered_prompt_returns_the_trimmed_answer() {
        let _lock = prompt_slot_guard();
        let (p, _last) = recording(PromptResult::Answered("  ship it  ".into()));
        let _guard = install(Arc::new(p));
        assert_eq!(
            AskTool.run(&json!({"question": "ship?"})).unwrap(),
            "ship it"
        );
    }

    #[test]
    fn a_declined_prompt_returns_the_literal_marker_not_an_error() {
        let _lock = prompt_slot_guard();
        let (p, _last) = recording(PromptResult::Declined);
        let _guard = install(Arc::new(p));
        assert_eq!(
            AskTool.run(&json!({"question": "ship?"})).unwrap(),
            DECLINED_MARKER
        );
    }

    #[test]
    fn no_channel_is_a_readable_error() {
        let _lock = prompt_slot_guard();
        let e = AskTool.run(&json!({"question": "ship?"})).unwrap_err().to_string();
        assert!(e.contains("cannot be reached"), "{e}");
        assert!(e.contains("one-shot"), "{e}");
    }

    #[test]
    fn the_request_reaches_the_channel_with_its_shape() {
        let _lock = prompt_slot_guard();
        let (p, last) = recording(PromptResult::Answered("ok".into()));
        let _guard = install(Arc::new(p));
        AskTool.run(&json!({
            "question": "which one?",
            "choices": ["a", "b"],
            "allow_custom": false
        }))
        .unwrap();
        let req = last.lock().unwrap().take().unwrap();
        assert_eq!(req.question, "which one?");
        assert_eq!(req.choices, Some(vec!["a".into(), "b".into()]));
        assert!(!req.allow_custom, "an explicit false must survive");

        // allow_custom defaults to true when present with choices, and
        // choices stays absent when not given.
        let (p, last) = recording(PromptResult::Answered("ok".into()));
        let _guard = install(Arc::new(p));
        AskTool.run(&json!({"question": "free text?"})).unwrap();
        let req = last.lock().unwrap().take().unwrap();
        assert!(req.choices.is_none());
        assert!(req.allow_custom, "the default must be true");
    }
}
