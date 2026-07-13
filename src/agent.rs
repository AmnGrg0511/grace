//! The agent — the ReAct loop.
//!
//! This is the irreducible spine we identified in the analysis:
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
//!
//! Everything Hermes adds on top (multi-provider fallback, context
//! compression, tool-safety guardrails, /steer, iteration budget) is wrapper
//! around this. We keep the loop honest and small.

use crate::error::{AgentError, Result};
use crate::message::{Message, Role};
use crate::tool::ToolRegistry;
use crate::transport::{FinishReason, ProviderTransport};

/// Run one conversation turn to completion and return the final answer.
///
/// `transport` and `tools` are injected so the loop stays pure logic. The
/// `messages` vector is the source of truth; it is mutated in place as the
/// conversation grows.
pub fn run_turn(
    transport: &(dyn ProviderTransport + '_),
    tools: &ToolRegistry,
    messages: &mut Vec<Message>,
    max_iterations: u32,
) -> Result<String> {
    let specs = tools.specs();

    let mut iterations = 0u32;
    loop {
        iterations += 1;
        if iterations > max_iterations {
            return Err(AgentError::BudgetExhausted { iterations });
        }

        let resp = transport.complete(messages, &specs, "")?;

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
                return Ok(resp.content);
            }
            FinishReason::Length => {
                // Treat as "continue": emit an empty tool turn is nonsensical,
                // so we just keep looping with the accumulated context.
                continue;
            }
            FinishReason::ToolCalls => {
                if resp.tool_calls.is_empty() {
                    // Provider said tool_calls but sent none; stop defensively.
                    return Ok(resp.content);
                }
                for call in &resp.tool_calls {
                    let result = match tools.execute(&call.name, &call.arguments) {
                        Ok(out) => out,
                        Err(e) => format!("tool error: {e}"),
                    };
                    messages.push(Message::tool(
                        call.id.clone(),
                        call.name.clone(),
                        result,
                    ));
                }
                // Loop again so the model sees the tool results.
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

        // We should have: user, assistant(call), tool(result), assistant(final).
        assert_eq!(messages.len(), 4, "expected user+assistant+tool+final");
        assert!(answer.contains("mock response"));
        // The tool message should contain the echoed shell output.
        let tool_msg = messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("a tool message must exist");
        assert!(tool_msg.content.contains("hello from tool"));
    }

    #[test]
    fn budget_exhaustion_is_an_error() {
        let transport: Box<dyn ProviderTransport> = Box::new(MockTransport::new(100)); // insists on tools
        let mut tools = ToolRegistry::new();
        crate::tools::register_builtins(&mut tools);
        let mut messages = base_messages();
        let res = run_turn(transport.as_ref(), &tools, &mut messages, 2);
        assert!(matches!(res, Err(AgentError::BudgetExhausted { .. })));
    }

    #[test]
    fn unknown_tool_recovers_gracefully() {
        let transport: Box<dyn ProviderTransport> = Box::new(MockTransport::new(1));
        // Registry intentionally empty so the tool is "unknown".
        let tools = ToolRegistry::new();
        let mut messages = base_messages();
        let answer = run_turn(transport.as_ref(), &tools, &mut messages, 8).unwrap();
        // The unknown tool error is fed back; the mock then answers.
        let tool_msg = messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content.contains("unknown tool"));
        assert!(answer.contains("mock response"));
    }
}
