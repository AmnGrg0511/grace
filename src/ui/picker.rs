//! One dropdown primitive for every "pick one of these" moment — slash-command
//! palette, `/model`, `/forget`, `/jump`, … — so muscle memory and keyboard
//! behavior are identical everywhere.
//!
//! Keyboard: type to filter (case-insensitive substring over label or
//! sublabel), ↑/↓ to move (wraps around the filtered list), Home/End to the
//! ends, Enter to select, Esc or Ctrl-C to cancel. The list renders as a
//! window of at most [`WINDOW`] rows centered on the selection, so any number
//! of items works on any terminal.
//!
//! This primitive redraws its own bounded region (cursor-up + re-print) — it
//! is NOT part of the answer stream path, whose append-only invariant forbids
//! cursor movement; the picker runs between turns, when the stream is idle.

use crate::ui::skin::Skin;
use std::io::{self, IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

// The picker redraws its own bounded region between turns, so it may move the
// cursor — the answer *stream* path is append-only and never does. These are
// the raw SGR/CUP sequences for that bounded redraw; `execute!` is avoided
// because it wants a concrete `io::Write`, and the picker writes through a
// `&mut dyn Write` seam (same seam the rest of the UI + tests use).
const MOVE_COL0: &str = "\x1b[1G";
const CLEAR_BELOW: &str = "\x1b[J";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
fn move_up(n: usize) -> String {
    format!("\x1b[{n}A")
}

/// How many entries of the filtered list are shown at once (an entry may
/// occupy several physical rows).
const WINDOW: usize = 9;

/// The most physical rows one entry may occupy before its tail is cut with
/// a bare ellipsis — a bound relative to the window so even a pathological
/// entry can't push the frame taller than the screen.
const MAX_ENTRY_LINES: usize = 7;

/// Display width of `s`, ignoring ANSI escape sequences (rows mix plain
/// text with skin-painted accents).
fn visible_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    crate::ui::markdown::strip_ansi(s).width()
}

/// Greedy word wrap to a display-width budget; words longer than the budget
/// are hard-broken at the boundary so no row ever wraps on its own (the
/// redraw counts physical lines).
pub fn wrap_words(text: &str, budget: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if budget == 0 {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let mut cur = String::new();
        let mut w = 0usize;
        for c in word.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > budget && !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(c);
            w += cw;
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for chunk in chunks {
        match lines.last_mut() {
            Some(last) if visible_width(last) + 1 + visible_width(&chunk) <= budget => {
                last.push(' ');
                last.push_str(&chunk);
            }
            _ => lines.push(chunk),
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// One selectable row (owned strings — callers build labels at runtime).
/// `id` is returned on selection (stable, not display text); `label` is what
/// the user sees; `sublabel` is an optional dimmed second hint (a path, a
/// timestamp, a count).
#[derive(Debug, Clone)]
pub struct Pick {
    pub id: String,
    pub label: String,
    pub sublabel: Option<String>,
    /// Shown but not selectable — the already-active choice (current skin,
    /// current model, the session that's open right now). Navigation skips
    /// over it and Enter on it does nothing.
    pub disabled: bool,
    /// The sublabel carries its own ANSI accent colors (e.g. a timestamp
    /// painted differently from the title) — written verbatim instead of
    /// being repainted with the row style. Verbatim sublabels render on one
    /// line; keep them short.
    pub sub_verbatim: bool,
    /// Pre-rendered lines under the row (skin color snapshots). Written
    /// verbatim and excluded from type-to-filter.
    pub extra_lines: Vec<String>,
    /// The color role a plain sublabel paints with, instead of the row's
    /// uniform style — command summaries in the palette read better in their
    /// own quieter voice than the label's. `None` keeps the old behavior.
    pub sub_role: Option<crate::ui::skin::Role>,
}

impl Pick {
    /// A plain, enabled row.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Pick {
            id: id.into(),
            label: label.into(),
            sublabel: None,
            disabled: false,
            sub_verbatim: false,
            extra_lines: Vec::new(),
            sub_role: None,
        }
    }

    /// Paint the plain sublabel in `role` (no-op for verbatim sublabels).
    pub fn hint_role(mut self, role: crate::ui::skin::Role) -> Self {
        self.sub_role = Some(role);
        self
    }

    /// Attach a hint shown after the label.
    pub fn hint(mut self, sub: impl Into<String>) -> Self {
        self.sublabel = Some(sub.into());
        self
    }

    /// Mark the row as the already-active choice.
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    /// Let the sublabel carry its own paint (accent colors).
    pub fn verbatim_sub(mut self) -> Self {
        self.sub_verbatim = true;
        self
    }

    /// Attach pre-rendered lines under the row (a color snapshot for skins).
    /// Written verbatim — the caller paints every byte — and never matched
    /// by type-to-filter, so preview words can't shadow real filtering.
    pub fn extra_lines(mut self, lines: Vec<String>) -> Self {
        self.extra_lines = lines;
        self
    }
}

/// Mutable filter/selection state of a [`pick`] run — split out so the
/// transition function below is pure and testable without a terminal.
#[derive(Debug, Default)]
pub struct PickState {
    /// What the user has typed so far.
    pub filter: String,
    /// Cursor position within the *filtered* list.
    pub sel: usize,
}

/// A key, as far as the picker cares — crossterm events map onto this, and
/// tests drive [`pick_step`] with it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKey {
    Type(char),
    Backspace,
    Up,
    Down,
    Home,
    End,
    Enter,
    Escape,
}

/// What the IO shell should do after [`pick_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickOutcome {
    /// Redraw and keep waiting for input.
    Continue,
    /// The user selected a row; `0`-based index into the full `items` list.
    Selected(usize),
    /// The user cancelled (Esc or Ctrl-C).
    Cancelled,
}

