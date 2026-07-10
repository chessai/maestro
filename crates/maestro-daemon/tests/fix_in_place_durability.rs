//! L15 DURABILITY regression (the CLAUDE.md cautionary case).
//!
//! L15's original test (`fix_in_place.rs`) proved the retry REUSES the worktree
//! and the worker's edits survive *in the working tree*. But in the first real
//! run the worker's near-complete edits were **never committed** (commit happened
//! only on the checks-PASSED path), and on the fix-in-place retry the worker's own
//! `git reset` wiped the uncommitted working tree back to base — a loss-loop: the
//! implementation was unrecoverable and re-written from scratch every retry.
//!
//! This test reproduces THAT round-trip: an attempt writes a real file, trips a
//! check (`checks_failed`), and then — on the reused retry — the worktree's working
//! tree is RESET TO HEAD (simulating the live worker's cleanup). Durability must
//! NOT depend on the fragile uncommitted working tree: the fix commits the
//! in-allowlist edits to the task branch on `checks_failed`, so `reset --hard HEAD`
//! RESTORES them (HEAD carries them) instead of discarding them.
//!
//! - Against the OLD code (no commit on `checks_failed`): HEAD stays at base, the
//!   reset wipes `src/impl.rs`, the check keeps failing → never `verify_passed`
//!   (the task blocks/fails). This test FAILS.
//! - Against the FIXED code: `src/impl.rs` is committed on attempt 1's
//!   `checks_failed`; the reset on attempt 2 restores it from HEAD; the check
//!   passes → `verify_passed` after exactly ONE `checks_failed`, no escalation.
//!   This test PASSES.
//!
//! Driven with the mock implementer + mock verifier (same harness as
//! `fix_in_place.rs`), in its OWN test binary so the process-global env writes do
//! not race the other integration tests.

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
        "maestro-fix-in-place-durability-{}-{}",
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
fn m_l15_checks_failed_edits_survive_a_worktree_reset_on_the_retry() {
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

    // The mock implementer writes the near-complete implementation `src/impl.rs`
    // each attempt.
    let instructions = serde_json::json!({
        "writes": [ { "path": "src/impl.rs", "content": "pub fn added() {}\n" } ]
    })
    .to_string();

    // The check command models the LIVE loss-loop:
    //
    //   attempt 1 (marker absent): the implementation is present but a trivial
    //     check trips (as `clippy::manual_strip` did live). Create the marker and
    //     exit non-zero → `checks_failed`. The fix commits `src/impl.rs` to the
    //     task branch HERE, before the retry.
    //
    //   attempt 2 (marker present, reused worktree): SIMULATE the worker's own
    //     cleanup on the retry — `git reset --hard HEAD`. The gate has already
    //     `git add -A`'d the worker's edits, so `reset --hard HEAD` reverts tracked
    //     files to HEAD:
    //       - OLD code: HEAD == base (no commit on checks_failed) → `src/impl.rs`
    //         is WIPED → `test -f src/impl.rs` fails → checks_failed again → loop.
    //       - FIXED code: `src/impl.rs` was committed on attempt 1 → it is in HEAD
    //         → the reset RESTORES it → `test -f src/impl.rs` passes → gate passes.
    //
    // `marker.txt` is in the allowlist so creating it is not a scope violation.
    let check_cmd = "if [ -f marker.txt ]; then \
         git reset --hard HEAD -q; \
         touch marker.txt; \
         test -f src/impl.rs; \
       else \
         echo 'L15: manual_strip lint fired under -D warnings' 1>&2; \
         touch marker.txt; \
         exit 1; \
       fi"
    .to_string();

    let s = TaskSpec {
        title: "durable fix in place".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/impl.rs".into(), "marker.txt".into()],
        instructions,
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "the implementation exists".into(),
                kind: CriterionKind::Invariant,
            },
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
        "L15 durability: the near-complete edits must survive a worktree reset on \
         the retry (committed to the task branch on checks_failed) → verify_passed \
         (got {state}). Against the pre-fix code the reset wipes the uncommitted \
         edits and the task never converges."
    );

    let kinds = event_kinds(&task);
    // Exactly ONE checks_failed (attempt 1). Attempt 2's reset restored the
    // committed edits and passed.
    let checks_failed = kinds.iter().filter(|k| *k == "checks_failed").count();
    assert_eq!(
        checks_failed, 1,
        "exactly one checks_failed (attempt 1); attempt 2 resumed from the commit and passed, got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "checks_passed"),
        "attempt 2 passed the checks after the reset restored the committed edits, got {kinds:?}"
    );
    // Fix-in-place is a SAME-TIER retry: no escalation.
    assert!(
        !kinds.iter().any(|k| k == "escalated"),
        "durable fix-in-place must NOT escalate; same tier retried, got {kinds:?}"
    );
    assert!(branch_exists(&repo, &task), "branch committed on pass");

    // The task branch must carry `src/impl.rs` as a REAL committed file — the
    // durability invariant. (`HEAD:src/impl.rs` resolves on the task branch.)
    let show = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["show", &format!("maestro/{task}:src/impl.rs")])
        .output()
        .expect("spawn git show");
    assert!(
        show.status.success(),
        "src/impl.rs must be committed on the task branch (durability), stderr: {}",
        String::from_utf8_lossy(&show.stderr)
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
