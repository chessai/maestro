//! Startup reconciliation: journal any in-flight tasks left by a prior daemon
//! instance as interrupted/failed so the advisor sees them (ADR-006).
//!
//! On daemon restart, every task in a non-terminal state was being driven by
//! the now-dead process. We journal them as torn down HERE, before any serving
//! begins, because THIS daemon has no live tasks yet — any in-flight task in
//! the journal is necessarily from a dead prior instance.

use std::sync::{Arc, Mutex};

use maestro_journal::domain::EventKind;
use maestro_journal::Journal;

use crate::worktree;

/// Classification of a task's current state for startup reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownClass {
    /// Terminal / resting state — leave untouched.
    Skip,
    /// Task had an active running session — emit `interrupted` (if not already)
    /// then `failed`.
    Active,
    /// Task was created/queued but never spawned — emit only `failed`.
    PreSession,
}

/// Classify a task's current state string for startup reconciliation.
///
/// The state string is the `kind` of the task's latest event.
pub fn teardown_class(state: &str) -> TeardownClass {
    match state {
        // Terminal / resting — these are done or awaiting an advisor action.
        "verify_passed" | "blocked" | "merged" | "failed" => TeardownClass::Skip,

        // Active — had a running session; full teardown with interrupted event.
        "spawned"
        | "iterating"
        | "impl_finished"
        | "checks_started"
        | "checks_passed"
        | "checks_failed"
        | "verify_started"
        | "verify_failed"
        | "escalated"
        | "containment_downgraded"
        | "interrupted" => TeardownClass::Active,

        // Pre-session — created or queued but never ran; terminal only.
        "created" | "queued" => TeardownClass::PreSession,

        // Unknown / unexpected state: skip without guessing.
        _ => {
            tracing::warn!(state, "reconcile_orphaned_tasks: unknown task state, skipping");
            TeardownClass::Skip
        }
    }
}

/// Reconcile any orphaned in-flight tasks left by a prior daemon instance.
///
/// Called ONCE at startup, before the serve loop begins, after the journal is
/// opened. This is safe to call unconditionally because THIS daemon has no live
/// tasks yet — any task in a non-terminal state is from a dead prior instance.
///
/// For each non-terminal task:
/// - **Active** (had a running session): snapshot the diff best-effort, emit
///   `interrupted` (if not already), then emit terminal `failed`, and
///   best-effort remove the worktree.
/// - **Pre-session** (`created`/`queued`): emit only terminal `failed` — no
///   session ran, so `interrupted` would be misleading, and there is no worktree.
/// - **Terminal** (`verify_passed`, `blocked`, `merged`, `failed`): skip.
pub fn reconcile_orphaned_tasks(journal: &Arc<Mutex<Journal>>) {
    // Snapshot the task list while holding the lock, then release it so each
    // per-task journal write can re-acquire without deadlocking.
    let tasks = {
        let j = journal.lock().expect("journal mutex poisoned");
        match j.list_tasks() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "reconcile_orphaned_tasks: list_tasks failed");
                return;
            }
        }
    };

    let mut active_count: u32 = 0;
    let mut pre_session_count: u32 = 0;

    for task in &tasks {
        let class = teardown_class(&task.state);
        match class {
            TeardownClass::Skip => continue,

            TeardownClass::Active => {
                // 1. Best-effort partial-diff snapshot.
                let partial_diff = {
                    let (repo_path, base_ref) = {
                        let j = journal.lock().expect("journal mutex poisoned");
                        match j.task_repo_and_base(&task.task_id) {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!(
                                    task = %task.task_id,
                                    error = %e,
                                    "reconcile: task_repo_and_base failed, using empty diff"
                                );
                                (None, String::new())
                            }
                        }
                    };
                    let wt_path = worktree::worktree_path(&task.task_id);
                    if wt_path.exists() {
                        let raw = worktree::snapshot_diff(&wt_path, &base_ref);
                        cap_diff(&raw)
                    } else {
                        let _ = (repo_path, base_ref); // suppress unused warning
                        String::new()
                    }
                };

                // 2. Emit `interrupted` only if not already in that state.
                if task.state != "interrupted" {
                    let interrupted_payload = serde_json::json!({
                        "reason": "daemon_restart",
                        "partial_diff": partial_diff,
                    })
                    .to_string();
                    let j = journal.lock().expect("journal mutex poisoned");
                    if let Err(e) =
                        j.append_event(&task.task_id, EventKind::Interrupted, Some(&interrupted_payload))
                    {
                        tracing::warn!(
                            task = %task.task_id,
                            error = %e,
                            "reconcile: failed to append interrupted event"
                        );
                    }
                }

                // 3. Emit terminal `failed`.
                {
                    let failed_payload = serde_json::json!({
                        "kind": "internal_error",
                        "message": "in-flight session torn down by daemon restart",
                        "reason": "daemon_restart",
                    })
                    .to_string();
                    let j = journal.lock().expect("journal mutex poisoned");
                    if let Err(e) =
                        j.append_event(&task.task_id, EventKind::Failed, Some(&failed_payload))
                    {
                        tracing::warn!(
                            task = %task.task_id,
                            error = %e,
                            "reconcile: failed to append failed event"
                        );
                    }
                }

                // 4. Best-effort worktree removal.
                {
                    let j = journal.lock().expect("journal mutex poisoned");
                    let repo_path = j
                        .task_repo_and_base(&task.task_id)
                        .ok()
                        .and_then(|(repo, _)| repo);
                    drop(j); // release lock before shelling out to git
                    if let Some(repo) = repo_path {
                        let wt_path = worktree::worktree_path(&task.task_id);
                        if wt_path.exists() {
                            worktree::remove(std::path::Path::new(&repo), &wt_path);
                        }
                    }
                }

                active_count += 1;
            }

            TeardownClass::PreSession => {
                // No session ran — emit only terminal `failed`.
                let failed_payload = serde_json::json!({
                    "kind": "internal_error",
                    "message": "task orphaned before spawn by daemon restart",
                    "reason": "daemon_restart",
                })
                .to_string();
                let j = journal.lock().expect("journal mutex poisoned");
                if let Err(e) =
                    j.append_event(&task.task_id, EventKind::Failed, Some(&failed_payload))
                {
                    tracing::warn!(
                        task = %task.task_id,
                        error = %e,
                        "reconcile: failed to append failed event for pre-session task"
                    );
                }
                pre_session_count += 1;
            }
        }
    }

    let total = active_count + pre_session_count;
    if total > 0 {
        tracing::info!(
            active = active_count,
            pre_session = pre_session_count,
            total,
            "reconcile_orphaned_tasks: torn down orphaned tasks from prior daemon"
        );
    } else {
        tracing::debug!("reconcile_orphaned_tasks: no orphaned tasks found");
    }
}

/// Cap a partial-diff snapshot to a forensically-useful size (matches
/// the 4000-char cap in `delegate::cap_diff`).
fn cap_diff(diff: &str) -> String {
    const CAP: usize = 4000;
    if diff.len() <= CAP {
        diff.to_string()
    } else {
        let mut s = diff[..CAP].to_string();
        s.push_str("\n…[truncated]");
        s
    }
}
