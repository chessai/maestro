# ADR-004: Sandboxing and Containment Levels

Status: DRAFT

## Context

Subagents — especially Codex, which takes liberties — need capability containment. Host machines vary: NixOS with the full toolkit at home; macOS at work, possibly without Nix. Policy must be portable: express *minimum containment per tier*, probe what the host offers, and record what each task actually got.

## Decision

### Capability probe

At daemon startup, probe and cache: `nix` (flakes usable), `bwrap` (Linux), Seatbelt (macOS, always present), container runtime, agent-native sandboxing (Codex CLI ships Seatbelt/Landlock+seccomp; Claude Code has permission modes). Probe results are queryable (`maestro doctor`).

The probe also **auth-checks each configured role model** (ADR-007): reachability + credentials for every tier's model and the verifier floor. A missing key or unreachable endpoint surfaces at setup via `maestro doctor` rather than only at first delegation (where it would still fail `model_unavailable`). (planned; v1 probes host capabilities only — model auth still surfaces as `model_unavailable` at first delegation)

### Levels

| level | composition |
|---|---|
| L0 | agent-native sandbox + universal post-hoc enforcement (below). Available everywhere. |
| L1 | L0 + OS sandbox wrapper (pluggable backend, see below): workspace-only writes, private tmp, network default-deny with per-task allowlist |
| L2 | L1 + Nix devShell tool whitelist: session PATH contains only the spec-declared toolchain; devShell variant selected per task |

**Universal post-hoc enforcement (all levels, fully portable):** out-of-scope diff rejection against the file allowlist, turn/token budgets, PTY watchdog, and daemon-held credentials (below). These do not depend on host capabilities and are the floor everything stands on.

### Policy

- Config sets `containment_min` per tier per profile (suggested: T0→L0, T1→L1, T2→L2 where available).
- **Backend selection is a per-profile config knob** (`containment.backend`, ADR-007): `auto` (default) prefers `podman` (rootless) → `bwrap` on Linux, Seatbelt on macOS; or force a specific backend, or `none` (L0-only, post-hoc enforcement only). Podman is the recommended default where present — stronger isolation and no dependency on the host toolchain layout. bwrap is the lightweight fallback that keeps the host toolchain visible while restricting writes + network.
- **L2 (nix devShell) is optional, not assumed.** A host without usable nix flakes simply cannot reach L2; the effective level is capped at the best the host offers (typically L1) via downgrade-and-tighten. This is the expected steady state on a locked-down work laptop with no nix — L1 podman/bwrap is the ceiling there, and that is fine.
- Host can't meet the minimum → **downgrade-and-tighten** (default): run at best available level, shrink budgets and require narrower allowlists (factors in config), record actual level in `tasks.containment_level`, emit a `containment_downgraded` event, and surface the downgrade in the advisor's inbox line. `refuse` available as a per-profile override.
- Supervision/freedom inverse: profiles with weaker advisors or weaker containment auto-tighten Codex spec templates (smaller allowlists, lower turn budgets) via config-level multipliers, not ad-hoc judgment.

### Credentials

Provider API keys never enter any sandbox. Sessions that need model access get it via the daemon's local proxy endpoint; the daemon injects auth upstream. This is simultaneously the cost meter (ADR-006) and the reason `budget_exhausted` is enforceable rather than advisory.

**v1:** provider API keys live in the daemon's environment and are used by in-process one-shot API calls (implementer, verifier, shim); they are **stripped from driven-CLI child processes** (`env_remove` of `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`/`OPENAI_API_KEY`/`CODEX_API_KEY`) so subscription CLIs use their own subscription (the `metered: false` mechanism). Per-task budgets are enforced at **attempt boundaries** from backend-reported token usage, not mid-stream. A streaming credential proxy (per-response metering + mid-stream hard-stop) remains the target but is not yet built. The `budget_exhausted` failure kind is therefore enforced at attempt boundaries in v1, not mid-stream.

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
