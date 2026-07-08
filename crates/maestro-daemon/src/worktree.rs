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
}
