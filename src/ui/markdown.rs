//! Minimal Markdown → terminal renderer using pulldown-cmark + syntect.
//!
//! Renders GitHub-Flavored Markdown to ANSI-styled terminal output. Only applied
//! when stdout is a real TTY; when piped, returns raw text unchanged.

use crate::ui::skin::Skin;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::io::IsTerminal;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";

/// Build the 24-bit ANSI escape for `skin`'s code color.
fn code_color(skin: &Skin) -> String {
    let anstyle::RgbColor(r, g, b) = skin.code;
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Ensure `out` ends with exactly `n` newlines (no more, no fewer).
/// This is the core spacing primitive — it guarantees blank separation
/// between block elements without ever doubling up.
fn ensure_blank(out: &mut String, n: usize) {
    if out.is_empty() {
        return;
    }
    let trailing_newlines = out.chars().rev().take_while(|&c| c == '\n').count();
    if trailing_newlines < n {
        for _ in 0..(n - trailing_newlines) {
            out.push('\n');
        }
    }
}

/// Render `md` to terminal-friendly ANSI text if stdout is a TTY; otherwise
/// return it unchanged.
pub fn render_terminal(md: &str, skin: &Skin) -> String {
    render_terminal_colored(
        md,
        skin,
        std::io::stdout().is_terminal() && !crate::ui::skin::no_color(),
    )
}

/// The column width to render to: the live size of the terminal attached to
/// stdout (ioctl, so a resize is picked up — shells here do not reliably
/// export `COLUMNS`), falling back to the `COLUMNS` environment variable for
/// piped output (e.g. `grace | less -R`) and tests.  `None` means unknown:
/// tables keep their natural width, which may overflow the terminal.
pub fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size()
        .map(|(cols, _)| cols.0 as usize)
        .or_else(columns_env_width)
}

/// The `COLUMNS` environment variable, parsed and positive-checked.
fn columns_env_width() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|&w| w > 0)
}

/// Render `md` with explicit color control.  When `color` is true, the output
/// contains ANSI escapes for bold/headings/code; when false, the raw markdown
/// is returned unchanged.  This is the primitive behind [`render_terminal`]
 /// (which passes `stdout().is_terminal() && !no_color()`) and is also used by the streaming
 /// renderer, which decides color once via [`crate::ui::skin::no_color`] and
/// re‑applies that decision on every incremental render.  Tables are fitted
/// to [`terminal_width`].
pub fn render_terminal_colored(md: &str, skin: &Skin, color: bool) -> String {
    render_terminal_width(md, skin, color, terminal_width())
}

