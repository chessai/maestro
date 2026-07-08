# ADR-004: Sandboxing and Containment Levels

Status: DRAFT

## Context

Subagents — especially Codex, which takes liberties — need capability containment. Host machines vary: NixOS with the full toolkit at home; macOS at work, possibly without Nix. Policy must be portable: express *minimum containment per tier*, probe what the host offers, and record what each task actually got.

## Decision

### Capability probe

At daemon startup, probe and cache: `nix` (flakes usable), `bwrap` (Linux), Seatbelt (macOS, always present), container runtime, agent-native sandboxing (Codex CLI ships Seatbelt/Landlock+seccomp; Claude Code has permission modes). Probe results are queryable (`maestro doctor`).

The probe also **auth-checks each configured role model** (ADR-007) and surfaces the result in `maestro doctor` (a `model_auth` section, one line per role): for every configured tier, the verifier floor, and the shim, it reports the backend and an **offline** status — credential presence (`missing_credential:ANTHROPIC_API_KEY` / `…OPENAI_API_KEY`), driven-CLI command-on-PATH (`command_not_found:<prog>`), `unconfigured:base_url`, or `ok`. A missing key surfaces at setup rather than only at first delegation (where it would still fail `model_unavailable`). The check is deliberately **offline** — no live API ping — so `doctor` works on a network-less or config-less machine; live reachability probing remains a future refinement.

### Levels

