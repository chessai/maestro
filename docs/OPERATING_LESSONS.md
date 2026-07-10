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

### L16. Acceptance must be INDEPENDENT of the worker's self-written tests — rule (advisor behavior)
- **Observed:** a driven worker "implemented" theseus-syntax and its own
  `tests/conformance.rs` passed — but the implementation was **missing ~half the
  frozen grammar** (only the `= expr` iso body; no clause bodies, patterns,
  `where`, `prim`, `$label`, tuple types, or literals). Its self-written tests
  were weak enough to pass the incomplete parser, so a build-only gate — or a gate
  running the *worker's own* tests — would have green-lit it. Only re-checking
  against the full conformance suite with strong assertions (parse **every**
  fixture except the one negative case, round-trip **every** program, precedence)
  exposed it. A subagent with cargo access then completed it to 24 real tests.
- **Impact:** a worker can define its own (weak) success criteria and ship
  incomplete work that passes the gate — a false "verified".
- **Incorporation:** advisor spec-authoring rule — **acceptance must not depend on
  tests the worker authors for itself.** Either (a) have `check_commands` run an
  **advisor-controlled / shared acceptance suite** (e.g. the versioned conformance
  fixtures with exhaustive assertions the worker cannot weaken), or (b) ship the
  test harness in the spec / repo, not "write your own tests." State the DoD as
  concrete, independently-checkable coverage ("**every** `conformance/**` fixture
  parses except 0007"), never "add tests." Complements L6 (gate must RUN tests):
  L6 says run tests; L16 says they must be tests the worker can't game.

### L15. Fix-in-place retry on `checks_failed` — preserve near-complete work — FIXED
- **Observed:** when an attempt failed at the mechanical GATE (`checks_failed` —
  a build/clippy/test `check_command` returned non-zero), the retry cut a FRESH
  worktree off `base_ref` and re-implemented the whole task from scratch,
  discarding the worker's near-complete code. Hit live: a theseus-syntax task
  whose parser compiled down to a single trivial borrow error (`use of moved
  value`) needed 3+ full ~15-min re-implementations, each throwing away working
  code and rewriting everything only to hit a *different* trivial compile error.
- **Impact:** for a near-complete implementation, "rewrite everything" is the
  wrong retry strategy — enormous wasted wall-clock/turns, and the retry can
  regress from an almost-passing state to a differently-broken one.
- **Incorporation:** LANDED. When an attempt's terminal outcome is a
  `checks_failed`, the pipeline now carries a `CheckFailure { command,
  output_digest }` into the NEXT same-tier attempt (new field on
  `AttemptOutcome::VerificationFailed { check_failure }`; set only on the gate's
  `ChecksFailed` arm, `None` for a model `verify_failed`). On that next attempt:
  1. **Reuse the same worktree** — `run_attempt` / `run_driven_attempt` skip
     `worktree::create` (which resets to `base_ref`) and reuse the existing
     worktree with the worker's edits intact, guarded by `worktree::reuse`
     (`is_live_worktree`: the dir exists AND `git rev-parse
     --is-inside-work-tree` succeeds; a missing/pruned worktree falls back to a
     fresh cut, so reuse is always safe).
  2. **Inject the failing check into the worker's context** — `check_fix_preamble`
     prepends a "Fix the failing check — do NOT rewrite" block (the failing
     command + its captured output + "your edits are ALREADY PRESENT, make the
     SMALLEST change") to the one-shot backend's `house_rules` and to the driven
     CLI's prompt, so the worker fixes the specific error instead of rewriting.
  3. **Escalation ladder preserved** — the tier-bump logic is unchanged; only the
     worktree-reset behavior changes. On escalation the carryover is DROPPED
     (`pending_check_fix = None`): a bigger model starts on a fresh worktree with
     the prior verifier reports + last diff (ADR-003). Fix-in-place is strictly a
     SAME-TIER retry optimization.
- **Failure-kind boundary (chosen decision table).** Reuse the worktree ONLY when
  the worker produced a coherent, near-complete diff that merely tripped a check;
  everything else may have junk/out-of-bounds/partial edits, so a fresh worktree
  is the safe default:

  | terminal outcome of the attempt | next attempt's worktree | why |
  |---|---|---|
  | `checks_failed` (build/clippy/test check returned non-zero) | **REUSE** + inject failing check | edits are near-complete; a targeted fix beats a rewrite |
  | `verify_failed` (model verifier rejected the approach) | fresh cut | the verifier rejected the *approach*, not a mechanical slip; re-implement with the report as context |
  | `scope_violation` / `TightenedScopeExceeded` | fresh cut (terminal anyway) | worker ignored the allowlist → out-of-bounds edits; doesn't earn reuse (ADR-003: terminal, not fuel) |
  | `session_wedged` / `turn_budget_exceeded` / `interrupted_*` (kill) / `plan_rejected` | fresh cut (terminal anyway) | worker didn't finish cleanly → possibly partial/junk edits |
  | escalation to a higher tier | fresh cut (carryover dropped) | a bigger model re-approaches from base; reuse is same-tier only |

- **Tests:** `worktree::is_live_worktree_true_for_live_false_for_absent_or_removed`
  (the reuse guard), `delegate::check_fix_preamble_names_command_output_and_says_do_not_rewrite`
  (the injected context), and the end-to-end
  `tests/fix_in_place.rs::m_l15_checks_failed_retry_reuses_worktree_and_fixes_in_place`:
  a check command that creates an in-allowlist marker + exits non-zero on its
  first run and passes once the marker exists — the marker only survives into
  attempt 2 if the worktree was reused, so `verify_passed` after exactly ONE
  `checks_failed` with NO `escalated` proves fix-in-place. The existing
  `verify_escalation` test still proves the `verify_failed` fresh-restart ladder
  is unchanged.
- **Follow-ups (not in this pass):** (a) the reused worktree's stale in-allowlist
  artifacts from attempt 1 persist (e.g. a marker/scratch file the worker wrote)
  — benign here (the gate re-diffs against the pinned base each attempt), but a
  worker that writes a broken file it later abandons keeps that file; consider a
  `git checkout -- .`-to-clean-untracked-but-keep-tracked policy if this bites.
  (b) fix-in-place currently reuses for BOTH same-tier `checks_failed` retries;
  it is intentionally NOT applied across an escalation — if we later want the
  escalated model to *see* the smaller model's near-complete worktree (rather
  than re-cut), that's a deliberate ADR-003 amendment, not a silent change.

### L15b. Fix-in-place durability must NOT ride on the uncommitted working tree — FIXED
- **Observed (live loss-loop, task `01KX5G3T90TPSJTVMJ6S8CDJDG`, repo theseus,
  crate `theseus-check`):** L15 reused the worktree on a `checks_failed` but the
  worker's edits were **never committed** — `commit_paths` ran ONLY on the
  checks-PASSED path. The near-complete implementation (`testkit.rs` + every
  module, one `clippy::manual_strip` fix from green) lived purely in the
  uncommitted working tree. On the fix-in-place retry the driven worker's own
  `git reset` (reflog: `reset: moving to HEAD`; HEAD still at base `b603cdb`)
  wiped the working tree back to base — `git log --all` had NO commit with the
  implementation. Unrecoverable: every `checks_failed` discarded the progress and
  re-implemented from scratch — the exact loss L15 meant to prevent.
- **Root cause:** the wipe was NOT maestro code (the only daemon `git reset` is the
  `--mixed` index reset inside `commit_paths`, which never touches the working
  tree). It was the **worker process** resetting a worktree whose edits maestro had
  left uncommitted. Durability that depends on a fragile uncommitted working tree
  loses to anything that resets it.
- **Incorporation:** LANDED. On a `checks_failed`, `run_gate_and_verify` now
  COMMITS the gate's in-allowlist `changed` set (captured before the check
  commands ran, so no `target/`/artifacts) to the task branch via the SAME
  `worktree::commit_paths` the pass path uses — BEFORE the attempt returns. The
  same-tier retry then resumes from a REAL commit: a later `git reset --hard HEAD`
  RESTORES the edits instead of discarding them. `GateOutcome::ChecksFailed` gained
  a `changed: Vec<String>` field to carry that set. Escalation is unchanged —
  `run_attempt` still cuts a FRESH worktree off `base_ref` (dropping the carryover
  and force-deleting the task branch), so a fix-commit NEVER survives a tier bump;
  only a same-tier `checks_failed` retry resumes from it (the decision table
  holds). The best-effort commit logs-not-fails on error.
- **Test:** `tests/fix_in_place_durability.rs::m_l15_checks_failed_edits_survive_a_worktree_reset_on_the_retry`
  reproduces the real round-trip — attempt writes a file, trips a check
  (`checks_failed`), and the reused retry does `git reset --hard HEAD` (the live
  worker's cleanup) before checking the file is present. It FAILS against the
  pre-fix code (`blocked`, the reset wiped the uncommitted edits) and PASSES after
  (`verify_passed` after exactly one `checks_failed`, no escalation, and
  `maestro/<task>:src/impl.rs` resolves — committed on the branch). Complements
  L15's `fix_in_place.rs`, which only proved working-tree reuse under a benign mock
  that never resets, and so missed this.

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
