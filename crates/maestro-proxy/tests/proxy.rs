//! End-to-end tests for the streaming credential proxy against a MOCK upstream
//! (a second `tiny_http::Server` on an ephemeral port). No live network.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use maestro_proxy::{Ledger, ProxyConfig};

/// A canned SSE stream: message_start (input 10), message_delta out=5,20,60,120,
/// then message_stop. `\n`-delimited SSE lines.
fn canned_sse() -> String {
    let mut s = String::new();
    s.push_str("event: message_start\n");
    s.push_str("data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n");
    for out in [5, 20, 60, 120] {
        s.push_str("event: message_delta\n");
        s.push_str(&format!(
            "data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":{out}}}}}\n\n"
        ));
    }
    s.push_str("event: message_stop\n");
    s.push_str("data: {\"type\":\"message_stop\"}\n\n");
    s
}

/// A record of what the mock upstream saw.
#[derive(Default)]
struct Seen {
    api_key: Option<String>,
    anthropic_version: Option<String>,
}

/// Start a mock upstream that asserts `x-api-key` presence and returns either a
/// canned SSE stream or a canned JSON body. Returns the base URL, a shared
/// `Seen` record, and a flag set true once at least one request was served.
fn start_mock_upstream(sse: bool) -> (String, Arc<Mutex<Seen>>, Arc<AtomicBool>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let base = format!("http://{addr}");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let served = Arc::new(AtomicBool::new(false));

    let seen_c = Arc::clone(&seen);
    let served_c = Arc::clone(&served);
    std::thread::spawn(move || {
        // Serve exactly one request (enough for each test).
        if let Ok(mut req) = server.recv() {
            let key = req
                .headers()
                .iter()
                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-api-key"))
                .map(|h| h.value.as_str().to_string());
            let ver = req
                .headers()
                .iter()
                .find(|h| {
                    h.field
                        .as_str()
                        .as_str()
                        .eq_ignore_ascii_case("anthropic-version")
                })
                .map(|h| h.value.as_str().to_string());
            {
                let mut g = seen_c.lock().unwrap();
                g.api_key = key;
                g.anthropic_version = ver;
            }
            let mut _body = String::new();
            let _ = req.as_reader().read_to_string(&mut _body);
            served_c.store(true, Ordering::SeqCst);

            if sse {
                let data = canned_sse();
                let ct = tiny_http::Header::from_bytes(
                    &b"content-type"[..],
                    &b"text/event-stream"[..],
                )
                .unwrap();
                let len = data.len();
                let resp = tiny_http::Response::empty(200)
                    .with_header(ct)
                    .with_data(std::io::Cursor::new(data.into_bytes()), Some(len));
                let _ = req.respond(resp);
            } else {
                let data = r#"{"type":"message","usage":{"input_tokens":7,"output_tokens":9}}"#;
                let ct = tiny_http::Header::from_bytes(
                    &b"content-type"[..],
                    &b"application/json"[..],
                )
                .unwrap();
                let resp = tiny_http::Response::empty(200)
                    .with_header(ct)
                    .with_data(std::io::Cursor::new(data.as_bytes().to_vec()), Some(data.len()));
                let _ = req.respond(resp);
            }
        }
    });

    (base, seen, served)
}

/// A key provider that always returns the given key.
fn key_provider(key: &'static str) -> maestro_proxy::KeyProvider {
    Arc::new(move || Some(key.to_string()))
}

/// POST a body through the proxy at `proxy_addr` with an optional task header,
/// returning (status, body-string). Uses a raw ureq call so we can set headers.
fn post_through_proxy(proxy_addr: &str, body: &str, task: Option<&str>) -> (u16, String) {
    let url = format!("http://{proxy_addr}/v1/messages");
    let mut req = ureq::post(&url).set("content-type", "application/json");
    if let Some(t) = task {
        req = req.set("X-Maestro-Task", t);
    }
    match req.send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("transport error posting through proxy: {e}"),
    }
}

