//! The agent — the ReAct loop.
//!
//! ```text
//! loop (bounded by an IterationBudget):
//!     compress context if it is over the trigger
//!     resp = transport.complete(messages, tools)
//!     append assistant message (content + tool_calls)
//!     if resp.finish_reason == Stop: return content
//!     for each tool_call: result = registry.execute(...); append tool message
//! ```
//!
//! Everything configurable about a turn lives in [`TurnOptions`] rather than
//! in a growing positional parameter list — `run_turn_with_events` had reached
//! seven positional arguments, four of them `Option`, which is exactly the
//! shape where a caller silently passes the wrong `None`.

use crate::core::context::{Compressor, ContextCompressionConfig};
use crate::core::lifecycle::{AgentEvent, IterationBudget};
use crate::message::{Message, Role};
use crate::tools::ToolRegistry;
use crate::transport::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
use crate::util::{AgentError, Result};
use std::sync::atomic::{AtomicBool, Ordering};

/// Everything optional about running a turn.
#[derive(Default)]
pub struct TurnOptions<'a> {
    /// Called for every model reply and tool call/result — the hook that lets
    /// a caller see what the agent is doing turn by turn instead of only the
    /// final answer.
    pub on_event: Option<&'a mut dyn FnMut(AgentEvent)>,
    /// Polled between iterations and after every tool call. Set it (e.g. from
    /// a Ctrl-C handler) to unwind the turn early.
    pub interrupted: Option<&'a AtomicBool>,
    /// Context compression policy. `None` disables compression.
    pub compression: Option<&'a ContextCompressionConfig>,
    /// Stream assistant content as it arrives, when the transport supports it.
    pub stream: bool,
}

impl<'a> TurnOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_events(mut self, sink: &'a mut dyn FnMut(AgentEvent)) -> Self {
        self.on_event = Some(sink);
        self
    }

    #[must_use]
    pub fn with_interrupt(mut self, flag: &'a AtomicBool) -> Self {
        self.interrupted = Some(flag);
        self
    }

    #[must_use]
    pub fn with_compression(mut self, cfg: &'a ContextCompressionConfig) -> Self {
        self.compression = Some(cfg);
        self
    }

    #[must_use]
    pub fn streaming(mut self, on: bool) -> Self {
        self.stream = on;
        self
    }
}

/// The result of a completed turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The model's final answer.
    pub answer: String,
    /// LLM round-trips actually consumed. Worth surfacing for delegation: a
    /// subtask that used 3 of 25 is a very different signal from one that
    /// used all 25.
    pub iterations: u32,
    /// Whether `answer` was already emitted as
    /// [`AgentEvent::ContentFragment`]s while it was being produced.
    ///
    /// Without this a streaming caller prints the answer twice: once live as
    /// it arrives, once again when it renders the returned string.
    pub streamed: bool,
}

/// Run one conversation turn to completion and return the final answer.
pub fn run_turn(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
) -> Result<String> {
    run_turn_with_options(
        transport,
        tools,
        messages,
        max_iterations,
        TurnOptions::new(),
    )
    .map(|o| o.answer)
}

