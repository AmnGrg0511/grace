//! Tools — the model's hands.
//!
//! A [`Tool`] is anything the loop can invoke. The registry maps a name
//! (as emitted by the model) to a handler.

use crate::error::Result;
use serde_json::Value;
use std::collections::HashMap;

/// A callable capability exposed to the model.
pub trait Tool {
    /// Stable name the model must emit to invoke this tool.
    fn name(&self) -> &str;
    /// Human-readable description (sent to the model in the tool spec).
    fn description(&self) -> &str;
    /// JSON-schema `properties` object describing the arguments.
    fn parameters(&self) -> Value;
    /// Execute the tool with already-parsed arguments. The returned string is
    /// fed back to the model as the tool result.
    fn run(&self, args: &Value) -> Result<String>;
}

/// Owns the set of available tools and dispatches by name.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool. Later registrations with the same name replace earlier.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// All registered tools, as provider-agnostic specs.
    pub fn specs(&self) -> Vec<crate::transport::ToolSpec> {
        self.tools
            .values()
            .map(|t| crate::transport::ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Parse `arguments` (a JSON object string) and run the named tool.
    pub fn execute(&self, name: &str, arguments: &str) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| crate::error::AgentError::Tool(format!("unknown tool '{name}'")))?;
        let parsed: Value = serde_json::from_str(arguments)
            .map_err(|e| crate::error::AgentError::Tool(format!("bad arguments json: {e}")))?;
        tool.run(&parsed)
    }
}
