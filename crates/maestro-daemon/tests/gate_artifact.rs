//! FIX 2 integration test: the committed task branch + the verifier's diff must
//! contain ONLY the implementer's allowlisted changes, never post-build
//! artifacts created by a check command — even when the repo has NO `.gitignore`.
//!
//! The scope check runs BEFORE the check commands, so the gate captures the
//! implementer's clean in-allowlist changed set at that point. The pipeline then
//! restricts the verifier diff (`worktree::diff_paths`) and the branch commit
//! (`worktree::commit_paths`) to exactly those paths, so an artifact a check
//! command drops (here a `stray_artifact` file outside the allowlist) never
//! reaches the committed branch. This drives the real daemon with the mock
//! implementer + mock verifier (same harness as `merge.rs`).
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
        "maestro-gate-artifact-test-{}-{}",
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

/// Init a repo with NO `.gitignore` — so a plain `git add -A` WOULD stage any
/// artifact a check command creates. That is exactly what FIX 2 must resist.
fn init_repo_no_gitignore(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "tester"]);
    std::fs::write(repo.join("README.md"), "initial\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    assert!(!repo.join(".gitignore").exists(), "repo intentionally has no .gitignore");
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

/// The committed contents of `maestro/<task_id>` HEAD as a list of file paths
/// (`git show --name-only`).
fn committed_files(repo: &Path, task_id: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "show",
            "--name-only",
            "--pretty=format:",
            &format!("maestro/{task_id}"),
        ])
        .output()
        .expect("spawn git show");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn committed_branch_excludes_check_command_artifacts_without_gitignore() {
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

    let repo = init_repo_no_gitignore(&tmp);
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

    // A spec whose mock implementer writes ONE allowlisted source file, and whose
    // check command CREATES an out-of-allowlist artifact (`stray_artifact`). With
    // no `.gitignore`, a naive `add -A` would stage `stray_artifact` into both the
    // verifier diff and the committed branch. FIX 2 must keep it out.
    let write_path = "src/only.rs";
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn only() {}\n" } ]
    })
    .to_string();
    let spec = TaskSpec {
        title: "write one allowlisted file".into(),
        tier: Tier::T0,
        base_ref: "main".into(),
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
        // The check command drops an out-of-allowlist artifact AFTER the scope
        // check has captured the clean changed set. It exits 0 so the gate passes.
        check_commands: vec!["echo artifact > stray_artifact".into()],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    };

    let task = delegate(&socket, &advisor, &repo_path, spec);
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "artifact-dropping check still passes the gate");

    // `verify_passed` is emitted AFTER the branch commit lands, so the committed
    // file set is observable immediately.
    let files = committed_files(&repo, &task);
    assert!(files.contains(write_path), "committed the allowlisted file, got: {files:?}");
    assert!(
        !files.contains("stray_artifact"),
        "committed branch must NOT contain the check-command artifact, got: {files:?}"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
