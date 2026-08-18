//! Provider transports — the vendor-neutral LLM seam.
//!
//! Layering inside this module:
//!
//! ```text
//! trait.rs   ProviderTransport + ModelResponse/ToolSpec/FinishReason
//!    ▲            (the only thing `core` is allowed to know about)
//!    │
//! wire.rs    shared OpenAI /chat/completions request+response shaping
//!    ▲
//!    ├── http.rs     any OpenAI-compatible endpoint (OpenAI, OpenRouter, ...)
//!    ├── copilot.rs  GitHub Copilot (OAuth token exchange + refresh)
//!    └── stream.rs   SSE accumulation, used by http.rs and chat mode
//! ```

pub mod copilot;
pub mod http;
pub mod stream;
pub mod r#trait;
pub mod wire;

pub use copilot::CopilotTransport;
pub use http::HttpTransport;
pub use r#trait::{FinishReason, ModelInfo, ModelResponse, ProviderTransport, TokenUsage, ToolSpec};
pub use wire::{parse_openai_message, parse_usage, tools_to_json, urlencoding};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trait_and_its_value_types_are_reachable_from_the_module_root() {
        // `core` imports these from `crate::transport`, so a dropped
        // re-export breaks the agent loop, not this module.
        let _spec = ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let _resp = ModelResponse::default();
        assert_eq!(FinishReason::default(), FinishReason::Stop);
    }

    #[test]
    fn the_concrete_transports_are_reachable_and_object_safe() {
        let http: Box<dyn ProviderTransport> =
            Box::new(HttpTransport::with_model("https://x/v1", "", "m"));
        assert_eq!(http.name(), "openai-http");

        let copilot: Box<dyn ProviderTransport> = Box::new(CopilotTransport::new("t", "m"));
        assert_eq!(copilot.name(), "github-copilot");
    }

    #[test]
    fn the_shared_wire_helpers_are_reachable() {
        assert_eq!(urlencoding("a/b"), "a%2Fb");
        assert_eq!(tools_to_json(&[]), serde_json::json!([]));
        let parsed =
            parse_openai_message(&serde_json::json!({"content": "hi"}), Some("stop")).unwrap();
        assert_eq!(parsed.content, "hi");
    }
}
