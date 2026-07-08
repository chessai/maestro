//! Integration tests for `maestro advise` (ADR-006, "Advisor filesystem
//! isolation"): the read-only-repo mount is THE security property.
//!
//! These run the real `maestro` binary with a real `bwrap` (available in the
//! devShell) via the hermetic `--exec` seam. They never touch the network. The
//! interactive `claude` launch itself is not tested here (no TTY/subscription),
//! but the read-only enforcement — the security-relevant part — is.
//!
//! Each test isolates HOME / XDG_STATE_HOME / XDG_CONFIG_HOME to a tempdir so it
//! never touches the developer's real maestro state.

use std::path::Path;
use std::process::Command;

/// Path to the built `maestro` binary under test.
fn maestro_bin() -> &'static str {
    env!("CARGO_BIN_EXE_maestro")
}

/// Skip (return `false`) if bwrap cannot create user namespaces here. The
/// devShell has bwrap, but some CI sandboxes forbid namespaces entirely; in that
/// case these tests are not meaningful and we bail early rather than fail.
fn bwrap_usable() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `maestro advise --exec <cmd>` in `repo`, isolating state under `home`.
/// Returns the child's exit status success flag + captured stderr.
fn advise_exec(repo: &Path, home: &Path, cmd: &str) -> (bool, String) {
    let output = Command::new(maestro_bin())
        .arg("advise")
        .arg("--exec")
        .arg(cmd)
        .current_dir(repo)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .expect("running maestro advise");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stderr)
}

/// The advisor scratch dir the CLI will create for the fixed `"session"` id,
/// given an isolated XDG_STATE_HOME under `home`.
fn scratch_dir(home: &Path) -> std::path::PathBuf {
    home.join("state").join("maestro").join("advisor").join("session")
}

// AC5: read-only enforcement. A write to a repo path fails and does not modify
// the repo; a read succeeds; a write to the advisor scratch succeeds.
#[test]
fn advise_repo_is_read_only_scratch_is_writable() {
    if !bwrap_usable() {
        eprintln!("skipping: bwrap cannot create namespaces in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("repo_file.txt"), "original\n").unwrap();

    // Write to a repo path FAILS (read-only mount) and leaves the file intact.
    let (ok, _) = advise_exec(&repo, &home, "echo x > repo_file.txt");
    assert!(!ok, "writing a repo path must FAIL under the read-only mount");
    assert_eq!(
        std::fs::read_to_string(repo.join("repo_file.txt")).unwrap(),
        "original\n",
        "the repo file must be UNCHANGED on the host"
    );

    // Reading a repo path SUCCEEDS.
    let (ok, _) = advise_exec(&repo, &home, "cat repo_file.txt");
    assert!(ok, "reading a repo path must SUCCEED");

    // Writing to the advisor scratch SUCCEEDS.
    let scratch = scratch_dir(&home);
    let note = scratch.join("note.txt");
    let (ok, err) = advise_exec(
        &repo,
        &home,
        &format!("echo y > {}", note.display()),
    );
    assert!(ok, "writing the advisor scratch must SUCCEED; stderr: {err}");
    assert_eq!(
        std::fs::read_to_string(&note).unwrap(),
        "y\n",
        "the scratch write must be visible on the host"
    );
}

// AC6: writable_paths carve-out. With `advisor.writable_paths = ["notes"]` and a
// `notes/` dir, a write under `notes/` succeeds while a write to a non-carved
// repo path still fails.
#[test]
fn advise_writable_paths_carve_out() {
    if !bwrap_usable() {
        eprintln!("skipping: bwrap cannot create namespaces in this environment");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join("config").join("maestro")).unwrap();
    // Config with a profile carving out `notes`.
    std::fs::write(
        home.join("config").join("maestro").join("config.toml"),
        "default_profile = \"personal\"\n\
         [profiles.personal]\n\
         advisor.writable_paths = [\"notes\"]\n",
    )
    .unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("notes")).unwrap();
    std::fs::write(repo.join("locked.txt"), "locked\n").unwrap();

    // Write under the carved-out `notes/` SUCCEEDS.
    let (ok, err) = advise_exec(&repo, &home, "echo z > notes/x.txt");
    assert!(ok, "write to a carved-out path must SUCCEED; stderr: {err}");
    assert_eq!(
        std::fs::read_to_string(repo.join("notes").join("x.txt")).unwrap(),
        "z\n"
    );

    // Write to a NON-carved repo path still FAILS.
    let (ok, _) = advise_exec(&repo, &home, "echo w > locked.txt");
    assert!(!ok, "write to a non-carved repo path must still FAIL");
    assert_eq!(
        std::fs::read_to_string(repo.join("locked.txt")).unwrap(),
        "locked\n"
    );
}
