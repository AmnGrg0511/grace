//! The tool system — the model's hands.
//!
//! ```text
//! trait.rs      the Tool contract + JSON-arg helpers
//! registry.rs   ToolRegistry: name -> handler dispatch
//! builtins/     terminal, file, patch — the baseline capability set
//! delegate.rs   `delegate`: run a bounded sub-agent as a tool call
//! session.rs    `session_search`: FTS over past conversations
//! plugin.rs     executable tools discovered from a directory
//! ```

pub mod builtins;
pub mod delegate;
pub mod plugin;
pub mod registry;
pub mod session;
pub mod r#trait;

pub use builtins::{
    register_builtins, BashTool, EditTool, ReadTool, WriteTool,
};
pub use delegate::DelegateTool;
pub use plugin::{PluginTool, PluginToolStore};
pub use registry::ToolRegistry;
pub use session::SessionSearchTool;
pub use r#trait::{arg_str, str_prop, Tool};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_and_trait_are_reachable_from_the_module_root() {
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        assert!(!reg.is_empty());
        let _: &dyn Tool = reg.get("read").unwrap();
    }

    #[test]
    fn every_builtin_type_is_individually_reachable() {
        // Callers assembling a custom registry need these by name.
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BashTool),
            Box::new(ReadTool),
            Box::new(WriteTool),
            Box::new(EditTool),
        ];
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec!["bash", "read", "write", "edit"]
        );
    }

    #[test]
    fn the_argument_helpers_are_reachable() {
        assert_eq!(
            arg_str(&serde_json::json!({"k": "v"}), "k").unwrap(),
            "v"
        );
        assert_eq!(str_prop("d")["type"], "string");
    }
}
