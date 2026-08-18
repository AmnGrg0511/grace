//! OpenAI-compatible HTTP transport.
//!
//! Talks to any endpoint that implements the `/chat/completions` contract:
//! OpenAI, most OpenAI-compatible proxies, Ollama in `/v1` mode, llama.cpp,
//! OpenRouter, etc. Uses `reqwest` (rustls) for real TLS — no more hand-rolled
//! TCP/HTTP/1.1 framing or chunked-transfer decoding.

use crate::message::Message;
use crate::transport::r#trait::{
    FinishReason, ModelInfo, ModelResponse, ProviderTransport, ToolSpec,
};
use crate::transport::wire::{parse_openai_message, parse_usage, tools_to_json};
use crate::util::{truncate_utf8, AgentError, Result};
use serde_json::{json, Value};

/// Best-effort API fetch to discover a model's context window. Covers
/// OpenRouter (GET /api/v1/models) and OpenAI (GET /v1/models/{id});
/// everything else returns `None` silently. Lives in the transport layer
/// (not `main.rs`) since it's an HTTP call against the same providers
/// `HttpTransport` talks to — CLI code shouldn't own network calls.
pub fn fetch_context_window(model: &str, base_url: &str, api_key: &str) -> Option<u32> {
    // OpenRouter: list endpoint returns context_length per model.
    if base_url.contains("openrouter") {
        let url = format!(
            "{}/api/v1/models",
            base_url.trim_end_matches("/v1").trim_end_matches('/')
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(&url).bearer_auth(api_key).send().ok()?;
        let data: serde_json::Value = resp.json().ok()?;
        let arr = data.get("data").and_then(|d| d.as_array())?;
        for entry in arr {
            let id = entry.get("id").and_then(|v| v.as_str())?;
            if id == model {
                return entry
                    .get("context_length")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
            }
            // Also match on model family prefix (e.g. "anthropic/claude-sonnet-4-*")
            if model.starts_with(id) || id.starts_with(model) {
                return entry
                    .get("context_length")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
            }
        }
        return None;
    }
    // OpenAI: the models/{id} endpoint returns max_context_window for some.
    if base_url.contains("openai.com") {
        let url = format!(
            "{}/models/{}",
            base_url.trim_end_matches('/'),
            crate::transport::wire::urlencoding(model)
        );
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        let resp = client.get(&url).bearer_auth(api_key).send().ok()?;
        let data: serde_json::Value = resp.json().ok()?;
        if let Some(ctx) = data.pointer("/max_context_window") {
            return ctx.as_u64().map(|n| n as u32);
        }
    }
    None
}

/// A transport that POSTs to an OpenAI-compatible `/chat/completions`.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    /// `RefCell` so `/model` can re-point the transport at a different
    /// provider mid-chat (see `set_endpoint`) — same single-threaded
    /// rationale as `model` below.
    base_url: std::cell::RefCell<String>,
    api_key: std::cell::RefCell<String>,
    /// Model id owned by the transport (the loop passes "", see `complete`).
    /// `RefCell` so `/model` can hot-swap it mid-chat via `&self` — Grace is
    /// single-threaded, so no `Sync` requirement, `RefCell` is enough.
    model: std::cell::RefCell<String>,
    /// Optional path override; defaults to `/chat/completions`.
    chat_path: String,
    /// Memoized context window for the current model, so compression can ask
    /// "how big is my budget?" every iteration without paying for an HTTP
    /// round-trip each time. `None` = not yet resolved; `Some(None)` = asked
    /// and the provider genuinely does not report one.
    #[allow(clippy::option_option)]
    context_window: std::cell::RefCell<Option<Option<u32>>>,
}

/// The per-request timeout for provider calls: the configured
/// `request_timeout_secs` when set, else the 60 s default. `0` is clamped to
/// 1 s — a zero timeout would time out every request instantly.
pub fn request_timeout(request_timeout_secs: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_secs(request_timeout_secs.unwrap_or(60).max(1))
}

