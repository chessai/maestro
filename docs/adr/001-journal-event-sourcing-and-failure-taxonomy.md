# ADR-001: Journal, Event Sourcing, and Failure Taxonomy

Status: DRAFT
Load-bearing: yes — telemetry, trace reuse, and every other ADR's audit story build on this schema.

## Context

maestro needs a durable record of every task lifecycle for four consumers:

1. **Live state** — the daemon and advisor need "what is running / blocked / done."
2. **Telemetry** — rubric tuning requires grouping failures by tier, model, containment level, and failure kind.
3. **Trace corpus** — the advisor reuses prior specs and verifier reports as context for related work.
4. **Forensics** — distinguishing "model misbehaved" from "harness broke" after the fact.

A mutable `status` column serves (1) but destroys (2)–(4). Multiple advisor sessions share one daemon per machine, so IDs must be globally unique.

## Decision

### Storage

SQLite, single file at `$XDG_DATA_HOME/maestro/journal.db` (macOS: `~/Library/Application Support/maestro/journal.db`). WAL mode, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`. The daemon is the sole writer; CLI and MCP proxy read via the daemon socket, never the file directly (avoids reader-schema skew and keeps the socket the single API surface).

### Identifiers

ULIDs everywhere (`task_id`, `event_id`, `session_id`, `advisor_session_id`). Sortable, globally unique across advisors, safe to expose in CLI output and branch names (`maestro/<task-ulid>`).

### Event sourcing

Task state transitions are append-only events. Current state is derived (latest terminal-ordering event per task), materialized as a view. UPDATEs on task rows are forbidden except for denormalized cache columns explicitly marked as derived.

Ordering within a task is by the daemon-assigned monotonic `seq`, not the ULID timestamp: the single-writer daemon stamps each event with the next `seq` for its task, so same-millisecond transitions (`plan_rejected` → `escalated`) never misorder the way a ULID random-suffix tiebreak could. ULIDs remain the identity.

```sql
CREATE TABLE advisors (
  advisor_session_id TEXT PRIMARY KEY,          -- ULID, minted by MCP proxy at startup
  profile            TEXT NOT NULL,             -- config profile name
  advisor_model      TEXT NOT NULL,             -- e.g. claude-opus-4-7
  advisor_context    TEXT NOT NULL,             -- standard | 1m
  started_at         TEXT NOT NULL              -- ISO 8601 UTC
);

CREATE TABLE tasks (
  task_id            TEXT PRIMARY KEY,          -- ULID
  advisor_session_id TEXT NOT NULL REFERENCES advisors(advisor_session_id),
  parent_task        TEXT REFERENCES tasks(task_id),   -- reserved for DAG (v1: NULL)
  depends_on         TEXT,                      -- JSON array of task_ids (v1: NULL)
  tier               INTEGER NOT NULL,          -- 0 | 1 | 2
  model              TEXT NOT NULL,             -- explicit, from config; never inferred
  containment_level  INTEGER NOT NULL,          -- see ADR-004
  spec               TEXT NOT NULL,             -- JSON TaskSpec, immutable
  workspace          TEXT,                      -- worktree path
  base_ref           TEXT NOT NULL,
  branch             TEXT NOT NULL,             -- maestro/<task-ulid>
  created_at         TEXT NOT NULL
);

CREATE TABLE events (
  event_id  TEXT PRIMARY KEY,                   -- ULID (identity)
  task_id   TEXT NOT NULL REFERENCES tasks(task_id),
  ts        TEXT NOT NULL,
  seq       INTEGER NOT NULL,                   -- per-task monotonic, daemon-assigned; the ordering key
  kind      TEXT NOT NULL,                      -- see Event kinds
  payload   TEXT,                               -- JSON, kind-specific
  UNIQUE (task_id, seq)
);
CREATE INDEX events_task ON events(task_id, seq);

CREATE TABLE advisor_events (                    -- advisor-scoped occurrences with no task
  event_id           TEXT PRIMARY KEY,          -- ULID
  advisor_session_id TEXT NOT NULL REFERENCES advisors(advisor_session_id),
  ts                 TEXT NOT NULL,
  seq                INTEGER NOT NULL,          -- per-advisor monotonic, daemon-assigned
  kind               TEXT NOT NULL,             -- advisor_write | …
  payload            TEXT,                      -- JSON, kind-specific
  UNIQUE (advisor_session_id, seq)
);
CREATE INDEX advisor_events_session ON advisor_events(advisor_session_id, seq);

CREATE TABLE sessions (                          -- one row per PTY / one-shot run
  session_id  TEXT PRIMARY KEY,                 -- ULID
  task_id     TEXT REFERENCES tasks(task_id),   -- NULL for shim calls (no task, no workspace)
  advisor_session_id TEXT REFERENCES advisors(advisor_session_id),  -- set when task_id is NULL
  role        TEXT NOT NULL,                    -- implementer | verifier | plan_check | shim  (note: plan_check is reserved but not yet used in v1 — plan-echo is handled inline by the driver, not as a separate session)
  model       TEXT NOT NULL,
  kind        TEXT NOT NULL,                    -- driven_pty | one_shot_api
  workspace   TEXT,
  started_at  TEXT NOT NULL,
  ended_at    TEXT,
  exit_status TEXT,                             -- ok | error | killed | wedged
  turns       INTEGER,
  tokens_in   INTEGER,
  tokens_out  INTEGER,
  log_path    TEXT                              -- captured PTY output
);

