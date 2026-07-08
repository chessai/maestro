# ADR-002: Verification Contract

Status: DRAFT
Load-bearing: yes — the report schema is frozen alongside the failure taxonomy.

## Context

The design's core trust mechanism is that no agent grades its own work. Verification must be cheap enough to run universally, strong enough to catch scope creep and Goodharted acceptance criteria, and structured enough to feed telemetry and escalation.

## Decision

### Universal verification, gated

Every task — including Tier 0 — is verified. Two stages:

1. **Mechanical gate (no model).** Orchestrator-executed, always first:
   - out-of-scope diff check: `git diff <base_ref>` restricted to the spec's file allowlist; any excess is a `scope_violation` failure regardless of test results;
   - the spec's check commands (build, test, lint) run fresh in a clean verifier sandbox — never trusting the implementer's claimed results.
   Gate failure short-circuits before any verifier tokens are spent (`checks_failed` event).

2. **Model verifier.** Fresh-context session, never the implementer. Input: the TaskSpec's acceptance criteria, the diff, and the mechanical gate's command outputs. Explicitly excluded: the implementer transcript. The verifier may run additional commands, but in a **throwaway checkout** of the implementation branch that is discarded after the report — its mutations can never reach the task branch, so "read-only" is structural, not requested. The verifier's sole output channel is the report: **verifiers report; they never mutate.**

   **v1:** the verifier judges from the provided diff + mechanical-gate output; running additional commands in a throwaway checkout is designed but not yet built. The throwaway-checkout description above is the target.

### Verifier selection

Constraint hierarchy, best available wins, recorded per report:

| independence | condition |
|---|---|
| `cross_provider` | implementer and verifier from different providers (Sonnet verifies Codex; Codex/Sonnet verifies Opus) |
| `cross_model` | same provider, different model (work profile: Sonnet verifies Opus) |
| `fresh_context_only` | same model, fresh context — last resort, allowed but flagged |

"Different from implementer" is the hard constraint; "expensive" is not — Tier 0 verifiers default to the cheapest configured model.

### Report schema (frozen)

```json
{
  "verdict": "pass | fail",
  "findings": [
    {
      "severity": "blocker | concern | note",
      "criterion_id": "string | null",
      "evidence": "string (verbatim command output / diff hunk reference)"
    }
  ],
  "out_of_scope_diff": false,
  "commands_run": [
    { "cmd": "string", "exit": 0, "output_digest": "string" }
  ]
}
```

`verdict: fail` requires at least one `blocker` finding. `concern`/`note` findings on a pass are surfaced to the advisor but do not block.

### Verdict handling

- Pass → `verify_passed`; branch presented for merge (ADR-006 approval modes).
- Fail → `verify_failed`; retried per escalation policy (ADR-003). Escalated implementers receive spec + all prior verifier reports + the failed diff — never failed-session transcripts.

### Verifier availability and failure

Verification is never silently skipped, and a broken verifier never masquerades as a passing task:

- No model can satisfy even `fresh_context_only` (e.g. the only configured model is the implementer's and no fresh session is possible) → the task fails `model_unavailable`, not `verify_passed`.
- The verifier session crashes, is killed, or returns a schema-invalid report before producing a verdict → retried once with a fresh session; a second failure is `internal_error` on the task.
- The verifier exhausts its own budget mid-report (no verdict) → treated as a crash (retry once), never as a `verify_failed` charged to the implementer — verifier cost is not the implementer's fault.

## Consequences

- Every task pays a second sandbox run; the mechanical gate keeps Tier 0's marginal cost near zero when the common failure modes (tests fail, scope violation) fire.
- Report `findings` keyed by `criterion_id` make "which acceptance criteria fail most often" a telemetry query, feeding spec-template improvement.
- Report-only verifiers keep the accountability line crisp: implementers are the only writers, the mechanical gate is the only enforcer.

## Tradeoffs accepted

- A verifier cannot add a failing regression test to pin a bug it found; it can only describe it. Accepted — the advisor can spawn a follow-up task to add the test.
- `fresh_context_only` independence is weaker than we'd like on single-provider profiles; accepted and measured (the `independence` column exists so telemetry can say whether it matters).
