//! Minimal Markdown → terminal renderer using pulldown-cmark + syntect.
//!
//! Renders GitHub-Flavored Markdown to ANSI-styled terminal output. Only applied
//! when stdout is a real TTY; when piped, returns raw text unchanged.

use crate::skin::Skin;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::io::IsTerminal;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

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
    if !std::io::stdout().is_terminal() {
        return md.to_string();
    }

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(md, opts);

    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let theme = &ts.themes["base16-ocean.dark"];

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
                Tag::Strong => {
                    strong_stack += 1;
                    out.push_str(BOLD);
                }
                Tag::Emphasis => {
                    em_stack += 1;
                    out.push_str(ITALIC);
                }
                Tag::Link { .. } => {
                    _in_link = true;
                    out.push_str(UNDERLINE);
                }
                Tag::Table(_) => {
                    ensure_blank(&mut out, 1);
                    table_rows.clear();
                    current_row.clear();
                }
                Tag::TableHead => {}
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
                        out.push_str(&render_code_block(&code_buf, &code_lang, &ss, theme, &gold));
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
                TagEnd::Strong => {
                    strong_stack = strong_stack.saturating_sub(1);
                    out.push_str(RESET);
                }
                TagEnd::Emphasis => {
                    em_stack = em_stack.saturating_sub(1);
                    out.push_str(RESET);
                }
                TagEnd::Link => {
                    _in_link = false;
                    out.push_str(RESET);
                }
                TagEnd::Table => {
                    if !table_rows.is_empty() {
                        out.push_str(&render_table(&table_rows));
                    }
                    table_rows.clear();
                }
                TagEnd::TableRow => {
                    if !current_row.is_empty() {
                        table_rows.push(current_row.clone());
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
                if in_code {
                    code_buf.push_str(&text);
                } else if in_cell {
                    cell_buf.push_str(&text);
                } else if in_blockquote {
                    if bq_needs_prefix {
                        out.push_str(DIM);
                        out.push_str("▏ ");
                        out.push_str(RESET);
                        bq_needs_prefix = false;
                    }
                    out.push_str(DIM);
                    out.push_str(&text);
                    out.push_str(RESET);
                } else if heading_level > 0 {
                    out.push_str(&heading_color);
                    out.push_str(BOLD);
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
                    out.push_str(&text);
                } else if !list_stack.is_empty() {
                    // Continuation text in a list item
                    out.push_str(&text);
                } else {
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

    // Trim trailing whitespace but keep content
    let trimmed = out.trim_end_matches('\n').to_string();
    format!("{trimmed}\n")
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
fn render_code_block(
    code: &str,
    lang: &str,
    ss: &SyntaxSet,
    theme: &syntect::highlighting::Theme,
    _gold: &str,
) -> String {
    let syntax = ss
        .find_syntax_by_token(lang.trim())
        .or_else(|| ss.find_syntax_by_extension("rs"))
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let lines: Vec<&str> = code.lines().collect();
    let max_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let box_width = max_len.max(20) + 2;

    let mut out = String::new();

    out.push_str(DIM);
    out.push('┌');
    out.push_str(&"─".repeat(box_width));
    out.push('┐');
    out.push('\n');
    out.push_str(RESET);

    for line in &lines {
        let ranges = highlighter.highlight_line(line, ss).unwrap_or_default();
        let visible_len: usize = ranges.iter().map(|(_, t)| t.chars().count()).sum();
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

/// Render a markdown table as aligned box-drawing.
fn render_table(rows: &[Vec<String>]) -> String {
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
            let visible = strip_ansi(cell);
            widths[i] = widths[i].max(visible.chars().count());
        }
    }

    let mut out = String::new();

    // Top border: ┌─────┬─────┐
    out.push_str(DIM);
    out.push('┌');
    for (ci, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if ci + 1 == ncols { '┐' } else { '┬' });
    }
    out.push('\n');
    out.push_str(RESET);

    for (ri, row) in rows.iter().enumerate() {
        let is_header = ri == 0;

        out.push_str(DIM);
        out.push_str("│ ");
        out.push_str(RESET);
        for (ci, cell) in row.iter().enumerate() {
            let visible = strip_ansi(cell);
            let pad = widths[ci].saturating_sub(visible.chars().count());
            if is_header {
                out.push_str(BOLD);
                out.push_str(cell);
                out.push_str(RESET);
            } else {
                out.push_str(cell);
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

        if is_header {
            out.push_str(DIM);
            out.push('├');
            for (ci, w) in widths.iter().enumerate() {
                out.push_str(&"─".repeat(w + 2));
                out.push(if ci + 1 == ncols { '┤' } else { '┼' });
            }
            out.push('\n');
            out.push_str(RESET);
        }
    }

    // Bottom border: └─────┴─────┘
    out.push_str(DIM);
    out.push('└');
    for (ci, w) in widths.iter().enumerate() {
        out.push_str(&"─".repeat(w + 2));
        out.push(if ci + 1 == ncols { '┘' } else { '┴' });
    }
    out.push('\n');
    out.push_str(RESET);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_not_a_tty() {
        let md = "# Title\n**bold** and `code`";
        assert_eq!(render_terminal(md, &crate::skin::SOLARIS), md);
    }

    #[test]
    fn inline_styling_contains_escapes() {
        let gold = code_color(&crate::skin::SOLARIS);
        assert!(!gold.is_empty());
        assert!(gold.contains("\x1b[38;2;"));
    }

    #[test]
    fn table_has_all_four_border_types() {
        let rows = vec![
            vec!["Feature".to_string(), "Description".to_string()],
            vec!["Variable".to_string(), "Declares".to_string()],
        ];
        let rendered = render_table(&rows);
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
        let gold = code_color(&crate::skin::SOLARIS);

        let code = "fn main() {\n    println!(\"hi\");\n}";
        let rendered = render_code_block(code, "rust", &ss, theme, &gold);
        assert!(rendered.contains('┌'));
        assert!(rendered.contains('└'));
        assert!(rendered.contains('│'));
        assert!(!rendered.contains("────────────────────────────────────────┐"));
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
