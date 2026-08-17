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
use crate::util::truncate_utf8;
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

/// How many rows of the filtered list are shown at once.
const WINDOW: usize = 9;

/// One selectable row. `id` is returned on selection (stable, not display
/// text); `label` is what the user sees; `sublabel` is an optional dimmed
/// second hint (a path, a timestamp, a count).
#[derive(Debug, Clone, Copy)]
pub struct Pick<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub sublabel: Option<&'a str>,
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
pub fn filter_items(items: &[Pick<'_>], filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            needle.is_empty()
                || it.label.to_lowercase().contains(&needle)
                || it
                    .sublabel
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
    items: &[Pick<'_>],
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
    match key {
        PickKey::Up => {
            state.sel = (state.sel + visible.len() - 1) % visible.len();
        }
        PickKey::Down => {
            state.sel = (state.sel + 1) % visible.len();
        }
        PickKey::Home => state.sel = 0,
        PickKey::End => state.sel = visible.len() - 1,
        PickKey::Enter => return (PickOutcome::Selected(visible[state.sel]), visible.clone()),
        PickKey::Escape => return (PickOutcome::Cancelled, visible.clone()),
        PickKey::Type(_) | PickKey::Backspace => {}
    }
    (PickOutcome::Continue, visible.clone())
}

/// Render one frame of the dropdown into `w`. Returns the number of lines
/// written so the caller knows how many to clear before the next frame.
fn render_frame(
    w: &mut dyn Write,
    items: &[Pick<'_>],
    visible: &[usize],
    state: &PickState,
    skin: &Skin,
    hint: &str,
) -> usize {
    use crate::ui::skin::Role;

    let mut lines: Vec<String> = vec![format!("  {hint}")];
    if visible.is_empty() {
        lines.push("    no match".into());
        for line in &lines {
            let _ = writeln!(w, "{line}");
        }
        return lines.len();
    }

    // Window of at most WINDOW rows around the selection.
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
    for (rel, &idx) in visible[start..(start + WINDOW).min(total)].iter().enumerate() {
        let i = start + rel;
        let it = &items[idx];
        let (bullet, row_style) = if i == state.sel {
            (skin.paint(Role::Prompt, "❯"), Role::Answer)
        } else {
            ("  ".to_string(), Role::ToolDim)
        };
        let mut row = format!("{bullet} {}", truncate_utf8(it.label, 76));
        if let Some(sub) = it.sublabel {
            row.push_str(&format!("  {}", truncate_utf8(sub, 40)));
        }
        lines.push(skin.paint(row_style, &row));
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
        let _ = writeln!(w, "{line}");
    }
    lines.len()
}

/// Open the dropdown, block until a choice (or cancel), and clear the frame.
/// Returns the `id` of the chosen item, or `None` on cancel/empty list.
pub fn pick(items: &[Pick<'_>], skin: &Skin, hint: &str) -> Option<String> {
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
    items: &[Pick<'_>],
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
                        return Some(items[idx].id.to_string());
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
fn pick_plain(items: &[Pick<'_>], hint: &str) -> Option<String> {
    println!("\n{hint}");
    for (i, it) in items.iter().enumerate() {
        let mut line = format!("  {}) {}", i + 1, it.label);
        if let Some(sub) = it.sublabel {
            line.push_str(&format!("  {}", truncate_utf8(sub, 60)));
        }
        println!("{line}");
    }
    print!("select [number, or 0 to cancel]: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    match line.trim().parse::<usize>() {
        Ok(0) => None,
        Ok(n) if (1..=items.len()).contains(&n) => Some(items[n - 1].id.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Pick<'static>> {
        vec![
            Pick { id: "solaris", label: "solaris", sublabel: Some("warm ambers") },
            Pick { id: "ocean", label: "ocean", sublabel: Some("cool blues") },
            Pick { id: "royal", label: "royal", sublabel: Some("deep purple") },
            Pick { id: "mono", label: "mono", sublabel: None },
        ]
    }

    /// Drive the pure state machine through `keys`; returns the final
    /// outcome.
    fn drive(items: &[Pick<'_>], keys: &[PickKey]) -> PickOutcome {
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
