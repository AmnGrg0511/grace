//! The UI side of the mid-turn interaction channel: renders a
//! [`UserPrompt`] on the terminal — the shared picker for choices, one
//! interruptible raw-mode line for free text — and hands the outcome back
//! as a [`PromptResult`].
//!
//! The raw-TTY gate: a mid-turn prompt that can't reach a real terminal
//! reports [`PromptResult::Unavailable`] instead of degrading to the
//! numbered `pick_plain` menu — that fallback belongs to the between-turn
//! pickers, where a pipe is a legitimate (if unglamorous) user. Mid-turn,
//! the pipe would be answered by whatever it holds, not the human.

use crate::tools::interaction::{PromptResult, Prompter, UserPrompt};
use crate::ui::picker::{pick, wrap_words, Pick};
use crate::ui::skin::{Role, Skin};
use crate::ui::spinner::SpinnerHandle;
use std::io::{self, IsTerminal, Write};

/// The terminal implementation of the mid-turn channel: owns the question
/// line, the interaction, and the outcome line for one prompt. The spinner
/// is parked for the prompt's duration — the picker and the line read own
/// the screen in raw mode, and the ticker must not paint through them.
pub(crate) struct TurnPrompter {
    skin: Skin,
    spinner: SpinnerHandle,
}

impl TurnPrompter {
    pub(crate) fn new(skin: Skin, spinner: SpinnerHandle) -> Self {
        TurnPrompter { skin, spinner }
    }
}

impl Prompter for TurnPrompter {
    fn prompt(&self, req: &UserPrompt) -> PromptResult {
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return PromptResult::Unavailable;
        }
        self.spinner.pause();
        let result = self.ask(req);
        self.spinner.resume();
        result
    }
}

impl TurnPrompter {
    /// The whole exchange: `? question`, the picker or the line, then the
    /// `→ answer` / `→ (declined)` line.
    fn ask(&self, req: &UserPrompt) -> PromptResult {
        let mut out = io::stdout();
        let _ = out.flush();
        self.print_question(&mut out, req);
        let answer = match req.choices.as_deref().unwrap_or_default() {
            [] => self.read_line(&mut out),
            choices => {
                let mut items: Vec<Pick> =
                    choices.iter().map(|c| Pick::new(c.clone(), c.clone())).collect();
                if req.allow_custom {
                    items.push(Pick::new("\u{1}custom", "type your own answer"));
                }
                match pick(&items, &self.skin, "pick an answer") {
                    None => None, // Esc / Ctrl-C / EOF — declined
                    Some(c) if c == "\u{1}custom" => self.read_line(&mut out),
                    Some(c) => Some(c),
                }
            }
        };
        self.print_outcome(&mut out, answer.as_deref());
        answer.map(PromptResult::Answered).unwrap_or(PromptResult::Declined)
    }

    /// The question in the prompt's own accent voice: the `?` glyph in the
    /// prompt role, the text stripped of any model-authored escapes and
    /// wrapped to the terminal width.
    fn print_question(&self, out: &mut dyn Write, req: &UserPrompt) {
        let question = crate::ui::markdown::strip_ansi(&req.question);
        let width = crate::ui::markdown::terminal_width().unwrap_or(100);
        let budget = width.saturating_sub(4).max(20);
        let glyph = self.skin.paint(Role::Prompt, "?");
        for (i, line) in wrap_words(&question, budget).into_iter().enumerate() {
            if i == 0 {
                let _ = writeln!(out, "{} {}", glyph, self.skin.paint(Role::Answer, &line));
            } else {
                let _ = writeln!(out, "   {line}");
            }
        }
        let _ = out.flush();
    }

    /// The free-text answer: one raw-mode line with manual echo. Enter
    /// confirms (empty declines); Esc / Ctrl-C / Ctrl-D decline — a mid-turn
    /// prompt must always have an out that costs the turn nothing.
    fn read_line(&self, out: &mut dyn Write) -> Option<String> {
        use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

        let _ = writeln!(out, "  {}", self.skin.paint(Role::ToolDim, "›"));
        let _ = out.flush();
        let answer = match enable_raw_mode() {
            Err(_) => None,
            Ok(()) => {
                let mut chars: Vec<char> = Vec::new();
                let answer = loop {
                    match event::read() {
                        Err(_) => break None,
                        Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                            if k.modifiers.contains(KeyModifiers::CONTROL) {
                                match k.code {
                                    KeyCode::Char('c') | KeyCode::Char('d') => break None,
                                    _ => {}
                                }
                            }
                            match k.code {
                                KeyCode::Enter => break Some(chars.iter().collect::<String>()),
                                KeyCode::Esc => break None,
                                KeyCode::Backspace => {
                                    if chars.pop().is_some() {
                                        let _ = write!(out, "\x08 \x08");
                                    }
                                }
                                KeyCode::Char(c) if !c.is_control() => {
                                    chars.push(c);
                                    let _ = write!(out, "{c}");
                                }
                                _ => {}
                            }
                            let _ = out.flush();
                        }
                        Ok(_) => {}
                    }
                };
                let _ = disable_raw_mode();
                // Close the input line whether or not anything was typed.
                let _ = writeln!(out);
                answer
            }
        };
        answer.and_then(|a| {
            let a = a.trim().to_string();
            if a.is_empty() {
                None
            } else {
                Some(a)
            }
        })
    }

    /// The outcome line under the exchange: the answer (or a bare
    /// "(declined)") in the tool's quieter voice.
    fn print_outcome(&self, out: &mut dyn Write, answer: Option<&str>) {
        let line = match answer {
            Some(a) => self.skin.paint(Role::ToolDim, &format!("  → {a}")),
            None => self.skin.paint(Role::ToolDim, "  → (declined)"),
        };
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_support::prompt_slot_guard;

    #[test]
    fn without_a_tty_the_turn_prompter_is_unavailable_not_a_numbered_menu() {
        // Tests run without a PTY: the raw-TTY gate must report
        // Unavailable (the tool turns that into an error) rather than
        // falling back to the between-turn pick_plain list.
        let _lock = prompt_slot_guard();
        let prompter = TurnPrompter::new(
            crate::ui::skin::SOLARIS,
            crate::ui::spinner::Spinner::start(&crate::ui::skin::SOLARIS).handle(),
        );
        assert_eq!(
            Prompter::prompt(&prompter, &UserPrompt::free_text("anything?")),
            PromptResult::Unavailable
        );
    }
}
