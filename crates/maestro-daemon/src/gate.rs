//! The mechanical gate (ADR-002). Two deterministic checks stand between an
//! implementer's edits and a passing task, and both run without spending any
//! verifier tokens:
//!
//! 1. **Scope check** — every path changed vs `base_ref` must match the spec's
//!    `file_allowlist` (a set of globs). Any out-of-allowlist path is a terminal
//!    `scope_violation` (an empty allowlist with any change is a violation).
//! 2. **Check commands** — each `check_command` is run *fresh* in the worktree
//!    via `bash -lc`, **wrapped in the task's containment recipe** (ADR-004):
//!    the gate is a verification surface and inherits the task's level, backend,
//!    and devShell. The first non-zero exit is a `checks_failed`. At L0 /
//!    `Backend::None` the wrap is identity, so the M1 behavior is unchanged.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use maestro_sandbox::SandboxSpec;

use crate::worktree;

/// The outcome of running the mechanical gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Scope check + all check commands passed. `changed` is the set of changed
    /// files that matched the allowlist, captured at the scope-check step —
    /// BEFORE the check commands ran, so it contains ONLY the implementer's
    /// clean edits and never post-build artifacts (e.g. `target/`) that a check
    /// command may have created. The caller restricts the verifier diff + the
    /// task-branch commit to exactly these paths (see `worktree::diff_paths` /
    /// `commit_paths`).
    Passed { changed: Vec<String> },
    /// One or more changed paths fell outside the allowlist (terminal).
    ScopeViolation { offending: Vec<String> },
    /// Every changed path was in-allowlist, but the count of changed files
    /// exceeded a tightened cap (ADR-004 / ADR-007). Terminal — a tightened
    /// blast-radius bound applied on a containment downgrade / codex_tighten.
    TightenedScopeExceeded {
        /// The number of changed files.
        changed: usize,
        /// The tightened cap (`ceil(allowlist_factor × allowlist_len)`, min 1).
        cap: usize,
        /// The changed paths (capped to a reasonable number for the payload).
        files: Vec<String>,
    },
    /// A check command exited non-zero.
    ChecksFailed {
        command: String,
        /// A bounded digest of the command's combined output.
        output_digest: String,
    },
}

/// Cap on the captured check-command output stored in the journal payload.
const OUTPUT_DIGEST_CAP: usize = 4000;

/// Cap on the number of changed-file paths recorded in a
/// `TightenedScopeExceeded` payload (bounds the journal write).
const TIGHTENED_FILES_CAP: usize = 50;

