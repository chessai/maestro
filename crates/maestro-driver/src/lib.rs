//! maestro-driver — portable-pty driven sessions, plan-echo, watchdog
//! (ADR-006, M3).
//!
//! This crate owns driven CLI agent sessions directly over a PTY (no tmux),
//! uniform across Linux/macOS via [`portable_pty`]. A [`DrivenSession`] spawns
//! the configured CLI in its own session/process-group, pumps its PTY output to
//! a per-session log file and a shared buffer, runs the ADR-003 **plan-echo**
//! gate (an early-abort efficiency check that rejects a plan BEFORE any edits),
//! then supervises the child to completion under a no-output **watchdog**
//! (ADR-006), and exposes a break-glass **kill** path whose teardown SIGTERMs
//! the child's process group, waits, then SIGKILLs it.
//!
//! The security control for scope is the post-hoc allowlist diff (ADR-003); the
//! plan-echo gate here is only an efficiency gate, per ADR-003.

mod checker;
mod claude;
mod pty;
mod session;

pub use checker::{
    build_plan_check_body, AnthropicPlanChecker, MockPlanChecker, PlanChecker, PlanVerdict,
};
pub use claude::run_claude_driven;
pub use session::{
    pid_alive, DrivenConfig, DrivenResult, DrivenSession, EndReason, KillKind, SessionHandle,
};
