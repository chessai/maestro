//! The streaming credential proxy (ADR-006 / ADR-004).
//!
//! A daemon-local HTTP endpoint that sits between a sandboxed caller and the
//! upstream provider. It does three things the sandbox must not do itself:
//!
//! 1. **Credential injection.** The inbound request carries NO API key; the
//!    proxy injects `x-api-key` from a daemon-held provider only when forwarding
//!    upstream. Keys never enter the sandbox that calls the proxy.
//! 2. **Metering.** It parses the SSE stream and records prompt/output tokens
//!    per task into a shared [`Ledger`], streaming every frame through to the
//!    caller unchanged as it does so (it does not buffer the whole response).
//! 3. **Mid-stream hard-stop.** After each `message_delta`, if the task has
//!    crossed its token ceiling, it stops reading upstream, emits a final
//!    `budget_exhausted` SSE error event, and closes the connection — making
//!    `budget_exhausted` enforceable mid-response, not only at attempt
//!    boundaries.
//!
//! This crate is SYNCHRONOUS: `tiny_http` for the server, `ureq` for the
//! upstream call, std threads for the accept loop. It is OPT-IN and does not
//! change the default live delegation path.

mod ledger;

pub use ledger::Ledger;

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::JoinHandle;

use tiny_http::{Header, Method, Request, Response, Server};

/// A daemon-held provider of the upstream API key. Returns `None`/empty when no
/// key is available; the proxy then refuses to forward (401).
pub type KeyProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Static proxy configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// The upstream base URL, e.g. `https://api.anthropic.com`. The proxy POSTs
    /// to `{upstream_base}/v1/messages`.
    pub upstream_base: String,
    /// The default `anthropic-version` header used when the inbound request does
    /// not carry one.
    pub anthropic_version: String,
}

/// Header name (lowercased) carrying the metering key for a GATED caller (the
/// implementer): the task id. A request with this header is metered into the
/// ledger AND subject to the pre-forward budget gate + mid-stream hard-stop.
const TASK_HEADER: &str = "x-maestro-task";
/// Header name (lowercased) carrying the metering key for a METER-ONLY caller
/// (the verifier): the task id. A request with this header is metered into the
/// ledger just like `X-Maestro-Task`, but is NEVER pre-blocked or hard-stopped —
/// even when its task is already over budget (ADR-002 "verification never
/// skipped"). It exists so the per-task ledger reflects TOTAL task spend, making
/// the implementer's own pre-forward gate accurate.
const METER_HEADER: &str = "x-maestro-meter";
/// The SSE error event body written to the client on a mid-stream hard-stop.
const BUDGET_ERROR_EVENT: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"budget_exhausted\",\"message\":\"task token ceiling exceeded mid-stream\"}}\n\n";

/// Bind a `tiny_http::Server` on `addr` (e.g. `"127.0.0.1:0"` for an ephemeral
/// port), spawn the accept loop on a background thread, and return the bound
/// [`SocketAddr`] plus the server thread handle.
pub fn spawn(
    addr: &str,
    cfg: ProxyConfig,
    ledger: Arc<Ledger>,
    api_key: KeyProvider,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let server = Server::http(addr).map_err(|e| std::io::Error::other(e.to_string()))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| std::io::Error::other("no bound IP address"))?;
    let handle = std::thread::Builder::new()
        .name("maestro-proxy".into())
        .spawn(move || serve(server, cfg, ledger, api_key))?;
    Ok((bound, handle))
}

/// Serve requests forever on `server`. Each request is handled independently;
/// a handler error is logged and never kills the loop.
pub fn serve(server: Server, cfg: ProxyConfig, ledger: Arc<Ledger>, api_key: KeyProvider) -> ! {
    loop {
        match server.recv() {
            Ok(request) => {
                if let Err(e) = handle(request, &cfg, &ledger, &api_key) {
                    tracing::warn!(error = %e, "proxy request handler error");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "proxy server recv error");
            }
        }
    }
}

