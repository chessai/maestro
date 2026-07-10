# Operating lessons — maestro-as-advisor field notes

A running log of lessons learned from **operating maestro on live runs** (as
the advisor). Each entry is something that should eventually be folded into
maestro itself — code, the driven adapter, the gate, spec/prompt defaults, an
ADR, or operator docs. Until then, **this file is the canonical record**; do
not rely on ephemeral advisor memory.

Status legend: **FIXED** (landed in code) · **open** (not yet done) ·
**rule** (operational discipline until/unless codified).

Provenance: mtg-engine card-impl runs (2026-07-08/09) and theseus M1 runs
(2026-07-09).

---

## Harness robustness

### L1. In-process critical-path network calls need a timeout + supervision — FIXED (`fb1617a`)
- **Observed:** `AnthropicPlanChecker::check()` makes a blocking `ureq` call with
  no timeout, in-process on the driven thread *between* the plan and execute
  phases. A hung connection (established, no response) blocked `check()`
  forever; the execute phase never spawned, `join.join()` never returned, no
  terminal event fired, and the PTY watchdog (which only covers the session)
  never reaped it. The task sat in `Iterating` indefinitely. Hit live on a
  theseus-syntax delegation.
- **Impact:** a single flaky connection can silently strand any task forever.
- **Incorporation:** timeout added (connect 10s + overall 120s; fails safe to
  permissive `Accept`). **Generalize:** audit every in-process blocking/network
  call on the critical path for a deadline, and ensure a supervisor covers
  non-PTY waits — the watchdog only covers the driven PTY session, not
  daemon-internal calls. Consider a per-attempt wall-clock deadline spanning
  plan+check+execute, not just the execute PTY.

### L2. The persistent journal has no schema migration — FIXED
- **Observed:** `~/.local/share/maestro/journal.db` created by an older binary
  lacked the `repo_path` column; `delegate` failed with `sqlite error: table
  tasks has no column named repo_path`. The `/goal` skill sidesteps this by
  using an ephemeral `/tmp` XDG data dir per run (fresh schema each time).
- **Impact:** a persistent journal breaks across binary upgrades; silent unless
  you know to wipe it.
- **Incorporation:** LANDED. `schema::migrate` now detects two incompatible
  cases and fails LOUD with a guided `Error::SchemaVersion` (instead of a raw
  sqlite error): (a) a **pre-versioning legacy DB** (`user_version == 0` but user
  tables already exist — the exact repo_path case), detected via `sqlite_master`,
  and (b) a **newer DB** (`user_version > SCHEMA_VERSION`). Both messages point
  the operator at resetting `journal.db`. A fresh DB still applies the v1 DDL +
  stamps the version; re-running is idempotent. The daemon's `Server::start`
  propagates the guided message with the db path. Tests in
  `maestro-journal/src/schema.rs`.

### L3. Don't move `base_ref` under an in-flight task — FIXED
- **Observed:** merging a completed task into `base_ref` while a *sibling* task
  still runs against it makes the gate diff the sibling's worktree against the
  **advanced tip**; the just-merged files appear as **deletions** outside the
  sibling's `file_allowlist` → `scope_violation` → the sibling fails despite
  doing nothing wrong. Hit live: theseus-eval merged into `theseus-v2` while
  theseus-syntax ran against it → syntax failed on `crates/theseus-eval/*`
  deletions.
- **Impact:** spurious task failures + wasted worker runs during parallel rounds.
- **Incorporation:** LANDED. `delegate` resolves `base_ref` to the concrete
  commit SHA it points at at spawn (`worktree::resolve_to_sha`) and threads that
  pinned commit through the pipeline. Every attempt's worktree is cut from it AND
  every diff (scope/allowlist, checks-failed, verifier structural, forensic
  snapshot) is taken against it — never the live `base_ref` tip. The **merge
  target** stays the live symbolic `base_ref` so `merge_task` can still
  fast-forward it. A ref that can't be peeled to a commit falls back to the
  symbolic ref (prior behavior). Regression test
  `worktree::pinned_base_scope_diff_survives_base_advance` proves that advancing
  the base after spawn does NOT produce a spurious scope violation. The old
  operational rule (finish siblings before merging) is now belt-and-suspenders,
  not required for correctness.

---

## Driven worker / CLI adapter

### L4. Turn cap is execute-phase only; the plan phase is uncapped — FIXED
- **Observed:** the driven `claude` adapter enforces the turn cap only in the
  execute phase; the plan phase runs uncapped (a plan is "a single short turn").
- **Impact:** a long or looping plan phase is unbounded (see also L1).
- **Incorporation:** LANDED. `run_json_phase` now takes an optional per-phase
  wall-clock ceiling and tears the phase down once elapsed time since spawn
  exceeds it — independent of activity, so it fires even when the idle watchdog
  never would (new `JsonPhaseKind::WallClockExceeded`). The plan phase passes
  `DrivenConfig.plan_ceiling` (derived by the daemon as 5× the watchdog, floored
  at 5 min, overridable via `MAESTRO_PLAN_CEILING_SECONDS`); the execute phase
  stays turn-capped. A plan-phase wall-clock exceed maps to `PlanRejected`
  (terminal, zero edits). Test:
  `claude::plan_phase_wall_clock_ceiling_tears_down_a_long_plan`.

