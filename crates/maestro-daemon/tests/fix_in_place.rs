//! L15 integration test: FIX-IN-PLACE retry on a `checks_failed`.
//!
//! When an attempt fails the mechanical gate's `check_commands` (a
//! `checks_failed`), the NEXT attempt must REUSE the same worktree (the worker's
//! edits intact — NOT reset to `base_ref`) and inject the failing check into the
//! worker's context, so it fixes the specific error instead of re-implementing
//! from scratch. This drives the real daemon with the mock implementer + mock
//! verifier (same harness as `verify_escalation.rs`).
//!
//! The observability trick: the check command creates a marker file (in the
//! allowlist, so no scope violation) on its FIRST run and exits non-zero
//! (`checks_failed`); on a SUBSEQUENT run it sees the marker and exits zero. The
//! marker only survives into the next attempt if the worktree was REUSED — a
//! fresh cut off `base_ref` would discard it, and the task would fail the check
//! forever and blow the whole ladder. So a `verify_passed` after exactly ONE
//! `checks_failed`, with NO `escalated`, proves fix-in-place worked.
//!
//! NOTE: `paths::*` and config resolution read process-global env vars, so all
//! exercises live in ONE `#[test]` to avoid cross-test env races.

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
        "maestro-fix-in-place-test-{}-{}",
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

/// Read a task's event kind chain straight from the journal DB.
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
fn m_l15_checks_failed_retry_reuses_worktree_and_fixes_in_place() {
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

    // ------- L15: a `checks_failed` on attempt 1 is FIXED IN PLACE on attempt 2.
    //
    // The mock implementer writes `src/impl.rs` each attempt. The check command
    // fails on its FIRST run (marker absent → create it, exit 1) and passes on
    // its SECOND (marker present → exit 0). The marker (`marker.txt`) is in the
    // allowlist so it is not a scope violation. The marker only survives into
    // attempt 2 if the worktree was REUSED — a fresh cut off base_ref would
    // discard it and the check would fail forever.
    let instructions = serde_json::json!({
        "writes": [ { "path": "src/impl.rs", "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    let check_cmd =
        "if [ -f marker.txt ]; then exit 0; else echo 'L15: use of moved value; fix the borrow' 1>&2; \
         touch marker.txt; exit 1; fi"
            .to_string();
    let s = TaskSpec {
        title: "fix in place".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/impl.rs".into(), "marker.txt".into()],
        instructions,
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "the file exists".into(),
                kind: CriterionKind::Invariant,
            },
            // mock:pass → the mock verifier passes ONCE the checks pass.
            AcceptanceCriterion {
                id: "AC2".into(),
                check: "mock:pass".into(),
                kind: CriterionKind::Invariant,
            },
        ],
        check_commands: vec![check_cmd],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    };

    let task = delegate(&socket, &advisor, &repo_path, s);
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(60));
    assert_eq!(
        state, "verify_passed",
        "L15: a checks_failed fixed in place on the reused worktree → verify_passed (got {state})"
    );

    let kinds = event_kinds(&task);
    // Exactly ONE checks_failed: attempt 1. Attempt 2 reused the worktree (marker
    // survived) and passed → checks_passed.
    let checks_failed = kinds.iter().filter(|k| *k == "checks_failed").count();
    assert_eq!(
        checks_failed, 1,
        "L15: exactly one checks_failed (attempt 1); attempt 2 fixed in place, got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "checks_passed"),
        "L15: attempt 2 passed the checks, got {kinds:?}"
    );
    // Fix-in-place is a SAME-TIER retry: no escalation happened.
    assert!(
        !kinds.iter().any(|k| k == "escalated"),
        "L15: fix-in-place must NOT escalate; the same tier retried, got {kinds:?}"
    );
    // No verify_failed (the model verifier never rejected — the checks did).
    assert_eq!(
        kinds.iter().filter(|k| *k == "verify_failed").count(),
        0,
        "L15: no model verify_failed on this path, got {kinds:?}"
    );
    assert!(branch_exists(&repo, &task), "L15: branch committed on pass");

    // Exactly two spawn events (attempt 1 + the reused attempt 2), confirming the
    // reuse happened as a real second attempt (not a single attempt).
    let spawned = kinds.iter().filter(|k| *k == "spawned").count();
    assert_eq!(spawned, 2, "L15: two attempts ran (spawn ×2), got {kinds:?}");

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
