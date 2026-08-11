//! `grace` — a minimal, vendor-neutral ReAct agent core.
//!
//! The irreducible spine of an agent, written in Rust, preferring official
//! crates (`reqwest`, `serde`) over hand-rolled reimplementations of
//! TCP/TLS/JSON.
//!
//! ```text
//! Message list  ──►  ProviderTransport (normalized LLM call)
//!                       │  returns content + optional tool_calls
//!                       ▼
//!                  if tool_calls: ToolRegistry executes each
//!                       │  results appended as `tool` messages
//!                       ▼
//!                  loop until FinishReason::Stop (or budget exhausted)
//! ```
//!
//! # Layering
//!
//! Dependencies point strictly downward; a cycle here is a design bug.
//!
//! ```text
//!   ui  ────────────────┐
//!   config ─────────────┤
//!   core (agent engine) ┤──►  transport ──►  message
//!   tools ──────────────┤          │
//!   memory/session/     │          ▼
//!   skill/recall  ──────┴───────► util
//! ```
//!
//! - [`message`] — the unified conversation record (the source of truth).
//! - [`transport`] — the `ProviderTransport` seam and its implementations.
//! - [`tools`] — the `Tool` trait, the registry, and every built-in tool.
//! - [`core`] — the ReAct loop, context compression, and sub-agent delegation.
//! - [`config`] — runtime configuration, settings, and the persona.
//! - [`memory`] — durable SQLite-backed facts.
//! - [`session`] — chat history with FTS5 search and cross-terminal locking.
//! - [`skill`] — filesystem-convention skill loading.
//! - [`recall`] — pre-flight recall (injects relevant past context).
//! - [`ui`] — the REPL, markdown rendering, skins, and onboarding.
//! - [`util`] — the error type, token estimation, and diffing.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // inline comments document intent; the public API is small

pub mod config;
pub mod core;
pub mod memory;
pub mod message;
pub mod recall;
pub mod session;
pub mod skill;
pub mod tools;
pub mod transport;
pub mod ui;
pub mod util;

// ---- Convenience re-exports -------------------------------------------------
// The types a caller embedding Grace needs most often, so the common case is
// `use grace::{Agent-ish things}` rather than six deep paths.

pub use config::{Config, Settings};
pub use core::{
    run_turn, run_turn_with_options, AgentEvent, ContextCompressionConfig, Delegation, SubTask,
    TurnOptions, TurnOutcome,
};
pub use memory::Memory;
pub use message::{Message, Role, ToolCall};
pub use session::SessionStore;
pub use skill::SkillStore;
pub use tools::{Tool, ToolRegistry};
pub use transport::{FinishReason, ModelResponse, ProviderTransport, ToolSpec};
pub use util::{AgentError, Result};
