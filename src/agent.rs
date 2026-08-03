//! The agent — the ReAct loop.
//!
//! ```text
//! loop (bounded by max_iterations):
//!     resp = transport.complete(messages, tools, model)
//!     append assistant message (content + tool_calls)
//!     if resp.finish_reason == Stop: break
//!     for each tool_call in resp.tool_calls:
//!         result = registry.execute(name, arguments)
//!         append tool message (result)
//! return final assistant content
//! ```

use crate::error::{AgentError, Result};
use crate::message::{Message, Role};
use crate::tool::ToolRegistry;
use crate::transport::{FinishReason, ProviderTransport};
use crate::config::ContextCompressionConfig;

/// Calls `transport.complete`, retrying up to 2 additional times within the
/// same iteration (not counted against `max_iterations`) if the provider
/// returns a malformed/garbage response (`AgentError::Response`). Transport
/// errors and other error kinds propagate immediately — only a bad response
/// shape is treated as transiently recoverable.
fn complete_with_response_retry(
    transport: &(dyn ProviderTransport + '_),
    messages: &[Message],
    specs: &[crate::transport::ToolSpec],
) -> Result<crate::transport::ModelResponse> {
    const MAX_RETRIES: u32 = 2;
    let mut last_err = None;
    for _ in 0..=MAX_RETRIES {
        match transport.complete(messages, specs, "") {
            Ok(resp) => return Ok(resp),
            Err(e @ AgentError::Response(_)) => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// Compress conversation context by summarizing older messages while preserving
/// recent ones and system message.
fn compress_context(
    messages: &[Message],
    context_window: u32,
    target_fraction: f32,
) -> Result<Vec<Message>> {
    let target_tokens = (context_window as f32 * target_fraction) as usize;
    
    // Always keep the system message if present
    let system_msg = messages.iter().find(|m| m.role == Role::System).cloned();
    
    // Keep the last N messages that fit in target tokens
    let mut result = Vec::new();
    if let Some(sys) = system_msg {
        result.push(sys);
    }
    
    // Add messages from the end until we hit target
    let mut token_count = 0;
    for msg in messages.iter().rev() {
        if msg.role == Role::System {
            continue; // Already added
        }
        let msg_tokens = msg.content.len() / 4;
        if token_count + msg_tokens > target_tokens && !result.is_empty() {
            break;
        }
        token_count += msg_tokens;
        result.push(msg.clone());
    }
    
    result.reverse();
    
    // If we still have too many, add a summary message at the beginning
    if token_count > target_tokens && result.len() > 2 {
        let summary = format!(
            "[Context compressed: {} older messages summarized to fit within {} tokens]",
            messages.len() - result.len(),
            target_tokens
        );
        let summary_msg = Message::assistant(summary);
        result.insert(1, summary_msg); // After system message
    }
    
    Ok(result)
}

/// Run one conversation turn to completion and return the final answer.
///
/// `on_event`, if given, is called for every model reply and every tool
/// call/result — the hook that gives a caller (the CLI, a TUI, a test)
/// visibility into what the agent is actually doing turn by turn, instead
/// of only seeing the final answer after the loop finishes silently.
pub fn run_turn(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
) -> Result<String> {
    run_turn_with_events(transport, tools, messages, max_iterations, None, None, None)
}

/// Agent lifecycle events, for surfacing progress to a human (or a log).
pub enum AgentEvent<'a> {
    /// The model produced assistant content this round (may be empty if it
    /// only emitted tool calls).
    AssistantContent(&'a str),
    /// The model asked to call a tool.
    ToolCallStart { name: &'a str, arguments: &'a str },
    /// A tool call finished (ok or error, both surfaced — errors are fed
    /// back to the model as a tool message, not fatal to the turn).
    /// `elapsed` is the wall-clock duration of the tool execution.
    ToolCallEnd {
        name: &'a str,
        result: &'a str,
        elapsed: std::time::Duration,
    },
}

/// Same as [`run_turn`] but takes an optional event callback so the caller
/// can render tool calls / intermediate content as they happen rather than
/// only receiving the final answer string. `interrupted`, if given, is
/// polled between iterations and after every tool call — set it (e.g. from
/// a Ctrl-C handler) to unwind the turn early with `AgentError::Interrupted`
/// instead of running to completion.
pub fn run_turn_with_events(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    mut on_event: Option<&mut dyn FnMut(AgentEvent)>,
    interrupted: Option<&std::sync::atomic::AtomicBool>,
    compression_config: Option<&ContextCompressionConfig>,
) -> Result<String> {
    let specs = tools.specs();
    let is_interrupted =
        || interrupted.is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst));

    let mut iterations = 0u32;
    loop {
        if is_interrupted() {
            return Err(AgentError::Interrupted);
        }
        iterations += 1;
        if iterations > max_iterations {
            return Err(AgentError::BudgetExhausted { iterations });
        }

        // Check if we need to compress context before making the request
        if let Some(cc) = compression_config {
            if cc.enabled && !messages.is_empty() {
                // Estimate token count from messages
                let estimated_tokens: usize = messages.iter().map(|m| m.content.len() / 4).sum();
                // We need the context window from the model's info
                // For now, we'll use a reasonable default (128k) but this should come from the transport
                let context_window = 128000u32; // Default, should be fetched from model info
                let trigger_tokens = (context_window as f32 * cc.trigger_fraction) as usize;
                
                if estimated_tokens > trigger_tokens {
                    // Compress the context
                    if let Ok(compressed) = compress_context(messages, context_window, cc.target_fraction) {
                        messages.clear();
                        messages.extend(compressed);
                    }
                }
            }
        }

        let resp = complete_with_response_retry(transport, messages, &specs)?;

        if is_interrupted() {
            return Err(AgentError::Interrupted);
        }

        // Record the assistant's turn.
        let assistant = Message {
            role: Role::Assistant,
            content: resp.content.clone(),
            tool_calls: resp.tool_calls.clone(),
            ..Default::default()
        };
        messages.push(assistant);

        match resp.finish_reason {
            FinishReason::Stop => {
                // This IS the final answer — the caller renders it once as
                // the answer, so no "thinking" event fires here (avoids the
                // final answer being printed twice: once as an intermediate
                // AssistantContent event, once as the answer itself).
                return Ok(resp.content);
            }
            FinishReason::Length => {
                if let Some(cb) = on_event.as_deref_mut() {
                    if !resp.content.is_empty() {
                        cb(AgentEvent::AssistantContent(&resp.content));
                    }
                }
                continue;
            }
            FinishReason::ToolCalls => {
                if resp.tool_calls.is_empty() {
                    return Ok(resp.content);
                }
                if let Some(cb) = on_event.as_deref_mut() {
                    if !resp.content.is_empty() {
                        cb(AgentEvent::AssistantContent(&resp.content));
                    }
                }
                for call in &resp.tool_calls {
                    if is_interrupted() {
                        return Err(AgentError::Interrupted);
                    }
                    if let Some(cb) = on_event.as_deref_mut() {
                        cb(AgentEvent::ToolCallStart {
                            name: call.name(),
                            arguments: call.arguments(),
                        });
                    }
                    let started = std::time::Instant::now();
                    let result = match tools.execute(call.name(), call.arguments()) {
                        Ok(out) => out,
                        Err(e) => format!("tool error: {e}"),
                    };
                    let elapsed = started.elapsed();
                    if let Some(cb) = on_event.as_deref_mut() {
                        cb(AgentEvent::ToolCallEnd {
                            name: call.name(),
                            result: &result,
                            elapsed,
                        });
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;
    use crate::transport::{FinishReason, ModelResponse, ToolSpec};
    use std::cell::Cell;

    /// Minimal scripted transport for testing the agent loop without network.
    struct StubTransport {
        rounds: Cell<u32>,
        infinite: bool,
    }

    impl StubTransport {
        fn new() -> Self { Self { rounds: Cell::new(0), infinite: false } }
        fn infinite() -> Self { Self { rounds: Cell::new(0), infinite: true } }
    }

    impl ProviderTransport for StubTransport {
        fn name(&self) -> &str { "stub" }
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
                    tool_calls: vec![ToolCall::new("call_1", "run_terminal", args)],
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
    }

    fn base_messages() -> Vec<Message> {
        vec![Message::user("please run a terminal command for me")]
    }

    #[test]
    fn loop_runs_terminal_tool_then_answers() {
        let transport = StubTransport::new();
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();

        let answer = run_turn(&transport, &tools, &mut messages, 8).unwrap();

        assert_eq!(messages.len(), 4, "expected user+assistant+tool+final");
        assert!(answer.contains("stub response"));
        let tool_msg = messages.iter().find(|m| m.role == Role::Tool)
            .expect("a tool message must exist");
        assert!(tool_msg.content.contains("hello from tool"),
            "got: {}", tool_msg.content);
    }

    #[test]
    fn budget_exhaustion_is_an_error() {
        let transport = StubTransport::infinite();
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();
        let res = run_turn(&transport, &tools, &mut messages, 2);
        assert!(matches!(res, Err(AgentError::BudgetExhausted { .. })));
    }

    #[test]
    fn unknown_tool_recovers_gracefully() {
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
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();
        let flag = std::sync::atomic::AtomicBool::new(true);

        let res = run_turn_with_events(
            &transport, &tools, &mut messages, 8, None, Some(&flag), None,
        );

        assert!(matches!(res, Err(AgentError::Interrupted)));
        assert_eq!(messages.len(), 1);
    }
}
