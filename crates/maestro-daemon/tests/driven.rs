//! Integration tests for M3 driven (PTY) sessions (AC4–AC7, ADR-006).
//!
//! These drive the daemon server in-process with an isolated XDG env (the
//! `tests/server.rs` pattern), against a real temp git repo, using a **fake CLI
//! script** written by the test — NO real `claude` / no network. The fake echoes
//! a `PLAN:` line (driving the plan-echo gate) then optionally edits the
//! worktree. The `"mock"` role model selects `MockPlanChecker`, so the plan text
//! deterministically drives Accept/Reject.
//!
//! NOTE: like the other daemon integration tests, `paths::*` and config
//! resolution read process-global env vars, so all driven exercises live in ONE
//! `#[test]` to avoid cross-test env races.

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
        "maestro-driven-test-{}-{}",
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

fn rev(repo: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// Write an executable fake-CLI script and return its path. `body` is the shell
/// body after the shebang; the script runs with the worktree as its cwd.
fn write_fake_cli(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    let script = format!("#!/usr/bin/env bash\nset -eu\n{body}\n");
    std::fs::write(&path, script).unwrap();
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().to_string()
}

fn driven_spec(
    base_ref: &str,
    allowlist: Vec<String>,
) -> TaskSpec {
    TaskSpec {
        title: "driven add a file".into(),
        tier: Tier::T0,
        base_ref: base_ref.into(),
        file_allowlist: allowlist,
        instructions: "create the allowlisted file".into(),
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "the file exists".into(),
                kind: CriterionKind::Invariant,
            },
            // Makes the mock verifier PASS so the happy path reaches
            // `verify_passed`.
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

fn delegate(socket: &Path, advisor: &str, repo: &Path, spec: TaskSpec) -> String {
    match round_trip(
        socket,
        &Request::Delegate {
            advisor_session_id: advisor.to_string(),
            repo_path: repo.to_string_lossy().to_string(),
            spec: Box::new(spec),
        },
    ) {
        Response::Delegate { task_id } => task_id,
        other => panic!("expected Delegate, got {other:?}"),
    }
}

/// Read a task's terminal `failed` event payload straight from the journal DB.
fn failed_payload(task_id: &str) -> serde_json::Value {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(task_id).expect("event chain");
    let failed = chain
        .iter()
        .find(|e| matches!(e.kind, maestro_journal::domain::EventKind::Failed))
        .expect("a failed event exists");
    serde_json::from_str(failed.payload.as_deref().unwrap_or("{}")).expect("payload json")
}

/// Does the task's event chain contain a kind (as string)?
fn chain_has_kind(task_id: &str, kind: &str) -> bool {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(task_id).expect("event chain");
    chain.iter().any(|e| e.kind.as_str() == kind)
}

/// The `interrupted` event payload for a task, if any.
fn interrupted_payload(task_id: &str) -> Option<serde_json::Value> {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal =
        maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal for read");
    let chain = journal.event_chain(task_id).expect("event chain");
    chain
        .iter()
        .find(|e| matches!(e.kind, maestro_journal::domain::EventKind::Interrupted))
        .map(|e| serde_json::from_str(e.payload.as_deref().unwrap_or("{}")).unwrap())
}

#[test]
fn m3_driven_sessions_end_to_end() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);
    std::env::remove_var("MAESTRO_WATCHDOG_SECONDS");

    let scripts = tmp.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();

    // Fakes. Each runs in the worktree (the driver sets cwd), so relative writes
    // land in the worktree.
    // AC4: plan accepted, writes the allowlisted file, exits 0.
    let ok_fake = write_fake_cli(
        &scripts,
        "ok.sh",
        "echo 'PLAN: create the file'\nmkdir -p src\nprintf 'pub fn added() {}\\n' > src/added.rs",
    );
    // AC5: plan rejected ("delete"), and it WOULD write an out-of-plan file.
    let reject_fake = write_fake_cli(
        &scripts,
        "reject.sh",
        "echo 'PLAN: delete everything'\nmkdir -p src\nprintf 'evil\\n' > src/evil.rs",
    );
    // AC6/AC7: plan accepted then sleep ~30s (kill / watchdog).
    let sleep_fake = write_fake_cli(
        &scripts,
        "sleep.sh",
        "echo 'PLAN: create the file'\nsleep 30",
    );

    // AC8 (claude adapter): fake "claude" CLI that keys off --permission-mode.
    // plan phase → print a plan (contains "create"); acceptEdits phase → write file.
    // We detect the mode by scanning $@ for --permission-mode <mode>.
    // The script produces output in both phases so the watchdog never fires.
    let claude_fake = write_fake_cli(
        &scripts,
        "fake-claude.sh",
        r#"mode=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--permission-mode" ]; then
        mode="$arg"
    fi
    prev="$arg"
done
if [ "$mode" = "plan" ]; then
    printf 'I will create src/claude_output.rs with a placeholder function.\n'
elif [ "$mode" = "acceptEdits" ]; then
    mkdir -p src
    printf 'pub fn claude_output() {}\n' > src/claude_output.rs
    sync
    printf 'done\n'
fi
exit 0"#,
    );

    // Config: a `driven_cli` tier0 role that spawns bash <fake> (fake chosen
    // per delegation by rewriting the config? No — config is static). We instead
    // set the command to `bash` and args to the specific fake per profile.
    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"
default_profile = "ok"
[defaults]
concurrency.machine_cap = 4

[profiles.ok]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{ok}"] }}
roles.verifier_floor = "mock"

[profiles.reject]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{reject}"] }}
roles.verifier_floor = "mock"

[profiles.sleep]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{sleep}"] }}
roles.verifier_floor = "mock"

[profiles.claude_adapter]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{claude}"], adapter = "claude" }}
roles.verifier_floor = "mock"
"#,
            ok = ok_fake,
            reject = reject_fake,
            sleep = sleep_fake,
            claude = claude_fake,
        ),
    )
    .unwrap();

    let repo = init_repo(&tmp);
    let base_commit = rev(&repo, "HEAD");

    // ---- AC4: driven success (profile "ok") --------------------------------
    std::env::set_var("MAESTRO_PROFILE", "ok");
    let server = Server::start(Options {
        profile: Some("ok".into()),
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
            profile: Some("ok".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let spec = driven_spec("HEAD", vec!["src/added.rs".into()]);
    let task_id = delegate(&socket, &advisor, &repo, spec);
    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "AC4: driven success must verify_pass");
    assert!(branch_exists(&repo, &task_id), "AC4: branch exists");
    assert!(
        path_in_tree(&repo, &format!("maestro/{task_id}"), "src/added.rs"),
        "AC4: driven-written file committed on the branch"
    );
    assert_eq!(rev(&repo, "HEAD"), base_commit, "AC4: base unchanged");

    // The session row is a driven_pty session with a log_path.
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal = maestro_journal::Journal::open(db_path.to_str().unwrap()).unwrap();
        let sessions = journal.sessions_for_task(&task_id).unwrap();
        let impl_session = sessions
            .iter()
            .find(|s| matches!(s.role, maestro_journal::domain::Role::Implementer))
            .expect("implementer session");
        assert!(
            matches!(impl_session.kind, maestro_journal::domain::SessionKind::DrivenPty),
            "AC4: implementer session is driven_pty"
        );
        assert!(impl_session.log_path.is_some(), "AC4: log_path recorded");
    }

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");

    // ---- AC5: plan_rejected + zero edits (profile "reject") ----------------
    std::env::set_var("MAESTRO_PROFILE", "reject");
    let server = Server::start(Options {
        profile: Some("reject".into()),
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
            profile: Some("reject".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let spec = driven_spec("HEAD", vec!["src/allowed.rs".into()]);
    let task_id = delegate(&socket, &advisor, &repo, spec);
    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(30));
    assert_eq!(state, "failed", "AC5: plan-rejected task fails");
    let payload = failed_payload(&task_id);
    assert_eq!(
        payload.get("kind").and_then(|v| v.as_str()),
        Some("plan_rejected"),
        "AC5: failure kind is plan_rejected"
    );
    // Zero edits: nothing committed on the branch, and the fake's file was never
    // written (the driver killed before edits, and the worktree was removed).
    if branch_exists(&repo, &task_id) {
        assert!(
            !path_in_tree(&repo, &format!("maestro/{task_id}"), "src/evil.rs"),
            "AC5: out-of-plan file must NOT be committed (zero edits)"
        );
    }
    assert_eq!(rev(&repo, "HEAD"), base_commit, "AC5: base unchanged");

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");

    // ---- AC6: kill → interrupted_human + snapshot (profile "sleep") --------
    std::env::set_var("MAESTRO_PROFILE", "sleep");
    // Long watchdog so the kill (not the watchdog) is what ends the session.
    std::env::set_var("MAESTRO_WATCHDOG_SECONDS", "60");
    let server = Server::start(Options {
        profile: Some("sleep".into()),
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
            profile: Some("sleep".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let spec = driven_spec("HEAD", vec!["src/added.rs".into()]);
    let task_id = delegate(&socket, &advisor, &repo, spec);

    // Wait until the fake's `sleep 30` is running (the plan echo has been read
    // and the child is in its long sleep), capturing its pid, then kill.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pid_opt: Option<i32> = None;
    while Instant::now() < deadline {
        if let Some(pid) = find_sleep_pid() {
            pid_opt = Some(pid);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Send the human kill; the session must be registered (KillTask succeeds).
    match round_trip(
        &socket,
        &Request::KillTask {
            task_id: task_id.clone(),
            kind: "human".to_string(),
        },
    ) {
        Response::Killed { .. } => {}
        other => panic!("AC6: expected Killed, got {other:?}"),
    }

    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(15));
    assert_eq!(state, "failed", "AC6: killed task fails");
    assert!(
        chain_has_kind(&task_id, "interrupted"),
        "AC6: event chain contains interrupted"
    );
    let payload = failed_payload(&task_id);
    assert_eq!(
        payload.get("kind").and_then(|v| v.as_str()),
        Some("interrupted_human"),
        "AC6: failure kind is interrupted_human"
    );
    assert!(
        payload.get("partial_diff").is_some(),
        "AC6: failed payload carries a partial_diff"
    );
    let ipayload = interrupted_payload(&task_id).expect("AC6: interrupted payload");
    assert!(
        ipayload.get("partial_diff").is_some(),
        "AC6: interrupted payload carries a partial_diff"
    );

    // No lingering fake process: the sleep pid (captured before terminal) is
    // gone after teardown.
    if let Some(pid) = pid_opt {
        let deadline = Instant::now() + Duration::from_secs(6);
        while maestro_driver::pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !maestro_driver::pid_alive(pid),
            "AC6: no lingering fake process (pid {pid})"
        );
    }

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");

    // ---- AC7: watchdog → session_wedged (profile "sleep", short watchdog) --
    std::env::set_var("MAESTRO_PROFILE", "sleep");
    std::env::set_var("MAESTRO_WATCHDOG_SECONDS", "2");
    let server = Server::start(Options {
        profile: Some("sleep".into()),
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
            profile: Some("sleep".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let spec = driven_spec("HEAD", vec!["src/added.rs".into()]);
    let task_id = delegate(&socket, &advisor, &repo, spec);
    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(10));
    assert_eq!(state, "failed", "AC7: watchdog task fails");
    let payload = failed_payload(&task_id);
    assert_eq!(
        payload.get("kind").and_then(|v| v.as_str()),
        Some("session_wedged"),
        "AC7: failure kind is session_wedged"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");

    // ---- AC8 (claude adapter): two-phase permission-mode path ---------------
    // A `driven_cli` role with `adapter = "claude"` must: run the fake script
    // twice (plan phase → acceptEdits phase), accept the plan (MockPlanChecker
    // sees "create"), write the file in the acceptEdits phase, reach
    // `verify_passed`, and commit the file on the branch.
    std::env::remove_var("MAESTRO_WATCHDOG_SECONDS");
    std::env::set_var("MAESTRO_PROFILE", "claude_adapter");
    let server = Server::start(Options {
        profile: Some("claude_adapter".into()),
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
            profile: Some("claude_adapter".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    // Spec: allowlist the file the fake script writes in acceptEdits phase;
    // mock:pass criterion so MockVerifier returns Pass → verify_passed.
    let spec = TaskSpec {
        title: "claude adapter: create output file".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/claude_output.rs".into()],
        instructions: "create src/claude_output.rs with a placeholder function".into(),
        acceptance_criteria: vec![
            AcceptanceCriterion {
                id: "AC1".into(),
                check: "src/claude_output.rs exists".into(),
                kind: CriterionKind::Invariant,
            },
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
    };

    let task_id = delegate(&socket, &advisor, &repo, spec);
    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(30));
    assert_eq!(
        state, "verify_passed",
        "AC8: claude adapter two-phase driven session must reach verify_passed"
    );
    // Allow the worker thread (which emits verify_passed before commit_all runs)
    // time to complete the commit. Poll for up to 2 seconds.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !branch_exists(&repo, &task_id) || !path_in_tree(&repo, &format!("maestro/{task_id}"), "src/claude_output.rs") {
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        branch_exists(&repo, &task_id),
        "AC8: branch must exist after verify_passed"
    );
    assert!(
        path_in_tree(
            &repo,
            &format!("maestro/{task_id}"),
            "src/claude_output.rs"
        ),
        "AC8: file written in acceptEdits phase must be committed on the branch"
    );
    assert_eq!(
        rev(&repo, "HEAD"),
        base_commit,
        "AC8: base commit must be unchanged"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");

    std::env::remove_var("MAESTRO_WATCHDOG_SECONDS");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Best-effort: find a `sleep 30` process spawned by the fake CLI, returning its
/// pid, so AC6 can assert it is reaped after kill. Uses `pgrep`; None if absent.
fn find_sleep_pid() -> Option<i32> {
    let out = Command::new("pgrep")
        .args(["-f", "sleep 30"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<i32>().ok())
}

