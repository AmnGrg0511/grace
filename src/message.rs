//! The unified conversation message — the single source of truth for state.
//!
//! This mirrors the OpenAI `messages` schema, which is also what the
//! `ProviderTransport` normalizes everything to. Keeping one message type
//! (rather than per-provider shapes) is the key simplification that makes the
//! core small: the transport converts *to* API format, the agent only ever
//! sees this.

use crate::json::Json;

/// The role of a message in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Default)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Present only on `Assistant` messages that request tool calls.
    pub tool_calls: Vec<ToolCall>,
    /// Present only on `Tool` messages: the id of the call being answered.
    pub tool_call_id: Option<String>,
    /// Present only on `Tool` messages: the tool's name.
    pub name: Option<String>,
}

/// An assistant-requested tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string, exactly as the model emitted them.
    pub arguments: String,
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

    pub fn tool(tool_call_id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    /// Serialize this message to the OpenAI `messages` JSON shape.
    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            (String::from("role"), Json::String(self.role.as_str().to_string())),
            (String::from("content"), Json::String(self.content.clone())),
        ];
        if !self.tool_calls.is_empty() {
            let calls: Vec<Json> = self
                .tool_calls
                .iter()
                .map(|tc| {
                    Json::Object(vec![
                        (String::from("id"), Json::String(tc.id.clone())),
                        (
                            String::from("type"),
                            Json::String(String::from("function")),
                        ),
                        (
                            String::from("function"),
                            Json::Object(vec![
                                (String::from("name"), Json::String(tc.name.clone())),
                                (
                                    String::from("arguments"),
                                    Json::String(tc.arguments.clone()),
                                ),
                            ]),
                        ),
                    ])
                })
                .collect();
            pairs.push((String::from("tool_calls"), Json::Array(calls)));
        }
        if let Some(id) = &self.tool_call_id {
            pairs.push((String::from("tool_call_id"), Json::String(id.clone())));
        }
        if let Some(name) = &self.name {
            pairs.push((String::from("name"), Json::String(name.clone())));
        }
        Json::Object(pairs)
    }
}
