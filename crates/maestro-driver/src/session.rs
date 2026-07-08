//! The driven-session state machine (ADR-006): spawn → plan-echo gate → run →
//! teardown, with a break-glass kill path.
//!
//! [`DrivenSession::spawn`] opens a PTY, spawns the CLI in its own session /
//! process group (portable-pty calls `setsid`, so the child pid is its own
//! process-group leader), and returns a [`SessionHandle`] plus a
//! [`std::thread::JoinHandle`] yielding a [`DrivenResult`]. A reader thread
//! pumps PTY output to the log file and a shared buffer and stamps
//! `last_output_at`. The driver thread reads the plan echo, runs the checker,
//! then supervises the child under a watchdog and an external-kill channel.
//! Teardown SIGTERMs the child's process group, waits up to 5s, then SIGKILLs.
//!
//! The low-level PTY spawn / reader-thread / teardown primitives live in
//! [`crate::pty`] and are shared with the two-phase Claude adapter
//! ([`crate::claude::run_claude_driven`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use maestro_journal::spec::TaskSpec;
use rustix::process::{test_kill_process, Pid};

use crate::checker::{PlanChecker, PlanVerdict};
use crate::pty::{PtyChild, Shared, POLL};

/// Which kill path fired (ADR-006). Distinguished in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillKind {
    /// `maestro kill` — a human operator, no model in the path.
    Human,
    /// Advisor `kill_task`.
    Advisor,
}

/// Why a driven session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The CLI exited on its own with a success-ish status.
    Completed,
    /// Plan-echo failed the plan-vs-spec check; killed before any edits.
    PlanRejected {
        /// The checker's justification.
        reason: String,
    },
    /// External kill request (human `maestro kill` or advisor `kill_task`).
    Killed(KillKind),
    /// Watchdog fired: no output past the configured timeout.
    Wedged,
    /// The execute phase exceeded the per-attempt turn cap and was hard-stopped
    /// mid-session (structured stream-json adapter only). Terminal, not fuel.
    TurnBudgetExceeded,
    /// Spawn/PTY/other error, or a non-zero CLI exit.
    Failed(String),
}

/// The outcome of a driven session.
///
/// Not `Eq`: `cost_usd: Option<f64>` carries a float; `PartialEq` suffices for
/// the test assertions and the daemon never hashes a result.
#[derive(Debug, Clone, PartialEq)]
pub struct DrivenResult {
    /// Terminal reason.
    pub reason: EndReason,
    /// The per-session PTY log file (journal `sessions.log_path`).
    pub log_path: PathBuf,
    /// Best-effort turn count (plan echo counts as turn 1).
    pub turns: u32,
    /// Input tokens the driven session reported (summed across phases), when
    /// known. `None` for unmetered generic sessions (ADR-006).
    pub tokens_in: Option<u64>,
    /// Output tokens the driven session reported, when known.
    pub tokens_out: Option<u64>,
    /// Total cost (USD) the driven session reported, when known.
    pub cost_usd: Option<f64>,
}

/// Configuration for a single driven session.
#[derive(Debug, Clone)]
pub struct DrivenConfig {
    /// The driven CLI, e.g. `codex` / `claude`, or a fake script in tests.
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
    /// The worktree the CLI edits (becomes the child's cwd).
    pub cwd: PathBuf,
    /// Task prompt written to the PTY right after spawn.
    pub prompt: String,
    /// Capture ALL PTY output here (append).
    pub log_path: PathBuf,
    /// No-output timeout → [`EndReason::Wedged`].
    pub watchdog: Duration,
    /// Line prefix marking the plan echo, e.g. `PLAN:`.
    pub plan_marker: String,
    /// Max wait for the plan echo before [`EndReason::Wedged`].
    pub plan_timeout: Duration,
    /// Environment variable NAMES to strip from the spawned CLI's environment
    /// (ADR-006 `metered: false`). The daemon's own process env is NOT
    /// affected; only the child's inherited copy loses these keys. Use this
    /// to prevent a subscription-authenticated CLI (claude, codex) from
    /// accidentally billing per-token via an API key that happens to be in
    /// the daemon's environment.
    pub env_remove: Vec<String>,
    /// Per-attempt turn budget the structured (stream-json) claude adapter
    /// enforces in its execute phase by hard-stopping mid-session once the
    /// observed assistant-turn count exceeds this cap. `None` = no cap. The
    /// generic [`DrivenSession`] path ignores this field.
    pub turn_cap: Option<u32>,
    /// Dollar cap passed as `--max-budget-usd <amount>` to the `claude` CLI
    /// (ADR-006). When set, the role is API-billed; provider API keys must NOT
    /// be stripped so the CLI can authenticate per-token and self-enforce the
    /// ceiling. `None` → subscription mode (keys stripped by the daemon).
    /// The generic [`DrivenSession`] path ignores this field.
    pub max_budget_usd: Option<f64>,
}

