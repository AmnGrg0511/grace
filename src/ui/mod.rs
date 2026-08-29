//! User interface — everything that talks to a terminal.
//!
//! ```text
//! chat.rs         the interactive REPL and its slash commands
//! spinner.rs      the `{frame} thinking` background ticker
//! stream.rs       the append-only streaming markdown commit/render engine
//! status.rs       the startup banner wordmark + per-turn status line
//! cli.rs          --help text and shell completions
//! commands.rs     the single slash-command registry (completer/palette/dispatch)
//! line_reader.rs  line editing / history
//! completer.rs    tab completion + hinting for the REPL
//! picker.rs       the shared dropdown (palette, /model, /jump)
//! skin.rs         color skins
//! markdown.rs     pulldown-cmark + syntect terminal rendering
//! wizard.rs       first-run onboarding
//! ```
//!
//! Nothing in [`crate::core`] may depend on this module: the agent engine must
//! stay usable with no terminal attached.

pub mod chat;
pub mod cli;
pub mod commands;
pub mod completer;
pub mod interaction;
pub mod line_reader;
pub mod markdown;
pub mod model;
pub mod picker;
pub mod skin;
pub mod status;
pub mod spinner;
pub mod stream;
pub mod wizard;

pub use markdown::render_terminal;
pub use skin::Skin;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_and_skins_are_reachable_from_the_module_root() {
        let skin: Skin = skin::by_name(Some("ocean"));
        let rendered = render_terminal("# Title", &skin);
        assert!(rendered.contains("Title"));
    }

    #[test]
    fn an_unknown_skin_name_falls_back_rather_than_panicking() {
        // A typo in ~/.grace/config.toml must not take the whole CLI down.
        let skin = skin::by_name(Some("no-such-skin"));
        assert!(!skin.name.is_empty());
    }

    #[test]
    fn the_cli_entry_points_are_reachable() {
        assert!(!cli::HELP_TEXT.is_empty());
        let parsed = cli::CliArgs::parse([] as [&str; 0]);
        assert!(parsed.wants_chat());
    }
}
