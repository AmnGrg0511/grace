//! Provider transport — the vendor-neutral seam.
//!
//! [`ProviderTransport`] is the only thing the agent loop knows about "an LLM".
//! Every provider (OpenAI, Anthropic, Ollama, a mock) implements these five
//! methods. This is the seam Hermes isolates behind `transports/base.py`:
//! `convert_messages / convert_tools / build_kwargs / normalize_response /
//! map_finish_reason`. We collapse it into one normalized call.

use crate::error::Result;
use crate::json::Json;
use crate::message::Message;

/// The set of tools, in the OpenAI tool-spec shape, that the model may call.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema fragment for the `properties` of the function.
    /// Stored as a `Json` object so we never have to re-serialize by hand.
    pub parameters: Json,
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
}

/// Build the OpenAI-compatible `tools` payload from our specs.
pub(crate) fn tools_to_json(tools: &[ToolSpec]) -> Json {
    Json::Array(
        tools
            .iter()
            .map(|t| {
                Json::Object(vec![
                    (String::from("type"), Json::String(String::from("function"))),
                    (
                        String::from("function"),
                        Json::Object(vec![
                            (String::from("name"), Json::String(t.name.clone())),
                            (String::from("description"), Json::String(t.description.clone())),
                            (
                                String::from("parameters"),
                                t.parameters.clone(),
                            ),
                        ]),
                    ),
                ])
            })
            .collect(),
    )
}

/// Parse an OpenAI-style `choices[0].message` JSON into our [`ModelResponse`].
pub(crate) fn parse_openai_message(msg: &Json) -> Result<ModelResponse> {
    let content = msg
        .get("content")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = msg.get("tool_calls").and_then(Json::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            let func = call.get("function").cloned().unwrap_or(Json::Null);
            let name = func
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = func
                .get("arguments")
                .and_then(Json::as_str)
                .unwrap_or("{}")
                .to_string();
            tool_calls.push(crate::message::ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    let finish_reason = msg
        .get("finish_reason")
        .and_then(Json::as_str)
        .map(FinishReason::from_api)
        .unwrap_or(FinishReason::Stop);

    // If the API nests finish_reason at the choice level, the caller passes it
    // via the message object already; here we fall back to Stop if absent.
    Ok(ModelResponse {
        content,
        tool_calls,
        finish_reason,
    })
}
