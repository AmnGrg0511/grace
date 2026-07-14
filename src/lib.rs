//! `grace` — a minimal, vendor-neutral ReAct agent core.
//!
//! This is the irreducible spine of an agent (Hermes-inspired), written in
//! Rust with best practices and **zero dependencies** (only `std`).
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
//! - [`json`] — a tiny, dependency-free JSON value/parser/serializer.
//! - [`error`] — the single error type.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // inline comments document intent; public API is small

pub mod agent;
pub mod config;
pub mod error;
pub mod json;
pub mod markdown;
pub mod message;
pub mod tool;
pub mod tools;
pub mod transport;
pub mod transport_http;
pub mod transport_mock;
pub mod transport_openrouter;