impl HttpTransport {
    /// Override the per-request timeout (default 60 s). The connect timeout
    /// stays 10 s: it bounds the DNS/TCP phase, while the request timeout
    /// bounds the whole exchange.
    pub fn with_request_timeout(mut self, secs: u64) -> Self {
        self.client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(request_timeout(Some(secs)))
            .build()
            .unwrap_or_default();
        self
    }
    /// Generic OpenAI-compatible endpoint. `model` defaults to empty and must
    /// be supplied by the caller via [`HttpTransport::with_model`] for real
    /// use; the agent loop passes "", so the transport must own the model.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_model(base_url, api_key, "")
    }

    /// Construct with an explicit model id the transport keeps.
    pub fn with_model(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url: std::cell::RefCell::new(base_url.into()),
            api_key: std::cell::RefCell::new(api_key.into()),
            model: std::cell::RefCell::new(model.into()),
            chat_path: String::from("/chat/completions"),
            context_window: std::cell::RefCell::new(None),
        }
    }

    /// Preset: OpenRouter's OpenAI-compatible endpoint (HTTPS, real TLS via
    /// reqwest — no proxy needed anymore).
    pub fn openrouter(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_model("https://openrouter.ai/api/v1", api_key, model)
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.borrow();
        let base = base.trim_end_matches('/');
        format!("{base}{}", self.chat_path)
    }

    /// POST `body`, retrying transport-level failures and 429/5xx responses
    /// up to 3 attempts total with exponential backoff (500ms, 1s). Manually
    /// verified against a flaky endpoint; not covered by an automated timing
    /// test (those are flaky by nature — the logic itself stays simple and
    /// readable instead).
    fn send_with_retry(&self, body: &Value) -> Result<Value> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut backoff = std::time::Duration::from_millis(500);
        let mut last_err = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let mut req = self.client.post(self.endpoint()).json(body);
            let api_key = self.api_key.borrow();
            if !api_key.is_empty() {
                req = req.bearer_auth(&*api_key);
            }
            drop(api_key);
            match req.send() {
                Ok(resp) => {
                    let status = resp.status();
                    // Read raw text first to handle non-JSON responses
                    let text = resp.text().map_err(|e| {
                        AgentError::Transport(format!("failed to read response body: {e}"))
                    })?;
                    if status.is_server_error() || status.as_u16() == 429 {
                        last_err = Some(AgentError::Transport(format!(
                            "retryable status {status}: {text}"
                        )));
                    } else {
                        // Try to parse as JSON, include raw text on failure for debugging
                        match serde_json::from_str(&text) {
                            Ok(json) => return Ok(json),
                            Err(e) => {
                                return Err(AgentError::Transport(format!(
                                    "invalid JSON response (status {status}): {e}. Raw: {}",
                                    truncate_utf8(&text, 500)
                                )));
                            }
                        }
                    }
                }
                Err(e) => {
                    last_err = Some(AgentError::Transport(format!("request failed: {e}")));
                }
            }
            if attempt < MAX_ATTEMPTS {
                std::thread::sleep(backoff);
                backoff *= 2;
            }
        }
        Err(last_err.unwrap_or_else(|| AgentError::Transport("request failed".into())))
    }

}

