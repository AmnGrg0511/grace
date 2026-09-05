//! Interactive onboarding flow: the first-run provider/model wizard.

use crate::config::settings::PROVIDER_PRESETS;
use std::io::Write;

/// Shared raw-mode line read from a real terminal. `mask` true prints one `*`
/// per key so a secret never lands on screen or in scrollback; false echoes
/// the characters so the user can verify the URL they typed. Enter confirms;
/// Esc / Ctrl-C / Ctrl-D abort (returns None). Returns `None` when no real
/// terminal is reachable so the caller can fall back to a plain stdin read
/// (piped input can't hide anything anyway).
fn read_term_line(prompt: &str, mask: bool) -> Option<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    print!("{prompt}");
    let _ = std::io::stdout().flush();

    let chars = match enable_raw_mode() {
        Err(_) => None,
        Ok(()) => {
            let mut chars: Vec<char> = Vec::new();
            let answer = loop {
                match event::read() {
                    Err(_) => break None,
                    Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                        if k.modifiers.contains(KeyModifiers::CONTROL) {
                            match k.code {
                                KeyCode::Char('c') | KeyCode::Char('d') => break None,
                                _ => {}
                            }
                        }
                        match k.code {
                            KeyCode::Enter => break Some(chars),
                            KeyCode::Esc => break None,
                            KeyCode::Backspace => {
                                if chars.pop().is_some() {
                                    let _ = write!(
                                        std::io::stdout(),
                                        "{}",
                                        if mask { "\x1b[1D\x1b[0K" } else { "\x08 \x08" }
                                    );
                                }
                            }
                            KeyCode::Char(c) if !c.is_control() => {
                                chars.push(c);
                                let _ = if mask {
                                    write!(std::io::stdout(), "*")
                                } else {
                                    write!(std::io::stdout(), "{c}")
                                };
                            }
                            _ => {}
                        }
                        let _ = std::io::stdout().flush();
                    }
                    Ok(_) => {}
                }
            };
            let _ = disable_raw_mode();
            answer
        }
    };
    // Close the line whether or not anything was typed.
    println!();
    chars.and_then(|c| {
        let s = c.into_iter().collect::<String>();
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    })
}

/// Read one line with visible echo (the base URL must appear on screen so the
/// user can verify what they typed). Falls back to a plain stdin read when no
/// real terminal is reachable.
fn read_line(prompt: &str) -> Option<String> {
    read_term_line(prompt, false).or_else(|| {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        let t = line.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    })
}

/// Read one line with masked echo — each key prints a `*`, so the API key
/// never lands on screen or in scrollback; backspace erases a star. Falls
/// back to a plain stdin read when no real terminal is reachable (piped input
/// can't hide anything anyway).
fn read_masked(prompt: &str) -> Option<String> {
    read_term_line(prompt, true).or_else(|| {
        // Piped input: nothing to hide, read the line as-is.
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        let t = line.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    })
}

