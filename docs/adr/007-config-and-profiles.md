# ADR-007: Configuration and Profiles

Status: DRAFT

## Context

Model availability, search backend reachability, containment capability, and advisor identity all vary per machine and per employer. Everything host- or policy-variable must be config, with fail-loud semantics when a profile promises what the host can't deliver.

## Decision

TOML at `$XDG_CONFIG_HOME/maestro/config.toml`, `[profiles.<name>]` tables overriding `[defaults]`. Active profile: `--profile` flag > `MAESTRO_PROFILE` env > `default_profile` key.

```toml
default_profile = "personal"

[defaults]
concurrency.machine_cap = 4
concurrency.advisor_cap = 2
watchdog_minutes = 10
search.backend = "anthropic"          # anthropic (server-side web_search, default) | searxng | none
shim.excerpt_cap_chars = 1500
shim.cache_ttl_hours = 24
downgrade_policy = "tighten"          # tighten | refuse
containment.backend = "auto"          # auto (podman>bwrap>seatbelt) | podman | bwrap | seatbelt | none
containment.network = "deny"          # deny | allow — L1+ default egress policy
tighten.allowlist_factor = 0.5        # applied on containment downgrade
tighten.turn_factor = 0.6
advisor.writable_paths = []           # in-repo globs made RW in the advisor mount; empty = repo fully read-only
lifetime.token_factor = 1.0           # task-lifetime token ceiling = factor × sum(per-tier attempt token budgets up the ladder)
lifetime.wall_clock_minutes = 30      # task-lifetime wall-clock ceiling

[profiles.personal]
advisor.model = "claude-fable-5"      # informational; advisor runs in Claude Code
advisor.context = "standard"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "codex", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 1, tier2 = 2 }
search.backend = "searxng"
search.endpoint = "https://searx.internal:8443"   # WireGuard-only is fine; unreachable => backend_unavailable

[profiles.work]
advisor.model = "claude-opus-4-8"
advisor.context = "1m"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "claude-sonnet-4-6", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 0, tier2 = 1 }   # possibly no nix
downgrade_policy = "tighten"
# search.backend not set => inherits the "anthropic" default (server-side web_search).
# Set search.backend = "none" to explicitly disable search, or "searxng" + endpoint for self-hosted.
```

### Rules

- **Fail-loud:** a tier whose configured model is unavailable at delegation → `model_unavailable`. An unset/unreachable search backend → `backend_unavailable` tool error. No silent substitution anywhere; the human edits config.
- **Advisor-context compaction flag:** `advisor.context = "standard"` switches the daemon to aggressive summarize-and-link inbox/report presentation (journal refs instead of inline payloads); `"1m"` inlines full verifier reports and traces. Derived behavior, not prompt hope.
- **Supervision/freedom inverse as invariant:** profiles may set `codex_tighten = true` (or it's implied by `advisor.context = "standard"` or any containment downgrade) to apply the tighten factors to all driven-CLI specs.
- Secrets: env vars or a 0600 `credentials.toml`; keychain/agenix backends behind a trait later. Keys are read by the daemon only (ADR-004/006).
- Journal records the profile name and resolved model per task, so telemetry never has to guess what config produced a row.

### Implementation status (v1)

The following config knobs are defined in the schema but **not yet wired or enforced** in v1; they are parsed and journaled but have no runtime effect:

- **`advisor.context` compaction presentation** (`standard`/`1m`): the flag is read and stored, but the daemon does not yet switch between aggressive-summarize and inline-full presentation modes for inbox/report delivery.
- **`tighten.*` factor enforcement** (`tighten.allowlist_factor`, `tighten.turn_factor`): values are computed and journaled on containment downgrade but are not yet applied to reduce budgets or allowlists at delegation time.
- **`shim.excerpt_cap_chars`**: parsed but not yet enforced as a hard cap on shim-returned excerpts.
- **`credentials.toml`**: the file path is reserved; in v1 secrets are read from environment variables only (the trait backend for keychain/agenix is not yet built).
- **`lifetime.token_factor`-derived default**: in v1 only an explicitly set `spec.lifetime_budget.tokens` is enforced; the `token_factor × sum(per-tier budgets)` derived default is not yet computed or applied.
- **`codex_tighten` and supervision-freedom multipliers**: the `codex_tighten` flag and the implied tightening from `advisor.context = "standard"` or containment downgrade are not yet applied to driven-CLI spec templates.

The rest of the config — profile selection, model routing, containment levels, concurrency caps, watchdog timeout, downgrade policy, search backend, and `advisor.writable_paths` — is wired and enforced in v1.

## Consequences

- Moving between home and work is `MAESTRO_PROFILE=work`; every behavioral difference is diffable TOML.
- The `advisor.model` key is informational (Claude Code owns the actual model choice), but journaling it keeps routing telemetry attributable to advisor capability.

## Tradeoffs accepted

- Config sprawl risk as knobs accumulate; mitigated by everything having a `[defaults]` value and `maestro doctor` printing the fully resolved profile.
