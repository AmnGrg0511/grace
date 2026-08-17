//! GitHub Copilot OAuth device flow — mints a bearer token the same way a
//! normal provider's API key would be typed in, except this one is fetched
//! via browser authorization instead of pasted. Once minted, Copilot is
//! wired up as a plain [`crate::config::TransportConfig::Http`] pointed at
//! `https://api.githubcopilot.com` — there is no separate Copilot transport
//! or CLI flag; the *only* difference from any other provider is how this
//! one function obtains the key.

use crate::message::Message;
use crate::transport::r#trait::{
    FinishReason, ModelInfo, ModelResponse, ProviderTransport, ToolSpec,
};
use crate::util::{truncate_utf8, AgentError, Result};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::time::Duration;

/// GitHub Copilot's OpenAI-compatible base URL — a plain HTTP preset like
/// any other provider's.
pub const BASE_URL: &str = "https://api.githubcopilot.com";

/// The long-lived OAuth device-flow token is NOT the Copilot API bearer —
/// GitHub requires exchanging it for a short-lived (~25min) session token
/// via this endpoint on every refresh. Feeding the OAuth token straight to
/// `api.githubcopilot.com` is exactly what silently expired and produced
/// 404s after a few minutes; this exchange (and the auto-refresh wrapper
/// below) is the actual fix.
const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// GitHub Copilot OAuth device flow response.
#[derive(Deserialize)]
#[allow(dead_code)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// GitHub OAuth token response.
#[derive(Deserialize)]
#[allow(dead_code)]
struct TokenResponse {
    access_token: Option<String>,
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: String,
    error: Option<String>,
    error_description: Option<String>,
}

/// Get a Copilot bearer token, running the OAuth device flow (browser +
/// one-time code) if none is cached yet. This is the "key" step for
/// Copilot — same conceptual step every other provider has, just backed by
/// OAuth instead of manual paste. Returns the token; the caller persists it
/// to `~/.grace/.env` exactly like a typed API key.
pub fn get_or_create_token() -> Result<String> {
    if let Ok(token) = std::env::var("GITHUB_COPILOT_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let device_code = start_device_flow()?;

    println!("\n🔐 GitHub Copilot Authentication Required");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("Please authorize GitHub Copilot in your browser:");
    println!();
    println!("  1. Open: {}", device_code.verification_uri);
    println!("  2. Enter code: {}", device_code.user_code);
    println!();
    println!("Waiting for authorization... (press Ctrl+C to cancel)");

    let token = poll_token(&device_code.device_code)?;
    println!("\n✅ Authentication successful!\n");
    Ok(token)
}

fn start_device_flow() -> Result<DeviceCodeResponse> {
    let resp = reqwest::blocking::Client::new()
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", "Iv1.b507a08c87ecfe98"), ("scope", "copilot")])
        .send()?
        .json::<DeviceCodeResponse>()?;
    Ok(resp)
}

fn poll_token(device_code: &str) -> Result<String> {
    let max_attempts = 120; // 10 minutes max
    for attempt in 0..max_attempts {
        std::thread::sleep(Duration::from_secs(5));
        let resp = reqwest::blocking::Client::new()
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", "Iv1.b507a08c87ecfe98"),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()?;

        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| AgentError::Transport(format!("Failed to read response: {e}")))?;

        if text.contains("authorization_pending") || text.contains("slow_down") {
            continue;
        }
        if text.contains("expired_token") {
            return Err(AgentError::Config("Device code expired".into()));
        }
        if text.contains("access_denied") {
            return Err(AgentError::Config("User denied authorization".into()));
        }
        if !status.is_success() {
            if attempt % 6 == 0 {
                eprintln!("Polling... (attempt {}/{})", attempt + 1, max_attempts);
            }
            continue;
        }

        let token_resp: TokenResponse = serde_json::from_str(&text).map_err(|e| {
            AgentError::Transport(format!(
                "Failed to parse token response: {e}. Raw: {}",
                truncate_utf8(&text, 200)
            ))
        })?;

        if let Some(token) = token_resp.access_token {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        match token_resp.error.as_deref() {
            Some("authorization_pending") | Some("slow_down") => continue,
            Some("expired_token") => return Err(AgentError::Config("Device code expired".into())),
            Some("access_denied") => {
                return Err(AgentError::Config("User denied authorization".into()))
            }
            _ => {}
        }
    }
    Err(AgentError::Config("Device flow timed out".into()))
}

/// Fetch Copilot's live model list via its `/models` endpoint. Its response
/// shape nests context window/max-output under `capabilities`, unlike the
/// flat shape most OpenAI-compatible providers use, so this can't share
/// `HttpTransport::list_models` — kept here since it's Copilot-specific.
pub fn fetch_models(token: &str) -> Result<Vec<ModelInfo>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| AgentError::Transport(format!("HTTP client error: {e}")))?;
    let resp = client
        .get(format!("{BASE_URL}/models"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| AgentError::Transport(format!("Copilot models request failed: {e}")))?
        .json::<Value>()
        .map_err(|e| AgentError::Transport(format!("Failed to parse Copilot models response: {e}")))?;

    let models = resp["data"]
        .as_array()
        .ok_or_else(|| AgentError::Transport("Invalid models response format".into()))?;

    let mut result = Vec::new();
    for model in models {
        let (Some(id), Some(name)) = (model["id"].as_str(), model["name"].as_str()) else {
            continue;
        };
        let context_window = model["capabilities"]["context_window"]
            .as_u64()
            .map(|v| v as u32);
        let max_output_tokens = model["capabilities"]["max_output_tokens"]
            .as_u64()
            .map(|v| v as u32);
        result.push(ModelInfo {
            id: id.to_string(),
            name: name.to_string(),
            context_window,
            max_output_tokens,
            provider: "github-copilot".to_string(),
        });
    }
    if result.is_empty() {
        return Err(AgentError::Transport("No models found in Copilot response".into()));
    }
    Ok(result)
}