/// Interactive first-run setup, entirely picker-based: provider -> key ->
/// model -> reasoning level. Every provider goes through the exact same
/// steps — the only branch is *how* the "key" step is obtained: typed with
/// masked echo (each key prints `*`) for a normal HTTP provider, or minted
/// via OAuth device flow for GitHub Copilot. Once the key exists, Copilot is
/// wired up exactly like any other provider (base_url + api_key), no separate
/// flag or code path downstream. The model stage asks the provider's real
/// `/models` endpoint for the live list + context windows, falling back to the
/// built-in preset list only if that call fails.
///
/// Persists the choice to `~/.grace/config.toml` (model/base_url/reasoning)
/// and `~/.grace/.env` (the key, so it's never asked twice and never lives
/// in shell history). Returns (model, base_url, api_key) to wire up for
/// *this* invocation.
pub fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
    use crate::ui::picker::{pick, Pick};

    println!(
        "\ngrace needs a model provider — this only runs once, choices are saved to ~/.grace/\n"
    );
    let skin = crate::ui::skin::by_name(None);

    // Stage 1: provider — name plus its endpoint as the padded hint chip.
    let items: Vec<Pick> = PROVIDER_PRESETS
        .iter()
        .map(|p| {
            if p.base_url.is_empty() {
                Pick::new(p.label, p.label).hint("type the base URL next")
            } else {
                Pick::new(p.label, p.label)
                    .hint(crate::ui::model::short_url(p.base_url))
            }
        })
        .collect();
    let choice = pick(&items, &skin, "pick a provider").ok_or("no provider selected")?;
    let preset = PROVIDER_PRESETS
        .iter()
        .find(|p| p.label == choice)
        .ok_or("unknown provider selection")?;
    let is_copilot = crate::transport::copilot::is_copilot_endpoint(preset.base_url);

    let base_url = if is_copilot || !preset.base_url.is_empty() {
        preset.base_url.to_string()
    } else {
        let raw = read_line("base URL (OpenAI-compatible /chat/completions endpoint): ")
            .ok_or("no base URL provided")?;
        if raw.is_empty() {
            return Err("no base URL provided".into());
        }
        raw
    };

    // Step 2, "the key": every provider gets asked for one. A normal HTTP
    // provider's key is typed in blind (or read from an already-exported env
    // var). Copilot's "key" is minted via OAuth device flow instead — same
    // conceptual step, just non-interactive input.
    let api_key = if is_copilot {
        crate::transport::copilot::get_or_create_token()?
    } else {
        // The env var wins if set; otherwise ask — re-prompting on empty
        // input, since an empty key would be persisted and sent as an empty
        // bearer instead of getting another chance.
        loop {
            if let Some(k) = std::env::var(preset.env_var).ok().filter(|k| !k.is_empty()) {
                break k;
            }
            let Some(typed) = read_masked(&format!(
                "API key for {} (${env_var} not set): ",
                preset.label,
                env_var = preset.env_var
            )) else {
                return Err("no API key provided".into());
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
    // list inside the shared stage-2 picker when the live call fails.
    // The factory builds the right transport per endpoint — for Copilot
    // that means the OAuth token is exchanged for a session token before
    // the /models call, instead of sending the OAuth token directly.
    // An authenticated fetch that fails is a real signal — say what the
    // endpoint reported instead of silently degrading to the static list.
    let live_models: Vec<crate::transport::ModelInfo> = if base_url.is_empty() {
        Vec::new()
    } else {
        crate::transport::transport_for(&base_url, &api_key, "", None)
            .list_models()
            .unwrap_or_else(|e| {
                if !api_key.is_empty() {
                    println!("(couldn't load the model list: {e})");
                }
                Vec::new()
            })
    };
    if live_models.is_empty() && api_key.is_empty() && !preset.models.is_empty() {
        println!("(couldn't reach {}'s /models endpoint — showing known models)", preset.label);
    }

    let known: Vec<(String, Option<u32>)> = preset
        .models
        .iter()
        .map(|m| (m.id.to_string(), Some(m.context_window)))
        .collect();
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("history.txt");
    let mut reader = crate::ui::line_reader::LineReader::new(history_path, skin);
    let Some((model, ctx_window)) =
        crate::ui::model::pick_model_for_endpoint(live_models, &known, &mut reader, &skin, None)
    else {
        return Err("no model selected".into());
    };

    // Persist: model + base_url + context window go to config.toml; the
    // key goes to .env (kept separate so config.toml can be safely shared).
    let mut settings = crate::config::settings::Settings::load();
    settings.default_model = Some(model.clone());
    settings.default_base_url = Some(base_url.clone());
    settings.default_context_window = ctx_window;
    settings.default_reasoning = None;
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    }
    // Per-key upsert: a whole-file rewrite here would wipe the other
    // providers' keys every time onboarding ran for a new one.
    if let Err(e) = crate::config::env::upsert_env_file(preset.env_var, &api_key) {
        eprintln!(
            "[grace] warning: could not save {}: {e}",
            crate::config::env::env_file_path().display()
        );
    }
    println!("\nsaved — future runs won't ask again. edit ~/.grace/config.toml or ~/.grace/.env to change.\n");

    Ok((model, base_url, api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_preset_is_selectable_and_labelled() {
        // A preset with no label or an out-of-range index would make the
        // first-run wizard unusable.
        assert!(!PROVIDER_PRESETS.is_empty());
        for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
            assert!(!p.label.is_empty(), "preset {i} has no label");
            assert!(
                !p.env_var.is_empty(),
                "{} has no env var, so an exported key can never be picked up",
                p.label
            );
        }
    }

    #[test]
    fn the_terminal_line_read_is_unavailable_without_a_tty() {
        // Tests run without a PTY: the raw-mode read must report None so the
        // caller can degrade to a plain stdin read rather than hang.
        assert!(read_term_line("prompt: ", false).is_none());
        assert!(read_term_line("prompt: ", true).is_none());
    }
}
