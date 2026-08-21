//! The single source of truth for chat slash commands.
//!
//! Three surfaces used to each keep their own copy of the command list — the
//! tab-completer, the dispatch match in `chat.rs`, and the command palette —
//! and nothing checked they agreed (audit S14); that is how `/help` and
//! `/commands` ended up dispatchable and documented but missing from
//! completion (audit G14). The completer, the command palette, and the
//! dispatcher all enumerate this table now, and `chat.rs` carries the
//! dispatch↔registry agreement test.

/// One slash command: its canonical name (no `/`), any aliases a typed input
/// may also match, and the one-line summary the help screen shows.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
}

impl SlashCommand {
    /// Every string (as typed after `/`) that selects this command.
    pub fn all_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.name).chain(self.aliases.iter().copied())
    }
}

/// The canonical command list. Order here is the order in the palette. A
/// handler with no entry here is undiscoverable — that is the bug
/// this table exists to prevent (see the agreement test in `chat.rs`).
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "exit",
        aliases: &[],
        summary: "Exit the chat",
    },
    SlashCommand {
        name: "model",
        aliases: &[],
        summary: "Switch model (picker if no arg)",
    },
    SlashCommand {
        name: "skin",
        aliases: &[],
        summary: "Switch color skin (picker if no arg)",
    },
    SlashCommand {
        name: "session",
        aliases: &[],
        summary: "Switch / start session (picker)",
    },
    SlashCommand {
        name: "jump",
        aliases: &[],
        summary: "Rewind context to an earlier message (picker)",
    },
    SlashCommand {
        name: "verbose",
        aliases: &[],
        summary: "Toggle tool-output visibility",
    },
    SlashCommand {
        name: "readonly",
        aliases: &[],
        summary: "Read-only posture: hide write/edit/bash/delegate [on|off]",
    },
];

/// Resolve a command word (the text after `/`, before the first space) to its
/// canonical entry — by name or alias. `None` means "not a command": that
/// input is model text, not a slash command.
pub fn resolve(cmd: &str) -> Option<&'static SlashCommand> {
    SLASH_COMMANDS.iter().find(|c| c.all_names().any(|n| n == cmd))
}

/// Every command whose name starts with `cmd` — the candidates for a typed
/// abbreviation like `/m`. One match means unambiguous shorthand; several
/// means the caller offers a picker between them; none means model text.
pub fn resolve_prefix(cmd: &str) -> Vec<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.all_names().any(|n| n.starts_with(cmd)))
        .collect()
}

/// Completion candidates for a line that already starts with `/`: every name
/// and alias (slash-prefixed) that has the typed line as a prefix.
pub fn completion_candidates(typed: &str) -> Vec<String> {
    if !typed.starts_with('/') {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .flat_map(|c| c.all_names())
        .map(|n| format!("/{n}"))
        .filter(|full| full.starts_with(typed))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_and_alias_resolves_to_its_command() {
        for c in SLASH_COMMANDS {
            for n in c.all_names() {
                assert_eq!(resolve(n).map(|r| r.name), Some(c.name), "{n}");
            }
        }
    }

    #[test]
    fn unknown_words_do_not_resolve() {
        for w in ["", "help2", "models", "retried", "bash"] {
            assert!(resolve(w).is_none(), "{w:?}");
        }
    }

    #[test]
    fn a_unique_prefix_resolves_to_its_command() {
        assert_eq!(resolve_prefix("e").len(), 1);
        assert_eq!(resolve_prefix("e")[0].name, "exit");
        assert_eq!(resolve_prefix("m")[0].name, "model");
        // Longer prefixes stay shorthands too.
        assert_eq!(resolve_prefix("ver")[0].name, "verbose");
    }

    #[test]
    fn an_ambiguous_prefix_lists_every_candidate_in_registry_order() {
        let names: Vec<_> = resolve_prefix("s").iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["skin", "session"]);
        // A full name matches exactly itself.
        assert_eq!(resolve_prefix("skin").len(), 1);
    }

    #[test]
    fn an_unknown_prefix_matches_nothing() {
        assert!(resolve_prefix("zz").is_empty());
        assert!(resolve_prefix("modelx").is_empty());
    }

    #[test]
    fn completions_cover_names_and_aliases() {
        let all = completion_candidates("/");
        for c in SLASH_COMMANDS {
            for n in c.all_names() {
                assert!(all.iter().any(|s| *s == format!("/{n}")), "missing /{n} in {all:?}");
            }
        }
        // A prefix narrows.
        let e = completion_candidates("/e");
        assert!(e.iter().any(|s| s == "/exit"));
        assert!(!e.iter().any(|s| s == "/model"));
        // Non-slash input completes nothing.
        assert!(completion_candidates("help").is_empty());
    }
}
