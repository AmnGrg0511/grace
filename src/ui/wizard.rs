//! Interactive onboarding flows: first-run provider/model wizard and the
//! standalone skin picker (`--select-skin`).

use crate::config::settings::PROVIDER_PRESETS;
use crate::transport::ProviderTransport;

/// What a numbered menu entry the user typed refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// A zero-based index into the listed options.
    Item(usize),
    /// The trailing "other (type it yourself)" entry.
    Other,
    /// Not a number, or out of range.
    Invalid,
}

/// Interpret a typed menu selection against a list of `len` options where
/// entry `len + 1` is an "other" escape hatch.
///
/// Pure, so the off-by-one boundaries — which are easy to get wrong and
/// impossible to notice without walking through the wizard by hand — are
/// directly testable.
pub fn parse_menu_choice(raw: &str, len: usize, has_other: bool) -> MenuChoice {
    let Ok(n) = raw.trim().parse::<usize>() else {
        return MenuChoice::Invalid;
    };
    if n >= 1 && n <= len {
        return MenuChoice::Item(n - 1);
    }
    if has_other && n == len + 1 {
        return MenuChoice::Other;
    }
    MenuChoice::Invalid
}

/// Interactive first-run picker: provider -> key -> model. Every provider
/// goes through the exact same three steps — the only branch is *how* step
/// 2 (the "key") is obtained: typed for a normal HTTP provider, or minted
/// via OAuth device flow for GitHub Copilot. Once the key exists, Copilot
/// is wired up exactly like any other provider (base_url + api_key), no
/// separate flag or code path downstream. Step 3 asks the provider's real
/// `/models` endpoint for the live list + context windows, falling back to
/// the built-in preset list only if that call fails.
///
/// Persists the choice to `~/.grace/config.toml` (model/base_url) and
/// `~/.grace/.env` (the key, so it's never asked twice and never lives in
/// shell history). Returns (model, base_url, api_key) to wire up for *this*
/// invocation.
pub fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut stdin_lines = std::io::stdin().lines();
    // Returns `None` on real EOF (piped/closed stdin) so callers can bail
    // out with a clear error instead of looping forever re-prompting on an
    // input source that will never produce another line — e.g. `grace`
    // run under a non-interactive script/CI with no stdin attached.
    let mut prompt_read = |label: &str| -> Option<String> {
        print!("{label}");
        let _ = std::io::stdout().flush();
        Some(stdin_lines.next()?.ok()?.trim().to_string())
    };
    let no_stdin = || -> Box<dyn std::error::Error> {
        "no model/provider configured and stdin is not interactive (EOF) — \
         run `grace --base-url <url> --api-key <key> --model <id>` non-interactively, \
         or run once from a real terminal to complete onboarding"
            .into()
    };

    println!(
        "\ngrace needs a model provider — this only runs once, choices are saved to ~/.grace/\n"
    );
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let choice: usize = loop {
        let Some(raw) = prompt_read("\nselect a provider [number]: ") else {
            return Err(no_stdin());
        };
        match parse_menu_choice(&raw, PROVIDER_PRESETS.len(), false) {
            MenuChoice::Item(i) => break i,
            _ => println!("enter a number between 1 and {}", PROVIDER_PRESETS.len()),
        }
    };
    let preset = &PROVIDER_PRESETS[choice];
    let is_copilot = preset.label == "GitHub Copilot";

    let base_url = if is_copilot || !preset.base_url.is_empty() {
        preset.base_url.to_string()
    } else {
        prompt_read("base URL (OpenAI-compatible /chat/completions endpoint): ")
            .ok_or_else(no_stdin)?
    };

    // Step 2, "the key": every provider gets asked for one. A normal HTTP
    // provider's key is typed in (or read from an already-exported env
    // var). Copilot's "key" is minted via OAuth device flow instead — same
    // conceptual step, just non-interactive input.
    let api_key = if is_copilot {
        crate::transport::copilot::get_or_create_token()?
    } else {
        // The env var wins if set; otherwise ask — re-prompting on empty
        // input, since an empty key would be persisted and sent as an empty
        // bearer instead of getting another chance.
        loop {
            if let Some(k) =
                std::env::var(preset.env_var).ok().filter(|k| !k.is_empty())
            {
                break k;
            }
            let Some(typed) = prompt_read(&format!(
                "API key for {} (or set ${} and re-run): ",
                preset.label, preset.env_var
            )) else {
                return Err(no_stdin());
            };
            if typed.is_empty() {
                println!("key is empty — enter the key again");
                continue;
            }
            break typed;
        }
    };

    // Step 3: ask the provider itself what models it has, rather than
    // trusting only the hard-coded preset list. Falls back to the preset
    // (or free-typed input) if the live call fails or returns nothing.
    let live_models: Vec<crate::transport::ModelInfo> = if is_copilot {
        crate::transport::copilot::fetch_models(&api_key).unwrap_or_default()
    } else if !base_url.is_empty() {
        crate::transport::http::HttpTransport::new(base_url.clone(), api_key.clone())
            .list_models()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let (model, ctx_window) = if !live_models.is_empty() {
        println!();
        for (i, m) in live_models.iter().enumerate() {
            match m.context_window {
                Some(ctx) => println!("  {}) {} (context: {ctx})", i + 1, m.id),
                None => println!("  {}) {}", i + 1, m.id),
            }
        }
        println!("  {}) other (type a model id)", live_models.len() + 1);
        loop {
            let raw = prompt_read("\nselect a model [number]: ").ok_or_else(no_stdin)?;
            match parse_menu_choice(&raw, live_models.len(), true) {
                MenuChoice::Item(i) => {
                    let m = &live_models[i];
                    break (m.id.clone(), m.context_window);
                }
                MenuChoice::Other => {
                    let typed = prompt_read("model id: ").ok_or_else(no_stdin)?;
                    let ctx = crate::ui::chat::fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
                MenuChoice::Invalid => println!("enter a valid number"),
            }
        }
    } else if !preset.models.is_empty() {
        println!(
            "\n(couldn't reach {}'s /models endpoint — showing known models)",
            preset.label
        );
        for (i, m) in preset.models.iter().enumerate() {
            println!("  {}) {} (context: {})", i + 1, m.id, m.context_window);
        }
        println!("  {}) other (type a model id)", preset.models.len() + 1);
        loop {
            let raw = prompt_read("\nselect a model [number]: ").ok_or_else(no_stdin)?;
            match parse_menu_choice(&raw, preset.models.len(), true) {
                MenuChoice::Item(i) => {
                    let m = &preset.models[i];
                    break (m.id.to_string(), Some(m.context_window));
                }
                MenuChoice::Other => {
                    let typed = prompt_read("model id: ").ok_or_else(no_stdin)?;
                    let ctx = crate::ui::chat::fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
                MenuChoice::Invalid => println!("enter a valid number"),
            }
        }
    } else {
        (prompt_read("model id: ").ok_or_else(no_stdin)?, None)
    };

    // Persist: model + base_url + context window go to config.toml; the
    // key goes to .env (kept separate so config.toml can be safely shared).
    let mut settings = crate::config::settings::Settings::load();
    settings.default_model = Some(model.clone());
    settings.default_base_url = Some(base_url.clone());
    settings.default_context_window = ctx_window;
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    }
    // Per-key upsert: a whole-file rewrite here would wipe the other
    // providers' keys every time onboarding ran for a new one.
    if let Err(e) = crate::ui::cli::upsert_env_file(preset.env_var, &api_key) {
        eprintln!(
            "[grace] warning: could not save {}: {e}",
            crate::ui::cli::env_file_path().display()
        );
    }
    println!("\nsaved — future runs won't ask again. edit ~/.grace/config.toml or ~/.grace/.env to change.\n");

    Ok((model, base_url, api_key))
}

/// Interactive skin picker: same list+preview flow as `/skin` mid-chat
/// ([`pick_skin_interactive`]), but persists the choice to
/// `~/.grace/config.toml` — same "choose once, remembered forever" pattern
/// as [`run_onboarding_wizard`]'s provider pick.
pub fn run_skin_picker() -> Result<(), Box<dyn std::error::Error>> {
    let names = crate::ui::skin::all_names();
    if names.is_empty() {
        println!("no skins available.");
        return Ok(());
    }
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("history.txt");
    let mut reader = crate::ui::line_reader::LineReader::new(history_path, crate::ui::skin::SOLARIS);
    let Some(picked) = crate::ui::chat::pick_skin_interactive(&names, &mut reader) else {
        return Ok(());
    };

    let mut settings = crate::config::settings::Settings::load();
    settings.skin = Some(picked.clone());
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    } else {
        println!("\nskin set to \"{picked}\" — saved to ~/.grace/config.toml.\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_selection_maps_to_a_zero_based_index() {
        assert_eq!(parse_menu_choice("1", 3, true), MenuChoice::Item(0));
        assert_eq!(parse_menu_choice("3", 3, true), MenuChoice::Item(2));
    }

    #[test]
    fn the_trailing_entry_is_the_other_escape_hatch() {
        assert_eq!(parse_menu_choice("4", 3, true), MenuChoice::Other);
    }

    #[test]
    fn zero_is_invalid_because_menus_are_one_based() {
        // The classic off-by-one: accepting 0 would silently select item 1
        // via an underflowing subtraction.
        assert_eq!(parse_menu_choice("0", 3, true), MenuChoice::Invalid);
    }

    #[test]
    fn a_number_past_the_end_is_invalid() {
        assert_eq!(parse_menu_choice("5", 3, true), MenuChoice::Invalid);
        assert_eq!(parse_menu_choice("99", 3, true), MenuChoice::Invalid);
    }

    #[test]
    fn without_an_other_entry_the_trailing_number_is_invalid() {
        assert_eq!(parse_menu_choice("4", 3, false), MenuChoice::Invalid);
    }

    #[test]
    fn non_numeric_input_is_invalid_rather_than_panicking() {
        assert_eq!(parse_menu_choice("gpt-4o", 3, true), MenuChoice::Invalid);
        assert_eq!(parse_menu_choice("", 3, true), MenuChoice::Invalid);
        assert_eq!(parse_menu_choice("-1", 3, true), MenuChoice::Invalid);
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(parse_menu_choice("  2  ", 3, true), MenuChoice::Item(1));
    }

    #[test]
    fn an_empty_menu_accepts_only_the_other_entry() {
        assert_eq!(parse_menu_choice("1", 0, true), MenuChoice::Other);
        assert_eq!(parse_menu_choice("1", 0, false), MenuChoice::Invalid);
    }

    #[test]
    fn every_provider_preset_is_selectable_and_labelled() {
        // A preset with no label or an out-of-range index would make the
        // first-run wizard unusable.
        assert!(!PROVIDER_PRESETS.is_empty());
        for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
            assert!(!p.label.is_empty(), "preset {i} has no label");
            assert_eq!(
                parse_menu_choice(&(i + 1).to_string(), PROVIDER_PRESETS.len(), false),
                MenuChoice::Item(i)
            );
        }
    }

    #[test]
    fn each_preset_names_the_env_var_its_key_is_read_from() {
        for p in PROVIDER_PRESETS {
            assert!(
                !p.env_var.is_empty(),
                "{} has no env var, so an exported key can never be picked up",
                p.label
            );
        }
    }
}
