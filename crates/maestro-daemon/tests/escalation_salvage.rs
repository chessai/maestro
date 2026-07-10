//! WORK-LOSS regression: a tier ESCALATION must NOT destroy the lower tier's
//! committed fix-in-place checkpoint.
//!
//! The confirmed bug (a live run): a tier-0 attempt fails the mechanical gate
//! (`checks_failed`); the L15 durability logic COMMITS the worker's in-allowlist
//! edits to `maestro/<task_id>` (the "fix-in-place checkpoint"); after two
//! tier-0 failures the pipeline ESCALATES; the escalated attempt calls
//! `worktree::create`, which force-deletes the branch (`git branch -D`) before
//! cutting a fresh worktree off `base_ref`. The committed checkpoint is GONE —
//! and in the live case the higher tier could not run (out of credits), so the
//! near-complete lower-tier work (7/8 tests) was unrecoverable.
//!
//! The fix: before `create` force-deletes the branch, it salvages any commits
//! beyond `base_ref` to a durable `refs/maestro/salvage/<task_id>/<sha>` ref.
//! Escalation semantics are unchanged (the escalated tier still starts fresh off
//! `base_ref`), but the committed checkpoint is now RECOVERABLE.
//!
//! Round-trip this against the REAL daemon (mock implementer + mock verifier):
//!   - tier-0 writes `src/impl.rs`, the check command always fails →
//!     `checks_failed` (the checkpoint is committed each time);
//!   - after two tier-0 failures the loop ESCALATES to tier-1, whose fresh cut
//!     force-deletes `maestro/<task_id>`;
//!   - ASSERT a salvage ref exists AFTER escalation and carries `src/impl.rs`.
//!
//! Against the PRE-FIX code the branch is force-deleted with no salvage: NO
//! salvage ref exists and `git show <salvage>:src/impl.rs` fails → this test
//! FAILS. Against the FIXED code the checkpoint is preserved → it PASSES.
//!
//! In its OWN test binary: `paths::*`/config read process-global env, so keeping
//! this isolated avoids the cross-test env races the sibling tests warn about.

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
        "maestro-escalation-salvage-{}-{}",
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

/// The salvage refs currently under `refs/maestro/salvage/<task_id>/`.
fn salvage_refs(repo: &Path, task_id: &str) -> Vec<String> {
    let prefix = format!("refs/maestro/salvage/{task_id}/");
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["for-each-ref", "--format=%(refname)", &prefix])
        .output()
        .expect("spawn git for-each-ref");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn m_escalation_salvages_the_lower_tier_committed_checkpoint() {
    let tmp = unique_tmp();
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);

    // Force mock implementer at every tier + a mock verifier floor, so tier-0 and
    // tier-1 both exist (escalation has somewhere to go).
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

    // The mock implementer writes the near-complete implementation each attempt.
    let instructions = serde_json::json!({
        "writes": [ { "path": "src/impl.rs", "content": "pub fn near_complete() {}\n" } ]
    })
    .to_string();

    // The check command ALWAYS fails → `checks_failed` on every attempt. The gate
    // commits `src/impl.rs` (in the allowlist) to the task branch as the
    // fix-in-place checkpoint BEFORE returning. Two failures at tier-0 escalate
    // to tier-1; tier-1's fresh cut force-deletes `maestro/<task_id>` — which,
    // pre-fix, DESTROYS the committed checkpoint.
    let check_cmd = "echo 'always fails (simulates the 8th failing test)' 1>&2; exit 1".to_string();

    let s = TaskSpec {
        title: "escalation must not lose the checkpoint".into(),
        tier: Tier::T0,
        base_ref: "HEAD".into(),
        file_allowlist: vec!["src/impl.rs".into()],
        instructions,
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC1".into(),
            check: "the implementation exists".into(),
            kind: CriterionKind::Invariant,
        }],
        check_commands: vec![check_cmd],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    };

    let task = delegate(&socket, &advisor, &repo_path, s);
    // The check always fails, so the task rides the ladder to `blocked` (top tier
    // still fails). We only care that at least one escalation happened and the
    // pre-escalation checkpoint was salvaged.
    let state = poll_terminal(&socket, &advisor, &task, Duration::from_secs(90));
    assert_eq!(
        state, "blocked",
        "the always-failing check rides the ladder to blocked (got {state})"
    );

    let kinds = event_kinds(&task);
    assert!(
        kinds.iter().any(|k| k == "escalated"),
        "the run must ESCALATE at least once (that is the destructive re-cut), got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "checks_failed"),
        "tier-0 must produce a checks_failed (which commits the checkpoint), got {kinds:?}"
    );

    // THE WORK-LOSS ASSERTION: after the escalation force-deleted the task branch,
    // the lower tier's committed checkpoint must still be RECOVERABLE via a
    // salvage ref. Pre-fix there is no salvage ref and the work is gone.
    let refs = salvage_refs(&repo, &task);
    assert!(
        !refs.is_empty(),
        "escalation must PRESERVE the tier-0 committed checkpoint to a salvage ref \
         (refs/maestro/salvage/{task}/<sha>); found none → the committed work was \
         DESTROYED by the escalation re-cut (the bug). events: {kinds:?}"
    );

    // The salvaged ref must carry the actual committed implementation file — the
    // real recoverability invariant (not just an empty ref).
    let carries_impl = refs.iter().any(|r| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["show", &format!("{r}:src/impl.rs")])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("near_complete"))
            .unwrap_or(false)
    });
    assert!(
        carries_impl,
        "a salvage ref must carry the committed src/impl.rs (recoverable work), refs: {refs:?}"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
