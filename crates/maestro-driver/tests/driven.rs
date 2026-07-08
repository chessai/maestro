//! Hermetic integration tests for the driven-session state machine.
//!
//! No real agent CLI and no network: the "agent" is a small bash script written
//! to a temp file that takes the scenario and target path as args and emulates
//! the plan-echo protocol. These drive real PTY behavior end to end.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maestro_driver::{
    pid_alive, DrivenConfig, DrivenSession, EndReason, KillKind, MockPlanChecker,
};
use maestro_journal::domain::Tier;
use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind, TaskSpec};
use tempfile::TempDir;

/// Write the fake agent CLI to `path` and mark it executable.
fn write_fake_cli(path: &Path) {
    // Scenario and target come as $1/$2 (args), not env, so concurrent tests
    // never race on a shared global.
    let script = r#"#!/usr/bin/env bash
set -u
SCENARIO="${1:-}"
TARGET="${2:-}"
# consume the driver's prompt line (written to the PTY) so it doesn't confuse us
read -r _prompt || true
case "${SCENARIO}" in
  good)
    echo "PLAN: create the file as specified"
    printf 'created by fake\n' > "${TARGET}"
    echo "DONE"
    exit 0
    ;;
  violating)
    echo "PLAN: delete everything and rewrite unrelated files"
    # If the driver failed to reject, we'd get here and make an edit; the test
    # asserts the target is NEVER created, so this sleep gives it the chance.
    sleep 3
    printf 'should never be written\n' > "${TARGET}"
    exit 0
    ;;
  hang)
    echo "PLAN: create the file"
    # No further output: drives the watchdog or an external kill.
    sleep 3000
    exit 0
    ;;
  *)
    echo "unknown scenario" >&2
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
        file_allowlist: vec!["target.txt".into()],
        instructions: "Create the target file".into(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC1".into(),
            check: "test -f target.txt".into(),
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
    let cli = dir.path().join("fake_cli.sh");
    write_fake_cli(&cli);
    let target = cwd.join("target.txt");
    let log = dir.path().join("session.log");
    Fixture {
        _dir: dir,
        cli,
        target,
        log,
        cwd,
    }
}

fn config(f: &Fixture, scenario: &str, watchdog: Duration) -> DrivenConfig {
    // Scenario and target are passed as CLI args, so concurrent in-process tests
    // stay independent (no shared global env).
    DrivenConfig {
        program: f.cli.to_string_lossy().to_string(),
        args: vec![scenario.to_string(), f.target.to_string_lossy().to_string()],
        cwd: f.cwd.clone(),
        prompt: "please implement the task".into(),
        log_path: f.log.clone(),
        watchdog,
        plan_marker: "PLAN:".into(),
        plan_timeout: Duration::from_secs(10),
        env_remove: vec![],
        turn_cap: None,
    }
}

