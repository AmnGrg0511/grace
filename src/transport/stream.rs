//! SSE streaming transport for OpenAI-compatible `/chat/completions`.
//!
//! Deliberately standalone: does not touch `ProviderTransport` or
//! `transport_http.rs`. Provides a free function that POSTs with
//! `"stream": true`, parses `data: {...}` SSE lines, accumulates
//! `choices[0].delta.content` fragments (invoking a callback per fragment)
//! and `choices[0].delta.tool_calls` deltas (concatenated by index), and
//! returns a final [`ModelResponse`] once `data: [DONE]` arrives.

use crate::message::{Message, ToolCall};
use crate::transport::r#trait::{FinishReason, ModelResponse, ToolSpec};
use crate::transport::wire::tools_to_json;
use crate::util::{AgentError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates streamed SSE chunks into a final [`ModelResponse`], invoking
/// `on_fragment` for every piece of assistant `content` as it arrives.
pub struct SseAccumulator {
    content: String,
    tool_calls: BTreeMap<u64, PartialToolCall>,
    finish_reason: FinishReason,
}

impl Default for SseAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl SseAccumulator {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: FinishReason::Stop,
        }
    }

    /// Feed one decoded SSE `data:` payload (without the `data: ` prefix).
    /// Returns `true` when this was the terminal `[DONE]` marker.
    pub fn feed(&mut self, payload: &str, mut on_fragment: impl FnMut(&str)) -> Result<bool> {
        let trimmed = payload.trim();
        if trimmed == "[DONE]" {
            return Ok(true);
        }
        if trimmed.is_empty() {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| AgentError::Response(format!("bad SSE chunk json: {e}")))?;

        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return Ok(false);
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(piece) = delta.get("content").and_then(Value::as_str) {
                if !piece.is_empty() {
                    self.content.push_str(piece);
                    on_fragment(piece);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.tool_calls.entry(idx).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            entry.id = id.to_string();
                        }
                    }
                    if let Some(func) = call.get("function") {
                        if let Some(name) = func.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                entry.name = name.to_string();
                            }
                        }
                        if let Some(args) = func.get("arguments").and_then(Value::as_str) {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = FinishReason::from_api(reason);
        }
        Ok(false)
    }

    /// Convert accumulated state into a final [`ModelResponse`].
    pub fn finish(self) -> ModelResponse {
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_values()
            .filter(|p| !p.name.is_empty())
            .map(|p| ToolCall::new(p.id, p.name, p.arguments))
            .collect();
        let finish_reason = if tool_calls.is_empty() {
            self.finish_reason
        } else {
            FinishReason::ToolCalls
        };
        ModelResponse {
            content: self.content,
            tool_calls,
            finish_reason,
        }
    }
}

/// Parse a raw byte stream of SSE lines (`data: ...\n\n` framed), calling
/// `on_fragment` for each content delta, and return the final response.
/// This function does no I/O beyond reading from `body` — network fetching
/// is the caller's job — which keeps it trivially testable with an in-memory
/// byte slice.
pub fn parse_sse_stream(
    body: impl Read,
    mut on_fragment: impl FnMut(&str),
) -> Result<ModelResponse> {
    use std::io::BufRead;
    let reader = std::io::BufReader::new(body);
    let mut acc = SseAccumulator::new();
    for line in reader.lines() {
        let line = line.map_err(AgentError::Io)?;
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim_start();
        if acc.feed(payload, &mut on_fragment)? {
            break;
        }
    }
    Ok(acc.finish())
}

