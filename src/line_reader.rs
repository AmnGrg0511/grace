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
    pub(crate) fn new(history_path: std::path::PathBuf) -> Self {
        if let Ok(mut editor) = rustyline::Editor::<
            grace::completer::CommandHelper,
            rustyline::history::DefaultHistory,
        >::new()
        {
            editor.set_helper(Some(grace::completer::CommandHelper));
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