/// A minted session token plus when it expires (Unix seconds), so the
/// transport can tell "still good" from "must refresh" without another
/// round-trip.
struct SessionToken {
    value: String,
    expires_at: u64,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Exchange the long-lived OAuth token for a short-lived Copilot API
/// session token. This step was missing entirely before — Grace was
/// sending the OAuth token itself as the bearer, which `api.githubcopilot.com`
/// accepts only briefly before every subsequent call 404s. Response shape:
/// `{"token": "...", "expires_at": <unix_secs>, ...}`.
fn exchange_for_session_token(oauth_token: &str) -> Result<SessionToken> {
    let resp = reqwest::blocking::Client::new()
        .get(TOKEN_EXCHANGE_URL)
        .header("Authorization", format!("token {oauth_token}"))
        .header("Accept", "application/json")
        .send()
        .map_err(|e| AgentError::Transport(format!("Copilot token exchange failed: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| AgentError::Transport(format!("failed to read token exchange body: {e}")))?;
    if !status.is_success() {
        return Err(AgentError::Config(format!(
            "Copilot session-token exchange returned {status} — the OAuth token is likely \
             stale or revoked; delete GITHUB_COPILOT_TOKEN from ~/.grace/.env and re-run to \
             re-authenticate. Raw: {}",
            truncate_utf8(&text, 300)
        )));
    }
    let data: Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Transport(format!("invalid token exchange JSON: {e}")))?;
    let value = data
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| AgentError::Transport("token exchange response missing 'token'".into()))?
        .to_string();
    // Default to a conservative 20-minute lifetime if the field is absent —
    // GitHub's real tokens run ~25min, better to refresh a bit early than late.
    let expires_at = data
        .get("expires_at")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| now_unix() + 1200);
    Ok(SessionToken { value, expires_at })
}

/// Wraps [`crate::transport::http::HttpTransport`]-equivalent behavior for
/// Copilot specifically, because Copilot needs one extra thing no other
/// provider does: the bearer it sends must be refreshed via
/// [`exchange_for_session_token`] roughly every 25 minutes, transparently,
/// with no user-visible re-auth prompt. The long-lived OAuth token (from
/// `get_or_create_token`, persisted in `~/.grace/.env`) is kept only to mint
/// fresh session tokens — it is never sent to the chat endpoint itself.
pub struct CopilotTransport {
    client: reqwest::blocking::Client,
    oauth_token: String,
    session: RefCell<Option<SessionToken>>,
    model: RefCell<String>,
}

impl CopilotTransport {
    pub fn new(oauth_token: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            client,
            oauth_token: oauth_token.into(),
            session: RefCell::new(None),
            model: RefCell::new(model.into()),
        }
    }

    /// Override the per-request timeout (default 60 s), mirroring
    /// `HttpTransport::with_request_timeout`.
    pub fn with_request_timeout(mut self, secs: u64) -> Self {
        self.client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(crate::transport::http::request_timeout(Some(secs)))
            .build()
            .unwrap_or_default();
        self
    }

    /// Returns a session token guaranteed to be valid for at least 60 more
    /// seconds, refreshing via [`exchange_for_session_token`] if the cached
    /// one is missing or about to expire. This is what makes token refresh
    /// invisible to the user — no re-prompt, no manual re-auth, just a
    /// silent exchange call before whichever request needed it.
    fn valid_session_token(&self) -> Result<String> {
        {
            let cached = self.session.borrow();
            if let Some(tok) = cached.as_ref() {
                if tok.expires_at > now_unix() + 60 {
                    return Ok(tok.value.clone());
                }
            }
        }
        let fresh = exchange_for_session_token(&self.oauth_token)?;
        let value = fresh.value.clone();
        *self.session.borrow_mut() = Some(fresh);
        Ok(value)
    }
}