### L5. Driven workers can't self-verify (cargo/nix not in `settings.json` allow) — open (spec rule applied)
- **Observed:** driven workers inherit the operator's `~/.claude/settings.json`.
  Its `allow` list permits read-only commands (`cat`, `grep`, `git log`, `nix
  eval`, `nix flake show`…) but **not `cargo` or `nix develop`**. In headless
  `--print` + `acceptEdits`, a non-allowed Bash command → "This command requires
  approval" → auto-denied (no one to approve). Workers flailed 45–110×/run
  trying to build, burning turns.
- **Impact:** wasted turns and confusing logs; **non-fatal** because the
  mechanical gate does the authoritative build/clippy/test independently of the
  worker (core & eval passed despite dozens of rejections).
- **Incorporation, two levers:**
  1. **Spec/prompt (done for theseus):** don't instruct workers to run
     build/test commands they can't — tell them the gate verifies, and to get it
     right by careful reading. Removes the flailing without weakening security.
  2. **Adapter (optional):** if worker self-verification is wanted, inject a
     *scoped* permission allow-list for exactly the spec's `check_commands` (e.g.
     `--allowedTools` / a settings overlay) rather than relying on the operator's
     global settings. **Security tension:** at containment L0 the settings
     allow-list is a real guardrail, and `Bash(nix develop …)` = arbitrary exec
     (`bash -c '…'`). Prefer lever 1 + gate-authoritative, or a tightly-scoped
     lever 2 — never a blanket allow.

### L13. End a driven phase on the `result` event, not on process exit — FIXED
- **Observed:** the driven adapter's phase loop only ended a phase when the child
  process **exited** (`try_wait`). But `claude --print` sometimes emits its
  terminal `result` stream-json event and then **never exits** (reproducibly
  after subagent-heavy plan phases — the plan is done, `plan_results=1`, but the
  process lingers idle). The loop then spun until the **idle watchdog** reaped it
  30 min later as `Wedged` → task `failed`. This was the root cause of theseus-
  syntax attempts 3 and 4 stranding at the plan→execute handoff (L4's plan-phase
  ceiling only made it fail *faster*, not succeed).
- **Impact:** a semantically-complete phase fails because the process doesn't
  exit; the whole task dies at the handoff, flakily.
- **Incorporation:** LANDED. `run_json_phase` now ends the phase as soon as the
  authoritative `result` event is parsed (`ParseState.result_seen`): it grace-
  drains final lines, then — if the process is still alive — tears it down and
  returns `Exited` with the result's own status (`result_is_error` → non-zero).
  The `result` event, not process exit, is claude's real "phase complete" signal.
  Regression test `claude::phase_completes_on_result_event_even_if_process_never_exits`
  (a mock that emits `result` then `sleep 3000` — completes on the event instead
  of wedging at the watchdog).

---

## Gate / verification

### L6. The gate must RUN the deliverable's tests, not just build — partly FIXED (warning landed)
- **Observed:** a light gate (build + clippy of one crate) never compiled the
  test files; 3 of 5 "verified" cards shipped with test-file defects that only a
  test-running gate catches.