/// The indices of the items whose label or sublabel contains `filter`
/// (case-insensitive). An empty filter matches everything.
pub fn filter_items(items: &[Pick], filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            needle.is_empty()
                || it.label.to_lowercase().contains(&needle)
                || it
                    .sublabel
                    .as_ref()
                    .map(|s| s.to_lowercase().contains(&needle))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect()
}

/// The whole dropdown state machine in one pure function.
///
/// `visible` is the caller's current `filter_items(...)` result; because a
/// typed character changes the filter, this recomputes it and clamps `sel`
/// into range, then returns the outcome plus the list the caller should
/// render next.
pub fn pick_step(
    items: &[Pick],
    state: &mut PickState,
    visible: &mut Vec<usize>,
    key: PickKey,
) -> (PickOutcome, Vec<usize>) {
    let refilter = match key {
        PickKey::Type(c) => {
            state.filter.push(c);
            true
        }
        PickKey::Backspace => {
            state.filter.pop();
            true
        }
        _ => visible.is_empty() && state.filter.is_empty(), // first key, no matches yet
    };
    if refilter {
        *visible = filter_items(items, &state.filter);
    }
    if visible.is_empty() {
        state.sel = 0;
        // Nothing matches: stay put (Enter below selects nothing, because
        // there is no `visible[sel]` to point at).
        return (PickOutcome::Continue, Vec::new());
    }
    state.sel = state.sel.min(visible.len() - 1);
    // The already-active row is visible but dead: navigation steps over it
    // and Enter refuses it, so "selecting the current skin" can never
    // happen by accident.
    let selectable = visible
        .iter()
        .enumerate()
        .filter(|(_, &idx)| !items[idx].disabled)
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    match key {
        PickKey::Up if !selectable.is_empty() => {
            let cur = nearest_selectable(&selectable, state.sel);
            let pos = selectable.iter().position(|&i| i == cur).unwrap_or(0);
            let prev = selectable[(pos + selectable.len() - 1) % selectable.len()];
            state.sel = prev;
        }
        PickKey::Down if !selectable.is_empty() => {
            let cur = nearest_selectable(&selectable, state.sel);
            let pos = selectable.iter().position(|&i| i == cur).unwrap_or(0);
            state.sel = selectable[(pos + 1) % selectable.len()];
        }
        PickKey::Home if !selectable.is_empty() => state.sel = selectable[0],
        PickKey::End if !selectable.is_empty() => state.sel = *selectable.last().unwrap(),
        PickKey::Enter => {
            if items[visible[state.sel]].disabled {
                return (PickOutcome::Continue, visible.clone());
            }
            return (PickOutcome::Selected(visible[state.sel]), visible.clone());
        }
        PickKey::Escape => return (PickOutcome::Cancelled, visible.clone()),
        PickKey::Type(_) | PickKey::Backspace | PickKey::Up | PickKey::Down | PickKey::Home
        | PickKey::End => {}
    }
    (PickOutcome::Continue, visible.clone())
}

