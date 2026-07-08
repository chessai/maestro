//! Daemon binary resolution, detached spawn, and the auto-spawn race (ADR-006).
//!
//! The race algorithm ([`ensure_daemon`]) is factored to be unit-testable
//! WITHOUT real sockets or processes: it is generic over an injected
//! `try_connect` (probe liveness + version) and `spawn` (start the daemon), plus
//! a bounded `wait_until_live` poller. The real client wires these to the actual
//! socket and `maestro-daemon` binary.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use maestro_journal::paths;
use maestro_journal::proto::{Request, Response, PROTOCOL_VERSION};

/// The result of a liveness probe against the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloOutcome {
    /// A daemon answered `Hello` with a protocol version equal to
    /// [`PROTOCOL_VERSION`].
    Compatible {
        /// The daemon's pid.
        pid: u32,
    },
    /// A daemon answered `Hello` but with a mismatched protocol version. The
    /// client must FAIL LOUD and never spawn a second daemon (ADR-006).
    Incompatible {
        /// The version the reachable daemon reported.
        reported: u32,
    },
}

/// Probe the daemon socket once. `None` means "no daemon reachable" (ENOENT /
/// ECONNREFUSED / no answer). `Some(outcome)` reports the handshake result.
pub fn try_connect() -> Option<HelloOutcome> {
    let resp = crate::client::request_at(&paths::socket_path(), &Request::Hello).ok()?;
    match resp {
        Response::Hello {
            protocol_version,
            pid,
        } => {
            if protocol_version == PROTOCOL_VERSION {
                Some(HelloOutcome::Compatible { pid })
            } else {
                Some(HelloOutcome::Incompatible {
                    reported: protocol_version,
                })
            }
        }
        // Any other response to Hello is a broken daemon; treat as unreachable.
        _ => None,
    }
}

/// Resolve the path to the `maestro-daemon` binary (ADR-006 order):
/// 1. `$MAESTRO_DAEMON_BIN` if set;
/// 2. a file named `maestro-daemon` next to the current executable — checking
///    BOTH `current_exe().parent()` (e.g. `target/debug/`) AND its parent
///    (`target/debug/deps/` → `target/debug/`);
/// 3. `maestro-daemon` on `$PATH`.
pub fn resolve_daemon_bin() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("MAESTRO_DAEMON_BIN") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("maestro-daemon"));
            if let Some(up) = dir.parent() {
                candidates.push(up.join("maestro-daemon"));
            }
        }
        for c in candidates {
            if c.is_file() {
                return Ok(c);
            }
        }
    }

    // Fall back to $PATH resolution by name; std::process::Command searches PATH
    // for a bare program name, so returning the name is sufficient.
    Ok(PathBuf::from("maestro-daemon"))
}

/// Spawn the daemon binary detached: stdio redirected to `data_dir()/daemon.log`
/// (falling back to `/dev/null`), and NOT waited on. `profile` is forwarded as
/// `--profile <name>` so the daemon resolves the same active profile.
pub fn spawn_daemon(profile: Option<&str>) -> Result<()> {
    let bin = resolve_daemon_bin()?;

    // Ensure the data dir exists for the log file.
    let data_dir = paths::data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let log = data_dir.join("daemon.log");

    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .or_else(|_| std::fs::File::open("/dev/null"))
        .context("opening daemon log / /dev/null for stdout")?;
    let stderr = stdout.try_clone().context("cloning daemon log handle")?;

    let mut cmd = Command::new(&bin);
    if let Some(p) = profile {
        cmd.arg("--profile").arg(p);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr));

    cmd.spawn()
        .with_context(|| format!("spawning daemon binary {}", bin.display()))?;
    // Detach: do NOT wait on the child.
    Ok(())
}

/// Poll `probe` up to `tries` times with `interval` between attempts, returning
/// the first `Some(outcome)`. `None` if it never came up within the budget.
pub fn wait_until_live(
    mut probe: impl FnMut() -> Option<HelloOutcome>,
    tries: u32,
    interval: std::time::Duration,
) -> Option<HelloOutcome> {
    for i in 0..tries {
        if let Some(o) = probe() {
            return Some(o);
        }
        if i + 1 < tries {
            std::thread::sleep(interval);
        }
    }
    None
}

/// Report of how [`ensure_daemon`] resolved: whether it had to spawn, and the
/// live daemon's pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnsureOutcome {
    /// Number of daemons this client spawned (0 or 1). The M0 invariant is that
    /// across two racers exactly one daemon is ever spawned.
    pub spawned: u32,
    /// The pid of the live, compatible daemon.
    pub pid: u32,
}

