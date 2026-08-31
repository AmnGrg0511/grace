//! The `/model`, `/provider` and `/effort` command domain: enumerate the
//! endpoints and models a transport can switch to, interactively pick one,
//! and apply + persist the switch. Pulled out of `chat.rs` so the REPL loop
//! stays that file's single focus.

use crate::config::settings::{PROVIDER_PRESETS};
use crate::ui::picker::Pick;
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
/// keyless endpoint simply yields the static list). `api_key` is the resolved
/// key *value* to authenticate with (empty for none) — not an env-var name —
/// so a key that was just prompted and saved to ~/.grace/.env in this session
/// (not yet in the process env) still authenticates.
fn fetched_models(base_url: &str, api_key: &str) -> Vec<crate::transport::ModelInfo> {
    if base_url.is_empty() {
        return Vec::new();
    }
    // The factory, not a raw HttpTransport: for the Copilot endpoint a raw
    // transport would send the long-lived OAuth token as the bearer.
    crate::transport::transport_for(base_url, api_key, "", None)
        .list_models()
        .unwrap_or_default()
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
                // Ambiguous: the same shared picker as /model with no
                // argument, narrowed to the matches — never a numbered list.
                let rows = model_candidate_rows(
                    &candidates,
                    &m,
                    transport.current_model().as_deref(),
                    transport.current_base_url().as_deref(),
                );
                match crate::ui::picker::pick(&rows, skin, "pick a model") {
                    Some(id) => match id.parse::<usize>() {
                        Ok(idx) => candidates[idx].clone(),
                        Err(_) => return,
                    },
                    None => return,
                }
            }
        }
    };

    let _ = apply_model_switch(transport, target, reader, cached_context_window);
}

/// Execute a resolved switch: key handling (env/OAuth/prompt), endpoint
/// re-point, model swap, context-window re-resolution + change notice, and
/// persistence (config.toml endpoint upsert + .env key upsert).
///
/// Returns the resolved API key (non-Copilot endpoints only) when the endpoint
/// was re-pointed, so a caller can validate against the live model list with
/// the *same* key that was just saved — including a key just prompted this
/// session, which is not yet in the process env.
#[allow(clippy::too_many_arguments)]
fn apply_model_switch(
    transport: &(dyn crate::transport::ProviderTransport + '_),
    target: ModelCandidate,
    reader: &mut LineReader,
    cached_context_window: &mut Option<u32>,
) -> Option<String> {
    let is_copilot_target = crate::transport::copilot::is_copilot_endpoint(&target.base_url);
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
        return None;
    }

    let mut resolved_key: Option<String> = None;
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
            return None;
        };
        transport.set_endpoint(&target.base_url, &key);
        if !is_copilot_target {
            // Per-key upsert: a whole-file rewrite would wipe the other
            // providers' keys every time `/model` switched providers.
            if let Err(e) = crate::config::env::upsert_env_file(&target.key_env, &key) {
                eprintln!(
                    "[grace] warning: could not save {}: {e}",
                    crate::config::env::env_file_path().display()
                );
            }
            // Hand the just-resolved key back so a caller (e.g. /provider's
            // validation) authenticates with the same key that was saved.
            resolved_key = Some(key);
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
        return resolved_key;
    }
    println!("model switched to \"{}\" (saved to config).", target.model);
    resolved_key
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
    // The key value (resolved from the process env, which startup loaded from
    // ~/.grace/.env), not the env-var name — fetched_models authenticates with
    // the value directly.
    let key = std::env::var(&picked.key_env).unwrap_or_default();
    let fetched = fetched_models(&picked.base_url, &key);
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

/// Picker rows for the ambiguous `/model <query>` case: the model id as the
/// label, its endpoint as the dim sublabel (so typing the host filters too);
/// the live model — same model on the same host — is shown but not
/// selectable. Pure, so the row assembly is testable without a terminal.
fn model_candidate_rows(
    candidates: &[ModelCandidate],
    idxs: &[usize],
    current_model: Option<&str>,
    current_base_url: Option<&str>,
) -> Vec<Pick> {
    idxs
        .iter()
        .map(|&i| {
            let c = &candidates[i];
            let is_current = current_model == Some(c.model.as_str())
                && current_base_url
                    .is_some_and(|b| short_url(b) == short_url(&c.base_url));
            let hint = if is_current {
                format!("{} · {} · current", c.name, short_url(&c.base_url))
            } else {
                format!("{} · {}", c.name, short_url(&c.base_url))
            };
            let mut row = Pick::new(i.to_string(), c.model.clone()).hint(hint);
            if is_current {
                row = row.disabled();
            }
            row
        })
        .collect()
}