impl ProviderTransport for HttpTransport {
    fn name(&self) -> &str {
        "openai-http"
    }

    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        _model: &str,
    ) -> Result<crate::transport::ModelResponse> {
        // The model is owned by this transport (the agent loop passes "").
        let model_owned = self.model.borrow().clone();
        let model = if model_owned.is_empty() {
            "grace-1"
        } else {
            model_owned.as_str()
        };

        let msg_json: Vec<Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();

        let mut body = json!({
            "model": model,
            "messages": msg_json,
            "temperature": 0.0,
        });
        if !tools.is_empty() {
            body["tools"] = tools_to_json(tools);
            body["tool_choice"] = Value::String("auto".to_string());
        }

        let parsed = self.send_with_retry(&body)?;

        // Surface the upstream error object if the provider returned one
        // (e.g. OpenRouter free-tier rate limit / 403 quota). Without this the
        // caller only sees the generic "no choices" and the real cause is lost.
        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider returned an error")
                .to_string();
            return Err(AgentError::Response(format!("provider error: {msg}")));
        }

        let choice = parsed
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| AgentError::Response("no choices in response".into()))?;
        let msg = choice.get("message").cloned().unwrap_or(Value::Null);
        let finish_reason_str = choice.get("finish_reason").and_then(Value::as_str);

        let mut resp = parse_openai_message(&msg, finish_reason_str)?;

        // The provider's own token count (when it reports one) is the source
        // of truth for the context bar and compaction, ahead of estimation.
        resp.usage = parse_usage(parsed.get("usage"));

        // If the model emitted tool_calls, force the finish reason regardless of
        // what the provider reported (some send "stop" with tool calls).
        if !resp.tool_calls.is_empty() {
            resp.finish_reason = FinishReason::ToolCalls;
        }
        Ok(resp)
    }

    fn set_model(&self, model: &str) {
        *self.model.borrow_mut() = model.to_string();
        // The window belongs to the model, not the transport — a `/model`
        // swap mid-chat must not keep budgeting against the old model's
        // (possibly 10x larger) window.
        *self.context_window.borrow_mut() = None;
    }

    fn current_model(&self) -> Option<String> {
        let m = self.model.borrow();
        if m.is_empty() {
            None
        } else {
            Some(m.clone())
        }
    }

    fn set_endpoint(&self, base_url: &str, api_key: &str) {
        *self.base_url.borrow_mut() = base_url.to_string();
        *self.api_key.borrow_mut() = api_key.to_string();
        *self.context_window.borrow_mut() = None;
    }

    /// Resolve the current model's context window, preferring what the
    /// provider reports and falling back to the static table in
    /// [`crate::config::settings`]. Memoized: the answer only changes when the
    /// model or endpoint changes, and both of those invalidate the cache.
    ///
    /// This replaces the hardcoded 128k that used to sit inside the agent
    /// loop — budgeting a 8k model against a 128k assumption meant
    /// compression never fired until the request had already failed.
    fn context_window(&self) -> Option<u32> {
        if let Some(cached) = *self.context_window.borrow() {
            return cached;
        }
        let model = self.model.borrow().clone();
        let base_url = self.base_url.borrow().clone();
        let api_key = self.api_key.borrow().clone();
        let resolved = fetch_context_window(&model, &base_url, &api_key)
            // Provider-specific probes above only know OpenRouter and OpenAI
            // by hostname. Any other OpenAI-compatible server (Ollama, vLLM,
            // LM Studio, a local proxy) still exposes a generic `/models`
            // listing, and ignoring it meant every such endpoint reported an
            // unknown window and fell back to the conservative default.
            .or_else(|| {
                self.list_models()
                    .ok()?
                    .into_iter()
                    .find(|m| m.id == model)?
                    .context_window
            })
            .or_else(|| crate::config::settings::context_window_for(&model));
        *self.context_window.borrow_mut() = Some(resolved);
        resolved
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    /// Real incremental streaming over SSE. Chat mode uses this so tokens
    /// appear as the model produces them; the one-shot `--stream` path uses
    /// the same code, so streaming behaviour can no longer differ between the
    /// two modes (it previously only existed for one-shot).
    fn complete_streaming(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        _model: &str,
        on_fragment: &mut dyn FnMut(&str),
    ) -> Result<ModelResponse> {
        let model_owned = self.model.borrow().clone();
        let model = if model_owned.is_empty() {
            "grace-1".to_string()
        } else {
            model_owned
        };
        let base_url = self.base_url.borrow().clone();
        let api_key = self.api_key.borrow().clone();
        crate::transport::stream::stream_complete(
            &base_url,
            &api_key,
            &model,
            messages,
            tools,
            on_fragment,
        )
    }

    fn current_base_url(&self) -> Option<String> {
        Some(self.base_url.borrow().clone())
    }

    /// Enumerate the provider's models via the generic `/models` route.
    ///
    /// Best-effort by design and deliberately silent: this is now also called
    /// from `context_window()` on the startup path, so an endpoint without a
    /// `/models` route (plenty of local servers) would otherwise print a
    /// warning on every single invocation. Callers that need to tell the user
    /// (the onboarding wizard) already say so themselves, from a place where
    /// it is actually actionable.
    ///
    /// A short timeout keeps a slow or hanging endpoint from delaying startup.
    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        else {
            return Ok(Vec::new());
        };
        let base_url = self.base_url.borrow().clone();
        let models_url = format!("{}/models", base_url.trim_end_matches('/'));

        let mut req = client.get(&models_url);
        let api_key = self.api_key.borrow().clone();
        if !api_key.is_empty() {
            req = req.bearer_auth(&api_key);
        }

        let Ok(resp) = req.send() else {
            return Ok(Vec::new());
        };
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        // A malformed/non-JSON 200 body (e.g. an HTML error page from a
        // misconfigured proxy) falls through to the empty list rather than
        // hard-failing the call.
        let Ok(json) = resp.json::<Value>() else {
            return Ok(Vec::new());
        };
        Ok(parse_model_list(&json))
    }
}

