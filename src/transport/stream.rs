//! SSE streaming transport for OpenAI-compatible `/chat/completions`.
//!
//! Deliberately standalone: does not touch `ProviderTransport` or
//! `transport_http.rs`. Provides a free function that POSTs with
//! `"stream": true`, parses `data: {...}` SSE lines, accumulates
//! `choices[0].delta.content` fragments (invoking a callback per fragment)
//! and `choices[0].delta.tool_calls` deltas (concatenated by index), and
//! returns a final [`ModelResponse`] once `data: [DONE]` arrives.
//!
//! A dead or erroring stream is an error, not a (possibly empty) partial
//! answer: an `error` frame inside the stream, or EOF before `[DONE]`,
//! yields `Err` so the turn reports why it has no answer instead of
//! silently presenting a truncated one.

use crate::message::{Message, ToolCall};
use crate::transport::r#trait::{FinishReason, ModelResponse, TokenUsage, ToolSpec};
use crate::transport::wire::{parse_usage, tools_to_json};
use crate::util::{AgentError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

/// How often the main thread re-checks the interrupt flag while the helper
/// thread is blocked in a socket read. Bounds Ctrl-C latency during the
/// pre-first-token silence (where no data ever arrives to wake the read).
const INTERRUPT_POLL: Duration = Duration::from_millis(100);

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
    /// Latest top-level `usage` seen on any chunk. Providers stream it
    /// per-chunk (vLLM) or in one trailing empty-choices chunk (OpenAI);
    /// either way the last non-absent value is the authoritative count.
    usage: Option<TokenUsage>,
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
            usage: None,
        }
    }

    /// Feed one decoded SSE `data:` payload (without the `data: ` prefix).
    /// Returns `true` when this was the terminal `[DONE]` marker.
    ///
    /// A chunk carrying a non-null top-level `error` field is a provider
    /// error *inside* the stream — that is an `Err`, not a silently dropped
    /// frame. Chunks that merely lack `choices` (e.g. Qwen/vLLM trailing
    /// usage chunks) are still fine: the key is `error`, not missing choices.
    pub fn feed(
        &mut self,
        payload: &str,
        mut on_fragment: impl FnMut(&str) -> Result<()>,
    ) -> Result<bool> {
        let trimmed = payload.trim();
        if trimmed == "[DONE]" {
            return Ok(true);
        }
        if trimmed.is_empty() {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| AgentError::Response(format!("bad SSE chunk json: {e}")))?;

        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            return Err(AgentError::Response(format!("provider stream error: {msg}")));
        }

        // Before the choices check below: OpenAI's authoritative usage chunk
        // arrives with *no* choices at all, so a missing-choices early
        // return would drop the count.
        if let Some(u) = parse_usage(value.get("usage")) {
            self.usage = Some(u);
        }

        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return Ok(false);
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(piece) = delta.get("content").and_then(Value::as_str) {
                if !piece.is_empty() {
                    self.content.push_str(piece);
                    on_fragment(piece)?;
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
            usage: self.usage,
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
    mut on_fragment: impl FnMut(&str) -> Result<()>,
) -> Result<ModelResponse> {
    use std::io::BufRead;
    let reader = std::io::BufReader::new(body);
    let mut acc = SseAccumulator::new();
    let mut done = false;
    for line in reader.lines() {
        let line = line.map_err(AgentError::Io)?;
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim_start();
        if acc.feed(payload, &mut on_fragment)? {
            done = true;
            break;
        }
    }
    // EOF without the `[DONE]` marker means the stream died mid-response
    // (connection reset, provider crash, idle close). Returning the
    // accumulator here would read a dead stream back as a normal — often
    // empty or partial — answer, so the turn must fail with a reason.
    if !done {
        return Err(AgentError::Transport(
            "stream ended before the [DONE] marker — the response was cut off".into(),
        ));
    }
    Ok(acc.finish())
}

/// A `Read` adapter that lets an interrupt flag abort a blocked socket read.
///
/// reqwest's blocking `Response` has no cancel handle: once `read()` blocks
/// waiting for the first token, nothing else can observe Ctrl-C until data
/// arrives. This wrapper moves the blocking read onto a helper thread that
/// forwards chunks over an mpsc channel; the main thread polls the channel
/// every [`INTERRUPT_POLL`] and, when the flag is set, returns
/// `ErrorKind::Interrupted` so the SSE parse aborts the turn immediately.
/// The connection closes once the helper thread wakes (its `send` fails when
/// the channel's receiver is dropped) and the response is dropped.
struct InterruptibleRead<'a> {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    interrupted: Option<&'a AtomicBool>,
}

