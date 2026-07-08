//! Two-phase Claude-CLI adapter (M3 follow-up to ADR-003 / ADR-006).
//!
//! The `claude` CLI does not fit the interactive [`crate::session::DrivenSession`]
//! model: `claude --print "<prompt>"` takes the prompt as an ARG, does plan +
//! edit in one shot, and EXITS — there is no long-lived process that echoes a
//! plan and then keeps editing. To preserve a genuine plan-echo gate (abort
//! BEFORE any edits), this adapter uses claude's own permission modes across two
//! separate, run-to-completion invocations:
//!
//! - **Phase 1 (plan):** `<program> <args...> --permission-mode plan <prompt>` —
//!   claude produces a plan and edits nothing. Its stdout is the plan text. The
//!   [`PlanChecker`] runs on it. Reject ⇒ [`EndReason::PlanRejected`] with ZERO
//!   edits (nothing that could edit ever ran).
//! - **Phase 2 (execute):** only if accepted:
//!   `<program> <args...> --permission-mode acceptEdits <prompt>` — claude makes
//!   the edits in the worktree and exits. This is the long, killable phase.
//!
//! `program`/`args` come from [`DrivenConfig`] (the daemon passes
//! `program="claude"`, `args=["--print"]`, and applies any sandbox wrapping
//! itself); this adapter never hardcodes `claude` and never wraps in a sandbox.
//! It reuses the shared PTY primitive [`crate::pty::run_pty_command`] for both
//! phases, so the watchdog / process-group teardown / external-kill behavior is
//! identical to the generic driven session.

use std::sync::Arc;
use std::thread::JoinHandle;

use maestro_journal::spec::TaskSpec;

use crate::checker::{PlanChecker, PlanVerdict};
use crate::pty::{run_pty_command, PtyRunOutcome};
use crate::session::{
    DrivenConfig, DrivenResult, EndReason, HandleWiring, SessionHandle,
};

/// claude permission mode for the plan phase (edits nothing).
const MODE_PLAN: &str = "plan";
/// claude permission mode for the execute phase (applies edits headlessly).
const MODE_ACCEPT_EDITS: &str = "acceptEdits";

/// Drive the `claude` CLI as a two-phase, subscription-backed driven session.
///
/// Mirrors [`crate::session::DrivenSession::spawn`]'s return shape: a
/// [`SessionHandle`] (kill / pid of the *currently running* phase) and a join
/// handle yielding the [`DrivenResult`]. The returned handle's
/// [`SessionHandle::request_kill`] tears down whichever phase is running (the
/// pid slot is re-published when phase 2 starts).
pub fn run_claude_driven(
    config: DrivenConfig,
    spec: TaskSpec,
    checker: Arc<dyn PlanChecker + Send + Sync>,
) -> anyhow::Result<(SessionHandle, JoinHandle<DrivenResult>)> {
    let wiring = HandleWiring::new();
    let handle = wiring.handle.clone();
    let kill_rx = wiring.kill_rx;
    let pid_slot = wiring.pid_slot;

    let join = std::thread::spawn(move || {
        let log_path = config.log_path.clone();

        // ---- Phase 1: plan. `<args...> --permission-mode plan <prompt>`. ----
        let plan_args = phase_args(&config, MODE_PLAN);
        let plan_run = run_pty_command(
            &config.program,
            &plan_args,
            &config.cwd,
            &log_path,
            config.watchdog,
            &kill_rx,
            &pid_slot,
            &config.env_remove,
        );

        match plan_run.outcome {
            PtyRunOutcome::Killed(kind) => {
                // Killing during the quick plan phase is fine → Killed.
                return DrivenResult {
                    reason: EndReason::Killed(kind),
                    log_path,
                    turns: 1,
                };
            }
            PtyRunOutcome::Wedged => {
                return DrivenResult {
                    reason: EndReason::Wedged,
                    log_path,
                    turns: 1,
                };
            }
            PtyRunOutcome::SpawnError(e) => {
                return DrivenResult {
                    reason: EndReason::Failed(e),
                    log_path,
                    turns: 1,
                };
            }
            PtyRunOutcome::Exited(code) => {
                // A non-zero plan phase means claude couldn't even plan.
                if !matches!(code, Some(0)) {
                    return DrivenResult {
                        reason: EndReason::Failed(format!(
                            "claude plan phase exited non-zero: {code:?}"
                        )),
                        log_path,
                        turns: 1,
                    };
                }
            }
        }

        // The captured stdout of the plan phase IS the plan text.
        let plan = plan_run.output;
        match checker.check(&plan, &spec) {
            PlanVerdict::Reject { reason } => {
                // Plan rejected → NO phase 2, ZERO edits (nothing that edits ran).
                DrivenResult {
                    reason: EndReason::PlanRejected { reason },
                    log_path,
                    turns: 1,
                }
            }
            PlanVerdict::Accept => {
                // ---- Phase 2: execute. `--permission-mode acceptEdits`. ----
                // Same log path (append). `run_pty_command` re-publishes the pid
                // slot with THIS child's pid, so the handle now kills phase 2.
                let exec_args = phase_args(&config, MODE_ACCEPT_EDITS);
                let exec_run = run_pty_command(
                    &config.program,
                    &exec_args,
                    &config.cwd,
                    &log_path,
                    config.watchdog,
                    &kill_rx,
                    &pid_slot,
                    &config.env_remove,
                );

                let reason = match exec_run.outcome {
                    PtyRunOutcome::Exited(Some(0)) => EndReason::Completed,
                    PtyRunOutcome::Exited(code) => {
                        EndReason::Failed(format!("claude execute phase exited non-zero: {code:?}"))
                    }
                    PtyRunOutcome::Killed(kind) => EndReason::Killed(kind),
                    PtyRunOutcome::Wedged => EndReason::Wedged,
                    PtyRunOutcome::SpawnError(e) => EndReason::Failed(e),
                };

                DrivenResult {
                    reason,
                    log_path,
                    turns: 2,
                }
            }
        }
    });

    Ok((handle, join))
}

/// Build the argv for one phase: the configured base args, then
/// `--permission-mode <mode>`, then the prompt as the LAST arg (prompt-as-arg,
/// never on stdin).
fn phase_args(config: &DrivenConfig, mode: &str) -> Vec<String> {
    let mut args = Vec::with_capacity(config.args.len() + 3);
    args.extend(config.args.iter().cloned());
    args.push("--permission-mode".to_string());
    args.push(mode.to_string());
    args.push(config.prompt.clone());
    args
}
