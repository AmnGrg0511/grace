//! `grace` — a minimal, vendor-neutral ReAct agent core.
//!
//! This is the irreducible spine of an agent, written in
//! Rust with best practices, preferring official/native crates (`reqwest`,
//! `serde`/`serde_json`) over hand-rolled reimplementations of TCP/TLS/JSON.
//!
//! The architecture mirrors the engine we studied:
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
//! Modules:
//! - [`message`] — the unified conversation record (the source of truth).
//! - [`transport`] — [`ProviderTransport`](transport::ProviderTransport) trait + OpenAI-compatible & mock transports.
//! - [`tool`] — [`Tool`](tool::Tool) trait + [`ToolRegistry`](tool::ToolRegistry).
//! - [`tools`] — the bundled built-in tools (terminal, file read/write, patch).
//! - [`agent`] — the ReAct loop.
//! - [`config`] — runtime configuration.
//! - [`error`] — the single error type.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // inline comments document intent; public API is small

pub mod agent;
pub mod config;
pub mod default_skills;
pub mod delegate_tool;
pub mod diff;
pub mod error;
pub mod markdown;
pub mod memory;
pub mod message;
pub mod plugin_tool;
pub mod recall;
pub mod session;
pub mod settings;
pub mod skill;
pub mod skin;
pub mod tool;
pub mod tools;
pub mod transport;
pub mod transport_http;
pub mod transport_mock;
pub mod transport_stream;