/// The closest selectable position to `sel` — `sel` itself if it points at a
/// live row (the common case), else the next one down, since filtering can
/// leave the clamped cursor on a dead row.
fn nearest_selectable(selectable: &[usize], sel: usize) -> usize {
    if selectable.contains(&sel) {
        sel
    } else {
        *selectable.iter().find(|&&i| i > sel).unwrap_or(&selectable[0])
    }
}

/// Render one frame of the dropdown into `w`. Returns the number of lines
/// written so the caller knows how many to clear before the next frame.
///
/// Rows are wrapped to the terminal width instead of cut: a long title or
/// model id flows onto indented continuation lines. Only one thing is ever
/// shortened, deliberately and with a bare ellipsis: an entry exceeding
/// [`MAX_ENTRY_LINES`] rows. No "(truncated N bytes)" anywhere — pickers
/// show text, not byte counts.
fn render_frame(
    w: &mut dyn Write,
    items: &[Pick],
    visible: &[usize],
    state: &PickState,
    skin: &Skin,
    hint: &str,
) -> usize {
    use crate::ui::skin::Role;

    // The picker redraws everything each key, unlike the stream path which
    // pins one width because committed bytes may not move — re-resolving the
    // width per frame is safe here.
    let width = crate::ui::markdown::terminal_width().unwrap_or(100);
    // Overhead per label row: bullet + space on the left, a small safety
    // margin on the right so a rounding-wide glyph can never spill into a
    // terminal wrap.
    let label_budget = width.saturating_sub(6).max(20);
    let sub_budget = label_budget.saturating_sub(2);

    let mut lines: Vec<String> = vec![format!("  {hint}")];
    if visible.is_empty() {
        lines.push("    no match".into());
        for line in &lines {
            let _ = write!(w, "{line}\r\n");
        }
        return lines.len();
    }

    // Window of at most WINDOW entries around the selection.
    let total = visible.len();
    let start = if total <= WINDOW {
        0
    } else {
        let s = state.sel.saturating_sub(WINDOW / 2);
        s.min(total - WINDOW)
    };
    if start > 0 {
        lines.push("    …".into());
    }
    for rel in 0..WINDOW.min(total - start) {
        let i = start + rel;
        let it = &items[visible[i]];
        let selected = i == state.sel;
        let (bullet, row_style) = if selected && it.disabled {
            // Cursor parked on the active choice (only possible right after
            // filtering): mark the dead state instead of letting Enter look
            // broken.
            ("·".to_string(), Role::ToolDim)
        } else if selected {
            (skin.paint(Role::Prompt, "❯"), Role::Answer)
        } else {
            ("  ".to_string(), Role::ToolDim)
        };

        // One entry assembles from plain-text parts — label lines, an
        // optional verbatim accent tail, optional sublabel lines (the first
        // of which may be glued onto the final label line when both fit) —
        // so widths and the budget never measure ANSI bytes; painting
        // happens at emit time and can give the sublabel its own role.
        let mut label_rows: Vec<(String, Option<String>)> =
            wrap_words(&it.label, label_budget).into_iter().map(|l| (l, None)).collect();
        let mut raw_tail: Option<String> = None;
        let mut sub_rows: Vec<String> = Vec::new();
        if let Some(sub) = it.sublabel.as_deref() {
            if it.sub_verbatim {
                if visible_width(sub) <= sub_budget {
                    raw_tail = Some(sub.to_string());
                } else {
                    // Pathologically wide accent text degrades to plain
                    // wrapped text rather than risking an overflowing row.
                    sub_rows = wrap_words(&crate::ui::markdown::strip_ansi(sub), sub_budget);
                    raw_tail = None;
                }
            } else {
                let mut sub_lines = wrap_words(sub, sub_budget);
                let fits_inline = label_rows.last().is_some_and(|(l, _)| {
                    visible_width(l) + 2 + visible_width(&sub_lines[0]) <= label_budget
                });
                if fits_inline {
                    // Glue the first subline onto the label row but remember
                    // it separately so the two paints stay independent.
                    if let Some((_, glued)) = label_rows.last_mut() {
                        *glued = sub_lines.remove(0).into();
                    }
                }
                sub_rows = sub_lines;
            }
        }

        // Per-entry row budget: the identity text is the label, so sublabel
        // rows are dropped first and only then does the label tail get cut.
        if label_rows.len() + sub_rows.len() > MAX_ENTRY_LINES {
            sub_rows.clear();
        }
        if label_rows.len() > MAX_ENTRY_LINES {
            label_rows.truncate(MAX_ENTRY_LINES);
            if let Some((last, _)) = label_rows.last_mut() {
                let cut = visible_width(last).saturating_sub(1).max(1);
                *last = format!("{}…", crate::util::truncate_utf8_display(last, cut));
            }
        }

        // Emit order: label rows (with any glue), then remaining sub rows.
        let rows: Vec<(String, Option<String>)> = label_rows
            .into_iter()
            .chain(sub_rows.into_iter().map(|s| (s, None)))
            .collect();
        let mut extras_rows: Vec<String> = Vec::new();
        if !it.extra_lines.is_empty() {
            let room = MAX_ENTRY_LINES.saturating_sub(rows.len());
            if room > 0 {
                let mut extras: Vec<String> =
                    it.extra_lines.iter().take(room).cloned().collect();
                if extras.len() < it.extra_lines.len() {
                    if let Some(last) = extras.last_mut() {
                        last.push_str(" …");
                    }
                }
                extras_rows = extras;
            }
        }

        let sub_paint = it.sub_role.unwrap_or(row_style);
        for (n, (text, inline_sub)) in rows.iter().enumerate() {
            let prefix = if n == 0 {
                format!("{bullet} ")
            } else {
                "    ".to_string()
            };
            match inline_sub.as_ref() {
                // Inline-glued subline: label keeps the row style, the tail
                // repaints in the sublabel role.
                Some(sub) => lines.push(format!(
                    "{prefix}{}  {}",
                    skin.paint(row_style, text),
                    skin.paint(sub_paint, sub)
                )),
                None => match raw_tail.as_ref().filter(|_| n + 1 == rows.len()) {
                    Some(tail) => lines.push(format!(
                        "{prefix}{}  {tail}",
                        skin.paint(row_style, text)
                    )),
                    None => lines.push(skin.paint(row_style, &format!("{prefix}{text}"))),
                },
            }
        }
        // Pre-rendered snapshot lines (verbatim paint), two-space indented.
        for extra in &extras_rows {
            lines.push(format!("    {extra}"));
        }
    }
    if start + WINDOW < total {
        lines.push("    …".into());
    }

    lines.push(format!(
        "    > {}  ({} of {})  ↑↓ move · enter pick · esc cancel",
        state.filter,
        total,
        items.len()
    ));

    for line in &lines {
        let _ = write!(w, "{line}\r\n");
    }
    lines.len()
}

