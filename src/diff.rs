use anstyle::{Color, RgbColor, Style};
use similar::{ChangeTag, TextDiff};
use std::io::IsTerminal;

/// Render a compact unified-style diff between `old` and `new`, capped to
/// `context` lines of unchanged context around each change. Colored when
/// stdout is a real terminal (green `+`, red `-`); plain `+`/`-` prefixes
/// otherwise so piped output/logs stay diffable.
pub fn unified_snippet(old: &str, new: &str, context: usize) -> String {
    let color = std::io::stdout().is_terminal();
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for group in diff.grouped_ops(context) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let (sign, line) = match change.tag() {
                    ChangeTag::Delete => ("-", change.to_string()),
                    ChangeTag::Insert => ("+", change.to_string()),
                    ChangeTag::Equal => (" ", change.to_string()),
                };
                let line = line.trim_end_matches('\n');
                if color {
                    let style = match change.tag() {
                        ChangeTag::Delete => Style::new().fg_color(Some(Color::from(RgbColor(255, 100, 100)))),
                        ChangeTag::Insert => Style::new().fg_color(Some(Color::from(RgbColor(100, 255, 100)))),
                        ChangeTag::Equal => Style::new().fg_color(Some(Color::from(RgbColor(150, 150, 150)))),
                    };
                    let reset = Style::new().render();
                    out.push_str(&format!("{}{}{}{}\n", style.render(), sign, line, reset));
                } else {
                    out.push_str(&format!("{sign}{line}\n"));
                }
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_additions_and_removals() {
        let old = "line one\nline two\nline three\n";
        let new = "line one\nline TWO\nline three\n";
        let snippet = unified_snippet(old, new, 1);
        assert!(snippet.contains("line two") || snippet.contains("line TWO"));
    }
}
