//! The verifier's throwaway-checkout command runner (ADR-002).
//!
//! A verifier MAY run bounded commands to gather evidence. Those commands run in
//! a THROWAWAY CHECKOUT — a recursive COPY of the implementer's worktree into a
//! fresh tempdir with the `.git` entry EXCLUDED. Excluding `.git` severs the copy
//! from the repo: a command in the copy has no branch, index, or objects to write
//! to, so "verifiers never mutate the task branch" is a structural guarantee, not
//! a policy (ADR-002 "read-only is structural"). The copy is created lazily on the
//! first command and dropped (auto-removed) with the runner.
//!
//! Each command runs `sh -c <cmd>` wrapped under the task's containment recipe
//! (mirroring how the gate runs its check commands), with a per-command
//! wall-clock timeout. Output is digested (sha256 of the full combined
//! stdout+stderr) and truncated char-safely to a cap for the model excerpt.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use maestro_implementer::{VerifierCommandRun, VerifierCommandRunner};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::delegate::ContainmentRecipe;

/// The cap on the model-facing `output_excerpt` (chars). The full output is what
/// is digested; only the excerpt shown to the model is truncated.
const OUTPUT_EXCERPT_CAP: usize = 4000;

/// Per-command wall-clock timeout. A command exceeding it is SIGKILLed and
/// recorded as exit `-1`, so a wedged build can never block verification forever.
const COMMAND_TIMEOUT_SECS: u64 = 120;

/// How often to poll a running child for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A [`VerifierCommandRunner`] backed by a throwaway copy of the implementer's
/// worktree. The copy is created lazily and cached; dropping the runner removes
/// it (via the held [`TempDir`], so cleanup happens even on panic).
pub struct ThrowawayCheckoutRunner {
    /// The implementer's worktree to copy from.
    worktree_path: PathBuf,
    /// The task's containment recipe; each command is wrapped under it.
    recipe: ContainmentRecipe,
    /// The lazily-created copy. `Some(Ok(dir))` once made; `Some(Err(msg))` if the
    /// copy failed (then every `run` returns that error-record). `None` until the
    /// first `run`.
    checkout: Mutex<Option<Result<TempDir, String>>>,
}

impl ThrowawayCheckoutRunner {
    /// Build a runner over `worktree_path` using the task's containment `recipe`.
    /// No copy is made until the first command runs.
    pub fn new(worktree_path: &Path, recipe: ContainmentRecipe) -> Self {
        ThrowawayCheckoutRunner {
            worktree_path: worktree_path.to_path_buf(),
            recipe,
            checkout: Mutex::new(None),
        }
    }

    /// Return the throwaway-checkout directory, creating it on first use. On a
    /// copy failure returns the error string (cached; not retried).
    fn checkout_dir(&self) -> Result<PathBuf, String> {
        let mut guard = self.checkout.lock().expect("checkout mutex poisoned");
        if guard.is_none() {
            *guard = Some(make_checkout(&self.worktree_path));
        }
        match guard.as_ref().expect("just populated") {
            Ok(dir) => Ok(dir.path().to_path_buf()),
            Err(e) => Err(e.clone()),
        }
    }
}

impl VerifierCommandRunner for ThrowawayCheckoutRunner {
    fn run(&self, cmd: &str) -> VerifierCommandRun {
        let dir = match self.checkout_dir() {
            Ok(d) => d,
            Err(e) => {
                let excerpt = format!("throwaway checkout unavailable: {e}");
                return error_record(cmd, &excerpt);
            }
        };

        // Wrap `sh -c <cmd>` under the task's containment recipe (ADR-004),
        // mirroring the gate. At L0 / Backend::None the wrap is identity.
        let spec = self.recipe.spec_for(&dir);
        let sh_args = vec!["-c".to_string(), cmd.to_string()];
        let wrapped = match maestro_sandbox::wrap(&spec, "sh", &sh_args) {
            Ok(w) => w,
            Err(e) => {
                let excerpt = format!("containment wrap failed: {e}");
                return error_record(cmd, &excerpt);
            }
        };

        match run_bounded(&wrapped.program, &wrapped.args, &dir) {
            Ok((exit, combined)) => VerifierCommandRun {
                cmd: cmd.to_string(),
                exit,
                output_digest: sha256_hex(&combined),
                output_excerpt: truncate_chars(&combined, OUTPUT_EXCERPT_CAP),
            },
            Err(excerpt) => error_record(cmd, &excerpt),
        }
    }
}

