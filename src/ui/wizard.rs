//! Interactive onboarding flows: first-run provider/model wizard and the
//! standalone skin picker (`--select-skin`).

use crate::config::settings::PROVIDER_PRESETS;
use crate::transport::ProviderTransport;

/// Read one line from the terminal with echo suppressed — API keys must not
/// land on screen or in scrollback. Uses `stty -echo` against `/dev/tty`
/// (std-only, no crate); falls back to plain stdin reading when no TTY is
/// reachable (piped input can't hide anything anyway). Returns `None` on EOF.
fn read_hidden(prompt: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};

    print!("{prompt}");
    let _ = std::io::stdout().flush();

    let stty = |arg: &str| -> bool {
        let Ok(tty) = std::fs::File::open("/dev/tty") else {
            return false;
        };
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(std::process::Stdio::from(tty))
            .status()
            .is_ok_and(|s| s.success())
    };
    let hidden = stty("-echo");
    let line = (|| {
        let tty = std::fs::File::open("/dev/tty").ok()?;
        let mut buf = String::new();
        BufReader::new(tty).read_line(&mut buf).ok()?;
        Some(buf)
    })()
    .or_else(|| {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok()?;
        Some(buf)
    });
    if hidden {
        stty("echo");
    }
    // The typed Enter left the cursor mid-row; close the line invisibly.
    println!();
    line.map(|l| l.trim().to_string())
}

/// Interactive first-run setup, entirely picker-based: provider -> key ->
/// model -> reasoning level. Every provider goes through the exact same
/// steps — the only branch is *how* the "key" step is obtained: typed with
/// echo suppressed for a normal HTTP provider, or minted via OAuth device
/// flow for GitHub Copilot. Once the key exists, Copilot is wired up exactly
/// like any other provider (base_url + api_key), no separate flag or code
/// path downstream. The model stage asks the provider's real `/models`
/// endpoint for the live list + context windows, falling back to the
/// built-in preset list only if that call fails.
///
/// Persists the choice to `~/.grace/config.toml` (model/base_url/reasoning)
/// and `~/.grace/.env` (the key, so it's never asked twice and never lives
/// in shell history). Returns (model, base_url, api_key) to wire up for
/// *this* invocation.
pub fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
    use crate::ui::picker::{pick, Pick};
    use crate::ui::skin::{reset, Role};

    println!(
        "\ngrace needs a model provider — this only runs once, choices are saved to ~/.grace/\n"
    );
    let skin = crate::ui::skin::by_name(None);
    let accent = |t: &str| skin.paint(Role::Code, t);

    // Stage 1: provider — name plus its endpoint in the accent color.
    let items: Vec<Pick> = PROVIDER_PRESETS
        .iter()
        .map(|p| {
            if p.base_url.is_empty() {
                Pick::new(p.label, p.label).hint("type the base URL next")
            } else {
                Pick::new(p.label, p.label)
                    .hint(format!("{}{}", accent(&crate::ui::model::short_url(p.base_url)), reset()))
                    .verbatim_sub()
            }
        })
        .collect();
    let choice = pick(&items, &skin, "pick a provider").ok_or("no provider selected")?;
    let preset = PROVIDER_PRESETS
        .iter()
        .find(|p| p.label == choice)
        .ok_or("unknown provider selection")?;
    let is_copilot = preset.label == "GitHub Copilot";

    let base_url = if is_copilot || !preset.base_url.is_empty() {
        preset.base_url.to_string()
    } else {
        let raw = read_hidden("base URL (OpenAI-compatible /chat/completions endpoint): ")
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
            let Some(typed) = read_hidden(&format!(
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
    let live_models: Vec<crate::transport::ModelInfo> = if is_copilot {
        crate::transport::copilot::fetch_models(&api_key).unwrap_or_default()
    } else if !base_url.is_empty() {
        crate::transport::http::HttpTransport::new(base_url.clone(), api_key.clone())
            .list_models()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if live_models.is_empty() && !preset.models.is_empty() {
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
    if let Err(e) = crate::ui::cli::upsert_env_file(preset.env_var, &api_key) {
        eprintln!(
            "[grace] warning: could not save {}: {e}",
            crate::ui::cli::env_file_path().display()
        );
    }
    println!("\nsaved — future runs won't ask again. edit ~/.grace/config.toml or ~/.grace/.env to change.\n");

    Ok((model, base_url, api_key))
}

/// Interactive skin picker with color previews, shared with `/skin`
/// mid-chat; persists the choice to `~/.grace/config.toml`. The
/// already-active skin is shown but not selectable.
pub fn run_skin_picker() -> Result<(), Box<dyn std::error::Error>> {
    use crate::ui::picker::{pick, Pick};

    let names = crate::ui::skin::all_names();
    if names.is_empty() {
        println!("no skins available.");
        return Ok(());
    }
    let settings = crate::config::settings::Settings::load();
    let active_skin = crate::ui::skin::by_name(settings.skin.as_deref());
    let items: Vec<Pick> = names
        .iter()
        .map(|n| {
            let mut row = Pick::new(n.clone(), n.clone()).extra_lines(
                crate::ui::chat::skin_preview_lines(&crate::ui::skin::by_name(Some(n))),
            );
            if settings.skin.as_deref() == Some(n.as_str()) {
                row = row.disabled().hint("already active");
            }
            row
        })
        .collect();
    let Some(picked) = pick(&items, &active_skin, "pick a skin") else {
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
}
