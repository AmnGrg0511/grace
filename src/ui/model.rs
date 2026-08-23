//! The `/model`, `/provider` and `/effort` command domain: enumerate the
//! endpoints and models a transport can switch to, interactively pick one,
//! and apply + persist the switch. Pulled out of `chat.rs` so the REPL loop
//! stays that file's single focus.

use crate::config::settings::{PROVIDER_PRESETS};
use crate::ui::status::human_tokens;
use crate::ui::line_reader::LineReader;
use crate::ui::skin::Skin;

/// One destination `/model` can switch to: a named endpoint with the model
/// id to point it at, plus display bookkeeping.
#[derive(Debug, Clone)]
struct ModelCandidate {
    name: String,
    model: String,
    base_url: String,
    key_env: String,
    context_window: Option<u32>,
}

/// Everything `/model` can target right now: the endpoints the user has
/// previously switched to (persisted in settings) plus the known provider
/// presets' models. De-duplicated by (base_url, model) — a persisted entry
/// for the same target keeps its (custom) name.
fn model_candidates(settings: &crate::config::settings::Settings) -> Vec<ModelCandidate> {
    let mut out: Vec<ModelCandidate> = Vec::new();
    let mut known: Vec<(String, String)> = Vec::new();
    // The endpoint the user is currently configured on, so `/model` can
    // always offer "switch back to where I am now" without the user typing
    // its URL as a custom endpoint.
    if let (Some(url), Some(model)) = (&settings.default_base_url, &settings.default_model) {
        out.push(ModelCandidate {
            name: friendly_endpoint_name(url),
            model: model.clone(),
            base_url: url.clone(),
            key_env: crate::config::settings::env_var_for_base_url(url).to_string(),
            context_window: settings
                .default_context_window
                .or_else(|| crate::config::settings::context_window_for(model)),
        });
        known.push((url.clone(), model.clone()));
    }
    for e in &settings.endpoints {
        if known.contains(&(e.base_url.clone(), e.model.clone())) {
            continue;
        }
        out.push(ModelCandidate {
            name: e.name.clone(),
            model: e.model.clone(),
            base_url: e.base_url.clone(),
            key_env: e.key_env.clone(),
            context_window: None,
        });
        known.push((e.base_url.clone(), e.model.clone()));
    }
    for preset in PROVIDER_PRESETS {
        for m in preset.models {
            if known.contains(&(preset.base_url.to_string(), m.id.to_string())) {
                continue;
            }
            out.push(ModelCandidate {
                name: preset.label.to_string(),
                model: m.id.to_string(),
                base_url: preset.base_url.to_string(),
                key_env: preset.env_var.to_string(),
                context_window: Some(m.context_window),
            });
        }
    }
    out
}

/// `/model <query>` resolution, pure so it is testable: an exact
/// (case-insensitive) match on endpoint name or model id wins outright;
/// otherwise a case-insensitive substring match. Returns 0 (no match), 1
/// (unambiguous), or N (ambiguous — the caller shows the list).
fn match_model_query(items: &[ModelCandidate], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    let exact: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name.eq_ignore_ascii_case(&q) || c.model.eq_ignore_ascii_case(&q))
        .map(|(i, _)| i)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    items
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.name.to_lowercase().contains(&q) || c.model.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Print a candidate compactly: `name — model (base_url)`.
fn describe_candidate(c: &ModelCandidate) -> String {
    let ctx = match c.context_window {
        Some(w) => format!("{}k ctx", w / 1000),
        None => String::new(),
    };
    if c.base_url.is_empty() {
        format!("{} — {} (custom endpoint)", c.name, c.model)
    } else {
        format!(
            "{} — {} ({} · {})",
            c.name,
            c.model,
            short_url(&c.base_url),
            if ctx.is_empty() { "?" } else { ctx.as_str() }
        )
    }
}

