//! Integration tests for the agent engine: the ReAct loop, context
//! compression, and sub-agent delegation, exercised through the public API
//! only — no `#[cfg(test)]` internals, no network.
//!
//! These cover the *seams between* modules. Per-function behaviour is unit
//! tested inside each module; what is asserted here is that the loop, the
//! compressor, the budget, and delegation compose correctly.

use grace::core::{
    run_turn, run_turn_with_options, AgentEvent, ContextCompressionConfig, Delegation,
    DelegationDepth, SubTask, TurnOptions, MAX_DELEGATION_DEPTH,
};
use grace::message::{Message, Role, ToolCall};
use grace::tools::{register_builtins, ToolRegistry};
use grace::transport::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
use grace::util::{AgentError, Result};
use std::cell::{Cell, RefCell};

// ---- test doubles -----------------------------------------------------------

/// Replays a fixed script of responses, one per call, then repeats the last.
struct ScriptedTransport {
    script: Vec<ModelResponse>,
    calls: Cell<usize>,
    window: Option<u32>,
}

impl ScriptedTransport {
    fn new(script: Vec<ModelResponse>) -> Self {
        Self {
            script,
            calls: Cell::new(0),
            window: None,
        }
    }

    fn with_window(mut self, window: u32) -> Self {
        self.window = Some(window);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.get()
    }
}

impl ProviderTransport for ScriptedTransport {
    fn name(&self) -> &str {
        "scripted"
    }

    fn complete(&self, _m: &[Message], _t: &[ToolSpec], _model: &str) -> Result<ModelResponse> {
        let n = self.calls.get();
        self.calls.set(n + 1);
        let idx = n.min(self.script.len() - 1);
        Ok(self.script[idx].clone())
    }

    fn context_window(&self) -> Option<u32> {
        self.window
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        content: text.to_string(),
        tool_calls: vec![],
        finish_reason: FinishReason::Stop,
    }
}

fn call_tool(id: &str, name: &str, args: &str) -> ModelResponse {
    ModelResponse {
        content: String::new(),
        tool_calls: vec![ToolCall::new(id, name, args)],
        finish_reason: FinishReason::ToolCalls,
    }
}

fn builtin_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_builtins(&mut reg);
    reg
}

fn filler(tokens: usize) -> String {
    vec!["word"; tokens].join(" ")
}

// ---- the ReAct loop ---------------------------------------------------------

