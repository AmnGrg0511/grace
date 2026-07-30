//! GitHub Copilot transport — device flow authentication + OpenAI-compatible API.

use crate::error::{AgentError, Result};
use crate::message::Message;
use crate::transport::{
    parse_openai_message, tools_to_json, FinishReason, ModelInfo, ModelResponse, ProviderTransport, ToolSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// GitHub Copilot OAuth device flow response.
#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// GitHub OAuth token response.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: String,
    error: Option<String>,
    error_description: Option<String>,
}

/// GitHub Copilot model from /models endpoint.
#[derive(Deserialize, Debug, Clone)]
struct CopilotModel {
    id: String,
    name: String,
}

/// GitHub Copilot transport using device flow authentication.
pub struct CopilotTransport {
    client: reqwest::blocking::Client,
    api_key: String,
    model: RefCell<String>,
    base_url: String,
}

impl CopilotTransport {
    /// Create a new Copilot transport with device flow authentication.
    pub fn new(_model: impl Into<String>) -> Result<Self> {
        // Try to get token from environment
        let api_key = Self::get_or_create_token()?;
        
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .map_err(|e| AgentError::Transport(format!("HTTP client error: {e}")))?,
            api_key,
            model: RefCell::new("gpt-4o".to_string()),
            base_url: "https://api.githubcopilot.com".to_string(),
        })
    }

    /// Get or create GitHub Copilot token using device flow
    fn get_or_create_token() -> Result<String> {
        // First check for GITHUB_COPILOT_TOKEN env var
        if let Ok(token) = std::env::var("GITHUB_COPILOT_TOKEN") {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        
        // For now, require GITHUB_COPILOT_TOKEN env var
        std::env::var("GITHUB_COPILOT_TOKEN")
            .map_err(|_| AgentError::Config(
                "GitHub Copilot token not found. Set GITHUB_COPILOT_TOKEN env var or run device flow auth.".into()
            ))
    }

    /// Start device flow authentication.
    fn start_device_flow() -> Result<DeviceCodeResponse> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let resp = reqwest::blocking::Client::new()
            .post("https://github.com/login/device/code")
            .header("Accept", "application/json")
            .form(&[("client_id", "Iv1.b507a08c87ecfe98"), ("scope", "copilot")])
            .send()?
            .json::<DeviceCodeResponse>()?;
        Ok(resp)
    }

    /// Poll for token after device flow started.
    fn poll_token(device_code: &str, _interval: u64) -> Result<String> {
        let client = reqwest::blocking::Client::new();
        let max_attempts = 120; // 10 minutes max
        for _ in 0..max_attempts {
            std::thread::sleep(Duration::from_secs(5));
            let resp = reqwest::blocking::Client::new()
                .post("https://github.com/login/oauth/access_token")
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", "Iv1.b507a08c87ecfe98"),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()?
                .json::<TokenResponse>()?;
            
            if let Some(token) = resp.access_token {
                if !token.is_empty() {
                    return Ok(token);
                }
            }
            
            if let Some(err) = &resp.error {
                if err == "authorization_pending" {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
                if err == "slow_down" {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
                if err == "expired_token" {
                    return Err(AgentError::Config("Device code expired".into()));
                }
                if err == "access_denied" {
                    return Err(AgentError::Config("User denied authorization".into()));
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        Err(AgentError::Config("Device flow timed out".into()))
    }
    
    /// Get or create access token (with caching)
    fn get_access_token(&self) -> Result<String> {
        // For now, require GITHUB_COPILOT_TOKEN env var
        // Full device flow would need interactive terminal
        std::env::var("GITHUB_COPILOT_TOKEN")
            .map_err(|_| AgentError::Config(
                "GITHUB_COPILOT_TOKEN not set. Please set GITHUB_COPILOT_TOKEN environment variable.".into()
            ))
    }

    /// Build the Copilot API endpoint URL
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
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
            &model_owned
        };

        let token = self.get_access_token()?;
        
        let args = json!({
            "model": model,
            "messages": messages,
            "temperature": 0.0,
            "tools": tools_to_json(tools),
            "tool_choice": if !tools.is_empty() { json!("auto") } else { json!(null) },
            "stream": false,
        });

        let url = self.endpoint("/chat/completions");
        let token = std::env::var("GITHUB_COPILOT_TOKEN")
            .map_err(|_| AgentError::Config("GITHUB_COPILOT_TOKEN not set".into()))?;

        let resp = self.client
            .post(self.endpoint("/chat/completions"))
            .header("Authorization", format!("Bearer {}", std::env::var("GITHUB_COPILOT_TOKEN").unwrap()))
            .header("Content-Type", "application/json")
            .json(&args)
            .send()
            .map_err(|e| AgentError::Transport(format!("Copilot request failed: {e}")))?
            .json::<Value>()
            .map_err(|e| AgentError::Transport(format!("Failed to parse Copilot response: {e}")))?;

        // Parse OpenAI-compatible response
        parse_openai_message(&resp, None)
    }

    fn set_model(&self, model: &str) {
        *self.model.borrow_mut() = model.to_string();
    }

    fn current_model(&self) -> Option<String> {
        let m = self.model.borrow();
        if m.is_empty() { None } else { Some(m.clone()) }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        // For now, return known Copilot models
        Ok(vec![
            ModelInfo {
                id: "gpt-4o".to_string(),
                name: "GPT-4o".to_string(),
                context_window: Some(128000),
                max_output_tokens: Some(16384),
                provider: "github-copilot".to_string(),
            },
            ModelInfo {
                id: "gpt-4o-mini".to_string(),
                name: "GPT-4o mini".to_string(),
                context_window: Some(128000),
                max_output_tokens: Some(16384),
                provider: "github-copilot".to_string(),
            },
            ModelInfo {
                id: "gpt-4-turbo".to_string(),
                name: "GPT-4 Turbo".to_string(),
                context_window: Some(128000),
                max_output_tokens: Some(4096),
                provider: "github-copilot".to_string(),
            },
        ])
    }
}

impl CopilotTransport {
    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            s.to_string()
        } else {
            format!("{}... [truncated {} bytes]", &s[..max], s.len() - max)
        }
    }
}

impl Default for CopilotTransport {
    fn default() -> Self {
        Self::new("gpt-4o").expect("Failed to create CopilotTransport")
    }
}