impl<'a> InterruptibleRead<'a> {
    fn new(mut resp: reqwest::blocking::Response, interrupted: Option<&'a AtomicBool>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match resp.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Ok(Vec::new()));
                        break;
                    }
                    Ok(n) => {
                        // Receiver dropped = turn aborted; the abandoned
                        // generation stops being consumed and `resp` closes
                        // the connection when this thread exits.
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Self {
            rx,
            pending: Vec::new(),
            interrupted,
        }
    }
}

impl Read for InterruptibleRead<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if !self.pending.is_empty() {
            let n = self.pending.len().min(out.len());
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return Ok(n);
        }
        loop {
            match self.rx.recv_timeout(INTERRUPT_POLL) {
                Ok(Ok(chunk)) if chunk.is_empty() => return Ok(0),
                Ok(Ok(chunk)) => {
                    let n = chunk.len().min(out.len());
                    out[..n].copy_from_slice(&chunk[..n]);
                    if chunk.len() > n {
                        self.pending = chunk[n..].to_vec();
                    }
                    return Ok(n);
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => {
                    if self.interrupted.is_some_and(|f| f.load(Ordering::SeqCst)) {
                        // Pretend EOF. `read_line` retries a real
                        // `Interrupted` error forever, so a distinct abort
                        // signal is needed; `stream_complete` converts this
                        // early EOF into `AgentError::Interrupted` by checking
                        // the flag again.
                        return Ok(0);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(std::io::Error::other(
                        "stream reader thread exited unexpectedly",
                    ));
                }
            }
        }
    }
}