/// AC5: `good` scenario → Completed and the target exists with expected content.
#[test]
fn ac5_success_completes_and_writes_target() {
    let f = fixture();
    let cfg = config(&f, "good", Duration::from_secs(10));
    let (_handle, join) =
        DrivenSession::spawn(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    assert_eq!(result.reason, EndReason::Completed, "log at {:?}", f.log);
    assert!(f.target.is_file(), "target should have been created");
    let content = std::fs::read_to_string(&f.target).unwrap();
    assert_eq!(content, "created by fake\n");
    // Log capture works.
    let log = std::fs::read_to_string(&f.log).unwrap();
    assert!(log.contains("PLAN:"), "log should capture PTY output: {log:?}");
    assert!(log.contains("DONE"));
}

/// AC4: `violating` scenario + MockPlanChecker → PlanRejected and ZERO edits.
#[test]
fn ac4_plan_rejected_makes_zero_edits() {
    let f = fixture();
    let cfg = config(&f, "violating", Duration::from_secs(10));
    let (_handle, join) =
        DrivenSession::spawn(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    match result.reason {
        EndReason::PlanRejected { .. } => {}
        other => panic!("expected PlanRejected, got {other:?}"),
    }
    // The whole point: killed before edits, so the target must NOT exist.
    assert!(
        !f.target.exists(),
        "violating plan must be rejected before any workspace edit"
    );
}

/// AC6: `hang` scenario + short watchdog → Wedged, no lingering child.
#[test]
fn ac6_watchdog_wedges_and_reaps() {
    let f = fixture();
    let cfg = config(&f, "hang", Duration::from_secs(1));
    let (handle, join) =
        DrivenSession::spawn(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();

    let start = Instant::now();
    let result = join.join().unwrap();
    assert_eq!(result.reason, EndReason::Wedged);
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "watchdog should fire within a few seconds"
    );
    // No lingering child.
    if let Some(pid) = handle.pid() {
        assert!(!pid_alive(pid), "child pid {pid} should be gone after wedge");
    }
}

/// AC7: `hang` scenario → after plan accepted, request_kill(Human) → Killed and
/// the child is gone.
#[test]
fn ac7_external_kill_human() {
    let f = fixture();
    // Long watchdog so the watchdog does not fire first.
    let cfg = config(&f, "hang", Duration::from_secs(60));
    let (handle, join) =
        DrivenSession::spawn(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();

    // Wait for the plan to be accepted: the child publishes a pid and the log
    // shows the plan line. Poll the log for the plan echo.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(log) = std::fs::read_to_string(&f.log) {
            if log.contains("PLAN: create the file") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "plan never echoed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(p) = handle.pid() {
                break p;
            }
            assert!(Instant::now() < deadline, "pid never published");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    handle.request_kill(KillKind::Human);
    // Idempotent: a second call is a no-op.
    handle.request_kill(KillKind::Advisor);

    let result = join.join().unwrap();
    assert_eq!(result.reason, EndReason::Killed(KillKind::Human));

    // Child should be gone (poll to allow reaping to settle).
    let deadline = Instant::now() + Duration::from_secs(6);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "child pid {pid} still alive after kill");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// AC4 (env stripping): `env_remove` strips the listed key from the child's
/// environment while leaving all other vars (e.g. PATH) intact. The daemon's
/// own process env is never mutated — only the child's inherited copy.
///
/// Uses a uniquely-prefixed var name so it cannot race with any other test.
#[test]
fn ac4_env_remove_strips_key_preserves_path() {
    // A var name that no other test or the system uses.
    const TEST_VAR: &str = "MAESTRO_TEST_ENV_STRIP_SECRET_12345";

    // Set the var in the current process env BEFORE spawning the child.
    // Safety note: on Unix, set_var is not thread-safe in the presence of
    // concurrent set_var/remove_var calls. We use a unique name so no other
    // test touches this key, and we restore it (remove) at the end regardless.
    // SAFETY: single writer, unique key, no concurrent readers of this key.
    unsafe { std::env::set_var(TEST_VAR, "test-secret") };

    let dir = TempDir::new().unwrap();
    let cwd = dir.path().join("worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    let log = dir.path().join("env_strip_test.log");
    let out = dir.path().join("env_out.txt");

    // Write a script that reports whether the stripped key is visible and
    // whether PATH is still present. Both results go to `out` so we can
    // assert on them after the child exits.
    //
    // The plan line must contain "create" so MockPlanChecker accepts it
    // (the checker accepts plans with "create", rejects "delete").
    let script_path = dir.path().join("env_check.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
read -r _prompt || true
echo "PLAN: create output file to check env visibility"
if [ -n "${{{var}:-}}" ]; then
  echo "KEY_VISIBLE" > "{out}"
else
  echo "KEY_STRIPPED" > "{out}"
fi
if [ -n "${{PATH:-}}" ]; then
  echo "PATH_PRESENT" >> "{out}"
else
  echo "PATH_MISSING" >> "{out}"
fi
exit 0
"#,
        var = TEST_VAR,
        out = out.display(),
    );
    std::fs::write(&script_path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
    }

    let cfg = DrivenConfig {
        program: script_path.to_string_lossy().to_string(),
        args: vec![],
        cwd: cwd.clone(),
        prompt: "check env please".into(),
        log_path: log.clone(),
        watchdog: Duration::from_secs(10),
        plan_marker: "PLAN:".into(),
        plan_timeout: Duration::from_secs(10),
        env_remove: vec![TEST_VAR.to_string()],
        turn_cap: None,
    };

    let (_handle, join) =
        DrivenSession::spawn(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();
    assert_eq!(result.reason, EndReason::Completed, "env-check script should complete; log: {:?}", log);

    let output = std::fs::read_to_string(&out)
        .expect("env-check script should have written its output file");

    assert!(
        output.contains("KEY_STRIPPED"),
        "child should NOT see the stripped env var; got: {output:?}"
    );
    assert!(
        !output.contains("KEY_VISIBLE"),
        "child must not see the stripped env var; got: {output:?}"
    );
    assert!(
        output.contains("PATH_PRESENT"),
        "child must still have PATH after env_remove; got: {output:?}"
    );

    // Restore: remove the test var from the daemon process env so later tests
    // or tools running in the same process are not affected.
    // SAFETY: single writer, unique key.
    unsafe { std::env::remove_var(TEST_VAR) };
}