/// Host-only view of a base URL for compact picker rows.
pub(crate) fn short_url(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// A human label for an endpoint URL: the host without scheme, port, common
/// `api.`/`www.` prefix, and TLD. `https://api.siemens.com/llm/v1` →
/// `siemens`; `http://localhost:11434/v1` → `localhost`.
fn friendly_endpoint_name(url: &str) -> String {
    let host = short_url(url);
    let host = host.split(':').next().unwrap_or(&host);
    let host = host
        .strip_prefix("api.")
        .or_else(|| host.strip_prefix("www."))
        .unwrap_or(host);
    let mut labels = host.split('.');
    let first = labels.next().unwrap_or(host);
    // Drop the TLD for multi-label hosts (`api.siemens.com` → `siemens`),
    // keep single-label hosts (`localhost`) as-is.
    if labels.next().is_some() {
        first.to_string()
    } else {
        host.to_string()
    }
}

/// Fetch the target endpoint's own model list (best-effort; a slow or
/// keyless endpoint simply yields the static list).
fn fetched_models(base_url: &str, key_env: &str) -> Vec<crate::transport::ModelInfo> {
    if base_url.is_empty() {
        return Vec::new();
    }
    if base_url == crate::transport::copilot::BASE_URL {
        return Vec::new(); // copilot enumerates via its own session flow
    }
    let key = std::env::var(key_env).unwrap_or_default();
    let transport = crate::transport::http::HttpTransport::new(base_url, key);
    crate::transport::ProviderTransport::list_models(&transport).unwrap_or_default()
}

/// `/model` — pick an endpoint (no arg) or resolve one (`<name>`), then
/// switch the transport to it. Persists the choice (model, endpoint, and
/// context window) to ~/.grace/config.toml so it sticks across restarts.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_model_command(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    arg: &str,
    reader: &mut LineReader,
    skin: &Skin,
    cached_context_window: &mut Option<u32>,
) {
    if transport.current_model().is_none() {
        println!(
            "this transport ({}) has no switchable model.",
            transport.name()
        );
        return;
    }
    let settings = crate::config::settings::Settings::load();
    let candidates = model_candidates(&settings);

    let target = if arg.is_empty() {
        match pick_model_interactive(&candidates, reader, skin, transport) {
            Some(t) => t,
            None => return,
        }
    } else {
        match match_model_query(&candidates, arg) {
            m if m.len() == 1 => candidates[m[0]].clone(),
            m if m.is_empty() => {
                println!("no model or endpoint matching \"{arg}\". Available:");
                for c in &candidates {
                    println!("  {}", describe_candidate(c));
                }
                println!("(or /model with no argument for the picker)");
                return;
            }
            m => {
                println!("\"{arg}\" matches more than one target:");
                for &i in &m {
                    println!("  {}", describe_candidate(&candidates[i]));
                }
                println!("be more specific, or /model with no argument for the picker.");
                return;
            }
        }
    };

    apply_model_switch(transport, target, reader, cached_context_window);
}

