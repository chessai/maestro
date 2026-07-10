//! ADR-009 Phase 2: daemon-side stall detection + auto-recovery.
//!
//! Behavioural integration test: a driven worker writes an in-allowlist file,
//! echoes its PLAN line, then **goes silent** (sleeps past `stall_timeout`).
//! The daemon detects the stall, kills the session, commits the partial edits,
//! and retries same-tier (fix-in-place). The retry's worker writes the same
//! file and exits cleanly → `verify_passed`.
//!
//! - Against the OLD code (no stall detection): the task hangs to the coarse
//!   watchdog (minutes), then terminally fails as `session_wedged` — this test
//!   would time out.
//! - Against the FIXED code: the task reaches `verify_passed` well within the
//!   test timeout, with `stall_detected` + `auto_recovered` events in the
//!   journal.

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
        "maestro-stall-recovery-{}-{}",
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

/// Write an executable fake-CLI script and return its path.
fn write_fake_cli(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    let script = format!("#!/usr/bin/env bash\nset -eu\n{body}\n");
    std::fs::write(&path, script).unwrap();
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().to_string()
}

#[test]
fn m_adr009_stall_detected_and_auto_recovered() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // Stall detection config: very short timeout (3s) so the test runs fast.
    // The coarse watchdog is set to 30s (via env override) as the outer backstop.
    std::env::set_var("MAESTRO_STALL_TIMEOUT_SECONDS", "3");
    std::env::set_var("MAESTRO_WATCHDOG_SECONDS", "30");

    let repo = init_repo(&tmp);
    let repo_path = repo.to_string_lossy().to_string();

    // --- Fake CLI scripts ---
    //
    // STALL script (attempt 1): writes the implementation file, echoes the PLAN
    // line, then goes SILENT (sleeps for 600s — way past the stall timeout).
    // The daemon should detect the stall after ~3s and kill the session.
    let stall_fake = write_fake_cli(
        &tmp,
        "stall.sh",
        r#"
# Write the implementation into the worktree (the cwd IS the worktree).
mkdir -p src
echo 'pub fn stall_test() {}' > src/impl.rs
# Echo the plan line so the plan-echo gate accepts.
echo "PLAN: implement the file and stall"
# Go silent — this is the stall the daemon should detect.
sleep 600
"#,
    );

    // OK script (attempt 2, fix-in-place retry): the edits from attempt 1 are
    // already committed to the worktree (by stall recovery). This script just
    // ensures the file exists and exits cleanly.
    let ok_fake = write_fake_cli(
        &tmp,
        "ok.sh",
        r#"
# The stall-recovery committed src/impl.rs — it should be here.
mkdir -p src
# Ensure the file exists (it was committed by stall recovery).
if [ ! -f src/impl.rs ]; then
  echo 'pub fn stall_test() {}' > src/impl.rs
fi
echo "PLAN: continue from stall recovery"
"#,
    );

    // A counter file distinguishes attempt 1 from attempt 2: the stall script
    // runs on first invocation, the ok script on subsequent ones.
    let dispatch_fake = write_fake_cli(
        &tmp,
        "dispatch.sh",
        &format!(
            r#"
COUNTER="{counter}"
if [ ! -f "$COUNTER" ]; then
  echo 1 > "$COUNTER"
  exec bash "{stall}"
else
  exec bash "{ok}"
fi
"#,
            counter = tmp.join("attempt_counter").display(),
            stall = stall_fake,
            ok = ok_fake,
        ),
    );

    // Write config: driven_cli role using the dispatch script.
    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"
default_profile = "test"
[defaults]
concurrency.machine_cap = 4
monitoring.stall_timeout_seconds = 3
monitoring.stall_action = "snapshot_kill_retry"

[profiles.test]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{dispatch}"] }}
roles.tier1 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{dispatch}"] }}
roles.tier2 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{dispatch}"] }}
roles.verifier_floor = "mock"
"#,
            dispatch = dispatch_fake,
        ),
    )
    .unwrap();
    std::env::set_var("MAESTRO_PROFILE", "test");

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

    let spec = TaskSpec {
        title: "stall recovery test".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/impl.rs".into()],
        instructions: "create src/impl.rs".into(),
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
        check_commands: vec!["test -f src/impl.rs".into()],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    };

    let task = delegate(&socket, &advisor, &repo_path, spec);
    // 60s is generous: stall fires at ~3s, retry should complete quickly.
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(60));
    assert_eq!(
        state, "verify_passed",
        "ADR-009: the stalled task must self-heal (stall → kill → retry → pass), \
         got terminal state '{state}'. Without stall detection the task would hang \
         to the coarse watchdog and fail as session_wedged."
    );

    let kinds = event_kinds(&task);

    // The journal must contain `stall_detected` and `auto_recovered` events.
    assert!(
        kinds.iter().any(|k| k == "stall_detected"),
        "stall_detected event must be present in the journal, got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "auto_recovered"),
        "auto_recovered event must be present in the journal, got {kinds:?}"
    );

    // The task should NOT have escalated — stall recovery is same-tier.
    assert!(
        !kinds.iter().any(|k| k == "escalated"),
        "stall recovery must NOT escalate; same tier retried, got {kinds:?}"
    );

    // Verify the task reached verify_passed (already asserted above via state,
    // but also check the event chain).
    assert!(
        kinds.iter().any(|k| k == "verify_passed"),
        "verify_passed must be in the event chain, got {kinds:?}"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
