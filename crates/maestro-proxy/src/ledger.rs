//! A thread-safe, per-task token ledger (ADR-006 / ADR-004).
//!
//! The streaming credential proxy meters token usage per response and needs a
//! shared place to accumulate usage keyed by task id, plus the per-task token
//! ceiling so it can decide — mid-stream — whether a response has pushed a task
//! over budget. This is that place: an `Arc<Ledger>` is shared between the
//! daemon (which registers ceilings at delegate time) and the proxy server
//! threads (which accumulate usage as SSE frames arrive).

use std::collections::HashMap;
use std::sync::Mutex;

/// Per-task accumulated usage plus the optional token ceiling.
struct Entry {
    /// The task-lifetime token ceiling (`tokens_in + tokens_out >= ceiling`
    /// ⇒ over budget). `None` means unbounded — usage is still metered.
    ceiling: Option<i64>,
    tokens_in: i64,
    tokens_out: i64,
}

/// A thread-safe token ledger keyed by task id. Wrap in `Arc` for sharing.
#[derive(Default)]
pub struct Ledger {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Ledger {
    /// A fresh, empty ledger.
    pub fn new() -> Self {
        Ledger {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Set (or reset) the entry for `task_id`, installing its ceiling and
    /// zeroing accumulated usage. Called at delegate time by the daemon.
    pub fn register(&self, task_id: &str, ceiling: Option<i64>) {
        let mut guard = self.lock();
        guard.insert(
            task_id.to_string(),
            Entry {
                ceiling,
                tokens_in: 0,
                tokens_out: 0,
            },
        );
    }

    /// Accumulate ADDITIVE token deltas for `task_id`. The proxy computes these
    /// deltas from the cumulative usage carried by the stream (`message_start`
    /// input, `message_delta` output), so what lands here is always a delta,
    /// never a cumulative snapshot. Unknown tasks are created on demand with no
    /// ceiling so metering a never-registered task is safe.
    pub fn add_usage(&self, task_id: &str, delta_in: i64, delta_out: i64) {
        let mut guard = self.lock();
        let entry = guard.entry(task_id.to_string()).or_insert(Entry {
            ceiling: None,
            tokens_in: 0,
            tokens_out: 0,
        });
        entry.tokens_in += delta_in;
        entry.tokens_out += delta_out;
    }

    /// The `(tokens_in, tokens_out)` accumulated for `task_id`. An unknown task
    /// reports `(0, 0)`.
    pub fn spent(&self, task_id: &str) -> (i64, i64) {
        let guard = self.lock();
        guard
            .get(task_id)
            .map(|e| (e.tokens_in, e.tokens_out))
            .unwrap_or((0, 0))
    }

    /// Whether `task_id` has met or crossed its token ceiling. A task with no
    /// ceiling — or an unknown task — is never over budget.
    pub fn over_budget(&self, task_id: &str) -> bool {
        let guard = self.lock();
        guard.get(task_id).is_some_and(|e| {
            e.ceiling
                .is_some_and(|c| e.tokens_in + e.tokens_out >= c)
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_spent_starts_at_zero() {
        let l = Ledger::new();
        l.register("t1", Some(100));
        assert_eq!(l.spent("t1"), (0, 0));
        assert!(!l.over_budget("t1"));
    }

    #[test]
    fn register_resets_existing_entry() {
        let l = Ledger::new();
        l.register("t1", Some(100));
        l.add_usage("t1", 10, 20);
        assert_eq!(l.spent("t1"), (10, 20));
        // Re-registering zeroes usage and installs the new ceiling.
        l.register("t1", None);
        assert_eq!(l.spent("t1"), (0, 0));
        assert!(!l.over_budget("t1"));
    }

    #[test]
    fn add_usage_is_additive() {
        let l = Ledger::new();
        l.register("t1", None);
        l.add_usage("t1", 10, 0);
        l.add_usage("t1", 0, 5);
        l.add_usage("t1", 3, 7);
        assert_eq!(l.spent("t1"), (13, 12));
    }

    #[test]
    fn over_budget_crosses_ceiling() {
        let l = Ledger::new();
        l.register("t1", Some(100));
        l.add_usage("t1", 10, 0); // in=10, out=0 → 10 < 100
        assert!(!l.over_budget("t1"));
        l.add_usage("t1", 0, 60); // total 70 < 100
        assert!(!l.over_budget("t1"));
        l.add_usage("t1", 0, 30); // total 100 >= 100 → over (>= semantics)
        assert!(l.over_budget("t1"));
        l.add_usage("t1", 0, 5); // total 105 stays over
        assert!(l.over_budget("t1"));
    }

    #[test]
    fn no_ceiling_never_over_budget() {
        let l = Ledger::new();
        l.register("t1", None);
        l.add_usage("t1", 1_000_000, 1_000_000);
        assert!(!l.over_budget("t1"));
    }

    #[test]
    fn unknown_task_safe_defaults() {
        let l = Ledger::new();
        // Never registered.
        assert_eq!(l.spent("ghost"), (0, 0));
        assert!(!l.over_budget("ghost"));
        // Metering a never-registered task creates it (no ceiling) and is safe.
        l.add_usage("ghost", 4, 6);
        assert_eq!(l.spent("ghost"), (4, 6));
        assert!(!l.over_budget("ghost"));
    }
}
