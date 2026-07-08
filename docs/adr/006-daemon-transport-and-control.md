# ADR-006: Daemon, Transport, Advisor Interface, and Control Paths

Status: DRAFT

## Context

The advisor interface is Claude Code (no custom chat UI). Subagents are headless. Multiple advisor chats share one machine. The human needs a break-glass kill path that does not route through any model's judgment.

## Decision

### Topology

One daemon per machine (`maestro-daemon`), Unix socket at `$XDG_RUNTIME_DIR/maestro.sock` (macOS: `~/Library/Application Support/maestro/maestro.sock`, mode 0600). Auto-spawn: any client (MCP proxy or CLI) that finds no socket spawns the daemon and retries. systemd/launchd units are optional contrib, not required. The macOS path must stay within the 104-byte `sun_path` limit; `maestro doctor` checks this and errors clearly rather than letting the bind fail obscurely.

### Auto-spawn race resolution

Every client auto-spawns through one algorithm so the M0 "two racers, exactly one daemon survives" criterion holds by construction:

1. **Connect.** `connect()` the socket. Success → handshake (below), done — the common path spawns nothing.
2. **Contend.** On failure (`ENOENT`, socket never created; `ECONNREFUSED`, stale socket from a dead daemon), acquire `flock(LOCK_EX)` on `maestro.lock`, a sibling of the socket that is **never unlinked**. The lockfile — not the socket — is the mutex, because the socket is unlinked and recreated across daemon lifetimes and so cannot serialise its own creation.
3. **Re-check under lock.** Holding the lock, `connect()` again: a racer that held the lock immediately before may have already spawned a live daemon. Success → release lock, use it. This double-check is what makes the losers cheap and correct.
4. **Spawn.** Still dead → unlink any stale socket, spawn the daemon, wait (bounded, with backoff) until it has `bind()`ed and is `accept()`ing, then release the lock.
5. **Losers** block at step 2; on acquiring the lock, step 3 finds the now-live daemon and they connect. Exactly one daemon is ever spawned.

Crash safety: `flock` releases automatically when its holder exits, so a client that dies mid-spawn frees the lock; any stale socket it left behind is cleaned by the next contender at step 4.

**Handshake** carries a protocol version. A client that reaches a running daemon of an incompatible version fails loud with a `maestro daemon restart` hint — it never spawns a second daemon to resolve skew, because the single-daemon-per-machine invariant outranks version convenience.

### Advisor = Claude Code session

- `maestro-mcp` is a stdio MCP binary configured in Claude Code; it is a thin proxy to the daemon socket and mints the `advisor_session_id` at startup.
- Advisor role, rubric, spec templates, and house-rules pointer live in the project's CLAUDE.md.
- The advisor's native Edit/Write/Bash are denied via permission **deny rules** shipped by `maestro init` into the project's Claude Code settings (deny rules are evaluated in every permission mode, including bypass). These are client-side and unverifiable from the daemon, so they are defense-in-depth, not the load-bearing control — that is filesystem isolation (below). `maestro doctor` checks the settings file contains the expected deny rules, and the advisor role prompt states the constraint. All merges additionally go through the daemon's controlled path. Approval modes (auto / ask-before / bypass) are Claude Code's own; maestro maps one behavior to them: `merge_task` is callable only in bypass mode, otherwise merge is human-only.

### Advisor filesystem isolation (structural)

The load-bearing control against the advisor mutating the repo outside the task machinery is that its Claude Code process sees the working tree **read-only**: bind-mounted read-only on Linux, denied write to the repo path by a Seatbelt profile on macOS. Every code write must route through a daemon-managed worktree via `delegate`. An advisor that ignores its role prompt, or whose deny rules are misconfigured, still physically cannot touch the tree.

Two write channels are carved out of the read-only mount:

- **Advisor scratch (default):** `$XDG_STATE_HOME/maestro/advisor/<advisor_session_id>/`, outside the repo, writable — for plans, research notes, and draft artifacts.
- **In-repo writable allowlist (opt-in):** `advisor.writable_paths` (ADR-007), a list of globs made read-write inside the mount, defaulting to **empty**. Intended for human-prose areas only (`docs/`, `notes/`). Paths on this list bypass verification by construction, so they must never cover build or test inputs (`build.rs`, generated-code dirs, manifests); `maestro doctor` warns if an allowlisted glob intersects known build inputs. Writes to these paths are journaled as `advisor_write` events (ADR-001) so the bypass still leaves a trace.

`maestro doctor` reports the mount mode and the resolved writable set.

### Advisor toolset