/// Extract `ModelInfo`s from an OpenAI-style `{"data": [...]}` listing.
///
/// `context_window` is read from several spellings because providers do not
/// agree: OpenAI-compatible servers use `context_window`, OpenRouter uses
/// `context_length`, and some proxies use `max_context_length`. Reading only
/// one meant the window silently came back unknown and compression fell back
/// to its conservative default.
fn parse_model_list(json: &Value) -> Vec<ModelInfo> {
    let Some(models) = json.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
            let context_window = ["context_window", "context_length", "max_context_length"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_u64))
                .and_then(|v| u32::try_from(v).ok());
            let max_output_tokens = m
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            Some(ModelInfo {
                id: id.to_string(),
                name: name.to_string(),
                context_window,
                max_output_tokens,
                provider: "openai-http".to_string(),
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_owns_its_model_and_reports_it() {
        let t = HttpTransport::with_model("https://example.test/v1", "k", "gpt-4o-mini");
        assert_eq!(t.current_model().as_deref(), Some("gpt-4o-mini"));
        t.set_model("gpt-4o");
        assert_eq!(t.current_model().as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn empty_model_reports_none_rather_than_an_empty_string() {
        let t = HttpTransport::new("https://example.test/v1", "k");
        assert_eq!(t.current_model(), None);
    }

    #[test]
    fn request_timeout_defaults_to_sixty_and_clamps_zero() {
        // Regression (G10): the configured value (when present) is the
        // single source for the request timeout; absent, 60 s; `0` would
        // kill every request instantly, so it clamps to 1 s.
        use std::time::Duration;
        assert_eq!(request_timeout(None), Duration::from_secs(60));
        assert_eq!(request_timeout(Some(120)), Duration::from_secs(120));
        assert_eq!(request_timeout(Some(0)), Duration::from_secs(1));
    }

    #[test]
    fn endpoint_joins_base_url_and_path_without_double_slash() {
        let t = HttpTransport::with_model("https://example.test/v1/", "k", "m");
        assert_eq!(t.endpoint(), "https://example.test/v1/chat/completions");
    }

    #[test]
    fn set_endpoint_repoints_the_transport() {
        let t = HttpTransport::with_model("https://a.test/v1", "k1", "m");
        t.set_endpoint("https://b.test/v1", "k2");
        assert_eq!(t.current_base_url().as_deref(), Some("https://b.test/v1"));
        assert_eq!(t.endpoint(), "https://b.test/v1/chat/completions");
    }

    #[test]
    fn switching_model_invalidates_the_cached_context_window() {
        // Regression guard: budgeting a freshly-selected small model against
        // the previous model's large cached window is how compression silently
        // stops protecting the request.
        let t = HttpTransport::with_model("https://example.test/v1", "", "gpt-4o-mini");
        *t.context_window.borrow_mut() = Some(Some(128_000));
        t.set_model("something-else");
        assert!(
            t.context_window.borrow().is_none(),
            "cache must be cleared on model change"
        );
    }

    #[test]
    fn switching_endpoint_invalidates_the_cached_context_window() {
        let t = HttpTransport::with_model("https://a.test/v1", "", "m");
        *t.context_window.borrow_mut() = Some(Some(128_000));
        t.set_endpoint("https://b.test/v1", "");
        assert!(t.context_window.borrow().is_none());
    }

    #[test]
    fn http_transport_advertises_real_streaming() {
        let t = HttpTransport::new("https://example.test/v1", "");
        assert!(t.supports_streaming());
    }

    #[test]
    fn openrouter_preset_points_at_openrouter() {
        let t = HttpTransport::openrouter("k", "openai/gpt-4o-mini");
        assert_eq!(
            t.current_base_url().as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn context_window_falls_back_to_the_static_table_for_known_models() {
        // No network in tests: fetch_context_window fails fast against a
        // bogus host, so this exercises the settings-table fallback path.
        let t = HttpTransport::with_model("https://127.0.0.1:1/v1", "", "gpt-4o-mini");
        let expected = crate::config::settings::context_window_for("gpt-4o-mini");
        assert_eq!(t.context_window(), expected);
    }

    #[test]
    fn model_list_parsing_accepts_the_common_context_window_spellings() {
        // Providers disagree: OpenAI-compatible uses context_window,
        // OpenRouter uses context_length, some proxies use
        // max_context_length. Reading only one silently loses the window.
        let json = serde_json::json!({"data": [
            {"id": "a", "context_window": 8000},
            {"id": "b", "context_length": 16000},
            {"id": "c", "max_context_length": 32000},
        ]});
        let models = parse_model_list(&json);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].context_window, Some(8_000));
        assert_eq!(models[1].context_window, Some(16_000));
        assert_eq!(models[2].context_window, Some(32_000));
    }

    #[test]
    fn a_model_without_a_reported_window_parses_with_none() {
        let json = serde_json::json!({"data": [{"id": "bare"}]});
        let models = parse_model_list(&json);
        assert_eq!(models[0].id, "bare");
        assert_eq!(models[0].context_window, None);
    }

    #[test]
    fn a_model_name_falls_back_to_its_id() {
        let json = serde_json::json!({"data": [{"id": "gpt-4o"}]});
        assert_eq!(parse_model_list(&json)[0].name, "gpt-4o");
    }

    #[test]
    fn entries_without_an_id_are_skipped_rather_than_panicking() {
        let json = serde_json::json!({"data": [{"name": "no id"}, {"id": "ok"}]});
        let models = parse_model_list(&json);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "ok");
    }

    #[test]
    fn a_payload_without_a_data_array_yields_no_models() {
        assert!(parse_model_list(&serde_json::json!({"error": "nope"})).is_empty());
        assert!(parse_model_list(&serde_json::json!({"data": "not an array"})).is_empty());
    }

    #[test]
    fn listing_models_against_an_unreachable_endpoint_is_empty_not_an_error() {
        // This runs on the startup path via context_window(); a hard error
        // (or a printed warning) for every endpoint lacking /models would be
        // noise on every single invocation.
        let t = HttpTransport::with_model("http://127.0.0.1:1/v1", "", "m");
        assert!(t.list_models().unwrap().is_empty());
    }
}
