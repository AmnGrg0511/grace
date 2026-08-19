//! Single-owner stdin line reading for chat mode.
//!
//! Root cause fixed here: every mid-chat picker (`/session`, `/model`,
//! `/skin`) used to open its own independent `std::io::stdin().lines()`
//! iterator while the outer REPL loop already owned either a
//! `rustyline::Editor` or its own separate stdin iterator. Two independent
//! buffered readers on the same fd race and steal each other's lines
//! non-deterministically — e.g. typing `/session` then `/exit` could have
//! `/exit` silently consumed by the session picker's numeric prompt instead
//! of the outer loop, leaving the REPL to exit on the next real EOF with no
//! "goodbye" message at all. Threading one `LineReader` through the whole
//! interactive session makes stdin ownership unambiguous.

use rustyline::history::History;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

/// The one and only stdin consumer for an interactive chat session.
/// Prefers rustyline (arrow-key history/editing); falls back to plain
/// buffered stdin when rustyline can't attach (piped input, no real tty).
pub enum LineReader {
    Rustyline {
        editor: Box<rustyline::Editor<crate::ui::completer::CommandHelper, rustyline::history::DefaultHistory>>,
        history_path: std::path::PathBuf,
    },
    Plain {
        lines: std::io::Lines<std::io::StdinLock<'static>>,
    },
}

/// The key events for multi-line input in the rustyline editor. Defined as
/// constants (not inline in `new`) so the tests can assert, against the raw
/// bytes the terminal actually emits, that *exactly* these keys are bound to
/// insert (not submit) — i.e. Ctrl+J continues the prompt and plain Enter
/// still submits. See `rustyline`'s `keys` module for the byte→event map.
const CTRL_J: KeyEvent = KeyEvent(KeyCode::Char('J'), Modifiers::CTRL);
const SHIFT_ENTER: KeyEvent = KeyEvent(KeyCode::Enter, Modifiers::SHIFT);
const PLAIN_ENTER: KeyEvent = KeyEvent(KeyCode::Enter, Modifiers::NONE);

impl LineReader {
    pub fn new(history_path: std::path::PathBuf, skin: crate::ui::skin::Skin) -> Self {
        if let Ok(mut editor) = rustyline::Editor::<
            crate::ui::completer::CommandHelper,
            rustyline::history::DefaultHistory,
        >::new()
        {
            editor.set_helper(Some(crate::ui::completer::CommandHelper { skin }));
            // Multi-line input, terminal-side. Ctrl+J (reliable — it sends \n)
            // and Shift+Enter (best-effort: many terminals report it as the
            // same \r as plain Enter) insert a newline instead of submitting.
            // Plain Enter always submits. rustyline's default otherwise maps
            // Enter, Ctrl+J, and Ctrl+M all to AcceptOrInsertLine (submit at
            // end of input), which would make Ctrl+J submit the prompt.
            editor.bind_sequence(CTRL_J, Cmd::Newline);
            editor.bind_sequence(SHIFT_ENTER, Cmd::Newline);
            editor.bind_sequence(PLAIN_ENTER, Cmd::AcceptLine);
            let _ = editor.load_history(&history_path);
            return LineReader::Rustyline {
                editor: Box::new(editor),
                history_path,
            };
        }
        LineReader::Plain {
            lines: std::io::stdin().lines(),
        }
    }

    pub fn is_interactive_editor(&self) -> bool {
        matches!(self, LineReader::Rustyline { .. })
    }

    /// Re-point the live input-line highlighter at a new skin — called
    /// after `/skin` switches mid-chat so subsequently typed lines pick up
    /// the new color instead of staying on the skin active at startup.
    pub fn set_skin(&mut self, skin: crate::ui::skin::Skin) {
        if let LineReader::Rustyline { editor, .. } = self {
            editor.set_helper(Some(crate::ui::completer::CommandHelper { skin }));
        }
    }

    /// Swap the up-arrow history to the given session's own file, called on
    /// every `/session` switch (including `/session new`/`new-persist`).
    ///
    /// Root cause fixed here: history used to live in one global
    /// `history.txt` regardless of which session was active, so up-arrow
    /// after `/session <other>` replayed lines typed in a *different*
    /// conversation — a single shared stack instead of a per-session one.
    /// Each session now gets its own `history_<id>.txt` under the same
    /// directory as the default history file; switching sessions saves the
    /// outgoing file, clears the in-memory ring, and loads the incoming
    /// session's file (or starts empty for a session with no prior history).
    /// A `None` session_id (unpersisted `/session new`/`none`) falls back to
    /// the original shared `history.txt` — there's no session identity to
    /// key on for those.
    pub fn set_history_scope(&mut self, session_id: Option<&str>) {
        let LineReader::Rustyline { editor, history_path } = self else {
            return; // Plain fallback has no history concept.
        };
        let _ = editor.save_history(history_path);
        let new_path = history_file_for(history_path, session_id);
        if new_path == *history_path {
            return; // Already scoped to this session — nothing to swap.
        }
        editor.history_mut().clear().ok();
        let _ = editor.load_history(&new_path);
        *history_path = new_path;
    }

