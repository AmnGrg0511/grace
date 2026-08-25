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

/// Build the transport an endpoint needs: the Copilot host gets
/// [`CopilotTransport`] (its bearer must be a short-lived session token,
/// exchanged and refreshed — a plain [`HttpTransport`] sending the OAuth
/// token 404s after ~25 minutes), everything else gets a plain
/// [`HttpTransport`]. This factory is the one place that knows Copilot is
/// special; callers pass an endpoint and get the right thing.
pub fn transport_for(
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout_secs: Option<u64>,
) -> Box<dyn ProviderTransport> {
    if copilot::is_copilot_endpoint(base_url) {
        let mut t = CopilotTransport::new(api_key, model);
        if let Some(secs) = timeout_secs {
            t = t.with_request_timeout(secs);
        }
        Box::new(t)
    } else {
        let mut t = HttpTransport::with_model(base_url, api_key, model);
        if let Some(secs) = timeout_secs {
            t = t.with_request_timeout(secs);
        }
        Box::new(t)
    }
}

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
    fn the_factory_picks_copilot_by_endpoint_and_http_for_the_rest() {
        let copilot = transport_for(
            copilot::BASE_URL,
            "t",
            "gpt-4o",
            None,
        );
        assert_eq!(copilot.name(), "github-copilot");
        // A trailing slash is the same endpoint, not a different one.
        let copilot_slashed = transport_for("https://api.githubcopilot.com/", "t", "m", None);
        assert_eq!(copilot_slashed.name(), "github-copilot");

        let http = transport_for("https://api.openai.com/v1", "k", "gpt-4o", None);
        assert_eq!(http.name(), "openai-http");
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
