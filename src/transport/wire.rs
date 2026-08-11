//! Shared OpenAI wire-format helpers.
//!
//! Both [`http`](super::http) and [`copilot`](super::copilot) speak the same
//! `/chat/completions` dialect, so the request/response shaping lives here
//! once instead of being duplicated per provider.

use super::r#trait::{FinishReason, ModelResponse, ToolSpec};
use crate::util::Result;
use serde::Serialize;
use serde_json::Value;

/// Encode '/' and other URI-unfriendly chars for model-id path segments.
pub fn urlencoding(s: &str) -> String {
    s.replace('/', "%2F")
        .replace('.', "%2E")
        .replace(':', "%3A")
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
pub fn tools_to_json(tools: &[ToolSpec]) -> Value {
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

/// Parse an OpenAI-style `choices[0].message` JSON into a [`ModelResponse`].
pub fn parse_openai_message(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn urlencoding_escapes_path_hostile_chars() {
        assert_eq!(urlencoding("openai/gpt-4o"), "openai%2Fgpt-4o");
        assert_eq!(urlencoding("a.b:c"), "a%2Eb%3Ac");
    }

    #[test]
    fn tools_to_json_emits_openai_function_shape() {
        let specs = vec![ToolSpec {
            name: "read".into(),
            description: "reads".into(),
            parameters: json!({"type": "object"}),
        }];
        let v = tools_to_json(&specs);
        assert_eq!(v[0]["type"], "function");
        assert_eq!(v[0]["function"]["name"], "read");
        assert_eq!(v[0]["function"]["description"], "reads");
    }

    #[test]
    fn empty_tool_list_is_an_empty_array_not_null() {
        assert_eq!(tools_to_json(&[]), json!([]));
    }

    #[test]
    fn parse_plain_content_message() {
        let msg = json!({"content": "hi there"});
        let resp = parse_openai_message(&msg, Some("stop")).unwrap();
        assert_eq!(resp.content, "hi there");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn parse_tool_calls_message() {
        let msg = json!({
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
            }]
        });
        let resp = parse_openai_message(&msg, Some("tool_calls")).unwrap();
        assert_eq!(resp.content, "");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name(), "bash");
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn missing_arguments_default_to_empty_object() {
        // A model emitting a no-arg tool call without `arguments` must not
        // produce a JSON parse failure downstream in the registry.
        let msg = json!({
            "tool_calls": [{"id": "c", "function": {"name": "noop"}}]
        });
        let resp = parse_openai_message(&msg, Some("tool_calls")).unwrap();
        assert_eq!(resp.tool_calls[0].arguments(), "{}");
    }

    #[test]
    fn absent_finish_reason_is_treated_as_stop() {
        let msg = json!({"content": "done"});
        let resp = parse_openai_message(&msg, None).unwrap();
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }
}
