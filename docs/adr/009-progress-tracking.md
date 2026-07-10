# ADR-009: First-class progress tracking (advisor `watch` + `status`; daemon stall auto-recovery)

Status: accepted (Phase 1 = B+C). Phase 2 (A) deferred, sketched below.

## Context

Monitoring in-flight delegated work is load-bearing but currently lives in the
wrong place: it is an **ad-hoc advisor responsibility**. The advisor must poll
`task-status`, hand-assemble liveness from `logs`/`trace`/worktree `ls`, and
notice stalls/blocks itself. Two failure modes follow directly:

1. **The advisor cannot reliably re-invoke itself.** Timer mechanisms available
   to a Claude-session advisor (`ScheduleWakeup`, cron) do not fire between user
   turns in the observed environment; the only thing that re-invokes the advisor
   is a *background process exiting*. So a fire-and-forget "I'll check back"
   silently never happens — in one run a task sat `blocked` (a 2-line error) for
   ~6 hours with zero progress (OPERATING_LESSONS L17).
2. **Even when running, the advisor assesses liveness by hand** — a fragile dance
   of `task-status` + `logs` timestamp + worktree `ls`.

The daemon, by contrast, is always running and already observes every state
transition and every worker's PTY heartbeat. Monitoring belongs there.

## Decision

Invert the responsibility. **The daemon owns continuous monitoring, stall
detection, and auto-recovery; the advisor becomes event-driven**, pulled in only
at genuine decision points via a blocking wait. Three components:

- **A. Daemon-side liveness + stall detection + auto-recovery** — DEFERRED (Phase 2).
- **B. `maestro watch`** — a blocking "return when there's something to do" primitive.
- **C. `maestro status`** — a one-shot situational digest.

Phase 1 ships **B + C** (read-side, client-only, no daemon protocol/control-flow
change — both reuse `Request::TaskStatus`). Phase 2 ships A (a daemon control-flow
change; full adversarial review + integration test per CLAUDE.md).

## State vocabulary (from `EventKind`, journal/domain.rs)

A task's state is the latest event kind by `seq` (journal `current_state`).

- **ADVISOR-ACTIONABLE** (a human/advisor decision is pending):
  `verify_passed` (→ merge), `blocked` (→ close/respec), `failed` (→ diagnose/re-delegate).
- **TERMINAL-DONE**: `merged` (and `failed`/`blocked` are also terminal-resting).
- **TRANSIENT** (the daemon will progress these on its own — the advisor must NOT
  be woken for them): `created`, `queued`, `spawned`, `iterating`,
  `checks_started`, `checks_passed`, `checks_failed`, `verify_started`,
  `verify_failed`, `escalated`, `interrupted`.

The `checks_failed`→fix-in-place-retry and `*_failed`→`escalated` loops are the
daemon grinding forward; `watch` must treat them as transient, not actionable.

## B. `maestro watch --advisor <ID> [--task <ID>]... [--interval <secs>] [--timeout <secs>]`

Blocks, then exits 0 the moment a tracked task reaches an ADVISOR-ACTIONABLE
state, or all tracked tasks are terminal. Prints a digest of what triggered it.

- **Tracked set**: the given `--task`s, else all of the advisor's tasks that are
  non-terminal at start.
- **Returns (exit 0) when**: any tracked task's state ∈ {`verify_passed`,
  `blocked`, `failed`}, OR every tracked task's state ∈ {`verify_passed`,
  `blocked`, `failed`, `merged`}. Prints each triggering task + state + why.
- **Keeps polling (does NOT return) while** any tracked task is in a transient
  state and none is actionable. `checks_failed`/`verify_failed`/`escalated` do
  NOT return — they are mid-flight retry/escalation.
- **Mechanism**: client-side poll loop over `Request::TaskStatus { advisor, None }`
  (no subscription exists; mirror `wait_for_socket_gone`'s loop, cli/main.rs:634).
  `--interval` default 5s. `--timeout` optional backstop → exit non-zero.
- **Advisor usage**: run in the background; the harness re-invokes the advisor when
  `watch` exits, i.e. exactly when a decision is due. This is the correct,
  first-class version of the throwaway watcher script in OPERATING_LESSONS L17.

## C. `maestro status --advisor <ID> [--task <ID>]`

One call → full situational awareness. Reuses `Request::TaskStatus`.

- Per task: state, tier, title, age since `created_at`, and a derived
  **needs-attention** flag (true iff state ∈ actionable set).
- Grouped digest: counts by state; an ACTIONABLE section listing tasks the advisor
  should act on (verify_passed → merge; blocked/failed → diagnose) and a WORKING
  section (transient). A final one-line summary (`N working, M need attention`).
- Exit 0 always (it is a report). (Liveness *age since last worker output* — a
  stall signal — needs Phase-2 daemon liveness data; Phase 1 uses `created_at` age
  as a coarse proxy and notes this.)

## Wiring (from the read-side map)

- Add `Watch`/`Status` variants to `enum Command` (cli/main.rs:35); handlers mirror
  `cmd_task_status` (main.rs:372). Both use `ensured_request(profile,
  &Request::TaskStatus { advisor_session_id, state: None })` and read
  `Response::TaskStatus { tasks: Vec<PsRow> }`.
- A shared `fn state_class(&str) -> {Actionable, Transient, Terminal}` helper
  encodes the vocabulary above (single source of truth; unit-tested).
- **No new `Request`/`Response` variant; no daemon dispatch/control-flow change.**

## Testing / review (Phase 1)

- Unit test `state_class` over EVERY `EventKind` string (guards the transient-vs-
  actionable partition — the load-bearing semantics).
- Integration test for `watch`: drive a task through transient states (assert it
  does NOT return) then to `verify_passed`/`blocked` (assert it returns promptly
  with the right digest); and the all-terminal early-return. Mirror the daemon test
  harness used by `tests/fix_in_place*.rs`.
- `watch`'s return semantics get an adversarial review (a wrong partition would
  either wake the advisor constantly or never) even though it is read-side.

## Phase 2 sketch — A. Daemon liveness + stall auto-recovery (separate ADR/PR)

The daemon tracks per-task liveness (last PTY output byte via the driver's session
buffer — the same signal `watchdog_minutes` already uses, but continuous and
finer). A configurable `stall_timeout` (e.g. 5 min of no output AND no transition)
< the coarse watchdog flags `suspected_stall`, snapshots a diagnostic, and
auto-acts (kill → fix-in-place if edits exist else fresh), emitting
`stall_detected`/`auto_recovered` events. Tasks self-heal from stalls with zero
advisor involvement. `status` then shows true liveness age; `watch` still only
returns for advisor-level decisions. Config: `[monitoring] stall_timeout_seconds`,
`stall_action = snapshot_kill_retry | flag_only`. This is a daemon control-flow
change → full adversarial review + a "worker goes silent → assert auto-recovery"
integration test.

Phase 2 MUST also fix a latent reconciliation hole surfaced by the Phase-1 review:
`startup.rs` reconciliation can leave a task resting permanently at `interrupted`
if `append_event(Failed)` fails and the daemon silently warn-and-continues — a
task stuck at the transient `interrupted` state makes `watch` block forever on it.
Fix: on a failed terminal-event append during reconciliation, surface it (error,
not warn) and ensure the task reaches a terminal state (retry / in-memory mark).
Until Phase 2 lands, **run `watch` with `--timeout` as a backstop** so a stuck task
degrades to a timed re-check instead of an infinite block; the advisor skill
documents this.
