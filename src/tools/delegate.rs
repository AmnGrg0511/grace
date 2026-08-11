//! The `delegate` tool — exposes [`crate::core::delegation`] to the model.
//!
//! Registration used to leak into `main.rs`, which meant the one place that
//! knows how to build a tool registry ([`crate::config`]) did *not* know about
//! the delegate tool, and anything else constructing a registry silently got a
//! Grace that could not delegate. It is now assembled with every other tool.
//!
//! The sub-registry handed to a sub-agent deliberately omits `delegate` once
//! the depth cap is reached, so nesting terminates structurally rather than
//! relying on the model to stop asking.

use crate::core::delegation::{
    Delegation, DelegationDepth, SubTask, DEFAULT_DELEGATION_BUDGET, MAX_DELEGATION_BUDGET,
};
use crate::tools::r#trait::{arg_str, str_prop, Tool};
use crate::tools::ToolRegistry;
use crate::transport::ProviderTransport;
use crate::util::Result;
use serde_json::{json, Value};
use std::rc::Rc;

/// Builds the tool registry a sub-agent runs against.
///
/// Boxed rather than a captured registry because the sub-registry must be
/// rebuilt per call: the parent's registry contains `delegate` itself, and
/// handing that to a child unchanged is how you get infinite recursion.
pub type SubRegistryFactory = Rc<dyn Fn(DelegationDepth) -> ToolRegistry>;

/// A tool named `delegate`: runs a bounded sub-agent and returns its answer.
pub struct DelegateTool {
    transport: Rc<dyn ProviderTransport>,
    depth: DelegationDepth,
    make_registry: SubRegistryFactory,
    compression: Option<crate::core::context::ContextCompressionConfig>,
}

impl DelegateTool {
    /// Build a delegate tool for `transport`.
    ///
    /// `make_registry` is called per delegation with the *child's* depth, so
    /// it can decide whether to include `delegate` in the sub-agent's own tool
    /// set.
    pub fn new(
        transport: Rc<dyn ProviderTransport>,
        depth: DelegationDepth,
        make_registry: SubRegistryFactory,
    ) -> Self {
        Self {
            transport,
            depth,
            make_registry,
            compression: None,
        }
    }

    /// Apply a compression policy to sub-agent conversations.
    #[must_use]
    pub fn with_compression(
        mut self,
        cfg: crate::core::context::ContextCompressionConfig,
    ) -> Self {
        self.compression = Some(cfg);
        self
    }

