//! Two-phase, STRUCTURED (stream-json) Claude-CLI adapter (M3 follow-up to
//! ADR-003 / ADR-006).
//!
//! The `claude` CLI does not fit the interactive [`crate::session::DrivenSession`]
//! model: `claude --print "<prompt>"` takes the prompt as an ARG, does plan +
//! edit in one shot, and EXITS — there is no long-lived process that echoes a
//! plan and then keeps editing. To preserve a genuine plan-echo gate (abort
//! BEFORE any edits) AND to get real turn-boundary detection + token/cost
//! metering, this adapter drives claude's `--output-format stream-json`
//! newline-delimited JSON protocol across two separate, permission-scoped
//! invocations:
//!
//! - **Phase 1 (plan):** `<program> <args...> --output-format stream-json
//!   --verbose --permission-mode plan <prompt>` — claude produces a plan and
//!   edits nothing. This phase is AGENTIC: claude emits many assistant turns
//!   (thinking, then `tool_use` for Read/Glob/Bash/etc.) BEFORE its plan text
//!   arrives in a later turn, and often submits the final plan via an
//!   `ExitPlanMode` tool call. So the plan text is the model's plan accumulated
//!   across the WHOLE plan phase: the `ExitPlanMode`-submitted plan when present,
//!   else the concatenation of every assistant event's `content[].text` blocks
//!   (NOT just the first assistant event, which is typically empty). The
//!   [`PlanChecker`] runs on it. Reject ⇒ [`EndReason::PlanRejected`] with ZERO
//!   edits (nothing that could edit ran).
//! - **Phase 2 (execute):** only if accepted:
//!   `<program> <args...> --output-format stream-json --verbose
//!   --permission-mode acceptEdits <prompt>` — claude makes the edits in the
//!   worktree and exits. This is the long, killable phase, and the ONLY phase
//!   that enforces the per-attempt turn cap: once the observed assistant-turn
//!   count EXCEEDS `config.turn_cap`, the phase is torn down mid-session and the
//!   session ends [`EndReason::TurnBudgetExceeded`].
//!
//! `--output-format stream-json` REQUIRES `--verbose` under `--print` (claude
//! errors otherwise). `program`/`args` come from [`DrivenConfig`] (the daemon
//! passes `program="claude"`, `args=["--print"]`, and applies any sandbox
//! wrapping itself); this adapter never hardcodes `claude` and never wraps in a
//! sandbox. It reuses [`PtyChild`] directly (spawn / reader thread / teardown /
//! idle) so the watchdog / process-group teardown / external-kill behavior is
//! identical to the generic driven session.
//!
//! Event stream (one JSON object per newline-terminated line on stdout):
//! - `{"type":"system","subtype":"init",...}` — session start (ignored).
//! - `{"type":"assistant","message":{...,"content":[...]}}` — ONE per assistant
//!   turn. Counting these = turn-boundary detection.
//! - `{"type":"user",...}` — tool results (ignored for counting).
//! - `{"type":"result","num_turns":N,"total_cost_usd":F,"usage":{...}}` — the
//!   final event carrying the real turn count, cost, and token usage.
//! - Any other / unparsable line is SKIPPED (never an error).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use maestro_journal::spec::TaskSpec;

use crate::checker::{PlanChecker, PlanVerdict};
use crate::pty::{PtyChild, POLL};
use crate::session::{
    DrivenConfig, DrivenResult, EndReason, HandleWiring, KillKind, SessionHandle,
};

/// claude permission mode for the plan phase (edits nothing).
const MODE_PLAN: &str = "plan";
/// claude permission mode for the execute phase (applies edits headlessly).
const MODE_ACCEPT_EDITS: &str = "acceptEdits";

/// Metering + turn count captured from a phase's stream-json events.
#[derive(Debug, Default, Clone)]
struct PhaseMetering {
    /// The `assistant` events observed (turn-boundary count).
    assistant_turns: u32,
    /// `result.num_turns`, when a `result` event arrived (authoritative count).
    result_turns: Option<u32>,
    /// `result.usage.input_tokens`, when reported.
    tokens_in: Option<u64>,
    /// `result.usage.output_tokens`, when reported.
    tokens_out: Option<u64>,
    /// `result.total_cost_usd`, when reported.
    cost_usd: Option<f64>,
}

impl PhaseMetering {
    /// The best turn count for this phase: the authoritative `result.num_turns`
    /// if present, else the counted assistant events.
    fn turns(&self) -> u32 {
        self.result_turns.unwrap_or(self.assistant_turns)
    }
}

