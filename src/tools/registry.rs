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

/// Tools that read-only mode hides (from `specs`/`names`) and refuses (in
/// `execute`) when on. `write`/`edit` mutate files, `bash` can do anything,
/// and `delegate` would hand a sub-agent the very capabilities we are
/// withholding — so all four drop together.
const READ_ONLY_HIDDEN: &[&str] = &["bash", "delegate", "edit", "write"];

/// Owns the set of available tools and dispatches by name.
///
/// The registry also carries the session's **read-only posture** as a shared
/// flag. A "filtered registry" is therefore not a second object: when the
/// flag is on, `specs`/`names`/`len` simply omit the mutating tools and
/// `execute` refuses them. Sharing one `Arc<AtomicBool>` across registries
/// is what lets delegated sub-agents (separate registry objects) inherit the
/// posture, and a later `/readonly` toggle cover them live.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    read_only: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a read-only flag shared with other registries (e.g. every
    /// registry one `RegistryOptions` builds, main and delegated alike).
    pub fn set_shared_read_only(
        &mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        self.read_only = flag;
    }

    /// The shared flag, for registries being built from the same options.
    pub fn shared_read_only(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::clone(&self.read_only)
    }

    /// Toggle the read-only posture for every registry sharing this flag.
    pub fn set_read_only(&self, on: bool) {
        self.read_only.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether read-only mode is on right now.
    pub fn is_read_only(&self) -> bool {
        self.read_only.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A tool is usable now if it exists AND read-only mode does not hide it.
    fn usable(&self, name: &str) -> bool {
        self.tools.contains_key(name)
            && (!self.is_read_only() || !READ_ONLY_HIDDEN.contains(&name))
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

    /// Every usable tool name, sorted — stable output for banners, tests,
    /// and the delegation allow-list error message. Read-only mode hides the
    /// mutating tools from this list.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .tools
            .keys()
            .filter(|n| !self.is_read_only() || !READ_ONLY_HIDDEN.contains(&n.as_str()))
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names
    }

    /// Number of usable tools (read-only mode hides the mutating ones).
    pub fn len(&self) -> usize {
        if self.is_read_only() {
            self.tools
                .keys()
                .filter(|n| !READ_ONLY_HIDDEN.contains(&n.as_str()))
                .count()
        } else {
            self.tools.len()
        }
    }

    /// Whether no tools are usable right now.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All usable tools, as provider-agnostic specs. Read-only mode omits
    /// the mutating tools here so the model never asks for them.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .filter(|t| !self.is_read_only() || !READ_ONLY_HIDDEN.contains(&t.name()))
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
    /// Returns the names that were requested but are not usable now (absent,
    /// or hidden by read-only mode), so the caller can tell the model it
    /// asked for something that does not exist instead of silently running
    /// with fewer tools than it believes it has.
    pub fn missing(&self, requested: &[String]) -> Vec<String> {
        requested
            .iter()
            .filter(|n| !self.usable(n.as_str()))
            .cloned()
            .collect()
    }

    /// Parse `arguments` (a JSON object string) and run the named tool.
    pub fn execute(&self, name: &str, arguments: &str) -> Result<String> {
        let tool = self
            .get(name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool '{name}'")))?;
        // Read-only mode refuses the mutating tools even though they remain
        // registered — a spec the model already cached, or a stray call, must
        // not mutate while the posture is on.
        if self.is_read_only() && READ_ONLY_HIDDEN.contains(&name) {
            return Err(AgentError::Tool(format!(
                "tool '{name}' is unavailable: read-only mode is on (/readonly off to restore it)"
            )));
        }
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

    fn full_registry() -> ToolRegistry {
        registry_with(&["read", "write", "edit", "bash", "delegate"])
    }

    #[test]
    fn read_only_hides_the_mutation_tools_from_specs_and_names() {
        let reg = full_registry();
        reg.set_read_only(true);
        let spec_names: Vec<String> = reg.specs().into_iter().map(|s| s.name).collect();
        assert!(!spec_names.contains(&"write".into()));
        assert!(!spec_names.contains(&"edit".into()));
        assert!(!spec_names.contains(&"bash".into()));
        assert!(!spec_names.contains(&"delegate".into()));
        assert!(spec_names.contains(&"read".into()));
        assert_eq!(reg.names(), vec!["read"]);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn read_only_execute_refuses_with_an_actionable_message() {
        let reg = full_registry();
        reg.set_read_only(true);
        for name in ["write", "edit", "bash", "delegate"] {
            let err = reg.execute(name, "{}").unwrap_err().to_string();
            assert!(err.contains("read-only mode is on"), "{name}: {err}");
            assert!(err.contains("/readonly off"), "{name}: {err}");
        }
        // Non-hidden tools still run.
        assert_eq!(reg.execute("read", r#"{"text":"hi"}"#).unwrap(), "hi");
    }

    #[test]
    fn read_only_off_restores_everything() {
        let reg = full_registry();
        reg.set_read_only(true);
        reg.set_read_only(false);
        assert_eq!(reg.len(), 5);
        assert_eq!(reg.names(), vec!["bash", "delegate", "edit", "read", "write"]);
        assert_eq!(
            reg.specs().len(),
            5,
            "turning the mode off must restore the specs"
        );
    }

    #[test]
    fn missing_treats_hidden_tools_as_missing_under_read_only() {
        let reg = full_registry();
        reg.set_read_only(true);
        let missing = reg.missing(&["read".into(), "write".into()]);
        assert_eq!(missing, vec!["write"]);
    }

    #[test]
    fn a_shared_flag_covers_every_registry_that_holds_it() {
        // Delegation model: the parent and the sub-agent are separate
        // registries over one shared flag.
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut parent = full_registry();
        let mut child = full_registry();
        parent.set_shared_read_only(std::sync::Arc::clone(&flag));
        child.set_shared_read_only(flag);
        assert!(!parent.is_read_only());
        parent.set_read_only(true);
        assert!(child.is_read_only(), "delegated registry must inherit the posture");
        assert!(parent.get("write").is_some(), "tools stay registered");
        assert!(child.get("write").is_some());
        parent.set_read_only(false);
        assert!(!child.is_read_only(), "toggling off must cover them too");
    }
}
