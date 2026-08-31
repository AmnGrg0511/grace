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
use crate::transport::wire::{parse_usage, stream_request_body};
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
        return Err(AgentError::Transport(CUT_OFF_ERROR.into()));
    }
    Ok(acc.finish())
}

/// The error an EOF without the `[DONE]` marker produces — a dead stream,
/// not a (possibly empty) partial answer.
const CUT_OFF_ERROR: &str = "stream ended before the [DONE] marker — the response was cut off";

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

/// Total attempts for one streaming request: the first plus two retries at
/// 500ms/1s backoff — the same budget as the non-streaming
/// `send_with_retry`, so chat mode and one-shot mode absorb flaky endpoints
/// the same way.
const MAX_ATTEMPTS: u32 = 3;

/// Perform a streaming completion against an OpenAI-compatible endpoint.
/// POSTs with `"stream": true`, parses SSE as it arrives, and calls
/// `on_fragment` per content fragment for live printing.
///
/// `interrupted` is polled while the socket read is silent (see
/// [`InterruptibleRead`]); when set, the turn aborts with
/// `AgentError::Interrupted` instead of waiting for the next token.
///
/// Transport-level stream deaths — 429/5xx responses, an unsent request, a
/// connection dropped mid-body, an EOF before `[DONE]` — are retried up to
/// [`MAX_ATTEMPTS`], but **only while no content fragment has been emitted**:
/// once text is on screen, re-issuing the request would print the same
/// answer a second time, which the append-only rendering path forbids.
#[allow(clippy::too_many_arguments)]
pub fn stream_complete(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    reasoning: Option<&str>,
    interrupted: Option<&AtomicBool>,
    on_fragment: impl FnMut(&str) -> Result<()>,
) -> Result<ModelResponse> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    // Built from the shared base so the streaming path sends the same
    // `temperature`/`tool_choice`/`reasoning_effort` as the non-streaming path
    // (a past bug dropped all three here); `stream_request_body` layers on the
    // `stream` flag and the `include_usage` request.
    let body = stream_request_body(model, messages, tools, reasoning);

    if crate::util::log::enabled() {
        let mut msgs = String::new();
        for m in messages {
            msgs.push_str(&m.render());
            msgs.push('\n');
        }
        crate::util::log::line(&format!(
            "request model={model} stream=true messages={}\n{}",
            messages.len(),
            msgs.trim_end()
        ));
    }

    // Connect timeout only — a global `.timeout()` would also cap the body
    // read and kill legitimately long generations mid-stream.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| AgentError::Transport(format!("HTTP client error: {e}")))?;

    let started = std::time::Instant::now();
    let mut backoff = Duration::from_millis(500);
    // Latched the first time a content fragment reaches the caller; from
    // that point the request is no longer safely re-issuable.
    let mut emitted = false;
    // `mut` so the per-attempt `forward` wrapper below can borrow it
    // mutably and forward each fragment.
    let mut on_fragment = on_fragment;

    for attempt in 1..=MAX_ATTEMPTS {
        // Ctrl-C during the backoff sleep: stop now, don't fire another
        // request the user has already cancelled.
        if attempt > 1 && interrupted.is_some_and(|f| f.load(Ordering::SeqCst)) {
            return Err(AgentError::Interrupted);
        }
        let mut req = client.post(&url).json(&body);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }

        // Set at each failure site: a 4xx (auth, bad model) is permanent and
        // a provider error frame is an answer-level failure owned by the
        // agent-layer retry — only transport-level stream deaths retry here.
        let mut retryable = true;
        let outcome = match req.send() {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().unwrap_or_default();
                    retryable = status.as_u16() == 429 || status.is_server_error();
                    Err(AgentError::Transport(format!("HTTP {status}: {text}")))
                } else {
                    // `Response` implements io::Read, so this reads the body
                    // incrementally and each fragment is delivered as it
                    // arrives — `resp.bytes()` would buffer the whole
                    // generation and fire everything in one burst at the end.
                    // The interruptible wrapper is what makes Ctrl-C land
                    // during the pre-first-token silence, not just between
                    // tokens.
                    let mut forward = |frag: &str| -> Result<()> {
                        emitted = true;
                        on_fragment(frag)
                    };
                    let parsed =
                        parse_sse_stream(InterruptibleRead::new(resp, interrupted), &mut forward);
                    if matches!(parsed, Err(AgentError::Response(_))) {
                        retryable = false;
                    }
                    parsed
                }
            }
            Err(e) => Err(AgentError::from(e)),
        };

        match outcome {
            Ok(resp) => {
                if crate::util::log::enabled() {
                    crate::util::log::line(&format!(
                        "response finish={:?} tool_calls={} content_chars={} usage={:?}",
                        resp.finish_reason,
                        resp.tool_calls.len(),
                        resp.content.chars().count(),
                        resp.usage.map(|u| format!(
                            "{} in / {} out",
                            u.prompt_tokens, u.completion_tokens
                        ))
                    ));
                }
                return Ok(resp);
            }
            Err(e) => {
                // An early-EOF driven by the interrupt flag is a
                // cancellation, not a cut-off stream: it must surface as
                // `AgentError::Interrupted` so the turn unwinds to the prompt
                // (and never as a silent no-answer), and must never burn a
                // retry.
                if interrupted.is_some_and(|f| f.load(Ordering::SeqCst)) {
                    return Err(AgentError::Interrupted);
                }
                if retryable && !emitted && attempt < MAX_ATTEMPTS {
                    std::thread::sleep(backoff);
                    backoff *= 2;
                    continue;
                }
                return Err(terminal_stream_error(e, attempt, emitted, started.elapsed()));
            }
        }
    }
    unreachable!("the loop returns on every path")
}

