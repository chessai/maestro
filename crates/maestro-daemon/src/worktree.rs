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

/// Create a worktree for `task_id` off `base_ref`, on a fresh branch
/// `maestro/<task_id>`. Returns the worktree path.
pub fn create(repo_path: &Path, base_ref: &str, task_id: &str) -> Result<PathBuf> {
    let root = worktrees_root();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating worktrees root {}", root.display()))?;
    let wt = root.join(task_id);
    let repo = repo_path.to_str().context("repo path is not valid UTF-8")?;
    let branch = format!("maestro/{task_id}");

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

/// The result of a successful [`merge_task_branch`]: the base ref now points at
/// `merged_sha` (the tip of the task branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The commit the base ref was fast-forwarded to (the task branch tip).
    pub merged_sha: String,
    /// The base branch that was advanced (a bare branch name, e.g. `main`).
    pub base_ref: String,
    /// The task branch that was merged (`maestro/<task_id>`).
    pub branch: String,
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

/// Fast-forward-merge the passed task's branch `maestro/<task_id>` into
/// `base_ref` (ADR-006, advisor-initiated `merge_task`). This is fast-forward
/// ONLY and NEVER disturbs a working tree it does not have to.
///
/// The branch was cut off `base_ref` and only added commits, so it is normally a
/// fast-forward of `base_ref`. Steps:
/// 1. resolve `task_sha` from `refs/heads/maestro/<task_id>` (missing → error);
/// 2. require `base_ref` to name an existing LOCAL branch (a SHA / tag / missing
///    branch → error, "merge manually");
/// 3. fast-forward guard: `base_sha` must be an ancestor of `task_sha`
///    (not-ff → error, no merge performed);
/// 4. if `base_ref` is the checked-out branch, require a clean working tree and
///    `merge --ff-only`; otherwise advance the ref with a compare-and-swap
///    `update-ref refs/heads/<base_ref> <task_sha> <base_sha>` (no working tree
///    is touched).
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

    // 3. Fast-forward guard: base must be an ancestor of the task tip.
    if !git_ok(repo, &["merge-base", "--is-ancestor", &base_sha, &task_sha]) {
        bail!(
            "base '{base_ref}' has advanced since the task branched; \
             not a fast-forward — rebase/merge manually"
        );
    }

    // 4. Advance the base ref. If base_ref is checked out, do a real ff merge
    //    (requiring a clean tree); otherwise a working-tree-free ref update.
    let checked_out = git_c(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|h| h == base_ref)
        .is_some();

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
        git_c(
            repo,
            &["update-ref", &base_branch_ref, &task_sha, &base_sha],
        )
        .with_context(|| format!("fast-forwarding ref {base_ref} to {branch}"))?;
    }

    Ok(MergeOutcome {
        merged_sha: task_sha,
        base_ref: base_ref.to_string(),
        branch,
    })
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
        // `main` now points at the task commit; the working tree was untouched.
        assert_eq!(head_of(rp, "main"), task_tip, "main advanced to task tip");
    }

    #[test]
    fn merge_refused_when_not_fast_forward() {
        let repo = init_repo();
        let rp = repo.path();
        let rps = rp.to_str().unwrap();
        let _task_tip = add_task_branch(rp, "nf1");
        // Advance `main` with an extra commit AFTER branching so the task branch
        // is no longer a fast-forward of main.
        std::fs::write(rp.join("other.txt"), "x\n").unwrap();
        git(&["-C", rps, "add", "-A"]).unwrap();
        git(&["-C", rps, "commit", "-q", "-m", "advance main"]).unwrap();
        let main_before = head_of(rp, "main");

        let err = merge_task_branch(rp, "main", "nf1").expect_err("non-ff must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fast-forward") || msg.contains("advanced"),
            "error mentions the ff failure, got: {msg}"
        );
        // main was not moved and no merge was performed.
        assert_eq!(head_of(rp, "main"), main_before, "main unchanged after refusal");
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
}
