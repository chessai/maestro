# maestro advisor — reference

Deep reference for the maestro-advisor skill. Grounded in the maestro source
(`crates/maestro-journal/src/spec.rs`, `crates/maestro-cli/src/main.rs`) and the
ADRs. When in doubt, the ADRs (`docs/adr/`) are authoritative.

## TaskSpec — exact schema

Source of truth: `crates/maestro-journal/src/spec.rs` (ADR-003). The spec is
stored verbatim as immutable JSON in `tasks.spec`. Fields:

| field | type | notes |
|---|---|---|
| `title` | string, required | human label, shown in `ps`/`task-status`. |
| `tier` | int 0\|1\|2, required | you choose at delegation; daemon maps tier→model via the profile, never infers. 0 = mechanical/single-file/boilerplate; 1 = bounded impl, iterate-until-green (driven CLI); 2 = architecturally sensitive / cross-crate. |
| `base_ref` | string, required | git ref the worktree branches from (e.g. `"main"`). **Per-spec, not config.** |
| `file_allowlist` | array of path globs, default `[]` | enforced by the mechanical gate (`git diff base_ref` must stay inside it). Globs like `crates/foo/src/**` are supported. **Required non-empty for tier-1 driven-CLI specs.** |
| `instructions` | string, required | goals and constraints — **not step-by-step** for tier 1/2. Do NOT instruct the worker to run build/test itself (see L5). |
| `acceptance_criteria` | array, required | each `{ "id": "AC1", "check": "...", "kind": "command" \| "invariant" }`. **Adjective-only criteria are rejected** (`spec_rejected`) — must be a runnable command or a falsifiable invariant. |
| `check_commands` | array of strings, default `[]` | e.g. `"cargo test -p foo"`. The gate runs these fresh in a clean sandbox. **Must run tests, not just build (L6).** |
| `house_rules_ref` | string \| null, default null | path to a house-rules file, injected verbatim into the worker prompt. |
| `budget` | `{ "turns": int\|null, "tokens": int\|null }` | **per-attempt** budget, re-derived from the tier on each escalation. Default `turns` 25 for driven CLIs. |
| `lifetime_budget` | `{ "tokens": int\|null, "wall_clock_minutes": int\|null }` | **task-lifetime** ceiling across all attempts + verifications. `wall_clock_minutes` from the first `spawned` event; hitting it → `budget_exhausted` (payload `lifetime_wall_clock`). |
| `containment_min` | int (u8), default 0 | can only **raise** the floor: effective min = max(profile per-tier floor, this). Never weakens containment. |

`AcceptanceCriterion.kind`:
- `command` — `check` is a shell command whose exit status is the verdict.
- `invariant` — `check` is a falsifiable statement the verifier evaluates against the diff.

Notes on budgets: for subscription-authed driven CLIs (`metered: false`) token
metering is unavailable, so only `wall_clock_minutes` + per-attempt turn budgets
truly enforce the lifetime ceiling; the token component is advisory there. Cold
self-builds are heavy — for maestro-on-maestro give `wall_clock_minutes` room
(60+) and keep tasks small (PLAN.md §8.9).

## CLI reference (from `crates/maestro-cli/src/main.rs`)

Global flag: `--profile <name>` (highest precedence over `MAESTRO_PROFILE` and
`default_profile`). Most subcommands auto-spawn the daemon.

| command | flags | purpose |
|---|---|---|
| `maestro doctor` | — | resolved profile + capability probe + per-role model auth. Run first. |
| `maestro init` | — | write starter config + dirs; ship advisor lockdown (`.claude/settings.json` deny rules, `.mcp.json`, `CLAUDE.md`) into the cwd repo. Idempotent. |
| `maestro delegate` | `--repo <path> --spec <spec.json>` | register an advisor + delegate the spec. Prints `task_id` and `advisor`. |
| `maestro task-status` | `--advisor <id> [--state <s>]` | table of the advisor's tasks (task_id, tier, state, title); optional derived-state filter. |
| `maestro journal-query` | `--advisor <id> --task <id> --query <name>` | read-only named query, pretty-printed JSON. |
| `maestro merge-task` | `--advisor <id> --task <id>` | merge a `verify_passed` task's branch into its base (ff or clean 3-way; genuine conflict refuses without mutating any ref). |
| `maestro close-task` | `--advisor <id> --task <id> --outcome abandoned\|superseded [--successor <id>]` | resolve a `blocked` task; records `failed(verification_failed)`. `superseded` links the successor into the trace chain. |
| `maestro kill` | `<task-id>` | break-glass kill (`interrupted_human`), no model in the path. (`--advisor`/`--all` fan-out parsed but not implemented in M3.) |
| `maestro logs` | `<task-id>` | print the task's latest driven session's captured PTY log. (`--follow` not implemented in M3.) |
| `maestro ps` | — | all tasks on the machine (all advisors). |
| `maestro daemon` | `status\|start\|stop\|restart` | daemon lifecycle. `restart` for protocol version skew (tears down in-flight driven sessions). |
| `maestro advise` | `[--exec <cmd>]` | launch the advisor's Claude Code session inside the read-only-repo mount. |

### Named journal queries (daemon `handle_journal_query`)

- `trace` — the task's full ordered event timeline (`{task_id}`). Use to see
  `spawned → iterating → checks_failed → escalated → verify_passed`, etc.
- `verifier_reports` — the report chain for a task (attempt, independence,
  report JSON). Read this on `blocked`/`verify_failed` to get the blocker findings.
