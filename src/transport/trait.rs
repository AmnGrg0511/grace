//! The vendor-neutral LLM seam: [`ProviderTransport`] and its value types.
//!
//! This module holds *only* the abstraction — no HTTP, no provider quirks, no
//! wire parsing. The agent loop depends on this file and nothing else in the
//! `transport` tree, which is what keeps `core` free of any provider
//! knowledge. Concrete providers live in [`http`](super::http) and
//! [`copilot`](super::copilot); shared OpenAI wire helpers live in
//! [`wire`](super::wire).

use crate::message::{Message, ToolCall};
use crate::util::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The set of tools, in the OpenAI tool-spec shape, that the model may call.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema fragment for the `properties` of the function.
    pub parameters: Value,
}

/// What a model returned for one turn.
#[derive(Debug, Clone, Default)]
pub struct ModelResponse {
    /// Assistant text (may be empty when the model only emits tool calls).
    pub content: String,
    /// Tool invocations the model is asking us to run.
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped: `stop` ends the turn; anything else usually
    /// means "continue" (e.g. `tool_calls`, `length`).
    pub finish_reason: FinishReason,
    /// Token accounting as reported by the provider, if it reported any.
    /// `None` for providers that omit it — callers fall back to a local
    /// estimate rather than assuming zero. This is the source of truth for
    /// the context bar when present, so the bar and compaction budget on
    /// what the provider actually counted, not a client-side guess.
    pub usage: Option<TokenUsage>,
}

/// Token counts from a single model call, in the provider's own accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Tokens the provider counted for the request (the whole prompt that
    /// was sent, i.e. everything in the context window).
    pub prompt_tokens: u64,
    /// Tokens the provider counted for the generated completion.
    pub completion_tokens: u64,
    /// `prompt_tokens + completion_tokens` as the provider reports it.
    pub total_tokens: u64,
}

/// Normalized stop reason, independent of provider vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinishReason {
    /// Model produced a final answer; the turn is done.
    #[default]
    Stop,
    /// Model requested tool calls; the loop must execute and continue.
    ToolCalls,
    /// Output was truncated by a length limit; the loop continues.
    Length,
}

impl FinishReason {
    /// Map a provider-specific finish-reason string to ours.
    pub fn from_api(s: &str) -> Self {
        match s {
            "stop" | "end_turn" => FinishReason::Stop,
            "tool_calls" | "function_call" => FinishReason::ToolCalls,
            "length" | "max_tokens" => FinishReason::Length,
            _ => FinishReason::Stop,
        }
    }
}

/// Information about a model as reported by its provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub provider: String,
}

/// A normalized LLM endpoint.
///
/// Only [`name`](ProviderTransport::name) and
/// [`complete`](ProviderTransport::complete) are required; everything else has
/// a conservative default so a minimal transport (a test stub, a fixed-model
/// provider) does not have to opt out of capabilities it does not have.
pub trait ProviderTransport {
    /// Short human-readable transport name, for the startup banner and logs.
    fn name(&self) -> &str;

    /// Send the conversation and available tools; return the model's response.
    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        model: &str,
    ) -> Result<ModelResponse>;

    /// Switch the model this transport talks to, for `/model` mid-chat.
    fn set_model(&self, _model: &str) {}

    /// Current model name, if this transport has one to report (used by
    /// `/model` with no argument to show what's active).
    fn current_model(&self) -> Option<String> {
        None
    }

    /// Re-point this transport at a different provider endpoint, for `/model`
    /// mid-chat picking a provider other than the one it started with.
    fn set_endpoint(&self, _base_url: &str, _api_key: &str) {}

    /// Current base_url, if this transport has a swappable one (used by
    /// `/model` to detect "you picked a different provider than the one this
    /// session is currently wired to").
    fn current_base_url(&self) -> Option<String> {
        None
    }

    /// Fetch available models from the provider. Defaults to empty — a
    /// transport that cannot enumerate models is not an error, it just has
    /// nothing to offer the model picker.
    fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    /// The model's usable context window in tokens, if the transport knows it.
    ///
    /// This is the fix for the hardcoded 128k default that used to live in the
    /// agent loop: compression now asks the transport, and only falls back to
    /// a static table when the provider genuinely cannot say.
    fn context_window(&self) -> Option<u32> {
        None
    }

    /// Streaming completion. Default: run the blocking
    /// [`complete`](ProviderTransport::complete) and emit the whole answer as
    /// one fragment, so every transport is *usable* in streaming call sites
    /// even when it cannot truly stream. Transports that can stream (see
    /// [`http`](super::http)) override this.
    fn complete_streaming(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        model: &str,
        on_fragment: &mut dyn FnMut(&str),
    ) -> Result<ModelResponse> {
        let resp = self.complete(messages, tools, model)?;
        if !resp.content.is_empty() {
            on_fragment(&resp.content);
        }
        Ok(resp)
    }

    /// Whether [`complete_streaming`](ProviderTransport::complete_streaming)
    /// actually streams incrementally, as opposed to falling back to the
    /// one-shot default. Callers use this to decide whether to show a
    /// spinner (no real streaming) or live text (real streaming).
    fn supports_streaming(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Minimal;

    impl ProviderTransport for Minimal {
        fn name(&self) -> &str {
            "minimal"
        }
        fn complete(&self, _m: &[Message], _t: &[ToolSpec], _model: &str) -> Result<ModelResponse> {
            Ok(ModelResponse {
                content: "hello".into(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }
    }

    #[test]
    fn finish_reason_maps_provider_vocabulary() {
        assert_eq!(FinishReason::from_api("stop"), FinishReason::Stop);
        assert_eq!(FinishReason::from_api("end_turn"), FinishReason::Stop);
        assert_eq!(FinishReason::from_api("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(
            FinishReason::from_api("function_call"),
            FinishReason::ToolCalls
        );
        assert_eq!(FinishReason::from_api("length"), FinishReason::Length);
        assert_eq!(FinishReason::from_api("max_tokens"), FinishReason::Length);
    }

    #[test]
    fn unknown_finish_reason_defaults_to_stop_not_a_hang() {
        // A provider inventing a new reason must end the turn, never loop.
        assert_eq!(FinishReason::from_api("banana"), FinishReason::Stop);
        assert_eq!(FinishReason::default(), FinishReason::Stop);
    }

    #[test]
    fn minimal_transport_gets_working_defaults() {
        let t = Minimal;
        assert!(t.current_model().is_none());
        assert!(t.current_base_url().is_none());
        assert!(t.context_window().is_none());
        assert!(t.list_models().unwrap().is_empty());
        assert!(!t.supports_streaming());
    }

    #[test]
    fn default_streaming_emits_the_whole_answer_as_one_fragment() {
        // A non-streaming transport must still be callable from streaming
        // code paths — otherwise chat mode would need two branches per
        // provider.
        let t = Minimal;
        let mut seen = String::new();
        let resp = t
            .complete_streaming(&[], &[], "m", &mut |f| seen.push_str(f))
            .unwrap();
        assert_eq!(seen, "hello");
        assert_eq!(resp.content, "hello");
    }

    #[test]
    fn transport_is_object_safe() {
        let t: Box<dyn ProviderTransport> = Box::new(Minimal);
        assert_eq!(t.name(), "minimal");
    }
}
