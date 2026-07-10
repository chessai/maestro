//! End-to-end integration test for the DRIVEN-CLI verifier (ADR-002).
//!
//! A `driven_cli` verifier role drives the local subscription `claude` CLI
//! read-only to produce the structured verdict, WITHOUT the Anthropic API (no API
//! credits). Here the "claude CLI" is a fake bash script (no real claude, no
//! network) that, in `--permission-mode plan`, emits stream-json whose assistant
//! text is a fenced ```json verdict block — exactly what the daemon's driven
//! verifier captures and parses.
//!
//! The implementer is a `driven_cli` role driving a separate fake that writes the
//! allowlisted file, so the whole worker→gate→verifier pipeline runs against a
//! real worktree with a real gate round-trip (the CLAUDE.md behavioral-test bar).
//!
//! Like the other daemon integration tests, config + `paths::*` read process-
//! global env, so the whole exercise lives in ONE `#[test]`.

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
        "maestro-driven-verifier-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() as u64
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
    assert!(out.status.success(), "git {:?} failed", args);
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

fn write_fake_cli(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    let script = format!("#!/usr/bin/env bash\nset -u\n{body}\n");
    std::fs::write(&path, script).unwrap();
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().to_string()
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

/// Read the persisted verifier report (JSON) for a task from the journal, if any.
fn verifier_report(task_id: &str) -> Option<serde_json::Value> {
    let db_path = maestro_journal::paths::journal_db_path();
    let journal = maestro_journal::Journal::open(db_path.to_str().unwrap()).ok()?;
    let reports = journal.verifier_reports_for_task(task_id).ok()?;
    reports
        .last()
        .and_then(|r| serde_json::from_str(&r.report).ok())
}

#[test]
fn driven_cli_verifier_produces_verdict_without_api() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);
    std::env::remove_var("MAESTRO_WATCHDOG_SECONDS");

    let scripts = tmp.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();

    // The IMPLEMENTER fake: a two-phase claude adapter that plans "create" then
    // writes the allowlisted file in the acceptEdits phase.
    let impl_fake = write_fake_cli(
        &scripts,
        "impl-claude.sh",
        r#"mode=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--permission-mode" ]; then mode="$arg"; fi
    prev="$arg"
done
if [ "$mode" = "plan" ]; then
    printf '{"type":"system","subtype":"init","session_id":"s1"}\n'
    printf '{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"PLAN: I will create src/added.rs."}]}}\n'
    printf '{"type":"result","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":11,"output_tokens":7}}\n'
elif [ "$mode" = "acceptEdits" ]; then
    printf '{"type":"system","subtype":"init","session_id":"s2"}\n'
    printf '{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"creating the file"}]}}\n'
    mkdir -p src
    printf 'pub fn added() {}\n' > src/added.rs
    sync
    printf '{"type":"result","num_turns":2,"total_cost_usd":0.02,"usage":{"input_tokens":13,"output_tokens":9}}\n'
fi
exit 0"#,
    );

    // The VERIFIER fake: a SINGLE plan-mode phase whose assistant text is a fenced
    // ```json PASS verdict. The inner double-quotes are JSON-escaped (\\") and
    // newlines are \\n so the whole stream-json line is itself valid JSON. It must
    // be invoked with --permission-mode plan (read-only) and must NOT write files.
    let verify_fake = write_fake_cli(
        &scripts,
        "verify-claude.sh",
        r#"mode=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--permission-mode" ]; then mode="$arg"; fi
    prev="$arg"
done
# A read-only verifier must NEVER be asked to edit; refuse acceptEdits loudly.
if [ "$mode" != "plan" ]; then
    printf 'verify-claude: unexpected permission-mode %s\n' "$mode" >&2
    exit 3
fi
printf '{"type":"system","subtype":"init","session_id":"v1"}\n'
printf '{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"Reviewed the diff and gate. Verdict:\\n\\n```json\\n{\\"verdict\\": \\"pass\\", \\"findings\\": [], \\"out_of_scope_diff\\": false, \\"commands_run\\": []}\\n```"}]}}\n'
printf '{"type":"result","num_turns":1,"total_cost_usd":0.01,"usage":{"input_tokens":20,"output_tokens":5}}\n'
exit 0"#,
    );

    // Profile: implementer is a claude-adapter driven role; verifier_floor is a
    // `driven_cli` role with a NON-mock model (so it is NOT treated as the mock
    // verifier) that drives the verify fake. No adapter on the verifier role: the
    // driven verifier always runs a single read-only plan phase.
    let cfg_dir = tmp.join("maestro");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"
default_profile = "dv"
[defaults]
concurrency.machine_cap = 4

[profiles.dv]
roles.tier0 = {{ model = "mock", kind = "driven_cli", command = "bash", args = ["{impl}"], adapter = "claude" }}
roles.verifier_floor = {{ model = "claude-verifier", kind = "driven_cli", command = "bash", args = ["{verify}"] }}
"#,
            impl = impl_fake,
            verify = verify_fake,
        ),
    )
    .unwrap();

    let repo = init_repo(&tmp);

    std::env::set_var("MAESTRO_PROFILE", "dv");
    let server = Server::start(Options {
        profile: Some("dv".into()),
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
            profile: Some("dv".into()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let spec = TaskSpec {
        title: "driven verifier: add a file".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/added.rs".into()],
        instructions: "create src/added.rs".into(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC1".into(),
            check: "src/added.rs exists".into(),
            kind: CriterionKind::Invariant,
        }],
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    };

    let task_id = delegate(&socket, &advisor, &repo, spec);
    let state = poll_terminal(&socket, &advisor, &task_id, Duration::from_secs(45));

    assert_eq!(
        state, "verify_passed",
        "the DRIVEN-CLI verifier must produce a PASS verdict (no API), reaching verify_passed"
    );

    // The persisted report is the driven verifier's PASS verdict.
    let report = verifier_report(&task_id).expect("a verifier report was persisted");
    assert_eq!(
        report.get("verdict").and_then(|v| v.as_str()),
        Some("pass"),
        "persisted report verdict is pass, got {report:?}"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
