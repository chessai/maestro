//! Client-side rendering + poll loop for `maestro status` and `maestro watch`
//! (ADR-009, Phase 1). The load-bearing state partition and the pure
//! `watch_once` decision core live in `maestro_journal::progress`; this module
//! is only I/O + presentation: it fetches `TaskStatus`, renders the digests,
//! and drives the blocking poll loop. No daemon protocol/control-flow change.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use maestro_journal::progress::{state_class, suggested_action, watch_once, WatchTrigger};
use maestro_journal::proto::{PsRow, Request, Response};

/// Outcome of a `watch` run, mapped to a process exit code by the caller.
pub enum WatchOutcome {
    /// A tracked task became actionable / all tracked tasks are terminal. Exit 0.
    Returned,
    /// The `--timeout` backstop fired before any return condition. Exit 2.
    TimedOut,
}

/// Fetch the advisor's task rows via `Request::TaskStatus` (state filter=None).
/// Shared by both commands; identical to `cmd_task_status`'s request.
fn fetch_tasks(
    send: &mut dyn FnMut(&Request) -> Result<Response>,
    advisor: &str,
) -> Result<Vec<PsRow>> {
    let resp = send(&Request::TaskStatus {
        advisor_session_id: advisor.to_string(),
        state: None,
    })?;
    match resp {
        Response::TaskStatus { tasks } => Ok(tasks),
        Response::Error { message } => bail!("daemon error on task-status: {message}"),
        other => bail!("unexpected response to TaskStatus: {other:?}"),
    }
}

/// `maestro status --advisor <ID> [--task <ID>]`: one-shot situational digest.
/// Always returns `Ok(())` (exit 0) — it is a report.
///
/// `send` is the transport (real: `ensured_request`); injected so the formatter
/// is unit-testable without a daemon.
pub fn run_status(
    send: &mut dyn FnMut(&Request) -> Result<Response>,
    advisor: &str,
    task: Option<&str>,
) -> Result<()> {
    let mut tasks = fetch_tasks(send, advisor)?;
    if let Some(t) = task {
        tasks.retain(|r| r.task_id == t);
    }
    print!("{}", render_status(&tasks));
    Ok(())
}

