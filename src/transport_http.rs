//! OpenAI-compatible HTTP transport.
//!
//! Talks to any endpoint that implements the `/chat/completions` contract:
//! OpenAI, most OpenAI-compatible proxies, Ollama in `/v1` mode, llama.cpp,
//! and the Nous/Hermes gateway behind a proxy. It speaks **plaintext HTTP**
//! (`http://`) because `std` has no TLS. In production, front a TLS provider
//! with a local proxy (e.g. `nginx` or `mitmproxy`) and point `base_url` at
//! `http://127.0.0.1:PORT`. This keeps the crate dependency-free and offline-
//! buildable while still being real.

use crate::error::{AgentError, Result};
use crate::json::{self, Json};
use crate::message::Message;
use crate::transport::{parse_openai_message, tools_to_json, FinishReason, ProviderTransport, ToolSpec};

/// A transport that POSTs to an OpenAI-compatible `/chat/completions`.
pub struct HttpTransport {
    base_url: String,
    api_key: String,
    /// Optional path override; defaults to `/chat/completions`.
    chat_path: String,
}

impl HttpTransport {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            chat_path: String::from("/chat/completions"),
        }
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}{}", self.chat_path)
    }
}

impl ProviderTransport for HttpTransport {
    fn name(&self) -> &str {
        "openai-http"
    }

    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        model: &str,
    ) -> Result<crate::transport::ModelResponse> {
        let msg_json: Vec<Json> = messages.iter().map(Message::to_json).collect();
        let mut body_pairs = vec![
            (String::from("model"), Json::String(model.to_string())),
            (String::from("messages"), Json::Array(msg_json)),
            (String::from("temperature"), Json::Number(0.0)),
        ];
        if !tools.is_empty() {
            body_pairs.push((String::from("tools"), tools_to_json(tools)));
            body_pairs.push((
                String::from("tool_choice"),
                Json::String(String::from("auto")),
            ));
        }
        let body = Json::Object(body_pairs).to_string_compact();

        let response = http_post(&self.endpoint(), &self.api_key, &body)
            .map_err(AgentError::Transport)?;

        let parsed = json::parse(&response).map_err(AgentError::Json)?;
        let choice = parsed
            .get("choices")
            .and_then(Json::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| AgentError::Response("no choices in response".into()))?;
        let msg = choice
            .get("message")
            .cloned()
            .unwrap_or(Json::Null);

        // Carry finish_reason from the choice level into the message object so
        // parse_openai_message can read it.
        let mut msg = msg;
        if let Some(fr) = choice.get("finish_reason").and_then(Json::as_str) {
            if let Json::Object(pairs) = &mut msg {
                pairs.push((String::from("finish_reason"), Json::String(fr.to_string())));
            }
        }

        let mut resp = parse_openai_message(&msg)?;
        // If the model emitted tool_calls, force the finish reason regardless of
        // what the provider reported (some send "stop" with tool calls).
        if !resp.tool_calls.is_empty() {
            resp.finish_reason = FinishReason::ToolCalls;
        }
        Ok(resp)
    }
}

/// Perform a minimal `POST` with `Content-Type: application/json` and an
/// `Authorization: Bearer` header, using only `std::net` + a hand-rolled
/// HTTP/1.1 request. No TLS (see module docs).
fn http_post(url: &str, api_key: &str, body: &str) -> std::result::Result<String, String> {
    let (host, port, path, tls) = parse_url(url)?;
    if tls {
        return Err("TLS is not supported by the std-only transport; use an http:// endpoint or a local TLS-terminating proxy".into());
    }

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {api_key}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        path = path,
        host = host,
        api_key = api_key,
        len = body.len(),
        body = body,
    );

    use std::io::{Read, Write};
    use std::net::TcpStream;

    let stream = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("connect to {host}:{port}: {e}"))?;
    let mut stream = stream;
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read response: {e}"))?;
    let text = String::from_utf8_lossy(&raw);

    // Split headers from body on the first blank line.
    let idx = text.find("\r\n\r\n").unwrap_or(text.len());
    let body = &text[idx + 4..];

    // If the server sent chunked encoding, strip the chunk framing minimally.
    if text.contains("Transfer-Encoding: chunked") {
        Ok(decode_chunked(body))
    } else {
        Ok(body.to_string())
    }
}

/// Very small chunked-transfer decoder (handles the common case).
fn decode_chunked(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Read chunk size line.
        let line_end = bytes[i..]
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .map(|p| i + p)
            .unwrap_or(bytes.len());
        let size_line = String::from_utf8_lossy(&bytes[i..line_end]);
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        i = line_end;
        // Skip the CRLF after the size line.
        while i < bytes.len() && (bytes[i] == b'\r' || bytes[i] == b'\n') {
            i += 1;
        }
        out.extend_from_slice(&bytes[i..i + size]);
        i += size;
        // Skip trailing CRLF after chunk data.
        while i < bytes.len() && (bytes[i] == b'\r' || bytes[i] == b'\n') {
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `http://host[:port][/path]` (only `http` supported here).
fn parse_url(url: &str) -> std::result::Result<(String, u16, String, bool), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("url must include a scheme: {url}"))?;
    let tls = match scheme {
        "http" => false,
        "https" => true,
        other => return Err(format!("unsupported scheme '{other}' (use http)")),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), String::from("/")),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(80)),
        None => (authority.clone(), 80),
    };
    if host.is_empty() {
        return Err(format!("empty host in url: {url}"));
    }
    Ok((host, if tls { 443 } else { port }, path, tls))
}
