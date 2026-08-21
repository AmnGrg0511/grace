//! Shared, dependency-light utilities used across every other module.
//!
//! Nothing here may depend on `core`, `transport`, `tools`, or `ui` — this is
//! the bottom layer of the crate, so a cycle here would be a layering bug.

pub mod diff;
pub mod error;
pub mod tokens;

#[cfg(test)]
pub mod test_support;

pub use error::{AgentError, Result};

/// Truncate `s` to at most `max` bytes, cut on a char boundary, appending a
/// `... [truncated N bytes]` marker. Text that already fits is returned
/// unchanged.
///
/// A plain `&s[..max]` panics whenever `max` lands inside a multi-byte UTF-8
/// char — provider and user text (Cyrillic, emoji, CJK) hits that routinely —
/// so every truncation site in the crate goes through this helper.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    format!("{}... [truncated {} bytes]", &s[..end], s.len() - end)
}

/// Truncate `s` to at most `max` display columns (unicode width, not bytes),
/// cutting only between characters so the result is valid UTF-8 that fits on
/// a terminal row. Zero-width characters (combining marks, control chars)
/// count nothing; wide characters count two columns.
///
/// Unlike [`truncate_utf8`], this never appends a marker — it is for fitting
/// a status line to the terminal width, where an ellipsis of unknown length
/// would defeat the purpose.
pub fn truncate_utf8_display(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            break;
        }
        out.push(c);
        width += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for any fallible crate function, so the assertions below
    /// exercise the real `Result` alias rather than a literal clippy can see
    /// through and flag as pointless.
    fn fallible(ok: bool) -> Result<u8> {
        if ok {
            Ok(7)
        } else {
            Err(AgentError::Tool("x".into()))
        }
    }

    #[test]
    fn the_error_type_and_alias_are_reachable_from_the_module_root() {
        assert_eq!(fallible(true).unwrap(), 7);
        assert_eq!(fallible(false).unwrap_err().to_string(), "tool error: x");
    }

    #[test]
    fn the_token_counter_and_diff_helpers_are_reachable() {
        use tokens::TokenCounter;
        assert!(tokens::default_counter().count_text("hello world") > 0);
        assert!(diff::unified_snippet("a", "b", 1).contains('b'));
    }

    #[test]
    fn truncate_utf8_passthrough_and_marker_shape() {
        assert_eq!(truncate_utf8("abc", 500), "abc");
        let out = truncate_utf8(&"x".repeat(600), 500);
        assert!(out.starts_with("xxxxx"));
        assert!(out.ends_with("... [truncated 100 bytes]"));
    }

    #[test]
    fn truncate_utf8_walks_back_to_a_char_boundary() {
        // 8 bytes; the 4-byte emoji spans byte 1..5, so max=2,3,4 all land
        // mid-codepoint and must walk back to byte 1 without panicking.
        let s = "a🦀bbb";
        for max in [2usize, 3, 4] {
            assert!(truncate_utf8(s, max).starts_with("a..."), "max={max}");
        }
        // A cut that already lands on a boundary is kept as-is.
        assert!(truncate_utf8(s, 6).starts_with("a🦀b..."));
        // Exhaustive sweep over 2-byte text: every cut point must be safe.
        let cyrillic = "привет, мир!";
        for max in 0..=cyrillic.len() {
            let _ = truncate_utf8(cyrillic, max);
        }
    }

    #[test]
    fn truncate_utf8_display_passthrough_and_hard_cut() {
        assert_eq!(truncate_utf8_display("abc", 500), "abc");
        assert_eq!(truncate_utf8_display("abcdef", 3), "abc");
        assert_eq!(truncate_utf8_display("abcdef", 0), "");
    }

    #[test]
    fn truncate_utf8_display_counts_wide_chars_as_two_columns() {
        // A CJK character occupies two columns: 2 chars fill a 4-col budget.
        assert_eq!(truncate_utf8_display("中中中", 4), "中中");
        // One wide char plus one narrow char = 3 columns; the second wide
        // char would exceed 4 and must be dropped.
        assert_eq!(truncate_utf8_display("中a中", 4), "中a");
    }

    #[test]
    fn truncate_utf8_display_never_splits_a_codepoint() {
        // A 4-byte emoji counts two columns and stays whole.
        assert_eq!(truncate_utf8_display("a🦀bbb", 2), "a");
        assert_eq!(truncate_utf8_display("🦀", 1), "");
        assert_eq!(truncate_utf8_display("🦀", 2), "🦀");
    }
}
