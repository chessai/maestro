//! `maestro advise` (ADR-006, "Advisor filesystem isolation"): LAUNCH the
//! advisor's Claude Code session inside a bwrap mount where the repo working
//! tree is **read-only**. This is the load-bearing structural control — an
//! advisor that ignores its role prompt, or whose deny rules are misconfigured,
//! still physically cannot mutate the tree.
//!
//! Two write channels are carved out of the read-only mount: the advisor
//! scratch dir (outside the repo) and the opt-in in-repo `advisor.writable_paths`
//! allowlist. Network is left ALLOWED (the advisor needs it for Claude).
//!
//! The bwrap argv is built by [`maestro_sandbox::advisor_mount_command`] so it
//! is unit-testable; this module resolves the inputs (repo, profile, scratch,
//! writable paths, bwrap path) and execs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use maestro_journal::config::Config;
use maestro_journal::paths;
use maestro_sandbox::{advisor_mount_command, AdvisorMount};

/// Everything the launcher resolved, before building the bwrap argv. Kept
/// separate so it can be inspected/tested.
#[derive(Debug, Clone)]
pub struct AdviseInputs {
    /// The advisor's repo, absolute + canonicalized.
    pub repo: PathBuf,
    /// The advisor scratch dir (created).
    pub scratch: PathBuf,
    /// Writable in-repo carve-outs (absolute, existing).
    pub writable_paths: Vec<PathBuf>,
    /// The advisor's `$HOME`.
    pub home: PathBuf,
}

/// Entry point for `maestro advise`.
pub fn run(profile: Option<&str>, exec: Option<&str>) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (profile, exec);
        anyhow::bail!(
            "maestro advise: the read-only advisor mount is only supported on Linux \
             (bwrap) today; macOS (Seatbelt) support is not yet implemented"
        );
    }

    #[cfg(target_os = "linux")]
    {
        let inputs = resolve_inputs(profile, "session")?;
        let bwrap = resolve_bwrap()?;

        let mount = AdvisorMount {
            repo: inputs.repo.clone(),
            scratch: inputs.scratch.clone(),
            writable_paths: inputs.writable_paths.clone(),
            home: inputs.home.clone(),
        };

        let (program, args) = advise_command(&mount, &bwrap, exec);

        // Exec bwrap, passing through stdio/TTY so the interactive `claude`
        // session works.
        let status = std::process::Command::new(&program)
            .args(&args)
            .status()
            .with_context(|| format!("launching advisor mount via {program}"))?;

        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Build the `(program, args)` for the advisor mount. Inside the mount we run
/// either `claude` (interactive) or, with `--exec`, `bash -lc <cmd>`.
///
/// Factored out (and independent of any real filesystem) so the argv is
/// unit-testable.
pub fn advise_command(
    mount: &AdvisorMount,
    bwrap_bin: &str,
    exec: Option<&str>,
) -> (String, Vec<String>) {
    let (inner_prog, inner_args): (&str, Vec<String>) = match exec {
        Some(cmd) => ("bash", vec!["-lc".to_string(), cmd.to_string()]),
        None => ("claude", Vec::new()),
    };
    let wrapped = advisor_mount_command(mount, bwrap_bin, inner_prog, &inner_args);
    (wrapped.program, wrapped.args)
}

/// Resolve the launch inputs: the repo (cwd, canonicalized), the active
/// profile's `advisor.writable_paths` (resolved + filtered to existing paths
/// under the repo), the scratch dir (created), and `$HOME`.
pub fn resolve_inputs(profile: Option<&str>, scratch_id: &str) -> Result<AdviseInputs> {
    let repo = std::env::current_dir()
        .context("resolving the current directory (advisor repo)")?;
    let repo = std::fs::canonicalize(&repo)
        .with_context(|| format!("canonicalizing repo path {}", repo.display()))?;

    let scratch = paths::advisor_scratch_dir(scratch_id);
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("creating advisor scratch dir {}", scratch.display()))?;
    let scratch = std::fs::canonicalize(&scratch)
        .with_context(|| format!("canonicalizing scratch dir {}", scratch.display()))?;

    let writable_globs = resolve_writable_paths(&paths::config_path(), profile)?;
    let writable_paths = resolve_repo_writable_paths(&repo, &writable_globs);

    let home = home_dir()?;

    Ok(AdviseInputs {
        repo,
        scratch,
        writable_paths,
        home,
    })
}

/// The advisor's `$HOME` (writable so `~/.claude` works).
fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("resolving $HOME (needed writable for Claude Code's ~/.claude)")
}

/// Read the config (if present) and resolve the active profile's
/// `advisor.writable_paths`, falling back to `defaults.advisor.writable_paths`.
/// Active profile precedence: `--profile` flag > `MAESTRO_PROFILE` >
/// `default_profile`. Missing config → empty list (repo fully read-only).
pub fn resolve_writable_paths(config_path: &Path, profile: Option<&str>) -> Result<Vec<String>> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let config = Config::from_toml_str(&text)
        .with_context(|| format!("parsing config {}", config_path.display()))?;

    let active = active_profile(&config, profile);

    // Profile override, else defaults.
    if let Some(name) = active {
        if let Some(p) = config.profiles.get(&name) {
            if !p.advisor.writable_paths.is_empty() {
                return Ok(p.advisor.writable_paths.clone());
            }
        }
    }
    Ok(config.defaults.advisor.writable_paths.clone())
}

