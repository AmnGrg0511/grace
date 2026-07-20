//! Minimal Markdown → terminal renderer (std-only, no dependencies).
//!
//! The agent loop returns the model's reply as raw text, which may contain
//! Markdown. A plain terminal cannot render it, so this module turns the
//! common Markdown constructs into ANSI-styled terminal output. It is
//! deliberately tiny: headings, bold, inline code, fenced code blocks, lists,
//! and block quotes. Anything unrecognized passes through verbatim.
//!
//! Rendering is only applied when stdout is a real terminal; when output is
//! piped (non-TTY), [`render_terminal`] returns the input unchanged so logs
//! and scripts stay clean.

use crate::skin::Skin;
use std::io::IsTerminal;
use unicode_width::UnicodeWidthStr;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_BLUE: &str = "\x1b[1;94m";

/// Build the 24-bit ANSI escape for `skin`'s code color.
fn code_color(skin: &Skin) -> String {
    let owo_colors::Rgb(r, g, b) = skin.code;
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Render `md` to terminal-friendly ANSI text if stdout is a TTY; otherwise
/// return it unchanged.
pub fn render_terminal(md: &str, skin: &Skin) -> String {
    if !std::io::stdout().is_terminal() {
        return md.to_string();
    }
    let gold = code_color(skin);
    let mut out = String::with_capacity(md.len() + md.len() / 4);
    let mut in_code = false;
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push_str(DIM);
            out.push_str("────────────────\n");
            out.push_str(RESET);
            i += 1;
            continue;
        }
        if in_code {
            out.push_str(&gold);
            out.push_str("│ ");
            out.push_str(line);
            out.push('\n');
            out.push_str(RESET);
            i += 1;
            continue;
        }
        // Table: a header row `| a | b |` followed by a separator row of
        // `|---|:--:|` etc. Collect the whole contiguous block and render
        // it as aligned columns with box-drawing, instead of raw pipes.
        if is_table_row(trimmed)
            && lines
                .get(i + 1)
                .is_some_and(|l| is_table_separator(l.trim_start()))
        {
            let mut block = vec![trimmed];
            let mut j = i + 2;
            while j < lines.len() && is_table_row(lines[j].trim_start()) {
                block.push(lines[j].trim_start());
                j += 1;
            }
            out.push_str(&render_table(&block, &gold));
            i = j;
            continue;
        }
        // Block quote.
        if let Some(rest) = trimmed.strip_prefix("> ") {
            out.push_str(DIM);
            out.push_str("▏ ");
            out.push_str(&style_inline(rest, &gold));
            out.push('\n');
            out.push_str(RESET);
            i += 1;
            continue;
        }
        // Headings.
        if let Some(rest) = trimmed.strip_prefix("### ") {
            out.push_str(BOLD);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            out.push_str(BOLD_BLUE);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            out.push_str(BOLD_BLUE);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            i += 1;
            continue;
        }
        // Unordered list item.
        if let Some(rest) = trimmed.strip_prefix("- ") {
            out.push_str(BOLD_CYAN);
            out.push_str("• ");
            out.push_str(RESET);
            out.push_str(&style_inline(rest, &gold));
            out.push('\n');
            i += 1;
            continue;
        }
        // Plain paragraph (may contain inline markup).
        out.push_str(&style_inline(line, &gold));
        out.push('\n');
        i += 1;
    }
    out
}

/// A markdown table row: starts and ends with `|` (after trimming).
fn is_table_row(line: &str) -> bool {
    line.starts_with('|') && line.trim_end().ends_with('|') && line.len() > 1
}

/// A markdown table separator row: `|---|:--:|---:|` — only `-`, `:`, `|`,
/// whitespace.
fn is_table_separator(line: &str) -> bool {
    is_table_row(line)
        && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
        && line.contains('-')
}

