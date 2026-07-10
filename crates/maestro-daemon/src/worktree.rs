//! Git worktree management for delegated tasks (M1, ADR-006 branch-per-task).
//!
//! Each delegated task gets an isolated git worktree on a fresh branch
//! `maestro/<task-id>`, created off the spec's `base_ref`. The implementer
//! backend writes into the worktree; the mechanical gate (see [`crate::gate`])
//! diffs it against `base_ref`. On gate pass the worktree is committed and the
//! branch is left for a human to merge — the daemon NEVER merges (ADR-006).
//!
//! All git operations shell out to the `git` binary via `std::process::Command`
//! (git is on PATH in the devShell); there is no libgit2 dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use maestro_journal::paths;

/// The managed root under which per-task worktrees are created:
/// `<state_dir>/worktrees`.
fn worktrees_root() -> PathBuf {
    paths::state_dir().join("worktrees")
}

/// The conventional worktree path for a task: `<state_dir>/worktrees/<task_id>`.
/// This is where [`create`] places the worktree; the merge cleanup path uses it
/// to detach the worktree before deleting the merged branch.
pub fn worktree_path(task_id: &str) -> PathBuf {
    worktrees_root().join(task_id)
}

/// Whether the task's existing worktree can be REUSED in place for a
/// fix-in-place retry (operating-lesson L15), rather than cutting a fresh one off
/// `base_ref`. `true` iff the conventional worktree dir exists on disk AND `git`
/// still registers it as a valid worktree of `repo` (its `.git` link resolves).
///
/// This is the guard for the `checks_failed` retry path: when the prior attempt
/// left near-complete edits that only tripped a check command, the next attempt
/// keeps THOSE edits (this returns `true`) instead of discarding them via
/// [`create`]. Best-effort: any git error or a missing/torn-down worktree → the
/// caller falls back to a fresh [`create`], so the reuse is always safe.
pub fn reuse(_repo_path: &Path, task_id: &str) -> bool {
    is_live_worktree(&worktree_path(task_id))
}

/// Whether `wt` is a live git working tree that can be reused in place: it exists
/// on disk AND `git -C <wt> rev-parse --is-inside-work-tree` succeeds (its gitdir
/// link resolves back into the parent repo). A stale dir whose administrative
/// worktree entry was pruned fails this → the caller cuts a fresh worktree. Split
/// out of [`reuse`] so it is unit-testable without the state-dir env.
fn is_live_worktree(wt: &Path) -> bool {
    if !wt.exists() {
        return false;
    }
    let Some(wt_str) = wt.to_str() else {
        return false;
    };
    git_ok(wt_str, &["rev-parse", "--is-inside-work-tree"])
}

/// Run `git` with `args`, returning combined stdout/stderr on success or an
/// error carrying the captured output on failure.
fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "`git {}` failed ({}): {}{}",
            args.join(" "),
            output.status,
            stdout,
            stderr
        );
    }
    Ok(stdout.into_owned())
}