/// Resolve the active profile name: flag > `MAESTRO_PROFILE` > `default_profile`.
fn active_profile(config: &Config, flag: Option<&str>) -> Option<String> {
    if let Some(f) = flag {
        return Some(f.to_string());
    }
    if let Some(v) = std::env::var_os("MAESTRO_PROFILE") {
        if !v.is_empty() {
            return Some(v.to_string_lossy().into_owned());
        }
    }
    config.default_profile.clone()
}

/// Resolve each writable path under the repo and keep only those that exist.
/// Paths are joined under `repo` (a leading `/` is stripped so they stay inside
/// the repo). Non-existent paths are skipped: bwrap cannot bind a source that
/// does not exist.
fn resolve_repo_writable_paths(repo: &Path, globs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for g in globs {
        let rel = g.trim_start_matches('/');
        if rel.is_empty() {
            continue;
        }
        let joined = repo.join(rel);
        if let Ok(canon) = std::fs::canonicalize(&joined) {
            // Only keep paths that stay inside the repo.
            if canon.starts_with(repo) {
                out.push(canon);
            }
        }
    }
    out
}

/// Resolve `bwrap` to an absolute executable path (required on Linux for this
/// feature). Errors clearly if not found.
#[cfg(target_os = "linux")]
pub fn resolve_bwrap() -> Result<String> {
    resolve_exe_abs("bwrap").context(
        "maestro advise requires `bwrap` (bubblewrap) on Linux to enforce the \
         read-only advisor mount, but it was not found on $PATH",
    )
}

/// Resolve `name` to an absolute path on `$PATH`.
#[cfg(target_os = "linux")]
fn resolve_exe_abs(name: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(name);
        let ok = std::fs::metadata(&candidate)
            .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
            .unwrap_or(false);
        if ok {
            Some(candidate.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount() -> AdvisorMount {
        AdvisorMount {
            repo: PathBuf::from("/repo"),
            scratch: PathBuf::from("/scratch"),
            writable_paths: vec![],
            home: PathBuf::from("/home/u"),
        }
    }

    // AC7: the bwrap argv ro-binds the repo, does NOT unshare net, binds the
    // scratch dir, and chdirs into the repo. (Also see the sandbox crate's
    // helper test.)
    #[test]
    fn advise_command_ro_repo_scratch_no_net() {
        let (prog, args) = advise_command(&mount(), "/usr/bin/bwrap", None);
        assert_eq!(prog, "/usr/bin/bwrap");
        assert!(window3(&args, "--ro-bind", "/repo", "/repo"));
        assert!(window3(&args, "--bind", "/scratch", "/scratch"));
        assert!(!args.contains(&"--unshare-net".to_string()));
        assert!(window2(&args, "--chdir", "/repo"));
        // interactive default runs `claude`.
        assert!(window2(&args, "--", "claude"));
    }

    // --exec runs `bash -lc <cmd>` inside the mount.
    #[test]
    fn advise_command_exec_runs_bash_lc() {
        let (_prog, args) = advise_command(&mount(), "bwrap", Some("echo hi"));
        // tail is: -- bash -lc "echo hi"
        let n = args.len();
        assert_eq!(&args[n - 4..], &["--", "bash", "-lc", "echo hi"]);
    }

    fn window2(hay: &[String], a: &str, b: &str) -> bool {
        hay.windows(2).any(|w| w[0] == a && w[1] == b)
    }
    fn window3(hay: &[String], a: &str, b: &str, c: &str) -> bool {
        hay.windows(3).any(|w| w[0] == a && w[1] == b && w[2] == c)
    }

    // Active-profile writable_paths resolution: profile override wins, else
    // defaults; missing config → empty.
    #[test]
    fn writable_paths_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            r#"default_profile = "personal"
[defaults]
advisor.writable_paths = ["docs"]
[profiles.personal]
advisor.writable_paths = ["notes"]
[profiles.bare]
"#,
        )
        .unwrap();

        // default_profile = personal → ["notes"].
        assert_eq!(
            resolve_writable_paths(&cfg, None).unwrap(),
            vec!["notes".to_string()]
        );
        // profile with no advisor override → falls back to defaults ["docs"].
        assert_eq!(
            resolve_writable_paths(&cfg, Some("bare")).unwrap(),
            vec!["docs".to_string()]
        );
        // missing config → empty.
        let missing = tmp.path().join("nope.toml");
        assert!(resolve_writable_paths(&missing, None).unwrap().is_empty());
    }

    // In-repo writable paths are resolved under the repo and filtered to those
    // that exist.
    #[test]
    fn repo_writable_paths_filtered_to_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(repo.join("notes")).unwrap();
        let resolved = resolve_repo_writable_paths(
            &repo,
            &["notes".to_string(), "does-not-exist".to_string(), "/notes".to_string()],
        );
        // `notes` and `/notes` (leading slash stripped) both resolve to the same
        // existing dir; `does-not-exist` is dropped.
        assert!(resolved.iter().all(|p| p.ends_with("notes")));
        assert!(!resolved.is_empty());
        assert!(resolved.iter().all(|p| p.starts_with(&repo)));
    }
}
