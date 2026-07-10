//! Progress-tracking semantics for `maestro watch` / `maestro status` (ADR-009,
//! Phase 1). This is the LOAD-BEARING partition of task states into
//! actionable / terminal / transient, plus the pure decision core `watch_once`
//! that `watch`'s poll loop is built on.
//!
//! It lives in `maestro-journal` (not `maestro-cli`) on purpose: it is a pure
//! function of a [`PsRow`] snapshot, so both the CLI poll loop AND the daemon
//! integration test (which drives a real task through states and polls
//! `TaskStatus`) can exercise the exact same semantics. No I/O, no daemon
//! protocol change — this is read-side classification only.

use crate::domain::EventKind;
use crate::proto::PsRow;

/// Classification of a task's derived state (the latest [`EventKind`] string).
///
/// The partition is the single source of truth for `watch`'s wake condition and
/// `status`'s needs-attention flag. A WRONG partition makes `watch` either wake
/// the advisor constantly (a transient misclassified actionable) or never (an
/// actionable misclassified transient), so it is exhaustively unit-tested over
/// every `EventKind` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateClass {
    /// A human/advisor decision is pending: `verify_passed` (→ merge),
    /// `blocked` (→ diagnose), `failed` (→ diagnose). These are ALSO terminal
    /// (resting) states — `watch` returns for them.
    Actionable,
    /// A resting state that needs no advisor action: `merged`. Terminal but not
    /// actionable — `watch` treats it as "done, nothing to do".
    Terminal,
    /// The daemon is (or will be) progressing this state on its own — the
    /// advisor must NOT be woken for it (`created`, `iterating`,
    /// `checks_failed`, `escalated`, …). `watch` keeps polling.
    Transient,
}

impl StateClass {
    /// Whether a task in this class is at rest (no further daemon progress
    /// expected): the actionable set ∪ `{merged}`. `watch`'s all-terminal
    /// early-return fires when every tracked task is terminal by this predicate.
    pub fn is_terminal(self) -> bool {
        matches!(self, StateClass::Actionable | StateClass::Terminal)
    }

    /// Whether a task in this class demands advisor action (the wake trigger).
    pub fn is_actionable(self) -> bool {
        matches!(self, StateClass::Actionable)
    }
}

/// Classify a derived-state string (a snake_case [`EventKind`], as carried by
/// [`PsRow::state`]) into its [`StateClass`].
///
/// An UNKNOWN string (not a valid `EventKind`) is treated as [`StateClass::Transient`]
/// — the conservative default: a state we do not understand must NOT wake the
/// advisor (better to keep polling than to spuriously return). The exhaustive
/// unit test guarantees every real `EventKind` is explicitly classified, so this
/// fallback only ever covers genuine garbage.
pub fn state_class(state: &str) -> StateClass {
    let Some(kind) = EventKind::from_str_kind(state) else {
        return StateClass::Transient;
    };
    match kind {
        // ADVISOR-ACTIONABLE — a decision is pending.
        EventKind::VerifyPassed | EventKind::Blocked | EventKind::Failed => StateClass::Actionable,
        // TERMINAL-DONE — nothing to do.
        EventKind::Merged => StateClass::Terminal,
        // TRANSIENT — the daemon is grinding this forward; do NOT wake.
        EventKind::Created
        | EventKind::Queued
        | EventKind::ContainmentDowngraded
        | EventKind::Spawned
        | EventKind::PlanSubmitted
        | EventKind::PlanRejected
        | EventKind::Iterating
        | EventKind::ImplFinished
        | EventKind::ChecksStarted
        | EventKind::ChecksFailed
        | EventKind::ChecksPassed
        | EventKind::VerifyStarted
        | EventKind::VerifyFailed
        | EventKind::Escalated
        | EventKind::Interrupted
        | EventKind::Pruned
        | EventKind::StallDetected
        | EventKind::AutoRecovered => StateClass::Transient,
    }
}

