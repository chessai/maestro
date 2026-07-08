//! Integration tests for M6 budgets & telemetry (ADR-003 lifetime ceilings,
//! ADR-001 routing telemetry).
//!
//! Drives the daemon server in-process against a real temp git repo with the
//! `"mock"` implementer + `"mock"` verifier at every tier. A config file in the
//! isolated `XDG_CONFIG_HOME` selects the profile via `MAESTRO_PROFILE`.
//!
//! Like the other daemon integration tests, `paths::*` + config resolution read
//! process-global env vars, so all exercises live in ONE `#[test]` to avoid
//! cross-test env races.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use maestro_daemon::{Options, Server};
use maestro_journal::domain::Tier;
use maestro_journal::proto::{Request, Response};
use maestro_journal::spec::{AcceptanceCriterion, CriterionKind, LifetimeBudget, TaskSpec};

fn unique_tmp() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "maestro-budget-test-{}-{}",
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

/// A Tier-0 spec whose mock implementer writes an allowlisted file, with NO
/// `mock:pass` criterion so the mock verifier fails every attempt. An optional
/// lifetime token budget drives the budget_exhausted path.
fn spec(write_path: &str, token_budget: Option<i64>) -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    let criteria = vec![AcceptanceCriterion {
        id: "AC1".into(),
        check: "the file exists".into(),
        kind: CriterionKind::Invariant,
    }];
    TaskSpec {
        title: "add a file".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec![write_path.into()],
        instructions,
        acceptance_criteria: criteria,
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: LifetimeBudget {
            tokens: token_budget,
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

fn event_kinds(task_id: &str) -> Vec<String> {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(task_id).expect("event chain");
    chain.into_iter().map(|e| e.kind.as_str().to_string()).collect()
}

#[test]
fn m6_budget_exhausted_routing_report_and_daily_total() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"
default_profile = "test"
[defaults]
concurrency.machine_cap = 4
[profiles.test]
roles.tier0 = "mock"
roles.tier1 = "mock"
roles.tier2 = "mock"
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

    // ---- AC4: a low lifetime_budget.tokens cuts the ladder short ----
    // The mock implementer bills tokens_in = 100 + content_bytes (~19) and
    // tokens_out = 20 per attempt, and the mock verifier bills 0. That is ~139
    // metered tokens per attempt. A ceiling of 200 is below 2 attempts' worth
    // (~278), so the task terminates budget_exhausted BEFORE the full 2+2+1
    // ladder (which would be 5 verify_failed).
    let budget_task = delegate(&socket, &advisor, &repo_path, spec("src/fail.rs", Some(200)));
    let state = poll_terminal(&socket, &advisor, &budget_task, Duration::from_secs(60));
    assert_eq!(state, "failed", "AC4: budget stop is terminal `failed`");

    let kinds = event_kinds(&budget_task);
    // The terminal event is `failed` with budget_exhausted / lifetime_tokens.
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let chain = journal.event_chain(&budget_task).unwrap();
        let failed = chain.last().unwrap();
        assert_eq!(failed.kind.as_str(), "failed", "AC4: terminal is failed");
        let payload: serde_json::Value =
            serde_json::from_str(failed.payload.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(payload["kind"], "budget_exhausted", "AC4: kind budget_exhausted");
        assert_eq!(payload["reason"], "lifetime_tokens", "AC4: reason lifetime_tokens");
    }
    // Fewer than 5 attempts ran: count verify_failed + checks_failed < 5.
    let attempt_failures = kinds
        .iter()
        .filter(|k| *k == "verify_failed" || *k == "checks_failed")
        .count();
    assert!(
        attempt_failures < 5,
        "AC4: ladder cut short, got {attempt_failures} attempt-failures in {kinds:?}"
    );

    // ---- AC5: routing_report is a non-empty array with the row shape ----
    let rr = round_trip(
        &socket,
        &Request::JournalQuery {
            advisor_session_id: advisor.clone(),
            query: "routing_report".into(),
            params: serde_json::Value::Null,
        },
    );
    match rr {
        Response::JournalResult { value } => {
            let arr = value.as_array().expect("routing_report is an array");
            assert!(!arr.is_empty(), "AC5: routing_report non-empty");
            let row = &arr[0];
            assert!(row.get("tier").is_some(), "AC5: row has tier");
            assert!(row.get("model").is_some(), "AC5: row has model");
            assert!(
                row.get("containment_level").is_some(),
                "AC5: row has containment_level"
            );
            assert!(
                row["total_tasks"].as_i64().unwrap() >= 1,
                "AC5: total_tasks >= 1"
            );
            assert!(row.get("tokens_in").is_some(), "AC5: row has tokens_in");
            assert!(row.get("tokens_out").is_some(), "AC5: row has tokens_out");
            // The budget_exhausted terminal is broken out under terminal_counts.
            let has_budget = arr.iter().any(|r| {
                r["terminal_counts"]
                    .get("failed:budget_exhausted")
                    .is_some()
            });
            assert!(has_budget, "AC5: failure kind surfaced in terminal_counts: {arr:?}");
        }
        other => panic!("AC5: expected JournalResult, got {other:?}"),
    }

    // ---- AC6: DrainInbox appends a `daily_total` advisory item ----
    // Sessions were written today by the budget task, so the drain appends one.
    let inbox = round_trip(
        &socket,
        &Request::DrainInbox {
            advisor_session_id: advisor.clone(),
        },
    );
    match inbox {
        Response::Inbox { items } => {
            let daily = items
                .iter()
                .find(|i| i.kind == "daily_total")
                .expect("AC6: daily_total item present");
            assert!(daily.event_id.is_empty(), "AC6: daily_total is synthetic (no event_id)");
            assert!(
                daily.summary.contains("tokens across"),
                "AC6: daily_total summary shape: {}",
                daily.summary
            );
        }
        other => panic!("AC6: expected Inbox, got {other:?}"),
    }

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