/// Bound on plan-echo lines read after the marker line.
const PLAN_MAX_LINES: usize = 40;

/// A `Send + Sync` handle to a live driven session (stored in the daemon's
/// live-session registry). Carries the kill channel and the child pid so
/// `maestro kill` can call [`SessionHandle::request_kill`].
#[derive(Clone)]
pub struct SessionHandle {
    kill_tx: Sender<KillKind>,
    kill_sent: Arc<AtomicBool>,
    pid: Arc<Mutex<Option<i32>>>,
}

impl SessionHandle {
    /// Request teardown with the given [`KillKind`]. Idempotent: only the first
    /// request is delivered; later calls are no-ops.
    pub fn request_kill(&self, kind: KillKind) {
        if self
            .kill_sent
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // The receiver lives for the session's lifetime; a send error only
            // means the driver already finished, which is fine.
            let _ = self.kill_tx.send(kind);
        }
    }

    /// The child's pid (its process-group leader), if spawned. For the two-phase
    /// Claude adapter this is the pid of whichever phase is currently running.
    pub fn pid(&self) -> Option<i32> {
        *self.pid.lock().unwrap()
    }
}

/// The pieces the daemon needs to register and later kill a driven session, plus
/// the receiving end wired to the driver thread. Both the generic
/// [`DrivenSession`] and [`crate::claude::run_claude_driven`] build one.
pub(crate) struct HandleWiring {
    pub(crate) handle: SessionHandle,
    pub(crate) kill_rx: Receiver<KillKind>,
    pub(crate) pid_slot: Arc<Mutex<Option<i32>>>,
}

impl HandleWiring {
    pub(crate) fn new() -> Self {
        let (kill_tx, kill_rx) = mpsc::channel::<KillKind>();
        let kill_sent = Arc::new(AtomicBool::new(false));
        let pid_slot: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let handle = SessionHandle {
            kill_tx,
            kill_sent,
            pid: pid_slot.clone(),
        };
        HandleWiring {
            handle,
            kill_rx,
            pid_slot,
        }
    }
}

/// A driven CLI session over a PTY.
pub struct DrivenSession;

impl DrivenSession {
    /// Spawn the driven CLI and start supervising it.
    ///
    /// Returns a [`SessionHandle`] (for kill / pid) and a join handle yielding
    /// the [`DrivenResult`]. Errors only on setup failures (PTY/log open);
    /// spawn failures of the child are reported through the join handle as
    /// [`EndReason::Failed`].
    pub fn spawn(
        config: DrivenConfig,
        spec: TaskSpec,
        checker: Arc<dyn PlanChecker + Send + Sync>,
    ) -> anyhow::Result<(SessionHandle, JoinHandle<DrivenResult>)> {
        let wiring = HandleWiring::new();
        let handle = wiring.handle.clone();
        let kill_rx = wiring.kill_rx;
        let pid_slot = wiring.pid_slot;

        let join = std::thread::spawn(move || run_session(config, spec, checker, kill_rx, pid_slot));

        Ok((handle, join))
    }
}

/// The whole session lifecycle, run on the driver thread.
fn run_session(
    config: DrivenConfig,
    spec: TaskSpec,
    checker: Arc<dyn PlanChecker + Send + Sync>,
    kill_rx: Receiver<KillKind>,
    pid_slot: Arc<Mutex<Option<i32>>>,
) -> DrivenResult {
    let log_path = config.log_path.clone();

    let mut pty = match PtyChild::spawn(&config.program, &config.args, &config.cwd, &log_path, &config.env_remove) {
        Ok(p) => p,
        Err(e) => return failed(log_path, e),
    };

    // Record the pid (== its process-group id) for the kill path.
    *pid_slot.lock().unwrap() = pty.child_pid;

    // Send the prompt on the PTY stdin.
    let mut prompt = config.prompt.clone().into_bytes();
    prompt.push(b'\n');
    pty.write_stdin(&prompt);

    // Drive the state machine; compute (reason, turns) then let `pty` drop
    // (which reaps the child, closes the master, and joins the reader).
    let (reason, turns) = drive(&config, &spec, checker.as_ref(), &kill_rx, &mut pty);

    DrivenResult {
        reason,
        log_path,
        turns,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
    }
}