/// POST a body through the proxy with an `X-Maestro-Meter` (meter-only) header,
/// returning (status, body-string).
fn post_through_proxy_meter(proxy_addr: &str, body: &str, meter: &str) -> (u16, String) {
    let url = format!("http://{proxy_addr}/v1/messages");
    let req = ureq::post(&url)
        .set("content-type", "application/json")
        .set("X-Maestro-Meter", meter);
    match req.send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("transport error posting through proxy: {e}"),
    }
}

fn cfg(upstream_base: &str) -> ProxyConfig {
    ProxyConfig {
        upstream_base: upstream_base.to_string(),
        anthropic_version: "2023-06-01".to_string(),
    }
}

// Test 1: metering — a task with no ceiling drains the full stream and the
// ledger ends at input=10, output=120.
#[test]
fn streaming_meters_full_usage() {
    let (upstream, seen, served) = start_mock_upstream(true);
    let ledger = Arc::new(Ledger::new());
    ledger.register("task-1", None);
    let (proxy_addr, _h) =
        maestro_proxy::spawn("127.0.0.1:0", cfg(&upstream), Arc::clone(&ledger), key_provider("sk-test"))
            .unwrap();

    let (status, body) =
        post_through_proxy(&proxy_addr.to_string(), r#"{"stream":true}"#, Some("task-1"));
    assert_eq!(status, 200, "body was: {body}");

    // The full stream drained → ledger at (10, 120).
    assert_eq!(ledger.spent("task-1"), (10, 120));
    assert!(!ledger.over_budget("task-1"));

    // Credential injection worked; the inbound version was forwarded.
    assert!(served.load(Ordering::SeqCst));
    let g = seen.lock().unwrap();
    assert_eq!(g.api_key.as_deref(), Some("sk-test"));
    assert_eq!(g.anthropic_version.as_deref(), Some("2023-06-01"));

    // The client saw a normal SSE stream (message_start ... message_stop).
    assert!(body.contains("message_start"));
    assert!(body.contains("message_stop"));
}

// Test 2: mid-stream hard-stop — a ceiling of 100 is crossed once cumulative
// (10 + output) reaches >= 100. Output crosses at 120 (10+120=130 >= 100 at the
// out=120 delta; and 10+60=70 < 100, so the stop fires at the 120 delta). The
// client stream ends with a budget_exhausted error event and over_budget is true.
#[test]
fn streaming_hard_stops_over_budget() {
    let (upstream, _seen, _served) = start_mock_upstream(true);
    let ledger = Arc::new(Ledger::new());
    ledger.register("task-2", Some(100));
    let (proxy_addr, _h) =
        maestro_proxy::spawn("127.0.0.1:0", cfg(&upstream), Arc::clone(&ledger), key_provider("sk-test"))
            .unwrap();

    let (status, body) =
        post_through_proxy(&proxy_addr.to_string(), r#"{"stream":true}"#, Some("task-2"));
    assert_eq!(status, 200);

    // The final error event was appended and the stream was cut before stop.
    assert!(
        body.contains("budget_exhausted"),
        "client stream must end with a budget_exhausted error event; body: {body}"
    );
    assert!(
        body.contains("task token ceiling exceeded mid-stream"),
        "error event message present; body: {body}"
    );
    // Upstream was cut: message_stop never reached the client.
    assert!(
        !body.contains("message_stop"),
        "hard-stop must cut the stream before message_stop; body: {body}"
    );
    assert!(ledger.over_budget("task-2"));
    // The delta that crossed 100 was recorded (10 in, 120 out at the crossing).
    assert_eq!(ledger.spent("task-2"), (10, 120));
}

// Test 3: no key — the proxy responds 401 and nothing is forwarded upstream.
#[test]
fn no_key_responds_401() {
    let (upstream, _seen, served) = start_mock_upstream(true);
    let ledger = Arc::new(Ledger::new());
    let no_key: maestro_proxy::KeyProvider = Arc::new(|| None);
    let (proxy_addr, _h) =
        maestro_proxy::spawn("127.0.0.1:0", cfg(&upstream), Arc::clone(&ledger), no_key).unwrap();

    let (status, body) =
        post_through_proxy(&proxy_addr.to_string(), r#"{"stream":true}"#, Some("task-3"));
    assert_eq!(status, 401);
    assert!(body.contains("no upstream API key"), "body: {body}");

    // Give any (erroneously) forwarded request a beat to arrive, then assert
    // the upstream saw nothing.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !served.load(Ordering::SeqCst),
        "nothing must be forwarded upstream when there is no key"
    );
}

// Test 4: non-streaming — the plain JSON usage is recorded (7, 9) and the client
// receives the JSON body.
#[test]
fn non_streaming_meters_end_usage() {
    let (upstream, seen, _served) = start_mock_upstream(false);
    let ledger = Arc::new(Ledger::new());
    ledger.register("task-4", None);
    let (proxy_addr, _h) =
        maestro_proxy::spawn("127.0.0.1:0", cfg(&upstream), Arc::clone(&ledger), key_provider("sk-test"))
            .unwrap();

    let (status, body) =
        post_through_proxy(&proxy_addr.to_string(), r#"{"stream":false}"#, Some("task-4"));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"usage\""), "client gets the JSON body; body: {body}");
    assert_eq!(ledger.spent("task-4"), (7, 9));

    let g = seen.lock().unwrap();
    assert_eq!(g.api_key.as_deref(), Some("sk-test"));
}

// Pre-forward budget gate: a task already at (or over) its ceiling is rejected
// with 429 budget_exhausted WITHOUT forwarding upstream. The mock upstream would
// return 500 if it were reached, proving the request never left the proxy.
#[test]
fn over_budget_rejected_before_forwarding() {
    // A mock upstream that returns 500 if it is ever reached.
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let upstream = format!("http://{addr}");
    let reached = Arc::new(AtomicBool::new(false));
    let reached_c = Arc::clone(&reached);
    std::thread::spawn(move || {
        if let Ok(req) = server.recv() {
            reached_c.store(true, Ordering::SeqCst);
            let resp = tiny_http::Response::empty(500);
            let _ = req.respond(resp);
        }
    });

    let ledger = Arc::new(Ledger::new());
    // Low ceiling, then push usage past it so the task is already over budget.
    ledger.register("task-over", Some(50));
    ledger.add_usage("task-over", 40, 20); // 60 >= 50 → over budget
    assert!(ledger.over_budget("task-over"));

    let (proxy_addr, _h) = maestro_proxy::spawn(
        "127.0.0.1:0",
        cfg(&upstream),
        Arc::clone(&ledger),
        key_provider("sk-test"),
    )
    .unwrap();

    let (status, body) =
        post_through_proxy(&proxy_addr.to_string(), r#"{"stream":false}"#, Some("task-over"));
    assert_eq!(status, 429, "over-budget task must be rejected 429; body: {body}");
    assert!(
        body.contains("budget_exhausted"),
        "429 body carries budget_exhausted; body: {body}"
    );
    assert!(
        body.contains("task token ceiling already reached"),
        "429 body carries the pre-forward message; body: {body}"
    );

    // Give any (erroneously) forwarded request a beat to arrive, then assert the
    // upstream was NEVER reached.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !reached.load(Ordering::SeqCst),
        "an over-budget request must NOT be forwarded upstream"
    );
}

// Routing: a non-/v1/messages path is 404; a GET is 405.
#[test]
fn routing_404_and_405() {
    let (upstream, _seen, _served) = start_mock_upstream(false);
    let ledger = Arc::new(Ledger::new());
    let (proxy_addr, _h) =
        maestro_proxy::spawn("127.0.0.1:0", cfg(&upstream), Arc::clone(&ledger), key_provider("sk-test"))
            .unwrap();

    // 404 wrong path.
    let url = format!("http://{proxy_addr}/nope");
    let (code, _) = match ureq::post(&url).send_string("{}") {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(code, 404);

    // 405 wrong method on the right path.
    let url = format!("http://{proxy_addr}/v1/messages");
    let (code, _) = match ureq::get(&url).call() {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(code, 405);
}

// Meter-only (`X-Maestro-Meter`, the verifier): a request whose task is ALREADY
// over budget is STILL forwarded (NOT 429) and its usage accumulates into the
// ledger. This is ADR-002 "verification never skipped": the verifier meters into
// the per-task ledger so total spend is accurate, but is never pre-blocked.
#[test]
fn meter_only_over_budget_still_forwarded_and_metered() {
    let (upstream, seen, served) = start_mock_upstream(false);
    let ledger = Arc::new(Ledger::new());
    // A low ceiling, already crossed → the task is over budget.
    ledger.register("task-meter", Some(50));
    ledger.add_usage("task-meter", 40, 20); // 60 >= 50 → over budget
    assert!(ledger.over_budget("task-meter"));

    let (proxy_addr, _h) = maestro_proxy::spawn(
        "127.0.0.1:0",
        cfg(&upstream),
        Arc::clone(&ledger),
        key_provider("sk-test"),
    )
    .unwrap();

    let (status, body) = post_through_proxy_meter(
        &proxy_addr.to_string(),
        r#"{"stream":false}"#,
        "task-meter",
    );
    // NOT pre-blocked: forwarded upstream (200), never 429.
    assert_eq!(
        status, 200,
        "an over-budget meter-only request must still be forwarded; body: {body}"
    );
    assert!(body.contains("\"usage\""), "client gets the JSON body; body: {body}");

    // The upstream WAS reached (proving no pre-forward block), the key was
    // injected, and the response usage (7, 9) accumulated ON TOP of the prior
    // (40, 20) → (47, 29).
    assert!(served.load(Ordering::SeqCst), "meter-only request must reach upstream");
    let g = seen.lock().unwrap();
    assert_eq!(g.api_key.as_deref(), Some("sk-test"));
    drop(g);
    assert_eq!(ledger.spent("task-meter"), (47, 29));
}

// Regression guard for the GATED gate (`X-Maestro-Task`, the implementer): a task
// already over its ceiling is rejected 429 budget_exhausted WITHOUT forwarding
// upstream. Complements `over_budget_rejected_before_forwarding`; here the mock
// upstream would return 200 with usage if reached, so the (0,0)-vs-forwarded
// distinction proves the request never left the proxy.
#[test]
fn gated_over_budget_rejected_429() {
    let (upstream, _seen, served) = start_mock_upstream(false);
    let ledger = Arc::new(Ledger::new());
    ledger.register("task-gated", Some(50));
    ledger.add_usage("task-gated", 40, 20); // 60 >= 50 → over budget
    assert!(ledger.over_budget("task-gated"));

    let (proxy_addr, _h) = maestro_proxy::spawn(
        "127.0.0.1:0",
        cfg(&upstream),
        Arc::clone(&ledger),
        key_provider("sk-test"),
    )
    .unwrap();

    let (status, body) = post_through_proxy(
        &proxy_addr.to_string(),
        r#"{"stream":false}"#,
        Some("task-gated"),
    );
    assert_eq!(status, 429, "over-budget gated task must be 429; body: {body}");
    assert!(
        body.contains("budget_exhausted"),
        "429 body carries budget_exhausted; body: {body}"
    );

    // Upstream never reached; the ledger is untouched by the blocked request.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !served.load(Ordering::SeqCst),
        "a gated over-budget request must NOT be forwarded upstream"
    );
    assert_eq!(ledger.spent("task-gated"), (40, 20));
}
