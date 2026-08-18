//! Sub-agent delegation — running a bounded ReAct loop *as a tool call*.
//!
//! # Why in-process, and why a budget
//!
//! The old `delegate` tool shelled out to a fresh `grace --prompt ...`
//! subprocess. That worked but paid for a process spawn, a second config
//! load, a second SQLite open, and a full re-onboarding per subtask — and it
//! could only ever return whatever it managed to scrape out of the child's
//! stdout, so a child that failed halfway produced a confusing partial answer
//! with no way to distinguish it from success.
//!
//! [`Delegation`] instead runs a *fresh conversation* against the same
//! in-process transport. The sub-agent gets:
//!
//! - **A clean message history.** The whole point of delegating is that the
//!   subtask does not inherit (or pollute) the parent's context. This is what
//!   makes delegation a context-management tool and not just a function call.
//! - **Its own iteration budget.** A sub-agent gets an explicit bounded
//!   budget, so one pathological branch cannot consume the parent's remaining
//!   rounds. A runaway subtask fails *its own* budget and reports back; the
//!   parent keeps every iteration it had left and can react.
//! - **A narrowable tool set.** A sub-agent asked to summarize files has no
//!   business holding `bash`.
//!
//! # No unbounded recursion
//!
//! A sub-agent that can itself delegate is an unbounded recursion generator.
//! [`DelegationDepth`] caps nesting, and the sub-registry is built without the
//! delegate tool once the cap is reached.

use crate::core::agent::{run_turn_with_options, TurnOptions};
use crate::core::context::ContextCompressionConfig;
use crate::core::lifecycle::AgentEvent;
use crate::message::Message;
use crate::tools::ToolRegistry;
use crate::transport::ProviderTransport;
use crate::util::{AgentError, Result};

/// Default iteration budget for a sub-agent when the caller does not say.
///
/// 25 is chosen to be generous enough for a real multi-step subtask
/// (read, reason, act, verify) while still being an order of magnitude below
/// a top-level turn's default, so a delegated branch cannot quietly become
/// the dominant cost of a session.
pub const DEFAULT_DELEGATION_BUDGET: u32 = 25;

/// Hard ceiling on a sub-agent's budget, regardless of what the model asks
/// for. A model that emits `iterations: 100000` gets clamped rather than
/// obeyed.
pub const MAX_DELEGATION_BUDGET: u32 = 200;

/// How deep sub-agents may nest before delegation is withdrawn.
pub const MAX_DELEGATION_DEPTH: u32 = 3;

/// Current nesting depth of a delegation chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DelegationDepth(pub u32);

impl DelegationDepth {
    /// The top-level agent.
    pub const ROOT: Self = Self(0);

    /// The depth one level below this one.
    pub fn child(self) -> Self {
        Self(self.0 + 1)
    }

    /// Whether an agent at this depth may still delegate further.
    pub fn may_delegate(self) -> bool {
        self.0 < MAX_DELEGATION_DEPTH
    }
}

/// A sub-task handed to a sub-agent.
#[derive(Debug, Clone)]
pub struct SubTask {
    /// What the sub-agent should accomplish.
    pub task: String,
    /// Extra context the parent wants to pass down explicitly. Delegation
    /// deliberately does *not* copy the parent's history, so anything the
    /// subtask needs must be stated here.
    pub context: Option<String>,
    /// Iteration ceiling for this sub-agent.
    pub budget: u32,
    /// Tool names the sub-agent may use. Empty = inherit the parent's set.
    pub allowed_tools: Vec<String>,
}

impl SubTask {
    /// A sub-task with the default budget and the parent's full tool set.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            context: None,
            budget: DEFAULT_DELEGATION_BUDGET,
            allowed_tools: Vec::new(),
        }
    }

    /// Set the iteration budget, clamped to `1..=MAX_DELEGATION_BUDGET`.
    #[must_use]
    pub fn with_budget(mut self, budget: u32) -> Self {
        self.budget = budget.clamp(1, MAX_DELEGATION_BUDGET);
        self
    }

    /// Restrict the sub-agent to a named subset of tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = tools;
        self
    }

    /// Attach explicit context for the sub-agent.
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// The system prompt handed to the sub-agent.
    ///
    /// Deliberately terse and directive: a sub-agent exists to return one
    /// self-contained answer to its parent, not to hold a conversation. It is
    /// told its budget explicitly so it can pace itself rather than
    /// discovering the ceiling by hitting it.
    pub fn system_prompt(&self) -> String {
        let mut prompt = format!(
            "You are a focused sub-agent working one delegated task to completion.\n\
             \n\
             Rules:\n\
             - You have at most {} tool-using iterations. Work efficiently and \
             do not explore beyond the task.\n\
             - You have no access to the parent conversation. Everything you \
             need is stated below.\n\
             - Your final message is returned verbatim to the parent agent. \
             Make it a complete, self-contained answer — state findings and \
             conclusions, not a description of what you did.\n\
             - If you cannot complete the task, say so plainly and explain \
             what blocked you.\n\
             \n\
             Task:\n{}",
            self.budget, self.task
        );
        if let Some(context) = &self.context {
            prompt.push_str("\n\nContext from the parent agent:\n");
            prompt.push_str(context);
        }
        prompt
    }
}

