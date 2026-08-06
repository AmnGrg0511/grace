//! GitHub Copilot OAuth device flow — mints a bearer token the same way a
//! normal provider's API key would be typed in, except this one is fetched
//! via browser authorization instead of pasted. Once minted, Copilot is
//! wired up as a plain [`crate::config::TransportConfig::Http`] pointed at
//! `https://api.githubcopilot.com` — there is no separate Copilot transport
//! or CLI flag; the *only* difference from any other provider is how this
//! one function obtains the key.

use crate::error::{AgentError, Result};
use crate::transport::ModelInfo;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// GitHub Copilot's OpenAI-compatible base URL — a plain HTTP preset like
/// any other provider's.
pub const BASE_URL: &str = "https://api.githubcopilot.com";

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
                truncate(&text, 200)
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

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}... [truncated {} bytes]", &s[..max], s.len() - max)
    }
}
