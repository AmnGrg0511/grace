//! Rustyline helper — provides `/`-command completion and hints for chat mode.
//!
//! When the user types `/` and hits Tab, rustyline calls our `Completer`
//! which returns matching slash-commands. The `Hinter` shows the same
//! candidates as a dim hint. The `Highlighter` paints the whole typed line
//! in the active skin's prompt color — this is deliberate, not a side
//! effect of `Skin::paint`'s reset fix: before that fix, `paint()` left an
//! unclosed color escape after printing the prompt glyph, which happened to
//! bleed into whatever the user typed next (accidentally orange in
//! Solaris). Closing that leak (correct, since it was also bleeding into
//! unrelated output like the `/model` provider list) killed the typed-text
//! color as a side effect, since nothing was ever coloring it on purpose.
//! This `Highlighter` is that on-purpose replacement.

use crate::ui::skin::{Role, Skin};
use rustyline::completion::Completer;
use rustyline::hint::Hinter;
use rustyline::highlight::Highlighter;
use rustyline::validate::Validator;
use rustyline::Context;
use rustyline::Result;
use std::borrow::Cow;

/// Rustyline helper that provides `/`-command tab completion, hints, and
/// skin-colored input-line highlighting. The candidate list comes from the
/// single registry in `crate::ui::commands` — not a second local copy that
/// could drift from what actually dispatches.
pub struct CommandHelper {
    pub skin: Skin,
}

impl Completer for CommandHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        _pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Self::Candidate>)> {
        // Only complete when the line starts with '/'
        if !line.starts_with('/') {
            return Ok((0, Vec::new()));
        }
        let candidates: Vec<String> = crate::ui::commands::completion_candidates(line)
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        // Return start position = 0 (replace the whole line with the match)
        Ok((0, candidates))
    }
}

impl Hinter for CommandHelper {
    type Hint = String;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<Self::Hint> {
        if !line.starts_with('/') || line.len() <= 1 {
            return None;
        }
        // Show the first matching command as a hint (minus what's already typed)
        crate::ui::commands::completion_candidates(line)
            .iter()
            .find(|cmd| **cmd != line)
            .map(|cmd| cmd[line.len()..].to_string())
    }
}

impl Highlighter for CommandHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            return Cow::Borrowed(line);
        }
        Cow::Owned(self.skin.paint(Role::Prompt, line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        // Re-run `highlight` on every keystroke, not just at submit — this
        // is what makes typed characters appear colored as you type rather
        // than only after pressing Enter.
        true
    }
}

impl Validator for CommandHelper {}

impl rustyline::Helper for CommandHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_slash_commands() {
        let helper = CommandHelper { skin: crate::ui::skin::SOLARIS };
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let (start, candidates) = helper.complete("/e", 2, &ctx).unwrap();
        assert_eq!(start, 0);
        assert!(candidates.contains(&"/exit".to_string()));
    }

    #[test]
    fn no_completion_for_non_slash() {
        let helper = CommandHelper { skin: crate::ui::skin::SOLARIS };
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let (_, candidates) = helper.complete("hello", 5, &ctx).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn hints_partial_command() {
        let helper = CommandHelper { skin: crate::ui::skin::SOLARIS };
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let hint = helper.hint("/se", 3, &ctx);
        assert_eq!(hint, Some("ssion".to_string()));
    }

    #[test]
    fn help_and_commands_are_completable() {
        // Regression (G14): both dispatch and document themselves, yet were
        // missing from completion because the completer kept its own list.
        let helper = CommandHelper { skin: crate::ui::skin::SOLARIS };
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let (_, h) = helper.complete("/h", 2, &ctx).unwrap();
        assert!(h.contains(&"/help".to_string()));
        let (_, c) = helper.complete("/c", 2, &ctx).unwrap();
        assert!(c.contains(&"/commands".to_string()));
    }
}