/// Dispatch a single request. Returns `Err` only for I/O failures while
/// responding (already logged upstream); routing/upstream errors are turned
/// into proper HTTP responses here.
fn handle(
    request: Request,
    cfg: &ProxyConfig,
    ledger: &Arc<Ledger>,
    api_key: &KeyProvider,
) -> std::io::Result<()> {
    // Route: only POST /v1/messages is handled.
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or(&url).to_string();
    if path != "/v1/messages" {
        return respond_json(request, 404, r#"{"error":"proxy: not found"}"#);
    }
    if *request.method() != Method::Post {
        return respond_json(request, 405, r#"{"error":"proxy: method not allowed"}"#);
    }

    // Pull the metering key + inbound anthropic-version before consuming the body.
    //
    // Two callers meter into the ledger under a task id:
    //  - `X-Maestro-Task` (the implementer) is GATED: metered AND subject to the
    //    pre-forward budget gate + the mid-stream hard-stop.
    //  - `X-Maestro-Meter` (the verifier) is METER-ONLY: metered so the ledger
    //    reflects total task spend, but NEVER pre-blocked or hard-stopped (the
    //    verifier must always run — ADR-002 "verification never skipped").
    // `X-Maestro-Task` wins when both are present. No header at all → pass-through
    // (forward + inject key, meter nothing).
    let task_header = header_value(&request, TASK_HEADER);
    let meter_key = task_header
        .clone()
        .or_else(|| header_value(&request, METER_HEADER));
    let gated = task_header.is_some();
    let anthropic_version =
        header_value(&request, "anthropic-version").unwrap_or_else(|| cfg.anthropic_version.clone());

    // Pre-forward budget gate: enforce the CUMULATIVE token ceiling BETWEEN
    // requests. The implementer is a multi-turn NON-streaming loop, so once a
    // turn's response pushes the ledger over the ceiling, the NEXT turn's request
    // is rejected HERE — without forwarding upstream. (The mid-stream hard-stop in
    // the streaming path complements this for streamed responses.) Applies ONLY to
    // a GATED (`X-Maestro-Task`) caller: a meter-only (`X-Maestro-Meter`) request
    // is never pre-blocked, EVEN when its task is over budget.
    if gated {
        if let Some(tid) = meter_key.as_deref() {
            if ledger.over_budget(tid) {
                return respond_json(
                    request,
                    429,
                    r#"{"error":{"type":"budget_exhausted","message":"task token ceiling already reached"}}"#,
                );
            }
        }
    }

    // Read the request body (JSON).
    let mut request = request;
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        return respond_json(
            request,
            400,
            &format!(r#"{{"error":"proxy: cannot read request body: {}"}}"#, esc(&e.to_string())),
        );
    }

    // Credential injection: never forward without a key.
    let key = match api_key() {
        Some(k) if !k.is_empty() => k,
        _ => return respond_json(request, 401, r#"{"error":"proxy: no upstream API key"}"#),
    };

    let streaming = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    let upstream_url = format!("{}/v1/messages", cfg.upstream_base.trim_end_matches('/'));

    if streaming {
        handle_streaming(
            request,
            &upstream_url,
            &key,
            &anthropic_version,
            body,
            meter_key,
            gated,
            ledger,
        )
    } else {
        handle_non_streaming(
            request,
            &upstream_url,
            &key,
            &anthropic_version,
            body,
            meter_key,
            ledger,
        )
    }
}

/// The streaming path: POST with `accept: text/event-stream`, then stream the
/// upstream SSE through a metering [`Read`] adapter that side-effects the ledger
/// and hard-stops mid-stream when the ceiling is crossed.
#[allow(clippy::too_many_arguments)]
fn handle_streaming(
    request: Request,
    upstream_url: &str,
    key: &str,
    anthropic_version: &str,
    body: String,
    meter_key: Option<String>,
    gated: bool,
    ledger: &Arc<Ledger>,
) -> std::io::Result<()> {
    let resp = ureq::post(upstream_url)
        .set("x-api-key", key)
        .set("anthropic-version", anthropic_version)
        .set("accept", "text/event-stream")
        .set("content-type", "application/json")
        .send_string(&body);

    let upstream = match resp {
        Ok(r) => r,
        // ureq surfaces non-2xx as Err(Status); forward the upstream status +
        // body if we have it, else a transport 502.
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return respond_raw(request, code, "application/json", text.into_bytes());
        }
        Err(e) => {
            return respond_json(
                request,
                502,
                &format!(r#"{{"error":"proxy: upstream transport error: {}"}}"#, esc(&e.to_string())),
            );
        }
    };

    let reader = upstream.into_reader();
    let meter = MeteringReader::new(reader, meter_key, gated, Arc::clone(ledger));

    let response = Response::empty(200)
        .with_header(header("content-type", "text/event-stream"))
        .with_header(header("cache-control", "no-cache"))
        .with_data(meter, None);
    request.respond(response)
}

/// The non-streaming path: forward, read the full JSON, record end-of-response
/// usage, and forward the body + status to the client.
#[allow(clippy::too_many_arguments)]
fn handle_non_streaming(
    request: Request,
    upstream_url: &str,
    key: &str,
    anthropic_version: &str,
    body: String,
    meter_key: Option<String>,
    ledger: &Arc<Ledger>,
) -> std::io::Result<()> {
    let resp = ureq::post(upstream_url)
        .set("x-api-key", key)
        .set("anthropic-version", anthropic_version)
        .set("content-type", "application/json")
        .send_string(&body);

    let (status, text) = match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => {
            return respond_json(
                request,
                502,
                &format!(r#"{{"error":"proxy: upstream transport error: {}"}}"#, esc(&e.to_string())),
            );
        }
    };

    // Meter end-of-response usage if we can parse it and have a metering key
    // (gated `X-Maestro-Task` OR meter-only `X-Maestro-Meter`).
    if let Some(tid) = meter_key.as_deref() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(usage) = v.get("usage") {
                let ti = usage.get("input_tokens").and_then(|n| n.as_i64()).unwrap_or(0);
                let to = usage.get("output_tokens").and_then(|n| n.as_i64()).unwrap_or(0);
                ledger.add_usage(tid, ti, to);
            }
        }
    }

    respond_raw(request, status, "application/json", text.into_bytes())
}

/// A `Read` adapter that pulls the upstream SSE stream line by line, meters each
/// `data:` frame into the ledger, forwards every raw line downstream unchanged,
/// and — after a `message_delta` that pushes the task over budget — appends a
/// final `budget_exhausted` error event and then reports EOF (dropping the
/// upstream reader, cutting the upstream connection).
///
/// `tiny_http` drives this reader: it repeatedly calls `read` and writes the
/// bytes to the client socket, so metering happens as the client drains the
/// stream — nothing is buffered whole.
struct MeteringReader<R: Read> {
    upstream: Option<BufReader<R>>,
    /// The metering key (task id) this response accumulates into, if any. Set for
    /// both a gated (`X-Maestro-Task`) and a meter-only (`X-Maestro-Meter`) caller.
    meter_key: Option<String>,
    /// Whether this caller is subject to the mid-stream hard-stop. TRUE only for a
    /// gated (`X-Maestro-Task`) caller; a meter-only caller meters but is never
    /// cut off (ADR-002 "verification never skipped").
    gated: bool,
    ledger: Arc<Ledger>,
    /// Last seen cumulative `output_tokens` for this response (for delta calc).
    last_output: i64,
    /// Bytes ready to hand to the caller (a forwarded line, or the final error
    /// event), consumed front-to-back across `read` calls.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Once set, no more upstream is read; drain `pending`, then EOF.
    finished: bool,
}

impl<R: Read> MeteringReader<R> {
    fn new(upstream: R, meter_key: Option<String>, gated: bool, ledger: Arc<Ledger>) -> Self {
        MeteringReader {
            upstream: Some(BufReader::new(upstream)),
            meter_key,
            gated,
            ledger,
            last_output: 0,
            pending: Vec::new(),
            pending_pos: 0,
            finished: false,
        }
    }

    /// Meter a single SSE `data:` JSON line. Returns `true` if this frame pushed
    /// the task over budget (⇒ hard-stop).
    fn meter_line(&mut self, line: &str) -> bool {
        let data = match line.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => return false,
        };
        let json: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let Some(tid) = self.meter_key.clone() else {
            return false;
        };
        match json.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                // Prompt tokens: record the input usage once (delta from 0).
                if let Some(input) = json
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|n| n.as_i64())
                {
                    self.ledger.add_usage(&tid, input, 0);
                }
                false
            }
            Some("message_delta") => {
                // Cumulative output_tokens → additive delta vs last seen.
                if let Some(cum) = json
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|n| n.as_i64())
                {
                    let delta = cum - self.last_output;
                    self.last_output = cum;
                    if delta != 0 {
                        self.ledger.add_usage(&tid, 0, delta);
                    }
                }
                // The hard-stop applies ONLY to a gated caller. A meter-only
                // caller meters this delta but is never cut off.
                self.gated && self.ledger.over_budget(&tid)
            }
            _ => false,
        }
    }

    /// Pull the next upstream line into `pending` (forwarding it), metering as we
    /// go. Sets `finished` at EOF or on a hard-stop (appending the error event).
    fn fill_pending(&mut self) {
        let Some(reader) = self.upstream.as_mut() else {
            self.finished = true;
            return;
        };
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // Upstream EOF.
                self.upstream = None;
                self.finished = true;
            }
            Ok(_) => {
                let over = self.meter_line(&line);
                // Forward the raw line downstream unchanged.
                self.pending.extend_from_slice(line.as_bytes());
                self.pending_pos = 0;
                if over {
                    // HARD-STOP: drop upstream, append the final error event.
                    self.upstream = None;
                    self.pending.extend_from_slice(BUDGET_ERROR_EVENT.as_bytes());
                    self.finished = true;
                }
            }
            Err(_) => {
                self.upstream = None;
                self.finished = true;
            }
        }
    }
}