    /// Read one line with the given prompt printed first (rustyline draws
    /// its own prompt glyph; the plain fallback prints it manually since it
    /// bypasses rustyline entirely). Returns `None` on EOF or Ctrl-D/Ctrl-C
    /// interrupt at an idle prompt -- the caller decides what that means
    /// (top-level loop exit vs. a picker's "no selection made").
    #[allow(clippy::doc_markdown)]
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        match self {
            LineReader::Rustyline { editor, history_path } => loop {
                match editor.readline(prompt) {
                    Ok(line) => {
                        let _ = editor.add_history_entry(line.as_str());
                        let _ = editor.save_history(history_path);
                        return Some(line);
                    }
                    // Ctrl-C at an idle prompt: redraw, don't treat as EOF.
                    Err(rustyline::error::ReadlineError::Interrupted) => continue,
                    Err(_) => return None,
                }
            },
            LineReader::Plain { lines } => {
                use std::io::Write;
                print!("{prompt}");
                let _ = std::io::stdout().flush();
                lines.next()?.ok()
            }
        }
    }
}

/// The history file a given session should use, resolved against the
/// directory of `current`.
///
/// Pulled out as a pure function so the per-session scoping rule is testable
/// without a tty, a rustyline editor, or real files.
///
/// A `None` session id (an unpersisted `/session new`/`none`) falls back to
/// the shared `history.txt` — there is no session identity to key on.
pub fn history_file_for(
    current: &std::path::Path,
    session_id: Option<&str>,
) -> std::path::PathBuf {
    let dir = current
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    match session_id {
        Some(sid) => dir.join(format!("history_{sid}.txt")),
        None => dir.join("history.txt"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::Event;
    use std::path::{Path, PathBuf};

    #[test]
    fn soft_newline_key_is_the_raw_newline_byte_and_not_submit() {
        // The terminal emits '\n' (0x0A) for Ctrl+J and '\r' (0x0D) for a
        // plain Enter. We bind Ctrl+J to *insert* and plain Enter to *submit*,
        // so those two raw bytes must normalize to distinct events — and the
        // keys we bind must equal what the bytes actually parse to, else the
        // binding silently never fires. If these matched each other, the
        // soft-newline key would swallow a submit (or the reverse).
        let ctrl_j = Event::from(CTRL_J);
        let from_newline_byte = Event::from(KeyEvent::from('\n'));
        let enter = Event::from(PLAIN_ENTER);
        let from_cr_byte = Event::from(KeyEvent::from('\r'));

        assert_eq!(ctrl_j, from_newline_byte, "bound key != raw Ctrl+J byte");
        assert_eq!(enter, from_cr_byte, "bound key != raw Enter byte");
        assert_ne!(ctrl_j, enter, "soft-newline key must not be the submit key");
    }

    #[test]
    fn each_session_gets_its_own_history_file() {
        // Regression: history used to live in one global history.txt, so
        // up-arrow after `/session other` replayed lines typed in a
        // completely different conversation.
        let current = Path::new("/home/u/.grace/history.txt");
        assert_eq!(
            history_file_for(current, Some("work")),
            PathBuf::from("/home/u/.grace/history_work.txt")
        );
        assert_eq!(
            history_file_for(current, Some("play")),
            PathBuf::from("/home/u/.grace/history_play.txt")
        );
    }

    #[test]
    fn an_unpersisted_session_falls_back_to_the_shared_file() {
        let current = Path::new("/home/u/.grace/history_work.txt");
        assert_eq!(
            history_file_for(current, None),
            PathBuf::from("/home/u/.grace/history.txt")
        );
    }

    #[test]
    fn the_history_file_stays_beside_the_current_one() {
        let current = Path::new("/custom/dir/history.txt");
        assert_eq!(
            history_file_for(current, Some("s1")).parent(),
            Some(Path::new("/custom/dir"))
        );
    }

    #[test]
    fn resolving_the_same_session_twice_is_stable() {
        // `set_history_scope` short-circuits on equality; an unstable result
        // would make it clear and reload history on every single turn.
        let current = Path::new("/home/u/.grace/history_work.txt");
        assert_eq!(
            history_file_for(current, Some("work")),
            PathBuf::from("/home/u/.grace/history_work.txt")
        );
    }

    #[test]
    fn a_bare_filename_with_no_directory_still_resolves() {
        assert_eq!(
            history_file_for(Path::new("history.txt"), Some("s")),
            PathBuf::from("history_s.txt")
        );
    }
}
