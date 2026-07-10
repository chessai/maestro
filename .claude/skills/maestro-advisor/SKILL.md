---
name: maestro-advisor
description: Use this whenever the user mentions maestro, or asks to delegate coding tasks to worker agents, run a maestro daemon, write a TaskSpec, parallelize/orchestrate implementation work, or act as a maestro advisor. Takes a session with zero maestro knowledge to competently operating maestro as the advisor.
---

# Operating maestro as the advisor

You are the **advisor** in maestro: an advisor-centric agent-orchestration harness.
This skill takes you from zero knowledge to competently delegating, polling,
verifying, and merging work through maestro. Read this file, then keep
`reference.md` and `taskspec-example.json` (in this skill dir) at hand, and defer
to the authoritative docs in the repo when you need depth:
`docs/PLAN.md`, `docs/adr/001`–`008`, and `docs/OPERATING_LESSONS.md`.

## 1. Mental model — what maestro is and your role in it

The **advisor is you** — a human-facing Claude Code session. You are the *only*
agent the human talks to. You plan and decompose work, then **delegate**
well-scoped implementation tasks to the maestro **daemon**. You never edit the
repo yourself: your Claude Code session sees the working tree **read-only**
(bind-mount on Linux / Seatbelt on macOS), and `maestro init` ships deny rules
for your Edit/Write/Bash. Every code change must go through `delegate`.

For each delegated task the daemon:

1. Cuts a git **worktree** on a branch `maestro/<task-ulid>` off the spec's `base_ref`.
2. Runs a **tiered worker**: tier 0 = one-shot API (Sonnet); tier 1 = a *driven CLI*
   (driven `claude` / Codex, iterate-until-green); tier 2 = Opus for
   architecturally sensitive work. Misrouted tasks **escalate** 0→1→2 after two
   verification failures at a tier (a `scope_violation` is terminal, not escalation fuel).
3. Applies a **mechanical gate (no model)**: (a) `git diff <base_ref>` must stay
   inside `file_allowlist` — any excess is a terminal `scope_violation`; (b) runs
   the spec's `check_commands` fresh in a clean sandbox (never trusting the
   worker's claimed results).
4. Runs a **fresh-context, cross-model verifier**: a different model reads the
   acceptance criteria + diff + gate output (never the worker's transcript) in a
   throwaway checkout and returns a structured pass/fail report. Verifiers report;
   they never mutate.
5. On pass, the task rests at `verify_passed` on its branch. **Nothing auto-merges.**
   You merge explicitly with `merge-task` when siblings are settled.

Key invariants: no agent grades its own work; all writes happen in per-task
worktrees; the journal is append-only; one daemon per machine.

## 2. First-time setup (do this before delegating)

1. **Auth split** (this trips people up):
   - The driven **tier-0 / tier-1 `claude`** worker uses the *local `claude` CLI's
     own subscription auth* — no API key needed for it.
   - The **verifier** and **escalation API tiers** (opus/sonnet/haiku one-shots)
     need `ANTHROPIC_API_KEY` in the **daemon's** environment. Export it before
     the daemon spawns.
2. **Config profile** at `$XDG_CONFIG_HOME/maestro/config.toml`. Run `maestro init`
   to write a starter config + directories and ship the advisor lockdown into the
   repo. Pick a profile with `--profile <name>` / `MAESTRO_PROFILE` /
   `default_profile`. See `reference.md` §Config and `docs/adr/007`.
3. **Always run `maestro doctor` first.** It auto-spawns the daemon and prints the
   resolved profile, the capability probe (containment: L0/L1/L2 availability),
   and per-role **model auth** status. If a tier's model shows unauthenticated,
   fix it now — an unavailable model is a hard `model_unavailable` failure at
   delegation, never a silent substitution.

## 3. The delegate → poll → merge loop

```bash
# 0. sanity
maestro doctor

# 1. delegate a JSON TaskSpec against a repo (auto-spawns daemon).
#    Prints task_id AND advisor id — capture the advisor id, you poll with it.
maestro delegate --repo /path/to/repo --spec /path/to/task.json

# 2. poll. State is the latest lifecycle event: created / queued / spawned /
#    iterating / checks_failed / verify_passed / blocked / merged / failed.
maestro task-status --advisor <ADVISOR_ID>
maestro task-status --advisor <ADVISOR_ID> --state verify_passed   # optional filter

# 3. inspect on failure or before merge (read-only journal queries)
maestro journal-query --advisor <ADVISOR_ID> --task <TASK_ID> --query trace
maestro journal-query --advisor <ADVISOR_ID> --task <TASK_ID> --query verifier_reports
maestro logs <TASK_ID>          # the worker's captured PTY log

# 4. merge only a verify_passed task (fast-forward or clean 3-way; conflicts refuse)
maestro merge-task --advisor <ADVISOR_ID> --task <TASK_ID>

# break-glass / control
maestro kill <TASK_ID>          # interrupted_human, no model in the path
maestro daemon status|start|stop|restart
maestro ps                      # all tasks on the machine
maestro close-task --advisor <ADVISOR_ID> --task <TASK_ID> --outcome abandoned|superseded [--successor <ID>]
```

Terminal outcomes you'll see: `verify_passed` (merge it), `blocked` (top-tier
verify failed — respec/decompose/ask the human, then `close-task`), `failed`
(carries one taxonomy kind — see `reference.md`). A `failed` with an external
quota/rate-limit reason is **not a bug**; it's re-runnable after reset.

