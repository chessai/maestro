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

### L2. The persistent journal has no schema migration — open
- **Observed:** `~/.local/share/maestro/journal.db` created by an older binary
  lacked the `repo_path` column; `delegate` failed with `sqlite error: table
  tasks has no column named repo_path`. The `/goal` skill sidesteps this by
  using an ephemeral `/tmp` XDG data dir per run (fresh schema each time).
- **Impact:** a persistent journal breaks across binary upgrades; silent unless
  you know to wipe it.
- **Incorporation:** add journal schema migrations (or a version check that
  auto-migrates / rebuilds with a loud notice). Until then, document that a
  schema bump requires resetting the journal.

### L3. Don't move `base_ref` under an in-flight task — open (rule)
- **Observed:** merging a completed task into `base_ref` while a *sibling* task
  still runs against it makes the gate diff the sibling's worktree against the
  **advanced tip**; the just-merged files appear as **deletions** outside the
  sibling's `file_allowlist` → `scope_violation` → the sibling fails despite
  doing nothing wrong. Hit live: theseus-eval merged into `theseus-v2` while
  theseus-syntax ran against it → syntax failed on `crates/theseus-eval/*`
  deletions.
- **Impact:** spurious task failures + wasted worker runs during parallel rounds.
- **Incorporation:** compute the **scope-check diff against the concrete commit
  the worktree was cut from** (pin `base_ref` at spawn for the scope check); the
  *merge target* can still be the live tip. Only the scope diff must use the
  pinned base. **Rule until fixed:** finish all tasks branched from a base before
  merging any of them; exploit natural dependencies (a dependent task can't
  start until the base settles) to keep merges safe.

---

## Driven worker / CLI adapter

### L4. Turn cap is execute-phase only; the plan phase is uncapped — note
- **Observed:** the driven `claude` adapter enforces the turn cap only in the
  execute phase; the plan phase runs uncapped (a plan is "a single short turn").
- **Impact:** a long or looping plan phase is unbounded (see also L1).
- **Incorporation:** consider a plan-phase wall-clock/turn ceiling, or fold it
  into the per-attempt deadline in L1.

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

---

## Gate / verification

### L6. The gate must RUN the deliverable's tests, not just build — open (rule)
- **Observed:** a light gate (build + clippy of one crate) never compiled the
  test files; 3 of 5 "verified" cards shipped with test-file defects that only a
  test-running gate catches.
- **Impact:** false "verified" — defects slip past a build-only gate.
- **Incorporation:** make "gate runs the acceptance test commands" a hard
  default; warn on a spec whose `check_commands` only build (no test). The `/goal`
  per-card gate must compile+run the card's tests.

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

### L10. Profile/auth/setup notes for operators — docs
- **Observed / how it works:** driven tier0 uses the local `claude` CLI's own
  subscription auth (no API key needed for it); verifier + escalation tiers
  (opus/sonnet/haiku) need `ANTHROPIC_API_KEY` in the daemon env. Config lives at
  `$XDG_CONFIG_HOME/maestro/config.toml`; `base_ref` is a **per-delegation spec
  field**, not a profile field. `doctor` validates model auth + containment
  before a run.
- **Incorporation:** an operator quickstart doc; always run `doctor` first;
  document the tier0-subscription vs API-tier auth split.

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