/// How a single stream-json phase ended.
enum JsonPhaseKind {
    /// The child exited on its own with this status code (`None` if unknown).
    Exited(Option<i32>),
    /// The execute phase exceeded the turn cap and was torn down mid-session.
    TurnCapExceeded,
    /// The phase exceeded its wall-clock ceiling and was torn down mid-session
    /// (operating-lesson L4). Distinct from `Wedged` (idle watchdog): this fires
    /// even when the phase is actively emitting output. Only the plan phase sets a
    /// wall-clock ceiling today; the execute phase is turn-capped instead.
    WallClockExceeded,
    /// No output past the watchdog → wedged (child torn down).
    Wedged,
    /// External kill request honored (child torn down).
    Killed(KillKind),
    /// Spawn / PTY setup error.
    SpawnError(String),
}

/// The terminal kind + captured plan text + metering for one stream-json phase.
struct JsonPhaseOutcome {
    kind: JsonPhaseKind,
    /// The model's plan accumulated across the plan phase: the
    /// `ExitPlanMode`-submitted plan when present, else all assistant text
    /// concatenated (NOT just the first assistant event). Only meaningful for
    /// phase 1; the execute phase ignores it.
    plan_text: String,
    metering: PhaseMetering,
}

/// Drive the `claude` CLI as a two-phase, subscription-backed driven session
/// over its structured stream-json protocol.
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

        // ---- Phase 1: plan (no turn cap; a plan is a single short turn). ----
        let plan_out = run_json_phase(
            &config.program,
            &config.args,
            MODE_PLAN,
            &config.prompt,
            &config.cwd,
            &log_path,
            config.watchdog,
            None,
            // L4: the plan phase is turn-uncapped; bound it by wall-clock instead.
            config.plan_ceiling,
            config.max_budget_usd,
            &kill_rx,
            &pid_slot,
            &config.env_remove,
        );
        let p1 = plan_out.metering.clone();

        // Non-clean plan-phase terminals map exactly as the old adapter did,
        // carrying any metering the phase managed to report.
        match plan_out.kind {
            JsonPhaseKind::Killed(kind) => {
                return result(EndReason::Killed(kind), log_path, p1.turns(), &p1);
            }
            JsonPhaseKind::Wedged => {
                return result(EndReason::Wedged, log_path, p1.turns(), &p1);
            }
            JsonPhaseKind::WallClockExceeded => {
                // L4: the plan phase ran past its wall-clock ceiling. Terminal,
                // ZERO edits (no execute phase) — mapped to PlanRejected so the
                // daemon's existing plan-rejected path handles it (not fuel).
                return result(
                    EndReason::PlanRejected {
                        reason: "plan phase exceeded its wall-clock ceiling".into(),
                    },
                    log_path,
                    p1.turns(),
                    &p1,
                );
            }
            JsonPhaseKind::TurnCapExceeded => {
                // No cap is set for the plan phase, so this cannot occur; treat
                // defensively as a failure rather than panicking.
                return result(
                    EndReason::Failed("claude plan phase hit a turn cap unexpectedly".into()),
                    log_path,
                    p1.turns(),
                    &p1,
                );
            }
            JsonPhaseKind::SpawnError(e) => {
                return result(EndReason::Failed(e), log_path, p1.turns(), &p1);
            }
            JsonPhaseKind::Exited(code) => {
                if !matches!(code, Some(0)) {
                    return result(
                        EndReason::Failed(format!("claude plan phase exited non-zero: {code:?}")),
                        log_path,
                        p1.turns(),
                        &p1,
                    );
                }
            }
        }

        // The plan text = the model's plan accumulated across the plan phase
        // (ExitPlanMode submission if present, else all assistant text; empty if
        // none — the checker decides what an empty plan means).
        let plan = plan_out.plan_text;
        match checker.check(&plan, &spec) {
            PlanVerdict::Reject { reason } => {
                // Plan rejected → NO phase 2, ZERO edits. Carry phase-1 metering.
                result(
                    EndReason::PlanRejected { reason },
                    log_path,
                    p1.turns(),
                    &p1,
                )
            }
            PlanVerdict::Accept => {
                // ---- Phase 2: execute (turn cap enforced). ----
                let exec_out = run_json_phase(
                    &config.program,
                    &config.args,
                    MODE_ACCEPT_EDITS,
                    &config.prompt,
                    &config.cwd,
                    &log_path,
                    config.watchdog,
                    config.turn_cap,
                    // The execute phase is turn-capped, not wall-clock-capped.
                    None,
                    config.max_budget_usd,
                    &kill_rx,
                    &pid_slot,
                    &config.env_remove,
                );
                let p2 = &exec_out.metering;

                let reason = match exec_out.kind {
                    JsonPhaseKind::Exited(Some(0)) => EndReason::Completed,
                    JsonPhaseKind::Exited(code) => {
                        EndReason::Failed(format!("claude execute phase exited non-zero: {code:?}"))
                    }
                    JsonPhaseKind::TurnCapExceeded => EndReason::TurnBudgetExceeded,
                    // Not reachable (execute passes `None` for wall_clock), but keep
                    // the match exhaustive and fail safe if that ever changes.
                    JsonPhaseKind::WallClockExceeded => EndReason::TurnBudgetExceeded,
                    JsonPhaseKind::Killed(kind) => EndReason::Killed(kind),
                    JsonPhaseKind::Wedged => EndReason::Wedged,
                    JsonPhaseKind::SpawnError(e) => EndReason::Failed(e),
                };

                let combined = combine(&p1, p2);
                DrivenResult {
                    reason,
                    log_path,
                    turns: p1.turns() + p2.turns(),
                    tokens_in: combined.0,
                    tokens_out: combined.1,
                    cost_usd: combined.2,
                }
            }
        }
    });

    Ok((handle, join))
}

