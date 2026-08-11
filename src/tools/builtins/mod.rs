//! The bundled built-in tools.
//!
//! These are intentionally thin wrappers over `std` I/O. Each tool declares
//! its name/description/parameters, pulls typed fields out of the JSON args,
//! performs the side effect, and returns a short string result that is fed
//! straight back to the model.
//
//! Matches Pi's minimal set: read, write, edit, bash (4 tools).

pub mod file;
pub mod patch;
pub mod terminal;

pub use file::{ReadTool, WriteTool};
pub use patch::EditTool;
pub use terminal::BashTool;

use crate::tools::registry::ToolRegistry;

/// Register the default built-in tool set into a registry.
///
/// This is the single definition of "what a bare Grace agent can do" —
/// delegation and the CLI both build on top of it rather than each assembling
/// their own list, so a sub-agent can never silently have a different notion
/// of the baseline toolset than its parent.
pub fn register_builtins(registry: &mut ToolRegistry) {
    registry.register(Box::new(BashTool));
    registry.register(Box::new(ReadTool));
    registry.register(Box::new(WriteTool));
    registry.register(Box::new(EditTool));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_the_documented_baseline_set() {
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        assert_eq!(reg.names(), vec!["bash", "edit", "read", "write"]);
    }

    #[test]
    fn every_builtin_exposes_an_object_schema() {
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        for spec in reg.specs() {
            assert_eq!(
                spec.parameters["type"], "object",
                "tool {} must declare an object schema",
                spec.name
            );
            assert!(
                !spec.description.is_empty(),
                "tool {} needs a description — it is the only thing telling the model when to use it",
                spec.name
            );
        }
    }

    #[test]
    fn registering_twice_is_idempotent() {
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        let first = reg.len();
        register_builtins(&mut reg);
        assert_eq!(reg.len(), first);
    }
}
