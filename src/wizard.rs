//! Interactive onboarding flows: first-run provider/model wizard and the
//! standalone skin picker (`--select-skin`).

use grace::settings::PROVIDER_PRESETS;

use crate::chat::pick_skin_interactive;

/// Interactive first-run picker: provider -> API key -> model. Persists the
/// choice to `~/.grace/config.toml` (model/base_url) and `~/.grace/.env`
/// (the key, so it's never asked twice and never lives in shell history).
/// Returns (model, base_url, api_key) to use for *this* invocation.
pub(crate) fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut stdin_lines = std::io::stdin().lines();
    let mut prompt_read = |label: &str| -> String {
        print!("{label}");
        let _ = std::io::stdout().flush();
        stdin_lines
            .next()
            .and_then(|l| l.ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    println!(
        "\ngrace needs a model provider — this only runs once, choices are saved to ~/.grace/\n"
    );
    for (i, p) in PROVIDER_PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 1, p.label);
    }
    let choice: usize = loop {
        let raw = prompt_read("\nselect a provider [number]: ");
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= PROVIDER_PRESETS.len() => break n - 1,
            _ => println!("enter a number between 1 and {}", PROVIDER_PRESETS.len()),
        }
    };
    let preset = &PROVIDER_PRESETS[choice];

    // GitHub Copilot has no static API key: it authenticates via OAuth
    // device flow (browser + one-time code), driven by `CopilotTransport`
    // itself on first use. Asking for a key here would be asking for
    // something the user doesn't have and can't get — skip straight past
    // it and let `--copilot` trigger the device flow later.
    let is_copilot = preset.label == "GitHub Copilot";

    let base_url = if is_copilot || preset.base_url.is_empty() {
        if is_copilot {
            preset.base_url.to_string()
        } else {
            prompt_read("base URL (OpenAI-compatible /chat/completions endpoint): ")
        }
    } else {
        preset.base_url.to_string()
    };

    // Prefer an already-set env var (e.g. exported this shell session) so we
    // don't re-ask for a key the user already has available. Copilot never
    // prompts for one — it authenticates via device flow instead.
    let api_key = if is_copilot {
        println!(
            "\nGitHub Copilot doesn't use a static API key — you'll authorize \
             once via a device code in your browser the first time grace runs \
             with --copilot. No key needed here.\n"
        );
        String::new()
    } else {
        std::env::var(preset.env_var)
            .ok()
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| {
                prompt_read(&format!(
                    "API key for {} (or set ${} and re-run): ",
                    preset.label, preset.env_var
                ))
            })
    };

    let (model, ctx_window) = if preset.models.is_empty() {
        (prompt_read("model id: "), None)
    } else {
        println!();
        for (i, m) in preset.models.iter().enumerate() {
            println!("  {}) {} (context: {})", i + 1, m.id, m.context_window);
        }
        println!("  {}) other (type a model id)", preset.models.len() + 1);
        let picked: (String, Option<u32>) = loop {
            let raw = prompt_read("\nselect a model [number]: ");
            if let Ok(n) = raw.parse::<usize>() {
                if n >= 1 && n <= preset.models.len() {
                    let m = &preset.models[n - 1];
                    break (m.id.to_string(), Some(m.context_window));
                }
                if n == preset.models.len() + 1 {
                    let typed = prompt_read("model id: ");
                    let ctx = crate::chat::fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
            }
            println!("enter a valid number");
        };
        picked
    };

    // Persist: model + base_url + context window go to config.toml; the
    // key goes to .env (kept separate so config.toml can be safely shared).
    let mut settings = grace::settings::Settings::load();
    settings.default_model = Some(model.clone());
    settings.default_base_url = Some(base_url.clone());
    settings.default_context_window = ctx_window;
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    }
    // Copilot's token is written by the device-flow itself (see
    // `transport_copilot::get_or_create_token`), not here — writing an
    // empty key would clobber an already-authenticated token on a rerun.
    if is_copilot {
        println!("\nsaved — run grace --copilot --model {model} to authenticate.\n");
        return Ok((model, base_url, api_key));
    }
    let env_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join(".env");
    if let Some(parent) = env_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&env_path, format!("{}={}\n", preset.env_var, api_key)) {
        eprintln!(
            "[grace] warning: could not save {}: {e}",
            env_path.display()
        );
    }
    println!("\nsaved — future runs won't ask again. edit ~/.grace/config.toml or ~/.grace/.env to change.\n");

    Ok((model, base_url, api_key))
}

/// Interactive skin picker: same list+preview flow as `/skin` mid-chat
/// ([`pick_skin_interactive`]), but persists the choice to
/// `~/.grace/config.toml` — same "choose once, remembered forever" pattern
/// as [`run_onboarding_wizard`]'s provider pick.
pub(crate) fn run_skin_picker() -> Result<(), Box<dyn std::error::Error>> {
    let names = grace::skin::all_names();
    if names.is_empty() {
        println!("no skins available.");
        return Ok(());
    }
    let Some(picked) = pick_skin_interactive(&names) else {
        return Ok(());
    };

    let mut settings = grace::settings::Settings::load();
    settings.skin = Some(picked.clone());
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    } else {
        println!("\nskin set to \"{picked}\" — saved to ~/.grace/config.toml.\n");
    }
    Ok(())
}