/// Execute a resolved switch: key handling (env/OAuth/prompt), endpoint
/// re-point, model swap, context-window re-resolution + change notice, and
/// persistence (config.toml endpoint upsert + .env key upsert).
#[allow(clippy::too_many_arguments)]
fn apply_model_switch(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    target: ModelCandidate,
    reader: &mut LineReader,
    cached_context_window: &mut Option<u32>,
) {
    let is_copilot_target = target.base_url == crate::transport::copilot::BASE_URL;
    let same_endpoint = transport
        .current_base_url()
        .as_deref()
        .map(|b| short_url(b) == short_url(&target.base_url))
        .unwrap_or(false);

    // A transport whose endpoint is fixed (Copilot) can swap models for its
    // own endpoint but never re-point mid-session. Persist the choice so the
    // *next* launch starts on the new endpoint, and say exactly that.
    if !same_endpoint && !transport.can_repoint_endpoint() {
        let window = target
            .context_window
            .or_else(|| crate::config::settings::context_window_for(&target.model));
        persist_model_choice(&target, window);
        println!(
            "this session is bound to the {} endpoint and cannot re-point mid-chat — this session keeps running; your next `grace` launch starts on {name}.",
            transport.name(),
            name = target.name
        );
        return;
    }

    if !same_endpoint {
        let key = if is_copilot_target {
            // Copilot uses the OAuth device flow, not a typed API key —
            // same path as the onboarding wizard.
            match crate::transport::copilot::get_or_create_token() {
                Ok(t) => Some(t),
                Err(e) => {
                    println!("copilot auth failed: {e}");
                    None
                }
            }
        } else {
            std::env::var(&target.key_env)
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .or_else(|| {
                    // Re-prompt on empty input (an empty key would be
                    // persisted and sent as an empty bearer); bail only on EOF.
                    loop {
                        let Some(raw) = reader
                            .read_line(&format!(
                                "API key for {} (${env_var} not set): ",
                                target.name,
                                env_var = target.key_env
                            ))
                        else {
                            break None;
                        };
                        let trimmed = raw.trim().to_string();
                        if trimmed.is_empty() {
                            println!("key is empty — type it again");
                            continue;
                        }
                        break Some(trimmed);
                    }
                })
        };
        let Some(key) = key else {
            if !is_copilot_target {
                println!("no key provided — staying on the current endpoint.");
            }
            return;
        };
        transport.set_endpoint(&target.base_url, &key);
        if !is_copilot_target {
            // Per-key upsert: a whole-file rewrite would wipe the other
            // providers' keys every time `/model` switched providers.
            if let Err(e) = crate::ui::cli::upsert_env_file(&target.key_env, &key) {
                eprintln!(
                    "[grace] warning: could not save {}: {e}",
                    crate::ui::cli::env_file_path().display()
                );
            }
        }
    }

    transport.set_model(&target.model);
    // Re-resolve the window now that model/endpoint changed (the swap
    // invalidated the transport's cache). The static-list value, if any,
    // avoids a network probe the picker already answered.
    let resolved = target.context_window.or(transport.context_window());
    if let (Some(old_w), Some(new_w)) = (*cached_context_window, resolved) {
        if old_w != new_w {
            println!(
                "note: context window changed {} -> {} (compaction budget follows).",
                human_tokens(old_w as u64),
                human_tokens(new_w as u64)
            );
        }
    }
    *cached_context_window = resolved;

    let mut settings = crate::config::settings::Settings::load();
    commit_model_choice(&mut settings, &target, resolved);
    if settings.save().is_err() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml");
        return;
    }
    println!("model switched to \"{}\" (saved to config).", target.model);
}

/// Write the chosen model/endpoint/window into settings (the endpoint list
/// stays one-entry-per-endpoint so the picker's list follows usage).
fn commit_model_choice(
    settings: &mut crate::config::settings::Settings,
    target: &ModelCandidate,
    context_window: Option<u32>,
) {
    settings.default_model = Some(target.model.clone());
    settings.default_context_window = context_window;
    settings.default_base_url = Some(target.base_url.clone());
    crate::config::settings::upsert_endpoint(
        &mut settings.endpoints,
        crate::config::settings::Endpoint {
            name: target.name.clone(),
            base_url: target.base_url.clone(),
            model: target.model.clone(),
            key_env: target.key_env.clone(),
        },
    );
}

/// Persist-for-next-launch variant used when the running transport can't
/// re-point: the change lands on the next startup, not this session.
fn persist_model_choice(target: &ModelCandidate, context_window: Option<u32>) {
    let mut settings = crate::config::settings::Settings::load();
    commit_model_choice(&mut settings, target, context_window);
    if let Err(e) = settings.save() {
        eprintln!("[grace] warning: could not save ~/.grace/config.toml: {e}");
    }
}

