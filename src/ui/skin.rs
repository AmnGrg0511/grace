//! Color skins — every ANSI color in the CLI comes from here, nowhere else.
//!
//! No hardcoded `owo_colors::red()`/`magenta()` calls should exist outside
//! this module: `main.rs` and `markdown.rs` ask a [`Skin`] for a role's
//! color and print that. This is what makes skins swappable — change
//! `~/.grace/config.toml`'s `skin = "..."` and every surface (prompt glyph,
//! answer glyph, thinking, tool-call tree, code) restyles together.
//!
//! Colors are 24-bit truecolor (`\x1b[38;2;r;g;b m`) so a skin can be an
//! exact palette, not just the 16-color ANSI approximation.

use anstyle::{Color, RgbColor, Style};

/// The single color policy for the whole UI.
///
/// Color comes off when stdout is not a terminal, or `NO_COLOR` is set, or
/// `CLICOLOR=0` — and back on when `CLICOLOR_FORCE=1` (lets a user pipe to
/// `less -R` and keep color). Every color-emitting surface (stream path,
/// `Skin::paint`, one-shot renders, the markdown renderer) must ask this
/// one function: a second policy with a subtly different check was bug
/// G13 (raw fragments next to colored bullets under `NO_COLOR=1` on a TTY).
pub fn no_color() -> bool {
    use std::io::IsTerminal;
    if std::env::var("CLICOLOR_FORCE").as_deref() == Ok("1") {
        return false;
    }
    !std::io::stdout().is_terminal()
        || std::env::var("NO_COLOR").is_ok()
        || std::env::var("CLICOLOR").as_deref() == Ok("0")
}

/// The ANSI reset to pair with painted output — empty when color is off, so
/// a `NO_COLOR` run contains no escape bytes at all.
pub fn reset() -> &'static str {
    if no_color() {
        ""
    } else {
        "\x1b[0m"
    }
}

/// One coherent palette. Each field is the color for a distinct role in the
/// transcript — no two roles share a slot, so a skin fully re-themes the UI.
#[derive(Debug, Clone, Copy)]
pub struct Skin {
    pub name: &'static str,
    /// The glyph printed at the start of the user's input line (replaces
    /// the old literal "you: " label).
    pub prompt_glyph: &'static str,
    pub prompt: RgbColor,
    /// The glyph printed before grace's final answer (replaces "grace: ").
    pub answer_glyph: &'static str,
    pub answer: RgbColor,
    /// Intermediate model content ("thinking" sub-level).
    pub thinking: RgbColor,
    /// Tool-call header bullet + name.
    pub tool_bullet: RgbColor,
    pub tool_name: RgbColor,
    /// Tool-call result / diff context lines (dimmed prefix).
    pub tool_dim: RgbColor,
    /// Inline code / fenced code blocks — "golden monospace" by default,
    /// but fully skin-controlled.
    pub code: RgbColor,
    /// The general highlight: picker cursors, status-line segments, anything
    /// that should pop without stealing a semantic role.
    pub accent: RgbColor,
    /// Secondary/explanatory text: command summaries in the palette, usage
    /// lines, footnotes. Deliberately quieter than [`Self::tool_dim`].
    pub hint: RgbColor,
    /// Positive confirmations (saved, applied, ok).
    pub success: RgbColor,
    /// Failures and warnings that must be impossible to miss.
    pub error: RgbColor,
    /// Background tint behind [`Self::hint`] text — a faint, low-luminance
    /// wash rather than a distinct foreground hue, so secondary text reads as
    /// a soft "chip" over any terminal background.
    pub hint_bg: RgbColor,
    /// Background tint behind [`Self::success`] — a deep green that makes
    /// confirmations read as highlighted, independent of the text color.
    pub success_bg: RgbColor,
    /// Background tint behind [`Self::error`] — a deep red for the same reason.
    pub error_bg: RgbColor,
    /// The one-line persona greeting printed under the banner at startup —
    /// fully skin-controlled so a custom skin can set the tone (formal,
    /// casual, a particular name of address) without touching code.
    pub welcome: &'static str,
    /// The farewell printed on `/exit` / EOF. Same control as [`Self::welcome`].
    pub goodbye: &'static str,
}