/// Open the dropdown, block until a choice (or cancel), and clear the frame.
/// Returns the `id` of the chosen item, or `None` on cancel/empty list.
pub fn pick(items: &[Pick], skin: &Skin, hint: &str) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    // Non-TTY fallback: a numbered list and one line of input — the same
    // contract as the numbered menus this picker replaces.
    if !std::io::stdout().is_terminal() {
        return pick_plain(items, hint);
    }

    if enable_raw_mode().is_err() {
        // Raw mode unavailable (odd terminal): degrade to the plain list
        // rather than hanging on keys the kernel never delivers.
        return pick_plain(items, hint);
    }
    let mut out = io::stdout();
    let result = run_raw(items, skin, hint, &mut out);
    let _ = disable_raw_mode();
    result
}

fn run_raw(
    items: &[Pick],
    skin: &Skin,
    hint: &str,
    out: &mut dyn Write,
) -> Option<String> {
    let mut state = PickState::default();
    let mut visible = filter_items(items, "");
    // An empty list can't happen (pick() checked), but keep the invariant.
    if visible.is_empty() {
        return None;
    }

    let mut drawn = 0usize;
    loop {
        // Clear the previous frame, then redraw (bounded region — the picker
        // owns these lines until it exits).
        if drawn > 0 {
            let _ = write!(out, "{MOVE_COL0}{}{CLEAR_BELOW}", move_up(drawn));
        }
        drawn = render_frame(out, items, &visible, &state, skin, hint);
        let _ = write!(out, "{HIDE_CURSOR}");

        match event::read() {
            // EOF or a terminal read failure: cancel and restore.
            Err(_) => {
                clear_frame(out, drawn);
                return None;
            }
            Ok(ev) => {
                let Some(key) = map_event(ev) else {
                    continue; // ignore non-key events (resize, mouse)
                };
                let (outcome, next) = pick_step(items, &mut state, &mut visible, key);
                visible = next;
                match outcome {
                    PickOutcome::Selected(idx) => {
                        clear_frame(out, drawn);
                        return Some(items[idx].id.clone());
                    }
                    PickOutcome::Cancelled => {
                        clear_frame(out, drawn);
                        return None;
                    }
                    PickOutcome::Continue => {}
                }
            }
        }
    }
}