| level | composition |
|---|---|
| L0 | agent-native sandbox + universal post-hoc enforcement (below). Available everywhere. |
| L1 | L0 + OS sandbox wrapper (pluggable backend, see below): workspace-only writes, private tmp (with `TMPDIR` normalized to the in-sandbox `/tmp` so tools that honor it — e.g. `rustdoc` — don't chase an orphaned host temp path), network default-deny with per-task allowlist |
| L2 | L1 + Nix devShell tool whitelist: session PATH contains only the spec-declared toolchain; devShell variant selected per task |

**Universal post-hoc enforcement (all levels, fully portable):** out-of-scope diff rejection against the file allowlist, turn/token budgets, PTY watchdog, and daemon-held credentials (below). These do not depend on host capabilities and are the floor everything stands on.

### Policy

- Config sets `containment_min` per tier per profile (suggested: T0→L0, T1→L1, T2→L2 where available).
- **Backend selection is a per-profile config knob** (`containment.backend`, ADR-007): `auto` (default) prefers `podman` (rootless) → `bwrap` on Linux, Seatbelt on macOS; or force a specific backend, or `none` (L0-only, post-hoc enforcement only). Podman is the recommended default where present — stronger isolation and no dependency on the host toolchain layout. bwrap is the lightweight fallback that keeps the host toolchain visible while restricting writes + network.
- **Functional probe, not just presence:** the capability probe accepts podman/docker only when the runtime is actually *functional* (`<runtime> info` exits 0), recorded as `container_runtime_functional` in `maestro doctor`. A present-but-broken rootless podman (missing `newuidmap`, unconfigured `policy.json`) is treated as unusable under `auto` selection and downgrades to bwrap rather than being chosen and failing at wrap time. An explicitly *forced* `containment.backend = "podman"` is still honored (operator's choice), with usability decided by the downgrade path.
- **Default image:** because podman does not inherit the host toolchain, an image is required; when `containment.podman_image` is unset the backend falls back to a compiled-in default (`docker.io/library/rust:1`, tracking the toolchain this repo builds against) so podman works out of the box. A work profile targeting another stack overrides it per profile.
- **L2 (nix devShell) is optional, not assumed.** A host without usable nix flakes simply cannot reach L2; the effective level is capped at the best the host offers (typically L1) via downgrade-and-tighten. This is the expected steady state on a locked-down work laptop with no nix — L1 podman/bwrap is the ceiling there, and that is fine.
- Host can't meet the minimum → **downgrade-and-tighten** (default): run at best available level, shrink budgets and require narrower allowlists (factors in config), record actual level in `tasks.containment_level`, emit a `containment_downgraded` event, and surface the downgrade in the advisor's inbox line. `refuse` available as a per-profile override.
- Supervision/freedom inverse: profiles with weaker advisors or weaker containment auto-tighten Codex spec templates (smaller allowlists, lower turn budgets) via config-level multipliers, not ad-hoc judgment. **v1:** the allowlist half is enforced — on a containment downgrade, or for a driven-CLI role under `codex_tighten`, the mechanical gate caps the number of changed files at `ceil(tighten.allowlist_factor × allowlist_len)` and rejects an over-cap (but in-allowlist) diff as a terminal `scope_violation`. The turn half (`tighten.turn_factor`) is enforced on the **structured `claude` driven adapter**, which counts `assistant` events in its stream-json output and hard-stops the execute phase at `floor(turns × turn_factor)` → terminal `turn_budget_exceeded` (the generic PTY driven path still lacks turn-accounting and relies on the watchdog).

### Credentials

Provider API keys never enter any sandbox. Sessions that need model access get it via the daemon's local proxy endpoint; the daemon injects auth upstream. This is simultaneously the cost meter (ADR-006) and the reason `budget_exhausted` is enforceable rather than advisory.

**v1:** provider API keys live in the daemon's environment and are used by in-process one-shot API calls (implementer, verifier, shim); they are **stripped from driven-CLI child processes** (`env_remove` of `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`/`OPENAI_API_KEY`/`CODEX_API_KEY`) so subscription CLIs use their own subscription (the `metered: false` mechanism). Per-task budgets are enforced by default at **attempt boundaries** from backend-reported token usage.

The streaming credential proxy (per-response metering + **mid-stream hard-stop**) is built as `maestro-proxy`: a daemon-local HTTP endpoint that injects the daemon-held key upstream (keys never enter the sandbox that calls it), streams the SSE response through unchanged while metering `message_start`/`message_delta` usage into a per-task `Ledger`, and cuts the response mid-stream with a `budget_exhausted` error event the moment a task's token ceiling is crossed. It is **on by default** in v1 (`[proxy].enabled` defaults true; set `proxy.enabled = false` to opt out) and **degrades gracefully** — if the proxy can't bind, the daemon logs a warning and the backends fall back to their direct upstream, so a bind failure never fails delegation. Both task-scoped metered surfaces route through it when it is up:

- The **implementer** routes gated (`base_url` → proxy, tagged `X-Maestro-Task: <task_id>`): the proxy meters its usage into the per-task ledger and enforces the ceiling at the chokepoint — a pre-forward gate rejects the next request (HTTP 429 `budget_exhausted`) once the cumulative task total is over budget (cutting a runaway multi-turn implementer between turns), plus the mid-stream SSE cut for any streaming caller.
- The **verifier** routes **meter-only** (tagged `X-Maestro-Meter: <task_id>`): its usage accumulates into the same per-task ledger — so the implementer's gate accounts for verifier spend and stays consistent with the attempt-boundary journal total — but it is **never pre-blocked or cut** (a task at its ceiling must still be verified — ADR-002's "verification never skipped").
- The **shim is deliberately not routed** (advisor-scoped, no task budget, already journaled, and in-daemon so the proxy would give it no key-hiding benefit).

`budget_exhausted` is therefore enforced both at attempt boundaries (journal) and at the proxy chokepoint (between-request + mid-stream) for the implementer. Verified live end-to-end: a real delegation completes through the default-on proxy, exercising the gated implementer and the meter-only verifier.

### Nix specifics (L2)

DevShell variants defined in the maestro flake (`devShells.<system>.codex-rust`, `codex-rust-net`, …); the orchestrator launches driven sessions via `nix develop .#<variant> -c <agent-cli>` inside the L1 wrapper. Missing tool in the whitelist is a structured failure surfaced to the advisor — never something the agent works around.

### OS-sandbox backends (pluggable)

The L1 wrapper is a `maestro-sandbox` backend selected by probe + config, not a fixed tool:

| backend | host | notes |
|---|---|---|
| `bwrap` | Linux | lightweight namespaces; default where present |
| `podman` (rootless) | Linux | stronger for sessions that execute untrusted build/test code (proc-macros, `build.rs`): real cgroups, pid namespace, network policy. Preferred backend for verification surfaces (below) |
| `seatbelt` (`sandbox-exec`) | macOS | host-level profile; the lightweight floor on bare macOS |
| `podman-machine` | macOS | true-VM isolation via a Linux VM; heavyweight (VM startup, toolchain lives in the image, not the host) — an upper option, never the mac default |

L2's nix devShell composes with a container backend by resolving the devShell on the host and bind-mounting `/nix/store` read-only into the container; the capability probe verifies this path works, else L2 falls back per downgrade policy.

### Verification surfaces inherit the task recipe

The mechanical gate and the model verifier both **execute untrusted code** — building and testing an implementer's diff runs attacker-influenceable build scripts, tests, and proc-macros. They run under the **same containment recipe as the task's implementer** (same level, same OS-sandbox backend, same devShell variant), never a weaker or divergent environment:

- A task contained at L2 has its gate and verifier contained at L2; the host is protected from the diff, not just the branch.
- Because the gate builds in the implementer's declared toolchain, an environment mismatch cannot itself produce a spurious `checks_failed`.

The verifier's throwaway checkout (ADR-002) sits *inside* this containment: read-only-to-the-branch is the data-flow guarantee, the shared containment recipe is the host-execution guarantee.

## Consequences

- "What could this session touch" is declarative and journaled; telemetry can test whether weakly contained tasks misbehave more (scope-violation rate by `containment_level`).
- A bare macOS work laptop is first-class: L0 there is still Codex-native Seatbelt plus the full post-hoc floor.

## Tradeoffs accepted

- Downgrade-and-tighten means Tier 2 work can run at L0 on a constrained host; accepted because refusing on exactly the machines you can't fix defeats the tool. Mitigated by tightening, journaling, and the per-profile `refuse` escape hatch.
- Seatbelt profiles are deprecated-but-functional Apple surface; accepted, isolated behind the `maestro-sandbox` crate so a replacement (e.g. containerized macOS runners) is a backend swap.