/// The renderer proper, with the render width resolved by the caller.
/// One-shot callers pass [`terminal_width()`] (fresh on each call); the
/// streaming renderer passes the width pinned at stream start (see
/// [`crate::ui::chat::StreamState::width`]) so a mid-stream terminal resize
/// cannot change an already-committed table's bytes and break the
/// prefix-stable invariant the delta emitter relies on.
pub fn render_terminal_width(md: &str, skin: &Skin, color: bool, width: Option<usize>) -> String {
    if !color {
        return md.to_string();
    }

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);

    // Loaded once per process: reconstructing the default syntax/theme sets
    // takes hundreds of milliseconds, and the streaming renderer calls this
    // once per finalized block.
    static SS: std::sync::LazyLock<SyntaxSet> =
        std::sync::LazyLock::new(SyntaxSet::load_defaults_newlines);
    static TS: std::sync::LazyLock<ThemeSet> = std::sync::LazyLock::new(ThemeSet::load_defaults);
    let theme = &TS.themes["base16-ocean.dark"];
    let ss = &*SS;

    let gold = code_color(skin);
    let heading_color = heading_ansi(skin);

    let mut out = String::with_capacity(md.len() + md.len() / 4);

    // State
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut heading_level: usize = 0;
    let mut in_blockquote = false;
    let mut bq_needs_prefix = true;
    // Stack of (is_ordered, index) for nested lists
    let mut list_stack: Vec<(bool, usize)> = Vec::new();
    let mut list_item_started = false;

    // Inline emphasis stack
    let mut strong_stack = 0u32;
    let mut em_stack = 0u32;
    let mut _in_link = false;

    // Table state
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut in_cell = false;
    let mut cell_buf = String::new();
    let mut in_table_head = false;
    let mut header_row_indices: Vec<usize> = Vec::new();

    for event in parser {
        match event {
            // ── Start tags ───────────────────────────────────────
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    ensure_blank(&mut out, 1);
                }
                Tag::Heading { level, .. } => {
                    ensure_blank(&mut out, 2);
                    heading_level = level as usize;
                }
                Tag::CodeBlock(kind) => {
                    ensure_blank(&mut out, 1);
                    in_code = true;
                    code_lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(info) => info.to_string(),
                        pulldown_cmark::CodeBlockKind::Indented => String::new(),
                    };
                    code_buf.clear();
                }
                Tag::BlockQuote(_) => {
                    ensure_blank(&mut out, 1);
                    in_blockquote = true;
                    // ▏ prefix will be added by the first Text/SoftBreak
                }
                Tag::List(ordered) => {
                    if !list_stack.is_empty() {
                        // Nested list — no blank line between items of the same list
                    } else {
                        ensure_blank(&mut out, 1);
                    }
                    list_stack.push((ordered.is_some(), 0));
                }
                Tag::Item => {
                    list_item_started = true;
                }
                // State only — the escapes are emitted with the styled TEXT
                // (see Event::Text), not here: in tight list items a tag can
                // open before the newline that starts its text's line, and an
                // early escape would land on the previous line, breaking the
                // byte-prefix invariant the streaming emitter relies on.
                Tag::Strong => {
                    strong_stack += 1;
                }
                Tag::Emphasis => {
                    em_stack += 1;
                }
                Tag::Link { .. } => {
                    _in_link = true;
                }
                Tag::Table(_) => {
                    ensure_blank(&mut out, 1);
                    table_rows.clear();
                    current_row.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                }
                Tag::TableRow => {
                    current_row.clear();
                }
                Tag::TableCell => {
                    in_cell = true;
                    cell_buf.clear();
                }
                _ => {}
            },

            // ── End tags ─────────────────────────────────────────
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    out.push('\n');
                }
                TagEnd::Heading(_) => {
                    out.push_str("\n\n");
                    heading_level = 0;
                }
                TagEnd::CodeBlock => {
                    if !code_buf.is_empty() {
                        out.push_str(&render_code_block(
                            &code_buf,
                            &code_lang,
                            ss,
                            theme,
                            &gold,
                            width,
                        ));
                    }
                    in_code = false;
                    code_lang.clear();
                    code_buf.clear();
                }
                TagEnd::BlockQuote(_) => {
                    out.push('\n');
                    in_blockquote = false;
                    bq_needs_prefix = true;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::Item => {
                    list_item_started = false;
                    out.push('\n');
                    // Reset numbering for next item in this list level
                    if let Some(elem) = list_stack.last_mut() {
                        elem.1 += 1;
                    }
                }
                // The closing reset goes to the same buffer the styled text
                // went to (cells render from cell_buf, not `out`).
                TagEnd::Strong => {
                    strong_stack = strong_stack.saturating_sub(1);
                    if in_cell {
                        cell_buf.push_str(RESET);
                    } else {
                        out.push_str(RESET);
                    }
                }
                TagEnd::Emphasis => {
                    em_stack = em_stack.saturating_sub(1);
                    if in_cell {
                        cell_buf.push_str(RESET);
                    } else {
                        out.push_str(RESET);
                    }
                }
                TagEnd::Link => {
                    _in_link = false;
                    if in_cell {
                        cell_buf.push_str(RESET);
                    } else {
                        out.push_str(RESET);
                    }
                }
                TagEnd::TableHead => {
                    // The header row is complete - push it and record its index
                    if !current_row.is_empty() {
                        if in_table_head {
                            // row_idx intentionally unused; tracked via table_rows.len()
                            header_row_indices.push(table_rows.len());
                        }
                        table_rows.push(current_row.clone());
                    }
                    in_table_head = false;
                }
                TagEnd::Table => {
                    if !table_rows.is_empty() {
                        out.push_str(&render_table(&table_rows, &header_row_indices, width));
                    }
                    table_rows.clear();
                    header_row_indices.clear();
                }
                TagEnd::TableRow => {
                    if !current_row.is_empty() {
                        if in_table_head {
                            // Header row already pushed in TableHead end
                            // (TableRow inside TableHead doesn't fire separately in pulldown-cmark)
                        } else {
                            table_rows.push(current_row.clone());
                        }
                    }
                }
                TagEnd::TableCell => {
                    if in_cell {
                        current_row.push(cell_buf.clone());
                    }
                    in_cell = false;
                    cell_buf.clear();
                }
                _ => {}
            },

            // ── Text ─────────────────────────────────────────────
            Event::Text(text) => {
                // Open inline styles are applied to the buffer the text
                // actually lands in, at the instant it is written — not at
                // the tag-open position.  In tight list items a tag can open
                // before the newline that starts this text's line, and an
                // early escape would land on the previous line, breaking the
                // byte-prefix invariant the streaming emitter depends on.
                let open_style = |buf: &mut String| {
                    if strong_stack > 0 {
                        buf.push_str(BOLD);
                    }
                    if em_stack > 0 {
                        buf.push_str(ITALIC);
                    }
                    if _in_link {
                        buf.push_str(UNDERLINE);
                    }
                };
                if in_code {
                    code_buf.push_str(&text);
                } else if in_cell {
                    open_style(&mut cell_buf);
                    cell_buf.push_str(&text);
                } else if in_blockquote {
                    if bq_needs_prefix {
                        out.push_str(DIM);
                        out.push_str("▏ ");
                        out.push_str(RESET);
                        bq_needs_prefix = false;
                    }
                    out.push_str(DIM);
                    open_style(&mut out);
                    out.push_str(&text);
                    out.push_str(RESET);
                } else if heading_level > 0 {
                    out.push_str(&heading_color);
                    out.push_str(BOLD);
                    open_style(&mut out);
                    out.push_str(&text);
                    out.push_str(RESET);
                } else if list_item_started {
                    let indent = list_depth(&list_stack);
                    // Ensure we're on a new line before printing the bullet
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&"  ".repeat(indent));
                    let (is_ordered, idx) = list_stack.last().copied().unwrap_or((false, 0));
                    if is_ordered {
                        out.push_str(&format!("{}. ", idx + 1));
                    } else {
                        out.push_str(BOLD);
                        out.push_str("• ");
                        out.push_str(RESET);
                    }
                    list_item_started = false;
                    open_style(&mut out);
                    out.push_str(&text);
                } else if !list_stack.is_empty() {
                    // Continuation text in a list item
                    open_style(&mut out);
                    out.push_str(&text);
                } else {
                    open_style(&mut out);
                    out.push_str(&text);
                }
            },

            // ── Inline code ──────────────────────────────────────
            Event::Code(text) => {
                if in_code {
                    code_buf.push_str(&text);
                } else if in_cell {
                    cell_buf.push_str(&gold);
                    cell_buf.push_str(&text);
                    cell_buf.push_str(RESET);
                } else {
                    out.push_str(&gold);
                    out.push_str(&text);
                    out.push_str(RESET);
                }
            },

            // ── Line breaks ──────────────────────────────────────
            Event::SoftBreak => {
                if in_code {
                    // Keep newlines in code blocks
                } else if in_cell {
                    cell_buf.push('\n');
                } else if in_blockquote {
                    out.push('\n');
                    out.push_str(DIM);
                    out.push_str("▏ ");
                    out.push_str(RESET);
                } else {
                    out.push('\n');
                }
            }
            Event::HardBreak => {
                if !in_code && !in_cell {
                    out.push('\n');
                }
            }

            // ── Horizontal rule ──────────────────────────────────
            Event::Rule => {
                ensure_blank(&mut out, 2);
                out.push_str(DIM);
                out.push_str("────────────────────────────────────────\n\n");
                out.push_str(RESET);
            }

            // ── Task list ────────────────────────────────────────
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "[x] " } else { "[ ] " });
            }

            _ => {}
        }
    }

    // Trim trailing whitespace but keep content.  With *no* content the
    // result must be empty, not a lone newline: the streaming renderer
    // incrementally renders growing prefixes and requires render(p) to be a
    // byte-prefix of render(p+more).  A phantom "\n" for empty input breaks
    // that (render("\n\n")=="\n" vs render("\n\nPONG")=="PONG\n"), which made
    // the duplication guard drop the entire answer — silent total loss,
    // e.g. for thinking-model replies whose content starts with "\n\n".
    let trimmed = out.trim_end_matches('\n').to_string();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn list_depth(stack: &[(bool, usize)]) -> usize {
    // The current item is in the last-pushed list; its indent is `depth - 1`
    stack.len().saturating_sub(1)
}

