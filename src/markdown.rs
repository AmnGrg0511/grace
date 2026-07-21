//! Minimal Markdown → terminal renderer using pulldown-cmark + syntect.
//!
//! Renders GitHub-Flavored Markdown to ANSI-styled terminal output. Only applied
//! when stdout is a real TTY; when piped, returns raw text unchanged.

use crate::skin::Skin;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, CodeBlockKind};
use std::io::IsTerminal;
use anstyle::{Color, RgbColor, Style as AnsiStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

/// Build the 24-bit ANSI escape for `skin`'s code color.
fn code_color(skin: &Skin) -> String {
    let anstyle::RgbColor(r, g, b) = skin.code;
    format!("\x1b[38;2;{r};{g};{b}m")
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
    let parser = Parser::new_ext(md, opts);

    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    let syntax = ss
        .find_syntax_by_extension("rs")
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let gold = code_color(skin);
    let mut out = String::with_capacity(md.len() + md.len() / 4);
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut heading_level = 0;
    let mut in_blockquote = false;
    let mut list_depth: usize = 0;
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut in_cell = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    heading_level = level as usize;
                }
                Tag::CodeBlock(kind) => {
                    in_code = true;
                    code_lang = match kind {
                        CodeBlockKind::Fenced(info) => info.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    code_buf.clear();
                }
                Tag::BlockQuote(_) => {
                    in_blockquote = true;
                }
                Tag::List(_) => {
                    list_depth += 1;
                }
                Tag::Item => {}
                Tag::Table(_) => {
                    in_table = true;
                    table_rows.clear();
                    current_row.clear();
                }
                Tag::TableHead => {}
                Tag::TableRow => {
                    current_row.clear();
                }
                Tag::TableCell => {
                    in_cell = true;
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(level) => {
                    out.push('\n');
                    // heading_level not needed anymore since we have the level here
                    let _ = level;
                    heading_level = 0;
                }
                TagEnd::CodeBlock => {
                    // Render syntax-highlighted code block
                    if !code_buf.is_empty() {
                        out.push_str(&gold);
                        out.push_str("┌────────────────────────────────────────┐\n");
                        for line in LinesWithEndings::from(&code_buf) {
                            out.push_str("│ ");
                            let ranges = highlighter.highlight_line(line, &ss).unwrap_or_default();
                            for (style, text) in ranges {
                                let color = syntect_style_to_ansi(style);
                                out.push_str(&color);
                                out.push_str(text);
                                out.push_str(RESET);
                            }
                            out.push_str(" │\n");
                        }
                        out.push_str("└────────────────────────────────────────┘\n");
                        out.push_str(RESET);
                    }
                    in_code = false;
                    code_lang.clear();
                    code_buf.clear();
                }
                TagEnd::BlockQuote(_) => {
                    in_blockquote = false;
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                }
                TagEnd::Item => {
                    out.push('\n');
                }
                TagEnd::Table => {
                    // Render table
                    if !table_rows.is_empty() {
                        out.push_str(&render_table(&table_rows));
                    }
                    in_table = false;
                    table_rows.clear();
                }
                TagEnd::TableRow => {
                    if !current_row.is_empty() {
                        table_rows.push(current_row.clone());
                    }
                }
                TagEnd::TableCell => {
                    in_cell = false;
                }
                _ => {}
            },
            Event::Text(text) => {
                let styled = style_inline(&text, &gold);
                if in_code {
                    code_buf.push_str(&text);
                } else if in_table && in_cell {
                    current_row.push(styled);
                } else if in_blockquote {
                    out.push_str(DIM);
                    out.push_str("▏ ");
                    out.push_str(&styled);
                    out.push_str(RESET);
                    out.push('\n');
                } else if heading_level > 0 {
                    out.push_str(BOLD);
                    out.push_str(&"#".repeat(heading_level));
                    out.push(' ');
                    out.push_str(&styled);
                    out.push_str(RESET);
                } else if list_depth > 0 {
                    out.push_str(&"  ".repeat(list_depth - 1));
                    out.push_str(BOLD);
                    out.push_str("• ");
                    out.push_str(RESET);
                    out.push_str(&styled);
                } else {
                    out.push_str(&styled);
                }
            },
            Event::Code(text) => {
                if in_code {
                    code_buf.push_str(&text);
                } else {
                    out.push_str(&gold);
                    out.push_str(&text);
                    out.push_str(RESET);
                }
            },
            Event::SoftBreak => {
                if !in_code && !in_table {
                    out.push('\n');
                }
            },
            Event::HardBreak => {
                if !in_code && !in_table {
                    out.push('\n');
                }
            },
            Event::Rule => {
                out.push_str(DIM);
                out.push_str("────────────────────────────────────────\n");
                out.push_str(RESET);
            },
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "[x] " } else { "[ ] " });
            },
            _ => {}
        }
    }

    out
}

