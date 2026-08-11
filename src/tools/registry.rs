//! [`ToolRegistry`] — name-to-handler dispatch for the model's hands.
//!
//! The registry is deliberately dumb: it owns boxed [`Tool`]s, hands out
//! [`ToolSpec`]s for the provider payload, and routes a `(name, arguments)`
//! pair to the right handler. It has no opinion about *which* tools exist —
//! that is [`crate::config`]'s job — which is what lets the same registry
//! serve the main agent, a delegated sub-agent with a narrowed tool set, and
//! a test with no tools at all.

use super::r#trait::Tool;
use crate::transport::ToolSpec;
use crate::util::{AgentError, Result};
use serde_json::Value;
use std::collections::HashMap;

/// Owns the set of available tools and dispatches by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Later registrations with the same name replace earlier
    /// ones, so a caller can deliberately override a built-in.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(std::convert::AsRef::as_ref)
    }

    /// Every registered tool name, sorted — stable output for banners, tests,
    /// and the delegation allow-list error message.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// All registered tools, as provider-agnostic specs.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// A registry containing only the named subset of this one's tools,
    /// re-resolved from `factory`. Used by delegation to hand a sub-agent a
    /// narrowed capability set.
    ///
    /// Returns the names that were requested but not found, so the caller can
    /// tell the model it asked for something that does not exist instead of
    /// silently running with fewer tools than it believes it has.
    pub fn missing(&self, requested: &[String]) -> Vec<String> {
        requested
            .iter()
            .filter(|n| !self.tools.contains_key(n.as_str()))
            .cloned()
            .collect()
    }

    /// Parse `arguments` (a JSON object string) and run the named tool.
    pub fn execute(&self, name: &str, arguments: &str) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool '{name}'")))?;
        // Models routinely emit `""` or whitespace for a no-argument call;
        // treating that as `{}` avoids a spurious "bad arguments json" that
        // the model then has to burn an iteration recovering from.
        let trimmed = arguments.trim();
        let parsed: Value = if trimmed.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(trimmed)
                .map_err(|e| AgentError::Tool(format!("bad arguments json: {e}")))?
        };
        tool.run(&parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo(&'static str);

    impl Tool for Echo {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "echoes its text argument"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn run(&self, args: &Value) -> Result<String> {
            Ok(args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("<none>")
                .to_string())
        }
    }

    fn registry_with(names: &[&'static str]) -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        for n in names {
            reg.register(Box::new(Echo(n)));
        }
        reg
    }

    #[test]
    fn empty_registry_reports_itself_empty() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.specs().is_empty());
    }

    #[test]
    fn register_then_get_and_execute() {
        let reg = registry_with(&["echo"]);
        assert!(reg.get("echo").is_some());
        let out = reg.execute("echo", r#"{"text":"hi"}"#).unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn unknown_tool_is_a_tool_error_naming_the_tool() {
        let reg = ToolRegistry::new();
        let err = reg.execute("nope", "{}").unwrap_err();
        assert!(err.to_string().contains("unknown tool 'nope'"));
    }

    #[test]
    fn empty_arguments_string_is_treated_as_an_empty_object() {
        // Models frequently send "" for a no-arg call. Rejecting that wastes
        // an iteration on a recoverable formatting quirk.
        let reg = registry_with(&["echo"]);
        assert_eq!(reg.execute("echo", "").unwrap(), "<none>");
        assert_eq!(reg.execute("echo", "   ").unwrap(), "<none>");
    }

    #[test]
    fn malformed_arguments_json_is_reported_clearly() {
        let reg = registry_with(&["echo"]);
        let err = reg.execute("echo", "{not json").unwrap_err();
        assert!(err.to_string().contains("bad arguments json"));
    }

    #[test]
    fn later_registration_replaces_an_earlier_same_named_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(Echo("dup")));
        reg.register(Box::new(Echo("dup")));
        assert_eq!(reg.len(), 1, "same name must not duplicate");
    }

    #[test]
    fn names_are_sorted_for_stable_output() {
        let reg = registry_with(&["zulu", "alpha", "mike"]);
        assert_eq!(reg.names(), vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn specs_expose_every_registered_tool() {
        let reg = registry_with(&["a", "b"]);
        let mut names: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn missing_reports_requested_tools_that_do_not_exist() {
        let reg = registry_with(&["read"]);
        let missing = reg.missing(&["read".into(), "fly_to_mars".into()]);
        assert_eq!(missing, vec!["fly_to_mars"]);
    }
}