/// `/model` (no arg) — the model list for the endpoint this session is
/// already pointed at: what the endpoint reports, merged with the known
/// static models, plus an "other" escape hatch for ids we don't know. The
/// provider/endpoint is *not* re-chosen here — that is `/provider` — so this
/// is a single, focused model list. Returns `None` on cancel/EOF (no-op).
fn pick_model_interactive(
    candidates: &[ModelCandidate],
    reader: &mut LineReader,
    skin: &Skin,
    transport: &(dyn crate::transport::ProviderTransport + '_),
) -> Option<ModelCandidate> {
    let base_url = transport
        .current_base_url()
        .or_else(|| crate::config::settings::Settings::load().default_base_url.clone())
        .unwrap_or_default();
    if base_url.is_empty() {
        println!("no provider configured — /provider picks one first.");
        return None;
    }
    let name = friendly_endpoint_name(&base_url);
    let picked = endpoint_pick(candidates, &name, &base_url);
    let fetched = fetched_models(&picked.base_url, &picked.key_env);
    // The endpoint is unchanged, so the active model is always the anchor
    // that should be marked in the list.
    let (model, ctx) = pick_model_for_endpoint(
        fetched,
        &picked.known_models,
        reader,
        skin,
        transport.current_model().as_deref(),
    )?;
    Some(ModelCandidate {
        name: picked.name,
        model,
        base_url: picked.base_url,
        key_env: picked.key_env,
        context_window: ctx,
    })
}

/// One resolved provider choice: where requests go and what we know about it.
struct EndpointPick {
    name: String,
    base_url: String,
    key_env: String,
    known_models: Vec<(String, Option<u32>)>,
}

/// The provider stage of both `/model` and `/provider`: one row per distinct
/// (name, base_url) — persisted endpoints plus presets — plus an escape hatch
/// for a custom OpenAI-compatible URL. Resolves the endpoint's key env var
/// and known-model list from the candidates.
fn pick_provider_endpoint(
    candidates: &[ModelCandidate],
    reader: &mut LineReader,
    skin: &Skin,
    title: &str,
) -> Option<EndpointPick> {
    use crate::ui::skin::{reset, Role};
    use crate::ui::picker::{pick, Pick};

    // Accent paint shared by the endpoint/context sublabels — a skin color
    // distinct from the dim row text, so the eye splits name from fact.
    let accent = |t: &str| skin.paint(Role::Code, t);

    // One row per distinct (name, base_url).
    let mut endpoints: Vec<(String, String)> = Vec::new();
    for c in candidates {
        if !endpoints.iter().any(|(n, u)| *n == c.name && *u == c.base_url) {
            endpoints.push((c.name.clone(), c.base_url.clone()));
        }
    }
    // Rows show only the provider and its endpoint (accent-painted); models
    // are a separate decision so the two choices stay clean.
    let items: Vec<Pick> = endpoints
        .iter()
        .map(|(name, base_url)| {
            Pick::new(format!("{name}\u{1}{base_url}"), name.clone())
                .hint(format!("{}{}", accent(&short_url(base_url)), reset()))
                .verbatim_sub()
        })
        .chain(std::iter::once(
            Pick::new("\u{1}custom", "Custom OpenAI-compatible endpoint").hint("type a base URL"),
        ))
        .collect();
    let choice = pick(&items, skin, title)?;

    let (name, base_url) = if choice == "\u{1}custom" {
        let raw = reader.read_line("base URL (e.g. https://host/v1): ")?;
        let url = raw.trim().to_string();
        if url.is_empty() {
            return None;
        }
        (friendly_endpoint_name(&url), url)
    } else {
        let mut it = choice.splitn(2, '\u{1}');
        (
            it.next().unwrap_or("").to_string(),
            it.next().unwrap_or("").to_string(),
        )
    };

    Some(endpoint_pick(candidates, &name, &base_url))
}

/// Resolve the key env var + known-model list for a `(name, base_url)` among
/// the candidates, falling back to the standard env-var naming for the URL
/// when no candidate carries an explicit key var.
fn endpoint_pick(
    candidates: &[ModelCandidate],
    name: &str,
    base_url: &str,
) -> EndpointPick {
    let (key_env, known): (String, Vec<(String, Option<u32>)>) = candidates
        .iter()
        .filter(|c| c.name == name && short_url(&c.base_url) == short_url(base_url))
        .fold(
        (String::new(), Vec::new()),
        |(mut ke, mut known), c| {
            if ke.is_empty() && !c.key_env.is_empty() {
                ke = c.key_env.clone();
            }
            known.push((c.model.clone(), c.context_window));
            (ke, known)
        },
    );
    let key_env = if key_env.is_empty() {
        crate::config::settings::env_var_for_base_url(base_url).to_string()
    } else {
        key_env
    };
    EndpointPick {
        name: name.to_string(),
        base_url: base_url.to_string(),
        key_env,
        known_models: known,
    }
}

/// The `/effort` picker: the reasoning-effort hint sent as
/// `reasoning_effort` on every chat request. Returns "" for "none" (send no
/// parameter), or "low"/"medium"/"high". The already-active level is shown
/// but not selectable.
fn pick_reasoning_level(current: Option<String>, skin: &Skin) -> Option<String> {
    use crate::ui::picker::{pick, Pick};

    let options: [(&str, &str); 4] = [
        ("none", "no reasoning parameter is sent"),
        ("low", "quick answers, light thinking"),
        ("medium", "balanced depth"),
        ("high", "deepest reasoning — slower and costlier"),
    ];
    let items: Vec<Pick> = options
        .iter()
        .map(|(id, blurb)| {
            let mut row = Pick::new(*id, *id).hint(*blurb);
            if current.as_deref() == Some(id) || (*id == "none" && current.is_none()) {
                row = row.disabled().hint("current");
            }
            row
        })
        .collect();
    match pick(&items, skin, "pick a reasoning level")? {
        id if id == "none" => Some(String::new()),
        id => Some(id.to_string()),
    }
}

/// `/provider` — switch endpoint without touching the model decision. The
/// current model id carries over; if the new endpoint reports its models and
/// doesn't have it, the shared stage-2 picker opens so the session lands on
/// something that actually exists there.
pub(crate) fn handle_provider_command(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    reader: &mut LineReader,
    skin: &Skin,
    cached_context_window: &mut Option<u32>,
) {
    use crate::ui::skin::Role;
    if transport.current_model().is_none() {
        println!(
            "this transport ({}) has no switchable model — /model sets one first.",
            transport.name()
        );
        return;
    }
    let settings = crate::config::settings::Settings::load();
    let candidates = model_candidates(&settings);
    let Some(picked) = pick_provider_endpoint(&candidates, reader, skin, "switch provider")
    else {
        return;
    };
    let same = transport
        .current_base_url()
        .as_deref()
        .map(|b| short_url(b) == short_url(&picked.base_url))
        .unwrap_or(false);
    if same {
        println!("already on {name} ({url}).", name = picked.name, url = short_url(&picked.base_url));
        return;
    }

    // Carry the model over untouched — that is the whole point of
    // decoupling provider from model. If it turns out not to exist on the
    // new endpoint, the check below catches it against the live list.
    let model = transport.current_model().unwrap_or_default();
    let target = ModelCandidate {
        name: picked.name.clone(),
        model: model.clone(),
        base_url: picked.base_url.clone(),
        key_env: picked.key_env.clone(),
        context_window: None,
    };
    apply_model_switch(transport, target, reader, cached_context_window);

    // Best-effort validation against the endpoint's own list (env-var key
    // only — no prompting here): an unknown model id would otherwise surface
    // as a confusing provider 404 mid-conversation.
    let key = std::env::var(&picked.key_env).ok().filter(|k| !k.is_empty());
    let ids: Vec<String> = fetched_models(&picked.base_url, key.as_deref().unwrap_or(""))
        .into_iter()
        .map(|m| m.id)
        .collect();
    if !ids.is_empty() && !ids.contains(&model) {
        println!(
            "{}",
            skin.paint_badge(
                Role::Error,
                &format!("note: \"{model}\" is not in {name}'s model list — pick one:", name = picked.name)
            )
        );
        let known: Vec<(String, Option<u32>)> = candidates
            .iter()
            .filter(|c| c.name == picked.name)
            .map(|c| (c.model.clone(), c.context_window))
            .collect();
        let fetched = fetched_models(&picked.base_url, std::env::var(&picked.key_env).ok().as_deref().unwrap_or(""));
        if let Some((m, ctx)) =
            pick_model_for_endpoint(fetched, &known, reader, skin, None)
        {
            transport.set_model(&m);
            if ctx.is_some() {
                *cached_context_window = ctx.or(*cached_context_window);
            }
            println!("model switched to \"{m}\".");
        }
    }
}

/// `/effort [low|medium|high|none]` — set the reasoning-effort hint on its
/// own, no model switch attached. No argument opens the picker. The choice
/// is per-request transport state plus a persisted default.
pub(crate) fn handle_effort_command(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    arg: &str,
    skin: &Skin,
) {
    use crate::ui::skin::Role;
    let apply = |level: Option<&str>| {
        transport.set_reasoning(level);
        let mut settings = crate::config::settings::Settings::load();
        settings.default_reasoning = level.map(str::to_string);
        if let Err(e) = settings.save() {
            eprintln!("[grace] warning: could not save reasoning level: {e}");
            return;
        }
        match level {
            Some(l) => println!(
                "{}",
                skin.paint_badge(Role::Success, &format!("reasoning effort set to {l}."))
            ),
            None => println!(
                "{}",
                skin.paint_badge(Role::Success, "reasoning effort cleared.")
            ),
        }
    };
    if arg.is_empty() {
        let Some(level) = pick_reasoning_level(transport.current_reasoning(), skin) else {
            return;
        };
        apply(if level.is_empty() { None } else { Some(level.as_str()) });
        return;
    }
    match arg.trim().to_ascii_lowercase().as_str() {
        "none" => apply(None),
        l @ ("low" | "medium" | "high") => apply(Some(l)),
        other => println!("usage: /effort [low|medium|high|none] — got '{other}'"),
    }
}

/// Stage 2 of the `/model` picker: the endpoint's reported models (best-
/// effort) merged with its known static models, plus "other". Returns
/// `(model_id, context_window_if_known)`.
pub(crate) fn pick_model_for_endpoint(
    fetched: Vec<crate::transport::ModelInfo>,
    known: &[(String, Option<u32>)],
    reader: &mut LineReader,
    skin: &Skin,
    current_model: Option<&str>,
) -> Option<(String, Option<u32>)> {
    use crate::ui::skin::{reset, Role};
    use crate::ui::picker::{pick, Pick};

    let mut models: Vec<(String, Option<u32>)> = Vec::new();
    for info in fetched {
        models.push((info.id, info.context_window));
    }
    for (id, ctx) in known {
        if !models.iter().any(|(m, _)| m == id) {
            models.push((id.clone(), *ctx));
        }
    }
    if models.is_empty() {
        let typed = reader.read_line("model id: ")?;
        let id = typed.trim().to_string();
        return if id.is_empty() { None } else { Some((id, None)) };
    }

    // Context window in the same skin accent as stage 1's endpoint; the
    // model this session already runs stays visible but unselectable.
    let items: Vec<Pick> = models
        .iter()
        .enumerate()
        .map(|(i, (id, ctx))| {
            let mut row = Pick::new(format!("m{i}"), id.clone());
            if let Some(w) = ctx {
                row = row
                    .hint(format!(
                        "{}{}",
                        skin.paint(Role::Code, &format!("{w}k")),
                        reset()
                    ))
                    .verbatim_sub();
            }
            if current_model == Some(id.as_str()) {
                row = row.disabled().hint("current");
            }
            row
        })
        .chain(std::iter::once(Pick::new(
            "other",
            "other — type a model id",
        )))
        .collect();
    let choice = pick(&items, skin, "pick a model")?;
    if choice == "other" {
        let typed = reader.read_line("model id: ")?;
        let id = typed.trim().to_string();
        return if id.is_empty() { None } else { Some((id, None)) };
    }
    let idx = choice.strip_prefix('m')?.parse::<usize>().ok()?;
    Some(models[idx].clone())
}


#[cfg(test)]
mod model_command_tests {
    use super::*;

    fn cand(name: &str, model: &str, base_url: &str, ctx: Option<u32>) -> ModelCandidate {
        ModelCandidate {
            name: name.into(),
            model: model.into(),
            base_url: base_url.into(),
            key_env: "SOME_KEY".into(),
            context_window: ctx,
        }
    }

    #[test]
    fn exact_model_match_wins_case_insensitively() {
        let items = vec![cand("OpenAI", "gpt-4o", "https://api.openai.com/v1", Some(128_000))];
        assert_eq!(match_model_query(&items, "GPT-4O"), vec![0]);
    }

    #[test]
    fn exact_endpoint_name_matches_only_that_endpoint() {
        let items = vec![
            cand(
                "OpenRouter",
                "openai/gpt-4o-mini",
                "https://openrouter.ai/api/v1",
                Some(128_000),
            ),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", Some(128_000)),
        ];
        assert_eq!(match_model_query(&items, "openai"), vec![1]);
    }

    #[test]
    fn substring_match_is_the_fallback_and_can_be_ambiguous() {
        let items = vec![
            cand("OpenRouter", "openai/gpt-4o-mini", "https://openrouter.ai/api/v1", None),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", None),
        ];
        // "gpt-4o" is no exact name/model; it substrings both models.
        assert_eq!(match_model_query(&items, "gpt-4o"), vec![0, 1]);
        assert!(match_model_query(&items, "flux").is_empty());
    }

    #[test]
    fn a_persisted_endpoint_dedups_the_preset_model() {
        let mut settings = crate::config::settings::Settings::default();
        settings.endpoints.push(crate::config::settings::Endpoint {
            name: "MyOpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            key_env: "OPENAI_API_KEY".into(),
        });
        let candidates = model_candidates(&settings);
        let hits: Vec<_> = candidates
            .iter()
            .filter(|c| c.model == "gpt-4o" && c.base_url == "https://api.openai.com/v1")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "MyOpenAI");
    }

    #[test]
    fn short_url_keeps_only_the_host() {
        assert_eq!(short_url("https://api.openai.com/v1"), "api.openai.com");
        assert_eq!(short_url("http://localhost:11434/v1"), "localhost:11434");
        assert_eq!(short_url("weird"), "weird");
    }

    #[test]
    fn friendly_endpoint_name_strips_scheme_prefix_and_port() {
        assert_eq!(friendly_endpoint_name("https://api.siemens.com/llm/v1"), "siemens");
        assert_eq!(friendly_endpoint_name("http://localhost:11434/v1"), "localhost");
        assert_eq!(friendly_endpoint_name("https://api.openai.com/v1"), "openai");
    }

    #[test]
    fn the_currently_configured_endpoint_is_a_candidate() {
        let settings = crate::config::settings::Settings {
            default_model: Some("qwen-3.8-27b".into()),
            default_base_url: Some("https://api.siemens.com/llm/v1".into()),
            default_context_window: Some(262_144),
            ..Default::default()
        };
        let candidates = model_candidates(&settings);
        let cur = candidates
            .iter()
            .find(|c| c.base_url == "https://api.siemens.com/llm/v1")
            .expect("configured endpoint is a candidate");
        assert_eq!(cur.name, "siemens");
        assert_eq!(cur.model, "qwen-3.8-27b");
        assert_eq!(cur.context_window, Some(262_144));
    }
}
