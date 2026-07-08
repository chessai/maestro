//! Integration test for the LIVE streaming-credential-proxy budget chokepoint
//! (ADR-006 / ADR-004): with `[defaults].proxy.enabled = true`, the IMPLEMENTER's
//! Anthropic calls route through the daemon-local proxy tagged with
//! `X-Maestro-Task`; the proxy meters each response into the per-task ledger and
//! HARD-STOPS the task at its token ceiling by rejecting the NEXT request 429
//! `budget_exhausted`, which the daemon maps to a terminal `budget_exhausted`.
//!
//! A `tiny_http` mock upstream stands in for `api.anthropic.com` (pointed at via
//! `ANTHROPIC_BASE_URL`), returning a non-streaming `tool_use` response with a
//! large `usage` so the FIRST proxied turn crosses a small ceiling. The task
//! must terminate `budget_exhausted` — proving the proxy blocked the second turn.
//!
//! Like the other daemon integration tests, `paths::*` + config resolution read
//! process-global env vars, so all exercise lives in ONE `#[test]`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maestro_daemon::{Options, Server};
use maestro_journal::domain::Tier;
use maestro_journal::proto::{Request, Response};
use maestro_journal::spec::{AcceptanceCriterion, CriterionKind, LifetimeBudget, TaskSpec};

fn unique_tmp() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "maestro-proxy-budget-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() as u64
            ^ (Instant::now().elapsed().as_nanos() as u64).rotate_left(17)
    );
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn round_trip(socket: &Path, req: &Request) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect to daemon socket");
    let mut line = serde_json::to_string(req).expect("serialize request");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).expect("read response");
    serde_json::from_str(buf.trim_end()).expect("deserialize response")
}