/// Build a single-phase [`DrivenResult`] carrying that phase's metering.
fn result(reason: EndReason, log_path: PathBuf, turns: u32, m: &PhaseMetering) -> DrivenResult {
    DrivenResult {
        reason,
        log_path,
        turns,
        tokens_in: m.tokens_in,
        tokens_out: m.tokens_out,
        cost_usd: m.cost_usd,
    }
}

/// Sum two phases' metering. A field is `Some` iff at least one phase reported
/// it; when both do, they add (tokens/cost) — for the phases that reported none
/// the field stays `None`.
fn combine(a: &PhaseMetering, b: &PhaseMetering) -> (Option<u64>, Option<u64>, Option<f64>) {
    (
        sum_opt(a.tokens_in, b.tokens_in),
        sum_opt(a.tokens_out, b.tokens_out),
        sum_opt_f(a.cost_usd, b.cost_usd),
    )
}

fn sum_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn sum_opt_f(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Build the argv for a single stream-json phase. Pure function (no I/O), split
/// out so tests can assert on the constructed argv without spawning a process.
///
/// `argv = base_args + ["--output-format","stream-json","--verbose","--permission-mode",mode]
///   [+ ["--max-budget-usd","<amount>"] when max_budget_usd is Some] + [prompt]`
pub fn json_phase_args(
    base_args: &[String],
    mode: &str,
    prompt: &str,
    max_budget_usd: Option<f64>,
) -> Vec<String> {
    let mut args = Vec::with_capacity(base_args.len() + 8);
    args.extend(base_args.iter().cloned());
    args.push("--output-format".to_string());
    args.push("stream-json".to_string());
    args.push("--verbose".to_string());
    args.push("--permission-mode".to_string());
    args.push(mode.to_string());
    if let Some(budget) = max_budget_usd {
        args.push("--max-budget-usd".to_string());
        args.push(format!("{budget}"));
    }
    args.push(prompt.to_string());
    args
}

/// Run ONE `claude --print --output-format stream-json --verbose
/// --permission-mode <mode> <prompt>` phase over a PTY, parsing the newline-
/// delimited JSON as it streams, and return the terminal kind + captured plan
/// text + metering.
///
/// Mirrors the generic PTY supervision loop (kill / child-exit / watchdog at
/// interval [`POLL`]) but ADDS incremental JSON parsing and, when
/// `turn_cap` is `Some(cap)`, turn-cap enforcement: once `assistant_turns > cap`
/// the child is torn down and [`JsonPhaseKind::TurnCapExceeded`] is returned.
#[allow(clippy::too_many_arguments)]
fn run_json_phase(
    program: &str,
    base_args: &[String],
    mode: &str,
    prompt: &str,
    cwd: &Path,
    log_path: &Path,
    watchdog: Duration,
    turn_cap: Option<u32>,
    // Wall-clock ceiling for THIS phase (operating-lesson L4): if the phase runs
    // longer than this since spawn — regardless of activity — it is torn down and
    // `JsonPhaseKind::WallClockExceeded` is returned. `None` = no ceiling.
    wall_clock: Option<Duration>,
    max_budget_usd: Option<f64>,
    kill_rx: &Receiver<KillKind>,
    pid_slot: &Arc<Mutex<Option<i32>>>,
    env_remove: &[String],
) -> JsonPhaseOutcome {
    let args = json_phase_args(base_args, mode, prompt, max_budget_usd);

    let mut pty = match PtyChild::spawn(program, &args, cwd, log_path, env_remove) {
        Ok(p) => p,
        Err(e) => {
            return JsonPhaseOutcome {
                kind: JsonPhaseKind::SpawnError(e),
                plan_text: String::new(),
                metering: PhaseMetering::default(),
            }
        }
    };

    // Publish the pid so an external kill targets THIS phase's child.
    *pid_slot.lock().unwrap() = pty.child_pid;

    let mut state = ParseState::default();
    let mut cursor = 0usize; // byte offset into the shared buffer already parsed.
    let started = Instant::now();

    let kind = loop {
        // Parse any newly-buffered complete lines (updates state + cursor).
        parse_new_lines(&pty, &mut cursor, &mut state);

        // Wall-clock ceiling (L4): bound the phase even when it is actively
        // emitting output (so the idle watchdog never fires). Checked first so a
        // runaway phase is reaped promptly.
        if let Some(ceiling) = wall_clock {
            if started.elapsed() > ceiling {
                pty.teardown();
                break JsonPhaseKind::WallClockExceeded;
            }
        }

        // Turn-cap enforcement (execute phase only): the observed assistant-turn
        // count EXCEEDING the cap hard-stops the phase mid-session.
        if let Some(cap) = turn_cap {
            if state.metering.assistant_turns > cap {
                pty.teardown();
                break JsonPhaseKind::TurnCapExceeded;
            }
        }
        // (c) external kill.
        if let Ok(kk) = kill_rx.try_recv() {
            pty.teardown();
            break JsonPhaseKind::Killed(kk);
        }
        // (a0) terminal `result` event — claude's authoritative phase-complete
        // signal. Prefer it over waiting for a process exit: some `claude --print`
        // sessions emit `result` and then never exit (observed after subagent-heavy
        // plan phases), which would otherwise leave this loop spinning until the
        // idle watchdog reaped a phase that had, in fact, finished. Give the child a
        // brief grace to exit and flush its final lines on its own; if it lingers,
        // tear it down and treat the phase as complete with the result's own status.
        if state.result_seen {
            std::thread::sleep(POLL);
            parse_new_lines(&pty, &mut cursor, &mut state);
            let code = match pty.child.try_wait() {
                Ok(Some(status)) => exit_code(&status),
                _ => {
                    pty.teardown();
                    Some(if state.result_is_error { 1 } else { 0 })
                }
            };
            break JsonPhaseKind::Exited(code);
        }
        // (a) child exit — drain any final lines the reader may still deliver.
        match pty.child.try_wait() {
            Ok(Some(status)) => {
                // The reader thread may lag the exit; give it a beat, then drain.
                std::thread::sleep(POLL);
                parse_new_lines(&pty, &mut cursor, &mut state);
                break JsonPhaseKind::Exited(exit_code(&status));
            }
            Ok(None) => {}
            Err(e) => {
                pty.teardown();
                break JsonPhaseKind::SpawnError(format!("try_wait failed: {e}"));
            }
        }
        // (b) watchdog.
        if pty.idle() > watchdog {
            pty.teardown();
            break JsonPhaseKind::Wedged;
        }
        std::thread::sleep(POLL);
    };

    // `pty` drops here: reader joined, master/writer closed.
    let metering = state.metering.clone();
    JsonPhaseOutcome {
        kind,
        plan_text: state.plan(),
        metering,
    }
}

/// The running parse state for one phase's stream-json events.
#[derive(Default)]
struct ParseState {
    metering: PhaseMetering,
    /// The `text` blocks of EVERY assistant event, concatenated with "\n"
    /// separators so multiple agentic turns' text stays readable. This is the
    /// fallback plan text when no explicit `ExitPlanMode` submission arrives.
    all_text: String,
    /// The plan submitted via an `ExitPlanMode` tool call, if any (last one
    /// wins). When non-empty this takes precedence over `all_text`.
    exit_plan: String,
    /// The terminal `result` stream-json event has been observed. This is
    /// claude's authoritative "phase complete" signal; the phase loop ends on it
    /// rather than waiting for a process exit that some `claude --print` sessions
    /// never deliver (observed after heavy subagent use — the process emits its
    /// `result` and then lingers, which would otherwise spin until the idle
    /// watchdog fired 30 min later).
    result_seen: bool,
    /// Whether that `result` event reported an error (`is_error` / `subtype ==
    /// "error"`), so a completed-but-errored phase still maps to a non-zero exit.
    result_is_error: bool,
}

impl ParseState {
    /// The effective plan text for this phase: the `ExitPlanMode`-submitted plan
    /// when present, else all accumulated assistant text. Only meaningful for
    /// the plan phase; the execute phase ignores it.
    fn plan(self) -> String {
        if self.exit_plan.is_empty() {
            self.all_text
        } else {
            self.exit_plan
        }
    }
}

/// Parse every NEW newline-terminated line in the shared buffer past `cursor`,
/// advancing `cursor` and folding each recognized event into `state`. Lines that
/// do not parse or carry an unknown `type` are SKIPPED (never an error).
fn parse_new_lines(pty: &PtyChild, cursor: &mut usize, state: &mut ParseState) {
    let buf = pty.shared.buf.lock().unwrap();
    // Advance line by line over complete (newline-terminated) lines only.
    while let Some(nl_rel) = buf[*cursor..].iter().position(|&b| b == b'\n') {
        let line_end = *cursor + nl_rel; // index of the '\n'
        let raw = &buf[*cursor..line_end];
        *cursor = line_end + 1; // consume the newline.
        // Trim a trailing '\r' (PTY may translate '\n' → '\r\n').
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let line = match std::str::from_utf8(raw) {
            Ok(s) => s.trim(),
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON / garbage line → skip.
        };
        fold_event(&value, state);
    }
}

/// Fold one parsed stream-json event into the running state.
fn fold_event(value: &serde_json::Value, state: &mut ParseState) {
    match value.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            state.metering.assistant_turns += 1;
            // Accumulate this turn's plain text (empty for thinking-only or
            // tool_use-only turns), separating turns with a newline so the plan
            // stays readable across the agentic plan phase.
            let text = assistant_text(value);
            if !text.is_empty() {
                if !state.all_text.is_empty() {
                    state.all_text.push('\n');
                }
                state.all_text.push_str(&text);
            }
            // Prefer an explicit ExitPlanMode submission when present (last wins).
            if let Some(plan) = exit_plan_from_assistant(value) {
                state.exit_plan = plan;
            }
        }
        Some("result") => {
            state.result_seen = true;
            state.result_is_error = value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || value.get("subtype").and_then(|v| v.as_str()) == Some("error");
            if let Some(n) = value.get("num_turns").and_then(|v| v.as_u64()) {
                state.metering.result_turns = Some(n as u32);
            }
            if let Some(usage) = value.get("usage") {
                if let Some(i) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                    state.metering.tokens_in = Some(i);
                }
                if let Some(o) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                    state.metering.tokens_out = Some(o);
                }
            }
            if let Some(c) = value.get("total_cost_usd").and_then(|v| v.as_f64()) {
                state.metering.cost_usd = Some(c);
            }
        }
        // "system" (init), "user" (tool results), and any unknown type → ignore.
        _ => {}
    }
}

/// Concatenate the `text` of every `content[]` block in an `assistant` event's
/// `message`. Non-text blocks (e.g. `tool_use`) contribute nothing.
fn assistant_text(value: &serde_json::Value) -> String {
    let Some(content) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return String::new();
    };
    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    out
}

/// Extract the plan submitted via an `ExitPlanMode` tool call in an `assistant`
/// event, if any. Scans `message.content[]` for a `tool_use` block whose `name`
/// is `ExitPlanMode` / `exit_plan_mode` (either casing) and returns its
/// `input.plan` string. Returns `None` when there is no such block or no plan.
fn exit_plan_from_assistant(value: &serde_json::Value) -> Option<String> {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if !name.eq_ignore_ascii_case("ExitPlanMode") && !name.eq_ignore_ascii_case("exit_plan_mode")
        {
            continue;
        }
        if let Some(plan) = block
            .get("input")
            .and_then(|i| i.get("plan"))
            .and_then(|p| p.as_str())
        {
            return Some(plan.to_string());
        }
    }
    None
}

/// Best-effort exit code from a portable-pty exit status (mirrors `pty.rs`).
fn exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    if status.success() {
        Some(0)
    } else {
        Some(status.exit_code() as i32)
    }
}