/// Env keys the gate removes from a check command's process for hermeticity
/// (ADR-004): the fixed set of daemon-owned `XDG_*` dirs (config/state/data/
/// runtime) plus every inherited `MAESTRO_*` var. `XDG_CACHE_HOME` is kept on
/// purpose — it aids build caching and carries no daemon config. Verify surfaces
/// that execute the same recipe should scrub identically.
fn hermetic_scrub_keys() -> Vec<String> {
    let mut keys: Vec<String> = [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (k, _) in std::env::vars() {
        if k.starts_with("MAESTRO_") {
            keys.push(k);
        }
    }
    keys
}

/// Run the mechanical gate against a worktree, running each check command under
/// the task's containment `spec` (ADR-004). A `wrap` failure (e.g. the podman
/// backend needs an image) is surfaced as an `Err` — the caller turns it into an
/// `internal_error`, never a panic.
pub fn run(
    worktree_path: &Path,
    base_ref: &str,
    file_allowlist: &[String],
    max_changed_files: Option<usize>,
    check_commands: &[String],
    spec: &SandboxSpec,
) -> Result<GateOutcome> {
    // --- 1. Scope check --------------------------------------------------
    let changed = worktree::changed_files(worktree_path, base_ref)
        .context("computing changed files for scope check")?;

    let mut builder = GlobSetBuilder::new();
    for pat in file_allowlist {
        let glob = Glob::new(pat)
            .with_context(|| format!("invalid allowlist glob {pat:?}"))?;
        builder.add(glob);
    }
    let set = builder.build().context("building allowlist glob set")?;

    // Any out-of-allowlist path is a plain ScopeViolation and wins FIRST. We
    // keep the full changed-file count + list around for the tightened-cap
    // check below, since the allowlist filter consumes `changed`.
    let changed_count = changed.len();
    let offending: Vec<String> = changed
        .iter()
        .filter(|path| !set.is_match(path))
        .cloned()
        .collect();
    if !offending.is_empty() {
        return Ok(GateOutcome::ScopeViolation { offending });
    }

    // The in-allowlist changed set, captured HERE — before check commands run,
    // so no build artifacts exist yet. Returned on the `Passed` arm so the caller
    // restricts the verifier diff + the committed branch to exactly these paths.
    // With no offending paths this is the full `changed` set.
    let in_allowlist: Vec<String> = changed
        .iter()
        .filter(|path| set.is_match(path))
        .cloned()
        .collect();

    // --- 1b. Tightened blast-radius cap (ADR-004 / ADR-007) --------------
    // All changed paths are in-allowlist. When a tightened cap is active and
    // the changed-file count exceeds it, this is a (tightened) scope violation.
    if let Some(cap) = max_changed_files {
        if changed_count > cap {
            let files: Vec<String> = changed.into_iter().take(TIGHTENED_FILES_CAP).collect();
            return Ok(GateOutcome::TightenedScopeExceeded {
                changed: changed_count,
                cap,
                files,
            });
        }
    }

    // --- 2. Check commands (fresh, in the worktree, contained) -----------
    for cmd in check_commands {
        // Wrap `bash -lc <cmd>` under the task's containment recipe. At L0 /
        // Backend::None this is identity (program="bash"); at L1+ the returned
        // program is the sandbox wrapper (e.g. `bwrap`). The cwd stays the
        // worktree (the wrapper also chdirs there at L1+, harmless overlap).
        let bash_args = vec!["-lc".to_string(), cmd.clone()];
        let wrapped = maestro_sandbox::wrap(spec, "bash", &bash_args)
            .with_context(|| format!("wrapping check command {cmd:?} for containment"))?;
        let mut command = Command::new(&wrapped.program);
        command.args(&wrapped.args).current_dir(worktree_path);
        // Gate hermeticity (ADR-004): scrub env vars that leak the DAEMON's own
        // configuration into the check process. The daemon runs with its own
        // `XDG_*` (config/state/runtime) and `MAESTRO_*` vars; a check command
        // inherits them, and when the repo under test reads XDG (e.g. maestro's
        // own tests, or any tool that honors it) its checks read the daemon's
        // config and fail non-deterministically. Removing these makes the check
        // env hermetic w.r.t. the daemon; `XDG_CACHE_HOME` is deliberately kept
        // (build-cache friendly, carries no config).
        for key in hermetic_scrub_keys() {
            command.env_remove(key);
        }
        let output = command
            .output()
            .with_context(|| format!("spawning check command {cmd:?}"))?;
        if !output.status.success() {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            let digest: String = combined.chars().take(OUTPUT_DIGEST_CAP).collect();
            return Ok(GateOutcome::ChecksFailed {
                command: cmd.clone(),
                output_digest: digest,
            });
        }
    }

    Ok(GateOutcome::Passed {
        changed: in_allowlist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_sandbox::{Backend, Level, NetworkPolicy};
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::TempDir;

    /// An L0 sandbox spec: `wrap` is identity, so the gate runs the command
    /// directly (M1/M3 behavior). Used by the gate tests that only care about
    /// scope + check-command semantics, not containment.
    fn l0_spec(workspace: &Path) -> SandboxSpec {
        SandboxSpec {
            level: Level::L0,
            backend: Backend::None,
            workspace: workspace.to_path_buf(),
            network: NetworkPolicy::Deny,
            flake_dir: None,
            devshell_variant: None,
            podman_image: None,
        }
    }

    fn init_worktree_repo() -> (TempDir, TempDir) {
        let repo = TempDir::new().unwrap();
        let rp = repo.path().to_str().unwrap();
        for args in [
            vec!["-C", rp, "init", "-q", "-b", "main"],
            vec!["-C", rp, "config", "user.email", "t@t"],
            vec!["-C", rp, "config", "user.name", "t"],
        ] {
            Command::new("git").args(&args).output().unwrap();
        }
        std::fs::write(repo.path().join("README.md"), "hi\n").unwrap();
        Command::new("git").args(["-C", rp, "add", "-A"]).output().unwrap();
        Command::new("git")
            .args(["-C", rp, "commit", "-q", "-m", "init"])
            .output()
            .unwrap();
        let wt = TempDir::new().unwrap();
        let wtp = wt.path().to_str().unwrap();
        Command::new("git")
            .args(["-C", rp, "worktree", "add", wtp, "-b", "maestro/g", "HEAD"])
            .output()
            .unwrap();
        (repo, wt)
    }

    #[test]
    fn in_allowlist_change_passes() {
        let (_repo, wt) = init_worktree_repo();
        std::fs::write(wt.path().join("src.rs"), "//\n").unwrap();
        let out = run(wt.path(), "HEAD", &["*.rs".into()], None, &[], &l0_spec(wt.path())).unwrap();
        match out {
            GateOutcome::Passed { changed } => assert_eq!(changed, vec!["src.rs".to_string()]),
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    #[test]
    fn out_of_allowlist_change_is_scope_violation() {
        let (_repo, wt) = init_worktree_repo();
        std::fs::write(wt.path().join("evil.txt"), "x\n").unwrap();
        let out = run(wt.path(), "HEAD", &["*.rs".into()], None, &[], &l0_spec(wt.path())).unwrap();
        match out {
            GateOutcome::ScopeViolation { offending } => {
                assert_eq!(offending, vec!["evil.txt".to_string()]);
            }
            other => panic!("expected ScopeViolation, got {other:?}"),
        }
    }

    #[test]
    fn empty_allowlist_with_change_is_violation() {
        let (_repo, wt) = init_worktree_repo();
        std::fs::write(wt.path().join("a.rs"), "//\n").unwrap();
        let out = run(wt.path(), "HEAD", &[], None, &[], &l0_spec(wt.path())).unwrap();
        assert!(matches!(out, GateOutcome::ScopeViolation { .. }));
    }

    #[test]
    fn failing_check_command_is_checks_failed() {
        let (_repo, wt) = init_worktree_repo();
        std::fs::write(wt.path().join("a.rs"), "//\n").unwrap();
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            None,
            &["exit 3".into()],
            &l0_spec(wt.path()),
        )
        .unwrap();
        assert!(matches!(out, GateOutcome::ChecksFailed { .. }));
    }

    // Gate hermeticity (ADR-004): the daemon's own XDG_*/MAESTRO_* env must NOT
    // leak into a check command — else e.g. maestro's own tests read the daemon's
    // config and fail. Set both in the parent; the check command passes only if
    // both are unset in the child. XDG_CACHE_HOME is intentionally preserved.
    #[test]
    fn check_command_env_is_scrubbed_of_daemon_config() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/daemon-xdg-config");
        std::env::set_var("MAESTRO_PROFILE", "leaky");
        std::env::set_var("XDG_CACHE_HOME", "/tmp/keep-cache");
        let (_repo, wt) = init_worktree_repo();
        std::fs::write(wt.path().join("a.rs"), "//\n").unwrap();
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            None,
            // Passes (exit 0) iff XDG_CONFIG_HOME + MAESTRO_PROFILE are unset AND
            // XDG_CACHE_HOME survived.
            &["[ -z \"$XDG_CONFIG_HOME\" ] && [ -z \"$MAESTRO_PROFILE\" ] && [ -n \"$XDG_CACHE_HOME\" ]"
                .into()],
            &l0_spec(wt.path()),
        )
        .unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("MAESTRO_PROFILE");
        std::env::remove_var("XDG_CACHE_HOME");
        assert!(
            matches!(out, GateOutcome::Passed { .. }),
            "daemon XDG_*/MAESTRO_* must be scrubbed (XDG_CACHE_HOME kept), got {out:?}"
        );
    }

    /// Write `n` distinct in-allowlist `*.rs` files into the worktree.
    fn write_rs_files(wt: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(wt.join(format!("f{i}.rs")), "//\n").unwrap();
        }
    }

    // (a) 3 in-allowlist files with a tightened cap of 2 → TightenedScopeExceeded.
    #[test]
    fn tightened_cap_exceeded_when_changed_over_cap() {
        let (_repo, wt) = init_worktree_repo();
        write_rs_files(wt.path(), 3);
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            Some(2),
            &[],
            &l0_spec(wt.path()),
        )
        .unwrap();
        match out {
            GateOutcome::TightenedScopeExceeded { changed, cap, files } => {
                assert_eq!(changed, 3);
                assert_eq!(cap, 2);
                assert_eq!(files.len(), 3, "all 3 in-allowlist paths recorded");
            }
            other => panic!("expected TightenedScopeExceeded, got {other:?}"),
        }
    }

    // (b) Same diff with cap 3 → NOT exceeded; proceeds to Passed (no checks).
    #[test]
    fn tightened_cap_at_boundary_passes() {
        let (_repo, wt) = init_worktree_repo();
        write_rs_files(wt.path(), 3);
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            Some(3),
            &[],
            &l0_spec(wt.path()),
        )
        .unwrap();
        assert!(
            matches!(out, GateOutcome::Passed { .. }),
            "changed == cap is within the cap, got {out:?}"
        );
    }

    // (c) None cap → never a tightened violation even with many changed files.
    #[test]
    fn tightened_cap_none_never_violates() {
        let (_repo, wt) = init_worktree_repo();
        write_rs_files(wt.path(), 5);
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            None,
            &[],
            &l0_spec(wt.path()),
        )
        .unwrap();
        assert!(
            matches!(out, GateOutcome::Passed { .. }),
            "None cap never violates, got {out:?}"
        );
    }

    // (d) An out-of-allowlist path with a tightened cap set still returns a plain
    // ScopeViolation — the allowlist check wins FIRST.
    #[test]
    fn out_of_allowlist_wins_over_tightened_cap() {
        let (_repo, wt) = init_worktree_repo();
        write_rs_files(wt.path(), 3);
        std::fs::write(wt.path().join("evil.txt"), "x\n").unwrap();
        let out = run(
            wt.path(),
            "HEAD",
            &["*.rs".into()],
            Some(2),
            &[],
            &l0_spec(wt.path()),
        )
        .unwrap();
        match out {
            GateOutcome::ScopeViolation { offending } => {
                assert_eq!(offending, vec!["evil.txt".to_string()]);
            }
            other => panic!("expected plain ScopeViolation, got {other:?}"),
        }
    }

    // AC5: at L1 with Backend::Bwrap the gate wraps `check_command` as
    // `bwrap … -- bash -lc "cargo test"`. We assert the wrapped argv shape via
    // the SAME `wrap` the gate calls (we do not spawn bwrap here).
    #[test]
    fn l1_bwrap_gate_wraps_check_command() {
        let ws = PathBuf::from("/w");
        let spec = SandboxSpec {
            level: Level::L1,
            backend: Backend::Bwrap,
            workspace: ws.clone(),
            network: NetworkPolicy::Deny,
            flake_dir: None,
            devshell_variant: None,
            podman_image: None,
        };
        let bash_args = vec!["-lc".to_string(), "cargo test".to_string()];
        let w = maestro_sandbox::wrap(&spec, "bash", &bash_args).unwrap();
        assert_eq!(w.program, "bwrap", "AC5: outer program is the bwrap wrapper");
        // The argv ends with the inner `bash -lc "cargo test"`.
        let tail = ["bash".to_string(), "-lc".to_string(), "cargo test".to_string()];
        let n = w.args.len();
        assert!(n >= tail.len());
        assert_eq!(&w.args[n - tail.len()..], &tail[..], "AC5: ends with bash -lc \"cargo test\"");
    }
}