/// Render the `status` digest as a string (pure over the row set → testable).
pub fn render_status(tasks: &[PsRow]) -> String {
    let now = OffsetNow::capture();
    let mut out = String::new();

    if tasks.is_empty() {
        out.push_str("no tasks\n");
        return out;
    }

    // Per-task lines: state, tier, age, title, and a `*` needs-attention marker
    // iff actionable.
    out.push_str(&format!(
        "{:<28} {:<6} {:<16} {:>6}  TITLE\n",
        "TASK_ID", "TIER", "STATE", "AGE"
    ));
    for t in tasks {
        let attn = if state_class(&t.state).is_actionable() {
            "*"
        } else {
            " "
        };
        out.push_str(&format!(
            "{} {:<28} T{:<5} {:<16} {:>6}  {}\n",
            attn,
            t.task_id,
            t.tier.as_int(),
            t.state,
            now.age_since(&t.created_at),
            t.title
        ));
    }

    // Grouped counts by state (stable order: by state string).
    let mut counts: Vec<(String, usize)> = Vec::new();
    for t in tasks {
        match counts.iter_mut().find(|(s, _)| s == &t.state) {
            Some((_, n)) => *n += 1,
            None => counts.push((t.state.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| a.0.cmp(&b.0));
    out.push_str("\nby state:\n");
    for (state, n) in &counts {
        out.push_str(&format!("  {state:<16} {n}\n"));
    }

    // ACTIONABLE section: tasks the advisor should act on now.
    let actionable: Vec<&PsRow> = tasks
        .iter()
        .filter(|t| state_class(&t.state).is_actionable())
        .collect();
    if !actionable.is_empty() {
        out.push_str("\nACTIONABLE:\n");
        for t in &actionable {
            let action = suggested_action(&t.state).unwrap_or("review");
            out.push_str(&format!(
                "  {} [{}] → {}  {}\n",
                t.task_id, t.state, action, t.title
            ));
        }
    }

    // A task is "working" if it is neither actionable nor terminal (transient
    // and still in flight). `merged` counts as neither working nor needing
    // attention.
    let need_attention = actionable.len();
    let working = tasks
        .iter()
        .filter(|t| {
            let c = state_class(&t.state);
            !c.is_terminal()
        })
        .count();
    out.push_str(&format!(
        "\nsummary: {working} working, {need_attention} need attention\n"
    ));
    out
}

/// `maestro watch`: block, polling `TaskStatus` every `interval`, until a
/// tracked task is actionable OR all tracked tasks are terminal; then print the
/// triggering digest and return [`WatchOutcome::Returned`]. `timeout` (optional)
/// is a backstop → [`WatchOutcome::TimedOut`].
///
/// The tracked set is `explicit_tasks` if non-empty, else all of the advisor's
/// tasks that are NON-TERMINAL at the first poll (a task already
/// merged/blocked/failed/verify_passed at start is not something to wait on).
///
/// `send`/`sleep`/`now` are injected so the loop is unit-testable with a scripted
/// transport and a fake clock (no real daemon, no real sleeping).
pub fn run_watch(
    send: &mut dyn FnMut(&Request) -> Result<Response>,
    sleep: &mut dyn FnMut(Duration),
    now: &mut dyn FnMut() -> Instant,
    advisor: &str,
    explicit_tasks: &[String],
    interval: Duration,
    timeout: Option<Duration>,
) -> Result<WatchOutcome> {
    let start = now();
    let deadline = timeout.map(|t| start + t);

    // First poll establishes the tracked set.
    let rows = fetch_tasks(send, advisor)?;
    let tracked: Vec<String> = if !explicit_tasks.is_empty() {
        explicit_tasks.to_vec()
    } else {
        rows.iter()
            .filter(|r| !state_class(&r.state).is_terminal())
            .map(|r| r.task_id.clone())
            .collect()
    };

    if tracked.is_empty() {
        // Nothing to wait on: either the advisor has no tasks, or all its tasks
        // were already terminal at start. Report and return promptly (exit 0).
        println!("watch: no non-terminal tasks to track for advisor {advisor}; nothing to wait on");
        return Ok(WatchOutcome::Returned);
    }

    // Adversarial-review Finding 2: an explicit `--task` that is absent at the
    // first poll is almost always a wrong/typo'd id — `delegate` writes `created`
    // synchronously before returning the id, so a real just-delegated task is
    // visible immediately. Warn (to stderr) so a bad id is not silently reported
    // as "all terminal / nothing to wait on".
    if !explicit_tasks.is_empty() {
        let missing: Vec<&str> = explicit_tasks
            .iter()
            .map(String::as_str)
            .filter(|t| !rows.iter().any(|r| r.task_id == **t))
            .collect();
        if missing.len() == explicit_tasks.len() {
            eprintln!(
                "watch: warning — none of the requested --task id(s) are known to the daemon \
                 ({}); check the id(s)",
                missing.join(", ")
            );
        }
    }

    // Evaluate the first snapshot before sleeping (a task may already be
    // actionable / all-terminal, e.g. an explicit --task that is already done).
    if let Some(digest) = watch_once(&rows, &tracked) {
        print!("{}", render_watch_digest(&digest, &tracked));
        return Ok(WatchOutcome::Returned);
    }

    loop {
        if let Some(dl) = deadline {
            if now() >= dl {
                let secs = timeout.map(|t| t.as_secs()).unwrap_or(0);
                println!(
                    "watch: timed out after {secs}s with no actionable task (tracked {} task(s)); \
                     the daemon may still be working — re-run watch or check `maestro status`",
                    tracked.len()
                );
                return Ok(WatchOutcome::TimedOut);
            }
        }
        sleep(interval);
        let rows = fetch_tasks(send, advisor)?;
        if let Some(digest) = watch_once(&rows, &tracked) {
            print!("{}", render_watch_digest(&digest, &tracked));
            return Ok(WatchOutcome::Returned);
        }
    }
}

/// Render a `watch` return digest (pure → testable).
pub fn render_watch_digest(
    digest: &maestro_journal::progress::WatchDigest,
    tracked: &[String],
) -> String {
    let mut out = String::new();
    match digest.trigger {
        WatchTrigger::Actionable => {
            out.push_str("watch: a tracked task needs attention:\n");
        }
        WatchTrigger::AllTerminal => {
            if digest.lines.is_empty() {
                out.push_str(&format!(
                    "watch: none of the {} tracked task(s) are present anymore \
                     (disappeared/pruned); nothing to wait on\n",
                    tracked.len()
                ));
                return out;
            }
            out.push_str("watch: all tracked tasks are terminal:\n");
        }
    }
    for line in &digest.lines {
        out.push_str(&format!(
            "  {} [{}] → {}  {}\n",
            line.task_id, line.state, line.action, line.title
        ));
    }
    out
}

/// A captured "now" for age formatting. Parsing failures degrade to `?` rather
/// than erroring — `status` must never fail to render.
struct OffsetNow {
    now: Option<time::OffsetDateTime>,
}

impl OffsetNow {
    fn capture() -> Self {
        OffsetNow {
            now: Some(time::OffsetDateTime::now_utc()),
        }
    }

    /// Age from an RFC3339 `created_at` to now, as a compact `Nd/Nh/Nm/Ns`.
    fn age_since(&self, created_at: &str) -> String {
        let (Some(now), Ok(created)) = (
            self.now,
            time::OffsetDateTime::parse(
                created_at,
                &time::format_description::well_known::Rfc3339,
            ),
        ) else {
            return "?".to_string();
        };
        let secs = (now - created).whole_seconds();
        format_age(secs)
    }
}

/// Format a duration in seconds as a compact human age. Negative (clock skew) →
/// `0s`.
fn format_age(secs: i64) -> String {
    if secs < 0 {
        return "0s".to_string();
    }
    let secs = secs as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::domain::{ContainmentLevel, Tier};

    fn row(task_id: &str, state: &str) -> PsRow {
        PsRow {
            task_id: task_id.to_string(),
            title: format!("do {task_id}"),
            tier: Tier::T0,
            model: "mock".to_string(),
            containment: ContainmentLevel::L0,
            state: state.to_string(),
            // Old timestamp so age renders deterministically as days.
            created_at: "2020-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(-5), "0s");
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(86_400), "1d");
    }

    /// `status` renders a representative task set without panicking and includes
    /// the key sections.
    #[test]
    fn status_smoke_renders_all_sections() {
        let tasks = vec![
            row("A", "iterating"),
            row("B", "verify_passed"),
            row("C", "blocked"),
            row("D", "merged"),
            row("E", "checks_failed"),
        ];
        let s = render_status(&tasks);
        assert!(s.contains("TASK_ID"), "has a header");
        assert!(s.contains("by state:"), "has grouped counts");
        assert!(s.contains("ACTIONABLE:"), "has an actionable section");
        // verify_passed → merge, blocked → diagnose surfaced.
        assert!(s.contains("→ merge"), "verify_passed suggests merge");
        assert!(s.contains("→ diagnose"), "blocked suggests diagnose");
        // Two actionable (B verify_passed, C blocked); working = A + E (transient);
        // D merged is neither.
        assert!(
            s.contains("2 working, 2 need attention"),
            "summary line, got:\n{s}"
        );
        // The needs-attention marker appears on actionable rows only.
        assert!(s.contains("* B"), "B marked needs-attention");
    }

    #[test]
    fn status_empty_is_no_tasks() {
        assert_eq!(render_status(&[]), "no tasks\n");
    }

    /// A scripted transport: returns each queued response in order.
    struct Script {
        responses: Vec<Response>,
        idx: usize,
    }
    impl Script {
        fn new(responses: Vec<Response>) -> Self {
            Script { responses, idx: 0 }
        }
        fn send(&mut self, _req: &Request) -> Result<Response> {
            let r = self.responses[self.idx].clone();
            self.idx += 1;
            Ok(r)
        }
    }

    fn task_status(rows: Vec<PsRow>) -> Response {
        Response::TaskStatus { tasks: rows }
    }

    /// watch keeps polling through transient states, then returns on actionable.
    /// Fake clock never advances past a (large) deadline, so timeout never fires.
    #[test]
    fn watch_polls_through_transient_then_returns_actionable() {
        let mut script = Script::new(vec![
            task_status(vec![row("A", "created")]),   // first poll: tracked = {A}
            task_status(vec![row("A", "iterating")]), // still transient
            task_status(vec![row("A", "checks_failed")]), // transient (mid-flight)
            task_status(vec![row("A", "verify_passed")]), // actionable → return
        ]);
        let mut sleeps = 0u32;
        let mut sleep = |_d: Duration| sleeps += 1;
        let t0 = Instant::now();
        let mut now = || t0; // clock frozen: no timeout

        let out = run_watch(
            &mut |r| script.send(r),
            &mut sleep,
            &mut now,
            "adv",
            &[],
            Duration::from_millis(1),
            Some(Duration::from_secs(3600)),
        )
        .unwrap();
        assert!(matches!(out, WatchOutcome::Returned));
        // Polled 4 times → slept 3 times (after polls 1..3, before the 4th).
        assert_eq!(sleeps, 3, "slept between each non-returning poll");
    }

    /// A task that goes straight to failed returns on the FIRST poll (no sleep).
    #[test]
    fn watch_first_poll_actionable_returns_immediately() {
        let mut script = Script::new(vec![task_status(vec![row("A", "failed")])]);
        let mut sleep = |_d: Duration| panic!("must not sleep when already actionable");
        let t0 = Instant::now();
        let mut now = || t0;
        let out = run_watch(
            &mut |r| script.send(r),
            &mut sleep,
            &mut now,
            "adv",
            &[],
            Duration::from_millis(1),
            None,
        )
        .unwrap();
        assert!(matches!(out, WatchOutcome::Returned));
    }

    /// All tracked tasks merged at start → tracked set is empty (all terminal) →
    /// returns promptly, "nothing to wait on".
    #[test]
    fn watch_all_terminal_at_start_returns_empty_tracked() {
        let mut script = Script::new(vec![task_status(vec![row("A", "merged")])]);
        let mut sleep = |_d: Duration| panic!("must not sleep");
        let t0 = Instant::now();
        let mut now = || t0;
        let out = run_watch(
            &mut |r| script.send(r),
            &mut sleep,
            &mut now,
            "adv",
            &[],
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert!(matches!(out, WatchOutcome::Returned));
    }

    /// The timeout backstop fires: the task stays transient and the fake clock
    /// jumps past the deadline.
    #[test]
    fn watch_times_out_when_task_stays_transient() {
        // Enough transient responses to outlast the poll before the clock trips.
        let mut script = Script::new(vec![
            task_status(vec![row("A", "iterating")]), // first poll, tracked = {A}
            task_status(vec![row("A", "iterating")]), // still transient (post-sleep poll)
        ]);
        let mut sleep = |_d: Duration| {};
        // Clock: first call (start) = t0; subsequent calls jump past the deadline.
        let t0 = Instant::now();
        let mut calls = 0u32;
        let mut now = || {
            calls += 1;
            if calls <= 1 {
                t0
            } else {
                t0 + Duration::from_secs(1000)
            }
        };
        let out = run_watch(
            &mut |r| script.send(r),
            &mut sleep,
            &mut now,
            "adv",
            &[],
            Duration::from_millis(1),
            Some(Duration::from_secs(10)),
        )
        .unwrap();
        assert!(matches!(out, WatchOutcome::TimedOut), "should time out");
    }

    /// An explicit --task that is already verify_passed returns on first poll,
    /// even though the auto-tracked set (non-terminal) would have been empty.
    #[test]
    fn watch_explicit_task_already_actionable_returns() {
        let mut script = Script::new(vec![task_status(vec![row("A", "verify_passed")])]);
        let mut sleep = |_d: Duration| panic!("must not sleep");
        let t0 = Instant::now();
        let mut now = || t0;
        let out = run_watch(
            &mut |r| script.send(r),
            &mut sleep,
            &mut now,
            "adv",
            &["A".to_string()],
            Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert!(matches!(out, WatchOutcome::Returned));
    }
}
