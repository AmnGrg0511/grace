//! Interactive onboarding flows: first-run provider/model wizard and the
//! standalone skin picker (`--select-skin`).

use grace::settings::PROVIDER_PRESETS;

use crate::chat::pick_skin_interactive;

/// What the onboarding wizard resolved to, for `main` to wire up.
pub(crate) enum WizardOutcome {
    /// Real OpenAI-compatible endpoint: model, base_url, api_key.
    Http(String, String, String),
    /// GitHub Copilot: device-flow auth already ran inside the wizard, so
    /// by the time this is returned the token is already on disk. Just
    /// the model remains to wire up.
    Copilot(String),
}

/// Interactive first-run picker: provider -> auth -> model. Persists the
/// choice to `~/.grace/config.toml` (model/base_url/provider) and
/// `~/.grace/.env` (the key, so it's never asked twice and never lives in
/// shell history). For GitHub Copilot, runs the OAuth device flow directly
/// (no separate `--copilot` invocation needed) and persists
/// `default_provider = "copilot"` so future bare `grace` runs pick it up
/// automatically.
pub(crate) fn run_onboarding_wizard() -> Result<WizardOutcome, Box<dyn std::error::Error>> {
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
    let is_copilot = preset.label == "GitHub Copilot";

    // GitHub Copilot has no static API key and no separate CLI dance to
    // remember: picking it here runs the OAuth device flow immediately
    // (browser + one-time code), same as `CopilotTransport::new` would do
    // on first use — then persists the pick so future bare `grace` runs
    // reconstruct Copilot automatically, no `--copilot` typed by hand.
    if is_copilot {
        let model = if preset.models.is_empty() {
            prompt_read("model id: ")
        } else {
            println!();
            for (i, m) in preset.models.iter().enumerate() {
                println!("  {}) {} (context: {})", i + 1, m.id, m.context_window);
            }
            println!("  {}) other (type a model id)", preset.models.len() + 1);
            loop {
                let raw = prompt_read("\nselect a model [number]: ");
                if let Ok(n) = raw.parse::<usize>() {
                    if n >= 1 && n <= preset.models.len() {
                        break preset.models[n - 1].id.to_string();
                    }
                    if n == preset.models.len() + 1 {
                        break prompt_read("model id: ");
                    }
                }
                println!("enter a valid number");
            }
        };

        // Runs (or reuses) the OAuth device flow and writes the token to
        // ~/.grace/.env itself — this IS the authorization step, done right
        // here in the picker instead of deferred to a later --copilot run.
        grace::transport_copilot::CopilotTransport::new(&model)?;

        let mut settings = grace::settings::Settings::load();
        settings.default_model = Some(model.clone());
        settings.default_provider = Some("copilot".to_string());
        if let Err(e) = settings.save() {
            eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
        }
        println!("\nsaved — future runs of grace will use Copilot automatically.\n");
        return Ok(WizardOutcome::Copilot(model));
    }

    let base_url = if preset.base_url.is_empty() {
        prompt_read("base URL (OpenAI-compatible /chat/completions endpoint): ")
    } else {
        preset.base_url.to_string()
    };

    // Prefer an already-set env var (e.g. exported this shell session) so we
    // don't re-ask for a key the user already has available.
    let api_key = std::env::var(preset.env_var)
        .ok()
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| {
            prompt_read(&format!(
                "API key for {} (or set ${} and re-run): ",
                preset.label, preset.env_var
            ))
        });

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
    settings.default_provider = None;
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
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

    Ok(WizardOutcome::Http(model, base_url, api_key))
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