impl Skin {
    /// Get a `Style` for a color role.
    ///
    /// Policy-aware: when [`no_color`] is on the style renders to nothing,
    /// which covers direct `style(..).render()` call sites as well as
    /// [`paint`].
    pub fn style(&self, role: Role) -> Style {
        if no_color() {
            return Style::new();
        }
        match role {
            Role::Prompt => Style::new().fg_color(Some(Color::from(self.prompt))),
            Role::Answer => Style::new().fg_color(Some(Color::from(self.answer))),
            Role::Thinking => Style::new().fg_color(Some(Color::from(self.thinking))),
            Role::ToolBullet => Style::new().fg_color(Some(Color::from(self.tool_bullet))),
            Role::ToolName => Style::new().fg_color(Some(Color::from(self.tool_name))),
            Role::ToolDim => Style::new().fg_color(Some(Color::from(self.tool_dim))),
            Role::Code => Style::new().fg_color(Some(Color::from(self.code))),
            Role::Accent => Style::new().fg_color(Some(Color::from(self.accent))),
            // These three read as *highlighted* via a low-luminance
            // background tint rather than a distinct foreground hue, so they
            // stand out against the terminal's own background (dark wash for
            // hint, deep green/red for success/error) while staying in-step
            // with whatever palette the skin chooses for the text.
            Role::Hint => Style::new()
                .fg_color(Some(Color::from(self.hint)))
                .bg_color(Some(Color::from(self.hint_bg))),
            Role::Success => Style::new()
                .fg_color(Some(Color::from(self.success)))
                .bg_color(Some(Color::from(self.success_bg))),
            Role::Error => Style::new()
                .fg_color(Some(Color::from(self.error)))
                .bg_color(Some(Color::from(self.error_bg))),
        }
    }

    /// Render text with a role's color (TTY only; no-op otherwise).
    pub fn paint(&self, role: Role, text: &str) -> String {
        // Early-return on the policy (not just an empty style) so no reset
        // escape is emitted either: `NO_COLOR` output stays byte-clean.
        if no_color() {
            return text.to_string();
        }
        let style = self.style(role);
        // anstyle's `Style::new().render()` (no attributes set) renders to
        // an EMPTY string, not a reset escape — using it as "the reset"
        // left every painted color bleeding into all subsequent unstyled
        // output (e.g. the plain-text /model provider list appearing
        // orange because the prior status-line paint() was never closed).
        // `anstyle::Reset` is the actual `\x1b[0m` sequence.
        format!("{}{}{}", style.render(), text, anstyle::Reset.render())
    }
}

/// Color roles used throughout the CLI.
#[derive(Debug, Clone, Copy)]
pub enum Role {
    Prompt,
    Answer,
    Thinking,
    ToolBullet,
    ToolName,
    ToolDim,
    Code,
    Accent,
    Hint,
    Success,
    Error,
}

/// "solaris" — layered amber/orange, high energy (DEFAULT). The palette is
/// deliberately a *spread* of warm tones rather than one flat orange: a
/// bright golden for code, a vivid ember for the accent/cursor, a deep burnt
/// orange for grace's own voice, a soft warm grey for thinking, and a warm
/// mid for the dim tier — so the eye can tell roles apart by temperature and
/// brightness, the way a multi-accent terminal theme does.
pub const SOLARIS: Skin = Skin {
    name: "solaris",
    prompt_glyph: "»",
    prompt: RgbColor(255, 168, 72),
    answer_glyph: "✦",
    answer: RgbColor(255, 132, 40),
    thinking: RgbColor(158, 132, 108),
    tool_bullet: RgbColor(255, 176, 0),
    tool_name: RgbColor(255, 238, 216),
    tool_dim: RgbColor(154, 124, 96),
    code: RgbColor(255, 201, 92),
    accent: RgbColor(255, 105, 45),
    hint: RgbColor(176, 148, 116),
    success: RgbColor(150, 205, 90),
    error: RgbColor(240, 85, 65),
    hint_bg: RgbColor(30, 27, 24),
    success_bg: RgbColor(18, 44, 26),
    error_bg: RgbColor(54, 18, 18),
    welcome: "Grace is ready. How may I help you, Sir?",
    goodbye: "Until next time, Sir.",
};

