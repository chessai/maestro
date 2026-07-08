//! Integration tests for the M1 one-shot delegation pipeline (AC4 / AC5).
//!
//! Both exercises drive the daemon server in-process with an isolated XDG env
//! (the `tests/server.rs` pattern), against a real temp git repo, using the
//! `"mock"` implementer backend. The mock model is forced via a config file in
//! the isolated `XDG_CONFIG_HOME` whose active profile sets `roles.tier0 =
//! "mock"`; the active profile is selected via `MAESTRO_PROFILE`.
//!
//! NOTE: like `server.rs`, `paths::*` and config resolution read process-global
//! env vars, so all delegation exercises live in ONE `#[test]` to avoid
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
        "maestro-delegate-test-{}-{}",
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

/// Init a repo with one committed file; return (repo_path, base_ref="HEAD").
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
/// Returns the derived state string.
fn poll_terminal(socket: &Path, advisor: &str, task_id: &str, timeout: Duration) -> String {
    const TERMINAL: &[&str] = &[
        "verify_passed",
        "blocked",
        "failed",
        "merged",
    ];
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

/// Does branch `maestro/<task_id>` exist in `repo`?
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

/// The commit hash `base_ref` points at in `repo`.
fn rev(repo: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Is `path` (relative) present in the tree of `rev` in `repo`?
fn path_in_tree(repo: &Path, rev: &str, path: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{rev}:{path}")])
        .output()
        .expect("spawn git cat-file")
        .status
        .success()
}

fn tier0_spec(base_ref: &str, allowlist: Vec<String>, write_path: &str) -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    TaskSpec {
        title: "add a file".into(),
        tier: Tier::T0,
        base_ref: base_ref.into(),
        file_allowlist: allowlist,
        instructions,
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "the file exists".into(),
                kind: CriterionKind::Invariant,
            },
            // M2: a `mock:pass` criterion makes the mock verifier PASS so the
            // happy path reaches `verify_passed` (AC5).
            AcceptanceCriterion {
                id: "AC2".into(),
                check: "mock:pass".into(),
                kind: CriterionKind::Invariant,
            },
        ],
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    }
}

#[test]
fn m1_delegation_pipeline_pass_and_scope_violation() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // Force the mock model: a config whose active profile sets tier0 = "mock".
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
"#,
    )
    .unwrap();
    std::env::set_var("MAESTRO_PROFILE", "test");

    let repo = init_repo(&tmp);
    let base_commit = rev(&repo, "HEAD");

    let server = Server::start(Options {
        profile: None,
        detach: false,
    })
    .expect("server starts");
    let socket = server.socket_path().to_path_buf();
    let shutdown = server.shutdown_handle();
    let handle = std::thread::spawn(move || server.serve_until().expect("serve loop"));
    wait_for_socket(&socket, Duration::from_secs(5));

    // Register an advisor.
    let advisor = match round_trip(
        &socket,
        &Request::RegisterAdvisor {
            profile: Some("test".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    // --- AC4: allowlisted single-file change → checks_passed → verify_passed
    //     (mock:pass criterion), committed to branch, base_ref unchanged. ---
    let spec = tier0_spec("HEAD", vec!["src/added.rs".into()], "src/added.rs");
    let repo_path = repo.to_string_lossy().to_string();
    let task_id = match round_trip(
        &socket,
        &Request::Delegate {
            advisor_session_id: advisor.clone(),
            repo_path: repo_path.clone(),
            spec: Box::new(spec),
        },
    ) {
        Response::Delegate { task_id } => task_id,
        other => panic!("expected Delegate, got {other:?}"),
    };

    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "AC4: task must pass gate + verifier");
    assert!(
        branch_exists(&repo, &task_id),
        "AC4: branch maestro/{task_id} must exist"
    );
    assert!(
        path_in_tree(&repo, &format!("maestro/{task_id}"), "src/added.rs"),
        "AC4: the new file must be committed on the branch"
    );
    // base_ref (main / HEAD) unchanged: no merge.
    assert_eq!(
        rev(&repo, "HEAD"),
        base_commit,
        "AC4: base_ref must be unchanged (daemon must not merge)"
    );
    assert_eq!(rev(&repo, "main"), base_commit, "AC4: main unchanged");

    // --- AC7 (ADR-008): the session row is metered back after the mock run.
    //     Read the journal DB directly (WAL allows a concurrent reader). ---
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal = maestro_journal::Journal::open(db_path.to_str().unwrap())
            .expect("open journal for session read");
        let (ended_at, exit_status, turns): (Option<String>, Option<String>, Option<i64>) = journal
            .connection()
            .query_row(
                "SELECT ended_at, exit_status, turns FROM sessions
                   WHERE task_id = ?1 AND role = 'implementer'",
                [&task_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("implementer session row exists");
        assert!(ended_at.is_some(), "AC7: session ended_at must be set");
        assert_eq!(
            exit_status.as_deref(),
            Some("ok"),
            "AC7: session exit_status must be ok"
        );
        assert_eq!(turns, Some(1), "AC7: mock reports 1 turn");
        drop(journal);
    }

    // --- AC5: write OUTSIDE the allowlist → failed(scope_violation),
    //     base_ref unchanged. ---
    let spec2 = tier0_spec("HEAD", vec!["src/allowed.rs".into()], "src/evil.rs");
    let task2 = match round_trip(
        &socket,
        &Request::Delegate {
            advisor_session_id: advisor.clone(),
            repo_path: repo_path.clone(),
            spec: Box::new(spec2),
        },
    ) {
        Response::Delegate { task_id } => task_id,
        other => panic!("expected Delegate, got {other:?}"),
    };

    let state2 = poll_terminal(&socket, &advisor, &task2, Duration::from_secs(30));
    assert_eq!(state2, "failed", "AC5: out-of-allowlist edit must fail");

    // Confirm the failure KIND is `scope_violation` by reading the failed
    // event's payload straight from the journal DB (WAL allows a concurrent
    // reader while the daemon holds the writer).
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(&task2).expect("event chain");
    let failed = chain
        .iter()
        .find(|e| matches!(e.kind, maestro_journal::domain::EventKind::Failed))
        .expect("AC5: a failed event exists");
    let payload: serde_json::Value =
        serde_json::from_str(failed.payload.as_deref().unwrap_or("{}")).expect("payload json");
    assert_eq!(
        payload.get("kind").and_then(|v| v.as_str()),
        Some("scope_violation"),
        "AC5: failure kind must be scope_violation"
    );
    drop(journal);

    // Confirm the inbox also surfaces the failed lifecycle event.
    let drained = round_trip(
        &socket,
        &Request::DrainInbox {
            advisor_session_id: advisor.clone(),
        },
    );
    match drained {
        Response::Inbox { items } => {
            assert!(
                items.iter().any(|i| i.task_id == task2 && i.kind == "failed"),
                "AC5: inbox carries a failed event for the scope-violating task"
            );
        }
        other => panic!("expected Inbox, got {other:?}"),
    }
    assert_eq!(
        rev(&repo, "HEAD"),
        base_commit,
        "AC5: base_ref must be unchanged"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
