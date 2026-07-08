//! Integration test for the advisor-initiated `merge_task` fast-forward merge.
//!
//! Drives the daemon server in-process against a real temp git repo with the
//! `"mock"` implementer + `"mock"` verifier backends (same harness as the M2
//! verify/escalation test). A task that PASSES rests in `verify_passed` with its
//! branch committed but unmerged; an explicit `MergeTask` fast-forwards the base
//! branch and records a `merged` event. This is NOT auto-merge — the daemon
//! never merges on its own.
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
        "maestro-merge-test-{}-{}",
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

fn rev(repo: &Path, r: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", r])
        .output()
        .expect("spawn git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A Tier-0 spec whose mock implementer writes an allowlisted file. `pass`
/// controls whether a `mock:pass` acceptance criterion is present.
fn spec(base_ref: &str, write_path: &str, pass: bool) -> TaskSpec {
    let instructions = serde_json::json!({
        "writes": [ { "path": write_path, "content": "pub fn added() {}\n" } ]
    })
    .to_string();
    let mut criteria = vec![AcceptanceCriterion {
        id: "AC1".into(),
        check: "the file exists".into(),
        kind: CriterionKind::Invariant,
    }];
    if pass {
        criteria.push(AcceptanceCriterion {
            id: "AC2".into(),
            check: "mock:pass".into(),
            kind: CriterionKind::Invariant,
        });
    }
    TaskSpec {
        title: "add a file".into(),
        tier: Tier::T0,
        base_ref: base_ref.into(),
        file_allowlist: vec![write_path.into()],
        instructions,
        acceptance_criteria: criteria,
        check_commands: vec![],
        house_rules_ref: None,
        budget: Default::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
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

#[test]
fn advisor_merge_task_fast_forwards_and_records_merged() {
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
    let base_commit = rev(&repo, "main");
    let repo_path = repo.to_string_lossy().to_string();
    // Detach HEAD so `main` is NOT the currently checked-out branch. The daemon's
    // worktrees live elsewhere, but detaching exercises the working-tree-free
    // `update-ref` fast-forward path (the common daemon case).
    git(&repo, &["checkout", "-q", "--detach", &base_commit]);

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

    // ---- A passing task rests in verify_passed, branch committed, unmerged ----
    let pass_task = delegate(&socket, &advisor, &repo_path, spec("main", "src/ok.rs", true));
    let state = poll_terminal(&socket, &advisor, &pass_task, Duration::from_secs(30));
    assert_eq!(state, "verify_passed", "mock:pass → verify_passed");
    assert!(branch_exists(&repo, &pass_task), "branch committed");
    let task_tip = rev(&repo, &format!("maestro/{pass_task}"));
    assert_eq!(rev(&repo, "main"), base_commit, "not merged yet");
    assert_ne!(task_tip, base_commit, "task branch has a commit ahead of base");

    // ---- Merging a NON-passed task is an Error, no merged event ----
    // A fresh, still-running/failing task id: use a bogus id which is not
    // verify_passed (never delegated) → error path.
    let bogus = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: "01BOGUSNOTATASK00000000000".into(),
        },
    );
    assert!(
        matches!(bogus, Response::Error { .. }),
        "merging an unknown/non-passed task is an Error, got {bogus:?}"
    );

    // ---- Explicit merge_task fast-forwards main and records `merged` ----
    let merged = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: pass_task.clone(),
        },
    );
    match merged {
        Response::Merged { task_id } => assert_eq!(task_id, pass_task),
        other => panic!("expected Merged, got {other:?}"),
    }
    // main was fast-forwarded to the task tip.
    assert_eq!(rev(&repo, "main"), task_tip, "main advanced to task tip");
    // The `merged` event is in the trace with the expected payload.
    let kinds = event_kinds(&pass_task);
    assert_eq!(kinds.last().map(String::as_str), Some("merged"), "latest is merged, got {kinds:?}");
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let chain = journal.event_chain(&pass_task).unwrap();
        let ev = chain.last().unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(ev.payload.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(payload["base_ref"], "main");
        assert_eq!(payload["branch"], format!("maestro/{pass_task}"));
        assert_eq!(payload["merged_sha"], task_tip);
    }
    // Post-merge cleanup: the task branch was best-effort deleted.
    assert!(!branch_exists(&repo, &pass_task), "task branch deleted after merge");

    // ---- Merging an already-merged task is an Error ----
    let again = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: pass_task.clone(),
        },
    );
    match again {
        Response::Error { message } => {
            assert!(message.contains("already merged"), "got: {message}");
        }
        other => panic!("expected Error on re-merge, got {other:?}"),
    }

    // ---- FIX 1: a spec with base_ref="HEAD" resolves to the current branch ----
    // Re-checkout `main` (it was previously detached) so HEAD is symbolic → the
    // delegate-time `resolve_base_ref` turns "HEAD" into the branch name "main",
    // which is what gets persisted AND what `merge_task` needs to fast-forward.
    git(&repo, &["checkout", "-q", "main"]);
    let main_before = rev(&repo, "main");
    let head_task = delegate(&socket, &advisor, &repo_path, spec("HEAD", "src/head.rs", true));
    let head_state = poll_terminal(&socket, &advisor, &head_task, Duration::from_secs(30));
    assert_eq!(head_state, "verify_passed", "HEAD base_ref task passes");
    // The task row persisted the RESOLVED base_ref ("main"), not "HEAD".
    {
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let (_repo, stored_base) = journal.task_repo_and_base(&head_task).unwrap();
        assert_eq!(stored_base, "main", "base_ref 'HEAD' resolved+stored as 'main'");
    }
    let head_tip = rev(&repo, &format!("maestro/{head_task}"));
    // merge_task fast-forwards `main` to the task tip — impossible if "HEAD" had
    // been stored raw (it is not a local branch).
    let head_merged = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: head_task.clone(),
        },
    );
    match head_merged {
        Response::Merged { task_id } => assert_eq!(task_id, head_task),
        other => panic!("expected Merged for HEAD-based task, got {other:?}"),
    }
    assert_ne!(head_tip, main_before, "task added a commit");
    assert_eq!(rev(&repo, "main"), head_tip, "main advanced to the HEAD-based task tip");

    // ---- PARALLEL BATCH: two disjoint tasks off the same base; ff then 3-way ----
    // Both branch off the SAME `main` tip. Merging the first fast-forwards main;
    // the second is then DIVERGED (its base moved under it) but touches a
    // DIFFERENT file, so it must integrate via a conflict-free 3-way merge.
    let batch_base = rev(&repo, "main");
    let card_a = delegate(&socket, &advisor, &repo_path, spec("main", "src/card_a.rs", true));
    let card_b = delegate(&socket, &advisor, &repo_path, spec("main", "src/card_b.rs", true));
    assert_eq!(
        poll_terminal(&socket, &advisor, &card_a, Duration::from_secs(30)),
        "verify_passed",
        "card A passes"
    );
    assert_eq!(
        poll_terminal(&socket, &advisor, &card_b, Duration::from_secs(30)),
        "verify_passed",
        "card B passes"
    );
    // Both cards branched off the same base commit.
    let tip_a = rev(&repo, &format!("maestro/{card_a}"));
    let tip_b = rev(&repo, &format!("maestro/{card_b}"));
    assert_ne!(tip_a, batch_base, "card A has a commit");
    assert_ne!(tip_b, batch_base, "card B has a commit");

    // Merge A first → fast-forward (base is still an ancestor).
    let merged_a = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: card_a.clone(),
        },
    );
    assert!(matches!(merged_a, Response::Merged { .. }), "card A merges, got {merged_a:?}");
    assert_eq!(rev(&repo, "main"), tip_a, "main fast-forwarded to card A tip");
    {
        let ka = event_kinds(&card_a);
        assert_eq!(ka.last().map(String::as_str), Some("merged"), "card A merged journaled: {ka:?}");
    }

    // Merge B second → main has DIVERGED from B's base; disjoint files → 3-way merge.
    let merged_b = round_trip(
        &socket,
        &Request::MergeTask {
            advisor_session_id: advisor.clone(),
            task_id: card_b.clone(),
        },
    );
    assert!(matches!(merged_b, Response::Merged { .. }), "card B 3-way merges, got {merged_b:?}");
    {
        let kb = event_kinds(&card_b);
        assert_eq!(kb.last().map(String::as_str), Some("merged"), "card B merged journaled: {kb:?}");
        // The merged event carries fast_forward=false for the diverged card.
        let db_path = maestro_journal::paths::journal_db_path();
        let journal =
            maestro_journal::Journal::open(db_path.to_str().unwrap()).expect("open journal");
        let chain = journal.event_chain(&card_b).unwrap();
        let ev = chain.last().unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(ev.payload.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(payload["fast_forward"], false, "card B was a 3-way merge, not a ff");
    }
    // The base branch ends up containing BOTH cards' files — the parallel batch
    // integrated cleanly.
    let main_final = rev(&repo, "main");
    assert!(
        Command::new("git")
            .arg("-C").arg(&repo)
            .args(["cat-file", "-e", &format!("{main_final}:src/card_a.rs")])
            .output().unwrap().status.success(),
        "main contains card A's file"
    );
    assert!(
        Command::new("git")
            .arg("-C").arg(&repo)
            .args(["cat-file", "-e", &format!("{main_final}:src/card_b.rs")])
            .output().unwrap().status.success(),
        "main contains card B's file"
    );

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    let _ = std::fs::remove_dir_all(&tmp);
}