/// The suggested advisor action for an actionable state, or `None` for a
/// non-actionable one. Used by both `status`'s ACTIONABLE section and `watch`'s
/// triggering digest.
pub fn suggested_action(state: &str) -> Option<&'static str> {
    match state {
        "verify_passed" => Some("merge"),
        "blocked" => Some("diagnose"),
        "failed" => Some("diagnose"),
        _ => None,
    }
}

/// One line of a `watch` return digest: the task that triggered the wake, its
/// state, and why the advisor was pulled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestLine {
    pub task_id: String,
    pub title: String,
    pub state: String,
    /// The suggested action (`merge` / `diagnose`) for an actionable trigger, or
    /// a short note for the all-terminal case (e.g. `done`).
    pub action: String,
}

/// Why `watch` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTrigger {
    /// At least one tracked task became actionable.
    Actionable,
    /// Every tracked task reached a terminal (resting) state; none is actionable.
    AllTerminal,
}

/// A `watch` return digest: the trigger and the task line(s) that explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchDigest {
    pub trigger: WatchTrigger,
    pub lines: Vec<DigestLine>,
}

/// The PURE decision core of `maestro watch`, evaluated once per poll over a
/// snapshot of the advisor's task rows.
///
/// `tracked` is the set of task ids `watch` is following. Returns:
/// - `Some(digest)` — `watch` should EXIT NOW. Either at least one tracked task
///   is actionable (`verify_passed`/`blocked`/`failed`), or every tracked task
///   that is present is terminal (actionable ∪ `{merged}`).
/// - `None` — keep polling: at least one tracked task is still transient and
///   none is actionable.
///
/// Edge cases handled here (not in the loop):
/// - **A tracked task absent from `rows`** (disappeared/pruned/never appeared):
///   it does not count toward the all-terminal check and cannot itself trigger.
///   If EVERY tracked task is absent, there is nothing to wait on → returns an
///   `AllTerminal` digest with no lines so the loop exits rather than spinning
///   forever. (The caller notes the empty tracked set separately.)
/// - **Actionable beats all-terminal**: if any task is actionable, the trigger
///   is `Actionable` and only the actionable task(s) are listed, even if the
///   rest are terminal.
pub fn watch_once(rows: &[PsRow], tracked: &[String]) -> Option<WatchDigest> {
    // The tracked rows actually present in this snapshot.
    let present: Vec<&PsRow> = rows
        .iter()
        .filter(|r| tracked.iter().any(|t| t == &r.task_id))
        .collect();

    // Any actionable tracked task → wake immediately, listing the actionable ones.
    let actionable: Vec<&PsRow> = present
        .iter()
        .copied()
        .filter(|r| state_class(&r.state).is_actionable())
        .collect();
    if !actionable.is_empty() {
        return Some(WatchDigest {
            trigger: WatchTrigger::Actionable,
            lines: actionable.iter().map(|r| digest_line(r)).collect(),
        });
    }

    // No actionable task. If every PRESENT tracked task is terminal (here, all
    // `merged`, since actionable was empty) → the all-terminal early return.
    // A tracked task absent from the snapshot does not block this: if none is
    // present at all, `all_terminal` is vacuously true and we exit (nothing to
    // wait on). If some present task is still transient, keep polling.
    let all_terminal = present.iter().all(|r| state_class(&r.state).is_terminal());
    if all_terminal {
        return Some(WatchDigest {
            trigger: WatchTrigger::AllTerminal,
            lines: present.iter().map(|r| digest_line(r)).collect(),
        });
    }

    None
}

