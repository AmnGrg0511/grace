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

use std::io::IsTerminal;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const BOLD_CYAN: &str = "\x1b[1;36m";
const BOLD_BLUE: &str = "\x1b[1;94m";
const REVERSE: &str = "\x1b[7m";

/// Render `md` to terminal-friendly ANSI text if stdout is a TTY; otherwise
/// return it unchanged.
pub fn render_terminal(md: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return md.to_string();
    }
    let mut out = String::with_capacity(md.len() + md.len() / 4);
    let mut in_code = false;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push_str(DIM);
            out.push_str("────────────────\n");
            out.push_str(RESET);
            continue;
        }
        if in_code {
            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(line);
            out.push('\n');
            out.push_str(RESET);
            continue;
        }
        // Block quote.
        if let Some(rest) = trimmed.strip_prefix("> ") {
            out.push_str(DIM);
            out.push_str("▏ ");
            out.push_str(&style_inline(rest));
            out.push('\n');
            out.push_str(RESET);
            continue;
        }
        // Headings.
        if let Some(rest) = trimmed.strip_prefix("### ") {
            out.push_str(BOLD);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            out.push_str(BOLD_BLUE);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            out.push_str(BOLD_BLUE);
            out.push_str(rest);
            out.push('\n');
            out.push_str(RESET);
            continue;
        }
        // Unordered list item.
        if let Some(rest) = trimmed.strip_prefix("- ") {
            out.push_str(BOLD_CYAN);
            out.push_str("• ");
            out.push_str(RESET);
            out.push_str(&style_inline(rest));
            out.push('\n');
            continue;
        }
        // Plain paragraph (may contain inline markup).
        out.push_str(&style_inline(line));
        out.push('\n');
    }
    out
}

/// Apply inline styling: `**bold**` and `` `code` ``.
fn style_inline(s: &str) -> String {
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
            out.push_str(REVERSE);
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
        assert_eq!(render_terminal(md), md);
    }

    #[test]
    fn inline_styling_contains_escapes() {
        // Force styling by simulating a TTY is hard; just check the helper.
        let styled = style_inline("a **b** c `d`");
        assert!(styled.contains(BOLD));
        assert!(styled.contains(REVERSE));
        assert!(styled.contains(RESET));
        assert!(styled.contains("b"));
        assert!(styled.contains("d"));
    }
}
