//! Provider transport — the vendor-neutral seam.
//!
//! [`ProviderTransport`] is the only thing the agent loop knows about "an LLM".
//! Every provider (OpenAI, Anthropic, Ollama, a mock) implements these two
//! methods. This is the seam isolated behind `ProviderTransport`.

use crate::error::Result;
use crate::message::Message;
use serde::Serialize;
use serde_json::Value;

/// The set of tools, in the OpenAI tool-spec shape, that the model may call.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema fragment for the `properties` of the function.
    pub parameters: Value,
}

/// What a model returned for one turn.
#[derive(Debug, Clone, Default)]
pub struct ModelResponse {
    /// Assistant text (may be empty when the model only emits tool calls).
    pub content: String,
    /// Tool invocations the model is asking us to run.
    pub tool_calls: Vec<crate::message::ToolCall>,
    /// Why the model stopped: `stop` ends the turn; anything else usually
    /// means "continue" (e.g. `tool_calls`, `length`).
    pub finish_reason: FinishReason,
}

/// Normalized stop reason, independent of provider vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// Model produced a final answer; the turn is done.
    #[default]
    Stop,
    /// Model requested tool calls; the loop must execute and continue.
    ToolCalls,
    /// Output was truncated by a length limit; the loop continues.
    Length,
}

impl FinishReason {
    /// Map a provider-specific finish-reason string to ours.
    pub fn from_api(s: &str) -> Self {
        match s {
            "stop" | "end_turn" => FinishReason::Stop,
            "tool_calls" | "function_call" => FinishReason::ToolCalls,
            "length" | "max_tokens" => FinishReason::Length,
            _ => FinishReason::Stop,
        }
    }
}

/// A normalized LLM endpoint.
pub trait ProviderTransport {
    /// Stable identifier, e.g. `"openai"`, `"mock"`. For diagnostics/logging.
    fn name(&self) -> &str;

    /// Send the conversation and available tools; return the model's response.
    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        model: &str,
    ) -> Result<ModelResponse>;

    /// Switch the model this transport talks to, for `/model` mid-chat.
    /// Default no-op (e.g. `MockTransport` has no real model to switch);
    /// `HttpTransport` overrides this to actually change what it sends.
    fn set_model(&self, _model: &str) {}

    /// Current model name, if this transport has one to report (used by
    /// `/model` with no argument to show what's active).
    fn current_model(&self) -> Option<String> {
        None
    }
}

#[derive(Serialize)]
struct ToolFunctionJson<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
struct ToolJson<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunctionJson<'a>,
}

/// Build the OpenAI-compatible `tools` payload from our specs.
pub(crate) fn tools_to_json(tools: &[ToolSpec]) -> Value {
    let items: Vec<ToolJson> = tools
        .iter()
        .map(|t| ToolJson {
            kind: "function",
            function: ToolFunctionJson {
                name: &t.name,
                description: &t.description,
                parameters: &t.parameters,
            },
        })
        .collect();
    serde_json::to_value(items).unwrap_or(Value::Array(vec![]))
}

/// Parse an OpenAI-style `choices[0].message` JSON into our [`ModelResponse`].
pub(crate) fn parse_openai_message(
    msg: &Value,
    finish_reason_str: Option<&str>,
) -> Result<ModelResponse> {
    let content = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let func = call.get("function").cloned().unwrap_or(Value::Null);
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = func
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            tool_calls.push(crate::message::ToolCall::new(id, name, arguments));
        }
    }

    let finish_reason = finish_reason_str
        .map(FinishReason::from_api)
        .unwrap_or(FinishReason::Stop);

    Ok(ModelResponse {
        content,
        tool_calls,
        finish_reason,
    })
}
