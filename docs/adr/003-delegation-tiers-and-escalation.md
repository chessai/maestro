# ADR-003: Delegation Rubric, Tiers, and Escalation

Status: DRAFT

## Context

The advisor routes implementation work by complexity. Routing must be explicit (auditable in the journal), configurable per profile (model availability differs between machines), and self-correcting (misrouted tasks escalate rather than loop).

## Decision

### Tiers

Tier is chosen by the advisor at delegation time, scored against a rubric embedded in its role prompt, and passed as an explicit argument to `delegate`. The daemon maps tier → model via the active profile; it never infers.

| tier | intent | default model (personal) | default model (work) |
|---|---|---|---|
| 0 | mechanical: single-file edits, boilerplate, renames, running suites, **all shim work** | Sonnet | Sonnet |
| 1 | bounded implementation: clear spec, contained blast radius, iterate-until-green | Codex (driven CLI) | Sonnet (driven CLI) |
| 2 | architecturally sensitive: cross-crate refactors, ambiguous specs, propagating decisions | Opus | Opus 4.7 |

Rubric dimensions (advisor prompt, not daemon logic): spec ambiguity, blast radius, context required, verification difficulty. Any dimension scoring high pulls the task up a tier.

Model unavailability at delegation time is a hard `model_unavailable` failure. No silent substitution; the user edits config.

### TaskSpec (schema sketch)

```json
{
  "title": "string",
  "tier": 0,
  "base_ref": "string",
  "file_allowlist": ["paths/globs — enforced by mechanical gate"],
  "instructions": "string — goals and constraints, not steps for T1/T2",
  "acceptance_criteria": [
    { "id": "AC1", "check": "command or falsifiable statement", "kind": "command | invariant" }
  ],
  "check_commands": ["cargo test -p …"],
  "house_rules_ref": "path — injected verbatim",
  "budget": { "turns": 25, "tokens": null },
  "lifetime_budget": { "tokens": null, "wall_clock_minutes": null },
  "containment_min": 0
}
```

`containment_min` can only *raise* the floor: effective minimum = max(profile's per-tier floor, spec's `containment_min`). A spec can never request weaker containment than the profile grants its tier (ADR-004 owns the levels and downgrade policy).

Specs with adjective-only acceptance criteria are rejected by the daemon (`spec_rejected`): every criterion must be a command or a falsifiable invariant. This is the blog-post lesson made mechanical.

### Codex-bound strictness (asymmetric)

Codex-bound specs additionally require:
- non-empty `file_allowlist` (Claude-bound Tier 2 may use broad globs);
- **plan-echo**: the driven session's first turn must restate its plan; a one-shot Sonnet call checks plan-vs-spec; mismatch aborts before any edits (`plan_rejected`). This is an early-abort *efficiency* gate — it catches misunderstanding before budget is spent — not a containment control: the plan does not bind the edits, so the security control against out-of-scope work remains the post-hoc allowlist diff (ADR-002);
- hard turn budget (default 25) and per-tier token budget.

**Plan-echo is per-CLI-adapter.** The description above ("first turn restates its plan") applies to interactive/Codex-style driven sessions. The built `claude` adapter implements plan-echo as a two-phase run: `claude --print --permission-mode plan` produces the plan text, the daemon checks it against the spec, and on pass the driver re-invokes with `--permission-mode acceptEdits` to execute. This is the faithful adaptation for a one-shot `--print` CLI — the plan phase is structurally separate rather than embedded in the first turn of a conversation. See ADR-006 open-item #4 (per-provider driven-CLI adapters) for the broader picture.

### Escalation

A **verification failure** for escalation purposes is either `checks_failed` (mechanical gate: fresh checks fail or out-of-scope diff) or `verify_failed` (model verifier). They count identically — an implementer that "finished" but fails fresh checks has failed verification.

- Two verification failures at the same tier → automatic `escalated` event to the next tier up (0→1→2). Escalated implementer receives: original spec + all verifier reports + last failed diff. Never transcripts. (Exception: a `scope_violation` gate failure is terminal, not escalation fuel — a model that ignored the allowlist doesn't earn a bigger model.)
- One verification failure **after** reaching tier 2 → task goes `blocked` and returns to the advisor with the report. The advisor respecs, decomposes, or asks the human. No unattended loops past this point.
- `blocked` is a resting state, not terminal. The advisor resolves it via `close_task(task_id, outcome)` — `abandoned`, or `superseded` with the successor task's ID (the successor sets `parent_task` to the blocked task, preserving the trace chain). Closing records `failed(verification_failed)`.
- Escalation may raise the model but never lowers containment or budgets; budgets re-derive from the new tier.

### Task-lifetime ceilings

Per-attempt budgets (`budget`) re-derive from the tier on each escalation, so they bound a single attempt, not the whole ladder. A separate **task-lifetime ceiling** bounds the sum across all attempts and verifications:

- `lifetime_budget.tokens` — total metered tokens across every implementer and verifier session for the task; hitting it terminates with `budget_exhausted` regardless of which attempt is in flight.
- `lifetime_budget.wall_clock_minutes` — wall-clock from the first `spawned` event; hitting it terminates with `budget_exhausted` (payload `lifetime_wall_clock`). This bounds *latency*, which per-attempt turn budgets do not: without it a misrouted task can grind through the full 2+2+1 ladder and stay slow even though it terminates.

Defaults derive from config (ADR-007) — roughly the sum of per-tier attempt budgets up the ladder — so the ceiling only bites pathological cases, not normal escalation. Lifetime exhaustion is a hard stop that returns the task to the advisor exactly like a top-tier block. For subscription-authed driven CLIs where token metering is unavailable (`metered: false`, ADR-006), only `wall_clock_minutes` and the per-attempt turn budgets enforce the ceiling; its token component is advisory there.

### Concurrency

Per-machine cap 4 running sessions, per-advisor cap 2 (config-overridable). Delegations that would exceed a cap enter `queued` rather than failing, and spawn as slots free.

There is no per-workspace lock: every task gets its own worktree and branch off `base_ref`, so tasks with overlapping `file_allowlist`s run concurrently and independently. Divergence is resolved at merge time, not by serialising the work — overlapping merges that conflict send the later task to `blocked` (ADR-006), never auto-resolved. This keeps concurrency real rather than collapsing to base-ref serialisation.

## Consequences

- The journal's `tier`/`model`/`escalated` columns make rubric tuning empirical: after a few weeks, compare verify-failure and scope-violation rates per tier×model and adjust the rubric prompt.
- "Two failures then escalate, one failure at top then stop" bounds worst-case spend per task at up to five implementation attempts (2 + 2 + 1) across three tiers, plus their verifications.

## Tradeoffs accepted

- Advisor-chosen tiers mean a weak advisor misroutes; accepted because the advisor is the strongest configured model and escalation self-corrects, and telemetry exposes systematic misrouting.
- Rejecting adjective criteria adds friction to quick delegations; accepted deliberately — the friction is the feature.