/// The auto-spawn race (ADR-006 "Auto-spawn race resolution"), factored over
/// injected behaviors so it is unit-testable without sockets or processes.
///
/// - `try_connect`: probe the socket once (`None` = dead).
/// - `acquire_lock`: acquire `flock(LOCK_EX)` on the lockfile (blocks); the
///   returned guard releases on drop. The lockfile is never unlinked.
/// - `spawn`: start the daemon once (detached).
/// - `wait_live`: poll `try_connect` (bounded) after a spawn until it answers.
///
/// Algorithm:
/// 1. `try_connect`; a compatible daemon → use it (spawn count 0).
///    An incompatible daemon → FAIL LOUD, never spawn.
/// 2. else acquire the lock.
/// 3. re-`try_connect` under the lock; now-compatible → release + use it
///    (spawn count 0 — the loser path). Incompatible now → FAIL LOUD.
/// 4. else spawn once, then poll until live; release the lock.
/// 5. version handshake enforced at every reachable point: a mismatched
///    daemon errors with a `maestro daemon restart` hint and never spawns a
///    second daemon.
pub fn ensure_daemon<L>(
    mut try_connect: impl FnMut() -> Option<HelloOutcome>,
    mut acquire_lock: impl FnMut() -> Result<L>,
    mut spawn: impl FnMut() -> Result<()>,
    mut wait_live: impl FnMut() -> Option<HelloOutcome>,
) -> Result<EnsureOutcome> {
    // 1. Common path: a live daemon already answers.
    match try_connect() {
        Some(HelloOutcome::Compatible { pid }) => {
            return Ok(EnsureOutcome { spawned: 0, pid });
        }
        Some(HelloOutcome::Incompatible { reported }) => {
            return Err(version_skew_error(reported));
        }
        None => {}
    }

    // 2. Contend for the spawn mutex.
    let _lock = acquire_lock().context("acquiring daemon spawn lock")?;

    // 3. Re-check under the lock — a racer may have spawned a live daemon.
    match try_connect() {
        Some(HelloOutcome::Compatible { pid }) => {
            return Ok(EnsureOutcome { spawned: 0, pid });
        }
        Some(HelloOutcome::Incompatible { reported }) => {
            return Err(version_skew_error(reported));
        }
        None => {}
    }

    // 4. Still dead under the lock → we are the winner: spawn exactly once.
    spawn().context("spawning daemon")?;

    // Poll until the freshly spawned daemon is accepting.
    match wait_live() {
        Some(HelloOutcome::Compatible { pid }) => Ok(EnsureOutcome { spawned: 1, pid }),
        Some(HelloOutcome::Incompatible { reported }) => Err(version_skew_error(reported)),
        None => bail!(
            "spawned the daemon but it did not become reachable in time; \
             check the daemon log at {}",
            paths::data_dir().join("daemon.log").display()
        ),
    }
    // `_lock` drops here, releasing the flock.
}

/// The loud version-skew error (ADR-006): never resolved by spawning a second
/// daemon; the human restarts.
fn version_skew_error(reported: u32) -> anyhow::Error {
    anyhow::anyhow!(
        "daemon protocol version mismatch: daemon reports {reported}, this CLI speaks \
         {PROTOCOL_VERSION}. The single-daemon-per-machine invariant means a second daemon \
         will NOT be spawned to resolve this. Run `maestro daemon restart` to restart the \
         daemon at the current version."
    )
}

/// An RAII flock guard over the lockfile. Holds the open file whose fd carries
/// the `flock`; dropping the file releases the lock.
pub struct FileLock {
    _file: std::fs::File,
}

/// Acquire `flock(LOCK_EX)` on [`paths::lock_path`], creating the file if
/// needed. The lockfile is the spawn mutex and is NEVER unlinked (ADR-006).
pub fn acquire_spawn_lock() -> Result<FileLock> {
    use rustix::fs::{flock, FlockOperation};

    let path = paths::lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lockfile {}", path.display()))?;
    flock(&file, FlockOperation::LockExclusive)
        .with_context(|| format!("flock(LOCK_EX) on {}", path.display()))?;
    Ok(FileLock { _file: file })
}

