//! The agent engine.
//!
//! ```text
//! lifecycle.rs   AgentEvent + IterationBudget (observability & termination)
//! context.rs     token-aware context compression
//! agent.rs       the ReAct loop
//! delegation.rs  bounded sub-agents
//! ```
//!
//! This layer depends only on [`crate::message`], [`crate::transport`],
//! [`crate::tools`], and [`crate::util`]. It knows nothing about the CLI,
//! sessions, skills, or persistence — which is what makes the same loop usable
//! from a REPL, a one-shot invocation, and a delegated sub-agent.

pub mod agent;
pub mod context;
pub mod delegation;
pub mod lifecycle;

pub use agent::{run_turn, run_turn_with_options, TurnOptions, TurnOutcome};
pub use context::{
    CompressionOutcome, CompressionResult, Compressor, ContextCompressionConfig,
};
pub use delegation::{
    Delegation, DelegationDepth, SubAgentReport, SubTask, DEFAULT_DELEGATION_BUDGET,
    MAX_DELEGATION_BUDGET, MAX_DELEGATION_DEPTH,
};
pub use lifecycle::{AgentEvent, IterationBudget};

#[cfg(test)]
mod tests {
    use super::*;

    /// The re-export surface is the contract for anyone embedding Grace.
    /// Compiling these paths is the assertion — a rename that silently drops
    /// a re-export breaks downstream code, not any test inside the module
    /// that moved.
    #[test]
    fn the_engine_api_is_reachable_from_the_module_root() {
        let _: fn(
            &(dyn crate::transport::ProviderTransport + '_),
            &crate::tools::ToolRegistry,
            &mut Vec<crate::message::Message>,
            u32,
        ) -> crate::util::Result<String> = run_turn;

        let _budget = IterationBudget::new(4);
        let _opts = TurnOptions::new();
        let _cfg = ContextCompressionConfig::default();
        let _task = SubTask::new("x");
        let _depth = DelegationDepth::ROOT;
    }

    /// Compile-time coherence check on the delegation constants: a default
    /// budget above the cap, or a zero depth limit, would make delegation
    /// either impossible or immediately clamped. Failing the *build* is the
    /// right outcome for a nonsensical constant, not failing a test run.
    const _: () = {
        assert!(DEFAULT_DELEGATION_BUDGET >= 1);
        assert!(DEFAULT_DELEGATION_BUDGET <= MAX_DELEGATION_BUDGET);
        assert!(MAX_DELEGATION_DEPTH >= 1);
    };
}
