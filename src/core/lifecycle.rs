//! Agent lifecycle: the event stream and the iteration budget.
//!
//! Splitting these out of the loop itself keeps `agent.rs` about *control
//! flow* and leaves "how do we tell a human what is happening" and "how many
//! rounds are we allowed" as separately testable concerns.

use crate::util::{AgentError, Result};

/// Agent lifecycle events, for surfacing progress to a human (or a log).
///
/// Borrowed rather than owned throughout: the loop emits these from data it
/// already holds, so observing an agent costs no allocations.
pub enum AgentEvent<'a> {
    /// The model produced assistant content this round (may be empty if it
    /// only emitted tool calls).
    AssistantContent(&'a str),

    /// A streamed fragment of assistant content, as it arrives. Only emitted
    /// by streaming transports; a non-streaming one emits a single
    /// [`AgentEvent::AssistantContent`] instead.
    ContentFragment(&'a str),

    /// The model asked to call a tool.
    ToolCallStart { name: &'a str, arguments: &'a str },

    /// A tool call finished (ok or error, both surfaced — errors are fed back
    /// to the model as a tool message, not fatal to the turn). `elapsed` is
    /// the wall-clock duration of the tool execution.
    ToolCallEnd {
        name: &'a str,
        result: &'a str,
        elapsed: std::time::Duration,
    },

    /// The conversation was compressed before this round's request.
    ContextCompressed {
        before_tokens: usize,
        after_tokens: usize,
        dropped_messages: usize,
    },

    /// A sub-agent was spawned by the `delegate` tool.
    DelegationStart { task: &'a str, budget: u32 },

    /// A sub-agent finished. `iterations` is what it actually consumed, which
    /// is the number worth surfacing — a budget of 25 that finishes in 3 is a
    /// very different signal from one that finishes in 25.
    DelegationEnd {
        task: &'a str,
        iterations: u32,
        ok: bool,
    },
}

/// A consumer of [`AgentEvent`]s. Type alias only — kept as a named type so
/// the several signatures that thread it around stay readable.
pub type EventSink<'a> = dyn FnMut(AgentEvent) + 'a;

/// Tracks how many LLM round-trips a turn has consumed against its ceiling.
///
/// The budget is the agent's only guaranteed termination condition: a model
/// that keeps requesting tool calls forever is otherwise an infinite loop
/// burning real money. Delegation makes this sharper — a sub-agent gets its
/// *own* budget, so a runaway subtask cannot consume the parent's remaining
/// rounds.
#[derive(Debug, Clone, Copy)]
pub struct IterationBudget {
    limit: u32,
    used: u32,
}

impl IterationBudget {
    /// A budget of `limit` iterations. Clamped to at least 1 — a zero budget
    /// would refuse before the model was ever asked anything, which is never
    /// what a caller means.
    pub fn new(limit: u32) -> Self {
        Self {
            limit: limit.max(1),
            used: 0,
        }
    }

    /// Consume one iteration, or fail if the ceiling is reached.
    pub fn consume(&mut self) -> Result<u32> {
        if self.used >= self.limit {
            return Err(AgentError::BudgetExhausted {
                iterations: self.used,
            });
        }
        self.used += 1;
        Ok(self.used)
    }

    /// Iterations consumed so far.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// The ceiling.
    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Iterations still available.
    pub fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }

    /// Whether the budget is spent.
    pub fn is_exhausted(&self) -> bool {
        self.used >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_up_to_its_limit() {
        let mut b = IterationBudget::new(3);
        assert_eq!(b.consume().unwrap(), 1);
        assert_eq!(b.consume().unwrap(), 2);
        assert_eq!(b.consume().unwrap(), 3);
        assert!(b.is_exhausted());
    }

    #[test]
    fn exceeding_the_budget_is_a_budget_exhausted_error() {
        let mut b = IterationBudget::new(1);
        b.consume().unwrap();
        let err = b.consume().unwrap_err();
        assert!(matches!(err, AgentError::BudgetExhausted { iterations: 1 }));
    }

    #[test]
    fn a_zero_budget_is_clamped_to_one_rather_than_refusing_immediately() {
        // `--max-iterations 0` (or a sub-agent asking for 0) means "minimal",
        // not "never call the model at all".
        let mut b = IterationBudget::new(0);
        assert_eq!(b.limit(), 1);
        assert!(b.consume().is_ok());
    }

    #[test]
    fn remaining_tracks_the_gap_and_saturates() {
        let mut b = IterationBudget::new(2);
        assert_eq!(b.remaining(), 2);
        b.consume().unwrap();
        assert_eq!(b.remaining(), 1);
        b.consume().unwrap();
        assert_eq!(b.remaining(), 0);
        let _ = b.consume();
        assert_eq!(b.remaining(), 0, "must saturate, not underflow");
    }

    #[test]
    fn a_fresh_budget_is_not_exhausted() {
        assert!(!IterationBudget::new(5).is_exhausted());
        assert_eq!(IterationBudget::new(5).used(), 0);
    }

    #[test]
    fn budgets_are_independent_copies() {
        // Delegation relies on this: a sub-agent's spend must not draw down
        // its parent's remaining rounds.
        let parent = IterationBudget::new(10);
        let mut child = parent;
        child.consume().unwrap();
        assert_eq!(parent.used(), 0);
        assert_eq!(child.used(), 1);
    }
}
