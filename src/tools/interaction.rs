//! The mid-turn user-interaction channel — how a tool asks the human a
//! question while a turn is in flight and gets the answer back as data.
//!
//! The tools side is UI-agnostic: it declares *what* to ask
//! ([`UserPrompt`]) and consumes the outcome ([`PromptResult`]). The UI side
//! ([`crate::ui::interaction`]) installs a [`Prompter`] for the duration of
//! one displayed turn — the only thing that may touch the terminal is that
//! installer. A turn that installed nothing (one-shot mode, no TTY) reports
//! [`PromptResult::Unavailable`], and the calling tool turns that into a
//! readable error rather than silently dropping the question.

use std::sync::{Arc, Mutex, OnceLock};

/// What a tool wants to ask the user, and in what shape the answer may come
/// back.
#[derive(Debug, Clone)]
pub struct UserPrompt {
    /// The question, shown verbatim to the user.
    pub question: String,
    /// Fixed choices to offer, when the tool wants one of a known set.
    /// `None` (or empty) asks for free text.
    pub choices: Option<Vec<String>>,
    /// With choices: also offer a free-text answer. Without choices it is
    /// ignored — free text is the only mode then.
    pub allow_custom: bool,
}

impl UserPrompt {
    /// A free-text question.
    pub fn free_text(question: impl Into<String>) -> Self {
        UserPrompt {
            question: question.into(),
            choices: None,
            allow_custom: true,
        }
    }

    /// A question over a fixed set of choices; `allow_custom` additionally
    /// offers a free-text answer.
    pub fn choices(question: impl Into<String>, choices: Vec<String>, allow_custom: bool) -> Self {
        UserPrompt {
            question: question.into(),
            choices: Some(choices),
            allow_custom,
        }
    }
}

/// The user's answer to a [`UserPrompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptResult {
    /// The user answered; the trimmed answer text.
    Answered(String),
    /// The user declined (Esc / Ctrl-C / Ctrl-D / empty answer). A valid
    /// outcome, not an error — the tool decides what a no-answer means.
    Declined,
    /// No interactive channel is available (one-shot mode, no TTY). The tool
    /// must fail with a readable error.
    Unavailable,
}

/// Anything that can carry a [`UserPrompt`] to the human and back.
pub trait Prompter: Send + Sync {
    fn prompt(&self, req: &UserPrompt) -> PromptResult;
}

/// The one process-wide channel slot. A displayed turn installs the UI
/// prompter for its duration; sub-agent turns run nested on the same thread
/// and inherit it, while one-shot runs install nothing.
static SLOT: OnceLock<Mutex<Option<Arc<dyn Prompter>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<dyn Prompter>>> {
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Installs `p` as the channel for the current turn and restores whatever
/// was there before on drop — a failed or interrupted turn never leaks its
/// prompter into the next one.
pub(crate) fn install(p: Arc<dyn Prompter>) -> PromptSlotGuard {
    PromptSlotGuard {
        previous: slot().lock().unwrap().replace(p),
    }
}

pub(crate) struct PromptSlotGuard {
    previous: Option<Arc<dyn Prompter>>,
}

impl Drop for PromptSlotGuard {
    fn drop(&mut self) {
        *slot().lock().unwrap() = self.previous.take();
    }
}

/// Ask the user through whatever prompter the current turn installed.
/// Without one this is [`PromptResult::Unavailable`] — the channel is a
/// capability the turn grants, never a tool's assumption.
pub fn prompt(req: &UserPrompt) -> PromptResult {
    // Clone the handle out of the lock: the prompter blocks on the human,
    // and nothing may run under the slot lock while it does.
    match slot().lock().unwrap().clone() {
        Some(p) => p.prompt(req),
        None => PromptResult::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_support::prompt_slot_guard;

    /// A prompter that records what it was asked and answers with a fixed
    /// value.
    struct Fixed(String);
    impl Prompter for Fixed {
        fn prompt(&self, _req: &UserPrompt) -> PromptResult {
            PromptResult::Answered(self.0.clone())
        }
    }

    struct Declining;
    impl Prompter for Declining {
        fn prompt(&self, _req: &UserPrompt) -> PromptResult {
            PromptResult::Declined
        }
    }

    #[test]
    fn a_prompt_with_no_installed_prompter_is_unavailable() {
        // The slot lock makes sure no other test's prompter is live while
        // this one asserts on the empty slot.
        let _lock = prompt_slot_guard();
        assert_eq!(prompt(&UserPrompt::free_text("anything?")), PromptResult::Unavailable);
    }

    #[test]
    fn an_installed_prompter_answers_and_the_guard_restores_on_drop() {
        let _lock = prompt_slot_guard();
        let result;
        {
            let _guard = install(Arc::new(Fixed("yes".into())));
            result = prompt(&UserPrompt::free_text("proceed?"));
        }
        assert_eq!(result, PromptResult::Answered("yes".into()));
        // Guard dropped: the slot is empty again.
        assert!(
            slot().lock().unwrap().is_none(),
            "the guard must restore the previous (empty) slot"
        );
    }

    #[test]
    fn a_declining_prompter_yields_declined_not_an_error() {
        let _lock = prompt_slot_guard();
        let _guard = install(Arc::new(Declining));
        assert_eq!(
            prompt(&UserPrompt::choices("pick", vec!["a".into(), "b".into()], false)),
            PromptResult::Declined
        );
    }
}