fn wait_for_socket(socket: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("daemon socket never became connectable");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "tester"]);
    std::fs::write(repo.join("README.md"), "initial\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn poll_terminal(socket: &Path, advisor: &str, task_id: &str, timeout: Duration) -> String {
    const TERMINAL: &[&str] = &["verify_passed", "blocked", "failed", "merged"];
    let deadline = Instant::now() + timeout;
    loop {
        let resp = round_trip(
            socket,
            &Request::TaskStatus {
                advisor_session_id: advisor.to_string(),
                state: None,
            },
        );
        if let Response::TaskStatus { tasks } = resp {
            if let Some(row) = tasks.iter().find(|t| t.task_id == task_id) {
                if TERMINAL.contains(&row.state.as_str()) {
                    return row.state.clone();
                }
            }
        }
        if Instant::now() >= deadline {
            panic!("task {task_id} did not reach a terminal state in time");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Start a mock Anthropic upstream: every `POST /v1/messages` returns a
/// non-streaming `tool_use` response with a large `usage` (input 100), which the
/// proxy meters into the ledger. Returns the base URL and a request counter.
fn start_mock_upstream() -> (String, Arc<AtomicUsize>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let base = format!("http://{addr}");
    let count = Arc::new(AtomicUsize::new(0));
    let count_c = Arc::clone(&count);
    std::thread::spawn(move || {
        while let Ok(mut req) = server.recv() {
            count_c.fetch_add(1, Ordering::SeqCst);
            let mut _body = String::new();
            let _ = req.as_reader().read_to_string(&mut _body);
            // A tool_use turn with a LARGE usage so a single proxied turn crosses
            // a small ceiling. The write targets the allowlisted file so the loop
            // is well-formed even though it never gets a 2nd turn.
            let data = r#"{
                "type": "message",
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 100, "output_tokens": 0},
                "content": [
                    {"type":"tool_use","id":"tu_1","name":"write_file",
                     "input":{"path":"src/added.rs","content":"pub fn added() {}\n"}}
                ]
            }"#;
            let ct =
                tiny_http::Header::from_bytes(&b"content-type"[..], &b"application/json"[..])
                    .unwrap();
            let resp = tiny_http::Response::empty(200)
                .with_header(ct)
                .with_data(std::io::Cursor::new(data.as_bytes().to_vec()), Some(data.len()));
            let _ = req.respond(resp);
        }
    });
    (base, count)
}

fn spec(write_path: &str, token_budget: i64) -> TaskSpec {
    let criteria = vec![AcceptanceCriterion {
        id: "AC1".into(),
        check: "the file exists".into(),
        kind: CriterionKind::Invariant,
    }];
    TaskSpec {
        title: "add a file via proxied implementer".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec![write_path.into()],
        instructions: "add the file".into(),
        acceptance_criteria: criteria,
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: LifetimeBudget {
            tokens: Some(token_budget),
            wall_clock_minutes: None,
        },
        containment_min: 0,
    }
}

fn delegate(socket: &Path, advisor: &str, repo_path: &str, s: TaskSpec) -> String {
    match round_trip(
        socket,
        &Request::Delegate {
            advisor_session_id: advisor.to_string(),
            repo_path: repo_path.to_string(),
            spec: Box::new(s),
        },
    ) {
        Response::Delegate { task_id } => task_id,
        other => panic!("expected Delegate, got {other:?}"),
    }
}

#[test]
fn proxy_hard_stops_implementer_at_token_ceiling() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // The mock upstream stands in for api.anthropic.com; the proxy reads
    // ANTHROPIC_BASE_URL for its upstream, and the key it injects comes from
    // ANTHROPIC_API_KEY (a dummy — the mock upstream ignores it).
    let (upstream, upstream_count) = start_mock_upstream();
    std::env::set_var("ANTHROPIC_BASE_URL", &upstream);
    std::env::set_var("ANTHROPIC_API_KEY", "sk-dummy");

    // Enable the proxy + a NON-mock tier0 model so the implementer runs the real
    // Anthropic backend routed through the proxy. verifier_floor = mock so the
    // verifier tail (if ever reached) needs no key — but the task should stop at
    // the budget before any verify runs.
    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"
default_profile = "test"
[defaults]
concurrency.machine_cap = 4
proxy.enabled = true
proxy.addr = "127.0.0.1:0"
[profiles.test]
roles.tier0 = "claude-sonnet-4-6"
roles.verifier_floor = "mock"
"#,
    )
    .unwrap();
    std::env::set_var("MAESTRO_PROFILE", "test");

    let repo = init_repo(&tmp);
    let repo_path = repo.to_string_lossy().to_string();

    let server = Server::start(Options {
        profile: None,
        detach: false,
    })
    .expect("server starts");
    let socket = server.socket_path().to_path_buf();
    let shutdown = server.shutdown_handle();
    let handle = std::thread::spawn(move || server.serve_until().expect("serve loop"));
    wait_for_socket(&socket, Duration::from_secs(5));

    let advisor = match round_trip(
        &socket,
        &Request::RegisterAdvisor {
            profile: Some("test".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    // Ceiling 50; the first proxied turn meters input=100 (> 50), so the SECOND
    // turn's proxied request is rejected 429 budget_exhausted at the pre-forward
    // gate → the daemon maps that to a terminal budget_exhausted.
    let task = delegate(&socket, &advisor, &repo_path, spec("src/added.rs", 50));
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(60));
    assert_eq!(state, "failed", "task must terminate failed (budget stop)");

    // The terminal failure kind is budget_exhausted.
    let db_path = maestro_journal::paths::journal_db_path();
    let journal = maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
    let chain = journal.event_chain(&task).unwrap();
    let failed = chain.last().unwrap();
    assert_eq!(failed.kind.as_str(), "failed", "terminal is failed");
    let payload: serde_json::Value =
        serde_json::from_str(failed.payload.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(
        payload["kind"], "budget_exhausted",
        "terminal kind is budget_exhausted; chain: {:?}",
        chain.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );

    // The upstream was reached exactly ONCE (turn 1 forwarded; turn 2 blocked at
    // the proxy before forwarding). It must have been reached at least once (proof
    // the implementer really routed through the proxy to the upstream) and never
    // more than once (proof the ceiling hard-stopped the second turn).
    let n = upstream_count.load(Ordering::SeqCst);
    assert_eq!(
        n, 1,
        "upstream must be reached exactly once (turn 1 forwarded, turn 2 blocked at proxy), got {n}"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
