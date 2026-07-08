//! Filesystem path resolution shared by the daemon and CLI (ADR-006 / ADR-007).
//!
//! Both binaries MUST agree on these paths, so resolution lives here in the
//! shared crate rather than being duplicated. Pure and deterministic (reads
//! only environment + `$HOME`); no policy.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Value of an env var if set and non-empty.
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn app_support() -> PathBuf {
    home().join("Library/Application Support")
}

/// The maestro config file: `$XDG_CONFIG_HOME/maestro/config.toml`
/// (macOS: `~/Library/Application Support/maestro/config.toml`).
pub fn config_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        app_support().join("maestro/config.toml")
    }
    #[cfg(not(target_os = "macos"))]
    {
        env_dir("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home().join(".config"))
            .join("maestro/config.toml")
    }
}

/// The maestro data directory (holds `journal.db`).
/// `$XDG_DATA_HOME/maestro` (macOS: `~/Library/Application Support/maestro`).
pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        app_support().join("maestro")
    }
    #[cfg(not(target_os = "macos"))]
    {
        env_dir("XDG_DATA_HOME")
            .unwrap_or_else(|| home().join(".local/share"))
            .join("maestro")
    }
}

/// The journal database path: `<data_dir>/journal.db`.
pub fn journal_db_path() -> PathBuf {
    data_dir().join("journal.db")
}

/// The maestro state directory (advisor scratch, etc.).
/// `$XDG_STATE_HOME/maestro` (macOS: under Application Support).
pub fn state_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        app_support().join("maestro/state")
    }
    #[cfg(not(target_os = "macos"))]
    {
        env_dir("XDG_STATE_HOME")
            .unwrap_or_else(|| home().join(".local/state"))
            .join("maestro")
    }
}

/// The advisor scratch directory for a given advisor session (ADR-006).
pub fn advisor_scratch_dir(advisor_session_id: &str) -> PathBuf {
    state_dir().join("advisor").join(advisor_session_id)
}

/// The runtime directory holding the socket + lockfile.
/// `$XDG_RUNTIME_DIR` when set (macOS: Application Support), else falls back to
/// [`data_dir`] so the daemon still works on hosts without a runtime dir.
pub fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        app_support().join("maestro")
    }
    #[cfg(not(target_os = "macos"))]
    {
        env_dir("XDG_RUNTIME_DIR").unwrap_or_else(data_dir)
    }
}

/// The daemon Unix socket path: `<runtime_dir>/maestro.sock`.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("maestro.sock")
}

/// The auto-spawn lockfile path: `<runtime_dir>/maestro.lock`. This file is the
/// spawn mutex (ADR-006) and is NEVER unlinked — unlike the socket.
pub fn lock_path() -> PathBuf {
    runtime_dir().join("maestro.lock")
}
