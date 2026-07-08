//! Hermetic integration tests for the two-phase Claude-CLI adapter
//! (`run_claude_driven`). No real `claude` and no network: the "claude CLI" is a
//! small bash script that inspects its args for the permission mode and either
//! prints a plan (plan mode, edits nothing) or writes the target file
//! (acceptEdits mode). The adapter runs `program=bash`, `args=[<fake.sh>]` and
//! appends `--permission-mode <mode>` + the prompt, exactly as the daemon would
//! for the real `claude --print`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maestro_driver::{
    pid_alive, run_claude_driven, DrivenConfig, EndReason, KillKind, MockPlanChecker,
};
use maestro_journal::domain::Tier;
use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind, TaskSpec};
use tempfile::TempDir;

/// Write the fake `claude` CLI to `path` and mark it executable.
///
/// It emulates claude's two permission modes by scanning ITS OWN args (the
/// adapter appends `--permission-mode <mode>` and the prompt):
/// - `plan` present  → print a plan line, write NOTHING, exit 0.
/// - `acceptEdits`   → write `$TARGET`, exit 0 (or, in the hang variant, sleep).
///
/// The plan text and target path come from `$1`/`$2` (set by the base args) so
/// concurrent in-process tests never race on shared global env.
fn write_fake_claude(path: &Path) {
    let script = r#"#!/usr/bin/env bash
set -u
# Base args (passed by the test as the leading args): the plan phrase and the
# target path. The adapter appends `--permission-mode <mode>` and the prompt.
PLAN_PHRASE="${1:-I will create the file as specified}"
TARGET="${2:-out.txt}"

mode=""
for a in "$@"; do
  case "$a" in
    plan) mode="plan" ;;
    acceptEdits) mode="acceptEdits" ;;
  esac
done

case "${mode}" in
  plan)
    # Plan mode: produce a plan, edit NOTHING.
    echo "${PLAN_PHRASE}"
    exit 0
    ;;
  acceptEdits)
    case "${PLAN_PHRASE}" in
      *HANG*)
        # Hang variant for the kill/watchdog tests: no output, sleep forever.
        sleep 3000
        exit 0
        ;;
    esac
    printf 'written by fake claude\n' > "${TARGET}"
    exit 0
    ;;
  *)
    echo "fake-claude: no permission mode in args: $*" >&2
    exit 2
    ;;
esac
"#;
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

fn spec() -> TaskSpec {
    TaskSpec {
        title: "Create target".into(),
        tier: Tier::T1,
        base_ref: "main".into(),
        file_allowlist: vec!["out.txt".into()],
        instructions: "Create the target file".into(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC1".into(),
            check: "test -f out.txt".into(),
            kind: CriterionKind::Invariant,
        }],
        check_commands: vec![],
        house_rules_ref: None,
        budget: Budget::default(),
        lifetime_budget: Default::default(),
        containment_min: 0,
    }
}