/// Split a table row into trimmed cell contents (drops the leading/trailing
/// empty cells produced by the outer `|`s).
fn split_row(row: &str) -> Vec<String> {
    let inner = row.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Max characters for any single table cell before we wrap it onto extra
/// lines within the same row. Keeps wide description columns from making
/// the whole table wider than a normal terminal, which is what breaks the
/// box-drawing visually (the terminal itself wraps mid-row).
const MAX_CELL_WIDTH: usize = 40;

/// Visible *terminal column* width of `s` after inline markdown
/// (`**bold**`, `` `code` ``) is stripped — what the reader actually sees
/// once rendered. Column-width math MUST use this, not `.chars().count()`:
/// (1) `style_inline` drops the `**`/backtick delimiter chars, so counting
/// them pads a cell too wide; (2) CJK/emoji occupy 2 terminal columns per
/// char while combining marks occupy 0 — `chars().count()` treats every
/// char as width 1, which is exactly the second breakage reported (wide
/// Unicode desyncing tables again after the markup fix). `unicode-width`
/// (used by rustc/ripgrep for this exact problem) gives the real column
/// count instead of us hand-rolling an East-Asian-width table.
fn visible_width(s: &str) -> usize {
    strip_inline_markup(s).width()
}

/// Strip `**bold**`/`` `code` `` delimiters, keeping only what prints.
fn strip_inline_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(&x) = chars.peek() {
                if x == '*' {
                    chars.next();
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        break;
                    }
                    out.push('*');
                } else {
                    out.push(x);
                    chars.next();
                }
            }
        } else if c == '`' {
            while let Some(&x) = chars.peek() {
                if x == '`' {
                    chars.next();
                    break;
                }
                out.push(x);
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Wrap `s` into lines of at most `width` chars, breaking on word
/// boundaries where possible.
fn wrap_cell(s: &str, width: usize) -> Vec<String> {
    if visible_width(s) <= width {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let extra = if cur.is_empty() { 0 } else { 1 };
        if visible_width(&cur) + extra + visible_width(word) > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Render a header row + body rows (separator already excluded) as an
/// aligned box-drawing table, column widths computed on visible chars.
fn render_table(rows: &[&str], gold: &str) -> String {
    let parsed: Vec<Vec<String>> = rows.iter().map(|r| split_row(r)).collect();
    let ncols = parsed.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; ncols];
    for row in &parsed {
        for (c, cell) in row.iter().enumerate() {
            widths[c] = widths[c].max(visible_width(cell).min(MAX_CELL_WIDTH));
        }
    }
    let mut out = String::new();
    for (r, row) in parsed.iter().enumerate() {
        // Wrap each cell in this row to its column width, then print as
        // many sub-lines as the tallest cell needs.
        let wrapped: Vec<Vec<String>> = (0..ncols)
            .map(|c| {
                let cell = row.get(c).map(|s| s.as_str()).unwrap_or("");
                wrap_cell(cell, widths[c])
            })
            .collect();
        let sub_rows = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        for sub in 0..sub_rows {
            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(RESET);
            for (c, w) in widths.iter().enumerate().take(ncols) {
                let cell = wrapped[c].get(sub).map(|s| s.as_str()).unwrap_or("");
                let pad = w.saturating_sub(visible_width(cell));
                if r == 0 {
                    out.push_str(BOLD);
                    out.push_str(cell);
                    out.push_str(RESET);
                } else {
                    out.push_str(&style_inline(cell, gold));
                }
                out.push_str(&" ".repeat(pad));
                out.push_str(DIM);
                out.push_str(" │ ");
                out.push_str(RESET);
            }
            out.push('\n');
        }
        if r == 0 {
            out.push_str(DIM);
            out.push('├');
            for (c, w) in widths.iter().enumerate() {
                out.push_str(&"─".repeat(w + 2));
                out.push(if c + 1 == ncols { '┤' } else { '┼' });
            }
            out.push('\n');
            out.push_str(RESET);
        }
    }
    out
}

/// Apply inline styling: `**bold**` and `` `code` ``.
fn style_inline(s: &str, gold: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second '*'
            let mut buf = String::new();
            while let Some(&n) = chars.peek() {
                if n == '*' {
                    chars.next();
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        break;
                    } else {
                        buf.push('*');
                    }
                } else {
                    buf.push(n);
                    chars.next();
                }
            }
            out.push_str(BOLD);
            out.push_str(&buf);
            out.push_str(RESET);
        } else if c == '`' {
            let mut buf = String::new();
            while let Some(&n) = chars.peek() {
                if n == '`' {
                    chars.next();
                    break;
                } else {
                    buf.push(n);
                    chars.next();
                }
            }
            out.push_str(gold);
            out.push_str(&buf);
            out.push_str(RESET);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_not_a_tty() {
        // In test harness stdout is piped, so rendering must be a no-op.
        let md = "# Title\n**bold** and `code`";
        assert_eq!(render_terminal(md, &crate::skin::GILDED), md);
    }

    #[test]
    fn inline_styling_contains_escapes() {
        // Force styling by simulating a TTY is hard; just check the helper.
        let gold = code_color(&crate::skin::GILDED);
        let styled = style_inline("a **b** c `d`", &gold);
        assert!(styled.contains(BOLD));
        assert!(styled.contains(&gold));
        assert!(styled.contains(RESET));
        assert!(styled.contains("b"));
        assert!(styled.contains("d"));
    }

    #[test]
    fn table_is_detected_and_rendered_as_box_drawing() {
        let gold = code_color(&crate::skin::GILDED);
        let md = "| a | bb |\n|---|----|\n| 1 | 22 |";
        let rendered = render_table(&["| a | bb |", "| 1 | 22 |"], &gold);
        assert!(rendered.contains('│'));
        assert!(rendered.contains('┼') || rendered.contains('┤'));
        assert!(rendered.contains("a"));
        assert!(rendered.contains("22"));
        // And the row/separator detectors agree it's a table.
        assert!(is_table_row("| a | bb |"));
        assert!(is_table_separator("|---|----|"));
        assert!(!is_table_row("not a table"));
        let _ = md; // documents the shape a real reply would have
    }
}