/// The plan-echo gate followed by run-to-completion supervision. Returns the
/// terminal reason and the observed turn count. Any teardown happens here while
/// the child is still owned by `pty`.
fn drive(
    config: &DrivenConfig,
    spec: &TaskSpec,
    checker: &dyn PlanChecker,
    kill_rx: &Receiver<KillKind>,
    pty: &mut PtyChild,
) -> (EndReason, u32) {
    // ---- Plan-echo gate: block until the plan echo or plan_timeout/kill. ----
    match wait_for_plan(&pty.shared, config, kill_rx, pty.child.as_mut()) {
        PlanWait::Plan(plan) => match checker.check(&plan, spec) {
            PlanVerdict::Reject { reason } => {
                pty.teardown();
                (EndReason::PlanRejected { reason }, 1)
            }
            PlanVerdict::Accept => {
                // ---- Run to completion under watchdog + kill supervision. ----
                (supervise(config, kill_rx, pty), 1)
            }
        },
        PlanWait::Killed(kind) => {
            pty.teardown();
            (EndReason::Killed(kind), 0)
        }
        PlanWait::Wedged => {
            pty.teardown();
            (EndReason::Wedged, 0)
        }
        PlanWait::Exited(reason) => (reason, 0),
    }
}

fn failed(log_path: PathBuf, msg: String) -> DrivenResult {
    DrivenResult {
        reason: EndReason::Failed(msg),
        log_path,
        turns: 0,
        tokens_in: None,
        tokens_out: None,
        cost_usd: None,
    }
}

/// The result of waiting for the plan echo.
enum PlanWait {
    Plan(String),
    Killed(KillKind),
    Wedged,
    Exited(EndReason),
}

/// Block until the plan echo appears, the plan_timeout elapses (Wedged), a kill
/// arrives, or the child exits early.
fn wait_for_plan(
    shared: &Arc<Shared>,
    config: &DrivenConfig,
    kill_rx: &Receiver<KillKind>,
    child: &mut dyn portable_pty::Child,
) -> PlanWait {
    let deadline = Instant::now() + config.plan_timeout;
    loop {
        if let Ok(kind) = kill_rx.try_recv() {
            return PlanWait::Killed(kind);
        }
        // Try to extract the plan from what we've buffered so far.
        if let Some(plan) = extract_plan(&shared.buf.lock().unwrap(), &config.plan_marker) {
            return PlanWait::Plan(plan);
        }
        // Early child exit before a plan → surface it.
        if let Ok(Some(status)) = child.try_wait() {
            return PlanWait::Exited(exit_reason(&status));
        }
        if Instant::now() >= deadline {
            return PlanWait::Wedged;
        }
        std::thread::sleep(POLL);
    }
}

/// Extract the plan text from the buffer: the remainder of the first line that
/// begins with `marker`, continuing across following non-blank lines until a
/// blank line or [`PLAN_MAX_LINES`]. Returns `None` until the marker line is
/// terminated by a newline in the raw buffer, so we never capture a
/// half-flushed marker line.
fn extract_plan(buf: &[u8], marker: &str) -> Option<String> {
    let text = String::from_utf8_lossy(buf);
    // Locate the marker line and require it to be newline-terminated so it is
    // fully flushed before we read it.
    let marker_at = text.find(marker)?;
    let after_marker = &text[marker_at + marker.len()..];
    let newline_at = after_marker.find('\n')?;
    let first = after_marker[..newline_at].trim().to_string();
    let rest_of_buf = &after_marker[newline_at + 1..];

    let mut plan_lines = if first.is_empty() {
        Vec::new()
    } else {
        vec![first]
    };
    for line in rest_of_buf.lines().take(PLAN_MAX_LINES) {
        if line.trim().is_empty() {
            break;
        }
        plan_lines.push(line.trim().to_string());
    }
    if plan_lines.is_empty() {
        return None;
    }
    Some(plan_lines.join(" "))
}

/// Supervise the child until exit, watchdog, or kill.
fn supervise(config: &DrivenConfig, kill_rx: &Receiver<KillKind>, pty: &mut PtyChild) -> EndReason {
    loop {
        // (c) external kill.
        if let Ok(kind) = kill_rx.try_recv() {
            pty.teardown();
            return EndReason::Killed(kind);
        }
        // (a) child exit.
        match pty.child.try_wait() {
            Ok(Some(status)) => return exit_reason(&status),
            Ok(None) => {}
            Err(e) => {
                pty.teardown();
                return EndReason::Failed(format!("try_wait failed: {e}"));
            }
        }
        // (b) watchdog.
        if pty.idle() > config.watchdog {
            pty.teardown();
            return EndReason::Wedged;
        }
        std::thread::sleep(POLL);
    }
}

/// Map a portable-pty exit status to an [`EndReason`].
fn exit_reason(status: &portable_pty::ExitStatus) -> EndReason {
    if status.success() {
        EndReason::Completed
    } else {
        EndReason::Failed(format!("CLI exited non-zero: {status:?}"))
    }
}

/// Liveness probe used by tests and callers: is `pid` still alive? Uses
/// `kill(pid, 0)` via [`test_kill_process`].
pub fn pid_alive(pid: i32) -> bool {
    match Pid::from_raw(pid) {
        Some(p) => test_kill_process(p).is_ok(),
        None => false,
    }
}