- **Impact:** false "verified" — defects slip past a build-only gate.
- **Incorporation:** the "warn on a build-only `check_commands`" half is LANDED.
  `delegate` runs `check_commands_look_build_only` (pure heuristic: commands run
  but none contain a test-runner token — cargo test/nextest, pytest, jest, go
  test, npm test, …; biased toward NOT warning to avoid false alarms). On a hit
  it warns NON-FATALLY — a `tracing::warn` for the operator plus an advisory
  inlined in the `created` event payload (rides the advisor inbox without a new,
  frozen event kind). The task still runs; the advisor stays in control. Tests in
  `delegate.rs` (`build_only_check_commands_are_flagged`). NOT changed: making a
  test-running gate a HARD default (that stays advisor/spec discipline + the
  `/goal` per-card gate must compile+run the card's tests).

### L7. Gate hermeticity: scrub the daemon's environment — FIXED
- **Observed:** maestro's own tests read the daemon's `XDG_CONFIG_HOME`; a task
  could pass/fail based on the daemon's environment leaking into the gate.
- **Impact:** non-hermetic gate; environment-dependent verdicts.
- **Incorporation:** landed — `hermetic_scrub_keys()` removes
  `XDG_CONFIG/STATE/DATA/RUNTIME_HOME` + all `MAESTRO_*` (keeps
  `XDG_CACHE_HOME`) via `.env_remove()` on the check command. Keep gate commands
  hermetic by construction.

---

## Advisor / spec authoring

### L8. One task per delegation — open (advisor rule; partly in CLAUDE_MD)
- **Observed:** bundling separable deliverables into one spec fails as a unit —
  a 30-min wall-clock timeout killed a 3-deliverable task; splitting + a 60-min
  ceiling per single deliverable fixed it.
- **Impact:** bundled tasks are fragile and waste the whole run on one part's
  failure.
- **Incorporation:** codify as an advisor rule / spec-linter: warn when a spec
  has multiple independent acceptance criteria that could be separate tasks.
  Size wall-clock/turns to a single deliverable. (Guidance already added to the
  advisor's CLAUDE_MD section.)

### L9. Surface the allowlist to the worker; size it to the blast radius — partly FIXED
- **Observed:** the `file_allowlist` is post-hoc scope control (a diff check),
  invisible to the worker. A too-narrow allowlist makes a necessary refactor
  fail as `scope_violation`, or the worker splits a file to dodge it (observed:
  a worker moved `engine.rs` logic into an un-allowlisted `engine_body.rs`).
- **Impact:** narrow scope silently forbids legitimate refactors.
- **Incorporation:** surface the allowlist in the worker prompt as a **HARD
  BOUNDARY** (done in `driven_prompt`), and size allowlists to include the
  change's blast radius, not just the obvious file. Consider letting a worker
  *request* an allowlist widening (surfaced to the advisor) instead of silently
  failing.
- **Status — allowlist-widening request DEFERRED (this pass):** the request
  mechanism is a larger change than the L2/L3/L4/L6 items landed here. The clean
  designs both have real cost: (a) a new terminal/event kind (e.g.
  `scope_widen_requested`) collides with the FROZEN ADR-001 event taxonomy /
  failure taxonomy and needs an ADR amendment; (b) parsing the worker's
  natural-language "I need a wider allowlist" out of PTY output is brittle and
  couples the daemon to model phrasing. The `driven_prompt` HARD-BOUNDARY text
  (already landed) tells the worker to STOP and report rather than work around a
  narrow allowlist, which is the low-risk lever. Recommended next step (own PR):
  an ADR amendment adding a first-class `scope_widen_requested` event the worker
  emits via a structured tool call, surfaced to the advisor's inbox — designed,
  not rushed.

### L10. Profile/auth/setup notes for operators — docs
- **Observed / how it works:** driven tier0 uses the local `claude` CLI's own
  subscription auth (no API key needed for it); verifier + escalation tiers
  (opus/sonnet/haiku) need `ANTHROPIC_API_KEY` in the daemon env. Config lives at
  `$XDG_CONFIG_HOME/maestro/config.toml`; `base_ref` is a **per-delegation spec
  field**, not a profile field. `doctor` validates model auth + containment
  before a run.
- **Incorporation:** an operator quickstart doc; always run `doctor` first;
  document the tier0-subscription vs API-tier auth split.

### L14. Specs must instruct INCREMENTAL implementation, not monolithic turns — rule (advisor behavior)
- **Observed:** a spec that asks a worker to implement a whole crate ("read the
  frozen grammar and the core/eval crates carefully, then get it right in one
  pass") pushes the driven model toward a single **giant generation turn** —
  read everything, then emit the entire crate at once. Those turns run many
  minutes and are hang-prone: theseus-syntax attempt 5's execute phase stalled on
  one ~10-min generation turn (proc alive, idle on the API, no output), bounded
  only by the 30-min watchdog. This is an **advisor behavioral bug**, not a
  harness bug — the spec caused the monolithic turn.
- **Impact:** slow, invisible progress; a single hung API turn wastes the whole
  attempt; larger blast radius per turn.
- **Incorporation:** advisor spec-authoring rule — instruct workers to implement
  **incrementally**: one module/file at a time, many small edits, interleaving
  reads and writes, never "emit the whole thing at once." Give an explicit build
  order when the crate has natural layers (e.g. spans/AST → lexer → parser →
  pretty → tests). Applied to the theseus-syntax spec (v3). Reflect in the
  `maestro-advisor` skill's rules and the advisor CLAUDE_MD guidance.

---

## Merge / parallelism

### L11. Disjoint-file parallel merges are conflict-free; shared files are the risk — note
- **Observed:** 5 concurrent card tasks merged cleanly (1 ff + 4 conflict-free
  3-way, 0 conflicts). The conflict risk is **shared files** (`Cargo.lock`, root
  `Cargo.toml`) touched by multiple concurrent tasks.
- **Impact:** parallel rounds are safe on disjoint files; shared-file edits can
  collide at merge.
- **Incorporation:** keep concurrent tasks on disjoint files; pre-wire
  shared-file changes (e.g. a workspace dep) into the base before fan-out, or
  accept a manual `Cargo.lock` regen. See also L3.

### L12. External quota surfaces cleanly as `failed` — note (no fix)
- **Observed:** the driven `claude` subscription 7-day usage limit surfaces as a
  clean `failed` with the rate-limit reason (`rateLimitType: seven_day`),
  re-runnable after reset — not a harness fault.
- **Incorporation:** none needed; document so operators don't mistake external
  quota for a bug.