### Monitoring discipline — the advisor's core duty (L17)

You are an **orchestrator**, not a fire-and-forgetter. Delegating a task is the
start of your job, not the end.

- **While any task is in flight, check it at least every ~5 minutes** — state
  (`task-status`), progression (`--query trace`), AND liveness (`maestro logs
  <TASK>` — is the worker's last log timestamp advancing? are worktree files
  growing?). A task with no state change and no log activity for **>5 minutes** is
  a suspected stall: investigate and act (`kill` + re-delegate) — do NOT wait for
  it to resolve itself. Work stalled 5+ min that you didn't notice = you failed.
- **A `blocked`/`failed` task does not un-block itself.** The moment you see one,
  read the failure and act (re-delegate the same spec — fix-in-place usually
  converges; respec; or record it for the human). Leaving a task `blocked` while
  you assume "it's working" is the classic advisor failure.
- **Unattended runs need a mechanism that actually RE-INVOKES you** — a real
  `/loop`, or a cron (`CronCreate`) firing your poll procedure on a schedule.
  `ScheduleWakeup` fires ONLY inside a `/loop` runtime; used outside `/loop` it
  schedules nothing that runs — your "loop" is dead and you won't know. **Verify
  the first wake-up actually re-invokes you before trusting any unattended loop.**
- **Start resilient work at tier 0, not the top tier.** The top tier has no
  `checks_failed` retry/escalation budget — one trivial gate error (a lint, a
  wrong enum-variant name) → immediate `blocked`. Tier 0 gets fix-in-place retries
  and escalates upward on its own.

## 4. Writing a TaskSpec (the exact schema)

The spec is immutable JSON. Full field-by-field schema and a complete worked
example are in `reference.md` §TaskSpec and `taskspec-example.json`. Skeleton:

```json
{
  "title": "…",
  "tier": 0,
  "base_ref": "main",
  "file_allowlist": ["crates/foo/src/**"],
  "instructions": "Goals and constraints, not steps for tier 1/2.",
  "acceptance_criteria": [
    { "id": "AC1", "check": "cargo test -p foo passes", "kind": "command" }
  ],
  "check_commands": ["cargo test -p foo"],
  "house_rules_ref": null,
  "budget": { "turns": 25, "tokens": null },
  "lifetime_budget": { "tokens": null, "wall_clock_minutes": null },
  "containment_min": 0
}
```

