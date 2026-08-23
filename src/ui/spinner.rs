//! The thinking spinner — one transient `{frame} thinking {m:ss}` line that
//! ticks on a background thread in the silent gaps of a turn (from turn start
//! until the first event, and after each tool result until the model answers
//! again). Piped (non-TTY) output never sees it: everything stays append-only
//! there.

use crate::ui::skin::{no_color, reset, Role, Skin};

const SPINNER_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// The thinking spinner: one transient line — `{frame} thinking {m:ss}` —
/// ticking while the turn waits on the model and nothing else is being
/// printed. It shows only in the silent gaps: from turn start until the
/// first event, and after each tool result until the model answers again;
/// [`Spinner::pause`] suppresses it the moment real output flows.
///
/// The line is never newline-terminated, so it always sits exactly on the
/// cursor's line and needs no cursor travel to replace — only column-home +
/// clear-line. Every event write erases it first under the same lock the
/// ticker holds while rendering, so the two can never interleave bytes.
/// Piped (non-TTY) output never sees it at all: everything stays
/// append-only there.
struct SpinnerCore {
    stop: bool,
    /// True while the spinner line is suppressed (real output flowing).
    paused: bool,
    /// True while the cursor sits one line below a live spinner line.
    visible: bool,
    /// True while the cursor sits at column 0 of an empty line. The claim
    /// path may only start there; otherwise it must break the line first.
    at_line_start: bool,
    frame: usize,
    started: std::time::Instant,
    dim: String,
}

pub(crate) struct Spinner {
    core: std::sync::Arc<std::sync::Mutex<SpinnerCore>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Holds the core lock for the duration of one event write, so the ticker
/// thread can never interleave its rewrite inside a multi-line event. Drop
/// is the conservative fallback (treats the write as line-breaking) in case
/// `end_write` is skipped by an early return.
pub(crate) struct SpinnerWriteGuard<'a> {
    core: std::sync::MutexGuard<'a, SpinnerCore>,
}

impl Drop for SpinnerWriteGuard<'_> {
    fn drop(&mut self) {
        // Conservative: an unterminated write is possible, so the next claim
        // breaks the line first. (Normal end_write reports the exact state.)
        self.core.at_line_start = false;
    }
}

impl Spinner {
    const UP: &'static str = "\x1b[F";
    const CLR: &'static str = "\x1b[2K";

    /// Arm the ticker. Without a terminal the thread simply never spawns,
    /// so every method below is a no-op — piped output stays append-only.
    pub(crate) fn start(skin: &Skin) -> Self {
        use std::io::IsTerminal;
        let dim = if no_color() {
            String::new()
        } else {
            skin.style(Role::ToolDim).to_string()
        };
        let core = std::sync::Arc::new(std::sync::Mutex::new(SpinnerCore {
            stop: false,
            paused: false,
            visible: false,
            // The input echo that ended the prompt carried a newline, so the
            // cursor starts on a fresh empty line.
            at_line_start: true,
            frame: 0,
            started: std::time::Instant::now(),
            dim,
        }));
        let thread = if std::io::stdout().is_terminal() {
            let shared = core.clone();
            Some(std::thread::spawn(move || {
            use std::io::Write;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut core = shared.lock().unwrap();
                if core.stop {
                    break;
                }
                if core.paused {
                    continue;
                }
                core.frame = (core.frame + 1) % SPINNER_FRAMES.len();
                let text = Self::render(&core);
                let mut out = std::io::stdout().lock();
                if core.visible {
                    // Redraw in place: the cursor sits on the empty line
                    // BELOW the spinner (the previous write ended \n).
                    let _ = write!(out, "{}{}", Self::UP, Self::CLR);
                } else if !core.at_line_start {
                    // A partial (newline-less) fragment owns the rest of
                    // this line — break it first so the spinner never glues
                    // onto other output.
                    let _ = writeln!(out);
                }
                let _ = writeln!(out, "{text}");
                let _ = out.flush();
                drop(out);
                core.visible = true;
                core.at_line_start = true;
            }
            }))
        } else {
            None
        };
        Self { core, thread }
    }

    fn render(core: &SpinnerCore) -> String {
        let secs = core.started.elapsed().as_secs();
        let time = format!("{}:{:02}", secs / 60, secs % 60);
        let line = format!("{} thinking {time}", SPINNER_FRAMES[core.frame]);
        if core.dim.is_empty() {
            line
        } else {
            format!("{}{line}{}", core.dim, reset())
        }
    }

    /// Suppress the spinner and erase it now: real output is about to land.
    pub(crate) fn pause(&self) {
        use std::io::Write;
        let mut core = self.core.lock().unwrap();
        core.paused = true;
        if core.visible {
            core.visible = false;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{}{}", Self::UP, Self::CLR);
            let _ = out.flush();
            core.at_line_start = true;
        }
    }

    /// Allow the ticker to claim the line again (a tool result just printed;
    /// the model is about to think once more).
    pub(crate) fn resume(&self) {
        self.core.lock().unwrap().paused = false;
    }

    /// Begin an event write: erases the spinner (when visible) so the event
    /// lands on a clean line, and blocks the ticker for the write's
    /// duration. Call [`end_write`](Spinner::end_write) when done.
    pub(crate) fn begin_write(&self) -> SpinnerWriteGuard<'_> {
        use std::io::Write;
        let mut core = self.core.lock().unwrap();
        if !core.paused && core.visible {
            core.visible = false;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{}{}", Self::UP, Self::CLR);
            let _ = out.flush();
        }
        SpinnerWriteGuard { core }
    }

    /// Report how the guarded event write left the cursor: at column 0 of an
    /// empty line, or mid-line (a raw fragment that was not newline-
    /// terminated). The next claim uses this to decide about the break.
    pub(crate) fn end_write(&self, mut guard: SpinnerWriteGuard<'_>, at_line_start: bool) {
        guard.core.at_line_start = at_line_start;
    }

    /// Stop the thread and clear the spinner so the turn's closing output
    /// starts on a clean line.
    pub(crate) fn finish(&mut self) {
        self.pause();
        {
            let mut core = self.core.lock().unwrap();
            core.stop = true;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // A turn that returns early (interrupt, error) must not leak the
        // ticker thread or leave a half-line on screen.
        self.finish();
    }
}

/// Whether one event's terminal output leaves the cursor at the start of an
/// empty line. Everything except the raw (non-colored) stream fragment
/// newline-terminates its output; a raw fragment may end mid-line.
pub(crate) fn event_ends_at_line_start(
    event: &crate::core::lifecycle::AgentEvent<'_>,
    disable_color: bool,
) -> bool {
    match event {
        crate::core::lifecycle::AgentEvent::ContentFragment(f) if disable_color => {
            f.is_empty() || f.ends_with('\n')
        }
        _ => true,
    }
}

#[cfg(test)]
mod spinner_tests {
    use super::*;

    #[test]
    fn spinner_renders_frame_label_and_elapsed() {
        let core = SpinnerCore {
            stop: false,
            paused: false,
            visible: false,
            at_line_start: true,
            frame: 3,
            started: std::time::Instant::now(),
            dim: String::new(),
        };
        let line = Spinner::render(&core);
        assert!(
            line.starts_with("⠸ thinking 0:00"),
            "unexpected line: {line}"
        );
    }
}