/// "royal" — deep violet/indigo with a brighter gold, more formal.
pub const ROYAL: Skin = Skin {
    name: "royal",
    prompt_glyph: "❯",
    prompt: RgbColor(147, 112, 219),
    answer_glyph: "◆",
    answer: RgbColor(186, 85, 211),
    thinking: RgbColor(125, 115, 105),
    tool_bullet: RgbColor(255, 196, 64),
    tool_name: RgbColor(236, 232, 255),
    tool_dim: RgbColor(118, 106, 140),
    code: RgbColor(255, 224, 120),
    accent: RgbColor(206, 128, 255),
    hint: RgbColor(156, 146, 178),
    success: RgbColor(120, 200, 145),
    error: RgbColor(245, 100, 115),
    hint_bg: RgbColor(30, 27, 24),
    success_bg: RgbColor(18, 44, 26),
    error_bg: RgbColor(54, 18, 18),
    welcome: "At your service, Sir.",
    goodbye: "Until next time, Sir.",
};

/// "ocean" — cool teal/blue, calm and low-contrast.
pub const OCEAN: Skin = Skin {
    name: "ocean",
    prompt_glyph: "›",
    prompt: RgbColor(64, 190, 200),
    answer_glyph: "«",
    answer: RgbColor(70, 130, 220),
    thinking: RgbColor(110, 105, 100),
    tool_bullet: RgbColor(0, 190, 178),
    tool_name: RgbColor(224, 240, 246),
    tool_dim: RgbColor(96, 128, 138),
    code: RgbColor(46, 219, 203),
    accent: RgbColor(95, 165, 255),
    hint: RgbColor(126, 146, 160),
    success: RgbColor(80, 200, 145),
    error: RgbColor(248, 112, 98),
    hint_bg: RgbColor(30, 27, 24),
    success_bg: RgbColor(18, 44, 26),
    error_bg: RgbColor(54, 18, 18),
    welcome: "Grace is ready. How can I help you?",
    goodbye: "Goodbye.",
};

/// "sakura" — warm pinks, soft and bright.
pub const SAKURA: Skin = Skin {
    name: "sakura",
    prompt_glyph: "✿",
    prompt: RgbColor(240, 130, 170),
    answer_glyph: "✦",
    answer: RgbColor(230, 100, 150),
    thinking: RgbColor(130, 115, 110),
    tool_bullet: RgbColor(255, 168, 194),
    tool_name: RgbColor(255, 238, 244),
    tool_dim: RgbColor(158, 118, 132),
    code: RgbColor(255, 176, 200),
    accent: RgbColor(255, 95, 135),
    hint: RgbColor(176, 148, 158),
    success: RgbColor(135, 195, 125),
    error: RgbColor(232, 72, 96),
    hint_bg: RgbColor(30, 27, 24),
    success_bg: RgbColor(18, 44, 26),
    error_bg: RgbColor(54, 18, 18),
    welcome: "I'm here. What shall we do?",
    goodbye: "Goodbye.",
};

pub const ALL: &[Skin] = &[SOLARIS, ROYAL, OCEAN, SAKURA];

/// A custom, user-defined skin loaded from `~/.grace/skins/<name>.toml`.
/// Owns its strings (unlike the `&'static` built-ins) since it's parsed at
/// runtime; `as_skin()` borrows from it to build a [`Skin`] for one call.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CustomSkin {
    pub name: String,
    #[serde(default = "default_prompt_glyph")]
    pub prompt_glyph: String,
    pub prompt: [u8; 3],
    #[serde(default = "default_answer_glyph")]
    pub answer_glyph: String,
    pub answer: [u8; 3],
    pub thinking: [u8; 3],
    pub tool_bullet: [u8; 3],
    pub tool_name: [u8; 3],
    pub tool_dim: [u8; 3],
    pub code: [u8; 3],
    #[serde(default = "default_accent")]
    pub accent: [u8; 3],
    #[serde(default = "default_hint")]
    pub hint: [u8; 3],
    #[serde(default = "default_success")]
    pub success: [u8; 3],
    #[serde(default = "default_error")]
    pub error: [u8; 3],
    #[serde(default = "default_hint_bg")]
    pub hint_bg: [u8; 3],
    #[serde(default = "default_success_bg")]
    pub success_bg: [u8; 3],
    #[serde(default = "default_error_bg")]
    pub error_bg: [u8; 3],
    #[serde(default = "default_welcome")]
    pub welcome: String,
    #[serde(default = "default_goodbye")]
    pub goodbye: String,
}