/// What a sub-agent produced.
#[derive(Debug, Clone)]
pub struct SubAgentReport {
    /// The sub-agent's final answer, returned to the parent as the tool result.
    pub answer: String,
    /// Iterations actually consumed.
    pub iterations: u32,
    /// Its ceiling, for context when reporting exhaustion.
    pub budget: u32,
    /// Whether it finished on its own terms rather than being cut off.
    pub completed: bool,
}

impl SubAgentReport {
    /// Render for the parent's tool result.
    ///
    /// Budget exhaustion is reported as a *result*, not an error: the parent
    /// can often still use partial findings, and it needs to know the branch
    /// was truncated so it does not treat a cut-off answer as complete.
    pub fn to_tool_result(&self) -> String {
        if self.completed {
            self.answer.clone()
        } else {
            format!(
                "[sub-agent stopped after exhausting its {} iteration budget — \
                 the result below is incomplete]\n\n{}",
                self.budget, self.answer
            )
        }
    }
}

/// Runs sub-agents. Borrows the parent's transport so a delegated task hits
/// the same provider, model, and credentials without re-resolving any of them.
pub struct Delegation<'a> {
    transport: &'a (dyn ProviderTransport + 'a),
    depth: DelegationDepth,
    compression: Option<ContextCompressionConfig>,
}

impl<'a> Delegation<'a> {
    /// A delegator rooted at the top-level agent.
    pub fn new(transport: &'a (dyn ProviderTransport + 'a)) -> Self {
        Self {
            transport,
            depth: DelegationDepth::ROOT,
            compression: None,
        }
    }

    /// A delegator at an explicit nesting depth.
    #[must_use]
    pub fn at_depth(mut self, depth: DelegationDepth) -> Self {
        self.depth = depth;
        self
    }

    /// Apply a compression policy to sub-agent conversations too. A sub-agent
    /// reading large files can overflow its context just as easily as its
    /// parent.
    #[must_use]
    pub fn with_compression(mut self, cfg: ContextCompressionConfig) -> Self {
        self.compression = Some(cfg);
        self
    }

    /// This delegator's nesting depth.
    pub fn depth(&self) -> DelegationDepth {
        self.depth
    }

    /// Run `task` to completion against `tools`.
    ///
    /// Budget exhaustion is folded into a [`SubAgentReport`] with
    /// `completed: false` rather than propagated: a truncated sub-answer is
    /// usually still useful to the parent, and turning it into a hard error
    /// would throw away work the parent already paid for. Every other error
    /// (transport down, interrupted) propagates — those are not partial
    /// results, they are failures.
    pub fn run(
        &self,
        task: &SubTask,
        tools: &ToolRegistry,
        mut on_event: Option<&mut dyn FnMut(AgentEvent)>,
    ) -> Result<SubAgentReport> {
        if !self.depth.may_delegate() {
            return Err(AgentError::Delegation {
                task: task.task.clone(),
                reason: format!(
                    "maximum delegation depth ({MAX_DELEGATION_DEPTH}) reached; \
                     complete this work directly instead of delegating further"
                ),
            });
        }

        let missing = tools.missing(&task.allowed_tools);
        if !missing.is_empty() {
            return Err(AgentError::Delegation {
                task: task.task.clone(),
                reason: format!(
                    "requested tools not available: {}. Available: {}",
                    missing.join(", "),
                    tools.names().join(", ")
                ),
            });
        }

        if let Some(cb) = &mut on_event {
            cb(AgentEvent::DelegationStart {
                task: &task.task,
                budget: task.budget,
            });
        }

        // A fresh history — this is the whole point of delegating.
        let mut messages = vec![
            Message::system(task.system_prompt()),
            Message::user(task.task.clone()),
        ];

        let mut options = TurnOptions::new();
        if let Some(cfg) = &self.compression {
            options = options.with_compression(cfg);
        }

        let result = run_turn_with_options(
            self.transport,
            tools,
            &mut messages,
            task.budget,
            options,
        );

        let report = match result {
            Ok(outcome) => SubAgentReport {
                answer: outcome.answer,
                iterations: outcome.iterations,
                budget: task.budget,
                completed: true,
            },
            Err(AgentError::BudgetExhausted { iterations }) => SubAgentReport {
                // Salvage the last thing it said; an empty string is still
                // more honest than pretending the branch never ran.
                answer: last_assistant_text(&messages),
                iterations,
                budget: task.budget,
                completed: false,
            },
            Err(e) => {
                if let Some(cb) = on_event.as_deref_mut() {
                    cb(AgentEvent::DelegationEnd {
                        task: &task.task,
                        iterations: 0,
                        ok: false,
                    });
                }
                return Err(AgentError::Delegation {
                    task: task.task.clone(),
                    reason: e.to_string(),
                });
            }
        };

        if let Some(cb) = &mut on_event {
            cb(AgentEvent::DelegationEnd {
                task: &task.task,
                iterations: report.iterations,
                ok: report.completed,
            });
        }
        Ok(report)
    }
}

