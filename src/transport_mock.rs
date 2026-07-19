//! Mock transport — proves the agent loop end-to-end with zero network.
//!
//! It is a tiny scripted "model": it parses the last user/tool message and,
//! based on simple keywords, either (a) emits a `run_terminal` tool call to
//! demonstrate the tool-execution path, (b) answers directly, or (c) finishes.
//! This is what lets `cargo test` and `--mock` runs verify the *loop*, the
//! *tool registry*, and *message bookkeeping* without any provider.

use crate::error::Result;
use crate::message::{Message, ToolCall};
use crate::transport::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
use serde_json::json;

/// A deterministic scripted LLM. Useful for tests and offline demos.
pub struct MockTransport {
    /// Maximum number of tool-calling rounds before it answers directly.
    /// Prevents infinite loops if a script keeps requesting tools.
    max_tool_rounds: u32,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self { max_tool_rounds: 2 }
    }
}

impl MockTransport {
    pub fn new(max_tool_rounds: u32) -> Self {
        Self { max_tool_rounds }
    }

    /// Count how many assistant turns have already requested tools.
    fn tool_rounds_so_far(messages: &[Message]) -> u32 {
        messages
            .iter()
            .filter(|m| m.role == crate::message::Role::Assistant && !m.tool_calls.is_empty())
            .count() as u32
    }
}

impl ProviderTransport for MockTransport {
    fn name(&self) -> &str {
        "mock"
    }

    fn complete(&self, messages: &[Message], _tools: &[ToolSpec], _model: &str) -> Result<ModelResponse> {
        // Match intent against the *original user prompt* (the first User
        // message), not the latest tool result — otherwise a tool's output
        // (e.g. "hello from tool") would be mistaken for the user's request.
        let user_intent = messages
            .iter()
            .find(|m| m.role == crate::message::Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let rounds = Self::tool_rounds_so_far(messages);

        let wants_terminal = user_intent.to_lowercase().contains("run")
            || user_intent.to_lowercase().contains("command")
            || user_intent.to_lowercase().contains("terminal");
        let wants_file = user_intent.to_lowercase().contains("write") || user_intent.to_lowercase().contains("file");

        if rounds < self.max_tool_rounds {
            if wants_terminal {
                return Ok(ModelResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall::new(
                        format!("call_{rounds}"),
                        "run_terminal",
                        json!({"command": "echo 'hello from tool'"}).to_string(),
                    )],
                    finish_reason: FinishReason::ToolCalls,
                });
            }
            if wants_file {
                return Ok(ModelResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall::new(
                        format!("call_{rounds}"),
                        "write_file",
                        json!({
                            "path": "/tmp/grace_demo.txt",
                            "content": "written by the minimal core"
                        })
                        .to_string(),
                    )],
                    finish_reason: FinishReason::ToolCalls,
                });
            }
        }

        Ok(ModelResponse {
            content: format!("Understood. (mock response after {rounds} tool round(s))"),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
        })
    }
}
