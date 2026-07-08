//! Integration tests for the M2 verification + escalation loop (AC4–AC7).
//!
//! Drives the daemon server in-process against a real temp git repo with the
//! `"mock"` implementer + `"mock"` verifier backends. A config file in the
//! isolated `XDG_CONFIG_HOME` forces a profile whose tier0/tier1/tier2 and
//! `verifier_floor` are all `"mock"`, selected via `MAESTRO_PROFILE`.
//!
//! NOTE: like the M1 delegate test, `paths::*` and config resolution read
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
use maestro_journal::spec::{AcceptanceCriterion, CriterionKind, TaskSpec};

fn unique_tmp() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "maestro-verify-test-{}-{}",
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

/// Poll `task_status` until the task reaches a terminal state or times out.
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

fn branch_exists(repo: &Path, task_id: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", &format!("maestro/{task_id}")])
        .output()
        .expect("spawn git rev-parse")
        .status
        .success()
}

fn rev(repo: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A Tier-0 spec whose mock implementer writes an allowlisted file.
/// `pass` controls whether a `mock:pass` acceptance criterion is present (so the
/// mock verifier passes) or absent (so it fails every attempt).
fn spec(base_ref: &str, write_path: &str, pass: bool) -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    let mut criteria = vec![AcceptanceCriterion {
        id: "AC1".into(),
        check: "the file exists".into(),
        kind: CriterionKind::Invariant,
    }];
    if pass {
        criteria.push(AcceptanceCriterion {
            id: "AC2".into(),
            check: "mock:pass".into(),
            kind: CriterionKind::Invariant,
        });
    }
    TaskSpec {
        title: "add a file".into(),
        tier: Tier::T0,
        base_ref: base_ref.into(),
        file_allowlist: vec![write_path.into()],
        instructions,
        acceptance_criteria: criteria,
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
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

/// Read a task's event kind chain straight from the journal DB (WAL allows a
/// concurrent reader while the daemon holds the writer).
fn event_kinds(task_id: &str) -> Vec<String> {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(task_id).expect("event chain");
    chain
        .into_iter()
        .map(|e| e.kind.as_str().to_string())
        .collect()
}

#[test]
fn m2_verify_escalation_and_close_and_query() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // Force mock implementer at every tier + a mock verifier floor.
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
    let base_commit = rev(&repo, "HEAD");
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

    // ---- AC4: no mock:pass → verifier fails every attempt → full ladder ----
    let blocked_task = delegate(&socket, &advisor, &repo_path, spec("HEAD", "src/fail.rs", false));
    let state = poll_terminal(&socket, &advisor, &blocked_task, Duration::from_secs(60));
    assert_eq!(state, "blocked", "AC4: top-tier verify failure → blocked");

    let kinds = event_kinds(&blocked_task);
    // Assert the escalation chain: 2 verify_failed @ t0 → escalated → 2 @ t1 →
    // escalated → 1 @ t2 → blocked.
    let verify_failed_count = kinds.iter().filter(|k| *k == "verify_failed").count();
    assert_eq!(verify_failed_count, 5, "AC4: 5 verify_failed events, got {kinds:?}");
    let escalated: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| *k == "escalated")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(escalated.len(), 2, "AC4: exactly two escalations, got {kinds:?}");
    // Ordering: [.. vf vf escalated .. vf vf escalated .. vf blocked]
    let vf_before_first_escalation = kinds[..escalated[0]]
        .iter()
        .filter(|k| *k == "verify_failed")
        .count();
    assert_eq!(vf_before_first_escalation, 2, "AC4: two vf before first escalation");
    let vf_between = kinds[escalated[0]..escalated[1]]
        .iter()
        .filter(|k| *k == "verify_failed")
        .count();
    assert_eq!(vf_between, 2, "AC4: two vf between escalations");
    assert_eq!(kinds.last().map(String::as_str), Some("blocked"), "AC4: ends blocked");

    // Assert the escalation payloads carry from_tier/to_tier 0→1 and 1→2.
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let chain = journal.event_chain(&blocked_task).unwrap();
        let esc: Vec<serde_json::Value> = chain
            .iter()
            .filter(|e| matches!(e.kind, maestro_journal::domain::EventKind::Escalated))
            .map(|e| serde_json::from_str(e.payload.as_deref().unwrap_or("{}")).unwrap())
            .collect();
        assert_eq!(esc[0]["from_tier"], 0);
        assert_eq!(esc[0]["to_tier"], 1);
        assert_eq!(esc[1]["from_tier"], 1);
        assert_eq!(esc[1]["to_tier"], 2);
    }

    // Report chain of length 5 via the reader.
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let reports = journal.verifier_reports_for_task(&blocked_task).unwrap();
        assert_eq!(reports.len(), 5, "AC4: five verifier reports");
        // attempts are 1..=5 in order.
        let attempts: Vec<i64> = reports.iter().map(|r| r.attempt).collect();
        assert_eq!(attempts, vec![1, 2, 3, 4, 5], "AC4: attempts increase 1..5");
    }

    // ---- AC5: with mock:pass → verify_passed on the FIRST attempt ----
    let pass_task = delegate(&socket, &advisor, &repo_path, spec("HEAD", "src/ok.rs", true));
    let state = poll_terminal(&socket, &advisor, &pass_task, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "AC5: mock:pass → verify_passed");
    let kinds = event_kinds(&pass_task);
    assert_eq!(
        kinds.iter().filter(|k| *k == "verify_failed").count(),
        0,
        "AC5: no verify_failed on the pass path, got {kinds:?}"
    );
    assert!(kinds.iter().any(|k| k == "verify_passed"), "AC5: verify_passed present");
    assert!(branch_exists(&repo, &pass_task), "AC5: branch committed");
    // Not merged: base_ref unchanged.
    assert_eq!(rev(&repo, "HEAD"), base_commit, "AC5: not merged");
    assert_eq!(rev(&repo, "main"), base_commit, "AC5: main unchanged");
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let reports = journal.verifier_reports_for_task(&pass_task).unwrap();
        assert_eq!(reports.len(), 1, "AC5: exactly one verifier report");
    }

    // ---- AC6: close the blocked task → failed(verification_failed) ----
    let closed = round_trip(
        &socket,
        &Request::CloseTask {
            advisor_session_id: advisor.clone(),
            task_id: blocked_task.clone(),
            outcome: "abandoned".into(),
            successor: None,
        },
    );
    match closed {
        Response::Closed { task_id } => assert_eq!(task_id, blocked_task),
        other => panic!("AC6: expected Closed, got {other:?}"),
    }
    let kinds = event_kinds(&blocked_task);
    assert_eq!(kinds.last().map(String::as_str), Some("failed"), "AC6: latest is failed");
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let chain = journal.event_chain(&blocked_task).unwrap();
        let failed = chain.last().unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(failed.payload.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(payload["kind"], "verification_failed", "AC6: kind verification_failed");
        assert_eq!(payload["outcome"], "abandoned");
    }
    // A second CloseTask on the now non-blocked task → Error.
    let second = round_trip(
        &socket,
        &Request::CloseTask {
            advisor_session_id: advisor.clone(),
            task_id: blocked_task.clone(),
            outcome: "abandoned".into(),
            successor: None,
        },
    );
    assert!(
        matches!(second, Response::Error { .. }),
        "AC6: closing a non-blocked task is an Error, got {second:?}"
    );

    // ---- AC7: JournalQuery verifier_reports + trace ----
    let vr = round_trip(
        &socket,
        &Request::JournalQuery {
            advisor_session_id: advisor.clone(),
            query: "verifier_reports".into(),
            params: serde_json::json!({ "task_id": blocked_task }),
        },
    );
    match vr {
        Response::JournalResult { value } => {
            let arr = value.as_array().expect("verifier_reports is an array");
            assert_eq!(arr.len(), 5, "AC7: verifier_reports length 5");
            // Each entry carries attempt / independence / verdict / findings.
            assert!(arr[0].get("attempt").is_some());
            assert_eq!(arr[0]["independence"], "fresh_context_only");
            assert_eq!(arr[0]["verdict"], "fail");
        }
        other => panic!("AC7: expected JournalResult, got {other:?}"),
    }
    let trace = round_trip(
        &socket,
        &Request::JournalQuery {
            advisor_session_id: advisor.clone(),
            query: "trace".into(),
            params: serde_json::json!({ "task_id": blocked_task }),
        },
    );
    match trace {
        Response::JournalResult { value } => {
            let arr = value.as_array().expect("trace is an array");
            assert!(!arr.is_empty(), "AC7: trace has events");
            assert_eq!(arr[0]["seq"], 0, "AC7: trace starts at seq 0");
            assert!(arr.iter().any(|e| e["kind"] == "blocked"), "AC7: trace has blocked");
        }
        other => panic!("AC7: expected JournalResult, got {other:?}"),
    }
    // Unknown query → Error.
    let bad = round_trip(
        &socket,
        &Request::JournalQuery {
            advisor_session_id: advisor.clone(),
            query: "nope".into(),
            params: serde_json::json!({ "task_id": blocked_task }),
        },
    );
    assert!(matches!(bad, Response::Error { .. }), "AC7: unknown query is an Error");

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