/// The canonical display name for a base URL: the preset's label when the host
/// matches a known preset (OpenRouter, OpenAI, GitHub Copilot), else the given
/// fallback (the persisted/friendly name).
fn display_name_for(base_url: &str, fallback: &str) -> String {
    crate::config::settings::PROVIDER_PRESETS
        .iter()
        .find(|p| !p.base_url.is_empty() && short_url(p.base_url) == short_url(base_url))
        .map(|p| p.label.to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// One picker row per distinct provider endpoint — persisted endpoints plus
/// presets — the live endpoint shown but not selectable (marked "current"),
/// and an escape hatch for a custom OpenAI-compatible URL. Pure, so the row
/// assembly is testable without a terminal.
///
/// Rows are keyed by the endpoint's host, not its name: a persisted "openrouter"
/// and the "OpenRouter" preset share one host and collapse into a single row,
/// labeled with the preset's canonical name.
fn provider_rows(
    candidates: &[ModelCandidate],
    current_base_url: Option<&str>,
    _skin: &Skin,
) -> Vec<Pick> {
    // One row per distinct host.
    let mut items: Vec<Pick> = Vec::with_capacity(candidates.len() + 1);
    for c in candidates {
        let host = short_url(&c.base_url);
        if items.iter().any(|r: &Pick| {
            r.id.split('\u{1}').nth(1).is_some_and(|u| short_url(u) == host)
        }) {
            continue;
        }
        let name = display_name_for(&c.base_url, &c.name);
        let is_current =
            current_base_url.is_some_and(|cur| short_url(cur) == host);
        let sub = if is_current {
            format!("{host} · current")
        } else {
            host.clone()
        };
        let id = format!("{name}\u{1}{}", c.base_url);
        let mut row = Pick::new(id, name).hint(sub);
        if is_current {
            row = row.disabled();
        }
        items.push(row);
    }
    items.push(
        Pick::new("\u{1}custom", "Custom OpenAI-compatible endpoint").hint("type a base URL"),
    );
    items
}

/// The provider stage of `/provider`: pick an endpoint from
/// [`provider_rows`] (or type a custom OpenAI-compatible URL), then resolve
/// the endpoint's key env var and known-model list from the candidates.
fn pick_provider_endpoint(
    candidates: &[ModelCandidate],
    reader: &mut LineReader,
    skin: &Skin,
    current_base_url: Option<&str>,
) -> Option<EndpointPick> {
    use crate::ui::picker::pick;

    let items = provider_rows(candidates, current_base_url, skin);
    let choice = pick(&items, skin, "pick a provider")?;

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
/// when no candidate carries an explicit key var. Rows are matched by host
/// only — the displayed `name` is a label, not an identity.
fn endpoint_pick(
    candidates: &[ModelCandidate],
    name: &str,
    base_url: &str,
) -> EndpointPick {
    let (key_env, known): (String, Vec<(String, Option<u32>)>) = candidates
        .iter()
        .filter(|c| short_url(&c.base_url) == short_url(base_url))
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
            let is_current =
                current.as_deref() == Some(id) || (*id == "none" && current.is_none());
            let hint = if is_current {
                format!("{blurb} · current")
            } else {
                blurb.to_string()
            };
            let mut row = Pick::new(*id, *id).hint(hint);
            if is_current {
                row = row.disabled();
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
    let Some(picked) = pick_provider_endpoint(
        &candidates,
        reader,
        skin,
        transport.current_base_url().as_deref(),
    )
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
    // Returns the key apply_model_switch resolved (possibly just prompted and
    // saved to .env this session) — the same key to validate against below.
    let resolved_key = apply_model_switch(transport, target, reader, cached_context_window);

    // Best-effort validation against the endpoint's own list (no prompting
    // here): an unknown model id would otherwise surface as a confusing
    // provider 404 mid-conversation. Prefers the just-resolved key, falling
    // back to the process env, so a freshly-saved key still authenticates.
    let key = resolved_key
        .clone()
        .or_else(|| std::env::var(&picked.key_env).ok())
        .filter(|k| !k.is_empty());
    let fetched = fetched_models(&picked.base_url, key.as_deref().unwrap_or(""));
    let ids: Vec<String> = fetched.iter().map(|m| m.id.clone()).collect();
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
        if let Some((m, ctx)) = pick_model_for_endpoint(fetched, &known, reader, skin, None) {
            // Re-route through apply_model_switch so the re-picked model is
            // persisted to config (same endpoint, so no re-keying) instead of
            // only landing in the in-memory transport and reverting on restart.
            let retarget = ModelCandidate {
                name: picked.name.clone(),
                model: m,
                base_url: picked.base_url.clone(),
                key_env: picked.key_env.clone(),
                context_window: ctx,
            };
            let _ = apply_model_switch(transport, retarget, reader, cached_context_window);
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

    // Context window as the padded hint; the model this session already runs
    // stays visible but unselectable.
    let items: Vec<Pick> = models
        .iter()
        .enumerate()
        .map(|(i, (id, ctx))| {
            let mut row = Pick::new(format!("m{i}"), id.clone());
            if let Some(w) = ctx {
                let hint = if current_model == Some(id.as_str()) {
                    format!("{w}k · current")
                } else {
                    format!("{w}k")
                };
                row = row.hint(hint);
            }
            if current_model == Some(id.as_str()) {
                row = row.disabled();
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
    fn provider_rows_mark_the_live_endpoint_disabled_and_current() {
        let candidates = vec![
            cand("siemens", "qwen-3.8-27b", "https://api.siemens.com/llm/v1", Some(262_144)),
            cand("OpenAI", "gpt-4o", "https://api.openai.com/v1", Some(128_000)),
        ];
        let skin = crate::ui::skin::by_name(None);
        let rows = provider_rows(&candidates, Some("https://api.siemens.com/llm/v1"), &skin);
        // One row per endpoint plus the custom escape hatch; the live
        // endpoint is shown but dead, the other stays selectable.
        assert_eq!(rows.len(), 3);
        assert!(rows[0].disabled, "the live endpoint must be disabled");
        assert!(!rows[1].disabled);
        assert_eq!(rows[2].label, "Custom OpenAI-compatible endpoint");
        // The disabled row keeps its endpoint and says why it is dead; the
        // other row shows just its endpoint host.
        assert_eq!(rows[0].sublabel.as_deref(), Some("api.siemens.com · current"));
        assert_eq!(rows[1].sublabel.as_deref(), Some("api.openai.com"));
        // No current endpoint: nothing is disabled.
        let rows = provider_rows(&candidates, None, &skin);
        assert!(!rows.iter().any(|r| r.disabled));
    }

    #[test]
    fn provider_rows_dedup_by_host_and_prefer_the_preset_name() {
        // The user's persisted "openrouter" (friendly name from a custom-URL
        // entry) and the "OpenRouter" preset share a host — they must collapse
        // into ONE row, labeled with the canonical preset name. This is the
        // regression for "openrouter shows up twice in /provider".
        let candidates = vec![
            cand("openrouter", "z-ai/glm-5.2:free", "https://openrouter.ai/api/v1", Some(200_000)),
            cand("OpenRouter", "openai/gpt-4o-mini", "https://openrouter.ai/api/v1", None),
        ];
        let skin = crate::ui::skin::by_name(None);
        let rows = provider_rows(&candidates, None, &skin);
        assert_eq!(rows.len(), 2, "one row per host + the custom row");
        assert_eq!(rows[0].label, "OpenRouter", "known preset host uses its canonical name");
        assert_eq!(rows[0].sublabel.as_deref(), Some("openrouter.ai"));
    }

    #[test]
    fn provider_rows_dedup_same_host_different_models() {
        // Two models on one host are one provider row, not two.
        let candidates = vec![
            cand("OpenAI", "gpt-4o", "https://api.openai.com/v1", Some(128_000)),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", None),
        ];
        let skin = crate::ui::skin::by_name(None);
        let rows = provider_rows(&candidates, None, &skin);
        assert_eq!(rows.len(), 2, "one row per endpoint + the custom row");
        assert_eq!(rows[0].label, "OpenAI");
    }

    #[test]
    fn model_candidate_rows_label_the_model_and_hint_the_endpoint() {
        let candidates = vec![
            cand("OpenRouter", "openai/gpt-4o-mini", "https://openrouter.ai/api/v1", None),
            cand("OpenAI", "gpt-4o-mini", "https://api.openai.com/v1", None),
        ];
        let rows = model_candidate_rows(
            &candidates,
            &[0, 1],
            Some("gpt-4o-mini"),
            Some("https://api.openai.com/v1"),
        );
        assert_eq!(rows[0].id, "0");
        assert_eq!(rows[0].label, "openai/gpt-4o-mini");
        assert_eq!(rows[0].sublabel.as_deref(), Some("OpenRouter · openrouter.ai"));
        // The live model (same model AND same host) is disabled but keeps its
        // endpoint hint, marked "current"; the same-named model on another
        // host is not.
        assert!(!rows[0].disabled);
        assert!(rows[1].disabled);
        assert_eq!(rows[1].sublabel.as_deref(), Some("OpenAI · api.openai.com · current"));
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