#[test]
fn a_single_turn_with_no_tools_returns_the_models_answer() {
    let transport = ScriptedTransport::new(vec![answer("hello, Sir")]);
    let mut messages = vec![Message::user("hi")];
    let out = run_turn(&transport, &ToolRegistry::new(), &mut messages, 8).unwrap();
    assert_eq!(out, "hello, Sir");
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn a_tool_call_round_trip_appends_assistant_and_tool_messages_in_order() {
    // The message sequence is the contract with the provider — a tool result
    // that does not directly follow its request is rejected outright.
    let transport = ScriptedTransport::new(vec![
        call_tool("c1", "bash", r#"{"command":"echo integration"}"#),
        answer("the command printed 'integration'"),
    ]);
    let mut messages = vec![Message::user("run echo")];
    let out = run_turn(&transport, &builtin_registry(), &mut messages, 8).unwrap();

    assert_eq!(out, "the command printed 'integration'");
    let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert!(messages[2].content.contains("integration"));
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
}

#[test]
fn several_tool_calls_in_one_response_all_execute() {
    let transport = ScriptedTransport::new(vec![
        ModelResponse {
            content: String::new(),
            tool_calls: vec![
                ToolCall::new("a", "bash", r#"{"command":"echo one"}"#),
                ToolCall::new("b", "bash", r#"{"command":"echo two"}"#),
            ],
            finish_reason: FinishReason::ToolCalls,
        },
        answer("both ran"),
    ]);
    let mut messages = vec![Message::user("run both")];
    run_turn(&transport, &builtin_registry(), &mut messages, 8).unwrap();

    let tool_msgs: Vec<&Message> = messages.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(tool_msgs.len(), 2);
    assert!(tool_msgs[0].content.contains("one"));
    assert!(tool_msgs[1].content.contains("two"));
}

#[test]
fn a_failing_tool_is_reported_to_the_model_rather_than_aborting_the_turn() {
    // The model must get the error text so it can correct course; a hard
    // abort throws away everything the turn has done so far.
    let transport = ScriptedTransport::new(vec![
        call_tool("c1", "read", r#"{"path":"/definitely/not/here"}"#),
        answer("that file does not exist"),
    ]);
    let mut messages = vec![Message::user("read it")];
    let out = run_turn(&transport, &builtin_registry(), &mut messages, 8).unwrap();

    let tool_msg = messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(tool_msg.content.contains("tool error"));
    assert_eq!(out, "that file does not exist");
}

#[test]
fn the_iteration_budget_bounds_a_model_that_never_stops() {
    let transport = ScriptedTransport::new(vec![call_tool(
        "c",
        "bash",
        r#"{"command":"echo loop"}"#,
    )]);
    let mut messages = vec![Message::user("go")];
    let err = run_turn(&transport, &builtin_registry(), &mut messages, 3).unwrap_err();
    assert!(matches!(
        err,
        AgentError::BudgetExhausted { iterations: 4 } | AgentError::BudgetExhausted { .. }
    ));
    assert!(
        transport.call_count() <= 4,
        "budget must bound provider calls, got {}",
        transport.call_count()
    );
}

#[test]
fn an_interrupt_unwinds_the_turn_and_preserves_completed_work() {
    // Ctrl-C mid-turn is not a failure — tool calls that already ran stay in
    // the history so the next turn can build on them.
    let transport = ScriptedTransport::new(vec![call_tool(
        "c",
        "bash",
        r#"{"command":"echo x"}"#,
    )]);
    let interrupted = std::sync::atomic::AtomicBool::new(false);
    let flag_ref = &interrupted;
    let mut sink = |e: AgentEvent<'_>| {
        if matches!(e, AgentEvent::ToolCallEnd { .. }) {
            flag_ref.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    };
    let mut messages = vec![Message::user("go")];
    let err = run_turn_with_options(
        &transport,
        &builtin_registry(),
        &mut messages,
        16,
        TurnOptions::new()
            .with_events(&mut sink)
            .with_interrupt(&interrupted),
    )
    .unwrap_err();

    assert!(matches!(err, AgentError::Interrupted));
    assert!(
        messages.iter().any(|m| m.role == Role::Tool),
        "work completed before the interrupt must be kept"
    );
}

// ---- context compression ----------------------------------------------------

#[test]
fn compression_keeps_a_long_conversation_inside_a_small_window() {
    // The end-to-end version of the bug this replaced: an 8k model budgeted
    // against a hardcoded 128k never compressed, so the request simply failed.
    let transport = ScriptedTransport::new(vec![answer("ok")]).with_window(4_000);
    let mut messages = vec![Message::system("you are grace")];
    for i in 0..120 {
        messages.push(Message::user(format!("q{i} {}", filler(40))));
        messages.push(Message::assistant(format!("a{i} {}", filler(40))));
    }
    let before = messages.len();
    let cfg = ContextCompressionConfig::default();

    run_turn_with_options(
        &transport,
        &ToolRegistry::new(),
        &mut messages,
        4,
        TurnOptions::new().with_compression(&cfg),
    )
    .unwrap();

    assert!(
        messages.len() < before,
        "history should have been compressed: {before} -> {}",
        messages.len()
    );
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(messages[0].content, "you are grace");
}

#[test]
fn compression_does_not_fire_when_the_window_is_ample() {
    let transport = ScriptedTransport::new(vec![answer("ok")]).with_window(200_000);
    let mut messages = vec![Message::system("sys")];
    for i in 0..20 {
        messages.push(Message::user(format!("q{i}")));
    }
    let before = messages.len();
    let cfg = ContextCompressionConfig::default();
    run_turn_with_options(
        &transport,
        &ToolRegistry::new(),
        &mut messages,
        4,
        TurnOptions::new().with_compression(&cfg),
    )
    .unwrap();
    // +1 for the assistant reply appended by the turn.
    assert_eq!(messages.len(), before + 1);
}

#[test]
fn a_compression_event_is_emitted_so_the_user_learns_history_was_dropped() {
    let transport = ScriptedTransport::new(vec![answer("ok")]).with_window(2_000);
    let mut messages = vec![Message::system("sys")];
    for i in 0..100 {
        messages.push(Message::user(format!("{i} {}", filler(40))));
    }
    let observed = RefCell::new(None);
    let mut sink = |e: AgentEvent<'_>| {
        if let AgentEvent::ContextCompressed {
            before_tokens,
            after_tokens,
            dropped_messages,
        } = e
        {
            *observed.borrow_mut() = Some((before_tokens, after_tokens, dropped_messages));
        }
    };
    let cfg = ContextCompressionConfig::default();
    run_turn_with_options(
        &transport,
        &ToolRegistry::new(),
        &mut messages,
        4,
        TurnOptions::new().with_events(&mut sink).with_compression(&cfg),
    )
    .unwrap();

    let (before, after, dropped) = observed.into_inner().expect("a compression event");
    assert!(after < before, "compression must reduce: {before} -> {after}");
    assert!(dropped > 0);
}

// ---- delegation -------------------------------------------------------------

#[test]
fn a_sub_agent_runs_to_completion_and_reports_its_answer() {
    let transport = ScriptedTransport::new(vec![answer("the subtask found 3 matches")]);
    let report = Delegation::new(&transport)
        .run(&SubTask::new("count matches"), &ToolRegistry::new(), None)
        .unwrap();
    assert!(report.completed);
    assert_eq!(report.answer, "the subtask found 3 matches");
    assert_eq!(report.to_tool_result(), "the subtask found 3 matches");
}

#[test]
fn a_sub_agent_can_use_tools_within_its_budget() {
    let transport = ScriptedTransport::new(vec![
        call_tool("c1", "bash", r#"{"command":"echo delegated"}"#),
        answer("the sub-agent ran the command"),
    ]);
    let report = Delegation::new(&transport)
        .run(
            &SubTask::new("run echo").with_budget(5),
            &builtin_registry(),
            None,
        )
        .unwrap();
    assert!(report.completed);
    assert_eq!(report.iterations, 2);
}

#[test]
fn a_runaway_sub_agent_spends_only_its_own_budget() {
    // The central guarantee, modelled on PowerPro's insert_obs: one
    // pathological branch cannot consume the parent's remaining rounds.
    let transport = ScriptedTransport::new(vec![call_tool(
        "c",
        "bash",
        r#"{"command":"echo spin"}"#,
    )]);
    let report = Delegation::new(&transport)
        .run(
            &SubTask::new("spin forever").with_budget(4),
            &builtin_registry(),
            None,
        )
        .unwrap();

    assert!(!report.completed, "should report truncation, not success");
    assert_eq!(report.iterations, 4);
    assert!(report.to_tool_result().contains("incomplete"));
    assert!(
        transport.call_count() <= 5,
        "the sub-agent must not exceed its budget, made {} calls",
        transport.call_count()
    );
}

#[test]
fn the_parent_keeps_its_full_budget_after_a_sub_agent_exhausts_its_own() {
    // A parent with 10 iterations that delegates a 2-iteration subtask must
    // still have room to react to the truncated result.
    let transport = ScriptedTransport::new(vec![call_tool(
        "c",
        "bash",
        r#"{"command":"echo x"}"#,
    )]);
    let delegation = Delegation::new(&transport);

    let first = delegation
        .run(
            &SubTask::new("a").with_budget(2),
            &builtin_registry(),
            None,
        )
        .unwrap();
    let second = delegation
        .run(
            &SubTask::new("b").with_budget(2),
            &builtin_registry(),
            None,
        )
        .unwrap();

    assert_eq!(first.iterations, 2);
    assert_eq!(
        second.iterations, 2,
        "each delegation gets a fresh budget, not a shared pool"
    );
}

#[test]
fn a_sub_agent_cannot_see_the_parents_conversation() {
    // Context isolation is the whole point; leaking parent history would make
    // delegation an expensive no-op.
    struct Spy {
        seen: RefCell<Vec<Message>>,
    }
    impl ProviderTransport for Spy {
        fn name(&self) -> &str {
            "spy"
        }
        fn complete(
            &self,
            m: &[Message],
            _t: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            self.seen.borrow_mut().extend_from_slice(m);
            Ok(answer("done"))
        }
    }
    let spy = Spy {
        seen: RefCell::new(Vec::new()),
    };
    Delegation::new(&spy)
        .run(
            &SubTask::new("do the subtask"),
            &ToolRegistry::new(),
            None,
        )
        .unwrap();

    let seen = spy.seen.into_inner();
    assert_eq!(seen.len(), 2, "only a system prompt and the task");
    assert!(seen.iter().all(|m| !m.content.contains("PARENT_SECRET")));
}

#[test]
fn delegation_nesting_terminates_at_the_depth_cap() {
    let transport = ScriptedTransport::new(vec![answer("ok")]);
    let at_cap =
        Delegation::new(&transport).at_depth(DelegationDepth(MAX_DELEGATION_DEPTH));
    let err = at_cap
        .run(&SubTask::new("recurse"), &ToolRegistry::new(), None)
        .unwrap_err();
    assert!(err.to_string().contains("maximum delegation depth"));
}

#[test]
fn delegation_events_report_the_budget_and_what_was_actually_spent() {
    // A subtask that finishes in 2 of 25 is a very different signal from one
    // that consumes all 25, so both numbers are surfaced.
    let transport = ScriptedTransport::new(vec![
        call_tool("c1", "bash", r#"{"command":"echo x"}"#),
        answer("done"),
    ]);
    let recorded = RefCell::new(Vec::new());
    let mut sink = |e: AgentEvent<'_>| match e {
        AgentEvent::DelegationStart { task, budget } => {
            recorded.borrow_mut().push(format!("start {task} {budget}"));
        }
        AgentEvent::DelegationEnd { iterations, ok, .. } => {
            recorded.borrow_mut().push(format!("end {iterations} {ok}"));
        }
        _ => {}
    };
    Delegation::new(&transport)
        .run(
            &SubTask::new("subtask").with_budget(25),
            &builtin_registry(),
            Some(&mut sink),
        )
        .unwrap();

    let recorded = recorded.into_inner();
    assert_eq!(recorded, vec!["start subtask 25", "end 2 true"]);
}
