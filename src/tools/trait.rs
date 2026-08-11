//! The [`Tool`] trait — the contract every capability implements.

use crate::util::Result;
use serde_json::Value;

/// A callable capability exposed to the model.
///
/// Implementations should be side-effect-honest: whatever `run` does is what
/// the model believes happened, because the returned string is the only thing
/// fed back into the conversation.
pub trait Tool {
    /// Stable name the model must emit to invoke this tool.
    fn name(&self) -> &str;

    /// Human-readable description (sent to the model in the tool spec). This
    /// is prompt text, not documentation — it is the only thing telling the
    /// model *when* to reach for this tool.
    fn description(&self) -> &str;

    /// JSON-schema object describing the arguments.
    fn parameters(&self) -> Value;

    /// Execute the tool with already-parsed arguments. The returned string is
    /// fed back to the model as the tool result.
    fn run(&self, args: &Value) -> Result<String>;
}

/// Pull a required string argument out of a tool's JSON arguments, with an
/// error message that tells the model exactly which key it forgot.
pub fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)
        .ok_or_else(|| {
            crate::util::AgentError::Tool(format!("missing string argument '{key}'"))
        })
}

/// Shorthand for a `{"type": "string", "description": ...}` schema property.
pub fn str_prop(desc: &str) -> Value {
    serde_json::json!({"type": "string", "description": desc})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arg_str_extracts_a_present_string() {
        let args = json!({"path": "/tmp/x"});
        assert_eq!(arg_str(&args, "path").unwrap(), "/tmp/x");
    }

    #[test]
    fn arg_str_names_the_missing_key() {
        let err = arg_str(&json!({}), "path").unwrap_err();
        assert!(err.to_string().contains("missing string argument 'path'"));
    }

    #[test]
    fn arg_str_rejects_a_non_string_value() {
        // A model sending `{"path": 3}` must get a clear message, not a
        // silent stringification of the wrong type.
        let err = arg_str(&json!({"path": 3}), "path").unwrap_err();
        assert!(err.to_string().contains("missing string argument"));
    }

    #[test]
    fn str_prop_builds_a_schema_fragment() {
        let p = str_prop("a path");
        assert_eq!(p["type"], "string");
        assert_eq!(p["description"], "a path");
    }
}
