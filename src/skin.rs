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
}

impl Skin {
    /// Get a `Style` for a color role.
    pub fn style(&self, role: Role) -> Style {
        match role {
            Role::Prompt => Style::new().fg_color(Some(Color::from(self.prompt))),
            Role::Answer => Style::new().fg_color(Some(Color::from(self.answer))),
            Role::Thinking => Style::new().fg_color(Some(Color::from(self.thinking))),
            Role::ToolBullet => Style::new().fg_color(Some(Color::from(self.tool_bullet))),
            Role::ToolName => Style::new().fg_color(Some(Color::from(self.tool_name))),
            Role::ToolDim => Style::new().fg_color(Some(Color::from(self.tool_dim))),
            Role::Code => Style::new().fg_color(Some(Color::from(self.code))),
        }
    }

    /// Render text with a role's color (TTY only; no-op otherwise).
    pub fn paint(&self, role: Role, text: &str) -> String {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return text.to_string();
        }
        let style = self.style(role);
        format!("{}{}{}", style.render(), text, Style::new().render())
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
}

/// "solaris" — amber/orange, high energy (DEFAULT).
pub const SOLARIS: Skin = Skin {
    name: "solaris",
    prompt_glyph: "»",
    prompt: RgbColor(230, 140, 40),
    answer_glyph: "»",
    answer: RgbColor(220, 100, 40),
    thinking: RgbColor(130, 110, 90),
    tool_bullet: RgbColor(255, 170, 0),
    tool_name: RgbColor(250, 230, 200),
    tool_dim: RgbColor(140, 110, 80),
    code: RgbColor(255, 180, 60),
};

/// "royal" — deep violet/indigo with a brighter gold, more formal.
pub const ROYAL: Skin = Skin {
    name: "royal",
    prompt_glyph: "❯",
    prompt: RgbColor(147, 112, 219),
    answer_glyph: "◆",
    answer: RgbColor(186, 85, 211),
    thinking: RgbColor(125, 115, 105),
    tool_bullet: RgbColor(255, 215, 0),
    tool_name: RgbColor(230, 230, 250),
    tool_dim: RgbColor(110, 100, 130),
    code: RgbColor(255, 215, 0),
};

/// "ocean" — cool teal/blue, calm and low-contrast.
pub const OCEAN: Skin = Skin {
    name: "ocean",
    prompt_glyph: "›",
    prompt: RgbColor(64, 190, 200),
    answer_glyph: "«",
    answer: RgbColor(70, 130, 220),
    thinking: RgbColor(110, 105, 100),
    tool_bullet: RgbColor(0, 180, 170),
    tool_name: RgbColor(220, 235, 240),
    tool_dim: RgbColor(90, 120, 130),
    code: RgbColor(0, 200, 190),
};

/// "sakura" — warm pinks, soft and bright.
pub const SAKURA: Skin = Skin {
    name: "sakura",
    prompt_glyph: "✿",
    prompt: RgbColor(240, 130, 170),
    answer_glyph: "✦",
    answer: RgbColor(230, 100, 150),
    thinking: RgbColor(130, 115, 110),
    tool_bullet: RgbColor(255, 180, 200),
    tool_name: RgbColor(255, 235, 240),
    tool_dim: RgbColor(150, 110, 125),
    code: RgbColor(255, 160, 190),
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
}

fn default_prompt_glyph() -> String {
    "❯".to_string()
}
fn default_answer_glyph() -> String {
    "◆".to_string()
}

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
}