/// The last non-empty assistant message, used to salvage partial work from a
/// budget-exhausted sub-agent.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == crate::message::Role::Assistant && !m.content.trim().is_empty())
        .map(|m| m.content.clone())
        .unwrap_or_else(|| "(sub-agent produced no answer before its budget ran out)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use crate::tools::builtins::register_builtins;
    use crate::transport::{FinishReason, ModelResponse, ToolSpec};
    use std::cell::{Cell, RefCell};

    /// Answers immediately with a fixed string.
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
                usage: None,
            })
        }
    }

    /// Never stops asking for tool calls — exercises budget exhaustion.
    struct NeverStops;
    impl ProviderTransport for NeverStops {
        fn name(&self) -> &str {
            "neverstops"
        }
        fn complete(
            &self,
            _m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: "still working".into(),
                tool_calls: vec![ToolCall::new(
                    "c1",
                    "bash",
                    r#"{"command":"echo x"}"#,
                )],
                finish_reason: FinishReason::ToolCalls,
                usage: None,
            })
        }
    }

    /// Records the messages it was sent, so tests can assert on isolation.
    struct Recording {
        seen: RefCell<Vec<Vec<Message>>>,
    }
    impl ProviderTransport for Recording {
        fn name(&self) -> &str {
            "recording"
        }
        fn complete(
            &self,
            m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            self.seen.borrow_mut().push(m.to_vec());
            Ok(ModelResponse {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }
    }

    fn builtin_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        reg
    }

    #[test]
    fn a_sub_agent_returns_its_final_answer() {
        let transport = OneShot("the subtask is complete: 42");
        let d = Delegation::new(&transport);
        let report = d
            .run(&SubTask::new("compute the answer"), &ToolRegistry::new(), None)
            .unwrap();
        assert_eq!(report.answer, "the subtask is complete: 42");
        assert!(report.completed);
        assert_eq!(report.iterations, 1);
    }

    #[test]
    fn the_default_budget_is_twenty_five() {
        assert_eq!(DEFAULT_DELEGATION_BUDGET, 25);
        assert_eq!(SubTask::new("x").budget, 25);
    }

    #[test]
    fn budget_is_clamped_to_a_sane_range() {
        // A model emitting `iterations: 100000` must be clamped, not obeyed.
        assert_eq!(
            SubTask::new("x").with_budget(1_000_000).budget,
            MAX_DELEGATION_BUDGET
        );
        assert_eq!(SubTask::new("x").with_budget(0).budget, 1);
        assert_eq!(SubTask::new("x").with_budget(7).budget, 7);
    }

    #[test]
    fn budget_exhaustion_returns_an_incomplete_report_not_an_error() {
        // A truncated sub-answer is usually still useful; erroring would
        // discard work the parent already paid for.
        let d = Delegation::new(&NeverStops);
        let report = d
            .run(
                &SubTask::new("loop forever").with_budget(3),
                &builtin_registry(),
                None,
            )
            .unwrap();
        assert!(!report.completed);
        assert_eq!(report.budget, 3);
        assert!(report.to_tool_result().contains("incomplete"));
        assert!(report.to_tool_result().contains("still working"));
    }

    #[test]
    fn a_sub_agents_budget_is_independent_of_its_parents() {
        // The core reason delegation has its own budget: a runaway subtask
        // must not consume the parent's remaining rounds.
        let d = Delegation::new(&NeverStops);
        let report = d
            .run(
                &SubTask::new("runaway").with_budget(2),
                &builtin_registry(),
                None,
            )
            .unwrap();
        assert_eq!(report.iterations, 2, "spent exactly its own budget");
    }

    #[test]
    fn the_sub_agent_does_not_inherit_the_parents_history() {
        // Context isolation is the point of delegating; leaking parent
        // history would make it just an expensive function call.
        let transport = Recording {
            seen: RefCell::new(Vec::new()),
        };
        let d = Delegation::new(&transport);
        d.run(&SubTask::new("summarize"), &ToolRegistry::new(), None)
            .unwrap();
        let seen = transport.seen.borrow();
        let first = &seen[0];
        assert_eq!(first.len(), 2, "system + the task, nothing else");
        assert!(first.iter().all(|m| !m.content.contains("parent secret")));
    }

    #[test]
    fn explicit_context_is_passed_down_in_the_system_prompt() {
        let transport = Recording {
            seen: RefCell::new(Vec::new()),
        };
        let d = Delegation::new(&transport);
        d.run(
            &SubTask::new("analyze").with_context("the build uses cargo"),
            &ToolRegistry::new(),
            None,
        )
        .unwrap();
        let seen = transport.seen.borrow();
        assert!(seen[0][0].content.contains("the build uses cargo"));
    }

    #[test]
    fn the_system_prompt_states_the_budget_so_the_agent_can_pace_itself() {
        let prompt = SubTask::new("x").with_budget(9).system_prompt();
        assert!(prompt.contains('9'));
        assert!(prompt.contains("no access to the parent conversation"));
    }

    #[test]
    fn requesting_an_unavailable_tool_is_an_explicit_error() {
        // Silently running with fewer tools than the model believes it has
        // produces a baffling failure several iterations later.
        let transport = OneShot("ok");
        let d = Delegation::new(&transport);
        let err = d
            .run(
                &SubTask::new("x").with_tools(vec!["teleport".into()]),
                &builtin_registry(),
                None,
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("teleport"), "got: {msg}");
        assert!(msg.contains("Available:"), "got: {msg}");
    }

    #[test]
    fn requesting_available_tools_succeeds() {
        let transport = OneShot("ok");
        let d = Delegation::new(&transport);
        let report = d
            .run(
                &SubTask::new("x").with_tools(vec!["read".into()]),
                &builtin_registry(),
                None,
            )
            .unwrap();
        assert!(report.completed);
    }

    #[test]
    fn depth_increments_and_caps() {
        let mut d = DelegationDepth::ROOT;
        assert!(d.may_delegate());
        for _ in 0..MAX_DELEGATION_DEPTH {
            d = d.child();
        }
        assert!(!d.may_delegate(), "must stop nesting at the cap");
    }

    #[test]
    fn delegating_past_the_depth_cap_is_refused() {
        // A sub-agent that can delegate is an unbounded recursion generator.
        let transport = OneShot("ok");
        let d = Delegation::new(&transport).at_depth(DelegationDepth(MAX_DELEGATION_DEPTH));
        let err = d
            .run(&SubTask::new("recurse"), &ToolRegistry::new(), None)
            .unwrap_err();
        assert!(err.to_string().contains("maximum delegation depth"));
    }

    #[test]
    fn transport_failures_propagate_as_delegation_errors_naming_the_task() {
        struct Broken;
        impl ProviderTransport for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                Err(AgentError::Transport("connection refused".into()))
            }
        }
        let d = Delegation::new(&Broken);
        let err = d
            .run(&SubTask::new("audit deps"), &ToolRegistry::new(), None)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("audit deps"), "failure must name its branch: {msg}");
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn lifecycle_events_bracket_the_sub_agent_run() {
        let transport = OneShot("done");
        let d = Delegation::new(&transport);
        let started = Cell::new(false);
        let ended = Cell::new(false);
        let mut sink = |e: AgentEvent| match e {
            AgentEvent::DelegationStart { budget, .. } => {
                assert_eq!(budget, 5);
                started.set(true);
            }
            AgentEvent::DelegationEnd { ok, iterations, .. } => {
                assert!(ok);
                assert_eq!(iterations, 1);
                ended.set(true);
            }
            _ => {}
        };
        d.run(
            &SubTask::new("x").with_budget(5),
            &ToolRegistry::new(),
            Some(&mut sink),
        )
        .unwrap();
        assert!(started.get() && ended.get());
    }

    #[test]
    fn a_completed_report_renders_as_the_bare_answer() {
        let r = SubAgentReport {
            answer: "the answer".into(),
            iterations: 2,
            budget: 25,
            completed: true,
        };
        assert_eq!(r.to_tool_result(), "the answer");
    }

    #[test]
    fn an_exhausted_sub_agent_with_no_output_still_reports_something_honest() {
        let msgs = vec![Message::system("sys"), Message::user("task")];
        let text = last_assistant_text(&msgs);
        assert!(text.contains("no answer"));
    }
}
