# ADR-009 / BLOCKER-1 — tracked follow-ups

BLOCKER-1 (stall detector false-kills live `claude` workers) was fixed and merged
(`bb79b3b`): the `claude` adapter stamps `idle_slot = now()` per supervision-loop
iteration (process-aliveness) instead of mirroring PTY silence. See
`OPERATING_LESSONS.md` L18 and the adapter-specific contract on
`SessionHandle::idle()`.

The core fix met both non-negotiable bars (fresh-context Fable-5 adversarial
review + behavioral integration test `m_adr009_active_claude_worker_not_false_stalled`)
and is **strictly better than production** on the false-positive. The four items
below are enhancements/hardening over a **pre-existing** baseline — none is a
regression introduced by the fix. They were deliberately deferred out of a
deep-context session (BLOCKER-1's own meta-lesson: do not rush the stall state
machine). **Each remaining item is daemon control-flow and needs its own
adversarial review + integration test before merge** (CLAUDE.md non-negotiable).

## Still open (deferred 2026-07-11)

### F-2. Wedged-claude integration test  (CLAUDE.md coverage bar)
Add a `claude`-adapter fake that stays genuinely wedged (PTY-silent past a short
`MAESTRO_WATCHDOG_SECONDS`, never exits) and assert the child is reaped and the
task does not hang. The current negative test covers only the *false-positive*
(silent-then-finishing) path; the genuine-wedge path is unchanged by the fix but
uncovered.

### F-3. Restore recovery semantics for a wedged claude worker
Today a `claude` `EndReason::Wedged` maps to terminal `session_wedged` (task dies,
worktree removed). The generic adapter's stall path instead does
`StallRecovered` (snapshot + commit in-allowlist edits + same-tier retry). Map the
claude wedge to the same recovery path so a genuinely-stuck worker gets the
fix-in-place retry budget rather than a hard fail. (Pre-existing behavior; the
BLOCKER-1 fix did not change this mapping.)

### F-5. Wall-clock ceiling on the execute phase  (chatty-wedge hole)
Because the fine detector is now inert for claude (by design), an alive worker that
makes no real progress is caught only by: the per-phase wall-clock ceiling, the
attempt-level wall-clock ceiling, and the PTY-idle internal watchdog. The last
fires only for a *silent* wedge — a **chatty** loop (emitting PTY output, making no
progress) escapes it. Confirm the per-phase/attempt wall-clock ceilings actually
bound the execute phase for claude, and tighten if there's a gap. Better still:
replace the pure-time signal with a real progress signal (trace-kind advance /
worktree growth).

### F-6. S2/S5 defensive guards
- **S2:** floor the effective `stall_timeout` for the claude adapter (~130s) so a
  mis-set tiny config can't reintroduce a false-positive window.
- **S5:** put a timeout on the post-kill `join.join()` in `delegate.rs` (~1919) so a
  child that ignores the kill can't hang the stall-recovery path indefinitely.

## Deploy note
Production still runs the pre-fix binary with the `stall_timeout_seconds ≈ 1740`
config workaround (which neuters claude stall detection wholesale). Keep that
workaround until the daemon is **rebuilt on the merged binary and restarted**, then
revert `stall_timeout_seconds` to a sane value (~300s). Do NOT rebuild/restart mid-
run — it destabilizes any in-flight orchestration.
