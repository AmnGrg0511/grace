//! Unified-diff snippets for tool output (the `patch` tool's terminal view).
//!
//! Uses `similar` (Myers diff, same engine class as `git diff`/`ruff`) instead
//! of hand-rolling LCS — a real diff snippet is worth a small, well-maintained
//! dependency rather than reinventing sequence alignment.

use similar::{ChangeTag, TextDiff};

/// Render a compact unified-style diff between `old` and `new`, capped to
/// `context` lines of unchanged context around each change. Colored when
/// stdout is a real terminal (green `+`, red `-`); plain `+`/`-` prefixes
/// otherwise so piped output/logs stay diffable.
pub fn unified_snippet(old: &str, new: &str, context: usize) -> String {
    use owo_colors::OwoColorize;
    use std::io::IsTerminal;

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
                    match change.tag() {
                        ChangeTag::Delete => {
                            out.push_str(&format!("{}\n", format!("-{line}").red()))
                        }
                        ChangeTag::Insert => {
                            out.push_str(&format!("{}\n", format!("+{line}").green()))
                        }
                        ChangeTag::Equal => {
                            out.push_str(&format!("{}\n", format!(" {line}").dimmed()))
                        }
                    }
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