/// The real `ensure_daemon`: wires the injected behaviors to the actual socket,
/// lockfile, and daemon binary. `profile` is forwarded to a spawned daemon.
pub fn ensure_daemon_real(profile: Option<&str>) -> Result<EnsureOutcome> {
    ensure_daemon(
        try_connect,
        acquire_spawn_lock,
        || spawn_daemon(profile),
        || wait_until_live(try_connect, 100, std::time::Duration::from_millis(50)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct NoopLock;

    /// AC4(a) COLD START: connect is dead until after a spawn → spawns exactly
    /// once, then reports live.
    #[test]
    fn cold_start_spawns_once_then_live() {
        let spawns = Cell::new(0u32);
        let spawned_flag = Cell::new(false);

        let out = ensure_daemon(
            // Dead until a spawn has happened.
            || {
                if spawned_flag.get() {
                    Some(HelloOutcome::Compatible { pid: 4242 })
                } else {
                    None
                }
            },
            || Ok(NoopLock),
            || {
                spawns.set(spawns.get() + 1);
                spawned_flag.set(true);
                Ok(())
            },
            // After spawn, the poller sees it live.
            || Some(HelloOutcome::Compatible { pid: 4242 }),
        )
        .expect("cold start should succeed");

        assert_eq!(spawns.get(), 1, "cold start must spawn exactly once");
        assert_eq!(out.spawned, 1);
        assert_eq!(out.pid, 4242);
    }

    /// AC4(b) LOSER: dead on the first connect, but ALIVE on the re-check under
    /// the lock → spawns ZERO times.
    #[test]
    fn loser_does_not_spawn() {
        let spawns = Cell::new(0u32);
        let calls = Cell::new(0u32);

        let out = ensure_daemon(
            || {
                let n = calls.get();
                calls.set(n + 1);
                // First probe (pre-lock): dead. Second probe (under lock): a
                // racer already brought one up.
                if n == 0 {
                    None
                } else {
                    Some(HelloOutcome::Compatible { pid: 7 })
                }
            },
            || Ok(NoopLock),
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || panic!("loser must never reach the wait-until-live poller"),
        )
        .expect("loser path should succeed");

        assert_eq!(spawns.get(), 0, "loser must spawn ZERO times");
        assert_eq!(out.spawned, 0);
        assert_eq!(out.pid, 7);
    }

    /// AC4(c) VERSION MISMATCH: connect returns Incompatible → error, spawns
    /// ZERO times.
    #[test]
    fn version_mismatch_errors_and_never_spawns() {
        let spawns = Cell::new(0u32);

        let err = ensure_daemon(
            || Some(HelloOutcome::Incompatible { reported: 999 }),
            || Ok(NoopLock),
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || panic!("must not poll after a version mismatch"),
        )
        .expect_err("version mismatch must be an error");

        assert_eq!(spawns.get(), 0, "version mismatch must spawn ZERO times");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("daemon restart"),
            "error must hint `maestro daemon restart`, got: {msg}"
        );
    }

    /// A mismatch discovered only under the lock also errors without spawning.
    #[test]
    fn version_mismatch_under_lock_never_spawns() {
        let spawns = Cell::new(0u32);
        let calls = Cell::new(0u32);

        let err = ensure_daemon(
            || {
                let n = calls.get();
                calls.set(n + 1);
                if n == 0 {
                    None
                } else {
                    Some(HelloOutcome::Incompatible { reported: 42 })
                }
            },
            || Ok(NoopLock),
            || {
                spawns.set(spawns.get() + 1);
                Ok(())
            },
            || panic!("must not poll after a version mismatch"),
        )
        .expect_err("mismatch under lock must error");

        assert_eq!(spawns.get(), 0);
        assert!(format!("{err:#}").contains("daemon restart"));
    }

    /// AC5: `resolve_daemon_bin()` honors `$MAESTRO_DAEMON_BIN`.
    #[test]
    fn resolve_daemon_bin_honors_env() {
        // This test process is single-threaded for env safety within the test
        // body; set, resolve, restore.
        let prev = std::env::var_os("MAESTRO_DAEMON_BIN");
        std::env::set_var("MAESTRO_DAEMON_BIN", "/tmp/custom-maestro-daemon");
        let got = resolve_daemon_bin().unwrap();
        assert_eq!(got, PathBuf::from("/tmp/custom-maestro-daemon"));
        match prev {
            Some(v) => std::env::set_var("MAESTRO_DAEMON_BIN", v),
            None => std::env::remove_var("MAESTRO_DAEMON_BIN"),
        }
    }
}