/// Perform a streaming completion against an OpenAI-compatible endpoint.
/// POSTs with `"stream": true`, parses SSE as it arrives, and calls
/// `on_fragment` per content fragment for live printing.
///
/// `interrupted` is polled while the socket read is silent (see
/// [`InterruptibleRead`]); when set, the turn aborts with
/// `AgentError::Interrupted` instead of waiting for the next token.
pub fn stream_complete(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    interrupted: Option<&AtomicBool>,
    on_fragment: impl FnMut(&str) -> Result<()>,
) -> Result<ModelResponse> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let msgs_json = serde_json::to_value(messages).unwrap_or(Value::Array(vec![]));
    // `include_usage` asks OpenAI (and conforming servers) to emit a final
    // usage frame. Providers that always stream usage ignore the flag; the
    // parse side is tolerant either way.
    let mut body = serde_json::json!({
        "model": model,
        "messages": msgs_json,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !tools.is_empty() {
        body["tools"] = tools_to_json(tools);
    }

    // Connect timeout only — a global `.timeout()` would also cap the body
    // read and kill legitimately long generations mid-stream.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AgentError::Transport(format!("HTTP client error: {e}")))?;
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
    // The interruptible wrapper is what makes Ctrl-C land during the
    // pre-first-token silence, not just between tokens.
    let outcome = parse_sse_stream(InterruptibleRead::new(resp, interrupted), on_fragment);
    // An early-EOF driven by the interrupt flag is a cancellation, not a
    // cut-off stream: it must surface as `AgentError::Interrupted` so the turn
    // unwinds to the prompt (and never as a silent no-answer).
    match outcome {
        Err(AgentError::Transport(_)) if interrupted.is_some_and(|f| f.load(Ordering::SeqCst)) => {
            Err(AgentError::Interrupted)
        }
        other => other,
    }
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
            Ok(())
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
            Ok(())
        })
        .unwrap();
        assert_eq!(collected, "\n\nPONG");
        assert_eq!(response.content, "\n\nPONG");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        // The trailing empty-choices chunk is the authoritative count — the
        // per-chunk increments before it must have been superseded, not summed.
        let usage = response.usage.expect("streamed usage was dropped");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 6);
        assert_eq!(usage.total_tokens, 16);
    }

    #[test]
    fn a_stream_without_any_usage_frames_yields_none_not_zero() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\
                   data: [DONE]\n";
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| Ok(())).unwrap();
        assert!(response.usage.is_none());
        assert_eq!(response.content, "hi");
    }

    #[test]
    fn concatenates_tool_call_arguments_across_chunks() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\"}}]}}]}\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"echo hi\\\"}\"}}]}}]}\n\
                    data: {\"choices\":[{\"delta\":{}, \"finish_reason\":\"tool_calls\"}]}\n\
                    data: [DONE]\n";
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| Ok(())).unwrap();
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
        let response = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| Ok(())).unwrap();
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
                |frag| {
                    collected.push_str(frag);
                    Ok(())
                },
            )
            .unwrap_or_else(|e| panic!("chunk={chunk}: {e}"));
            assert_eq!(collected, "Hello, world!", "chunk={chunk}");
            assert_eq!(response.content, "Hello, world!", "chunk={chunk}");
            assert_eq!(response.finish_reason, FinishReason::Stop, "chunk={chunk}");
            assert!(response.tool_calls.is_empty(), "chunk={chunk}");
        }
    }

    #[test]
    fn an_error_frame_fails_the_stream_with_its_message() {
        // Providers surface mid-stream problems as an `error` frame; reading
        // that as "a chunk with no choices" would swallow the error and
        // return the partial content as a normal answer.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\
                   data: {\"error\":{\"message\":\"rate limited by upstream\"}}\n\
                   data: [DONE]\n";
        let err = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("rate limited by upstream"), "{err}");
        assert!(err.to_string().contains("provider stream error"), "{err}");
    }

    #[test]
    fn eof_without_the_done_marker_is_an_error_not_a_partial_answer() {
        // Even with `finish_reason: stop`, an abrupt EOF before `[DONE]` is a
        // dead stream — the turn must say so, not present the fragment.
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n";
        let err = parse_sse_stream(std::io::Cursor::new(sse.as_bytes()), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("[DONE]"), "{err}");
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
            |_| Ok(()),
        )
        .expect("[DONE] must end the parse even though the reader never EOFs");
        assert_eq!(response.content, "ok");
    }

    /// A `Read` whose only act is to fail with a specific error kind — used to
    /// prove non-interrupt I/O errors still map to `AgentError::Io`.
    struct FailingRead(std::io::ErrorKind);

    impl Read for FailingRead {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "synthetic"))
        }
    }

    #[test]
    fn a_non_interrupt_io_error_is_reported_as_such() {
        // A genuine I/O failure (reset, disconnect) must surface as an error,
        // never as a silently truncated answer.
        let err = parse_sse_stream(FailingRead(std::io::ErrorKind::ConnectionAborted), |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, AgentError::Io(_)), "{err:?}");
    }

    #[test]
    fn interruptible_read_pretends_eof_when_the_flag_is_set_while_silent() {
        // Simulates the silence before the first token: the channel has no
        // data and never will, but the flag is set, so the read must abort
        // promptly (EOF, since read_line would retry an Interrupted error
        // forever) instead of blocking until the server sends data.
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
        let flag = AtomicBool::new(false);
        let mut body = InterruptibleRead {
            rx,
            pending: Vec::new(),
            interrupted: Some(&flag),
        };
        let start = std::time::Instant::now();
        flag.store(true, Ordering::SeqCst);
        let n = body.read(&mut [0u8; 16]).unwrap();
        assert_eq!(n, 0, "interrupt must read as EOF");
        // Prompt: well under the poll interval plus slack. (The `tx` keeps the
        // channel alive so recv_timeout is what fires, not a disconnect.)
        assert!(
            start.elapsed() < Duration::from_millis(300),
            "interrupt must land promptly, took {:?}",
            start.elapsed()
        );
        drop(tx);
    }
}
