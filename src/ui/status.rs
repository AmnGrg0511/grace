//! Status-line and startup chrome: the banner wordmark, the per-turn
//! status line (model · context bar · elapsed), the input prompt glyph, and the
//! small display helpers they need. Pulled out of `chat.rs` so the REPL loop
//! stays that file's single focus.

use crate::core::context::ContextCompressionConfig;
use crate::ui::model::short_url;
use crate::ui::skin::{no_color, Role, Skin};

/// The startup wordmark printed once above the "chat mode —" hint.
///
/// 6 rows: "GRACE" in dense box-drawing capitals (ANSI Shadow style). The
/// lower half stands out in the skin's main color (the answer role —
/// grace's own output color, so each skin rebrands the mark at once); the
/// upper half recedes in the muted `tool_dim` tier (same as the status bar)
/// so it never competes with the conversation. `Skin::paint` is a no-op when
/// stdout is not a TTY, so piped output gets the plain art with no escapes.
pub fn chat_banner(skin: &Skin) -> String {
    const ART: [&str; 6] = [
        " ██████╗ ██████╗  █████╗  ██████╗███████╗",
        "██╔════╝ ██╔══██╗██╔══██╗██╔════╝██╔════╝",
        "██║  ███╗██████╔╝███████║██║     █████╗",
        "██║   ██║██╔══██╗██╔══██║██║     ██╔══╝",
        "╚██████╔╝██║  ██║██║  ██║╚██████╗███████╗",
        " ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚══════╝",
    ];
    const MAIN_COLOR_START: usize = 3; // rows 3–5 (the lower half) get the main color
    ART.iter()
        .enumerate()
        .map(|(i, line)| {
            let role = if i >= MAIN_COLOR_START {
                Role::Answer
            } else {
                Role::ToolDim
            };
            skin.paint(role, line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The idle status line text:
/// `· host · model[RO] · high · [context bar] · elapsed`.
#[allow(clippy::too_many_arguments)]
fn status_line_content(
    provider: Option<&str>,
    model: &str,
    effort: Option<&str>,
    count: usize,
    measured: bool,
    window: Option<u32>,
    trigger_fraction: f32,
    started: std::time::Instant,
    read_only: bool,
) -> String {
    let secs = started.elapsed().as_secs();
    let time = if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    };
    let bar = context_bar(count, measured, window, trigger_fraction);
    // `[RO]` sits right after the model: the posture is a property of what
    // the model may do, and the line already leads with the model. The
    // effort level follows the same rule — it changes how the model answers,
    // so it belongs next to the model's name. Hidden when off (the default).
    let ro = if read_only { " [RO]" } else { "" };
    // Provider leads when known — it is the widest fact (which endpoint is
    // being billed) and the thing /provider changes.
    let lead = match provider {
        Some(host) => format!("· {host} · {model}{ro}"),
        None => format!("· {model}{ro}"),
    };
    // The effort level sits next to the model it shapes. Shown bare (`high`,
    // not "high effort") — the slot is narrow and "effort" would only repeat
    // what /effort's own label already says. Hidden when off (the default).
    match effort {
        Some(level) => format!("{lead} · {level} · {bar} · {time}"),
        None => format!("{lead} · {bar} · {time}"),
    }
}

/// The context portion of the status line, computed without touching stdout
/// so the rule is unit-testable.
///
/// * `count` — tokens in use (provider-measured or locally estimated).
/// * `measured` — whether `count` came from the provider (no `~` prefix).
/// * `ctx` — the context window, if known. Known → a percent bar. Unknown
///   (`None`) → the raw count plus the auto-compaction point, so the
///   fallback-window trigger is never silent. `Some(0)` → the raw count only.
fn context_bar(count: usize, measured: bool, ctx: Option<u32>, trigger_fraction: f32) -> String {
    let approx = if measured { "" } else { "~" };
    match ctx {
        Some(limit) if limit > 0 => {
            let pct = ((count as f64) / (limit as f64) * 100.0) as usize;
            let filled = (pct * 8 / 100).min(8);
            let empty = 8 - filled;
            format!("[{}] {pct}%", "█".repeat(filled) + &"░".repeat(empty))
        }
        None => {
            // No trustworthy window: show the running count and the point at
            // which the compressor (budgeted against its fallback window)
            // will start eliding history.
            let trigger = (crate::core::context::FALLBACK_CONTEXT_WINDOW as f64
                * f64::from(trigger_fraction)) as u64;
            format!("{approx}{count} tok · compacts near ~{} tok", human_tokens(trigger))
        }
        // `Some(0)`: a window the caller explicitly set to zero — just the
        // count, no bar and no compaction claim.
        _ => format!("{approx}{count} tok"),
    }
}

/// Human-readable token count for the status line: `950`, `1.2k`, `32k`.
pub(crate) fn human_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

/// A subtle status line above the prompt: model · context bar · elapsed.
/// All in the skin's muted `tool_dim` color so it recedes behind the prompt
/// glyph and never competes with the conversation itself. Single dim
/// (color only, no extra ANSI dim) — same tier as tool output body.
///
/// The bar prefers the provider's own `prompt_tokens` from the last turn
/// (`measured`); only when the provider didn't report usage does it fall back
/// to the same byte-based estimate the compressor uses (prefix `~`). When the
/// context window is unknown, no bar is drawn (a % against a guessed window
/// would be worse than nothing) — instead the line shows the running count
/// and *where auto-compaction will fire*, so the hidden 32k fallback trigger
/// is never silent.
#[allow(clippy::too_many_arguments)]
pub fn print_status_line(
    skin: &Skin,
    transport: &(dyn crate::transport::ProviderTransport + '_),
    messages: &[crate::message::Message],
    started_at: std::time::Instant,
    cached_context_window: Option<u32>,
    last_usage: Option<crate::transport::TokenUsage>,
    compression_config: &ContextCompressionConfig,
    read_only: bool,
) {
    let model = transport
        .current_model()
        .unwrap_or_else(|| transport.name().to_string());

    // Prefer the provider's real count; fall back to the same estimator the
    // compressor uses, so the bar and the trigger never disagree.
    let (count, measured) = match last_usage {
        Some(u) => (u.prompt_tokens as usize, true),
        None => {
            use crate::util::tokens::TokenCounter;
            let est = crate::util::tokens::default_counter()
                .count_messages(messages)
                .max(1);
            (est, false)
        }
    };
    // Saved context window (loaded once per chat session, not re-read from
    // disk every turn) beats the static lookup table — it covers models
    // only known at runtime.
    let ctx = cached_context_window.or_else(|| crate::config::settings::context_window_for(&model));

    let line = status_line_content(
        transport.current_base_url().as_deref().map(short_url).as_deref(),
        &model,
        transport.current_reasoning().as_deref(),
        count,
        measured,
        ctx,
        compression_config.normalized().trigger_fraction,
        started_at,
        read_only,
    );
    // Cap to the terminal width so the status line can never wrap onto the
    // prompt row below it.
    let line = match crate::ui::markdown::terminal_width() {
        Some(w) => crate::util::truncate_utf8_display(&line, w.saturating_sub(1)),
        None => line,
    };
    if no_color() {
        println!("{line}");
    } else {
        // skin's muted tool-dim color, single dim.
        println!("{}", skin.paint(Role::ToolDim, &line));
    }
}

/// The interactive-chat input prompt — a skin-colored glyph, never the
/// literal word "you", so the transcript reads as two distinct visual
/// speakers instead of a flat `you:`/`grace:` log.
pub fn prompt_label(skin: &Skin) -> String {
    if no_color() {
        return format!("{} ", skin.prompt_glyph);
    }
    skin.paint(Role::Prompt, &format!("{} ", skin.prompt_glyph))
}

#[cfg(test)]
mod status_line_tests {
    use super::*;

    #[test]
    fn the_idle_line_keeps_one_format() {
        let started = std::time::Instant::now();
        let line =
            status_line_content(None, "gpt-4o", None, 4_000, false, Some(10_000), 0.75, started, false);
        // `· model · [context bar] · elapsed`. Heuristic count (measured=
        // false) marks the percent with `~`; a fresh start reads 0:00.
        assert!(
            line.starts_with("· gpt-4o · [███░░░░░] 40% · "),
            "unexpected line: {line}"
        );
        assert!(line.ends_with("0:00"), "unexpected line: {line}");
    }

    #[test]
    fn an_effort_level_sits_between_the_model_and_the_bar() {
        let started = std::time::Instant::now();
        let line = status_line_content(
            Some("api.example.com"),
            "gpt-4o",
            Some("high"),
            4_000,
            false,
            Some(10_000),
            0.75,
            started,
            false,
        );
        assert!(
            line.starts_with(
                "· api.example.com · gpt-4o · high · [███░░░░░] 40% · "
            ),
            "unexpected line: {line}"
        );
        // No base URL (e.g. a fixed-endpoint transport) drops the segment.
        let bare =
            status_line_content(None, "gpt-4o", None, 4_000, false, Some(10_000), 0.75, started, false);
        assert!(bare.starts_with("· gpt-4o · "), "{bare:?}");
        // Off stays hidden — the default line must not grow noise.
        let plain =
            status_line_content(None, "gpt-4o", None, 4_000, false, Some(10_000), 0.75, started, false);
        assert!(!plain.contains("effort"), "{plain:?}");
    }

    #[test]
    fn read_only_mode_marks_the_line_with_an_ro_badge() {
        let started = std::time::Instant::now();
        let line =
            status_line_content(None, "gpt-4o", None, 4_000, true, Some(10_000), 0.75, started, true);
        assert!(line.starts_with("· gpt-4o [RO] · "), "unexpected line: {line}");
        // The badge must not disturb the rest of the format.
        let off = status_line_content(None, "gpt-4o", None, 4_000, true, Some(10_000), 0.75, started, false);
        assert!(line.replacen(" [RO]", "", 1) == off, "only the badge differs: {line:?} vs {off:?}");
    }
}

#[cfg(test)]
mod context_bar_tests {
    use super::*;

    #[test]
    fn measured_provider_count_gets_no_approx_prefix() {
        assert_eq!(context_bar(5_000, true, None, 0.75), "5000 tok · compacts near ~24k tok");
        assert_eq!(context_bar(5_000, false, None, 0.75), "~5000 tok · compacts near ~24k tok");
    }

    #[test]
    fn a_known_window_renders_the_percent_bar() {
        // 40% of a 10k window.
        let bar = context_bar(4_000, true, Some(10_000), 0.75);
        assert_eq!(bar, "[███░░░░░] 40%");
    }

    #[test]
    fn the_bar_never_clips_above_eight_segments() {
        assert_eq!(context_bar(99_999, true, Some(1_000), 0.75), "[████████] 9999%");
    }

    #[test]
    fn compaction_point_follows_the_trigger_fraction() {
        // 32k fallback window at 0.25 → 8.0k.
        assert!(context_bar(10, true, None, 0.25).contains("compacts near ~8.0k tok"));
    }

    #[test]
    fn a_zero_window_shows_only_the_count() {
        assert_eq!(context_bar(123, false, Some(0), 0.75), "~123 tok");
    }

    #[test]
    fn human_tokens_scales_readably() {
        assert_eq!(human_tokens(950), "950");
        assert_eq!(human_tokens(1_200), "1.2k");
        assert_eq!(human_tokens(32_000), "32k");
    }
}

#[cfg(test)]
mod banner_tests {
    use super::chat_banner;
    use crate::ui::skin::SOLARIS;

    #[test]
    fn banner_is_a_compact_box_wordmark() {
        // Structural invariants, not a copy of the constant: the wordmark
        // must stay 6 compact rows (it prints above the status line on every
        // chat start), stay within a 43-col budget (well under a third of a
        // terminal — no wall of art), use only box-drawing/block characters,
        // and — with no TTY in tests — be free of any leaked ANSI escapes.
        let banner = chat_banner(&SOLARIS);
        assert!(!banner.contains('\x1b'), "no ANSI escapes in non-TTY banner: {banner:?}");
        let lines: Vec<&str> = banner.lines().collect();
        assert_eq!(lines.len(), 6, "expected 6 art rows: {banner:?}");
        for line in &lines {
            // chars, not bytes: the box glyphs are 3 bytes each in UTF-8.
            assert!(line.chars().count() <= 43, "wordmark must stay within 43 cols: {line:?}");
            assert!(
                line.chars().all(|c| matches!(c, ' ' | '█' | '═' | '║' | '╗' | '╔' | '╚' | '╝')),
                "wordmark must use only box/block characters: {line:?}"
            );
        }
    }
}
