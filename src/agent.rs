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

/// Run one conversation turn to completion and return the final answer.
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
                continue;
            }
            FinishReason::ToolCalls => {
                if resp.tool_calls.is_empty() {
                    return Ok(resp.content);
                }
                for call in &resp.tool_calls {
                    let result = match tools.execute(call.name(), call.arguments()) {
                        Ok(out) => out,
                        Err(e) => format!("tool error: {e}"),
                    };
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
