//! Integration tests for ADR-006 startup reconciliation of orphaned in-flight
//! tasks (`startup::reconcile_orphaned_tasks`).
//!
//! Tests seed tasks directly into an in-memory journal, run reconcile, then
//! assert the correct events were appended. No daemon server is needed.

use std::sync::{Arc, Mutex};

use maestro_daemon::startup::{teardown_class, TeardownClass, reconcile_orphaned_tasks};
use maestro_journal::domain::{ContainmentLevel, EventKind, Tier};
use maestro_journal::Journal;

// ── helpers ─────────────────────────────────────────────────────────────────

fn open_journal() -> Arc<Mutex<Journal>> {
    let j = Journal::open_in_memory().expect("in-memory journal");
    Arc::new(Mutex::new(j))
}

fn create_advisor(journal: &Arc<Mutex<Journal>>) -> String {
    journal
        .lock()
        .unwrap()
        .create_advisor("test", "mock", "standard")
        .unwrap()
}

fn create_task(journal: &Arc<Mutex<Journal>>, advisor: &str) -> String {
    let spec = serde_json::json!({ "title": "test task" }).to_string();
    journal
        .lock()
        .unwrap()
        .create_task(
            advisor,
            Tier::T0,
            "mock",
            ContainmentLevel::L0,
            &spec,
            "HEAD",
            None,
            None,
            None,
        )
        .unwrap()
}

fn append(journal: &Arc<Mutex<Journal>>, task_id: &str, kind: EventKind) {
    journal
        .lock()
        .unwrap()
        .append_event(task_id, kind, None)
        .unwrap();
}

fn current_state(journal: &Arc<Mutex<Journal>>, task_id: &str) -> Option<EventKind> {
    journal
        .lock()
        .unwrap()
        .current_state(task_id)
        .expect("current_state query failed")
}

fn event_kinds(journal: &Arc<Mutex<Journal>>, task_id: &str) -> Vec<String> {
    journal
        .lock()
        .unwrap()
        .event_chain(task_id)
        .expect("event_chain failed")
        .into_iter()
        .map(|e| e.kind.as_str().to_string())
        .collect()
}

fn count_kind(journal: &Arc<Mutex<Journal>>, task_id: &str, kind: &str) -> usize {
    event_kinds(journal, task_id)
        .into_iter()
        .filter(|k| k == kind)
        .count()
}

// ── unit tests: teardown_class classifier ────────────────────────────────────

#[test]
fn teardown_class_terminal_states_skip() {
    assert_eq!(teardown_class("verify_passed"), TeardownClass::Skip);
    assert_eq!(teardown_class("blocked"), TeardownClass::Skip);
    assert_eq!(teardown_class("merged"), TeardownClass::Skip);
    assert_eq!(teardown_class("failed"), TeardownClass::Skip);
}

#[test]
fn teardown_class_active_states() {
    let active = [
        "spawned",
        "iterating",
        "impl_finished",
        "checks_started",
        "checks_passed",
        "checks_failed",
        "verify_started",
        "verify_failed",
        "escalated",
        "containment_downgraded",
        "interrupted",
    ];
    for state in active {
        assert_eq!(
            teardown_class(state),
            TeardownClass::Active,
            "expected Active for state {state}"
        );
    }
}

#[test]
fn teardown_class_pre_session_states() {
    assert_eq!(teardown_class("created"), TeardownClass::PreSession);
    assert_eq!(teardown_class("queued"), TeardownClass::PreSession);
}

#[test]
fn teardown_class_unknown_state_skips() {
    assert_eq!(teardown_class("totally_unknown"), TeardownClass::Skip);
    assert_eq!(teardown_class(""), TeardownClass::Skip);
    assert_eq!(teardown_class("running"), TeardownClass::Skip);
}

// ── integration tests: reconcile_orphaned_tasks ──────────────────────────────

/// Active task (`iterating`) → gets both `interrupted` and `failed`.
#[test]
fn active_task_gets_interrupted_then_failed() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);
    append(&j, &task, EventKind::Spawned);
    append(&j, &task, EventKind::Iterating);

    reconcile_orphaned_tasks(&j);

    // Must now be `failed`.
    assert_eq!(current_state(&j, &task), Some(EventKind::Failed));

    // Trace must contain `interrupted` before `failed`.
    let kinds = event_kinds(&j, &task);
    let interrupted_pos = kinds.iter().position(|k| k == "interrupted");
    let failed_pos = kinds.iter().rposition(|k| k == "failed");
    assert!(
        interrupted_pos.is_some(),
        "trace must contain an interrupted event; got: {kinds:?}"
    );
    assert!(
        failed_pos.is_some(),
        "trace must contain a failed event; got: {kinds:?}"
    );
    assert!(
        interrupted_pos.unwrap() < failed_pos.unwrap(),
        "interrupted must precede failed; got: {kinds:?}"
    );

    // The failed payload must carry reason=daemon_restart.
    let raw_payload = j
        .lock()
        .unwrap()
        .event_chain(&task)
        .unwrap()
        .into_iter().rfind(|e| e.kind == EventKind::Failed)
        .and_then(|e| e.payload)
        .expect("failed event must have a payload");
    let v: serde_json::Value = serde_json::from_str(&raw_payload).unwrap();
    assert_eq!(v["reason"], "daemon_restart");
    assert_eq!(v["kind"], "internal_error");
}

