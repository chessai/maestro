# maestro — contributor guidance for Claude

maestro is an advisor-centric agent-orchestration harness: a Unix-socket daemon
cuts per-task git worktrees, runs a tiered worker, applies a mechanical gate
(scope + `check_commands`), then a fresh-context cross-model verifier, and merges
on the advisor's explicit command. Because maestro *runs other agents' code*, a
bug here silently corrupts every task it supervises — so changes carry a higher
bar than ordinary application code.

## Non-negotiable: adversarial review for orchestration changes

Any change to the **daemon's control flow** — the retry/escalation state machine
(`delegate.rs`), worktree lifecycle (`worktree.rs`), the mechanical gate, or the
driver adapters (`maestro-driver`) — MUST, before it is merged:

1. **Get an adversarial review from a fresh-context agent** that did NOT write the
   change. The reviewer's job is to *break* it: enumerate the states the change
   moves between and ask, for each, "what on disk / in git / in the journal is
   assumed here, and what defeats that assumption?" Reviewers report; they do not
   rubber-stamp. Prefer a different model from the implementer.
2. **Ship a behavioral / integration test that reproduces the real scenario** —
   an actual worktree + a stub worker + a gate round-trip — not only unit tests of
   the helper functions in isolation. Unit tests that each helper "works" do not
   prove the *sequence* of helpers preserves the invariant across a real attempt.

### Cautionary case — L15 (fix-in-place retry)

L15 (preserve a near-complete implementation across a `checks_failed` retry
instead of rewriting) had green unit tests for `reuse()` and `commit_all()` in
isolation, and shipped. In practice the very first real task lost its
implementation: the worker's edits were **never committed** (commit happens only
on the checks-*passed* path), so on the fix-in-place retry the uncommitted working
tree was reset back to base and re-implemented from scratch — a loss-loop, the
exact failure L15 was meant to prevent. No unit test caught it because none ran a
worker→gate→checks_failed→retry round-trip against a real worktree. That end-to-end
test is now the minimum bar for touching this code.

## Where the durable knowledge lives

- `docs/OPERATING_LESSONS.md` — the canonical, numbered record of hard-won
  operating lessons (L1–L16). Read it before changing behavior it describes; add
  to it (don't rewrite history) when a new lesson is earned.
- `docs/PLAN.md`, `docs/adr/001`–`008` — architecture and rationale.
- `.claude/skills/maestro-advisor/` — the advisor onboarding skill.

## Repo hygiene

- `.envrc` holds a real `ANTHROPIC_API_KEY` and is gitignored — NEVER commit or
  push it.
- Commit/push only when asked; branch first when on the default branch.
- theseus/mtg cargo runs inside `nix develop`; use `env -u LD_LIBRARY_PATH` for
  `git push`/`gh`.
