# ADR-008: Implementer Backends and Cost Routing

Status: DRAFT

## Context

Implementation work is performed by a pluggable **implementer backend** (the
`maestro-implementer` crate): given a resolved task (spec + worktree + house
rules + model), it makes the edits and returns a *descriptive* outcome; the
mechanical gate and verifier judge it (ADR-002), never the backend itself.

Cost is the routing pressure. Per-token API pricing is acceptable — and at work,
the only path — but prohibitive for sustained personal use. The design must let
the human route implementation to a cheaper, subscription, or local backend by
**config alone**, without touching orchestration, and without silently
substituting a backend the profile didn't ask for.

## Decision

### Backend selection is a profile decision, by role `kind`

The daemon picks a backend per task from the resolved role (ADR-007), never
hardcoded. A role is either a bare model string (⇒ the default `anthropic`
backend) or a table `{ model, kind, base_url?, turn_budget? }`. `kind` selects
the backend:

| kind | auth / cost | wire | status |
|---|---|---|---|
| `anthropic` (default) | Anthropic API key, per-token metering | Messages API + `write_file` tool loop; `base_url` overridable | **live** |
| `mock` | none | deterministic, driven by the spec | **live** (tests / dogfood) |
| `driven_cli` | subscription (Codex Pro / Claude Max), flat-rate | portable-pty driven session; `metered: false` (ADR-006) | **M3** |
| `openai_compat` | per-token or free (local) | OpenAI Chat Completions + tools; `base_url` **required** | **reserved here, implemented later** |

`openai_compat` is the personal-cost escape valve for local models
(Ollama/llama.cpp) and cheap OpenAI-compatible providers. This ADR reserves its
config seam (`kind` + `base_url` already carry it) so that adding it is a new
backend impl behind the trait, not an orchestration change. It is deferred, not
designed-away.

Example routing (ADR-007 profiles):

```toml
[profiles.work]
roles.tier0 = "claude-sonnet-4-6"                    # anthropic API — the only path at work

[profiles.personal]
roles.tier0 = { model = "qwen2.5-coder", kind = "openai_compat", base_url = "http://localhost:11434/v1" }
roles.tier1 = { model = "codex", kind = "driven_cli" }   # subscription, flat-rate (M3)
roles.tier2 = "claude-opus-4-8"                       # API for the hard cases
```

### Fail-loud on unavailable kinds

A role whose `kind` is not implemented on this build fails the delegation loud
(`model_unavailable`, ADR-003) with a message naming the kind — never a silent
fallback to the API backend. Consistent with ADR-007's no-silent-substitution
rule: the human edits config.

### Metering is recorded per session, proxy or not

The M1 backends call upstream directly (the daemon holds the API key in its own
environment; the credential-isolating proxy is a later milestone, ADR-004/006).
Metering therefore comes from the backend's **reported** usage: on completion the
daemon writes `tokens_in` / `tokens_out` / `turns` / `ended_at` / `exit_status`
back onto the task's `sessions` row from the `ImplementerOutcome`. Subscription
`driven_cli` sessions that cannot report token counts record `metered: false`
(ADR-006) and rely on turn budgets. This keeps cost visible per task from M1
onward and feeds the telemetry queries in M6.

## Consequences

- Cost routing is a profile edit: personal → cheap/local/subscription, work →
  API, per tier. No code change to switch.
- Telemetry (M6) can group cost and failure rates by `tier × backend`, so "is
  the local model failing verification more than Sonnet?" is a query.
- Adding a backend is a new `kind` + a trait impl; the delegate pipeline,
  gate, and verifier are unchanged.

## Tradeoffs accepted

- Unimplemented kinds fail loud rather than degrade — friction on a
  mis-set profile, deliberately (the ADR-007 invariant).
- Local / OpenAI-compatible models vary in quality; that is the human's routing
  choice, and the universal verification (ADR-002) grades their output the same
  as any other backend's — a weak backend simply blocks more.
- Direct-call metering trusts the backend's reported token counts until the
  daemon proxy (later milestone) makes it authoritative; accepted for M1–M2.
