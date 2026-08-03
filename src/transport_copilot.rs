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
        
        // Check for cached token file
        let token_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".grace")
            .join("copilot_token");
        
        if let Ok(token) = std::fs::read_to_string(&token_path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        
        // Trigger device flow
        let device_code = Self::start_device_flow()?;
        
        println!("\n🔐 GitHub Copilot Authentication Required");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Please authorize GitHub Copilot in your browser:");
        println!();
        println!("  1. Open: {}", device_code.verification_uri);
        println!("  2. Enter code: {}", device_code.user_code);
        println!();
        println!("Waiting for authorization... (press Ctrl+C to cancel)");
        
        // Poll for token
        let token = Self::poll_token(&device_code.device_code, device_code.interval)?;
        
        // Save token to cache file
        if let Some(parent) = token_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&token_path, &token);
        
        println!("\n✅ Authentication successful! Token cached for future use.\n");
        
        Ok(token)
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
            let text = resp.text().map_err(|e| AgentError::Transport(format!("Failed to read response: {e}")))?;
            
            // Check for error conditions in text BEFORE trying to parse JSON
            if text.contains("authorization_pending") {
                continue;
            }
            if text.contains("slow_down") {
                continue;
            }
            if text.contains("expired_token") {
                return Err(AgentError::Config("Device code expired".into()));
            }
            if text.contains("access_denied") {
                return Err(AgentError::Config("User denied authorization".into()));
            }
            
            if !status.is_success() {
                if attempt % 6 == 0 { // Print every 30 seconds
                    eprintln!("Polling... (attempt {}/{})", attempt + 1, max_attempts);
                }
                continue;
            }
            
            // Try to parse as JSON
            let token_resp: TokenResponse = serde_json::from_str(&text)
                .map_err(|e| AgentError::Transport(format!("Failed to parse token response: {e}. Raw: {}", Self::truncate(&text, 200))))?;
            
            if let Some(token) = token_resp.access_token {
                if !token.is_empty() {
                    return Ok(token);
                }
            }
            
            if let Some(err) = &token_resp.error {
                if err == "authorization_pending" {
                    continue;
                }
                if err == "slow_down" {
                    continue;
                }
                if err == "expired_token" {
                    return Err(AgentError::Config("Device code expired".into()));
                }
                if err == "access_denied" {
                    return Err(AgentError::Config("User denied authorization".into()));
                }
            }
        }
        Err(AgentError::Config("Device flow timed out".into()))
    }
    
    /// Get or create access token (with caching)
    fn get_access_token(&self) -> Result<String> {
        // Check for GITHUB_COPILOT_TOKEN env var first (highest priority)
        if let Ok(token) = std::env::var("GITHUB_COPILOT_TOKEN") {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        
        // Check for cached token file
        let token_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".grace")
            .join("copilot_token");
        
        if let Ok(token) = std::fs::read_to_string(&token_path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        
        // Fallback: require env var
        Err(AgentError::Config(
            "GITHUB_COPILOT_TOKEN not set and no cached token found. Run with --copilot to trigger device flow auth.".into()
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
        // Try to fetch models from Copilot API, fall back to known models
        match self.fetch_models_from_api() {
            Ok(models) => Ok(models),
            Err(_) => Ok(self.known_models()),
        }
    }
}

impl CopilotTransport {
    /// Fetch models from GitHub Copilot API
    fn fetch_models_from_api(&self) -> Result<Vec<ModelInfo>> {
        let token = self.get_access_token()?;
        let url = self.endpoint("/models");
        
        let resp = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/json")
            .send()
            .map_err(|e| AgentError::Transport(format!("Copilot models request failed: {e}")))?
            .json::<Value>()
            .map_err(|e| AgentError::Transport(format!("Failed to parse Copilot models response: {e}")))?;

        // Parse the models array from the response
        let models = resp["data"].as_array()
            .ok_or_else(|| AgentError::Transport("Invalid models response format".into()))?;
        
        let mut result = Vec::new();
        for model in models {
            if let (Some(id), Some(name)) = (
                model["id"].as_str(),
                model["name"].as_str(),
            ) {
                // Try to get context window from capabilities or use defaults
                let context_window = model["capabilities"]["context_window"]
                    .as_u64()
                    .map(|v| v as u32)
                    .or_else(|| {
                        // Default context windows based on model name
                        if id.contains("gpt-4o") {
                            Some(128000)
                        } else if id.contains("gpt-4-turbo") {
                            Some(128000)
                        } else if id.contains("gpt-3.5") {
                            Some(16384)
                        } else {
                            Some(8192)
                        }
                    });
                
                let max_output_tokens = model["capabilities"]["max_output_tokens"]
                    .as_u64()
                    .map(|v| v as u32)
                    .or_else(|| {
                        if id.contains("gpt-4o") && !id.contains("mini") {
                            Some(16384)
                        } else if id.contains("gpt-4-turbo") {
                            Some(4096)
                        } else if id.contains("gpt-4o-mini") {
                            Some(16384)
                        } else {
                            Some(4096)
                        }
                    });

                result.push(ModelInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                    context_window,
                    max_output_tokens,
                    provider: "github-copilot".to_string(),
                });
            }
        }
        
        if result.is_empty() {
            return Err(AgentError::Transport("No models found in Copilot response".into()));
        }
        
        Ok(result)
    }

    /// Known Copilot models as fallback
    fn known_models(&self) -> Vec<ModelInfo> {
        vec![
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
        ]
    }

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