fn clear_frame(out: &mut dyn Write, drawn: usize) {
    if drawn > 0 {
        let _ = write!(out, "{MOVE_COL0}{}{CLEAR_BELOW}", move_up(drawn));
    }
    let _ = write!(out, "{SHOW_CURSOR}");
}

/// Translate one crossterm event into picker keys (or `None` to ignore).
fn map_event(ev: Event) -> Option<PickKey> {
    let Event::Key(k) = ev else {
        return None;
    };
    // On terminals that report press/release pairs, act on presses only.
    if k.kind != KeyEventKind::Press {
        return None;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') => Some(PickKey::Escape),
            _ => None,
        };
    }
    Some(match k.code {
        KeyCode::Char(c) => PickKey::Type(c),
        KeyCode::Backspace => PickKey::Backspace,
        KeyCode::Up => PickKey::Up,
        KeyCode::Down => PickKey::Down,
        KeyCode::Home => PickKey::Home,
        KeyCode::End => PickKey::End,
        KeyCode::Enter => PickKey::Enter,
        KeyCode::Esc => PickKey::Escape,
        // Unknown keys are ignored, never treated as cancel: a stray Tab or
        // ? in a menu must not throw away the choice.
        _ => return None,
    })
}

/// Numbered-list fallback for non-TTY runs (piped output, tests): prints the
/// options and reads one number — the same visible contract as the numbered
/// menus this picker replaces, minus the interactivity.
fn pick_plain(items: &[Pick], hint: &str) -> Option<String> {
    println!("\n{hint}");
    for (i, it) in items.iter().enumerate() {
        let marker = if it.disabled { " (current)" } else { "" };
        let mut line = format!("  {}) {}{marker}", i + 1, it.label);
        if let Some(sub) = it.sublabel.as_deref() {
            let plain = crate::ui::markdown::strip_ansi(sub);
            line.push_str(&format!("  {}", crate::util::truncate_utf8_display(&plain, 60)));
        }
        println!("{line}");
    }
    loop {
        print!("select [number, or 0 to cancel]: ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        match line.trim().parse::<usize>() {
            Ok(0) => return None,
            Ok(n) if (1..=items.len()).contains(&n) => {
                if items[n - 1].disabled {
                    println!("that choice is already active.");
                    continue;
                }
                return Some(items[n - 1].id.clone());
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Pick> {
        vec![
            Pick::new("solaris", "solaris").hint("warm ambers"),
            Pick::new("ocean", "ocean").hint("cool blues"),
            Pick::new("royal", "royal").hint("deep purple"),
            Pick::new("mono", "mono"),
        ]
    }

    /// Drive the pure state machine through `keys`; returns the final
    /// outcome.
    fn drive(items: &[Pick], keys: &[PickKey]) -> PickOutcome {
        let mut state = PickState::default();
        let mut visible = filter_items(items, "");
        let mut outcome = PickOutcome::Continue;
        for k in keys {
            let (next, vis) = pick_step(items, &mut state, &mut visible, *k);
            outcome = next;
            visible = vis;
            if matches!(outcome, PickOutcome::Selected(_) | PickOutcome::Cancelled) {
                break;
            }
        }
        outcome
    }

    #[test]
    fn rendered_frames_use_crlf_line_endings() {
        // In raw mode the terminal's ONLCR translation is off, so a bare \n
        // moves down without returning to column 0 — each subsequent line
        // drifts right by the width of the previous one. Every frame line
        // must therefore be \r\n terminated. (Regression: / <Enter> rendered
        // the palette with every row shunted right.)
        let mut out: Vec<u8> = Vec::new();
        let items = items();
        let visible = filter_items(&items, "");
        let state = PickState::default();
        render_frame(&mut out, &items, &visible, &state, &crate::ui::skin::SOLARIS, "hint");
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "every \\n must be preceded by \\r (raw-mode LF-only drift): {text:?}"
        );
        assert!(text.ends_with("\r\n"), "last line must end with \\r\\n: {text:?}");
    }

    #[test]
    fn long_labels_wrap_instead_of_truncating() {
        // A long label used to be cut at the row width; now it flows onto
        // indented continuation rows, and nothing is ever cut with a byte
        // count — the only shortening is the per-entry `…` row budget.
        let long = format!("fact {} end", "x ".repeat(120));
        let items = vec![Pick::new("1", long)];
        let visible = filter_items(&items, "");
        let state = PickState::default();

        let mut out: Vec<u8> = Vec::new();
        render_frame(&mut out, &items, &visible, &state, &crate::ui::skin::SOLARIS, "hint");
        let text = strip_frames_ansi(&String::from_utf8(out).unwrap());

        let body: Vec<&str> = text
            .lines()
            .filter(|l| l.contains('x') || l.contains("end"))
            .collect();
        assert!(body.len() > 1, "label must span several rows: {text:?}");
        assert!(
            text.lines()
                .any(|l| strip_frames_ansi(l).trim_end().ends_with("end")),
            "the tail must survive wrapping: {text:?}"
        );
        assert!(!text.contains("truncated"), "no byte-count markers: {text:?}");
    }

    #[test]
    fn wrap_words_respects_display_width_and_hard_breaks_long_words() {
        assert_eq!(wrap_words("one two three", 8), vec!["one two", "three"]);
        // A single word longer than the budget is split, never dropped.
        assert_eq!(wrap_words("abcdef", 3), vec!["abc", "def"]);
        // Wide chars count as two columns.
        assert_eq!(wrap_words("日本語テスト", 4), vec!["日本", "語テ", "スト"]);
    }

    #[test]
    fn an_entry_taller_than_the_budget_is_cut_with_a_bare_ellipsis() {
        let long = (0..200)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let items = vec![Pick::new("1", long)];
        let visible = filter_items(&items, "");
        let state = PickState::default();
        let mut out: Vec<u8> = Vec::new();
        render_frame(&mut out, &items, &visible, &state, &crate::ui::skin::SOLARIS, "hint");
        let plain = strip_frames_ansi(&String::from_utf8(out).unwrap());
        let entry_rows = plain.lines().count().saturating_sub(2); // hint + footer
        assert!(entry_rows <= MAX_ENTRY_LINES, "entry exceeded its row budget: {plain:?}");
        assert!(plain.contains('…'), "the cut must be marked with an ellipsis: {plain:?}");
    }

    #[test]
    fn navigation_skips_disabled_rows_and_enter_refuses_them() {
        let mut items = items();
        items[2].disabled = true; // royal is "current"
        // From ocean (sel 1), Down lands on mono (3), never on royal (2).
        let mut state = PickState { filter: String::new(), sel: 1 };
        let mut visible = filter_items(&items, "");
        let (o, _) = pick_step(&items, &mut state, &mut visible, PickKey::Down);
        assert_eq!(o, PickOutcome::Continue);
        assert_eq!(state.sel, 3);
        // Filtering down to only the disabled row leaves Enter powerless.
        let mut state = PickState::default();
        let mut visible = filter_items(&items, "");
        for k in [
            PickKey::Type('r'),
            PickKey::Type('o'),
            PickKey::Type('y'),
            PickKey::Type('a'),
            PickKey::Type('l'),
        ] {
            let (_, v) = pick_step(&items, &mut state, &mut visible, k);
            visible = v;
        }
        assert_eq!(visible, vec![2]);
        let (o, _) = pick_step(&items, &mut state, &mut visible, PickKey::Enter);
        assert_eq!(o, PickOutcome::Continue, "the current choice must not be selectable");
        // Navigation from a parked-on-dead cursor still reaches live rows.
        let (_, _) = pick_step(&items, &mut state, &mut visible, PickKey::Down);
        assert_ne!(state.sel, 2);
    }

    #[test]
    fn a_verbatim_sublabel_keeps_accent_text_on_the_row() {
        let accent = "\x1b[38;2;255;0;0m14:32\x1b[0m";
        let items = vec![Pick::new("s1", "debug session").hint(accent).verbatim_sub()];
        let visible = filter_items(&items, "");
        let state = PickState::default();
        let mut out: Vec<u8> = Vec::new();
        render_frame(&mut out, &items, &visible, &state, &crate::ui::skin::SOLARIS, "hint");
        let raw = String::from_utf8(out).unwrap();
        assert!(raw.contains("14:32"), "accent text present: {raw:?}");
        let row = raw.lines().nth(1).unwrap();
        assert!(
            !strip_frames_ansi(row).contains('\x1b'),
            "row must carry no torn escapes after stripping: {row:?}"
        );
    }

    #[test]
    fn a_hint_role_paints_the_sublabel_without_breaking_the_row() {
        // CLICOLOR_FORCE makes paint() emit escapes even though tests run
        // without a TTY; the env guard restores the process env after.
        let _lock = crate::util::test_support::env_guard();
        std::env::set_var("CLICOLOR_FORCE", "1");
        let items = vec![Pick::new("exit", "/exit")
            .hint("Exit the chat")
            .hint_role(crate::ui::skin::Role::Hint)];
        let visible = filter_items(&items, "");
        let state = PickState::default();
        let mut out: Vec<u8> = Vec::new();
        render_frame(&mut out, &items, &visible, &state, &crate::ui::skin::SOLARIS, "hint");
        let raw = String::from_utf8(out).unwrap();
        // Derive the expected escapes from the live skin's style() (not
        // hardcoded RGB) so a palette re-tune can't silently break this: the
        // summary wears the hint role — now foreground *and* its *_bg tint —
        // and the lone selected row's label wears the answer color.
        use crate::ui::skin::{Role, SOLARIS};
        // paint() is `style.render()` directly followed by the text (no gap).
        // Derive it from the live skin so a palette re-tune can't silently
        // break this: the summary wears the hint role — foreground *and* its
        // *_bg tint — and the lone selected row's label wears the answer
        // color (not tool_dim).
        let hint_esc = format!("{}{}", SOLARIS.style(Role::Hint).render(), "Exit the chat");
        let ans_esc = format!("{}{}", SOLARIS.style(Role::Answer).render(), "/exit");
        assert!(raw.contains(&hint_esc), "{raw:?}");
        assert!(raw.contains(&ans_esc), "{raw:?}");
        // Filtering still matches the plain sublabel text.
        assert_eq!(filter_items(&items, "chat"), vec![0]);
        std::env::remove_var("CLICOLOR_FORCE");
    }

    /// Escape stripper for frame assertions (rows are skin-painted).
    fn strip_frames_ansi(s: &str) -> String {
        crate::ui::markdown::strip_ansi(s)
    }

    #[test]
    fn an_empty_filter_matches_everything_in_order() {
        let items = items();
        assert_eq!(filter_items(&items, ""), vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_filter_matches_label_and_sublabel_case_insensitively() {
        let items = items();
        assert_eq!(filter_items(&items, "OCE"), vec![1]);
        assert_eq!(filter_items(&items, "purple"), vec![2]);
        assert_eq!(filter_items(&items, "zzz"), Vec::<usize>::new());
    }

    #[test]
    fn enter_selects_the_current_row() {
        let items = items();
        assert_eq!(drive(&items, &[PickKey::Down, PickKey::Enter]), PickOutcome::Selected(1));
    }

    #[test]
    fn navigation_wraps_and_type_to_filters_to_the_only_match() {
        let items = items();
        // From the top, Up wraps to the last row; filtering "mon" narrows to
        // mono only (no label or sublabel of the others contains it); Enter
        // picks it.
        assert_eq!(
            drive(
                &items,
                &[
                    PickKey::Up,
                    PickKey::Type('m'),
                    PickKey::Type('o'),
                    PickKey::Type('n'),
                    PickKey::Enter
                ]
            ),
            PickOutcome::Selected(3)
        );
    }

    #[test]
    fn typing_filters_and_clamps_the_cursor() {
        let items = items();
        let mut state = PickState {
            filter: String::new(),
            sel: 3,
        };
        let mut visible = filter_items(&items, "");
        // Filter to one item while the cursor is past its end: `sel` clamps,
        // and Enter still selects.
        let (_, mut visible) = pick_step(&items, &mut state, &mut visible, PickKey::Type('o'));
        let (_, mut visible) = pick_step(&items, &mut state, &mut visible, PickKey::Type('c'));
        let (_, mut visible) = pick_step(&items, &mut state, &mut visible, PickKey::Type('e'));
        assert_eq!(visible, vec![1]);
        let (o, _) = pick_step(&items, &mut state, &mut visible, PickKey::Enter);
        assert_eq!(o, PickOutcome::Selected(1));
    }

    #[test]
    fn backspace_restores_the_list() {
        let items = items();
        let mut state = PickState::default();
        let mut visible = filter_items(&items, "");
        // "e" hits three: solaris, royal (via the "e" in their sublabels
        // "warm ambers"/"deep purple") and ocean (its label) — the filter is
        // over both fields; mono has no "e" anywhere.
        let (_, mut visible) = pick_step(&items, &mut state, &mut visible, PickKey::Type('e'));
        assert_eq!(visible, vec![0, 1, 2]);
        let (_, visible) = pick_step(&items, &mut state, &mut visible, PickKey::Backspace);
        assert_eq!(visible, vec![0, 1, 2, 3]);
    }

    #[test]
    fn escape_cancels() {
        let items = items();
        assert_eq!(
            drive(&items, &[PickKey::Down, PickKey::Escape]),
            PickOutcome::Cancelled
        );
    }

    #[test]
    fn home_and_end_jump_within_the_filtered_list() {
        let items = items();
        let mut state = PickState::default();
        let mut visible = filter_items(&items, "");
        pick_step(&items, &mut state, &mut visible, PickKey::Type('s')); // solaris only
        let (_, visible) = pick_step(&items, &mut state, &mut visible, PickKey::End);
        assert_eq!(state.sel, visible.len() - 1);
    }
}