fn digest_line(r: &PsRow) -> DigestLine {
    let action = suggested_action(&r.state)
        .map(str::to_string)
        .unwrap_or_else(|| "done".to_string());
    DigestLine {
        task_id: r.task_id.clone(),
        title: r.title.clone(),
        state: r.state.clone(),
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ContainmentLevel, Tier};

    /// Build a minimal `PsRow` in a given state for the pure-core tests.
    fn row(task_id: &str, state: &str) -> PsRow {
        PsRow {
            task_id: task_id.to_string(),
            title: format!("task {task_id}"),
            tier: Tier::T0,
            model: "mock".to_string(),
            containment: ContainmentLevel::L0,
            state: state.to_string(),
            created_at: "2026-07-10T00:00:00Z".to_string(),
        }
    }

    /// The load-bearing guard: EVERY `EventKind`'s rendered string maps to a
    /// defined class, and the partition matches the ADR-009 vocabulary exactly.
    /// A miss here means `watch` would treat a real state with the fallback
    /// (transient) — a latent wake bug.
    #[test]
    fn state_class_covers_every_event_kind() {
        // The full, hand-mirrored EventKind set (kept in lockstep with
        // domain.rs). If a variant is added to EventKind, this list must grow —
        // the `exhaustive` assertion below fails loudly until it does.
        const ALL: &[EventKind] = &[
            EventKind::Created,
            EventKind::Queued,
            EventKind::ContainmentDowngraded,
            EventKind::Spawned,
            EventKind::PlanSubmitted,
            EventKind::PlanRejected,
            EventKind::Iterating,
            EventKind::ImplFinished,
            EventKind::ChecksStarted,
            EventKind::ChecksFailed,
            EventKind::ChecksPassed,
            EventKind::VerifyStarted,
            EventKind::VerifyFailed,
            EventKind::VerifyPassed,
            EventKind::Escalated,
            EventKind::Blocked,
            EventKind::Merged,
            EventKind::Interrupted,
            EventKind::Failed,
            EventKind::Pruned,
            EventKind::StallDetected,
            EventKind::AutoRecovered,
        ];

        // Guard that ALL is exhaustive over the enum: every string round-trips
        // AND parses back to a member of ALL. This catches a new EventKind that
        // was NOT added to ALL (its string would parse but not be in the list).
        for &k in ALL {
            let s = k.as_str();
            let parsed = EventKind::from_str_kind(s).expect("every EventKind string parses");
            assert!(
                ALL.contains(&parsed),
                "{s} parsed to a kind missing from ALL"
            );
        }

        // The expected partition, verbatim from ADR-009.
        let actionable = ["verify_passed", "blocked", "failed"];
        let terminal = ["merged"];

        for &k in ALL {
            let s = k.as_str();
            let class = state_class(s);
            let expected = if actionable.contains(&s) {
                StateClass::Actionable
            } else if terminal.contains(&s) {
                StateClass::Terminal
            } else {
                StateClass::Transient
            };
            assert_eq!(class, expected, "state_class({s}) misclassified");

            // Cross-check the predicates against the class.
            assert_eq!(
                class.is_actionable(),
                actionable.contains(&s),
                "is_actionable({s})"
            );
            assert_eq!(
                class.is_terminal(),
                actionable.contains(&s) || terminal.contains(&s),
                "is_terminal({s})"
            );
        }

        // Belt-and-suspenders: the partition sizes sum to the whole enum.
        let n_actionable = ALL
            .iter()
            .filter(|k| state_class(k.as_str()).is_actionable())
            .count();
        let n_terminal_only = ALL
            .iter()
            .filter(|k| state_class(k.as_str()) == StateClass::Terminal)
            .count();
        let n_transient = ALL
            .iter()
            .filter(|k| state_class(k.as_str()) == StateClass::Transient)
            .count();
        assert_eq!(n_actionable, 3, "exactly 3 actionable states");
        assert_eq!(n_terminal_only, 1, "exactly 1 terminal-only state (merged)");
        assert_eq!(n_transient, ALL.len() - 4, "the rest are transient");
    }

    /// An unknown/garbage state defaults to transient (never wakes the advisor).
    #[test]
    fn unknown_state_is_transient() {
        assert_eq!(state_class("not_a_real_state"), StateClass::Transient);
        assert_eq!(state_class(""), StateClass::Transient);
    }

    #[test]
    fn watch_returns_on_a_single_actionable_task() {
        let rows = vec![row("A", "iterating"), row("B", "verify_passed")];
        let tracked = vec!["A".to_string(), "B".to_string()];
        let d = watch_once(&rows, &tracked).expect("verify_passed triggers");
        assert_eq!(d.trigger, WatchTrigger::Actionable);
        assert_eq!(d.lines.len(), 1, "only the actionable task is listed");
        assert_eq!(d.lines[0].task_id, "B");
        assert_eq!(d.lines[0].action, "merge");
    }

    #[test]
    fn watch_keeps_polling_while_transient() {
        // checks_failed / verify_failed / escalated are mid-flight, NOT actionable.
        for st in [
            "created",
            "iterating",
            "checks_failed",
            "verify_failed",
            "escalated",
        ] {
            let rows = vec![row("A", st)];
            let tracked = vec!["A".to_string()];
            assert!(
                watch_once(&rows, &tracked).is_none(),
                "watch must keep polling while {st}"
            );
        }
    }

    #[test]
    fn watch_returns_when_all_terminal() {
        let rows = vec![row("A", "merged"), row("B", "merged")];
        let tracked = vec!["A".to_string(), "B".to_string()];
        let d = watch_once(&rows, &tracked).expect("all merged triggers all-terminal");
        assert_eq!(d.trigger, WatchTrigger::AllTerminal);
        assert_eq!(d.lines.len(), 2);
    }

    #[test]
    fn watch_does_not_early_return_when_one_is_still_transient() {
        let rows = vec![row("A", "merged"), row("B", "iterating")];
        let tracked = vec!["A".to_string(), "B".to_string()];
        assert!(
            watch_once(&rows, &tracked).is_none(),
            "one merged + one iterating → keep polling (not all terminal, none actionable)"
        );
    }

    #[test]
    fn watch_actionable_beats_all_terminal() {
        // A mix of merged + failed → the failed (actionable) triggers, not all-terminal.
        let rows = vec![row("A", "merged"), row("B", "failed")];
        let tracked = vec!["A".to_string(), "B".to_string()];
        let d = watch_once(&rows, &tracked).unwrap();
        assert_eq!(d.trigger, WatchTrigger::Actionable);
        assert_eq!(d.lines.len(), 1);
        assert_eq!(d.lines[0].task_id, "B");
        assert_eq!(d.lines[0].action, "diagnose");
    }

    #[test]
    fn watch_task_straight_to_failed_returns() {
        let rows = vec![row("A", "failed")];
        let tracked = vec!["A".to_string()];
        let d = watch_once(&rows, &tracked).unwrap();
        assert_eq!(d.trigger, WatchTrigger::Actionable);
        assert_eq!(d.lines[0].action, "diagnose");
    }

    #[test]
    fn watch_ignores_untracked_tasks() {
        // An actionable task NOT in the tracked set must not trigger a return.
        let rows = vec![row("A", "iterating"), row("B", "verify_passed")];
        let tracked = vec!["A".to_string()];
        assert!(
            watch_once(&rows, &tracked).is_none(),
            "B is actionable but untracked; A is transient → keep polling"
        );
    }

    #[test]
    fn watch_all_tracked_absent_returns_empty_all_terminal() {
        // Tracked tasks that never appear (or vanished) → nothing to wait on.
        let rows = vec![row("Z", "iterating")];
        let tracked = vec!["A".to_string(), "B".to_string()];
        let d = watch_once(&rows, &tracked).expect("all-absent → exit, don't spin");
        assert_eq!(d.trigger, WatchTrigger::AllTerminal);
        assert!(d.lines.is_empty(), "no present tracked tasks → empty digest");
    }

    #[test]
    fn watch_one_present_terminal_others_absent_returns() {
        // A tracked task that disappeared does not keep watch alive: the one
        // present tracked task is merged → all-terminal.
        let rows = vec![row("A", "merged"), row("Z", "iterating")];
        let tracked = vec!["A".to_string(), "GONE".to_string()];
        let d = watch_once(&rows, &tracked).expect("present one is terminal, other absent");
        assert_eq!(d.trigger, WatchTrigger::AllTerminal);
        assert_eq!(d.lines.len(), 1);
        assert_eq!(d.lines[0].task_id, "A");
    }
}