struct Fixture {
    _dir: TempDir,
    cli: PathBuf,
    target: PathBuf,
    log: PathBuf,
    cwd: PathBuf,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().join("worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    let cli = dir.path().join("fake_claude.sh");
    write_fake_claude(&cli);
    let target = cwd.join("out.txt");
    let log = dir.path().join("session.log");
    Fixture {
        _dir: dir,
        cli,
        target,
        log,
        cwd,
    }
}

/// Build the config the way the daemon does for `claude --print`: base
/// `program`/`args` only; the adapter appends `--permission-mode <mode>` and the
/// prompt. Here `program=bash`, and the leading args are the fake script, the
/// plan phrase, and the target path.
fn config(f: &Fixture, plan_phrase: &str, watchdog: Duration) -> DrivenConfig {
    DrivenConfig {
        program: "bash".into(),
        args: vec![
            f.cli.to_string_lossy().to_string(),
            plan_phrase.to_string(),
            f.target.to_string_lossy().to_string(),
        ],
        cwd: f.cwd.clone(),
        prompt: "please implement the task".into(),
        log_path: f.log.clone(),
        watchdog,
        // Unused by the two-phase adapter (plan comes from phase-1 stdout), but
        // required by the shared config shape.
        plan_marker: "PLAN:".into(),
        plan_timeout: Duration::from_secs(10),
        env_remove: vec![],
    }
}

/// AC4: accept → edits. Plan mentions "create" → MockPlanChecker accepts → phase
/// 2 runs and writes the target → Completed AND the target exists.
#[test]
fn ac4_accept_runs_phase_two_and_writes_target() {
    let f = fixture();
    let cfg = config(&f, "I will create the file as specified", Duration::from_secs(15));
    let (_handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    assert_eq!(result.reason, EndReason::Completed, "log at {:?}", f.log);
    assert!(
        f.target.is_file(),
        "phase 2 (acceptEdits) should have created the target"
    );
    assert_eq!(
        std::fs::read_to_string(&f.target).unwrap(),
        "written by fake claude\n"
    );
    assert_eq!(result.turns, 2, "a completed two-phase run counts 2 turns");
    // The plan text was captured to the log (phase 1 stdout).
    let log = std::fs::read_to_string(&f.log).unwrap();
    assert!(log.contains("create the file as specified"), "log: {log:?}");
}

/// AC5: reject → ZERO edits. Plan mentions "delete" → MockPlanChecker rejects →
/// phase 2 never runs → PlanRejected AND the target does NOT exist.
#[test]
fn ac5_reject_makes_zero_edits() {
    let f = fixture();
    let cfg = config(&f, "I will delete everything and rewrite files", Duration::from_secs(15));
    let (_handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    match result.reason {
        EndReason::PlanRejected { .. } => {}
        other => panic!("expected PlanRejected, got {other:?}"),
    }
    assert!(
        !f.target.exists(),
        "rejected plan must abort before phase 2, so no edit is ever made"
    );
    assert_eq!(result.turns, 1, "plan-only rejection counts 1 turn");
}

/// AC6: kill. Plan accepts (phase 1 prints "create"), phase 2 hangs; once phase
/// 2 is running, request_kill(Human) → Killed(Human) and no lingering child.
#[test]
fn ac6_kill_tears_down_running_phase_two() {
    let f = fixture();
    // The plan phrase must (a) contain "create" and not "delete" so
    // MockPlanChecker accepts it in phase 1, and (b) contain "HANG" so the fake
    // sleeps in phase-2 acceptEdits mode (drives the kill path). "create::HANG"
    // satisfies both. Long watchdog so the watchdog does not fire first.
    let cfg = config(&f, "create::HANG", Duration::from_secs(60));
    let (handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();

    // Wait until phase 2 is the running child: phase 1 exits quickly, then the
    // pid slot is re-published for phase 2. We detect "phase 2 running" by the
    // pid being alive AND the plan already echoed to the log.
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        assert!(Instant::now() < deadline, "phase 2 never started");
        if let (Ok(log), Some(pid)) = (std::fs::read_to_string(&f.log), handle.pid()) {
            // The plan line is present (phase 1 done) and the currently-published
            // pid is alive (a running child). Because phase 1 exits fast, a live
            // pid at this point is phase 2.
            if log.contains("create::HANG") && pid_alive(pid) {
                // Give the slot a beat to settle on phase 2's pid, then re-read.
                std::thread::sleep(Duration::from_millis(150));
                if let Some(p2) = handle.pid() {
                    if pid_alive(p2) {
                        break p2;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    };

    handle.request_kill(KillKind::Human);
    // Idempotent: a second request is a no-op.
    handle.request_kill(KillKind::Advisor);

    let result = join.join().unwrap();
    assert_eq!(result.reason, EndReason::Killed(KillKind::Human));

    // No lingering child (poll to let reaping settle).
    let deadline = Instant::now() + Duration::from_secs(6);
    while pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "child pid {pid} still alive after kill"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