/// Wrap a failed streaming request in a message that says what happened,
/// embedding the original error so the underlying cause (e.g. "error
/// decoding response body") stays visible. Already-self-explanatory failures
/// (auth 4xx, provider error frames) pass through untouched.
fn terminal_stream_error(
    e: AgentError,
    attempt: u32,
    emitted: bool,
    elapsed: Duration,
) -> AgentError {
    let is_stream_death = matches!(&e, AgentError::Io(_))
        || matches!(&e, AgentError::Transport(s) if s == CUT_OFF_ERROR);
    if !is_stream_death {
        return e;
    }
    if emitted {
        AgentError::Transport(format!(
            "the response was cut off mid-stream after partial output — the answer \
             is incomplete and the request was not retried (a retry would duplicate \
             it on screen): {e}"
        ))
    } else if attempt >= MAX_ATTEMPTS {
        AgentError::Transport(format!(
            "the response was cut off mid-stream after {:.1}s across {attempt} attempts: {e}",
            elapsed.as_secs_f64()
        ))
    } else {
        e
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

    // ---- retry behavior against a real (loopback) socket ----
    //
    // Deterministic by construction: the server hands out a fixed sequence
    // of canned responses, one per connection, so no timing is involved —
    // only how many times `stream_complete` dials in.

    /// Serve `replies` on `127.0.0.1`, one canned response per connection
    /// (in order), then close the connection. `seen` counts connections so
    /// a test can assert exactly how many attempts were made. Loopback and
    /// small payloads: by the time the client blocks for the response the
    /// request is fully on the wire, so draining to the header terminator
    /// (plus a read timeout as a backstop) is enough.
    fn mock_server(replies: Vec<Vec<u8>>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        std::thread::spawn(move || {
            let mut i = 0usize;
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut got = Vec::new();
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    let mut buf = [0u8; 4096];
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if got.len() > 64 * 1024 {
                                break;
                            }
                        }
                    }
                }
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(reply) = replies.get(i) {
                    use std::io::Write as _;
                    let _ = stream.write_all(reply);
                    let _ = stream.flush();
                }
                i += 1;
                let _ = stream.shutdown(std::net::Shutdown::Both);
                if i >= replies.len() {
                    break;
                }
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    /// A 200 whose SSE body simply ends without `[DONE]` — the server
    /// closes the connection mid-response.
    fn sse_200(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
        )
        .into_bytes()
    }

    /// A 200 with `transfer-encoding: chunked` that promises 8 chunk bytes
    /// and closes after 6 — the body decoder fails mid-stream, the io
    /// error class a production drop produces ("error decoding response
    /// body" / "request or response body error" depending on hyper build).
    fn chunked_cut_off() -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
           transfer-encoding: chunked\r\nconnection: close\r\n\r\n\
           8\r\ndata: "
            .to_vec()
    }

    fn hello_done() -> Vec<u8> {
        sse_200(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
    }

    fn stream_once(base_url: &str) -> Result<ModelResponse> {
        stream_complete(
            base_url,
            "",
            "m",
            &[Message::user("hi")],
            &[],
            None,
            None,
            |_| Ok(()),
        )
    }

    #[test]
    fn a_stream_dropped_mid_body_is_retried_until_it_completes() {
        let (base_url, seen) = mock_server(vec![chunked_cut_off(), hello_done()]);
        let resp = stream_once(&base_url).expect("the second attempt completes the turn");
        assert_eq!(resp.content, "Hello");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "one cut-off attempt, then one success"
        );
    }

    #[test]
    fn a_cut_off_stream_exhausts_its_attempts_with_an_actionable_error() {
        let (base_url, seen) =
            mock_server(vec![chunked_cut_off(), chunked_cut_off(), chunked_cut_off()]);
        let err = stream_once(&base_url).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cut off mid-stream"), "{msg}");
        assert!(msg.contains("3 attempts"), "{msg}");
        // The original failure stays visible inside the wrapped message —
        // a body-stream failure, however this hyper build spells it.
        assert!(
            msg.contains("stream ended before the [DONE] marker")
                || msg.contains("error decoding response body")
                || msg.contains("body error"),
            "{msg}"
        );
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_stream_that_already_emitted_output_is_not_retried() {
        // The fragment is real on-screen output: re-issuing the request
        // would print "partial" a second time, so the failure must stand.
        let (base_url, seen) = mock_server(vec![
            sse_200("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"),
        ]);
        let err = stream_once(&base_url).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("partial output"), "{msg}");
        assert!(msg.contains("not retried"), "{msg}");
        assert_eq!(seen.load(Ordering::SeqCst), 1, "emitted output must not be re-streamed");
    }

    #[test]
    fn a_429_is_retried_but_a_later_4xx_is_not() {
        let (base_url, seen) = mock_server(vec![
            b"HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\n\
               connection: close\r\n\r\nslow down"
                .to_vec(),
            b"HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\n\
               connection: close\r\n\r\nbad key"
                .to_vec(),
        ]);
        let err = stream_once(&base_url).unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        assert_eq!(seen.load(Ordering::SeqCst), 2, "429 retried once, then the 401 stands");
    }
}
