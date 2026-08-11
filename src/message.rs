//! The unified conversation message — the single source of truth for state.
//!
//! This mirrors the OpenAI `messages` schema, which is also what the
//! `ProviderTransport` normalizes everything to. Backed by `serde` derives so
//! (de)serialization is handled by `serde_json` rather than hand-rolled.

use serde::{Deserialize, Serialize};

/// The role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// The API string form (`"system"`, `"user"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A single conversation message.
///
/// `content` is the visible text. `tool_calls` carries assistant-requested
/// tool invocations. `tool_call_id` links a `Tool` message back to the call
/// that produced it. `name` is the tool's name (set on `Tool` messages).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Present only on `Assistant` messages that request tool calls.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    /// Present only on `Tool` messages: the id of the call being answered.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    /// Present only on `Tool` messages: the tool's name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

/// An assistant-requested tool invocation, in the API wire shape
/// (`{id, type: "function", function: {name, arguments}}`).
///
/// `type` is always serialized (not skipped when `"function"`) — some
/// OpenAI-compatible servers (e.g. vLLM) validate it as a required field
/// even though it never varies in practice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

fn function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Raw JSON arguments string, exactly as the model emitted them.
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: function_type(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.function.name
    }

    pub fn arguments(&self) -> &str {
        &self.function.arguments
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            ..Default::default()
        }
    }

    pub fn tool(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: some OpenAI-compatible servers (vLLM, observed live
    /// against a self-hosted deployment) reject a follow-up request after a
    /// tool call if the wire-format `type` field is missing from the
    /// echoed-back tool_calls — even though `"function"` is the only value
    /// that ever appears. `skip_serializing_if` on that field used to omit
    /// it in exactly that case. Assert it's always present.
    #[test]
    fn tool_call_always_serializes_type_field() {
        let call = ToolCall::new("call_1", "read", "{\"path\":\"a.txt\"}");
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["type"], "function");
    }

    #[test]
    fn assistant_message_with_tool_calls_round_trips() {
        let msg = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall::new("call_1", "read", "{}")],
            tool_call_id: None,
            name: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
    }
}
