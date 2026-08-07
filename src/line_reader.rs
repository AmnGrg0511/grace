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

/// The one and only stdin consumer for an interactive chat session.
/// Prefers rustyline (arrow-key history/editing); falls back to plain
/// buffered stdin when rustyline can't attach (piped input, no real tty).
pub(crate) enum LineReader {
    Rustyline {
        editor: Box<rustyline::Editor<grace::completer::CommandHelper, rustyline::history::DefaultHistory>>,
        history_path: std::path::PathBuf,
    },
    Plain {
        lines: std::io::Lines<std::io::StdinLock<'static>>,
    },
}

impl LineReader {
    pub(crate) fn new(history_path: std::path::PathBuf, skin: grace::skin::Skin) -> Self {
        if let Ok(mut editor) = rustyline::Editor::<
            grace::completer::CommandHelper,
            rustyline::history::DefaultHistory,
        >::new()
        {
            editor.set_helper(Some(grace::completer::CommandHelper { skin }));
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

    pub(crate) fn is_interactive_editor(&self) -> bool {
        matches!(self, LineReader::Rustyline { .. })
    }

    /// Re-point the live input-line highlighter at a new skin — called
    /// after `/skin` switches mid-chat so subsequently typed lines pick up
    /// the new color instead of staying on the skin active at startup.
    pub(crate) fn set_skin(&mut self, skin: grace::skin::Skin) {
        if let LineReader::Rustyline { editor, .. } = self {
            editor.set_helper(Some(grace::completer::CommandHelper { skin }));
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
    pub(crate) fn set_history_scope(&mut self, session_id: Option<&str>) {
        let LineReader::Rustyline { editor, history_path } = self else {
            return; // Plain fallback has no history concept.
        };
        let _ = editor.save_history(history_path);
        let dir = history_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let new_path = match session_id {
            Some(sid) => dir.join(format!("history_{sid}.txt")),
            None => dir.join("history.txt"),
        };
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
    pub(crate) fn read_line(&mut self, prompt: &str) -> Option<String> {
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