/// Build an error-record (`exit: -1`, digest of the error text) for a command
/// that could not run to completion.
fn error_record(cmd: &str, excerpt: &str) -> VerifierCommandRun {
    VerifierCommandRun {
        cmd: cmd.to_string(),
        exit: -1,
        output_digest: sha256_hex(excerpt),
        output_excerpt: excerpt.to_string(),
    }
}

/// Recursively copy `src` into a fresh tempdir, EXCLUDING the top-level `.git`
/// entry (severing the copy from the repo). Returns the tempdir on success.
fn make_checkout(src: &Path) -> Result<TempDir, String> {
    let dir = TempDir::new().map_err(|e| format!("creating tempdir: {e}"))?;
    copy_tree_excluding_git(src, dir.path())
        .map_err(|e| format!("copying worktree {}: {e}", src.display()))?;
    Ok(dir)
}

/// Recursively copy `src` into `dst`, skipping any entry literally named `.git`
/// (so the top-level repo link is severed; a stray nested `.git`, e.g. a
/// submodule, is likewise dropped, which is fine — the copy must not reach git).
fn copy_tree_excluding_git(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(".git") {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree_excluding_git(&from, &to)?;
        } else if file_type.is_symlink() {
            // Copy the link target as a regular file's contents if it resolves;
            // a broken/looping link is skipped rather than failing the copy.
            match std::fs::read(&from) {
                Ok(bytes) => std::fs::write(&to, bytes)?,
                Err(_) => continue,
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Spawn `program args…` with cwd `dir`, capturing combined stdout+stderr, under
/// a wall-clock timeout. On timeout the child is SIGKILLed and reported as exit
/// `-1` with a timeout note. Returns `(exit_code, combined_output)` or an
/// `Err(excerpt)` if the child could not be spawned/captured.
fn run_bounded(program: &str, args: &[String], dir: &Path) -> Result<(i64, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning `{program}`: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(COMMAND_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_child(&mut child);
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(format!("waiting on child: {e}")),
        }
    };

    // Drain the captured pipes (the child has exited or been killed).
    let mut combined = String::new();
    if let Some(mut out) = child.stdout.take() {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        combined.push_str(&String::from_utf8_lossy(&buf));
    }
    if let Some(mut err) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        combined.push_str(&String::from_utf8_lossy(&buf));
    }

    match status {
        Some(status) => {
            // `-1` when killed by a signal / no exit code available.
            let exit = status.code().map(i64::from).unwrap_or(-1);
            Ok((exit, combined))
        }
        None => {
            combined.push_str(&format!(
                "\n[maestro] command killed after {COMMAND_TIMEOUT_SECS}s wall-clock timeout"
            ));
            Ok((-1, combined))
        }
    }
}

/// SIGKILL a child and reap it so no zombie remains.
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// sha256 hex digest (no prefix) of the full combined output. Matches the
/// implementer-side digest so records line up with the frozen schema.
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Truncate `s` to at most `cap` chars (char-safe, never splitting a codepoint).
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_string()
    } else {
        s.chars().take(cap).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_sandbox::{Backend, Level, NetworkPolicy};
    use tempfile::TempDir;

    /// An L0 / Backend::None recipe: `wrap` is identity, so commands run directly.
    fn l0_recipe() -> ContainmentRecipe {
        ContainmentRecipe {
            level: Level::L0,
            backend: Backend::None,
            network: NetworkPolicy::Deny,
            flake_dir: PathBuf::from("/"),
            devshell_variant: None,
            podman_image: None,
            requested: Level::L0,
            downgraded: false,
        }
    }

    /// Build a worktree-like dir: a `.git` marker dir plus a data file.
    fn make_worktree() -> TempDir {
        let wt = TempDir::new().unwrap();
        std::fs::create_dir_all(wt.path().join(".git")).unwrap();
        std::fs::write(wt.path().join(".git").join("HEAD"), "ref: refs/heads/x\n").unwrap();
        std::fs::write(wt.path().join("somefile"), "hello contents\n").unwrap();
        std::fs::create_dir_all(wt.path().join("src")).unwrap();
        std::fs::write(wt.path().join("src").join("lib.rs"), "pub fn x() {}\n").unwrap();
        wt
    }

    #[test]
    fn cat_reads_copied_file_stable_digest() {
        let wt = make_worktree();
        let runner = ThrowawayCheckoutRunner::new(wt.path(), l0_recipe());
        let r = runner.run("cat somefile");
        assert_eq!(r.exit, 0, "cat should succeed, got {r:?}");
        assert!(
            r.output_excerpt.contains("hello contents"),
            "excerpt must contain the file contents, got {:?}",
            r.output_excerpt
        );
        // Digest is the sha256 of the full combined output (here == excerpt).
        assert_eq!(r.output_digest, sha256_hex(&r.output_excerpt));
        // Stable across runs.
        let r2 = runner.run("cat somefile");
        assert_eq!(r.output_digest, r2.output_digest);
    }

    #[test]
    fn false_command_nonzero_exit() {
        let wt = make_worktree();
        let runner = ThrowawayCheckoutRunner::new(wt.path(), l0_recipe());
        let r = runner.run("false");
        assert_ne!(r.exit, 0, "`false` must exit non-zero, got {r:?}");
    }

    #[test]
    fn checkout_is_a_copy_original_untouched() {
        let wt = make_worktree();
        let runner = ThrowawayCheckoutRunner::new(wt.path(), l0_recipe());
        let r = runner.run("echo x > newfile");
        assert_eq!(r.exit, 0, "echo redirect should succeed, got {r:?}");
        // The write landed in the COPY, not the original worktree.
        assert!(
            !wt.path().join("newfile").exists(),
            "the original worktree must not gain `newfile` (copy is severed)"
        );
    }

    #[test]
    fn git_is_absent_in_the_copy() {
        let wt = make_worktree();
        let runner = ThrowawayCheckoutRunner::new(wt.path(), l0_recipe());
        // `test -e .git` exits non-zero when .git is absent in the copy.
        let r = runner.run("test -e .git; echo exit=$?");
        assert!(
            r.output_excerpt.contains("exit=1"),
            ".git must be absent in the throwaway copy, got {:?}",
            r.output_excerpt
        );
        // But the ordinary data file WAS copied.
        let r2 = runner.run("test -e somefile; echo exit=$?");
        assert!(
            r2.output_excerpt.contains("exit=0"),
            "non-.git files must be copied, got {:?}",
            r2.output_excerpt
        );
    }

    #[test]
    fn copy_failure_yields_error_record_not_panic() {
        // Point at a non-existent worktree: the lazy copy fails, and every run
        // returns an error-record rather than panicking.
        let missing = PathBuf::from("/no/such/worktree/path/maestro-test");
        let runner = ThrowawayCheckoutRunner::new(&missing, l0_recipe());
        let r = runner.run("cat somefile");
        assert_eq!(r.exit, -1);
        assert!(
            r.output_excerpt.contains("throwaway checkout unavailable"),
            "expected an unavailability note, got {:?}",
            r.output_excerpt
        );
        assert_eq!(r.output_digest, sha256_hex(&r.output_excerpt));
    }

    #[test]
    fn truncate_chars_is_codepoint_safe() {
        let s = "áéíóú"; // 5 multibyte chars
        assert_eq!(truncate_chars(s, 3), "áéí");
        assert_eq!(truncate_chars(s, 10), s);
    }
}
