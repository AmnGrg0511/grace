//! OpenAI-compatible HTTP transport.
//!
//! Talks to any endpoint that implements the `/chat/completions` contract:
//! OpenAI, most OpenAI-compatible proxies, Ollama in `/v1` mode, llama.cpp,
//! OpenRouter, etc. Uses `reqwest` (rustls) for real TLS — no more hand-rolled
//! TCP/HTTP/1.1 framing or chunked-transfer decoding.

use crate::error::{AgentError, Result};
use crate::message::Message;
use crate::transport::{
    parse_openai_message, tools_to_json, FinishReason, ModelInfo, ProviderTransport, ToolSpec,
};
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
            crate::transport::urlencoding(model)
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
    base_url: String,
    api_key: String,
    /// Model id owned by the transport (the loop passes `""`; see `complete`).
    /// `RefCell` so `/model` can hot-swap it mid-chat via `&self` — Grace is
    /// single-threaded, so no `Sync` requirement, `RefCell` is enough.
    model: std::cell::RefCell<String>,
    /// Optional path override; defaults to `/chat/completions`.
    chat_path: String,
}

impl HttpTransport {
    /// Generic OpenAI-compatible endpoint. `model` defaults to empty and must
    /// be supplied by the caller via [`HttpTransport::with_model`] for real
    /// use; the agent loop passes `""`, so the transport must own the model.
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
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: std::cell::RefCell::new(model.into()),
            chat_path: String::from("/chat/completions"),
        }
    }

    /// Preset: OpenRouter's OpenAI-compatible endpoint (HTTPS, real TLS via
    /// reqwest — no proxy needed anymore).
    pub fn openrouter(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_model("https://openrouter.ai/api/v1", api_key, model)
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
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
            if !self.api_key.is_empty() {
                req = req.bearer_auth(&self.api_key);
            }
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
                                    Self::truncate(&text, 500)
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

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            s.to_string()
        } else {
            format!("{}... [truncated {} bytes]", &s[..max], s.len() - max)
        }
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

        // If the model emitted tool_calls, force the finish reason regardless of
        // what the provider reported (some send "stop" with tool calls).
        if !resp.tool_calls.is_empty() {
            resp.finish_reason = FinishReason::ToolCalls;
        }
        Ok(resp)
    }

    fn set_model(&self, model: &str) {
        *self.model.borrow_mut() = model.to_string();
    }

    fn current_model(&self) -> Option<String> {
        let m = self.model.borrow();
        if m.is_empty() {
            None
        } else {
            Some(m.clone())
        }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // Try to fetch models from the provider's /models endpoint
        let client = reqwest::blocking::Client::new();
        let models_url = format!("{}/models", self.base_url.trim_end_matches('/'));
        
        let mut req = client.get(&models_url);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        
        match req.send() {
            Ok(resp) => {
                if resp.status().is_success() {
                    let json: Value = resp.json().map_err(|e| AgentError::Transport(format!("Failed to parse models response: {e}")))?;
                    let empty_vec = Vec::new();
                    let models = json.get("data").and_then(Value::as_array).unwrap_or(&empty_vec);
                    let result: Vec<ModelInfo> = models.iter().filter_map(|m| {
                        let id = m.get("id")?.as_str()?;
                        let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
                        let context_window = m.get("context_window").and_then(Value::as_u64).map(|v| v as u32);
                        let max_output_tokens = m.get("max_output_tokens").and_then(Value::as_u64).map(|v| v as u32);
                        Some(ModelInfo {
                            id: id.to_string(),
                            name: name.to_string(),
                            context_window,
                            max_output_tokens,
                            provider: "openai-http".to_string(),
                        })
                    }).collect();
                    if !result.is_empty() {
                        return Ok(result);
                    }
                }
            }
            Err(e) => {
                // If /models endpoint fails, return empty list
                eprintln!("[grace] warning: failed to fetch models from {}: {}", models_url, e);
            }
        }
        Ok(Vec::new())
    }
}