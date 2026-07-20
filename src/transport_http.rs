//! OpenAI-compatible HTTP transport.
//!
//! Talks to any endpoint that implements the `/chat/completions` contract:
//! OpenAI, most OpenAI-compatible proxies, Ollama in `/v1` mode, llama.cpp,
//! OpenRouter, etc. Uses `reqwest` (rustls) for real TLS — no more hand-rolled
//! TCP/HTTP/1.1 framing or chunked-transfer decoding.

use crate::error::{AgentError, Result};
use crate::message::Message;
use crate::transport::{parse_openai_message, tools_to_json, FinishReason, ProviderTransport, ToolSpec};
use serde_json::{json, Value};

/// A transport that POSTs to an OpenAI-compatible `/chat/completions`.
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    /// Model id owned by the transport (the loop passes `""`; see `complete`).
    model: String,
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
    pub fn with_model(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
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
                    if status.is_server_error() || status.as_u16() == 429 {
                        last_err = Some(AgentError::Transport(format!("retryable status {status}")));
                    } else {
                        return resp
                            .json()
                            .map_err(|e| AgentError::Transport(format!("invalid JSON response: {e}")));
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

    fn complete(&self, messages: &[Message], tools: &[ToolSpec], _model: &str) -> Result<crate::transport::ModelResponse> {
        // The model is owned by this transport (the agent loop passes "").
        let model = if self.model.is_empty() { "grace-1" } else { self.model.as_str() };

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
}