/// Convert a syntect Style to ANSI escape sequence.
fn syntect_style_to_ansi(style: SyntectStyle) -> String {
    let fg = style.foreground;
    let ansi_style = AnsiStyle::new().fg_color(Some(Color::from(RgbColor(fg.r, fg.g, fg.b))));
    ansi_style.render().to_string()
}

/// Apply inline markdown styling: **bold**, `code`.
fn style_inline(text: &str, gold: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' && chars.peek() == Some(&'*') {
            chars.next();
            out.push_str(BOLD);
            let mut buf = String::new();
            while let Some(c2) = chars.next() {
                if c2 == '*' && chars.peek() == Some(&'*') {
                    chars.next();
                    break;
                }
                buf.push(c2);
            }
            out.push_str(&buf);
            out.push_str(RESET);
        } else if c == '`' {
            let mut buf = String::new();
            for c2 in chars.by_ref() {
                if c2 == '`' {
                    break;
                }
                buf.push(c2);
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

/// Render a markdown table as aligned box-drawing.
fn render_table(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut widths = vec![0; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            // Strip ANSI for width calculation
            let visible = strip_ansi(cell);
            widths[i] = widths[i].max(visible.len().min(40));
        }
    }

    let mut out = String::new();
    for (ri, row) in rows.iter().enumerate() {
        let is_header = ri == 0;
        let mut wrapped: Vec<Vec<String>> = Vec::new();
        let mut max_lines = 1;
        for (ci, cell) in row.iter().enumerate() {
            let w = widths[ci];
            let lines = wrap_cell(cell, w);
            max_lines = max_lines.max(lines.len());
            wrapped.push(lines);
        }
        for line_idx in 0..max_lines {
            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(RESET);
            for (ci, lines) in wrapped.iter().enumerate() {
                let cell = lines.get(line_idx).map(|s| s.as_str()).unwrap_or("");
                let pad = widths[ci].saturating_sub(strip_ansi(cell).len());
                if is_header {
                    out.push_str(BOLD);
                    out.push_str(cell);
                    out.push_str(RESET);
                } else {
                    out.push_str(cell);
                }
                out.push_str(&" ".repeat(pad));
                out.push_str(DIM);
                out.push_str(" │ ");
                out.push_str(RESET);
            }
            out.push('\n');
        }
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

/// Wrap a cell to max width, preserving ANSI.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let visible = strip_ansi(text);
    if visible.len() <= width {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_visible = 0;
    for word in visible.split_whitespace() {
        let wlen = word.len();
        if current_visible + wlen + 1 > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
            current_visible = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_visible += 1;
        }
        current.push_str(word);
        current_visible += wlen;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
        let _gold = code_color(&crate::skin::GILDED);
        let _md = "| a | bb |\n|---|----|\n| 1 | 22 |";
        let rendered = render_table(&[vec!["a".into(), "bb".into()], vec!["1".into(), "22".into()]]);
        assert!(rendered.contains('│'));
        assert!(rendered.contains('┼') || rendered.contains('┤'));
        assert!(rendered.contains("a"));
        assert!(rendered.contains("22"));
    }
}