/// Run a turn with full control over events, interruption, and compression.
pub fn run_turn_with_options(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    mut options: TurnOptions<'_>,
) -> Result<TurnOutcome> {
    let specs = tools.specs();
    let mut budget = IterationBudget::new(max_iterations);
    let compressor = options
        .compression
        .map(|cfg| Compressor::for_transport(cfg, transport));

    let is_interrupted =
        || options.interrupted.is_some_and(|f| f.load(Ordering::SeqCst));
    // Only true streaming produces fragments; a transport falling back to the
    // one-shot default emits nothing incrementally, so the caller must still
    // render the returned answer itself.
    let streamed = options.stream && transport.supports_streaming();

    loop {
        if is_interrupted() {
            return Err(AgentError::Interrupted);
        }
        budget.consume()?;

        if let Some(compressor) = &compressor {
            if let Some(result) = compressor.compress_in_place_with_model(messages, transport) {
                let summary_text = result.summary.as_deref();
                emit(
                    &mut options.on_event,
                    AgentEvent::ContextCompressed {
                        before_tokens: result.outcome.before_tokens,
                        after_tokens: result.outcome.after_tokens,
                        dropped_messages: result.outcome.dropped_messages,
                        summary: summary_text,
                    },
                );
            }
        }

        let resp = complete_with_retry(
            transport,
            messages,
            &specs,
            options.stream,
            &mut options.on_event,
        )?;

        if is_interrupted() {
            return Err(AgentError::Interrupted);
        }

        messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
            tool_calls: resp.tool_calls.clone(),
            ..Default::default()
        });

        match resp.finish_reason {
            FinishReason::Stop => {
                // This IS the final answer — the caller renders it once as the
                // answer, so no intermediate content event fires here (that
                // would print the final answer twice).
                return Ok(TurnOutcome {
                    answer: resp.content,
                    iterations: budget.used(),
                    streamed,
                });
            }
            FinishReason::Length => {
                emit_content(&mut options.on_event, &resp, options.stream);
            }
            FinishReason::ToolCalls => {
                if resp.tool_calls.is_empty() {
                    // A provider claiming `tool_calls` with none attached would
                    // otherwise spin the loop until the budget ran out.
                    return Ok(TurnOutcome {
                        answer: resp.content,
                        iterations: budget.used(),
                        streamed,
                    });
                }
                emit_content(&mut options.on_event, &resp, options.stream);
                for (i, call) in resp.tool_calls.iter().enumerate() {
                    if is_interrupted() {
                        // The assistant message above carries `tool_calls`,
                        // and providers hard-reject it unless every call is
                        // answered by a tool message. An interrupt mid-loop
                        // would leave the remaining calls open, corrupting
                        // the next request — so close them out with a
                        // synthesized result before unwinding the turn.
                        for pending in &resp.tool_calls[i..] {
                            messages.push(Message::tool(
                                pending.id.clone(),
                                pending.name().to_string(),
                                "[interrupted — no result]",
                            ));
                        }
                        return Err(AgentError::Interrupted);
                    }
                    emit(
                        &mut options.on_event,
                        AgentEvent::ToolCallStart {
                            name: call.name(),
                            arguments: call.arguments(),
                        },
                    );
                    let started = std::time::Instant::now();
                    // A failing tool is data for the model, not a fatal turn
                    // error — it gets the message and can correct course.
                    let result = match tools.execute(call.name(), call.arguments()) {
                        Ok(out) => out,
                        Err(e) => format!("tool error: {e}"),
                    };
                    let elapsed = started.elapsed();
                    emit(
                        &mut options.on_event,
                        AgentEvent::ToolCallEnd {
                            name: call.name(),
                            result: &result,
                            elapsed,
                        },
                    );
                    messages.push(Message::tool(
                        call.id.clone(),
                        call.name().to_string(),
                        result,
                    ));
                }
            }
        }
    }
}

fn emit(sink: &mut Option<&mut dyn FnMut(AgentEvent)>, event: AgentEvent) {
    if let Some(cb) = sink.as_deref_mut() {
        cb(event);
    }
}

/// Emit assistant content, unless it was already streamed fragment-by-fragment
/// (which would print it twice).
fn emit_content(
    sink: &mut Option<&mut dyn FnMut(AgentEvent)>,
    resp: &ModelResponse,
    already_streamed: bool,
) {
    if already_streamed || resp.content.is_empty() {
        return;
    }
    emit(sink, AgentEvent::AssistantContent(&resp.content));
}