- `routing_report` — telemetry aggregate grouped by (tier, model,
  containment_level); no task param needed (pass any task, it's ignored).

## Task lifecycle states (derived = latest event kind, ADR-001)

`created` → `queued` (cap saturation) → `spawned` → `iterating` →
`checks_failed` (gate) / `verify_failed` → `verify_passed` (rest, mergeable) →
`merged`. Off-ramps: `blocked` (top-tier verify fail; resting, resolve via
`close-task`), `failed` (terminal, one taxonomy kind), `interrupted`.

Escalation (ADR-003): two verification failures at a tier → escalate 0→1→2 (the
escalated worker gets spec + all verifier reports + last failed diff, never
transcripts). One failure after reaching tier 2 → `blocked`. A `scope_violation`
is terminal — it does not earn a bigger model.

## Failure taxonomy (frozen, ADR-001) — what a `failed` means

| kind | meaning / your response |
|---|---|
| `spec_rejected` | schema/validation failed pre-spawn (adjective-only criterion; tier-1 empty allowlist). Fix the spec. |
| `plan_rejected` | driven plan-echo failed the spec check; no edits made. Clarify instructions. |
| `verification_failed` | a blocked task you closed after top-tier verify failure. |
| `scope_violation` | diff left the allowlist. Re-scope to the real blast radius, re-delegate. |
| `budget_exhausted` | turn/token/wall-clock ceiling hit (payload distinguishes; `lifetime_wall_clock` for wall clock). Raise budget or shrink task. |
| `model_unavailable` | configured tier model missing/unauthenticated at delegate time. Fix auth/config (run `doctor`). |
| `sandbox_killed` | reserved, not emitted in v1. |
| `session_wedged` | PTY silent past the watchdog (default 10 min). |
| `internal_error` | maestro bug / env fault (incl. `reason: daemon_restart`). |
| `interrupted_human` | `maestro kill`. |
| `interrupted_advisor` | advisor `kill_task`. |

External quota (subscription usage limit) surfaces as a clean `failed` with a
rate-limit reason — re-runnable after reset, not a harness bug (L12). Shim
errors (`backend_unavailable`) are tool-result errors, not task failures.

## Verification contract (ADR-002)

1. **Mechanical gate (no model):** scope-diff check against `file_allowlist`
   (any excess → `scope_violation`); then `check_commands` run fresh in a clean
   sandbox (never trusting the worker). Gate failure short-circuits before any
   verifier tokens are spent (`checks_failed`). The gate scrubs the daemon's env
   (`XDG_*`/`MAESTRO_*`) for hermeticity (L7).
2. **Model verifier:** fresh-context session, never the implementer, in a
   throwaway checkout (`.git` removed, mutations can't reach the branch). Sees
   criteria + diff + gate output, NOT the worker transcript. Report schema is
   frozen: `{ verdict: pass|fail, findings: [{severity: blocker|concern|note,
   criterion_id, evidence}], out_of_scope_diff, commands_run }`. `fail` requires
   ≥1 `blocker`. Verifier **independence** (best available, recorded): `cross_provider`
   > `cross_model` > `fresh_context_only`.

## Config & profiles (ADR-007)

TOML at `$XDG_CONFIG_HOME/maestro/config.toml`. `[defaults]` + `[profiles.<name>]`
tables. Active profile: `--profile` > `MAESTRO_PROFILE` > `default_profile`.
Fail-loud: an unavailable tier model → `model_unavailable`; no silent substitution.

Key knobs: `roles.tier0/tier1/tier2/verifier_floor` (tier→model, a driven-CLI role
is `{ model, kind = "driven_cli", turn_budget }`); `containment_min = { tier0, tier1,
tier2 }`; `concurrency.machine_cap` (default 4) / `advisor_cap` (default 2, over-cap
delegations `queued` not failed); `watchdog_minutes` (10); `lifetime.wall_clock_minutes`
(30) / `lifetime.token_factor`; `advisor.context = "standard"|"1m"` (inbox detail);
`advisor.writable_paths` (in-repo globs made RW in the advisor mount; empty = fully
read-only); `containment.backend`/`network`; `search.backend` (`anthropic`|`searxng`|`none`).

Example profile block (personal):
```toml
[profiles.personal]
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "codex", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 1, tier2 = 2 }
```

## Containment levels (ADR-004, surfaced by `doctor`)

L0 (no isolation) < L1 (bwrap/Seatbelt) < L2 (nix devShell whitelist). The probe
reports `max_level_available`. `downgrade_policy = "tighten"` downgrades to the
best available level and tightens allowlist/turn budgets (`tighten.*_factor`);
`"refuse"` fails instead. `containment_min` in the spec can only raise the floor.

## Auth split (recap — the common gotcha)

- Driven **tier-0/tier-1 `claude`** worker → local `claude` CLI subscription auth.
- **Verifier + escalation API tiers** (opus/sonnet/haiku one-shots) →
  `ANTHROPIC_API_KEY` in the **daemon** environment.
- API keys are **stripped from driven-CLI child processes** so subscription CLIs
  use their own auth (the `metered: false` mechanism); a driven role with
  `max_budget_usd` opts into API billing instead (keys kept, CLI self-caps).

## Making the skill globally available

This skill lives at `.claude/skills/maestro-advisor/` in the maestro repo, so it
triggers when cwd is the repo. To have it trigger in **any** Claude Code session,
symlink it into your user skills dir:

```bash
ln -s "$PWD/.claude/skills/maestro-advisor" ~/.claude/skills/maestro-advisor
```

(Adjust `$PWD` to the maestro repo root.) Verify with `ls -l ~/.claude/skills/`.
