//! Interactive onboarding flows: first-run provider/model wizard and the
//! standalone skin picker (`--select-skin`).

use grace::settings::PROVIDER_PRESETS;
use grace::transport::ProviderTransport;

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
pub(crate) fn run_onboarding_wizard() -> Result<(String, String, String), Box<dyn std::error::Error>> {
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
        match raw.parse::<usize>() {
            Ok(n) if n >= 1 && n <= PROVIDER_PRESETS.len() => break n - 1,
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
        grace::transport_copilot::get_or_create_token()?
    } else {
        match std::env::var(preset.env_var).ok().filter(|k| !k.is_empty()) {
            Some(k) => k,
            None => prompt_read(&format!(
                "API key for {} (or set ${} and re-run): ",
                preset.label, preset.env_var
            ))
            .ok_or_else(no_stdin)?,
        }
    };

    // Step 3: ask the provider itself what models it has, rather than
    // trusting only the hard-coded preset list. Falls back to the preset
    // (or free-typed input) if the live call fails or returns nothing.
    let live_models: Vec<grace::transport::ModelInfo> = if is_copilot {
        grace::transport_copilot::fetch_models(&api_key).unwrap_or_default()
    } else if !base_url.is_empty() {
        grace::transport_http::HttpTransport::new(base_url.clone(), api_key.clone())
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
            if let Ok(n) = raw.parse::<usize>() {
                if n >= 1 && n <= live_models.len() {
                    let m = &live_models[n - 1];
                    break (m.id.clone(), m.context_window);
                }
                if n == live_models.len() + 1 {
                    let typed = prompt_read("model id: ").ok_or_else(no_stdin)?;
                    let ctx = crate::chat::fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
            }
            println!("enter a valid number");
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
            if let Ok(n) = raw.parse::<usize>() {
                if n >= 1 && n <= preset.models.len() {
                    let m = &preset.models[n - 1];
                    break (m.id.to_string(), Some(m.context_window));
                }
                if n == preset.models.len() + 1 {
                    let typed = prompt_read("model id: ").ok_or_else(no_stdin)?;
                    let ctx = crate::chat::fetch_context_window(&typed, &base_url, &api_key);
                    break (typed, ctx);
                }
            }
            println!("enter a valid number");
        }
    } else {
        (prompt_read("model id: ").ok_or_else(no_stdin)?, None)
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
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".grace")
        .join("history.txt");
    let mut reader = crate::line_reader::LineReader::new(history_path, grace::skin::SOLARIS);
    let Some(picked) = crate::chat::pick_skin_interactive(&names, &mut reader) else {
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
