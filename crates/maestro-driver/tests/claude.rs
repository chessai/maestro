//! Hermetic integration tests for the STRUCTURED two-phase Claude-CLI adapter
//! (`run_claude_driven`). No real `claude` and no network: the "claude CLI" is a
//! small bash script that inspects its args for the permission mode and emits
//! canned newline-delimited stream-json to stdout (a `system` init, one or more
//! `assistant` events, then a final `result`), exactly the shape the adapter
//! parses. The adapter runs `program=bash`, `args=[<fake.sh>, ...]` and appends
//! `--output-format stream-json --verbose --permission-mode <mode>` + the prompt,
//! exactly as it would for the real `claude --print`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use maestro_driver::{
    json_phase_args, pid_alive, run_claude_driven, DrivenConfig, EndReason, KillKind,
    MockPlanChecker,
};
use maestro_journal::domain::Tier;
use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind, TaskSpec};
use tempfile::TempDir;

/// Write the fake `claude` CLI to `path` and mark it executable.
///
/// It emulates claude's two permission modes by scanning ITS OWN args (the
/// adapter appends `--permission-mode <mode>` and the prompt) and emits canned
/// stream-json. The `plan` mode emits `system` init, ONE `assistant` with the
/// plan text, and a `result` (11 in / 7 out / $0.01), editing NOTHING. The
/// `acceptEdits` mode, per the scenario (`$1`), normally emits `system` + one
/// assistant + a write of `$TARGET` + a metered `result`; special scenarios
/// drive the turn-cap and kill/watchdog paths.
///
/// The scenario and target path come from `$1`/`$2` (the leading base args) so
/// concurrent in-process tests never race on shared global env.
fn write_fake_claude(path: &Path) {
    let script = r#"#!/usr/bin/env bash
set -u
# Base args (passed by the test as the leading args): the SCENARIO and the
# target path. The adapter appends the stream-json flags + `--permission-mode
# <mode>` + the prompt.
SCENARIO="${1:-plain}"
TARGET="${2:-out.txt}"

mode=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--permission-mode" ]; then mode="$a"; fi
  prev="$a"
done

emit_init() {
  printf '{"type":"system","subtype":"init","session_id":"s1"}\n'
}
# $1 = plan/text for the assistant turn.
emit_assistant() {
  printf '{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"%s"}]}}\n' "$1"
}
# $1 in, $2 out, $3 cost, $4 num_turns.
emit_result() {
  printf '{"type":"result","num_turns":%s,"total_cost_usd":%s,"usage":{"input_tokens":%s,"output_tokens":%s}}\n' "$4" "$3" "$1" "$2"
}

case "${mode}" in
  plan)
    emit_init
    case "${SCENARIO}" in
      *reject*) emit_assistant "PLAN: I will delete everything and rewrite files" ;;
      *)        emit_assistant "PLAN: I will create the file as specified" ;;
    esac
    emit_result 11 7 0.01 1
    exit 0
    ;;
  acceptEdits)
    case "${SCENARIO}" in
      turncap)
        # Emit 4 assistant turns with NO timely exit, so a turn_cap of 2 trips
        # while the process is still running. Sleep after so the poll loop
        # observes > cap before the child would exit.
        emit_init
        emit_assistant "turn 1"
        emit_assistant "turn 2"
        emit_assistant "turn 3"
        emit_assistant "turn 4"
        sleep 3000
        exit 0
        ;;
      hang)
        # No output at all: drives the watchdog / external kill.
        sleep 3000
        exit 0
        ;;
      garbage)
        # Interleave a non-JSON line and an unknown event type; the adapter must
        # skip them and still parse the assistant + result + write the file.
        emit_init
        printf 'this is not json at all\n'
        printf '{"type":"rate_limit_event","retry_after":1}\n'
        emit_assistant "doing the work"
        printf 'written by fake claude\n' > "${TARGET}"
        emit_result 11 7 0.01 2
        exit 0
        ;;
      *)
        emit_init
        emit_assistant "doing the work"
        printf 'written by fake claude\n' > "${TARGET}"
        emit_result 11 7 0.01 2
        exit 0
        ;;
    esac
    ;;
  *)
    printf 'fake-claude: no permission mode in args: %s\n' "$*" >&2
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
/// `program`/`args` only; the adapter appends the stream-json flags,
/// `--permission-mode <mode>`, and the prompt. Here `program=bash`, and the
/// leading args are the fake script, the SCENARIO, and the target path.
fn config(f: &Fixture, scenario: &str, watchdog: Duration, turn_cap: Option<u32>) -> DrivenConfig {
    DrivenConfig {
        program: "bash".into(),
        args: vec![
            f.cli.to_string_lossy().to_string(),
            scenario.to_string(),
            f.target.to_string_lossy().to_string(),
        ],
        cwd: f.cwd.clone(),
        prompt: "please implement the task".into(),
        log_path: f.log.clone(),
        watchdog,
        // Unused by the structured adapter (plan comes from phase-1 assistant
        // events), but required by the shared config shape.
        plan_marker: "PLAN:".into(),
        plan_timeout: Duration::from_secs(10),
        env_remove: vec![],
        turn_cap,
        max_budget_usd: None,
    }
}