fn default_prompt_glyph() -> String {
    "❯".to_string()
}
fn default_answer_glyph() -> String {
    "◆".to_string()
}
// Serde fallbacks for custom skins written before these roles existed — an
// old ~/.grace/skins/*.toml must keep parsing (defaults mirror solaris).
fn default_accent() -> [u8; 3] { [255, 105, 45] }
fn default_hint() -> [u8; 3] { [155, 130, 100] }
fn default_success() -> [u8; 3] { [150, 205, 90] }
fn default_error() -> [u8; 3] { [240, 85, 65] }
// Dark background tints for the three semantic roles — near-black washes so
// they read as translucent chips on any terminal background.
fn default_hint_bg() -> [u8; 3] { [30, 27, 24] }
fn default_success_bg() -> [u8; 3] { [18, 44, 26] }
fn default_error_bg() -> [u8; 3] { [54, 18, 18] }
// Serde fallbacks for persona lines — an old ~/.grace/skins/*.toml that
// predates them keeps parsing (defaults mirror solaris).
fn default_welcome() -> String { "Grace is ready. How may I help you, Sir?".to_string() }
fn default_goodbye() -> String { "Until next time, Sir.".to_string() }

/// Directory custom skins live in: `~/.grace/skins/*.toml`.
pub fn custom_skins_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("skins")
}

/// Load every valid `*.toml` under [`custom_skins_dir`]. Malformed files are
/// skipped silently — a broken custom skin must never block startup.
pub fn load_custom_skins() -> Vec<CustomSkin> {
    let dir = custom_skins_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| toml::from_str::<CustomSkin>(&text).ok())
        .collect()
}

/// Names of every skin available right now: the 4 built-ins plus any custom
/// skins found on disk. Used by the `--select-skin` picker and `--list-skins`.
pub fn all_names() -> Vec<String> {
    let mut names: Vec<String> = ALL.iter().map(|s| s.name.to_string()).collect();
    names.extend(load_custom_skins().into_iter().map(|s| s.name));
    names
}

/// Resolve a skin by name (case-insensitive) — built-ins first, then custom
/// skins from disk — falling back to [`SOLARIS`] for an unknown/missing name
/// so a typo in `config.toml` never breaks startup. Custom skins are leaked
/// once into `'static` storage so the return type stays a plain [`Skin`].
pub fn by_name(name: Option<&str>) -> Skin {
    let Some(name) = name else { return SOLARIS };
    if let Some(s) = ALL.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
        return *s;
    }
    for c in load_custom_skins() {
        if c.name.eq_ignore_ascii_case(name) {
            return leak_custom(c);
        }
    }
    SOLARIS
}