/// Perform a streaming completion against an OpenAI-compatible endpoint.
/// POSTs with `"stream": true`, parses SSE as it arrives, and calls
/// `on_fragment` per content fragment for live printing.
pub fn stream_complete(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    on_fragment: impl FnMut(&str),
) -> Result<ModelResponse> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let msgs_json = serde_json::to_value(messages).unwrap_or(Value::Array(vec![]));
    let mut body = serde_json::json!({
        "model": model,
        "messages": msgs_json,
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = tools_to_json(tools);
    }

    let client = reqwest::blocking::Client::new();
    let mut req = client.post(&url).json(&body);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(AgentError::Transport(format!("HTTP {status}: {text}")));
    }
    // `Response` implements io::Read, so this reads the body incrementally
    // and each fragment is delivered as it arrives — `resp.bytes()` would
    // buffer the whole generation and fire everything in one burst at the end.
    parse_sse_stream(resp, on_fragment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_content_fragments() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"lo, \"}}]}\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}, \"finish_reason\":null}]}\n\
                    data: {\"choices\":[{\"delta\":{}, \"finish_reason\":\"stop\"}]}\n\
                    data: [DONE]\n";
        let mut collected = String::new();
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |frag| {
            collected.push_str(frag);
        })
        .unwrap();
        assert_eq!(collected, "Hello, world!");
        assert_eq!(response.content, "Hello, world!");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn qwen_thinking_stream_extracts_content_after_reasoning() {
        // Shape of vLLM/Qwen reasoning gateways: role chunk with empty
        // content, then `delta.reasoning` chunks (no content field), then
        // content chunks, a stop chunk, an empty-choices usage chunk, [DONE].
        let sse = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen-3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"logprobs\":null}],\"usage\":{\"prompt_tokens\":10,\"total_tokens\":10,\"completion_tokens\":0}}\n\
                   data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen-3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"Let\"},\"logprobs\":null}],\"usage\":{\"prompt_tokens\":10,\"total_tokens\":11,\"completion_tokens\":1}}\n\
                   data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen-3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\n\\n\"},\"logprobs\":null}],\"usage\":{\"prompt_tokens\":10,\"total_tokens\":12,\"completion_tokens\":2}}\n\
                   data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen-3.8-27b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"PONG\"},\"logprobs\":null,\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"total_tokens\":16,\"completion_tokens\":6}}\n\
                   data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"qwen-3.8-27b\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"total_tokens\":16,\"completion_tokens\":6},\"system_fingerprint\":\"vllm-0.27.1\"}\n\
                   data: [DONE]\n";
        let mut collected = String::new();
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |frag| {
            collected.push_str(frag);
        })
        .unwrap();
        assert_eq!(collected, "\n\nPONG");
        assert_eq!(response.content, "\n\nPONG");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn concatenates_tool_call_arguments_across_chunks() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"echo hi\\\"}\"}}]}}]}\n\
                    data: {\"choices\":[{\"delta\":{}, \"finish_reason\":\"tool_calls\"}]}\n\
                    data: [DONE]\n";
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| {}).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        let call = &response.tool_calls[0];
        assert_eq!(call.name(), "bash");
        assert_eq!(call.arguments(), "{\"command\":\"echo hi\"}");
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn multiple_indexed_tool_calls_stay_separate() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"foo\",\"arguments\":\"{}\"}}]}}]}\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"bar\",\"arguments\":\"{}\"}}]}}]}\n\
                    data: [DONE]\n";
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| {}).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name(), "foo");
        assert_eq!(response.tool_calls[1].name(), "bar");
    }

    /// A `Read` that yields at most `chunk` bytes per call — a stand-in for
    /// a network delivering an SSE body one chunk at a time, where chunk
    /// boundaries can fall anywhere (mid-`data:`, mid-JSON, mid-line-break).
    struct ChunkyRead {
        bytes: Vec<u8>,
        pos: usize,
        chunk: usize,
        /// Error instead of EOF once the bytes are exhausted — a keep-alive
        /// server that never closes the connection.
        open_after_end: bool,
    }

    impl Read for ChunkyRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.bytes.len() {
                if self.open_after_end {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "connection still open",
                    ));
                }
                return Ok(0);
            }
            let n = self.chunk.min(buf.len()).min(self.bytes.len() - self.pos);
            buf[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn sse_parse_is_chunk_boundary_agnostic() {
        // Live streaming depends on this: the body must parse identically
        // whether it arrives in one slab or byte by byte, so a socket
        // delivering chunks can drive the same code path a Cursor does.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"lo, \"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"world!\"}, \"finish_reason\":null}]}\n\
                   data: {\"choices\":[{\"delta\":{}, \"finish_reason\":\"stop\"}]}\n\
                   data: [DONE]\n";
        for chunk in [1usize, 2, 3, 7] {
            let mut collected = String::new();
            let response = parse_sse_stream(
                ChunkyRead {
                    bytes: sse.as_bytes().to_vec(),
                    pos: 0,
                    chunk,
                    open_after_end: false,
                },
                |frag| collected.push_str(frag),
            )
            .unwrap_or_else(|e| panic!("chunk={chunk}: {e}"));
            assert_eq!(collected, "Hello, world!", "chunk={chunk}");
            assert_eq!(response.content, "Hello, world!", "chunk={chunk}");
            assert_eq!(response.finish_reason, FinishReason::Stop, "chunk={chunk}");
            assert!(response.tool_calls.is_empty(), "chunk={chunk}");
        }
    }

    #[test]
    fn sse_parse_returns_at_done_without_waiting_for_eof() {
        // A real streaming connection can stay open after `[DONE]`
        // (keep-alive): the parser must stop reading at the marker instead
        // of blocking until the server closes the connection.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                   data: [DONE]\n";
        let response = parse_sse_stream(
            ChunkyRead {
                bytes: sse.as_bytes().to_vec(),
                pos: 0,
                chunk: 2,
                open_after_end: true,
            },
            |_| {},
        )
        .expect("[DONE] must end the parse even though the reader never EOFs");
        assert_eq!(response.content, "ok");
    }
}
