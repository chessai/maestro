//! Integration test for `maestro watch`'s return semantics (ADR-009, Phase 1).
//!
//! `watch`'s poll loop is built on the PURE core `maestro_journal::progress::
//! watch_once(rows, tracked)`, evaluated once per poll over a `TaskStatus`
//! snapshot. This test drives a REAL task through the daemon (mock implementer +
//! mock verifier, same harness as `merge.rs`/`fix_in_place.rs`) and asserts
//! `watch_once` — the exact decision the CLI loop makes — against the LIVE rows:
//!
//!   1. while the task dwells in a TRANSIENT state (a `check_command` that sleeps
//!      holds it in `checks_started`), `watch_once` returns `None` → watch keeps
//!      polling, it does NOT wake the advisor for mid-flight work;
//!   2. once the task reaches `verify_passed` (ACTIONABLE), `watch_once` returns
//!      an `Actionable` digest suggesting `merge` → watch exits 0 promptly;
//!   3. after an explicit merge (all tracked tasks `merged`), `watch_once`
//!      returns the `AllTerminal` early-return.
//!
//! This complements the CLI-side unit tests of the loop (`progress.rs`, with a
//! scripted transport + fake clock) with a real journal/daemon round-trip, per
//! the CLAUDE.md bar (a wrong partition would wake the advisor constantly or
//! never).
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
use maestro_journal::progress::{state_class, watch_once, StateClass, WatchTrigger};
use maestro_journal::proto::{PsRow, Request, Response};
use maestro_journal::spec::{AcceptanceCriterion, CriterionKind, TaskSpec};

fn unique_tmp() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "maestro-watch-test-{}-{}",
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

/// Fetch the advisor's rows via the same request the CLI uses.
fn fetch_rows(socket: &Path, advisor: &str) -> Vec<PsRow> {
    match round_trip(
        socket,
        &Request::TaskStatus {
            advisor_session_id: advisor.to_string(),
            state: None,
        },
    ) {
        Response::TaskStatus { tasks } => tasks,
        other => panic!("expected TaskStatus, got {other:?}"),
    }
}

fn poll_terminal(socket: &Path, advisor: &str, task_id: &str, timeout: Duration) -> String {
    const TERMINAL: &[&str] = &["verify_passed", "blocked", "failed", "merged"];
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(row) = fetch_rows(socket, advisor)
            .into_iter()
            .find(|t| t.task_id == task_id)
        {
            if TERMINAL.contains(&row.state.as_str()) {
                return row.state;
            }
        }
        if Instant::now() >= deadline {
            panic!("task {task_id} did not reach a terminal state in time");
        }
        std::thread::sleep(Duration::from_millis(25));
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

/// A Tier-0 spec that writes an allowlisted file, passes the mock verifier, and
/// runs `check_commands` (used to hold the task in a transient state).
fn spec(base_ref: &str, write_path: &str, check_commands: Vec<String>) -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    TaskSpec {
        title: "watched task".into(),
        tier: Tier::T0,
        base_ref: base_ref.into(),
        file_allowlist: vec![write_path.into()],
        instructions,
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "the file exists".into(),
                kind: CriterionKind::Invariant,
            },
            AcceptanceCriterion {
                id: "AC2".into(),
                check: "mock:pass".into(),
                kind: CriterionKind::Invariant,
            },
        ],
        check_commands,
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    }
}

#[test]
fn watch_once_semantics_over_a_live_task() {
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

    // ---- 1. TRANSIENT → watch_once must NOT return -----------------------------
    // A `check_command` that sleeps holds the task in a transient state
    // (`checks_started`) for the sleep window. We poll fast and assert that at
    // least once we observe a transient tracked state AND `watch_once` returns
    // None for it. (Every non-terminal snapshot in this window MUST be None —
    // asserted below.)
    let task = delegate(
        &socket,
        &advisor,
        &repo_path,
        spec("HEAD", "src/impl.rs", vec!["sleep 2".to_string()]),
    );
    let tracked = vec![task.clone()];

    let mut saw_transient = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = fetch_rows(&socket, &advisor);
        let row = rows.iter().find(|r| r.task_id == task);
        let terminal_now = row
            .map(|r| state_class(&r.state).is_terminal())
            .unwrap_or(false);

        if let Some(r) = row {
            let class = state_class(&r.state);
            let decision = watch_once(&rows, &tracked);
            if class == StateClass::Transient {
                saw_transient = true;
                // The load-bearing assertion: watch does NOT return mid-flight.
                assert!(
                    decision.is_none(),
                    "watch_once returned for a TRANSIENT state {} (must keep polling): {decision:?}",
                    r.state
                );
            }
        }

        if terminal_now {
            break;
        }
        if Instant::now() >= deadline {
            panic!("task never reached a terminal state within 20s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        saw_transient,
        "the sleeping check_command should have held the task in a transient state at least once"
    );

    // ---- 2. verify_passed (ACTIONABLE) → watch_once returns, action=merge ------
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "mock:pass → verify_passed");

    let rows = fetch_rows(&socket, &advisor);
    let digest = watch_once(&rows, &tracked).expect("verify_passed is actionable → watch returns");
    assert_eq!(digest.trigger, WatchTrigger::Actionable);
    assert_eq!(digest.lines.len(), 1, "one triggering task");
    assert_eq!(digest.lines[0].task_id, task);
    assert_eq!(digest.lines[0].state, "verify_passed");
    assert_eq!(digest.lines[0].action, "merge", "verify_passed → merge");

    // ---- 3. all merged → watch_once AllTerminal early-return -------------------
    let merged = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: task.clone(),
        },
    );
    assert!(matches!(merged, Response::Merged { .. }), "merge succeeds, got {merged:?}");

    let rows = fetch_rows(&socket, &advisor);
    let after = rows.iter().find(|r| r.task_id == task).expect("task row still present");
    assert_eq!(after.state, "merged", "task is now merged");
    let digest = watch_once(&rows, &tracked).expect("all tracked merged → all-terminal return");
    assert_eq!(
        digest.trigger,
        WatchTrigger::AllTerminal,
        "all tracked tasks terminal → AllTerminal early return"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
