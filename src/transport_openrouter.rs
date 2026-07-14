//! OpenRouter transport — the zero-dependency TLS bridge.
//!
//! `std` has no TLS, so the crate's HTTP transport only speaks plaintext
//! `http://`. OpenRouter (like every real provider) is HTTPS-only. Rather than
//! pull in an TLS crate (which would break the dependency-free, offline-build
//! guarantee), this transport spawns a *tiny pure-stdlib* `python3` proxy that
//! terminates TLS to OpenRouter and exposes a plaintext `http://127.0.0.1:PORT`
//! endpoint that [`super::transport_http::HttpTransport`] already speaks.
//!
//! The proxy is embedded in the binary (`PROXY_PY`), written to a temp file,
//! spawned as a child process, and force-killed on `Drop`. This keeps the whole
//! thing one command: `grace --openrouter --model ... --prompt ...`.

use crate::error::{AgentError, Result};
use crate::transport_http::HttpTransport;
use std::process::{Child, Command, Stdio};

/// A minimal HTTP CONNECT-free reverse proxy: it accepts a plaintext POST on
/// a local port, forwards it to `https://openrouter.ai` over TLS (python's
/// `urllib` does the TLS), and streams the response body straight back. Only
/// headers we forward are Content-Type/Content-Length/Authorization; the
/// proxy injects nothing else. This is exactly the "local TLS-terminating
/// proxy" pattern the README recommends, expressed in ~40 lines of stdlib.
const PROXY_PY: &str = r#"
import http.server, socketserver, urllib.request, sys

UPSTREAM = "https://openrouter.ai"

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        url = UPSTREAM + self.path
        req = urllib.request.Request(url, data=body, method="POST")
        for h in ("Authorization", "Content-Type"):
            if h in self.headers:
                req.add_header(h, self.headers[h])
        try:
            r = urllib.request.urlopen(req, timeout=120)
            data = r.read()
            self.send_response(r.status)
            self.send_header("Content-Type", r.headers.get("Content-Type", "application/json"))
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except urllib.error.HTTPError as e:
            data = e.read()
            self.send_response(e.code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except Exception as e:
            msg = str(e).encode()
            self.send_response(502)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(msg)))
            self.end_headers()
            self.wfile.write(msg)
    def log_message(self, *a):
        pass

port = int(sys.argv[1])
with socketserver.TCPServer(("127.0.0.1", port), H) as httpd:
    httpd.serve_forever()
"#;

/// Pick a free ephemeral TCP port by binding, reading, then closing.
fn free_port() -> Result<u16> {
    use std::net::TcpListener;
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| AgentError::Transport(format!("bind: {e}")))?;
    let port = l.local_addr().map(|a| a.port()).map_err(|e| AgentError::Transport(format!("addr: {e}")))?;
    Ok(port)
}

/// A transport that proxies to OpenRouter through a spawned python3 TLS bridge.
pub struct OpenRouterTransport {
    inner: HttpTransport,
    child: Child,
    /// Held so we can report / debug; not read after spawn.
    _port: u16,
}

impl OpenRouterTransport {
    /// `api_key` is the OpenRouter bearer token. `model` is the OpenRouter
    /// model id (e.g. `openai/gpt-4o-mini`, `anthropic/claude-3.5-sonnet`).
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let api_key = api_key.into();
        let model = model.into();
        if api_key.is_empty() {
            return Err(AgentError::Config(
                "OpenRouter requires an API key: pass --api-key or set OPENROUTER_API_KEY".into(),
            ));
        }
        if model.is_empty() {
            return Err(AgentError::Config(
                "OpenRouter requires a model id: pass --model <provider/model>".into(),
            ));
        }

        let port = free_port()?;
        // Write the embedded proxy to a temp file (cleaned by the OS on reboot;
        // we kill the child on Drop anyway).
        let proxy_path = std::env::temp_dir().join(format!("grace_openrouter_proxy_{port}.py"));
        std::fs::write(&proxy_path, PROXY_PY)
            .map_err(|e| AgentError::Transport(format!("write proxy: {e}")))?;

        let child = Command::new("python3")
            .arg(proxy_path.to_string_lossy().as_ref())
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                AgentError::Transport(format!(
                    "failed to spawn python3 proxy (need python3 >=3.7 on PATH): {e}"
                ))
            })?;

        // Wait until the proxy is accepting connections (python startup <100ms).
        let mut ok = false;
        for _ in 0..40 {
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(50),
            )
            .is_ok()
            {
                ok = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !ok {
            return Err(AgentError::Transport(
                "OpenRouter TLS proxy did not become ready (is python3 reachable?)".into(),
            ));
        }

        // The inner transport targets the local plaintext endpoint and owns the
        // OpenRouter model + key.
        let base = format!("http://127.0.0.1:{port}/api/v1");
        let inner = HttpTransport::with_model(base, api_key, model);
        Ok(Self {
            inner,
            child,
            _port: port,
        })
    }
}

impl Drop for OpenRouterTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl crate::transport::ProviderTransport for OpenRouterTransport {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn complete(
        &self,
        messages: &[crate::message::Message],
        tools: &[crate::transport::ToolSpec],
        _model: &str,
    ) -> Result<crate::transport::ModelResponse> {
        // The model is owned by the inner transport (set at construction).
        self.inner.complete(messages, tools, "")
    }
}