/// Resolve a possibly-symbolic `base_ref` to a concrete, mergeable branch name.
///
/// The delegation pipeline branches every task off `base_ref` and `merge_task`
/// later fast-forwards that same `base_ref` — which requires it to name a LOCAL
/// branch. A natural spec value like `"HEAD"` is symbolic: it branches fine but
/// cannot be advanced. This resolves such symbolic refs to the current branch's
/// short name UP FRONT so `"HEAD"` "just works" end-to-end.
///
/// Rules (best-effort; any git error → `base_ref` unchanged, never panics):
/// - `base_ref` already names a local branch (`refs/heads/<base_ref>` verifies)
///   → return it unchanged.
/// - `base_ref == "HEAD"`, OR `HEAD` is a symbolic ref and `base_ref` names the
///   current branch → resolve to the current branch short name and return that.
/// - Otherwise (a raw SHA, a tag, or detached HEAD) → return `base_ref`
///   UNCHANGED. Branching off a SHA/tag is valid advanced usage; it simply is
///   not `merge_task`-able, and `merge_task` already gives a clear error there.
pub fn resolve_base_ref(repo: &Path, base_ref: &str) -> String {
    let Some(repo) = repo.to_str() else {
        return base_ref.to_string();
    };

    // Already a local branch → mergeable as-is.
    if git_ok(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{base_ref}")],
    ) {
        return base_ref.to_string();
    }

    // Symbolic ref resolving to the current branch. `symbolic-ref --quiet HEAD`
    // succeeds only when HEAD points at a branch (not detached). We resolve when
    // the caller asked for "HEAD" itself, or named the current branch explicitly.
    let current_branch = git_c(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    if let Some(branch) = current_branch {
        if base_ref == "HEAD" || base_ref == branch {
            return branch;
        }
    }

    // A raw SHA / tag / detached HEAD: leave unchanged (valid, just not mergeable).
    base_ref.to_string()
}

/// Resolve `base_ref` to the concrete commit SHA it points at RIGHT NOW
/// (`git rev-parse <base_ref>^{commit}`). Best-effort: any git error → `None`.
///
/// The delegation pipeline pins this SHA at spawn — the exact commit the task's
/// worktree is cut from — and uses it (NOT the live symbolic `base_ref`) for the
/// scope/allowlist diff. This makes the scope check immune to the base branch
/// advancing while the task runs (ADR-006 / operating-lesson L3): if a sibling
/// task merges into `base_ref` mid-flight, the newly-merged files no longer
/// appear as out-of-allowlist deletions in this task's diff. The *merge target*
/// stays the live `base_ref`; only the scope diff uses the pinned SHA.
pub fn resolve_to_sha(repo_path: &Path, base_ref: &str) -> Option<String> {
    let repo = repo_path.to_str()?;
    // `<ref>^{commit}` forces peeling to a commit (a tag/annotated-tag resolves
    // to the commit it points at), so the pinned value is always a commit SHA.
    let spec = format!("{base_ref}^{{commit}}");
    git_c(repo, &["rev-parse", "--verify", "--quiet", &spec]).ok()
}

/// Create a worktree for `task_id` off `base_ref`, on a fresh branch
/// `maestro/<task_id>`. Returns the worktree path.
pub fn create(repo_path: &Path, base_ref: &str, task_id: &str) -> Result<PathBuf> {
    let root = worktrees_root();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating worktrees root {}", root.display()))?;
    let wt = root.join(task_id);
    let repo = repo_path.to_str().context("repo path is not valid UTF-8")?;
    let branch = format!("maestro/{task_id}");

    // WORK-LOSS GUARD (same class as operating-lesson L15): the branch
    // `maestro/<task_id>` may carry REAL committed work — the fix-in-place
    // checkpoint the mechanical gate commits on `checks_failed`. On a tier
    // ESCALATION the pipeline re-cuts a fresh worktree off `base_ref` (ADR-003:
    // the bigger model re-approaches from base, it does NOT resume the smaller
    // model's checkpoint), and the `git branch -D` below would DESTROY that
    // committed checkpoint — unrecoverable if the escalated tier then can't run
    // (the live case: tier-0 at 7/8 tests, tier-1 out of credits). Before
    // deleting, PRESERVE any commits the branch has beyond `base_ref` to a durable
    // salvage ref an advisor can `git log`/cherry-pick. This does NOT change
    // escalation semantics (the fresh worktree still starts at base_ref); it only
    // guarantees nothing committed is ever lost.
    salvage_task_branch(repo, base_ref, task_id, &branch);

    // If a stale worktree dir exists (a prior crashed run OR a prior attempt in
    // the escalation loop, ADR-003: a fresh worktree per attempt off base_ref),
    // best-effort clear it so `git worktree add` does not refuse.
    if wt.exists() {
        remove(repo_path, &wt);
        let _ = std::fs::remove_dir_all(&wt);
    }
    // The branch `maestro/<task_id>` persists across attempts even after the
    // worktree is removed. Re-running an attempt re-creates it off base_ref, so
    // force-delete any existing branch first; `git worktree add -b` would
    // otherwise refuse an existing branch. Best-effort: absent branch is fine.
    let _ = Command::new("git")
        .args(["-C", repo, "branch", "-D", &branch])
        .output();

    let wt_str = wt.to_str().context("worktree path is not valid UTF-8")?;
    git(&[
        "-C", repo, "worktree", "add", wt_str, "-b", &branch, base_ref,
    ])
    .with_context(|| format!("creating worktree for task {task_id}"))?;
    Ok(wt)
}

/// Before [`create`] force-deletes an existing `maestro/<task_id>` branch,
/// PRESERVE its tip to a durable salvage ref IFF it carries commits beyond
/// `base_ref` (real work — the fix-in-place checkpoint). Without this, a tier
/// escalation's `git branch -D` silently DESTROYS the lower tier's committed
/// near-complete implementation (the confirmed work-loss bug, same class as L15).
///
/// The salvage ref is `refs/maestro/salvage/<task_id>/<short_sha>` — the tip's
/// short sha is embedded so repeated escalations of the SAME task never clobber
/// each other's salvage (each checkpoint tip lands at its own ref). It lives
/// under `refs/maestro/` (NOT `refs/heads/`), so it is never a branch the
/// pipeline reuses/re-cuts/merges — it is a pure recovery anchor.
///
/// Best-effort but LOUD (mirrors the L15 philosophy): any git probe failure or a
/// branch with NO commits beyond base is a silent no-op (nothing to save); a
/// FAILURE to write the salvage ref when there IS work to save is logged at
/// `error` (discoverability), but never wedges the task — `create` proceeds to
/// re-cut regardless. On success emits a `tracing::info` recording the salvage
/// ref + task id so it is discoverable in the daemon log.
///
/// `base_ref` may be a branch name, a tag, or a raw SHA — `<base_ref>..<branch>`
/// resolves in all three cases (git peels the endpoints to commits).
fn salvage_task_branch(repo: &str, base_ref: &str, task_id: &str, branch: &str) {
    let branch_ref = format!("refs/heads/{branch}");

    // The branch must exist AND resolve to a commit. Absent branch → nothing to
    // salvage (the common first-attempt / fresh-cut case): silent no-op.
    let Ok(tip) = git_c(repo, &["rev-parse", "--verify", "--quiet", &branch_ref]) else {
        return;
    };

    // Does the branch carry commits BEYOND base_ref? `git rev-list base..branch`
    // is empty when the branch is at/behind base (no checkpoint was committed —
    // e.g. an escalation triggered by verifier failures with no `checks_failed`).
    // A git error here (e.g. base_ref unresolvable) → treat as "cannot prove
    // there is work" and skip: we do not want to spuriously salvage the whole
    // history, and the delete is not our call to block.
    let range = format!("{base_ref}..{branch}");
    match git_c(repo, &["rev-list", &range]) {
        Ok(revs) if !revs.trim().is_empty() => {} // there ARE task commits → salvage
        Ok(_) => return,                          // branch == base: nothing committed beyond base
        Err(e) => {
            tracing::warn!(
                task = task_id,
                base_ref,
                error = %e,
                "salvage: could not compute base..branch to check for committed work; \
                 skipping salvage (branch delete proceeds)"
            );
            return;
        }
    }

    // Embed the tip's short sha so repeated escalations of the same task land at
    // distinct salvage refs (no overwrite). `--short` gives a stable abbrev.
    let short = git_c(repo, &["rev-parse", "--short", &tip]).unwrap_or_else(|_| {
        // Fall back to a fixed-length prefix of the full sha if abbrev fails.
        tip.chars().take(12).collect()
    });
    let salvage_ref = format!("refs/maestro/salvage/{task_id}/{short}");

    // Point the salvage ref at the tip. This is a pure ref write — no worktree,
    // no branch — so it cannot conflict with the re-cut that follows.
    match git_c(repo, &["update-ref", &salvage_ref, &tip]) {
        Ok(_) => {
            tracing::info!(
                task = task_id,
                salvage_ref = %salvage_ref,
                tip = %tip,
                "salvage: preserved the task branch's committed checkpoint before \
                 escalation re-cut; recover with `git log`/`git cherry-pick`"
            );
        }
        Err(e) => {
            // LOUD but non-fatal: we could not save the work, but wedging the task
            // helps no one. The branch delete proceeds; the loss is at least noisy.
            tracing::error!(
                task = task_id,
                salvage_ref = %salvage_ref,
                tip = %tip,
                error = %e,
                "salvage: FAILED to preserve the task branch's committed checkpoint \
                 before escalation re-cut — committed work may be lost"
            );
        }
    }
}

/// `git add -A` in the worktree, then commit with `message` — but only if there
/// is something staged. Returns `true` if a commit was made, `false` if the
/// worktree was clean.
pub fn commit_all(worktree: &Path, message: &str) -> Result<bool> {
    let wt = worktree.to_str().context("worktree path is not valid UTF-8")?;
    git(&["-C", wt, "add", "-A"])?;
    // `git diff --cached --quiet` exits 1 when there IS something staged.
    let anything_staged = !Command::new("git")
        .args(["-C", wt, "diff", "--cached", "--quiet"])
        .status()
        .with_context(|| "spawning `git diff --cached --quiet`")?
        .success();
    if !anything_staged {
        return Ok(false);
    }
    git(&["-C", wt, "commit", "-m", message])?;
    Ok(true)
}

/// The set of paths changed vs `base_ref` (staged + working tree), including
/// new files. Runs `git add -A` first so untracked files are counted, then
/// `git diff --cached --name-only <base_ref>`.
pub fn changed_files(worktree: &Path, base_ref: &str) -> Result<Vec<String>> {
    let wt = worktree.to_str().context("worktree path is not valid UTF-8")?;
    git(&["-C", wt, "add", "-A"])?;
    let out = git(&["-C", wt, "diff", "--cached", "--name-only", base_ref])?;
    Ok(out
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// The unified diff of the worktree's changes vs `base_ref` (staged + new).
/// Runs `git add -A` first so untracked files are included, then
/// `git diff --cached <base_ref>`. This is the verifier's structural input
/// (ADR-002): the implementer's changes as a unified diff.
pub fn diff(worktree: &Path, base_ref: &str) -> Result<String> {
    let wt = worktree.to_str().context("worktree path is not valid UTF-8")?;
    git(&["-C", wt, "add", "-A"])?;
    git(&["-C", wt, "diff", "--cached", base_ref])
}

/// The unified diff of ONLY `paths` vs `base_ref` (staged + new). Runs
/// `git add -A` first so untracked writes are staged, then
/// `git diff --cached <base_ref> -- <paths...>` with each path as a LITERAL
/// pathspec. Unlike [`diff`], this restricts the diff to an exact file set — the
/// gate's already-globset-filtered in-allowlist changed files — so post-build
/// artifacts (e.g. `target/`) staged by `add -A` after the gate ran its check
/// commands never leak into the verifier's structural input. An empty `paths`
/// slice yields an empty diff (no `git` invocation).
pub fn diff_paths(worktree: &Path, base_ref: &str, paths: &[String]) -> Result<String> {
    if paths.is_empty() {
        return Ok(String::new());
    }
    let wt = worktree.to_str().context("worktree path is not valid UTF-8")?;
    git(&["-C", wt, "add", "-A"])?;
    let mut args: Vec<&str> = vec!["-C", wt, "diff", "--cached", base_ref, "--"];
    for p in paths {
        args.push(p.as_str());
    }
    git(&args)
}

/// Commit with `message` ONLY the changes to `paths`, iff there is something to
/// commit for them (mirrors [`commit_all`]'s "nothing staged → `Ok(false)`"
/// guard). Restricting to the gate's in-allowlist changed files makes the
/// committed task branch immune to post-build artifacts (e.g. `target/`) even
/// when the repo has no `.gitignore`. An empty `paths` slice commits nothing →
/// `Ok(false)`. Paths are EXACT files (not globs), so there is no
/// git-pathspec/globset mismatch.
///
/// The index is reset to the current commit FIRST, so any earlier `git add -A`
/// (e.g. from a preceding [`diff_paths`] call, which stages everything to build
/// the verifier diff) cannot leak already-staged out-of-allowlist paths into the
/// commit — only `paths` are staged, then committed with an explicit pathspec.
pub fn commit_paths(worktree: &Path, paths: &[String], message: &str) -> Result<bool> {
    let wt = worktree.to_str().context("worktree path is not valid UTF-8")?;
    if paths.is_empty() {
        return Ok(false);
    }
    // Reset the index to HEAD so a prior `add -A` (from diff_paths) does not leave
    // out-of-allowlist paths staged; then stage ONLY the given paths.
    git(&["-C", wt, "reset", "-q"])?;
    let mut add_args: Vec<&str> = vec!["-C", wt, "add", "--"];
    for p in paths {
        add_args.push(p.as_str());
    }
    git(&add_args)?;
    // `git diff --cached --quiet -- <paths>` exits 1 when something is staged for
    // those paths. Check on the restricted pathspec so we do not commit unrelated
    // staged changes even if the reset were partial.
    let mut quiet_args: Vec<&str> = vec!["-C", wt, "diff", "--cached", "--quiet", "--"];
    for p in paths {
        quiet_args.push(p.as_str());
    }
    let anything_staged = !Command::new("git")
        .args(&quiet_args)
        .status()
        .with_context(|| "spawning `git diff --cached --quiet`")?
        .success();
    if !anything_staged {
        return Ok(false);
    }
    // Commit with an explicit pathspec so ONLY the given paths are recorded, even
    // if something else were somehow staged.
    let mut commit_args: Vec<&str> = vec!["-C", wt, "commit", "-m", message, "--"];
    for p in paths {
        commit_args.push(p.as_str());
    }
    git(&commit_args)?;
    Ok(true)
}

/// A best-effort partial-diff snapshot of a driven session's worktree, for
/// forensic capture on kill / wedge (ADR-006). Stages everything then diffs
/// against `base_ref`; any git error yields an empty string (the snapshot is
/// advisory, never load-bearing). Untracked files ARE included via `add -A`.
pub fn snapshot_diff(worktree: &Path, base_ref: &str) -> String {
    let Some(wt) = worktree.to_str() else {
        return String::new();
    };
    // Stage everything so untracked writes are captured; ignore any error.
    let _ = Command::new("git")
        .args(["-C", wt, "add", "-A"])
        .output();
    match Command::new("git")
        .args(["-C", wt, "diff", "--cached", base_ref])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

/// Best-effort list of changed files in the worktree that match the file
/// allowlist globs. Used by stall-recovery (ADR-009 Phase 2) to commit only
/// in-scope edits before a retry. Returns an empty vec on any error.
pub fn changed_in_allowlist(worktree: &Path, base_ref: &str, allowlist: &[String]) -> Vec<String> {
    let Ok(changed) = changed_files(worktree, base_ref) else {
        return Vec::new();
    };
    if allowlist.is_empty() {
        return changed;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pat in allowlist {
        if let Ok(g) = globset::Glob::new(pat) {
            builder.add(g);
        }
    }
    let Ok(set) = builder.build() else {
        return changed;
    };
    changed
        .into_iter()
        .filter(|path| set.is_match(path))
        .collect()
}

/// The result of a successful [`merge_task_branch`]: the base branch now
/// includes the task branch, via a fast-forward or a 3-way merge commit, and
/// points at `merged_sha`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The commit the base ref now points at: either the task branch tip (a
    /// fast-forward) or the new 2-parent merge commit (a diverged 3-way merge).
    pub merged_sha: String,
    /// The base branch that was advanced (a bare branch name, e.g. `main`).
    pub base_ref: String,
    /// The task branch that was merged (`maestro/<task_id>`).
    pub branch: String,
    /// `true` if the base was fast-forwarded (base was an ancestor of the task
    /// tip); `false` if the base had diverged and a conflict-free 3-way merge
    /// commit was created instead.
    pub fast_forward: bool,
}

/// Run `git` scoped to `repo`, returning trimmed stdout on success. Uses the same
/// combined-output error as [`git`]. Convenience wrapper for the merge helpers.
fn git_c(repo: &str, args: &[&str]) -> Result<String> {
    let mut full = vec!["-C", repo];
    full.extend_from_slice(args);
    git(&full).map(|s| s.trim().to_string())
}

/// `true` iff `git -C repo <args>` exits 0. Used for boolean git probes
/// (`merge-base --is-ancestor`, `rev-parse --verify`) where the exit code, not
/// the output, is the answer.
fn git_ok(repo: &str, args: &[&str]) -> bool {
    let mut full = vec!["-C", repo];
    full.extend_from_slice(args);
    Command::new("git")
        .args(&full)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Merge the passed task's branch `maestro/<task_id>` into `base_ref` (ADR-006,
/// advisor-initiated `merge_task`), preferring a fast-forward but falling back to
/// a conflict-free 3-way merge when the base has diverged. This NEVER disturbs a
/// working tree it does not have to, and never mutates a ref on a conflict/error.
///
/// The branch was cut off `base_ref` and only added commits. Steps:
/// 1. resolve `task_sha` from `refs/heads/maestro/<task_id>` (missing → error);
/// 2. require `base_ref` to name an existing LOCAL branch (a SHA / tag / missing
///    branch → error, "merge manually");
/// 3. branch on divergence:
///    - **base is an ancestor of the task tip (fast-forward)**: advance the base
///      to `task_sha`. If `base_ref` is the checked-out branch, require a clean
///      working tree and `merge --ff-only`; otherwise a compare-and-swap
///      `update-ref refs/heads/<base_ref> <task_sha> <base_sha>` (no working tree
///      touched). `fast_forward = true`.
///    - **diverged**: compute the merge in memory with
///      `git merge-tree --write-tree <base_sha> <task_sha>`. On a CONFLICT
///      (exit 1) → error listing the conflicted paths, NO ref mutated. On a CLEAN
///      merge (exit 0) → create a real 2-parent merge commit: if `base_ref` is
///      checked out, `merge --no-ff` (requiring a clean tree); otherwise
///      `commit-tree <merged_tree> -p <base_sha> -p <task_sha>` + a
///      compare-and-swap `update-ref`. `fast_forward = false`.
///
/// On success returns the [`MergeOutcome`]; the caller emits `merged`, removes
/// the worktree, and best-effort deletes the task branch.
pub fn merge_task_branch(repo_path: &Path, base_ref: &str, task_id: &str) -> Result<MergeOutcome> {
    let repo = repo_path.to_str().context("repo path is not valid UTF-8")?;
    let branch = format!("maestro/{task_id}");
    let branch_ref = format!("refs/heads/{branch}");

    // 1. Resolve the task branch tip.
    let task_sha = git_c(repo, &["rev-parse", "--verify", "--quiet", &branch_ref])
        .with_context(|| format!("task branch {branch} is missing; nothing to merge"))?;

    // 2. Require base_ref to be a local branch (not a SHA / tag / detached).
    let base_branch_ref = format!("refs/heads/{base_ref}");
    if !git_ok(repo, &["rev-parse", "--verify", "--quiet", &base_branch_ref]) {
        bail!("base_ref '{base_ref}' is not a local branch; merge manually");
    }
    let base_sha = git_c(repo, &["rev-parse", "--verify", &base_branch_ref])?;

    // Is base_ref the currently checked-out branch? Shared by both paths.
    let checked_out = git_c(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|h| h == base_ref)
        .is_some();

    // 3a. Fast-forward path: base is an ancestor of the task tip.
    if git_ok(repo, &["merge-base", "--is-ancestor", &base_sha, &task_sha]) {
        if checked_out {
            let status = git_c(repo, &["status", "--porcelain"])?;
            if !status.is_empty() {
                bail!(
                    "base branch '{base_ref}' is checked out with a dirty working tree; \
                     commit/stash then merge"
                );
            }
            git_c(repo, &["merge", "--ff-only", &branch_ref])
                .with_context(|| format!("fast-forwarding {base_ref} to {branch}"))?;
        } else {
            // Compare-and-swap: the old-value arg makes this safe against races.
            git_c(repo, &["update-ref", &base_branch_ref, &task_sha, &base_sha])
                .with_context(|| format!("fast-forwarding ref {base_ref} to {branch}"))?;
        }

        return Ok(MergeOutcome {
            merged_sha: task_sha,
            base_ref: base_ref.to_string(),
            branch,
            fast_forward: true,
        });
    }

    // 3b. Diverged path: attempt a conflict-free 3-way merge.
    //
    // Compute the merge in memory (pure — mutates no ref/worktree). git 2.x
    // `merge-tree --write-tree`: exit 0 → clean, stdout's FIRST line is the
    // merged tree OID; exit 1 → conflicts, stdout lists conflicted paths; exit
    // >1 → a real error. We need both the exit code and stdout, so use a raw
    // Command (git_c would discard the code and bail on any non-zero exit).
    let mt = Command::new("git")
        .args(["-C", repo, "merge-tree", "--write-tree", &base_sha, &task_sha])
        .output()
        .with_context(|| "spawning `git merge-tree --write-tree`")?;
    let mt_stdout = String::from_utf8_lossy(&mt.stdout);
    let code = mt.status.code();

    match code {
        Some(0) => {
            // Clean merge. The merged tree OID is the first line of stdout.
            let merged_tree = mt_stdout
                .lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .context("merge-tree reported a clean merge but wrote no tree OID")?
                .to_string();

            let merge_msg = format!("maestro: merge {branch} into {base_ref}");
            let merged_sha = if checked_out {
                // Real merge in the working tree (recomputes the same clean
                // merge and updates tree+ref+commit natively). Require clean.
                let status = git_c(repo, &["status", "--porcelain"])?;
                if !status.is_empty() {
                    bail!(
                        "base branch '{base_ref}' is checked out with a dirty working tree; \
                         commit/stash then merge"
                    );
                }
                git_c(repo, &["merge", "--no-ff", "--no-edit", &branch_ref])
                    .with_context(|| format!("3-way merging {branch} into {base_ref}"))?;
                git_c(repo, &["rev-parse", "HEAD"])?
            } else {
                // Build the merge commit off the merged tree with two parents
                // (base then task), then advance the ref race-safely (CAS).
                let commit = git_c(
                    repo,
                    &[
                        "commit-tree",
                        &merged_tree,
                        "-p",
                        &base_sha,
                        "-p",
                        &task_sha,
                        "-m",
                        &merge_msg,
                    ],
                )
                .with_context(|| format!("creating merge commit for {branch} into {base_ref}"))?;
                git_c(repo, &["update-ref", &base_branch_ref, &commit, &base_sha])
                    .with_context(|| format!("advancing ref {base_ref} to merge commit"))?;
                commit
            };

            Ok(MergeOutcome {
                merged_sha,
                base_ref: base_ref.to_string(),
                branch,
                fast_forward: false,
            })
        }
        Some(1) => {
            // Conflicts: stdout lists the conflicted paths. No ref was mutated.
            let paths = mt_stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "base '{base_ref}' has advanced and merging {branch} conflicts — \
                 resolve manually (conflicted paths: {paths})"
            );
        }
        _ => {
            let stderr = String::from_utf8_lossy(&mt.stderr);
            bail!(
                "`git merge-tree` failed ({}): {}{}",
                mt.status,
                mt_stdout,
                stderr
            );
        }
    }
}

/// Best-effort deletion of a task's branch `maestro/<task_id>` (`git branch -D`).
/// Errors are swallowed: post-merge branch cleanup is never load-bearing.
pub fn delete_branch(repo_path: &Path, task_id: &str) {
    if let Some(repo) = repo_path.to_str() {
        let branch = format!("maestro/{task_id}");
        let _ = Command::new("git")
            .args(["-C", repo, "branch", "-D", &branch])
            .output();
    }
}

/// Best-effort removal of a worktree (`git worktree remove --force`). Errors are
/// swallowed: cleanup is never allowed to fail a task.
pub fn remove(repo_path: &Path, worktree: &Path) {
    if let (Some(repo), Some(wt)) = (repo_path.to_str(), worktree.to_str()) {
        let _ = Command::new("git")
            .args(["-C", repo, "worktree", "remove", "--force", wt])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path().to_str().unwrap();
        git(&["-C", p, "init", "-q", "-b", "main"]).unwrap();
        git(&["-C", p, "config", "user.email", "t@t"]).unwrap();
        git(&["-C", p, "config", "user.name", "t"]).unwrap();
        std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        git(&["-C", p, "add", "-A"]).unwrap();
        git(&["-C", p, "commit", "-q", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn changed_files_reports_new_and_edited() {
        let repo = init_repo();
        // Manually add a worktree to avoid depending on state_dir env here.
        let wt = TempDir::new().unwrap();
        let rp = repo.path().to_str().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rp, "worktree", "add", wtp, "-b", "maestro/x", "HEAD"]).unwrap();
        std::fs::write(wt.path().join("new.txt"), "x\n").unwrap();
        let changed = changed_files(wt.path(), "HEAD").unwrap();
        assert_eq!(changed, vec!["new.txt".to_string()]);
        remove(repo.path(), wt.path());
    }

    // L15: `is_live_worktree` (the reuse guard) is TRUE for a live worktree and
    // FALSE for a non-existent path or one that git no longer tracks. This is the
    // predicate the fix-in-place retry uses to decide reuse vs a fresh cut.
    #[test]
    fn is_live_worktree_true_for_live_false_for_absent_or_removed() {
        let repo = init_repo();
        let rp = repo.path().to_str().unwrap();
        let wt = TempDir::new().unwrap();
        let wtp = wt.path().to_str().unwrap();

        // A non-existent path → not reusable.
        assert!(
            !is_live_worktree(Path::new("/no/such/worktree/path")),
            "absent path is not a live worktree"
        );

        // A live worktree with the worker's edits intact → reusable.
        git(&["-C", rp, "worktree", "add", wtp, "-b", "maestro/reuse", "HEAD"]).unwrap();
        std::fs::write(wt.path().join("edit.rs"), "// worker edits\n").unwrap();
        assert!(
            is_live_worktree(wt.path()),
            "a live worktree is reusable in place"
        );

        // After git removes the worktree admin entry, the dir is no longer a live
        // worktree → the caller falls back to a fresh cut.
        remove(repo.path(), wt.path());
        assert!(
            !is_live_worktree(wt.path()),
            "a removed worktree is not reusable"
        );
    }

    #[test]
    fn commit_all_is_noop_when_clean() {
        let repo = init_repo();
        let wt = TempDir::new().unwrap();
        let rp = repo.path().to_str().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rp, "worktree", "add", wtp, "-b", "maestro/y", "HEAD"]).unwrap();
        assert!(!commit_all(wt.path(), "noop").unwrap(), "clean tree = no commit");
        std::fs::write(wt.path().join("a.txt"), "1\n").unwrap();
        assert!(commit_all(wt.path(), "add a").unwrap(), "dirty tree = commit");
        remove(repo.path(), wt.path());
    }

    #[test]
    fn resolve_base_ref_symbolic_and_concrete() {
        let repo = init_repo();
        let rp = repo.path();
        // "HEAD" resolves to the current branch (`main`).
        assert_eq!(resolve_base_ref(rp, "HEAD"), "main", "HEAD → current branch");
        // An existing branch name is returned unchanged.
        assert_eq!(resolve_base_ref(rp, "main"), "main", "existing branch unchanged");
        // A raw commit SHA (valid object, not a branch) is returned unchanged.
        let sha = head_of(rp, "main");
        assert_eq!(resolve_base_ref(rp, &sha), sha, "SHA unchanged");
        // A bogus ref is returned unchanged (never panics).
        assert_eq!(resolve_base_ref(rp, "no-such-ref"), "no-such-ref", "bogus unchanged");
    }

    #[test]
    fn resolve_to_sha_peels_refs_and_returns_none_for_bogus() {
        let repo = init_repo();
        let rp = repo.path();
        let head = head_of(rp, "main");
        // A branch name resolves to its concrete commit SHA.
        assert_eq!(resolve_to_sha(rp, "main").as_deref(), Some(head.as_str()));
        // "HEAD" resolves to the same commit.
        assert_eq!(resolve_to_sha(rp, "HEAD").as_deref(), Some(head.as_str()));
        // A raw SHA resolves to itself.
        assert_eq!(resolve_to_sha(rp, &head).as_deref(), Some(head.as_str()));
        // A bogus ref → None (never panics).
        assert_eq!(resolve_to_sha(rp, "no-such-ref"), None);
    }

    /// L3 regression: a task's scope diff must be taken against the commit its
    /// worktree was cut from (the pinned base), NOT the live base ref. If the base
    /// branch advances while the task runs — e.g. a sibling task merges into it —
    /// the just-merged files appear as DELETIONS in a diff vs the advanced tip,
    /// which (being outside the task's allowlist) would be a spurious
    /// `scope_violation`. Pinning the base to the cut-from SHA prevents this.
    #[test]
    fn pinned_base_scope_diff_survives_base_advance() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();

        // Pin the base to the concrete commit the worktree will be cut from.
        let pinned = resolve_to_sha(rp, "main").expect("main resolves to a SHA");

        // Cut the task worktree off `main` (== the pinned commit right now).
        let wt = TempDir::new().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rps, "worktree", "add", wtp, "-b", "maestro/l3", "main"]).unwrap();
        // The task writes its ONE allowlisted file.
        std::fs::write(wt.path().join("task.rs"), "// task\n").unwrap();

        // A SIBLING task merges into `main` mid-flight: advance the base branch on
        // a DIFFERENT file, outside this task's worktree.
        std::fs::write(rp.join("sibling.rs"), "// sibling\n").unwrap();
        git(&["-C", rps, "add", "-A"]).unwrap();
        git(&["-C", rps, "commit", "-q", "-m", "sibling merged into main"]).unwrap();

        // Diffing against the LIVE base ref now spuriously reports `sibling.rs` as
        // a deletion (it exists on the advanced tip but not in the worktree cut
        // from the older commit) — this is the bug.
        let live = changed_files(wt.path(), "main").unwrap();
        assert!(
            live.contains(&"sibling.rs".to_string()),
            "live base diff spuriously includes the sibling file: {live:?}"
        );

        // Diffing against the PINNED base (the cut-from commit) reports ONLY the
        // task's own file — no spurious sibling deletion. This is the fix.
        let pinned_diff = changed_files(wt.path(), &pinned).unwrap();
        assert_eq!(
            pinned_diff,
            vec!["task.rs".to_string()],
            "pinned base diff is exactly the task's own change: {pinned_diff:?}"
        );
        assert!(
            !pinned_diff.contains(&"sibling.rs".to_string()),
            "pinned base diff must NOT include the sibling file"
        );

        remove(rp, wt.path());
    }

    #[test]
    fn commit_and_diff_paths_restrict_to_given_files() {
        let repo = init_repo();
        let wt = TempDir::new().unwrap();
        let rp = repo.path().to_str().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rp, "worktree", "add", wtp, "-b", "maestro/p", "HEAD"]).unwrap();
        // An in-allowlist file plus a stray (out-of-allowlist) file.
        std::fs::write(wt.path().join("allowed.rs"), "//\n").unwrap();
        std::fs::write(wt.path().join("stray.txt"), "x\n").unwrap();

        // diff_paths shows ONLY the allowlisted file.
        let d = diff_paths(wt.path(), "HEAD", &["allowed.rs".to_string()]).unwrap();
        assert!(d.contains("allowed.rs"), "diff includes allowed.rs");
        assert!(!d.contains("stray.txt"), "diff excludes stray.txt");
        // Empty paths → empty diff.
        assert!(diff_paths(wt.path(), "HEAD", &[]).unwrap().is_empty());

        // commit_paths commits ONLY the allowlisted file.
        assert!(commit_paths(wt.path(), &["allowed.rs".to_string()], "add allowed").unwrap());
        let files = git(&["-C", wtp, "show", "--name-only", "--pretty=format:", "HEAD"]).unwrap();
        assert!(files.contains("allowed.rs"), "committed allowed.rs, got: {files}");
        assert!(!files.contains("stray.txt"), "did NOT commit stray.txt, got: {files}");
        // Empty paths → nothing staged → no commit.
        assert!(!commit_paths(wt.path(), &[], "noop").unwrap(), "empty paths = no commit");
        remove(repo.path(), wt.path());
    }

    /// Add a `maestro/<task_id>` branch off `HEAD` with one extra commit, without
    /// leaving a worktree attached. Returns the task branch's tip SHA.
    fn add_task_branch(repo: &Path, task_id: &str) -> String {
        let rp = repo.to_str().unwrap();
        let branch = format!("maestro/{task_id}");
        let wt = TempDir::new().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rp, "worktree", "add", wtp, "-b", &branch, "HEAD"]).unwrap();
        std::fs::write(wt.path().join("feature.txt"), "feature\n").unwrap();
        git(&["-C", wtp, "add", "-A"]).unwrap();
        git(&["-C", wtp, "commit", "-q", "-m", "feature"]).unwrap();
        let tip = git(&["-C", rp, "rev-parse", &format!("refs/heads/{branch}")])
            .unwrap()
            .trim()
            .to_string();
        // Detach the worktree so the branch is not checked out anywhere.
        remove(repo, wt.path());
        tip
    }

    fn head_of(repo: &Path, r: &str) -> String {
        git(&["-C", repo.to_str().unwrap(), "rev-parse", r])
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn merge_ff_when_base_not_checked_out() {
        let repo = init_repo();
        let rp = repo.path();
        let task_tip = add_task_branch(rp, "ff1");
        // Detach HEAD so `main` is NOT the checked-out branch → update-ref path.
        let main_sha = head_of(rp, "main");
        git(&["-C", rp.to_str().unwrap(), "checkout", "-q", "--detach", &main_sha]).unwrap();

        let out = merge_task_branch(rp, "main", "ff1").expect("ff merge succeeds");
        assert_eq!(out.merged_sha, task_tip);
        assert_eq!(out.base_ref, "main");
        assert_eq!(out.branch, "maestro/ff1");
        assert!(out.fast_forward, "ancestor base → fast-forward");
        // `main` now points at the task commit; the working tree was untouched.
        assert_eq!(head_of(rp, "main"), task_tip, "main advanced to task tip");
    }

    /// Add a `maestro/<task_id>` branch off `HEAD` that edits (creates) `file`
    /// with `content`, without leaving a worktree attached. Returns the tip SHA.
    fn add_task_branch_writing(repo: &Path, task_id: &str, file: &str, content: &str) -> String {
        let rp = repo.to_str().unwrap();
        let branch = format!("maestro/{task_id}");
        let wt = TempDir::new().unwrap();
        let wtp = wt.path().to_str().unwrap();
        git(&["-C", rp, "worktree", "add", wtp, "-b", &branch, "HEAD"]).unwrap();
        std::fs::write(wt.path().join(file), content).unwrap();
        git(&["-C", wtp, "add", "-A"]).unwrap();
        git(&["-C", wtp, "commit", "-q", "-m", &format!("task {task_id}")]).unwrap();
        let tip = git(&["-C", rp, "rev-parse", &format!("refs/heads/{branch}")])
            .unwrap()
            .trim()
            .to_string();
        remove(repo, wt.path());
        tip
    }

    /// Commit `content` to `file` directly on the checked-out `main`, advancing it.
    fn advance_main(repo: &Path, file: &str, content: &str) {
        let rp = repo.to_str().unwrap();
        std::fs::write(repo.join(file), content).unwrap();
        git(&["-C", rp, "add", "-A"]).unwrap();
        git(&["-C", rp, "commit", "-q", "-m", &format!("advance main: {file}")]).unwrap();
    }

    /// The number of parents of `rev` (2 ⇒ a merge commit).
    fn parent_count(repo: &Path, rev: &str) -> usize {
        let out = git(&["-C", repo.to_str().unwrap(), "rev-list", "--parents", "-n1", rev]).unwrap();
        // Output: "<commit> <parent1> <parent2> ..." → parents = tokens - 1.
        out.split_whitespace().count().saturating_sub(1)
    }

    /// The file content at `rev:path`, or None if the path is absent there.
    fn file_at(repo: &Path, rev: &str, path: &str) -> Option<String> {
        git(&["-C", repo.to_str().unwrap(), "show", &format!("{rev}:{path}")]).ok()
    }

    #[test]
    fn merge_diverged_conflict_free_base_not_checked_out() {
        let repo = init_repo();
        let rp = repo.path();
        // Task branch edits a.txt off the initial commit.
        add_task_branch_writing(rp, "d1", "a.txt", "A\n");
        // Advance main on a DIFFERENT file so it diverges without conflict.
        advance_main(rp, "b.txt", "B\n");
        let main_before = head_of(rp, "main");
        // Detach HEAD so main is NOT checked out → commit-tree + update-ref path.
        git(&["-C", rp.to_str().unwrap(), "checkout", "-q", "--detach", &main_before]).unwrap();

        let out = merge_task_branch(rp, "main", "d1").expect("conflict-free 3-way merge succeeds");
        assert!(!out.fast_forward, "diverged base → not a fast-forward");
        let tip = head_of(rp, "main");
        assert_eq!(out.merged_sha, tip, "outcome sha is main's new tip");
        assert_ne!(tip, main_before, "main advanced");
        assert_eq!(parent_count(rp, "main"), 2, "new tip is a 2-parent merge commit");
        // Both changes are present in the merged tree.
        assert_eq!(file_at(rp, "main", "a.txt").as_deref(), Some("A\n"), "task change present");
        assert_eq!(file_at(rp, "main", "b.txt").as_deref(), Some("B\n"), "base change present");
    }

    #[test]
    fn merge_diverged_conflict_free_base_checked_out() {
        let repo = init_repo();
        let rp = repo.path();
        add_task_branch_writing(rp, "d2", "a.txt", "A\n");
        // main stays checked out and clean → the `merge --no-ff` path.
        advance_main(rp, "b.txt", "B\n");
        let main_before = head_of(rp, "main");

        let out = merge_task_branch(rp, "main", "d2").expect("conflict-free 3-way merge succeeds");
        assert!(!out.fast_forward, "diverged base → not a fast-forward");
        let tip = head_of(rp, "main");
        assert_eq!(out.merged_sha, tip);
        assert_ne!(tip, main_before, "main advanced");
        assert_eq!(parent_count(rp, "main"), 2, "new tip is a 2-parent merge commit");
        assert_eq!(file_at(rp, "main", "a.txt").as_deref(), Some("A\n"), "task change present");
        assert_eq!(file_at(rp, "main", "b.txt").as_deref(), Some("B\n"), "base change present");
    }

    #[test]
    fn merge_diverged_with_conflict_is_refused_and_leaves_base_unchanged() {
        let repo = init_repo();
        let rp = repo.path();
        // Task and base both edit the SAME file incompatibly → real conflict.
        add_task_branch_writing(rp, "d3", "clash.txt", "task side\n");
        advance_main(rp, "clash.txt", "base side\n");
        let main_before = head_of(rp, "main");

        let err = merge_task_branch(rp, "main", "d3").expect_err("conflicting 3-way merge refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("conflict"), "error mentions conflict, got: {msg}");
        // No ref was mutated on the conflict path.
        assert_eq!(head_of(rp, "main"), main_before, "main unchanged after conflict refusal");
    }

    #[test]
    fn merge_diverged_but_clean_now_3way_merges_instead_of_refusing() {
        // Regression rename of the former `merge_refused_when_not_fast_forward`:
        // a divergence on a DIFFERENT file used to be refused ("not a
        // fast-forward"); it now succeeds via a conflict-free 3-way merge.
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();
        // Task branch adds feature.txt off the initial commit.
        let task_tip = add_task_branch(rp, "nf1");
        // Advance `main` on a DIFFERENT file so it diverges without conflict.
        std::fs::write(rp.join("other.txt"), "x\n").unwrap();
        git(&["-C", rps, "add", "-A"]).unwrap();
        git(&["-C", rps, "commit", "-q", "-m", "advance main"]).unwrap();
        let main_before = head_of(rp, "main");

        let out = merge_task_branch(rp, "main", "nf1").expect("diverged clean → 3-way merge");
        assert!(!out.fast_forward, "diverged → not a fast-forward");
        assert_ne!(head_of(rp, "main"), main_before, "main advanced past the merge commit");
        assert_eq!(parent_count(rp, "main"), 2, "new tip is a 2-parent merge commit");
        // Both the task's file and main's file are present in the merged tree.
        assert!(file_at(rp, "main", "feature.txt").is_some(), "task file present");
        assert!(file_at(rp, "main", "other.txt").is_some(), "base file present");
        // The task tip is an ancestor of the new merge commit (it was merged in).
        assert!(
            git_ok(rps, &["merge-base", "--is-ancestor", &task_tip, &head_of(rp, "main")]),
            "task tip merged into main"
        );
    }

    #[test]
    fn merge_refused_when_base_ref_is_not_a_branch() {
        let repo = init_repo();
        let rp = repo.path();
        add_task_branch(rp, "sha1");
        // Pass a raw SHA (a valid object, but not a local branch) as base_ref.
        let sha = head_of(rp, "main");
        let err = merge_task_branch(rp, &sha, "sha1").expect_err("SHA base_ref refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("not a local branch"), "got: {msg}");
    }

    #[test]
    fn merge_errors_when_task_branch_missing() {
        let repo = init_repo();
        let err = merge_task_branch(repo.path(), "main", "absent").expect_err("missing branch");
        let msg = format!("{err:#}");
        assert!(msg.contains("missing") || msg.contains("nothing to merge"), "got: {msg}");
    }

    /// All salvage refs currently under `refs/maestro/salvage/<task_id>/`.
    fn salvage_refs_for(repo: &Path, task_id: &str) -> Vec<String> {
        let rp = repo.to_str().unwrap();
        let prefix = format!("refs/maestro/salvage/{task_id}/");
        git(&["-C", rp, "for-each-ref", "--format=%(refname)", &prefix])
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// The commit a ref points at (full sha), or None if the ref is absent.
    fn ref_sha(repo: &Path, r: &str) -> Option<String> {
        let rp = repo.to_str().unwrap();
        git_c(rp, &["rev-parse", "--verify", "--quiet", r]).ok()
    }

    /// WORK-LOSS GUARD: a `maestro/<task_id>` branch carrying commits beyond
    /// `base_ref` (the fix-in-place checkpoint) is PRESERVED to a durable salvage
    /// ref before it is deleted, and the salvage ref carries the committed file.
    #[test]
    fn salvage_preserves_a_task_branch_with_commits_beyond_base() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();
        // A task branch with one committed file beyond `main` (the checkpoint).
        let tip = add_task_branch_writing(rp, "salv1", "impl.rs", "// near-complete\n");

        salvage_task_branch(rps, "main", "salv1", "maestro/salv1");

        let refs = salvage_refs_for(rp, "salv1");
        assert_eq!(refs.len(), 1, "exactly one salvage ref, got {refs:?}");
        assert_eq!(
            ref_sha(rp, &refs[0]).as_deref(),
            Some(tip.as_str()),
            "salvage ref points at the checkpoint tip"
        );
        // The committed file is recoverable through the salvage ref.
        let show = git(&["-C", rps, "show", &format!("{}:impl.rs", refs[0])]).unwrap();
        assert!(show.contains("near-complete"), "salvage ref carries impl.rs, got: {show}");

        // Now the (real) force-delete can proceed and the work is still recoverable.
        let _ = git(&["-C", rps, "branch", "-D", "maestro/salv1"]);
        assert!(
            ref_sha(rp, &refs[0]).is_some(),
            "salvage survives the branch delete"
        );
    }

    /// A branch with NO commits beyond base (e.g. an escalation from repeated
    /// verifier failures, no `checks_failed` checkpoint) is NOT salvaged — there is
    /// nothing committed to lose, and we must not spuriously anchor base itself.
    #[test]
    fn salvage_is_noop_when_branch_has_no_commits_beyond_base() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();
        // A task branch cut at `main` with NO extra commit.
        git(&["-C", rps, "branch", "maestro/nowork", "main"]).unwrap();

        salvage_task_branch(rps, "main", "nowork", "maestro/nowork");

        assert!(
            salvage_refs_for(rp, "nowork").is_empty(),
            "no salvage ref for a branch at base"
        );
    }

    /// An absent branch (the common first-attempt / fresh-cut case) is a silent
    /// no-op — nothing to salvage.
    #[test]
    fn salvage_is_noop_when_branch_absent() {
        let repo = init_repo();
        let rp = repo.path();
        salvage_task_branch(rp.to_str().unwrap(), "main", "ghost", "maestro/ghost");
        assert!(salvage_refs_for(rp, "ghost").is_empty(), "no salvage ref for an absent branch");
    }

    /// Edge: `base_ref` is a raw SHA (advanced usage — worktree cut off a pinned
    /// commit, not a branch). The `base..branch` range still resolves, so the
    /// checkpoint is salvaged exactly as with a branch base.
    #[test]
    fn salvage_works_with_base_ref_as_a_sha() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();
        let base_sha = head_of(rp, "main");
        let tip = add_task_branch_writing(rp, "shabase", "impl.rs", "// x\n");

        salvage_task_branch(rps, &base_sha, "shabase", "maestro/shabase");

        let refs = salvage_refs_for(rp, "shabase");
        assert_eq!(refs.len(), 1, "salvaged with a SHA base, got {refs:?}");
        assert_eq!(ref_sha(rp, &refs[0]).as_deref(), Some(tip.as_str()));
    }

    /// Repeated escalations of the SAME task must not clobber each other's salvage:
    /// each distinct checkpoint tip lands at its own `refs/.../<short_sha>` ref.
    #[test]
    fn repeated_escalations_produce_distinct_salvage_refs() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();

        // Escalation 1: a checkpoint with content A.
        let tip1 = add_task_branch_writing(rp, "multi", "impl.rs", "// A\n");
        salvage_task_branch(rps, "main", "multi", "maestro/multi");
        // (create would delete the branch here; simulate that.)
        git(&["-C", rps, "branch", "-D", "maestro/multi"]).unwrap();

        // Escalation 2: a DIFFERENT checkpoint (different content → different sha).
        let tip2 = add_task_branch_writing(rp, "multi", "impl.rs", "// B different\n");
        assert_ne!(tip1, tip2, "the two checkpoints have distinct tips");
        salvage_task_branch(rps, "main", "multi", "maestro/multi");

        let refs = salvage_refs_for(rp, "multi");
        assert_eq!(refs.len(), 2, "two escalations → two distinct salvage refs, got {refs:?}");
        let salvaged: std::collections::HashSet<String> =
            refs.iter().filter_map(|r| ref_sha(rp, r)).collect();
        assert!(salvaged.contains(&tip1), "first checkpoint preserved");
        assert!(salvaged.contains(&tip2), "second checkpoint preserved");
    }
}