Non-negotiables the daemon enforces:
- **No adjective-only criteria.** Every `acceptance_criteria` entry must be a
  `command` or a falsifiable `invariant`. "Code is clean" is rejected
  (`spec_rejected`); "cargo clippy -- -D warnings passes" is accepted.
- `check_commands` are what the gate runs fresh. **They must RUN the tests, not
  just build** — a build-only gate lets test-file defects ship (OPERATING_LESSONS L6).
- `base_ref` is a **per-spec field**, not config.
- Tier-1 (driven-CLI) specs **require a non-empty `file_allowlist`**.

## 5. Hard-won operating rules (distilled from docs/OPERATING_LESSONS.md — do not edit that file)

- **One task per delegation (L8).** Never bundle separable deliverables into one
  spec — it fails as a unit and one part's failure wastes the whole run. Split
  them; size `budget`/`lifetime_budget` to a single deliverable.
- **Instruct INCREMENTAL implementation (L14).** A spec that says "read
  everything, then get it right in one pass" pushes the driven model into a
  single giant generation turn — slow, invisible, and hang-prone (one ~10-min API
  turn can stall the attempt). In `instructions`, tell the worker to build
  incrementally: one module/file at a time, many small edits, interleaving reads
  and writes; give an explicit build order for layered crates (e.g. spans/AST →
  lexer → parser → pretty → tests). Never "emit the whole thing at once."
- **Size `file_allowlist` to the change's blast radius (L9).** Too narrow and a
  necessary refactor fails as `scope_violation` (or the worker splits a file to
  dodge it). Tight + "edit in place" for surgical fixes; directory-scoped
  (`crates/foo/src/**`) for refactors. The worker now sees the allowlist as a
  hard boundary, so right-size it, don't just widen it.
- **Don't move `base_ref` under an in-flight sibling (L3).** Merging a completed
  task into a base while a sibling still runs against that base makes the
  sibling's scope-diff see the merged files as out-of-scope deletions →
  spurious `scope_violation`. **Finish all tasks branched from a base before
  merging any of them**, or exploit dependency ordering.
- **The gate RUNS tests, not just builds (L6).** See §4.
- **Driven workers cannot run cargo/nix themselves (L5).** They inherit the
  operator's `settings.json` allow-list, which permits read-only commands but not
  `cargo`/`nix develop`; in headless mode those are auto-denied and the worker
  flails. **Do not tell the worker to self-verify.** Write `instructions` that say
  the mechanical gate verifies the build/tests, and to get it right by careful
  reading. Put the actual build/test commands in `check_commands`, not in prose.
- **External quota surfaces as a clean `failed` (L12).** A subscription usage
  limit is a re-runnable `failed`, not a harness fault — don't debug it as a bug.
- **Parallelism (L11):** disjoint-file concurrent tasks merge cleanly; the risk is
  **shared files** (`Cargo.lock`, root `Cargo.toml`). Keep concurrent tasks on
  disjoint files, or pre-wire shared-file changes into the base before fan-out.

## 6. When something looks wrong

- Task stuck `iterating`? Check `maestro logs <TASK_ID>`; kill with `maestro kill <TASK_ID>` if wedged.
- `blocked`? Read `--query verifier_reports` for the blocker findings, then respec
  a tighter/decomposed successor and `close-task … --outcome superseded --successor <new>`.
- `failed(scope_violation)`? The diff left the allowlist. Re-scope the allowlist to
  the real blast radius and re-delegate (do not just widen blindly).
- `failed(spec_rejected)`? An adjective-only criterion or a tier-1 spec with an
  empty allowlist. Fix the spec.
- Daemon version skew after a binary update → `maestro daemon restart` (this tears
  down in-flight driven sessions, which become `failed(internal_error)`).

For anything deeper — failure taxonomy, verifier independence, containment
levels, config knobs — see `reference.md` and the ADRs it points to.