/// ANSI color for headings, derived from the skin's answer color.
fn heading_ansi(skin: &Skin) -> String {
    let anstyle::RgbColor(r, g, b) = skin.answer;
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Render a fenced code block with syntax highlighting and a content-width box.
///
/// A single very long line (e.g. a pasted command) would otherwise stretch the
/// box across the whole terminal. When a terminal `width` is known, the box is
/// capped to it and long lines are wrapped to fit, so the box stays on-screen.
fn render_code_block(
    code: &str,
    lang: &str,
    ss: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
    _gold: &str,
    width: Option<usize>,
) -> String {
    let syntax = ss
        .find_syntax_by_token(lang.trim())
        .or_else(|| ss.find_syntax_by_extension("rs"))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let lines: Vec<&str> = code.lines().collect();
    let natural = lines.iter().map(|l| display_width(l)).max().unwrap_or(0);
    // The box has two border columns plus two inner padding columns.
    let cap = width.map(|w| w.saturating_sub(4)).unwrap_or(usize::MAX);
    let box_width = natural.min(cap).max(20) + 2;

    let mut out = String::new();

    out.push_str(DIM);
    out.push('┌');
    out.push_str(&"─".repeat(box_width));
    out.push('┐');
    out.push('\n');
    out.push_str(RESET);

    for line in &lines {
        let wrapped: Vec<String> = if display_width(line) <= box_width.saturating_sub(2) {
            vec![(*line).to_string()]
        } else {
            wrap_line(line, box_width.saturating_sub(2))
        };
        for piece in &wrapped {
            let ranges = highlighter.highlight_line(piece, ss).unwrap_or_default();
            let visible_len = display_width(piece);
            let pad = box_width.saturating_sub(visible_len + 2);

            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(RESET);
            for (style, text) in &ranges {
                let color = syntect_style_to_ansi(*style);
                out.push_str(&color);
                out.push_str(text);
                out.push_str(RESET);
            }
            out.push_str(&" ".repeat(pad));
            out.push_str(DIM);
            out.push_str(" │");
            out.push('\n');
            out.push_str(RESET);
        }
    }

    out.push_str(DIM);
    out.push('└');
    out.push_str(&"─".repeat(box_width));
    out.push('┘');
    out.push('\n');
    out.push_str(RESET);

    out
}

/// Convert a syntect Style to ANSI escape sequence.
fn syntect_style_to_ansi(style: SyntectStyle) -> String {
    let fg = style.foreground;
    let ansi_style =
        anstyle::Style::new().fg_color(Some(anstyle::Color::from(anstyle::RgbColor(
            fg.r, fg.g, fg.b,
        ))));
    ansi_style.render().to_string()
}

/// Render a markdown table as aligned box-drawing.  The columns start at
/// their natural width (widest content line); when the resulting box would
/// be wider than `width` columns, the column widths are shrunk toward
/// `budget = width - (3*ncols + 1)` — the per-column cost of border+padding,
/// plus the left border — and cell text is wrapped to fit, so a wide table
/// stays one line per row instead of every row soft-wrapping.  `width ==
/// None` (no terminal / unknown size), or a fit that cannot give every
/// column at least one column, renders at natural width (the old behavior).
fn render_table(
    rows: &[Vec<String>],
    header_indices: &[usize],
    width: Option<usize>,
) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return String::new();
    }

    let mut widths = vec![0usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.lines().map(display_width).max().unwrap_or(0));
        }
    }

    // A rendered row is Σcolumn widths + 3 columns per column (one border,
    // two padding) + the one leftmost border.
    if let Some(w) = width {
        if widths.iter().sum::<usize>() + 3 * ncols + 1 > w {
            if let Some(fit) = fit_widths(&widths, w.saturating_sub(3 * ncols + 1)) {
                widths = fit;
            }
        }
    }

    let mut out = String::new();

    let border = |out: &mut String, start: char, join: char, end: char| {
        out.push_str(DIM);
        out.push(start);
        for (ci, w) in widths.iter().enumerate() {
            out.push_str(&"─".repeat(w + 2));
            out.push(if ci + 1 == ncols { end } else { join });
        }
        out.push('\n');
        out.push_str(RESET);
    };

    border(&mut out, '┌', '┬', '┐');

    for (ri, row) in rows.iter().enumerate() {
        let is_header = header_indices.contains(&ri);
        let cells: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(ci, cell)| wrap_cell_to(cell, widths[ci]))
            .collect();
        let height = cells.iter().map(|c| c.len()).max().unwrap_or(1);
        for j in 0..height {
            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(RESET);
            for (ci, lines) in cells.iter().enumerate() {
                let line = lines.get(j).map(String::as_str).unwrap_or("");
                let pad = widths[ci].saturating_sub(display_width(line));
                if is_header {
                    out.push_str(BOLD);
                    out.push_str(line);
                    out.push_str(RESET);
                } else {
                    out.push_str(line);
                }
                out.push_str(&" ".repeat(pad));
                out.push_str(DIM);
                if ci + 1 == ncols {
                    out.push_str(" │");
                } else {
                    out.push_str(" │ ");
                }
                out.push_str(RESET);
            }
            out.push('\n');
        }

        // Header separator: ├───┼───┤ (only after the LAST header row)
        if is_header && header_indices.last() == Some(&ri) {
            border(&mut out, '├', '┼', '┤');
        }
    }

    border(&mut out, '└', '┴', '┘');

    out
}