/// Terminal task (`verify_passed`) → untouched after reconcile.
#[test]
fn terminal_verify_passed_is_unchanged() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);
    append(&j, &task, EventKind::VerifyPassed);

    let before = event_kinds(&j, &task);
    reconcile_orphaned_tasks(&j);
    let after = event_kinds(&j, &task);

    assert_eq!(before, after, "verify_passed task must not gain new events");
    assert_eq!(current_state(&j, &task), Some(EventKind::VerifyPassed));
}

/// Terminal task (`blocked`) → untouched after reconcile.
#[test]
fn terminal_blocked_is_unchanged() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);
    append(&j, &task, EventKind::Blocked);

    let before = event_kinds(&j, &task);
    reconcile_orphaned_tasks(&j);
    let after = event_kinds(&j, &task);

    assert_eq!(before, after, "blocked task must not gain new events");
}

/// Pre-session task (`created`) → gets only `failed`, NO `interrupted`.
#[test]
fn pre_session_created_gets_failed_no_interrupted() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);

    reconcile_orphaned_tasks(&j);

    assert_eq!(current_state(&j, &task), Some(EventKind::Failed));
    let kinds = event_kinds(&j, &task);
    assert!(
        !kinds.contains(&"interrupted".to_string()),
        "pre-session task must not get an interrupted event; got: {kinds:?}"
    );

    // The failed payload must carry reason=daemon_restart.
    let raw_payload = j
        .lock()
        .unwrap()
        .event_chain(&task)
        .unwrap()
        .into_iter().rfind(|e| e.kind == EventKind::Failed)
        .and_then(|e| e.payload)
        .expect("failed event must have a payload");
    let v: serde_json::Value = serde_json::from_str(&raw_payload).unwrap();
    assert_eq!(v["reason"], "daemon_restart");
}

/// Pre-session task (`queued`) → gets only `failed`, NO `interrupted`.
#[test]
fn pre_session_queued_gets_failed_no_interrupted() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);
    append(&j, &task, EventKind::Queued);

    reconcile_orphaned_tasks(&j);

    assert_eq!(current_state(&j, &task), Some(EventKind::Failed));
    let kinds = event_kinds(&j, &task);
    assert!(
        !kinds.contains(&"interrupted".to_string()),
        "queued task must not get an interrupted event; got: {kinds:?}"
    );
}

/// Already-interrupted task → gets `failed` but NOT a second `interrupted`.
#[test]
fn already_interrupted_gets_failed_not_double_interrupted() {
    let j = open_journal();
    let adv = create_advisor(&j);
    let task = create_task(&j, &adv);
    append(&j, &task, EventKind::Created);
    append(&j, &task, EventKind::Spawned);
    append(&j, &task, EventKind::Interrupted);

    reconcile_orphaned_tasks(&j);

    // State is now failed.
    assert_eq!(current_state(&j, &task), Some(EventKind::Failed));

    // Exactly ONE interrupted event — not double-emitted.
    assert_eq!(
        count_kind(&j, &task, "interrupted"),
        1,
        "must not double-emit interrupted"
    );
}

/// All four classified states in one journal: verify_passed, iterating, created, blocked.
/// Complete scenario matching the spec.
#[test]
fn full_scenario_all_four_state_classes() {
    let j = open_journal();
    let adv = create_advisor(&j);

    // Active: iterating
    let active = create_task(&j, &adv);
    append(&j, &active, EventKind::Created);
    append(&j, &active, EventKind::Spawned);
    append(&j, &active, EventKind::Iterating);

    // Terminal: verify_passed
    let vp = create_task(&j, &adv);
    append(&j, &vp, EventKind::Created);
    append(&j, &vp, EventKind::VerifyPassed);

    // Pre-session: created
    let pre = create_task(&j, &adv);
    append(&j, &pre, EventKind::Created);

    // Terminal: blocked
    let bl = create_task(&j, &adv);
    append(&j, &bl, EventKind::Created);
    append(&j, &bl, EventKind::Blocked);

    // Already interrupted
    let already = create_task(&j, &adv);
    append(&j, &already, EventKind::Created);
    append(&j, &already, EventKind::Spawned);
    append(&j, &already, EventKind::Interrupted);

    reconcile_orphaned_tasks(&j);

    // Active → failed with interrupted event present
    assert_eq!(current_state(&j, &active), Some(EventKind::Failed));
    let active_kinds = event_kinds(&j, &active);
    assert!(
        active_kinds.contains(&"interrupted".to_string()),
        "active task must have interrupted event"
    );
    assert!(
        active_kinds.last() == Some(&"failed".to_string()),
        "active task must end in failed"
    );

    // verify_passed → unchanged
    assert_eq!(current_state(&j, &vp), Some(EventKind::VerifyPassed));
    let vp_kinds = event_kinds(&j, &vp);
    assert!(!vp_kinds.contains(&"failed".to_string()), "verify_passed must not be failed");

    // created (pre-session) → failed only, no interrupted
    assert_eq!(current_state(&j, &pre), Some(EventKind::Failed));
    let pre_kinds = event_kinds(&j, &pre);
    assert!(
        !pre_kinds.contains(&"interrupted".to_string()),
        "pre-session must not have interrupted event"
    );

    // blocked → unchanged
    assert_eq!(current_state(&j, &bl), Some(EventKind::Blocked));
    let bl_kinds = event_kinds(&j, &bl);
    assert!(!bl_kinds.contains(&"failed".to_string()), "blocked must not be failed");

    // already-interrupted → failed, exactly 1 interrupted
    assert_eq!(current_state(&j, &already), Some(EventKind::Failed));
    assert_eq!(
        count_kind(&j, &already, "interrupted"),
        1,
        "must not double-emit interrupted for already-interrupted task"
    );
}
