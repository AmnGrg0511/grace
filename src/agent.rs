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
    run_turn_with_events(transport, tools, messages, max_iterations, None)
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
    ToolCallEnd { name: &'a str, result: &'a str },
}

/// Same as [`run_turn`] but takes an optional event callback so the caller
/// can render tool calls / intermediate content as they happen rather than
/// only receiving the final answer string.
pub fn run_turn_with_events(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
    mut on_event: Option<&mut dyn FnMut(AgentEvent)>,
) -> Result<String> {
    let specs = tools.specs();

    let mut iterations = 0u32;
    loop {
        iterations += 1;
        if iterations > max_iterations {
            return Err(AgentError::BudgetExhausted { iterations });
        }

        let resp = complete_with_response_retry(transport, messages, &specs)?;

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
                    if let Some(cb) = on_event.as_deref_mut() {
                        cb(AgentEvent::ToolCallStart { name: call.name(), arguments: call.arguments() });
                    }
                    let result = match tools.execute(call.name(), call.arguments()) {
                        Ok(out) => out,
                        Err(e) => format!("tool error: {e}"),
                    };
                    if let Some(cb) = on_event.as_deref_mut() {
                        cb(AgentEvent::ToolCallEnd { name: call.name(), result: &result });
                    }
                    messages.push(Message::tool(call.id.clone(), call.name().to_string(), result));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport_mock::MockTransport;

    fn base_messages() -> Vec<Message> {
        vec![Message::user("please run a terminal command for me")]
    }

    #[test]
    fn loop_runs_terminal_tool_then_answers() {
        let transport: Box<dyn ProviderTransport> = Box::new(MockTransport::new(1));
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();

        let answer = run_turn(transport.as_ref(), &tools, &mut messages, 8).unwrap();

        assert_eq!(messages.len(), 4, "expected user+assistant+tool+final");
        assert!(answer.contains("mock response"));
        let tool_msg = messages.iter().find(|m| m.role == Role::Tool).expect("a tool message must exist");
        assert!(tool_msg.content.contains("hello from tool"));
    }

    #[test]
    fn budget_exhaustion_is_an_error() {
        let transport: Box<dyn ProviderTransport> = Box::new(MockTransport::new(100));
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();
        let res = run_turn(transport.as_ref(), &tools, &mut messages, 2);
        assert!(matches!(res, Err(AgentError::BudgetExhausted { .. })));
    }

    #[test]
    fn unknown_tool_recovers_gracefully() {
        let transport: Box<dyn ProviderTransport> = Box::new(MockTransport::new(1));
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        let answer = run_turn(transport.as_ref(), &tools, &mut messages, 8).unwrap();
        let tool_msg = messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content.contains("unknown tool"));
        assert!(answer.contains("mock response"));
    }
}