/// Call the transport, retrying up to two extra times within the same
/// iteration (not counted against the budget) if the provider returns a
/// malformed response. Transport errors propagate immediately — only a bad
/// response *shape* is treated as transiently recoverable, since retrying a
/// genuine auth or network failure just triples the latency before the same
/// error.
fn complete_with_retry(
    transport: &(dyn ProviderTransport + '_),
    messages: &[Message],
    specs: &[ToolSpec],
    stream: bool,
    sink: &mut Option<&mut dyn FnMut(AgentEvent)>,
) -> Result<ModelResponse> {
    const MAX_RETRIES: u32 = 2;
    let mut last_err = None;
    for _ in 0..=MAX_RETRIES {
        let attempt = if stream && transport.supports_streaming() {
            let mut on_fragment = |frag: &str| {
                if let Some(cb) = sink.as_deref_mut() {
                    cb(AgentEvent::ContentFragment(frag));
                }
            };
            transport.complete_streaming(messages, specs, "", &mut on_fragment)
        } else {
            transport.complete(messages, specs, "")
        };
        match attempt {
            Ok(resp) => return Ok(resp),
            Err(e @ AgentError::Response(_)) => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use crate::tools::builtins::register_builtins;
    use crate::tools::r#trait::Tool;
    use serde_json::{json, Value};
    use std::cell::{Cell, RefCell};

    /// Scripted transport: emits one tool call, then a final answer.
    struct StubTransport {
        rounds: Cell<u32>,
        infinite: bool,
        window: Option<u32>,
    }

    impl StubTransport {
        fn new() -> Self {
            Self {
                rounds: Cell::new(0),
                infinite: false,
                window: None,
            }
        }
        fn infinite() -> Self {
            Self {
                rounds: Cell::new(0),
                infinite: true,
                window: None,
            }
        }
    }

    impl ProviderTransport for StubTransport {
        fn name(&self) -> &str {
            "stub"
        }
        fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _model: &str,
        ) -> Result<ModelResponse> {
            let n = self.rounds.get();
            self.rounds.set(n + 1);
            let args = r#"{"command":"echo hello from tool"}"#.to_string();
            if self.infinite || n == 0 {
                Ok(ModelResponse {
                    content: "Running that for you.".to_string(),
                    tool_calls: vec![ToolCall::new("call_1", "bash", args)],
                    finish_reason: FinishReason::ToolCalls,
                })
            } else {
                Ok(ModelResponse {
                    content: "Done \u{2014} stub response after tool round.".to_string(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
        }
        fn context_window(&self) -> Option<u32> {
            self.window
        }
    }

    fn base_messages() -> Vec<Message> {
        vec![Message::user("please run a terminal command for me")]
    }

    fn builtin_registry() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        register_builtins(&mut tools);
        tools
    }

    #[test]
    fn loop_runs_terminal_tool_then_answers() {
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();

        let answer = run_turn(&transport, &tools, &mut messages, 8).unwrap();

        assert_eq!(messages.len(), 4, "expected user+assistant+tool+final");
        assert!(answer.contains("stub response"));
        let tool_msg = messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool message must exist");
        assert!(
            tool_msg.content.contains("hello from tool"),
            "got: {}",
            tool_msg.content
        );
    }

    #[test]
    fn budget_exhaustion_is_an_error() {
        let transport = StubTransport::infinite();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let res = run_turn(&transport, &tools, &mut messages, 2);
        assert!(matches!(res, Err(AgentError::BudgetExhausted { .. })));
    }

    #[test]
    fn reported_iterations_match_what_was_consumed() {
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let outcome =
            run_turn_with_options(&transport, &tools, &mut messages, 8, TurnOptions::new())
                .unwrap();
        assert_eq!(outcome.iterations, 2, "one tool round plus the answer");
    }

    #[test]
    fn unknown_tool_recovers_gracefully() {
        // A tool error must be fed back as data, not abort the turn — the
        // model can then pick a different tool.
        let transport = StubTransport::new();
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        let answer = run_turn(&transport, &tools, &mut messages, 8).unwrap();
        let tool_msg = messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content.contains("unknown tool"));
        assert!(answer.contains("stub response"));
    }

    #[test]
    fn pre_set_interrupt_flag_aborts_before_any_completion() {
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let flag = AtomicBool::new(true);

        let res = run_turn_with_options(
            &transport,
            &tools,
            &mut messages,
            8,
            TurnOptions::new().with_interrupt(&flag),
        );

        assert!(matches!(res, Err(AgentError::Interrupted)));
        assert_eq!(messages.len(), 1, "nothing should have been appended");
    }

    #[test]
    fn an_interrupt_mid_tool_loop_leaves_every_tool_call_answered() {
        // A Ctrl-C during the tool loop used to bail with the assistant's
        // tool_calls only half answered — the transcript sent on the next
        // turn was malformed (provider 400 / dropped context). The remaining
        // calls must be closed out with synthesized results.
        struct FlipTool {
            flag: std::sync::Arc<AtomicBool>,
        }
        impl Tool for FlipTool {
            fn name(&self) -> &str {
                "flip"
            }
            fn description(&self) -> &str {
                "test tool"
            }
            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }
            fn run(&self, _args: &Value) -> Result<String> {
                self.flag.store(true, Ordering::SeqCst);
                Ok("flipped".to_string())
            }
        }
        struct TwoCalls;
        impl ProviderTransport for TwoCalls {
            fn name(&self) -> &str {
                "two-calls"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                Ok(ModelResponse {
                    content: String::new(),
                    tool_calls: vec![
                        ToolCall::new("call_a", "flip", "{}"),
                        ToolCall::new("call_b", "never-runs", "{}"),
                    ],
                    finish_reason: FinishReason::ToolCalls,
                })
            }
        }
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(FlipTool {
            flag: std::sync::Arc::clone(&flag),
        }));
        let mut messages = base_messages();
        let res = run_turn_with_options(
            &TwoCalls,
            &tools,
            &mut messages,
            8,
            TurnOptions::new().with_interrupt(&flag),
        );
        assert!(matches!(res, Err(AgentError::Interrupted)));
        let assistant = messages
            .iter()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
            .expect("the assistant tool-call message must be in the transcript");
        let tool_msgs: Vec<&Message> = messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(
            tool_msgs.len(),
            assistant.tool_calls.len(),
            "every tool_call must have an answering tool message"
        );
        for (call, msg) in assistant.tool_calls.iter().zip(&tool_msgs) {
            assert_eq!(msg.tool_call_id.as_deref(), Some(call.id.as_str()));
        }
        assert!(
            tool_msgs[1].content.contains("interrupted"),
            "the never-run second call must carry a synthesized result"
        );
    }

    #[test]
    fn events_are_emitted_for_tool_calls() {
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let seen = RefCell::new(Vec::new());
        let mut sink = |e: AgentEvent| match e {
            AgentEvent::ToolCallStart { name, .. } => {
                seen.borrow_mut().push(format!("start:{name}"));
            }
            AgentEvent::ToolCallEnd { name, .. } => {
                seen.borrow_mut().push(format!("end:{name}"));
            }
            AgentEvent::AssistantContent(_) => seen.borrow_mut().push("content".into()),
            _ => {}
        };
        run_turn_with_options(
            &transport,
            &tools,
            &mut messages,
            8,
            TurnOptions::new().with_events(&mut sink),
        )
        .unwrap();
        let seen = seen.into_inner();
        assert!(seen.contains(&"start:bash".to_string()));
        assert!(seen.contains(&"end:bash".to_string()));
    }

    #[test]
    fn the_final_answer_is_not_also_emitted_as_an_intermediate_event() {
        // Regression: emitting content on Stop as well as returning it made
        // the CLI print every answer twice.
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let contents = RefCell::new(Vec::new());
        let mut sink = |e: AgentEvent| {
            if let AgentEvent::AssistantContent(c) = e {
                contents.borrow_mut().push(c.to_string());
            }
        };
        let outcome = run_turn_with_options(
            &transport,
            &tools,
            &mut messages,
            8,
            TurnOptions::new().with_events(&mut sink),
        )
        .unwrap();
        assert!(!contents.into_inner().contains(&outcome.answer));
    }

    #[test]
    fn a_bad_response_shape_is_retried_within_the_same_iteration() {
        struct FlakyThenGood {
            calls: Cell<u32>,
        }
        impl ProviderTransport for FlakyThenGood {
            fn name(&self) -> &str {
                "flaky"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                let n = self.calls.get();
                self.calls.set(n + 1);
                if n < 2 {
                    Err(AgentError::Response("garbage".into()))
                } else {
                    Ok(ModelResponse {
                        content: "recovered".into(),
                        tool_calls: vec![],
                        finish_reason: FinishReason::Stop,
                    })
                }
            }
        }
        let transport = FlakyThenGood { calls: Cell::new(0) };
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        // Budget of 1: the retries must NOT consume iterations, or this fails.
        let answer = run_turn(&transport, &tools, &mut messages, 1).unwrap();
        assert_eq!(answer, "recovered");
    }

    #[test]
    fn transport_errors_are_not_retried() {
        // Retrying a genuine auth/network failure just triples the wait before
        // the identical error.
        struct AlwaysDown {
            calls: Cell<u32>,
        }
        impl ProviderTransport for AlwaysDown {
            fn name(&self) -> &str {
                "down"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                self.calls.set(self.calls.get() + 1);
                Err(AgentError::Transport("connection refused".into()))
            }
        }
        let transport = AlwaysDown { calls: Cell::new(0) };
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        let res = run_turn(&transport, &tools, &mut messages, 4);
        assert!(matches!(res, Err(AgentError::Transport(_))));
        assert_eq!(transport.calls.get(), 1, "must not retry a transport error");
    }

    #[test]
    fn tool_calls_finish_reason_with_no_calls_terminates_instead_of_spinning() {
        struct EmptyToolCalls;
        impl ProviderTransport for EmptyToolCalls {
            fn name(&self) -> &str {
                "empty"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                Ok(ModelResponse {
                    content: "nothing to call".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::ToolCalls,
                })
            }
        }
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        let answer = run_turn(&EmptyToolCalls, &tools, &mut messages, 8).unwrap();
        assert_eq!(answer, "nothing to call");
    }

    #[test]
    fn compression_fires_on_a_small_window_and_emits_an_event() {
        // End-to-end proof the loop budgets against the transport's real
        // window instead of the old hardcoded 128k.
        struct Tiny {
            rounds: Cell<u32>,
        }
        impl ProviderTransport for Tiny {
            fn name(&self) -> &str {
                "tiny"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                self.rounds.set(self.rounds.get() + 1);
                Ok(ModelResponse {
                    content: "ok".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
            fn context_window(&self) -> Option<u32> {
                Some(2_000)
            }
        }
        let mut messages = vec![Message::system("sys")];
        for i in 0..80 {
            messages.push(Message::user(format!("{i} {}", vec!["word"; 40].join(" "))));
        }
        let cfg = ContextCompressionConfig::default();
        let compressed = Cell::new(false);
        let mut sink = |e: AgentEvent| {
            if matches!(e, AgentEvent::ContextCompressed { .. }) {
                compressed.set(true);
            }
        };
        run_turn_with_options(
            &Tiny { rounds: Cell::new(0) },
            &ToolRegistry::new(),
            &mut messages,
            4,
            TurnOptions::new()
                .with_events(&mut sink)
                .with_compression(&cfg),
        )
        .unwrap();
        assert!(compressed.get(), "compression should have fired");
    }

    #[test]
    fn streaming_falls_back_cleanly_on_a_non_streaming_transport() {
        let transport = StubTransport::new();
        let tools = builtin_registry();
        let mut messages = base_messages();
        let outcome = run_turn_with_options(
            &transport,
            &tools,
            &mut messages,
            8,
            TurnOptions::new().streaming(true),
        )
        .unwrap();
        assert!(outcome.answer.contains("stub response"));
    }

    #[test]
    fn streaming_transport_emits_content_fragments() {
        struct Streamer;
        impl ProviderTransport for Streamer {
            fn name(&self) -> &str {
                "streamer"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                Ok(ModelResponse {
                    content: "abc".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn complete_streaming(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
                on_fragment: &mut dyn FnMut(&str),
            ) -> Result<ModelResponse> {
                for piece in ["a", "b", "c"] {
                    on_fragment(piece);
                }
                Ok(ModelResponse {
                    content: "abc".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
        }
        let fragments = RefCell::new(String::new());
        let mut sink = |e: AgentEvent| {
            if let AgentEvent::ContentFragment(f) = e {
                fragments.borrow_mut().push_str(f);
            }
        };
        let mut messages = base_messages();
        run_turn_with_options(
            &Streamer,
            &ToolRegistry::new(),
            &mut messages,
            4,
            TurnOptions::new().with_events(&mut sink).streaming(true),
        )
        .unwrap();
        assert_eq!(fragments.into_inner(), "abc");
    }

    #[test]
    fn a_streamed_answer_is_flagged_so_the_caller_does_not_print_it_twice() {
        // Without this flag the CLI shows the answer once live and once
        // markdown-rendered underneath — the whole thing, duplicated.
        struct Streamer;
        impl ProviderTransport for Streamer {
            fn name(&self) -> &str {
                "streamer"
            }
            fn complete(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
            ) -> Result<ModelResponse> {
                Ok(ModelResponse {
                    content: "hi".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
            fn supports_streaming(&self) -> bool {
                true
            }
            fn complete_streaming(
                &self,
                _m: &[Message],
                _t: &[ToolSpec],
                _model: &str,
                on_fragment: &mut dyn FnMut(&str),
            ) -> Result<ModelResponse> {
                on_fragment("hi");
                Ok(ModelResponse {
                    content: "hi".into(),
                    tool_calls: vec![],
                    finish_reason: FinishReason::Stop,
                })
            }
        }
        let mut messages = base_messages();
        let outcome = run_turn_with_options(
            &Streamer,
            &ToolRegistry::new(),
            &mut messages,
            4,
            TurnOptions::new().streaming(true),
        )
        .unwrap();
        assert!(outcome.streamed);
    }

    #[test]
    fn a_non_streaming_transport_is_not_flagged_as_streamed_even_when_asked() {
        // The default `complete_streaming` emits one whole-answer fragment,
        // but nothing was shown incrementally — the caller must still render.
        let mut messages = base_messages();
        let outcome = run_turn_with_options(
            &StubTransport::new(),
            &builtin_registry(),
            &mut messages,
            8,
            TurnOptions::new().streaming(true),
        )
        .unwrap();
        assert!(
            !outcome.streamed,
            "a fallback transport must not claim to have streamed"
        );
    }

    #[test]
    fn a_non_streaming_run_is_never_flagged_as_streamed() {
        let mut messages = base_messages();
        let outcome = run_turn_with_options(
            &StubTransport::new(),
            &builtin_registry(),
            &mut messages,
            8,
            TurnOptions::new(),
        )
        .unwrap();
        assert!(!outcome.streamed);
    }
}
