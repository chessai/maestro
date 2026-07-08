# maestro — Planning Document

Status: DRAFT v0.2
Working name: **maestro** (known collisions: mobile.dev's Maestro UI-testing framework, ai-maestro dashboard. Acceptable for a personal tool; revisit before any open-sourcing.)

## 1. What this is

maestro is an advisor-centric agent harness. A single strong model (the **advisor** — Fable, or Opus 4.7 at work) is the only agent the human converses with. It plans, delegates implementation to tier-routed subagents (Sonnet / Codex / Opus), delegates web retrieval to pure data shims, and reviews verifier reports. Subagents are stateless, headless, sandboxed, and never talk to the human or each other. The human retains two break-glass controls: Claude Code's approval modes, and a CLI kill path that routes through no model.

Design lineage: "give goals not steps, falsifiable acceptance criteria, never verify your own work" (adopted and made mechanical); provenance discipline from glory/theseus (shim offsets, journal trace chains); sentinel-style independent kill path from the trading system.

## 2. Non-goals (v1)

- No custom chat UI — the advisor **is** a Claude Code session (ADR-006).
- No task DAG execution — linear sequences per advisor; schema reserves DAG columns (ADR-001, ADR-003).
- No auto-merge in any mode except explicit bypass `merge_task` (ADR-006).
- No cross-machine orchestration — one daemon per machine, journals are per-machine.
- No agent-to-agent communication, ever (this one is load-bearing, not just deferred).

## 3. Architecture

```
 human ──chat──▶ Claude Code (advisor session, tools = maestro only)
                     │ stdio
                     ▼
               maestro-mcp (proxy, mints advisor_session_id)
                     │ unix socket
                     ▼
              ┌─ maestro-daemon ─────────────────────────────┐
              │ task registry · scheduler · locks · budgets  │
              │ API proxy (creds + metering) · shim executor │
              │ journal writer (sole)                        │
              └──┬──────────────┬──────────────┬─────────────┘
                 ▼              ▼              ▼
           maestro-driver  maestro-sandbox  journal.db (SQLite/WAL)
           (portable-pty)  (L0/L1/L2)            ▲
                 │                               │ read-only via socket
        driven CLI / one-shot sessions      maestro-cli
        in git worktrees (maestro/<ulid>)   (ps · kill · logs · doctor)
```

Data-flow invariants:

1. All **subagent** model credentials live in the daemon and never enter a sandbox; subagent traffic goes through its proxy (metering + hard budgets). The advisor's own credentials belong to Claude Code and are out of scope. Exception journaled as `metered: false` for subscription-authenticated CLIs (ADR-006).
2. All synthesis happens in the advisor; shims return verbatim spans with daemon-validated offsets (ADR-005).
3. All writes to the repo happen in per-task worktrees by implementers only; the mechanical gate diffs against the allowlist post-session (ADR-002).
4. All state transitions are append-only journal events (ADR-001).

## 4. Decision index

| # | decision | ADR |
|---|---|---|
| 1 | SQLite/WAL journal, event-sourced, daemon sole writer, ULIDs | 001 |
| 2 | Failure taxonomy (11 kinds, frozen) | 001 |
| 3 | Universal verification: mechanical gate → fresh-context model verifier; report-only | 002 |
| 4 | Verifier independence hierarchy, recorded per report | 002 |
| 5 | Frozen verifier report schema (findings by criterion) | 002 |
| 6 | 3 tiers, advisor-scored rubric, explicit tier arg, fail-loud model availability | 003 |
| 7 | Adjective-free acceptance criteria enforced by spec validation | 003 |
| 8 | Codex asymmetric strictness: allowlist required, plan-echo, hard budgets | 003 |
| 9 | Escalate after 2 verification failures; blocked after 1 at tier 2; scope violations don't escalate | 003 |
| 10 | Containment levels L0–L2, probe-based, downgrade-and-tighten default | 004 |
| 11 | Nix devShell whitelists as L2, not a dependency | 004 |
| 12 | Two shim tools, no free-text fields, offset-validated verbatim extraction | 005 |
| 13 | SearXNG per-profile, no fallback, `backend_unavailable` is loud | 005 |
| 14 | Advisor = locked-down Claude Code session over stdio MCP proxy | 006 |
| 15 | Inbox-on-every-tool-result notification; no push | 006 |
| 16 | Dual kill paths (advisor tool / CLI), one daemon teardown code path | 006 |
| 17 | portable-pty driven sessions, no tmux; watchdog for wedged sessions | 006 |
| 18 | Branch-per-task, human-merge default, `merge_task` bypass-only | 006 |
| 19 | TOML profiles, fail-loud everywhere, supervision/freedom inverse as config invariant | 007 |
| 20 | Pluggable implementer backends selected by role `kind` (anthropic default, mock, driven_cli @ M3, openai_compat later); per-session metering | 008 |

## 5. Workspace layout

```
maestro/
├── flake.nix                  # dev env + L2 devShell variants (codex-rust, codex-rust-net, …)
├── Cargo.toml                 # workspace
├── crates/
│   ├── maestro-journal/       # schema, migrations, typed queries (shared, no I/O policy)
│   ├── maestro-daemon/        # scheduler, locks, budgets, API proxy, shim executor
│   ├── maestro-mcp/           # stdio MCP proxy binary
│   ├── maestro-cli/           # ps / kill / logs / doctor / daemon / init
│   ├── maestro-driver/        # portable-pty session driving, plan-echo, watchdog
│   ├── maestro-implementer/   # ImplementerBackend trait + backends (mock, anthropic; driven_cli/openai_compat later — ADR-008)
│   └── maestro-sandbox/       # capability probe, L0/L1/L2 profiles (bwrap/Seatbelt/nix)
├── docs/
│   ├── PLAN.md
│   └── adr/001…007
├── prompts/                   # advisor role, rubric, spec templates, house-rules template
└── contrib/                   # systemd/launchd units, notification scripts, waybar module
```

`maestro-journal` is the only crate two binaries share deeply; keep it free of daemon policy so the CLI's read models don't drag in scheduler code.

## 6. Milestones

Each milestone has falsifiable exit criteria (eating the dogfood of ADR-003 §TaskSpec).

**M0 — skeleton.** Daemon with socket, auto-spawn, journal migrations, `maestro ps/doctor`, config/profile resolution.
*Exit:* `maestro doctor` prints resolved profile + capability probe on NixOS and bare macOS; two CLI invocations race to auto-spawn and exactly one daemon survives.

**M1 — one-shot delegation.** `delegate` for Tier 0 via API one-shot; worktree lifecycle; mechanical gate (allowlist diff + check commands); inbox.
*Exit:* advisor in Claude Code delegates a real single-file change to Sonnet; out-of-scope edit is auto-rejected with `scope_violation`; branch appears; human merges by hand.

**M2 — verification.** Fresh-context verifier sessions, report schema, escalation loop, `blocked`/`close_task`.
*Exit:* a task with a deliberately failing criterion escalates 0→1 after two failures and blocks at the configured top tier; `journal_query` returns the full report chain.

**M3 — driven sessions.** portable-pty driver, plan-echo gate, watchdog, Tier 1 Codex (personal) / Sonnet-CLI (work), kill paths.
*Exit:* `maestro kill` terminates a live Codex session mid-run leaving `interrupted_human` + partial diff snapshot; a spec-violating plan aborts with `plan_rejected` and zero workspace edits.

**M4 — containment.** L1 (bwrap/Seatbelt) and L2 (nix devShell) profiles, downgrade-and-tighten with journaled levels.
*Exit:* same Tier 1 task runs L2 on NixOS and L0-tightened on bare macOS with `containment_downgraded` evidence; an L2 session cannot see a tool outside its variant's whitelist.

**M5 — shim.** search/fetch_extract, offset validation, cache, `backend_unavailable`.
*Exit:* advisor answers a research question citing url+offset extractions; a fabricated offset is rejected by the daemon; work profile (no backend) surfaces the error verbatim in chat.

**M6 — budgets & telemetry.** API proxy metering, hard per-task budgets, daily totals in inbox, canned telemetry queries (failure rates by tier×model×containment).
*Exit:* a task hits its token budget mid-flight → `budget_exhausted`; `journal_query("routing_report")` returns non-empty aggregates over M1–M5 history.

Order rationale: verification (M2) before driven sessions (M3) so Codex arrives into a world that already grades it; containment (M4) before shim (M5) because shim is lower-risk (no workspace) and containment protects M3's new attack surface sooner.

## 7. Risks

| risk | exposure | mitigation |
|---|---|---|
| Codex CLI drive-ability (prompt injection into PTY, ToS on automation, output-format drift) | M3 | plan-echo + budgets bound damage; driver isolates per-CLI adapters; fallback is Sonnet driven-CLI everywhere |
| Advisor Edit/Write denial misconfigured | ongoing | deny rules evaluated in all modes + `maestro doctor` check + merges only via daemon path (defense in depth, honestly limited — ADR-006) |
| Goodharted acceptance criteria (implementer satisfies letter not intent) | ongoing | verifier sees criteria *and* diff, `concern` findings surface smells without blocking; advisor owns criterion quality; telemetry on which criteria pass suspiciously often |
| Verifier cost creep | M2+ | mechanical gate short-circuit; cheapest-model floor for T0; measured per-task in journal |
| SQLite contention under parallel sessions | M3+ | single-writer daemon by construction; WAL; if it ever bites, batch event writes |
| Subscription-authed CLIs bypass metering | M3 | turn budgets + reported usage, journaled `metered: false`; accept residual |
| Category churn / dependency rot (portable-pty, rmcp) | ongoing | thin adapters; tmux fallback documented but not built |

## 8. Open items (tracked, not blocking)

1. Name collision if open-sourced (see header note).
2. DAG execution semantics (columns reserved; needs its own ADR when linear stops sufficing).
3. Keychain/agenix secret backends (trait exists at v1, backends later).
4. Per-provider driven-CLI adapters beyond Codex/Claude (OpenCode etc.) — driver trait shaped for it, no commitments.
5. Advisor-context compaction thresholds ("standard" vs "1m" presentation) — ship conservative defaults, tune with use.
6. `openai_compat` implementer backend for local (Ollama/llama.cpp) and cheap OpenAI-compatible providers — the personal-cost escape valve. Config seam reserved in ADR-008 (`kind` + `base_url`); implement after M3 so it lands alongside the driven-CLI subscription path.

## 9. First actions

1. `maestro init` repo scaffold: workspace, flake, journal migrations, ADRs (this directory).
2. Freeze review: one more pass over ADR-001 taxonomy, ADR-002 report schema, ADR-003 TaskSpec by human before M0 code.
3. Write `prompts/advisor.md` (role, rubric, spec templates) — can be drafted in parallel with M0.