`delegate(task_spec)`, `task_status(filter?)`, `kill_task(task_id)`, `close_task(task_id, outcome, successor?)` (resolves `blocked` tasks: `abandoned` | `superseded`), `merge_task(task_id)` (bypass-only), `search(queries)`, `fetch_extract(url, schema)`, `journal_query(named_query, params)` (curated read-only queries: recent traces, verifier reports, cost summaries — not raw SQL).

**v1:** `merge_task` is built as an explicit advisor-initiated action gated on the task's resting `verify_passed` state; it performs a **fast-forward-only** merge of `maestro/<task-ulid>` into the task's base branch (compare-and-swap `update-ref` when the base is not checked out; `merge --ff-only` on a clean checked-out base), refuses non-ff / dirty / non-branch bases with a structured error, and emits the `merged` event. The daemon still never merges on its own — merge happens only on this explicit request. The bypass-mode restriction is a client-side Claude Code permission concern (unverifiable from the daemon). See also Merge path below.

### Notification

MCP cannot push. Every tool result the advisor receives carries an appended **inbox**: pending events since the advisor's last tool call (completions, escalations, downgrades, budget warnings, daily cost total). `task_status` exists for explicit polling. CLI can emit OS notifications as contrib.

### Kill paths (break-glass)

- Advisor: `kill_task` → `interrupted_advisor`.
- Human: `maestro kill <task-id>` | `--advisor <session>` | `--all` → `interrupted_human`. CLI talks to the daemon socket directly — no model in the path. Teardown: SIGTERM to the session process group, 5s, SIGKILL; sandbox torn down; event recorded with partial diff snapshot for forensics.
- `maestro ps` and `maestro logs -f <task-id>` (tail of captured PTY log) round out the operator surface.

### Driven sessions

`maestro-driver` owns PTYs directly via `portable-pty` (Linux/macOS uniform). No tmux dependency. Output captured to per-session log files (journal `sessions.log_path`). Watchdog: no output and no pending tool call for N minutes (config, default 10) → `session_wedged`.

Because the daemon owns PTYs in-process, a daemon restart (the version-skew remedy) necessarily tears down every driven session — you cannot update maestro without ending in-flight driven work. Affected tasks are journaled `interrupted` (payload `reason: daemon_restart`) with a partial-diff snapshot, then terminal `failed(internal_error)`; the advisor sees them in its inbox and re-delegates if still wanted. One-shot API tasks (Tier 0) are unaffected once returned. (**v1:** a daemon restart ends in-flight driven sessions but does not yet journal them as `interrupted` — planned)

### Merge path

Verified work sits on `maestro/<task-ulid>`. Never auto-merged. The advisor presents diff summary + verifier report; merging is a human git/PR action, or `merge_task` in bypass mode (fast-forward or merge commit into the task's base ref; conflicts → task `blocked`, never auto-resolved).

**v1:** `merge_task` performs a fast-forward-only merge into the task's base branch and emits the `merged` event (ADR-001); non-ff / conflicting merges are refused with a structured error (the human resolves them via `git`/PR), never auto-resolved to `blocked`.

### API proxying and cost

All subagent model traffic flows through the daemon's local proxy endpoint (ADR-004 credentials decision). The daemon meters tokens per session, enforces per-task budgets mid-flight (hard stop → `budget_exhausted`), and accumulates advisory per-day totals surfaced in the inbox. Advisor traffic (Claude Code's own) is not proxied — its costs are visible in Claude Code itself.

**v1:** provider API keys live in the daemon's environment and are used by in-process one-shot API calls (implementer, verifier, shim); they are **stripped from driven-CLI child processes** (`env_remove` of `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`/`OPENAI_API_KEY`/`CODEX_API_KEY`) so subscription CLIs use their own subscription (the `metered: false` mechanism). Per-task budgets are enforced at **attempt boundaries** from backend-reported token usage, not mid-stream. A streaming credential proxy (per-response metering + mid-stream hard-stop) remains the target but is not yet built.

## Consequences

- Two independent kill paths converge on one daemon code path; the journal distinguishes them.
- The inbox pattern means advisor awareness is pull-shaped; an idle advisor learns nothing until its next turn. Accepted: the human is in the loop, and the CLI/notification contrib covers "away from the chat."

## Tradeoffs accepted

- Auto-spawned daemon means version skew is possible when the binary updates while a daemon runs; mitigated by a protocol version in the handshake and `maestro daemon restart`.
- Driven-CLI agents (Codex) authenticated via their own subscription login may bypass the daemon's token metering; where the provider offers no API-key path, metering falls back to turn budgets + the agent's reported usage, journaled as `metered: false`. Accepted, revisit per provider.
