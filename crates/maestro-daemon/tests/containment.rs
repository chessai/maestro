//! Integration test for M4 containment (ADR-004), AC6.
//!
//! On THIS host (which has bwrap + nix in the devShell), a delegation with a
//! profile forcing `containment.backend = "bwrap"` and `containment_min.tier0 =
//! 1`, a `mock` tier0 implementer, and a `check_command` that PROVES confinement
//! (writing to `/` fails under bwrap's read-only root) must:
//!   1. record `tasks.containment_level == 1` in the journal;
//!   2. reach a terminal state — and specifically `verify_passed`, because the
//!      confinement check exits 0 only when the write to `/` is denied.
//!
//! The gate runs the check command WRAPPED under the task's L1/bwrap recipe
//! (ADR-004 "verification surfaces inherit the task recipe"). If the gate ran
//! unwrapped (on the host) the write to `/` would still fail for an unprivileged
//! user, so to make the assertion load-bearing we also assert the recorded
//! `containment_level` is 1 — i.e. the daemon resolved and journaled L1.
//!
//! NOTE: the worktree lives under `$XDG_STATE_HOME/maestro/worktrees`. The bwrap
//! wrapper mounts `--tmpfs /tmp`, which would SHADOW a workspace located under
//! `/tmp` (masking the `--chdir` target). So this test roots its XDG dirs under
//! `$HOME` (or `CARGO_TARGET_TMPDIR`), never the system temp dir.
//!
//! Like the other daemon integration tests, config + `paths::*` read
//! process-global env, so the whole exercise lives in ONE `#[test]`.

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

/// A unique temp dir NOT under the system `/tmp` (so the bwrap `--tmpfs /tmp`
/// does not shadow the worktree). Prefers `CARGO_TARGET_TMPDIR` (lives under the
/// target dir), else `$HOME`.
fn unique_tmp() -> PathBuf {
    let base = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let name = format!(
        "maestro-containment-test-{}-{}",
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

/// A tier0 spec that writes an allowlisted file and carries a confinement-probe
/// check command: writing to `/` fails under bwrap's read-only root → `CONFINED`
/// (exit 0); a leak (write succeeds) → exit 1 → `checks_failed`. The `mock:pass`
/// criterion makes the mock verifier PASS so the happy path reaches
/// `verify_passed`.
fn confinement_spec() -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": "src/added.rs", "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    TaskSpec {
        title: "contained add".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/added.rs".into()],
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
        // Proves confinement: exit 1 (checks_failed) on LEAK, exit 0 on CONFINED.
        check_commands: vec![
            "if echo x > /confinement_probe 2>/dev/null; then echo LEAK; exit 1; else echo CONFINED; fi"
                .into(),
        ],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    }
}

#[test]
fn m4_containment_bwrap_l1_end_to_end() {
    // Skip cleanly if bwrap is not available on this host.
    let caps = maestro_sandbox::probe();
    if !caps.bwrap {
        eprintln!("SKIP: bwrap not available on this host");
        return;
    }

    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // A profile forcing bwrap + tier0 containment floor L1, mock implementer.
    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"
default_profile = "contained"
[defaults]
concurrency.machine_cap = 4

[profiles.contained]
roles.tier0 = "mock"
roles.verifier_floor = "mock"
containment_min = { tier0 = 1 }
containment.backend = "bwrap"
containment.network = "deny"
"#,
    )
    .unwrap();
    std::env::set_var("MAESTRO_PROFILE", "contained");

    let repo = init_repo(&tmp);

    let server = Server::start(Options {
        profile: Some("contained".into()),
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
            profile: Some("contained".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let task_id = match round_trip(
        &socket,
        &Request::Delegate {
            advisor_session_id: advisor.clone(),
            repo_path: repo.to_string_lossy().to_string(),
            spec: Box::new(confinement_spec()),
        },
    ) {
        Response::Delegate { task_id } => task_id,
        other => panic!("expected Delegate, got {other:?}"),
    };

    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(60));

    // Assert the recorded effective containment level is L1 (downgrade-and-tighten
    // records the ACTUAL level; here the host has bwrap so L1 holds).
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
    let task = journal.get_task(&task_id).expect("task row");
    assert_eq!(
        task.containment_level,
        maestro_journal::domain::ContainmentLevel::L1,
        "AC6: tasks.containment_level must be recorded as L1"
    );

    // The task reaches a terminal state without an internal error; specifically
    // `verify_passed`, because the gate's confinement check ran under bwrap and
    // the write to `/` was denied (CONFINED → exit 0).
    assert_eq!(
        state, "verify_passed",
        "AC6: contained delegation must pass the confinement check + verifier"
    );

    // No `containment_downgraded` event (bwrap is available → no downgrade).
    let chain = journal.event_chain(&task_id).expect("event chain");
    assert!(
        !chain
            .iter()
            .any(|e| e.kind == maestro_journal::domain::EventKind::ContainmentDowngraded),
        "AC6: no downgrade when bwrap is available"
    );
    drop(journal);

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