/// Plan phase parses the FIRST assistant event's text; MockPlanChecker accepts
/// (plan mentions "create") → phase 2 runs and writes the target → Completed.
/// Metering from the final `result` is surfaced on the DrivenResult.
#[test]
fn accept_runs_phase_two_and_writes_target_metered() {
    let f = fixture();
    let cfg = config(&f, "plain", Duration::from_secs(15), None);
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

    // Metering: both phases reported a result (11 in / 7 out / $0.01), so the
    // sums are 22 / 14 / $0.02.
    assert_eq!(result.tokens_in, Some(22), "summed input tokens");
    assert_eq!(result.tokens_out, Some(14), "summed output tokens");
    assert_eq!(result.cost_usd, Some(0.02), "summed cost");
    // Turn count: result.num_turns 1 (plan) + 2 (execute) = 3.
    assert_eq!(result.turns, 3, "real turn count from result.num_turns sums");

    // The stream-json was captured to the log (phase 1 + phase 2 stdout).
    let log = std::fs::read_to_string(&f.log).unwrap();
    assert!(log.contains("\"type\":\"assistant\""), "log: {log:?}");
}

/// A single-phase metering assertion: the plan phase alone reports
/// `input_tokens=11, output_tokens=7, total_cost_usd=0.01`, and a REJECTED plan
/// (phase 2 never runs) surfaces exactly that phase-1 metering.
#[test]
fn metering_from_result_event_single_phase() {
    let f = fixture();
    let cfg = config(&f, "reject", Duration::from_secs(15), None);
    let (_handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    match result.reason {
        EndReason::PlanRejected { .. } => {}
        other => panic!("expected PlanRejected, got {other:?}"),
    }
    assert!(!f.target.exists(), "rejected plan must make zero edits");
    // Only the plan phase ran → its result event's metering, verbatim.
    assert_eq!(result.tokens_in, Some(11));
    assert_eq!(result.tokens_out, Some(7));
    assert_eq!(result.cost_usd, Some(0.01));
}

/// Reject → ZERO edits. Plan mentions "delete" → MockPlanChecker rejects → phase
/// 2 never runs → PlanRejected AND the target does NOT exist.
#[test]
fn reject_makes_zero_edits() {
    let f = fixture();
    let cfg = config(&f, "reject", Duration::from_secs(15), None);
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
}

/// TURN CAP: the execute phase emits 4 assistant events (then sleeps, no timely
/// exit) with `turn_cap = Some(2)` → the adapter observes assistant_turns > cap,
/// tears the phase down mid-session, and returns TurnBudgetExceeded. No lingering
/// child.
#[test]
fn turn_cap_hard_stops_execute_phase() {
    let f = fixture();
    // Plan mentions "create" → accepted; execute scenario "turncap" emits 4 turns.
    let cfg = config(&f, "turncap", Duration::from_secs(60), Some(2));
    let (handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();

    let result = join.join().unwrap();
    assert_eq!(
        result.reason,
        EndReason::TurnBudgetExceeded,
        "4 turns with a cap of 2 must hard-stop; log at {:?}",
        f.log
    );
    // No lingering child (poll to let reaping settle).
    if let Some(pid) = handle.pid() {
        let deadline = Instant::now() + Duration::from_secs(6);
        while pid_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "child pid {pid} still alive after turn-cap teardown"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// GARBAGE lines: the execute stream mixes a non-JSON line and an unknown
/// `rate_limit_event` in with the real events. The adapter must skip them and
/// still complete, write the file, and surface the metering from the `result`.
#[test]
fn garbage_and_unknown_lines_are_ignored() {
    let f = fixture();
    let cfg = config(&f, "garbage", Duration::from_secs(15), None);
    let (_handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();
    let result = join.join().unwrap();

    assert_eq!(result.reason, EndReason::Completed, "log at {:?}", f.log);
    assert!(f.target.is_file(), "the real edit must still land");
    assert_eq!(result.tokens_in, Some(22), "metering parsed despite garbage");
    assert_eq!(result.tokens_out, Some(14));
}

/// KILL: plan accepts (phase 1 emits a "create" plan), phase 2 hangs; once phase
/// 2 is running, request_kill(Human) → Killed(Human) and no lingering child.
#[test]
fn kill_tears_down_running_phase_two() {
    let f = fixture();
    // "hang" execute scenario sleeps forever with no output. Long watchdog so the
    // watchdog does not fire first. No turn cap.
    let cfg = config(&f, "hang", Duration::from_secs(60), None);
    let (handle, join) = run_claude_driven(cfg, spec(), Arc::new(MockPlanChecker)).unwrap();

    // Wait until phase 2 is the running child: phase 1 exits quickly, then the
    // pid slot is re-published for phase 2. We detect "phase 2 running" by the
    // plan already parsed (its assistant event is in the log) AND a live pid.
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        assert!(Instant::now() < deadline, "phase 2 never started");
        if let (Ok(log), Some(pid)) = (std::fs::read_to_string(&f.log), handle.pid()) {
            if log.contains("I will create the file as specified") && pid_alive(pid) {
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

/// `json_phase_args` with `max_budget_usd = Some(5.0)` inserts
/// `--max-budget-usd 5` immediately before the prompt (last arg).
#[test]
fn json_phase_args_includes_max_budget_usd_when_set() {
    let base: Vec<String> = vec!["--print".into()];
    let args = json_phase_args(&base, "plan", "do the thing", Some(5.0));

    // The argv must contain --max-budget-usd followed by the formatted amount.
    let pos = args
        .iter()
        .position(|a| a == "--max-budget-usd")
        .expect("--max-budget-usd must be present when max_budget_usd is Some");
    assert_eq!(
        args.get(pos + 1).map(|s| s.as_str()),
        Some("5"),
        "--max-budget-usd value must be '5'"
    );

    // The prompt must still be the last argument.
    assert_eq!(
        args.last().map(|s| s.as_str()),
        Some("do the thing"),
        "prompt must remain the last argument"
    );
}

/// `json_phase_args` with `max_budget_usd = None` must NOT contain
/// `--max-budget-usd` anywhere in the argv.
#[test]
fn json_phase_args_omits_max_budget_usd_when_none() {
    let base: Vec<String> = vec!["--print".into()];
    let args = json_phase_args(&base, "acceptEdits", "do the thing", None);

    assert!(
        !args.iter().any(|a| a == "--max-budget-usd"),
        "--max-budget-usd must NOT appear when max_budget_usd is None; got: {args:?}"
    );

    // The prompt must still be the last argument.
    assert_eq!(
        args.last().map(|s| s.as_str()),
        Some("do the thing"),
        "prompt must remain the last argument"
    );
}