/// Display lines for `cell` at column width `w`.  A line that already fits
/// is kept verbatim (byte-identical output to the old render); a wider line
/// is word-wrapped.
fn wrap_cell_to(cell: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in cell.lines() {
        if display_width(line) <= w {
            lines.push(line.to_string());
        } else {
            lines.extend(wrap_line(line, w));
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Shrink `ideal` column widths so they sum to exactly `budget`.  Each
/// column keeps at least 1 column, and at least 2 when its content is wider
/// (a two-column glyph like ✅ must always have room in a wrapped layout);
/// the rest is allocated proportionally to the ideal widths, and any
/// leftover goes one column at a time to the widest-deficit column.
/// `None` when the floors alone exceed the budget — the caller then renders
/// at natural width, because no fit would keep the box rows aligned.
fn fit_widths(ideal: &[usize], budget: usize) -> Option<Vec<usize>> {
    let n = ideal.len();
    if n == 0 || budget < n {
        return None;
    }
    let floor = |x: usize| x.clamp(1, 2);
    let floor_sum: usize = ideal.iter().map(|&x| floor(x)).sum();
    if floor_sum > budget {
        return None;
    }
    // total == 0 means every column is empty; all ideal[i] are then 0 and
    // the floor/cap below pin them at one frame column regardless.
    let total: u128 = ideal.iter().map(|&x| x as u128).sum();
    let total = total.max(1);
    let mut w: Vec<usize> = (0..n)
        .map(|i| {
            let proportional = budget as u128 * ideal[i] as u128 / total;
            // The cap keeps columns at their ideal width: the caller only
            // shrinks (the natural box already overflowed), and a degenerate
            // budget ≥ Σideal must not grow anything.
            let cap = ideal[i].max(1) as u128; // empty columns keep one frame column
            proportional.max(floor(ideal[i]) as u128).min(cap) as usize
        })
        .collect();
    // Floors can push the sum past the budget; steal it back from the widest
    // columns that still have room above their floor.
    let mut over = w.iter().sum::<usize>().saturating_sub(budget);
    while over > 0 {
        let i = (0..n).filter(|&i| w[i] > floor(ideal[i])).max_by_key(|&i| w[i])?;
        w[i] -= 1;
        over -= 1;
    }
    let mut rem = budget - w.iter().sum::<usize>();
    while rem > 0 {
        let i = (0..n)
            .filter(|&i| w[i] < ideal[i])
            .max_by_key(|&i| ideal[i] - w[i])?;
        w[i] += 1;
        rem -= 1;
    }
    Some(w)
}

/// Split a visible line into words (ANSI escapes stay attached to the styled
/// run they precede) so wrapping never tears an escape sequence in half and
/// a moved word keeps its styling.  Runs of spaces collapse to one.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_esc = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_esc = true;
            cur.push(c);
        } else if in_esc {
            cur.push(c);
            if c == 'm' {
                in_esc = false;
            }
        } else if c == ' ' || c == '\t' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn wrap_line(s: &str, limit: usize) -> Vec<String> {
    let words = split_words(s);
    if words.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for word in &words {
        push_word(word, limit, &mut out, &mut line, &mut used);
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Greedily pack `word` onto `line` (currently `used` columns wide),
/// completing lines into `out`.  A word wider than `limit` is hard-broken
/// across its own lines.
fn push_word(
    word: &str,
    limit: usize,
    out: &mut Vec<String>,
    line: &mut String,
    used: &mut usize,
) {
    let w = display_width(word);
    if *used == 0 {
        if w <= limit {
            line.push_str(word);
            *used = w;
        } else {
            for p in hard_break(word, limit) {
                out.push(p);
            }
        }
        return;
    }
    if *used + 1 + w <= limit {
        line.push(' ');
        line.push_str(word);
        *used += 1 + w;
    } else {
        out.push(std::mem::take(line));
        if w <= limit {
            line.push_str(word);
            *used = w;
        } else {
            for p in hard_break(word, limit) {
                out.push(p);
            }
            *used = 0;
        }
    }
}

/// Break `word` (escapes plus visible text) into lines of at most `limit`
/// display columns.  Escapes are emitted whole and in order, so a style
/// crossing a break stays open on the next line — which is what the
/// terminal already renders for the unbroken text.
fn hard_break(word: &str, limit: usize) -> Vec<String> {
    // Re-scan into ordered (escapes, visible text) pieces; a break may only
    // ever land between visible characters, never inside an escape.
    let mut pieces: Vec<(String, String)> = vec![(String::new(), String::new())];
    let mut in_esc = false;
    for c in word.chars() {
        if c == '\x1b' {
            in_esc = true;
            pieces.last_mut().unwrap().0.push(c);
        } else if in_esc {
            pieces.last_mut().unwrap().0.push(c);
            if c == 'm' {
                in_esc = false;
                pieces.push((String::new(), String::new()));
            }
        } else {
            pieces.last_mut().unwrap().1.push(c);
        }
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for (esc, text) in pieces {
        line.push_str(&esc);
        for c in text.chars() {
            let cw = c.width().unwrap_or(0);
            if used + cw > limit && used > 0 {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += cw;
        }
    }
    if out.is_empty() || display_width(&line) > 0 {
        out.push(line);
    } else if !line.is_empty() {
        // A trailing escape with no visible text (a lone RESET) merges into
        // the previous line instead of becoming a phantom empty row.
        out.last_mut()
            .map(|last| last.push_str(&line))
            .unwrap_or_else(|| out.push(line));
    }
    out
}
/// Strip ANSI escape sequences for width calculation.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape && c == 'm' {
            in_escape = false;
        } else if !in_escape {
            out.push(c);
        }
    }
    out
}

/// Terminal cell count of `s` after dropping ANSI escapes: wide/CJK/emoji
/// glyphs (e.g. `✅`, `中`) occupy two columns, not one.  This is what keeps
/// box/table borders aligned — `chars().count()` undercounts wide glyphs and
/// shifts the right `│` border one cell off on the affected lines.
fn display_width(s: &str) -> usize {
    strip_ansi(s).width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_not_a_tty() {
        let md = "# Title\n**bold** and `code`";
        assert_eq!(render_terminal(md, &crate::ui::skin::SOLARIS), md);
    }

    #[test]
    fn inline_styling_contains_escapes() {
        let gold = code_color(&crate::ui::skin::SOLARIS);
        assert!(!gold.is_empty());
        assert!(gold.contains("\x1b[38;2;"));
    }

    #[test]
    fn table_has_all_four_border_types() {
        let rows = vec![
            vec!["Feature".to_string(), "Description".to_string()],
            vec!["Variable".to_string(), "Declares".to_string()],
        ];
        let rendered = render_table(&rows, &[0], None);
        assert!(rendered.contains('┌'), "missing top-left");
        assert!(rendered.contains('┐'), "missing top-right");
        assert!(rendered.contains('├'), "missing header-left separator");
        assert!(rendered.contains('┤'), "missing header-right separator");
        assert!(rendered.contains('└'), "missing bottom-left");
        assert!(rendered.contains('┘'), "missing bottom-right");
        assert!(rendered.contains('│'), "missing vertical bar");
        assert!(!rendered.contains('|'), "table should not contain literal | pipes");
    }

    #[test]
    fn render_code_block_scales_to_content() {
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        let gold = code_color(&crate::ui::skin::SOLARIS);

        let code = "fn main() {\n    println!(\"hi\");\n}";
        let rendered = render_code_block(code, "rust", &ss, theme, &gold, None);
        assert!(rendered.contains('┌'));
        assert!(rendered.contains('└'));
        assert!(rendered.contains('│'));
        assert!(!rendered.contains("────────────────────────────────────────┐"));
    }

    #[test]
    fn code_block_box_aligns_with_wide_glyphs() {
        // A code block whose comments contain a wide (2-cell) ✅ glyph: every
        // rendered row — top/bottom borders AND every content line — must have
        // the same display width, so the right `│` border lines up.  With the
        // old chars().count() sizing, ✅ lines rendered one cell too wide and
        // the right border stuck out.
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        let gold = code_color(&crate::ui::skin::SOLARIS);

        let code = "fn shout(text: &str) {\n\
                    |    println!(\"{}\", text.to_uppercase());\n\
                    |}\n\
                    |\n\
                    |shout(\"hi\");              // ✅ literal\n\
                    |shout(&my_string);        // ✅ &String auto-derefs to &str";
        let rendered = render_code_block(code, "rust", &ss, theme, &gold, None);

        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(widths.len() >= 3, "expected a bordered box: {rendered:?}");
        assert_eq!(
            widths.iter().max(),
            widths.iter().min(),
            "box rows must all be the same display width: {rendered:?} widths={widths:?}"
        );
    }

    #[test]
    fn code_block_with_a_huge_single_line_wraps_to_the_terminal_width() {
        // A one-line code block the length of a pasted command used to draw a
        // box spanning the whole terminal.  With a known width the box must be
        // capped to it and the line wrapped, so every row fits on-screen.
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = &ts.themes["base16-ocean.dark"];
        let gold = code_color(&crate::ui::skin::SOLARIS);

        let long = "grace --remember \"On 2026-08-20 the model was observed fabricating tool-call results as plain text when prompted for repeated tool calls; verify filesystem claims with a fresh call.\"";
        assert!(display_width(long) > 80, "fixture must exceed the cap");
        let rendered = render_code_block(long, "sh", &ss, theme, &gold, Some(80));

        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(!widths.is_empty(), "expected a wrapped box: {rendered:?}");
        assert_eq!(
            widths.iter().max(),
            widths.iter().min(),
            "wrapped box rows must all match: {rendered:?} widths={widths:?}"
        );
        assert!(
            widths[0] <= 80,
            "box must not exceed the terminal width: {} > 80 ({rendered:?})",
            widths[0]
        );
        assert!(
            rendered.lines().count() > 3,
            "a wrapped single line must produce extra content rows: {rendered:?}"
        );
    }

    #[test]
    fn table_aligns_with_wide_glyphs() {
        // Same invariant for tables: a cell containing a wide glyph must not
        // shift its column's right border.
        let rows = vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["✅ ok".to_string(), "plain".to_string()],
        ];
        let rendered = render_table(&rows, &[0], None);
        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(widths.len() >= 4, "expected a bordered table: {rendered:?}");
        assert_eq!(
            widths.iter().max(),
            widths.iter().min(),
            "table rows must all be the same display width: {rendered:?} widths={widths:?}"
        );
    }

    #[test]
    fn wide_table_wraps_to_terminal_width() {
        // The failure the fix targets: a table whose natural width far
        // exceeds the terminal must render as a box whose every row fits the
        // width, with the wide cell's text wrapped, not soft-wrapped by the
        // terminal.
        let rows = vec![
            vec![
                "#".to_string(),
                "Skill".to_string(),
                "Result".to_string(),
                "Evidence".to_string(),
            ],
            vec![
                "1".to_string(),
                "slec-tdd".to_string(),
                "✅".to_string(),
                "Triage of obs_novel sanity log: 273 lines, one Output-Map falsified in transaction 2 (SEQ-FBS). Verdict: genuine falsification — this is the golden-run failure, not an environment issue."
                    .to_string(),
            ],
        ];
        let natural = render_table(&rows, &[0], None);
        let natural_w = natural.lines().map(display_width).max().unwrap_or(0);
        let rendered = render_table(&rows, &[0], Some(80));
        // It was wide, and now it is not.
        assert!(natural_w > 80, "test table must overflow 80 cols: {natural_w}");
        // `w > 0`: the render ends with the frame's closing RESET on its own
        // (zero-width) "line" — an artifact of the shared border pattern, not a
        // misaligned row.
        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(
            widths.iter().all(|&w| w == 80),
            "every box row must be exactly the terminal width: {rendered:?} widths={widths:?}"
        );
        // Nothing is lost — the cell's words survive the wrap.  "slec-tdd"
        // lands in the 2-wide Skill column and hard-breaks into 2-char
        // pieces, so check those pieces char-for-char.
        let plain = strip_ansi(&rendered);
        for word in ["Triage", "273", "lines", "SEQ-FBS", "golden-run", "sl", "ec", "-t", "dd", "✅"] {
            assert!(plain.contains(word), "missing {word:?} in: {plain:?}");
        }
    }

    #[test]
    fn wide_table_wraps_long_unbroken_words() {
        // A single 100-char token cannot word-break; it must hard-break
        // without losing characters or misaligning the box.
        let rows = vec![
            vec!["col".to_string()],
            vec!["x".repeat(100)],
        ];
        let rendered = render_table(&rows, &[0], Some(60));
        // `w > 0`: the render ends with the frame's closing RESET on its own
        // (zero-width) "line" — an artifact of the shared border pattern, not a
        // misaligned row.
        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(
            widths.iter().all(|&w| w == 60),
            "hard-broken word must keep rows aligned: {rendered:?} widths={widths:?}"
        );
        let plain = strip_ansi(&rendered);
        assert_eq!(
            plain.matches('x').count(),
            100,
            "no character may be lost: {plain:?}"
        );
    }

    #[test]
    fn wrapped_table_stays_aligned_with_wide_glyphs() {
        // CJK text is two columns per character; wrapping must count that or
        // the right border drifts on the affected rows.
        let rows = vec![
            vec!["列".to_string(), "note".to_string()],
            vec!["数据".to_string(), "甲乙丙丁戊己庚辛壬癸子丑寅卯辰巳".repeat(8)],
        ];
        let rendered = render_table(&rows, &[0], Some(40));
        // `w > 0`: the render ends with the frame's closing RESET on its own
        // (zero-width) "line" — an artifact of the shared border pattern, not a
        // misaligned row.
        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(
            widths.iter().all(|&w| w == 40),
            "CJK rows must not shift the border: {rendered:?} widths={widths:?}"
        );
        let plain = strip_ansi(&rendered);
        assert_eq!(
            plain.matches('甲').count(),
            8,
            "no wide glyph may be lost: {plain:?}"
        );
    }

    #[test]
    fn narrow_table_output_is_byte_identical_with_and_without_width() {
        // A table that already fits must render byte-identically whether or
        // not a width is given — the fit path must not perturb small tables.
        let rows = vec![
            vec!["Feature".to_string(), "Description".to_string()],
            vec!["Variable".to_string(), "Declares".to_string()],
        ];
        assert_eq!(
            render_table(&rows, &[0], None),
            render_table(&rows, &[0], Some(200)),
            "fitting a table that fits must change nothing"
        );
    }

    #[test]
    fn too_narrow_falls_back_to_natural_width() {
        // Fewer columns of budget than floors (one per column, two for any
        // wide-content column): no fit can keep rows aligned, so the table
        // renders at natural width instead of mangling itself.
        let rows = vec![
            vec!["a".to_string(), "bb ✅".to_string()],
            vec!["c d".to_string(), "e f".to_string()],
        ];
        // ncols=2 needs 3*2+1 = 7 minimum; width 7 leaves 0 budget.
        let rendered = render_table(&rows, &[0], Some(7));
        assert_eq!(
            rendered,
            render_table(&rows, &[0], None),
            "unfit widths fall back to the natural render"
        );
    }

    #[test]
    fn fit_widths_sums_to_budget_and_respects_bounds() {
        let ideal = vec![1, 27, 6, 290];
        // The caller only shrinks (natural box already overflowed), so
        // budget < Σideal (= 324) — test the full range of that contract.
        for budget in 8..=323 {
            let fit = fit_widths(&ideal, budget).expect("fit expected");
            assert!(
                fit.iter().sum::<usize>() == budget,
                "budget {budget}: {fit:?}"
            );
            for (i, &w) in fit.iter().enumerate() {
                assert!(w >= 1, "column floor: {fit:?}");
                if ideal[i] >= 2 {
                    assert!(w >= 2, "wide-glyph floor: budget {budget}: {fit:?}");
                }
                assert!(w <= ideal[i].max(1), "no column grows: {fit:?}");
            }
            // The wide column (ideal 290) must get the most room — it is the
            // one the user actually reads.
            assert!(fit[3] >= fit[1] && fit[3] >= fit[2] && fit[3] >= fit[0]);
        }
        assert_eq!(fit_widths(&[0, 0], 1), None, "no room, no fit");
        assert_eq!(fit_widths(&[2, 2, 2, 2], 3), None, "floors don't fit");
        // Exact budget for the ideal widths is the identity layout.
        assert_eq!(fit_widths(&[3, 4], 7), Some(vec![3, 4]));
    }

    #[test]
    fn terminal_width_prefers_tty_falls_back_to_columns() {
        // Sequential scopes: each guard owns ENV_LOCK until it drops, so a
        // second guard in the same scope would wait on the first forever.
        {
            let _guard = crate::util::test_support::EnvVarGuard::set("COLUMNS", "117");
            match terminal_size::terminal_size() {
                Some((c, _)) => {
                    // stdout is a real TTY (e.g. `cargo test --nocapture`):
                    // the live size wins over COLUMNS.
                    assert_eq!(terminal_width(), Some(c.0 as usize));
                }
                None => {
                    // Piped stdout: COLUMNS is the width source.
                    assert_eq!(terminal_width(), Some(117));
                }
            }
        }
        {
            let _guard = crate::util::test_support::EnvVarGuard::set("COLUMNS", "bogus");
            assert_eq!(columns_env_width(), None, "unparseable COLUMNS is no width");
        }
    }

    #[test]
    fn wrapped_rows_keep_inline_styling_intact() {
        // A gold inline-code word pushed onto a second line must keep its
        // escapes attached: the styled run reassembles and stays balanced.
        let code = format!("{}\x1b[38;2;210;153;34m{}\x1b[0m", "", "abcdefg".repeat(10));
        let rows = vec![vec![code]];
        let rendered = render_table(&rows, &[], Some(30));
        let plain = strip_ansi(&rendered);
        assert_eq!(plain.matches('a').count(), 10, "run must survive: {plain:?}");
        // Every 24-bit color open is closed within the rendered table.
        let opens = rendered.matches("\x1b[38;2;210;153;34m").count();
        let closes = rendered.matches(RESET).count();
        assert!(closes >= opens, "unbalanced escapes: {rendered:?}");
        // `w > 0`: the render ends with the frame's closing RESET on its own
        // (zero-width) "line" — an artifact of the shared border pattern, not a
        // misaligned row.
        let widths: Vec<usize> = rendered
            .lines()
            .map(display_width)
            .filter(|&w| w > 0)
            .collect();
        assert!(
            widths.iter().all(|&w| w == 30),
            "styled wrap must stay aligned: {rendered:?} widths={widths:?}"
        );
    }

    #[test]
    fn ensure_blank_adds_correct_newlines() {
        let mut s = String::from("hello\n");
        ensure_blank(&mut s, 2);
        assert_eq!(s, "hello\n\n");
    }

    #[test]
    fn ensure_blank_no_double_up() {
        let mut s = String::from("hello\n\n\n");
        ensure_blank(&mut s, 2);
        assert_eq!(s, "hello\n\n\n");
    }

    #[test]
    fn ensure_blank_empty_is_noop() {
        let mut s = String::new();
        ensure_blank(&mut s, 2);
        assert_eq!(s, "");
    }
}