CREATE TABLE verifier_reports (
  report_id  TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(task_id),
  session_id TEXT NOT NULL REFERENCES sessions(session_id),
  attempt    INTEGER NOT NULL,
  independence TEXT NOT NULL,                   -- cross_provider | cross_model | fresh_context_only
  report     TEXT NOT NULL                      -- JSON, schema in ADR-002
);

CREATE TABLE shim_cache (
  url          TEXT NOT NULL,
  schema_hash  TEXT NOT NULL,
  retrieved_at TEXT NOT NULL,
  payload      TEXT NOT NULL,                   -- JSON extraction result
  PRIMARY KEY (url, schema_hash)
);
```

Promoted-and-indexed columns (`tier`, `model`, `containment_level`, timestamps, token counts) are the telemetry filter set; everything else lives in JSON and is reached via SQLite JSON functions.

### Event kinds

`created`, `queued` (concurrency cap saturation), `containment_downgraded` (payload: requested_level, actual_level, tighten_applied), `spawned`, `plan_submitted`, `plan_rejected`, `iterating`, `impl_finished`, `checks_started`, `checks_failed`, `verify_started`, `verify_failed`, `verify_passed`, `escalated` (payload: from_tier, to_tier, reason), `blocked`, `merged`, `interrupted` (payload: `reason` — `daemon_restart` when a daemon restart tore the session down), `failed` (payload: failure kind, optional `superseded_by` task_id), `pruned`.

**Note (v1):** `plan_submitted` is **reserved but not yet emitted in v1** — the plan-echo is handled in the driver and checked inline; it is not journaled as a separate event yet. `merged` is likewise reserved but not yet emitted (merging is human-only in v1; see ADR-006). These values are part of the frozen taxonomy and will be emitted when the corresponding features are built.

Advisor-scoped kinds live in `advisor_events` (no `task_id`): `advisor_write` (payload: `path`; advisor write to an allowlisted in-repo path, ADR-006).

### Failure taxonomy (frozen)

Terminal `failed` events carry exactly one of:

| kind | meaning |
|---|---|
| `spec_rejected` | orchestrator rejected the TaskSpec (schema/validation) before spawn |
| `plan_rejected` | Codex plan-echo failed the spec check; no edits made |
| `verification_failed` | blocked task closed after top-tier verify failure (abandoned or superseded via `close_task`; payload may carry `superseded_by`) |
| `scope_violation` | out-of-allowlist diff detected post-session |
| `budget_exhausted` | turn or token budget hit |
| `model_unavailable` | configured model missing/unauthenticated at delegation time |
| `sandbox_killed` | containment layer terminated the session |
| `session_wedged` | PTY unresponsive past watchdog timeout |
| `internal_error` | maestro bug or environment fault |
| `interrupted_human` | CLI kill |
| `interrupted_advisor` | advisor `kill_task` |

**Note (v1):** `sandbox_killed` is **reserved but not yet emitted in v1** — the current containment model confines rather than actively killing; a denied write surfaces as `checks_failed` (scope_violation) rather than a mid-session kill. This failure kind is part of the frozen taxonomy and will be emitted when active containment kill signals are implemented.

Shim errors (`backend_unavailable`, fetch failures) are **tool-result errors returned to the advisor**, not task failures; they never enter this taxonomy.

### Retention

Events, sessions, reports, logs: kept indefinitely. Worktrees pruned on merge or 7 days after terminal state (`pruned` event recorded). `blocked` is non-terminal, so blocked worktrees are exempt from the sweep; instead a long-blocked task (default 7 days, config) raises an inbox reminder (ADR-006) rather than being pruned, since blocked work may still be resumed. `shim_cache` TTL 24h, vacuumed opportunistically.

## Consequences

- Timeline reconstruction (`spawned → plan_rejected → escalated → verify_passed`) is a single indexed query; this is both the debugging surface and the trace corpus.
- Telemetry questions ("do Tier 1 tasks fail verification more on Codex?", "do L0-contained tasks violate scope more?") are `GROUP BY` over promoted columns joined to terminal events.
- Adding an event kind is cheap; adding a failure kind after freeze requires an ADR amendment, because historical telemetry buckets shift.

## Tradeoffs accepted

- Derived-state views cost a little query complexity vs. a status column; accepted for auditability.
- Daemon-only writes mean the CLI cannot operate with the daemon down; accepted — the CLI can auto-spawn the daemon (ADR-006).
- Indefinite log retention grows unboundedly in principle; accepted at personal-tool scale, revisit if logs exceed low GBs.