fn leak_custom(c: CustomSkin) -> Skin {
    let leak_str = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    Skin {
        name: leak_str(c.name),
        prompt_glyph: leak_str(c.prompt_glyph),
        prompt: RgbColor(c.prompt[0], c.prompt[1], c.prompt[2]),
        answer_glyph: leak_str(c.answer_glyph),
        answer: RgbColor(c.answer[0], c.answer[1], c.answer[2]),
        thinking: RgbColor(c.thinking[0], c.thinking[1], c.thinking[2]),
        tool_bullet: RgbColor(c.tool_bullet[0], c.tool_bullet[1], c.tool_bullet[2]),
        tool_name: RgbColor(c.tool_name[0], c.tool_name[1], c.tool_name[2]),
        tool_dim: RgbColor(c.tool_dim[0], c.tool_dim[1], c.tool_dim[2]),
        code: RgbColor(c.code[0], c.code[1], c.code[2]),
        accent: RgbColor(c.accent[0], c.accent[1], c.accent[2]),
        hint: RgbColor(c.hint[0], c.hint[1], c.hint[2]),
        success: RgbColor(c.success[0], c.success[1], c.success[2]),
        error: RgbColor(c.error[0], c.error[1], c.error[2]),
        hint_bg: RgbColor(c.hint_bg[0], c.hint_bg[1], c.hint_bg[2]),
        success_bg: RgbColor(c.success_bg[0], c.success_bg[1], c.success_bg[2]),
        error_bg: RgbColor(c.error_bg[0], c.error_bg[1], c.error_bg[2]),
        welcome: leak_str(c.welcome),
        goodbye: leak_str(c.goodbye),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_default_skins_all_distinct_by_name() {
        assert_eq!(ALL.len(), 4);
        let mut names: Vec<&str> = ALL.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 4, "skin names must be unique");
    }

    #[test]
    fn every_skin_gives_every_role_a_distinct_color() {
        // A skin must re-theme the whole UI, which only works if no two roles
        // share a color — otherwise, say, code blocks and tool bullets would
        // be indistinguishable under that skin.
        for s in ALL {
            let colors = [
                s.prompt, s.answer, s.thinking, s.tool_bullet, s.tool_name, s.tool_dim, s.code,
                s.accent, s.hint, s.success, s.error,
            ];
            let mut sorted = colors.to_vec();
            sorted.sort_by_key(|c| (c.0, c.1, c.2));
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                colors.len(),
                "{} reuses a color across roles",
                s.name
            );
        }
    }

    #[test]
    fn a_custom_skin_toml_without_the_new_fields_still_parses() {
        // Pre-accent-era skin file: serde defaults must fill the gaps.
        let toml = r#"
name = "legacy"
prompt = [1, 2, 3]
answer = [4, 5, 6]
thinking = [7, 8, 9]
tool_bullet = [10, 11, 12]
tool_name = [13, 14, 15]
tool_dim = [16, 17, 18]
code = [19, 20, 21]
"#;
        let c: CustomSkin = toml::from_str(toml).expect("legacy toml must parse");
        assert_eq!(c.accent, default_accent());
        assert_eq!(c.hint, default_hint());
        assert_eq!(c.success, default_success());
        assert_eq!(c.error, default_error());
    }

    #[test]
    fn by_name_is_case_insensitive_and_falls_back() {
        assert_eq!(by_name(Some("ROYAL")).name, "royal");
        assert_eq!(by_name(Some("nonexistent")).name, "solaris");
        assert_eq!(by_name(None).name, "solaris");
    }

    #[test]
    fn style_rendering_works() {
        let skin = SOLARIS;
        let styled = skin.paint(Role::Prompt, "test");
        assert!(styled.contains("test"));
    }

    #[test]
    fn the_color_policy_is_single_and_precedence_is_pinned() {
        // Regression (G13): the stream path and `paint`/the markdown
        // renderer each kept their own "is a TTY" check, so `NO_COLOR=1` on
        // a TTY left the bullets colored while fragments went raw. The whole
        // UI now asks this one function — pin its precedence and that
        // paint/reset follow it.
        let _lock = crate::util::test_support::env_guard();
        std::env::remove_var("CLICOLOR_FORCE");
        std::env::remove_var("NO_COLOR");
        std::env::remove_var("CLICOLOR");

        // Not a TTY (tests): color off by default, reset emits nothing.
        assert!(no_color());
        assert_eq!(reset(), "");

        // NO_COLOR keeps it off where a bare TTY check would disagree.
        std::env::set_var("NO_COLOR", "1");
        assert!(no_color());
        assert_eq!(SOLARIS.paint(Role::Prompt, "x"), "x");

        // CLICOLOR_FORCE wins over NO_COLOR (piping through `less -R`).
        std::env::set_var("CLICOLOR_FORCE", "1");
        assert!(!no_color());
        std::env::remove_var("CLICOLOR_FORCE");
        std::env::remove_var("NO_COLOR");
    }
}
