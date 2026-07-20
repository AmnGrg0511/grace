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

use owo_colors::Rgb;

/// One coherent palette. Each field is the color for a distinct role in the
/// transcript — no two roles share a slot, so a skin fully re-themes the UI.
#[derive(Debug, Clone, Copy)]
pub struct Skin {
    pub name: &'static str,
    /// The glyph printed at the start of the user's input line (replaces
    /// the old literal "you: " label).
    pub prompt_glyph: &'static str,
    pub prompt: Rgb,
    /// The glyph printed before grace's final answer (replaces "grace: ").
    pub answer_glyph: &'static str,
    pub answer: Rgb,
    /// Intermediate model content ("thinking" sub-level).
    pub thinking: Rgb,
    /// Tool-call header bullet + name.
    pub tool_bullet: Rgb,
    pub tool_name: Rgb,
    /// Tool-call result / diff context lines (dimmed prefix).
    pub tool_dim: Rgb,
    /// Inline code / fenced code blocks — "golden monospace" by default,
    /// but fully skin-controlled.
    pub code: Rgb,
}

macro_rules! rgb {
    ($r:expr, $g:expr, $b:expr) => {
        Rgb($r, $g, $b)
    };
}

/// "gilded" (default) — graphite neutrals with an old-gold code accent;
/// the palette this CLI shipped its first classy pass with.
pub const GILDED: Skin = Skin {
    name: "gilded",
    prompt_glyph: "❯",
    prompt: rgb!(0, 200, 200),
    answer_glyph: "◆",
    answer: rgb!(190, 120, 220),
    thinking: rgb!(120, 110, 100),
    tool_bullet: rgb!(210, 170, 60),
    tool_name: rgb!(230, 230, 230),
    tool_dim: rgb!(120, 120, 120),
    code: rgb!(212, 175, 55),
};

/// "royal" — deep violet/indigo with a brighter gold, more formal.
pub const ROYAL: Skin = Skin {
    name: "royal",
    prompt_glyph: "❯",
    prompt: rgb!(147, 112, 219),
    answer_glyph: "◆",
    answer: rgb!(186, 85, 211),
    thinking: rgb!(125, 115, 105),
    tool_bullet: rgb!(255, 215, 0),
    tool_name: rgb!(230, 230, 250),
    tool_dim: rgb!(110, 100, 130),
    code: rgb!(255, 215, 0),
};

/// "ocean" — cool teal/blue, calm and low-contrast.
pub const OCEAN: Skin = Skin {
    name: "ocean",
    prompt_glyph: "›",
    prompt: rgb!(64, 190, 200),
    answer_glyph: "«",
    answer: rgb!(70, 130, 220),
    thinking: rgb!(110, 105, 100),
    tool_bullet: rgb!(0, 180, 170),
    tool_name: rgb!(220, 235, 240),
    tool_dim: rgb!(90, 120, 130),
    code: rgb!(0, 200, 190),
};

/// "sakura" — warm pinks, soft and bright.
pub const SAKURA: Skin = Skin {
    name: "sakura",
    prompt_glyph: "✿",
    prompt: rgb!(240, 130, 170),
    answer_glyph: "✦",
    answer: rgb!(230, 100, 150),
    thinking: rgb!(130, 115, 110),
    tool_bullet: rgb!(255, 180, 200),
    tool_name: rgb!(255, 235, 240),
    tool_dim: rgb!(150, 110, 125),
    code: rgb!(255, 160, 190),
};

/// "forest" — greens and earth tones, grounded.
pub const FOREST: Skin = Skin {
    name: "forest",
    prompt_glyph: "❯",
    prompt: rgb!(80, 170, 100),
    answer_glyph: "◆",
    answer: rgb!(60, 140, 80),
    thinking: rgb!(115, 110, 95),
    tool_bullet: rgb!(180, 160, 60),
    tool_name: rgb!(220, 230, 210),
    tool_dim: rgb!(100, 115, 95),
    code: rgb!(160, 190, 90),
};

/// "solaris" — amber/orange, high energy.
pub const SOLARIS: Skin = Skin {
    name: "solaris",
    prompt_glyph: "»",
    prompt: rgb!(230, 140, 40),
    answer_glyph: "»",
    answer: rgb!(220, 100, 40),
    thinking: rgb!(130, 110, 90),
    tool_bullet: rgb!(255, 170, 0),
    tool_name: rgb!(250, 230, 200),
    tool_dim: rgb!(140, 110, 80),
    code: rgb!(255, 180, 60),
};

/// "midnight" — near-monochrome blue/violet, minimal and quiet.
pub const MIDNIGHT: Skin = Skin {
    name: "midnight",
    prompt_glyph: "·",
    prompt: rgb!(100, 110, 190),
    answer_glyph: "·",
    answer: rgb!(130, 120, 200),
    thinking: rgb!(105, 100, 100),
    tool_bullet: rgb!(150, 150, 210),
    tool_name: rgb!(210, 210, 230),
    tool_dim: rgb!(90, 90, 110),
    code: rgb!(160, 170, 220),
};

pub const ALL: &[Skin] = &[GILDED, ROYAL, OCEAN, SAKURA, FOREST, SOLARIS, MIDNIGHT];

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

/// Names of every skin available right now: the 7 built-ins plus any custom
/// skins found on disk. Used by the `--select-skin` picker and `--list-skins`.
pub fn all_names() -> Vec<String> {
    let mut names: Vec<String> = ALL.iter().map(|s| s.name.to_string()).collect();
    names.extend(load_custom_skins().into_iter().map(|s| s.name));
    names
}

/// Resolve a skin by name (case-insensitive) — built-ins first, then custom
/// skins from disk — falling back to [`GILDED`] for an unknown/missing name
/// so a typo in config.toml never breaks startup. Custom skins are leaked
/// once into `'static` storage so the return type stays a plain `Skin`.
pub fn by_name(name: Option<&str>) -> Skin {
    let Some(name) = name else { return GILDED };
    if let Some(s) = ALL.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
        return *s;
    }
    for c in load_custom_skins() {
        if c.name.eq_ignore_ascii_case(name) {
            return leak_custom(c);
        }
    }
    GILDED
}

fn leak_custom(c: CustomSkin) -> Skin {
    let leak_str = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    Skin {
        name: leak_str(c.name),
        prompt_glyph: leak_str(c.prompt_glyph),
        prompt: Rgb(c.prompt[0], c.prompt[1], c.prompt[2]),
        answer_glyph: leak_str(c.answer_glyph),
        answer: Rgb(c.answer[0], c.answer[1], c.answer[2]),
        thinking: Rgb(c.thinking[0], c.thinking[1], c.thinking[2]),
        tool_bullet: Rgb(c.tool_bullet[0], c.tool_bullet[1], c.tool_bullet[2]),
        tool_name: Rgb(c.tool_name[0], c.tool_name[1], c.tool_name[2]),
        tool_dim: Rgb(c.tool_dim[0], c.tool_dim[1], c.tool_dim[2]),
        code: Rgb(c.code[0], c.code[1], c.code[2]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_default_skins_all_distinct_by_name() {
        assert_eq!(ALL.len(), 7);
        let mut names: Vec<&str> = ALL.iter().map(|s| s.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 7, "skin names must be unique");
    }

    #[test]
    fn by_name_is_case_insensitive_and_falls_back() {
        assert_eq!(by_name(Some("ROYAL")).name, "royal");
        assert_eq!(by_name(Some("nonexistent")).name, "gilded");
        assert_eq!(by_name(None).name, "gilded");
    }
}
