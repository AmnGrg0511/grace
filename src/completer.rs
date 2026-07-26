//! Rustyline helper — provides `/`-command completion and hints for chat mode.
//!
//! When the user types `/` and hits Tab, rustyline calls our `Completer`
//! which returns matching slash-commands. The `Hinter` shows the same
//! candidates as a dim hint. The `Highlighter` paints the prompt.

use rustyline::completion::Completer;
use rustyline::hint::Hinter;
use rustyline::highlight::Highlighter;
use rustyline::validate::Validator;
use rustyline::Context;
use rustyline::Result;

/// All `/` commands available in chat mode.
const SLASH_COMMANDS: &[&str] = &[
    "/exit",
    "/quit",
    "/model",
    "/skin",
    "/session",
];

/// Rustyline helper that provides `/`-command tab completion and hints.
pub struct CommandHelper;

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
        let candidates: Vec<String> = SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.starts_with(line))
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
        SLASH_COMMANDS
            .iter()
            .find(|cmd| cmd.starts_with(line) && **cmd != line)
            .map(|cmd| cmd[line.len()..].to_string())
    }
}

impl Highlighter for CommandHelper {}

impl Validator for CommandHelper {}

impl rustyline::Helper for CommandHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_slash_commands() {
        let helper = CommandHelper;
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let (start, candidates) = helper.complete("/e", 2, &ctx).unwrap();
        assert_eq!(start, 0);
        assert!(candidates.contains(&"/exit".to_string()));
    }

    #[test]
    fn no_completion_for_non_slash() {
        let helper = CommandHelper;
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let (_, candidates) = helper.complete("hello", 5, &ctx).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn hints_partial_command() {
        let helper = CommandHelper;
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let hint = helper.hint("/se", 3, &ctx);
        assert_eq!(hint, Some("ssion".to_string()));
    }
}