impl ProviderTransport for CopilotTransport {
    fn name(&self) -> &str {
        "github-copilot"
    }

    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        _model: &str,
    ) -> Result<ModelResponse> {
        let model_owned = self.model.borrow().clone();
        let model = if model_owned.is_empty() {
            "gpt-4o"
        } else {
            model_owned.as_str()
        };
        let msg_json: Vec<Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        let mut body = serde_json::json!({
            "model": model,
            "messages": msg_json,
            "temperature": 0.0,
        });
        if !tools.is_empty() {
            body["tools"] = crate::transport::tools_to_json(tools);
            body["tool_choice"] = Value::String("auto".to_string());
        }

        // One retry after a fresh token exchange if the first attempt gets
        // 401/404 — covers the case where the cached token expired early
        // (clock skew, GitHub shortening the window under load) without
        // making every call pay for two round-trips in the common case.
        let mut token = self.valid_session_token()?;
        let mut resp = self
            .client
            .post(format!("{BASE_URL}/chat/completions"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .map_err(|e| AgentError::Transport(format!("Copilot request failed: {e}")))?;
        if resp.status().as_u16() == 401 || resp.status().as_u16() == 404 {
            *self.session.borrow_mut() = None;
            token = self.valid_session_token()?;
            resp = self
                .client
                .post(format!("{BASE_URL}/chat/completions"))
                .bearer_auth(&token)
                .json(&body)
                .send()
                .map_err(|e| AgentError::Transport(format!("Copilot request failed: {e}")))?;
        }
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| AgentError::Transport(format!("failed to read response body: {e}")))?;
        if !status.is_success() {
            return Err(AgentError::Transport(format!(
                "Copilot returned {status}: {}",
                truncate_utf8(&text, 500)
            )));
        }
        let parsed: Value = serde_json::from_str(&text).map_err(|e| {
            AgentError::Transport(format!(
                "invalid JSON response (status {status}): {e}. Raw: {}",
                truncate_utf8(&text, 500)
            ))
        })?;
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
        let mut result = crate::transport::parse_openai_message(&msg, finish_reason_str)?;
        if !result.tool_calls.is_empty() {
            result.finish_reason = FinishReason::ToolCalls;
        }
        Ok(result)
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

    fn set_endpoint(&self, _base_url: &str, _api_key: &str) {
        // Copilot's endpoint is fixed; `/model <provider>` switching to a
        // different provider replaces the whole transport upstream instead.
    }

    fn current_base_url(&self) -> Option<String> {
        Some(BASE_URL.to_string())
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let token = self.valid_session_token()?;
        fetch_models(&token)
    }

    /// Copilot's `/models` payload carries a real per-model context window,
    /// so prefer it and only fall back to the static table. Best-effort: a
    /// failed lookup returns `None` and the compressor uses its own default
    /// rather than propagating a network error into the agent loop.
    fn context_window(&self) -> Option<u32> {
        let model = self.model.borrow().clone();
        if let Ok(models) = self.list_models() {
            if let Some(found) = models
                .iter()
                .find(|m| m.id == model)
                .and_then(|m| m.context_window)
            {
                return Some(found);
            }
        }
        crate::config::settings::context_window_for(&model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_reports_its_fixed_endpoint() {
        let t = CopilotTransport::new("tok", "gpt-4o");
        assert_eq!(t.current_base_url().as_deref(), Some(BASE_URL));
    }

    #[test]
    fn set_endpoint_is_a_no_op_for_a_fixed_provider() {
        // Copilot cannot be re-pointed; `/model` swaps the whole transport
        // upstream instead. Assert it silently ignores rather than corrupting
        // its own base_url.
        let t = CopilotTransport::new("tok", "gpt-4o");
        t.set_endpoint("https://elsewhere.test/v1", "other");
        assert_eq!(t.current_base_url().as_deref(), Some(BASE_URL));
    }

    #[test]
    fn model_is_swappable_and_reported() {
        let t = CopilotTransport::new("tok", "gpt-4o");
        assert_eq!(t.current_model().as_deref(), Some("gpt-4o"));
        t.set_model("claude-sonnet-4");
        assert_eq!(t.current_model().as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn empty_model_reports_none() {
        let t = CopilotTransport::new("tok", "");
        assert_eq!(t.current_model(), None);
    }

    #[test]
    fn transport_name_identifies_the_provider() {
        let t = CopilotTransport::new("tok", "gpt-4o");
        assert_eq!(t.name(), "github-copilot");
    }
}