impl<R: Read> Read for MeteringReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Drain any pending bytes first.
            if self.pending_pos < self.pending.len() {
                let n = (self.pending.len() - self.pending_pos).min(buf.len());
                buf[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
                self.pending_pos += n;
                if self.pending_pos >= self.pending.len() {
                    self.pending.clear();
                    self.pending_pos = 0;
                }
                return Ok(n);
            }
            if self.finished {
                return Ok(0);
            }
            self.fill_pending();
        }
    }
}

// --- small HTTP helpers ---

/// Read a request header value by (case-insensitive) name.
fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header is valid")
}

/// Respond with a JSON body and a status code.
fn respond_json(request: Request, status: u16, body: &str) -> std::io::Result<()> {
    respond_raw(request, status, "application/json", body.as_bytes().to_vec())
}

/// Respond with a raw body, content-type, and status code.
fn respond_raw(
    request: Request,
    status: u16,
    content_type: &str,
    body: Vec<u8>,
) -> std::io::Result<()> {
    let len = body.len();
    let response = Response::empty(status)
        .with_header(header("content-type", content_type))
        .with_data(std::io::Cursor::new(body), Some(len));
    request.respond(response)
}

/// Escape a string for embedding inside a JSON string literal (quotes +
/// backslashes; good enough for error messages).
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