    /// Parse the model's arguments into a [`SubTask`].
    fn parse_task(&self, args: &Value) -> Result<SubTask> {
        let task = arg_str(args, "task")?;
        let mut sub = SubTask::new(task);

        if let Some(budget) = args.get("max_iterations").and_then(Value::as_u64) {
            sub = sub.with_budget(u32::try_from(budget).unwrap_or(MAX_DELEGATION_BUDGET));
        }
        if let Some(context) = args.get("context").and_then(Value::as_str) {
            if !context.trim().is_empty() {
                sub = sub.with_context(context);
            }
        }
        if let Some(tools) = args.get("tools").and_then(Value::as_array) {
            let names: Vec<String> = tools
                .iter()
                .filter_map(Value::as_str)
                .map(std::string::ToString::to_string)
                .collect();
            if !names.is_empty() {
                sub = sub.with_tools(names);
            }
        }
        Ok(sub)
    }
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained subtask to a fresh sub-agent with its own \
         iteration budget and no access to this conversation's history. Returns \
         the sub-agent's final answer. Use this to keep a large, noisy subtask \
         (searching a codebase, summarizing many files, a long build-and-fix \
         loop) out of the main context. Because the sub-agent cannot see this \
         conversation, state everything it needs in `task` and `context`."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": str_prop(
                    "The complete, self-contained instruction for the sub-agent. \
                     It cannot see this conversation, so be explicit."
                ),
                "context": str_prop(
                    "Optional background the sub-agent needs (file paths, prior \
                     findings, constraints)."
                ),
                "max_iterations": {
                    "type": "integer",
                    "description": format!(
                        "Iteration budget for the sub-agent (default {DEFAULT_DELEGATION_BUDGET}, \
                         max {MAX_DELEGATION_BUDGET}). Raise it for multi-step work, \
                         lower it for a quick lookup."
                    ),
                },
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional subset of tool names the sub-agent may use. \
                                    Omit to give it the same tools you have.",
                },
            },
            "required": ["task"],
        })
    }

    fn run(&self, args: &Value) -> Result<String> {
        let task = self.parse_task(args)?;
        let child_depth = self.depth.child();
        let sub_tools = (self.make_registry)(child_depth);

        let mut delegation = Delegation::new(self.transport.as_ref()).at_depth(self.depth);
        if let Some(cfg) = &self.compression {
            delegation = delegation.with_compression(cfg.clone());
        }

        let report = delegation.run(&task, &sub_tools, None)?;
        Ok(report.to_tool_result())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delegation::MAX_DELEGATION_DEPTH;
    use crate::message::Message;
    use crate::tools::builtins::register_builtins;
    use crate::transport::{FinishReason, ModelResponse, ToolSpec};

    struct OneShot(&'static str);
    impl ProviderTransport for OneShot {
        fn name(&self) -> &str {
            "oneshot"
        }
        fn complete(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: self.0.to_string(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
            })
        }
    }

    fn factory() -> SubRegistryFactory {
        Rc::new(|_depth| {
            let mut reg = ToolRegistry::new();
            register_builtins(&mut reg);
            reg
        })
    }

    fn tool() -> DelegateTool {
        DelegateTool::new(
            Rc::new(OneShot("subtask complete")),
            DelegationDepth::ROOT,
            factory(),
        )
    }

    #[test]
    fn returns_the_sub_agents_answer_as_the_tool_result() {
        let out = tool().run(&json!({"task": "do the thing"})).unwrap();
        assert_eq!(out, "subtask complete");
    }

    #[test]
    fn missing_task_argument_is_reported() {
        let err = tool().run(&json!({})).unwrap_err();
        assert!(err.to_string().contains("missing string argument 'task'"));
    }

    #[test]
    fn schema_advertises_task_as_the_only_required_field() {
        let p = tool().parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "task");
        assert_eq!(p["required"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn schema_documents_the_default_and_max_budget() {
        // The model picks the budget; it can only do that sensibly if the
        // bounds are in the description it actually reads.
        let p = tool().parameters();
        let desc = p["properties"]["max_iterations"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains("25"));
        assert!(desc.contains("200"));
    }

    #[test]
    fn parses_an_explicit_budget() {
        let t = tool();
        let task = t.parse_task(&json!({"task": "x", "max_iterations": 7})).unwrap();
        assert_eq!(task.budget, 7);
    }

    #[test]
    fn an_absurd_budget_is_clamped_rather_than_obeyed() {
        let t = tool();
        let task = t
            .parse_task(&json!({"task": "x", "max_iterations": 100000}))
            .unwrap();
        assert_eq!(task.budget, MAX_DELEGATION_BUDGET);
    }

    #[test]
    fn omitted_budget_uses_the_default() {
        let t = tool();
        let task = t.parse_task(&json!({"task": "x"})).unwrap();
        assert_eq!(task.budget, DEFAULT_DELEGATION_BUDGET);
    }

    #[test]
    fn parses_a_tool_subset() {
        let t = tool();
        let task = t
            .parse_task(&json!({"task": "x", "tools": ["read", "edit"]}))
            .unwrap();
        assert_eq!(task.allowed_tools, vec!["read", "edit"]);
    }

    #[test]
    fn an_empty_tools_array_means_inherit_everything() {
        let t = tool();
        let task = t.parse_task(&json!({"task": "x", "tools": []})).unwrap();
        assert!(task.allowed_tools.is_empty());
    }

    #[test]
    fn blank_context_is_ignored_rather_than_padding_the_prompt() {
        let t = tool();
        let task = t.parse_task(&json!({"task": "x", "context": "   "})).unwrap();
        assert!(task.context.is_none());
    }

    #[test]
    fn context_is_carried_through() {
        let t = tool();
        let task = t
            .parse_task(&json!({"task": "x", "context": "use cargo"}))
            .unwrap();
        assert_eq!(task.context.as_deref(), Some("use cargo"));
    }

    #[test]
    fn the_sub_registry_factory_is_called_with_the_child_depth() {
        // Depth must advance, or the recursion guard never engages.
        use std::cell::Cell;
        let seen = Rc::new(Cell::new(u32::MAX));
        let seen2 = Rc::clone(&seen);
        let t = DelegateTool::new(
            Rc::new(OneShot("ok")),
            DelegationDepth(1),
            Rc::new(move |depth: DelegationDepth| {
                seen2.set(depth.0);
                ToolRegistry::new()
            }),
        );
        t.run(&json!({"task": "x"})).unwrap();
        assert_eq!(seen.get(), 2, "child depth must be parent + 1");
    }

    #[test]
    fn delegating_at_the_depth_cap_is_refused() {
        let t = DelegateTool::new(
            Rc::new(OneShot("ok")),
            DelegationDepth(MAX_DELEGATION_DEPTH),
            factory(),
        );
        let err = t.run(&json!({"task": "recurse"})).unwrap_err();
        assert!(err.to_string().contains("maximum delegation depth"));
    }

    #[test]
    fn an_unavailable_requested_tool_produces_an_actionable_error() {
        let t = tool();
        let err = t
            .run(&json!({"task": "x", "tools": ["nonexistent_tool"]}))
            .unwrap_err();
        assert!(err.to_string().contains("nonexistent_tool"));
    }
}
