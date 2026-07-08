//! maestro-implementer — the implementer backend abstraction (M1).
//!
//! A backend takes a resolved [`ImplementerTask`] (spec + worktree + house
//! rules + model) and performs the file edits inside the worktree, returning an
//! [`ImplementerOutcome`]. The daemon's mechanical gate (ADR-002) then judges
//! the result — backends never self-report success, so the outcome is
//! *descriptive*, not authoritative.
//!
//! Two backends are provided:
//! - [`MockBackend`] — deterministic, driven by JSON in the spec's
//!   `instructions`. Selected when `task.model == "mock"`. Used to exercise the
//!   M1 pipeline end-to-end without an API key.
//! - [`AnthropicBackend`] — a real Anthropic Messages API client (`ureq`) that
//!   runs a one-shot tool-use loop with a `write_file` tool. Wired to the
//!   documented wire shape but never exercised live in CI.
//!
//! Neither backend enforces the spec's `file_allowlist`: that is the daemon's
//! mechanical gate's job (ADR-002). Both write whatever the plan says; the
//! only path guard is that writes must stay inside the worktree.

mod anthropic;
mod mock;
mod verifier;

pub use anthropic::{build_request_body, AnthropicBackend};
pub use mock::MockBackend;
pub use verifier::{
    build_verify_request_body, AnthropicVerifier, MockVerifier, NoCommandRunner, VerifierBackend,
    VerifierCommandRun, VerifierCommandRunner, VerifyOutcome, VerifyTask,
};

use std::path::{Path, PathBuf};

use thiserror::Error;

/// A fully-resolved unit of implementation work handed to a backend.
pub struct ImplementerTask {
    /// The immutable spec (ADR-003).
    pub spec: maestro_journal::spec::TaskSpec,
    /// The worktree edits happen in; all written paths are relative to it.
    pub worktree: PathBuf,
    /// House rules injected verbatim into the system prompt.
    pub house_rules: String,
    /// The model id, e.g. `"claude-sonnet-4-6"` or `"mock"`.
    pub model: String,
    /// The `X-Maestro-Task` header value sent on each upstream request when the
    /// backend is routed through the daemon's streaming credential proxy (ADR-006).
    /// `None` on the default (proxy-off) path — no header is sent. This is a
    /// transport concern only; it never enters `build_request_body`.
    pub task_header: Option<String>,
}

/// A *descriptive*, non-authoritative account of what a backend did.
///
/// Per ADR-002 the implementer never self-reports success; the mechanical gate
/// and verifier judge the result. These fields exist for telemetry and to let
/// the daemon locate the writes, not to assert correctness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementerOutcome {
    /// Worktree-relative paths the backend wrote.
    pub files_written: Vec<String>,
    /// Number of model turns taken (1 for the mock).
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// A brief, non-authoritative note describing what happened.
    pub notes: String,
}

/// Errors a backend can return. The daemon maps these onto its failure
/// taxonomy (e.g. [`ImplementerError::Unavailable`] → `model_unavailable`).
#[derive(Debug, Error)]
pub enum ImplementerError {
    /// No credentials, or the model is not reachable. Daemon maps to
    /// `model_unavailable`.
    #[error("model unavailable: {0}")]
    Unavailable(String),
    /// An HTTP transport error or non-2xx status from the API.
    #[error("http error: {0}")]
    Http(String),
    /// The API returned an unexpected/unparseable response shape.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A filesystem error while applying a write.
    #[error("io error: {0}")]
    Io(String),
    /// The turn budget was exhausted before the model finished.
    #[error("budget exhausted: {0}")]
    Budget(String),
}

/// The abstraction every implementer backend satisfies.
pub trait ImplementerBackend {
    /// Perform the edits described by `task` inside `task.worktree`.
    fn run(&self, task: &ImplementerTask) -> Result<ImplementerOutcome, ImplementerError>;
}

/// Join `rel_path` onto `worktree`, create parent directories, and write
/// `content` — refusing any path that is absolute or escapes the worktree.
///
/// The escape guard canonicalizes the *parent* directory (after creating it)
/// and checks that it is still under the canonicalized worktree, which defends
/// against `..` traversal and symlink tricks. The daemon's allowlist check is a
/// separate, post-hoc concern (ADR-002) — this function does not consult it.
pub fn write_within_worktree(
    worktree: &Path,
    rel_path: &str,
    content: &str,
) -> Result<(), ImplementerError> {
    let rel = Path::new(rel_path);
    if rel.is_absolute() {
        return Err(ImplementerError::Io(format!(
            "refusing absolute path: {rel_path}"
        )));
    }
    // Reject any explicit parent-dir components up front. Even without this,
    // the canonicalized-parent check below would catch a true escape, but this
    // gives a precise error and rejects `..` even when it would resolve back
    // inside the tree (e.g. `a/../b`), keeping written paths literal.
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ImplementerError::Io(format!(
            "refusing path with parent-dir (`..`) component: {rel_path}"
        )));
    }

    let worktree_canon = worktree.canonicalize().map_err(|e| {
        ImplementerError::Io(format!(
            "cannot canonicalize worktree {}: {e}",
            worktree.display()
        ))
    })?;

    let target = worktree_canon.join(rel);
    let parent = target.parent().ok_or_else(|| {
        ImplementerError::Io(format!("path has no parent directory: {rel_path}"))
    })?;

    std::fs::create_dir_all(parent).map_err(|e| {
        ImplementerError::Io(format!("cannot create {}: {e}", parent.display()))
    })?;

    // Canonicalize the (now-existing) parent and confirm containment. This is
    // the real escape guard: it resolves any symlinks in the path.
    let parent_canon = parent.canonicalize().map_err(|e| {
        ImplementerError::Io(format!("cannot canonicalize {}: {e}", parent.display()))
    })?;
    if !parent_canon.starts_with(&worktree_canon) {
        return Err(ImplementerError::Io(format!(
            "refusing path escaping the worktree: {rel_path}"
        )));
    }

    // Recompose the final target under the canonical parent so the file name is
    // written into the verified directory.
    let file_name = target
        .file_name()
        .ok_or_else(|| ImplementerError::Io(format!("path has no file name: {rel_path}")))?;
    let final_target = parent_canon.join(file_name);

    std::fs::write(&final_target, content).map_err(|e| {
        ImplementerError::Io(format!("cannot write {}: {e}", final_target.display()))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_a_file_and_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        write_within_worktree(dir.path(), "src/lib.rs", "pub fn x(){}").unwrap();
        let got = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        assert_eq!(got, "pub fn x(){}");
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = TempDir::new().unwrap();
        let err = write_within_worktree(dir.path(), "/etc/passwd", "x").unwrap_err();
        assert!(matches!(err, ImplementerError::Io(_)), "got {err:?}");
    }

    #[test]
    fn rejects_parent_dir_escape_and_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let err = write_within_worktree(dir.path(), "../escape.txt", "boom").unwrap_err();
        assert!(matches!(err, ImplementerError::Io(_)), "got {err:?}");
        // Nothing was written outside the worktree.
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn in_tree_out_of_allowlist_path_is_written() {
        // The backend does not enforce the allowlist (ADR-002); a plain in-tree
        // path like `secrets.rs` is written — the gate rejects it later.
        let dir = TempDir::new().unwrap();
        write_within_worktree(dir.path(), "secrets.rs", "const K: u8 = 1;").unwrap();
        assert!(dir.path().join("secrets.rs").exists());
    }
}
