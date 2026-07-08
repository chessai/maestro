//! Daemon transport for the MCP proxy (ADR-006).
//!
//! [`DaemonTransport`] is the seam the MCP server calls through; tests inject a
//! fake so the whole server is exercised without a real daemon. [`SocketTransport`]
//! is the production impl: it ensures a daemon is live (auto-spawn race, mirroring
//! the CLI's algorithm), then performs one request/response exchange per call over
//! the Unix socket.
//!
//! NOTE (future consolidation): the auto-spawn race and the wire client here
//! duplicate `maestro-cli`'s `daemon`/`client` modules. Both should move into a
//! shared client crate; for now the duplication is accepted (ADR-006).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use maestro_journal::paths;
use maestro_journal::proto::{Request, Response, PROTOCOL_VERSION};

/// The seam every daemon call goes through. Implemented by [`SocketTransport`]
/// in production and by a recording fake in tests.
pub trait DaemonTransport {
    /// Send one request to the daemon and return its response.
    fn call(&self, req: &Request) -> Result<Response>;
}

// ---------------------------------------------------------------------------
// Wire exchange (one request per connection).
// ---------------------------------------------------------------------------

/// Send a single request over a connected stream and read exactly one response
/// line. One request per connection (ADR-006 wire protocol).
fn exchange(stream: UnixStream, req: &Request) -> Result<Response> {
    let mut reader = BufReader::new(stream.try_clone().context("cloning socket stream")?);
    let mut write_half = stream;

    let mut out = serde_json::to_string(req).context("serializing request")?;
    out.push('\n');
    write_half
        .write_all(out.as_bytes())
        .context("writing request")?;
    write_half.flush().context("flushing request")?;

    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("reading response line")?;
    if n == 0 {
        bail!("daemon closed the connection without responding");
    }
    let resp: Response =
        serde_json::from_str(line.trim_end()).context("deserializing response")?;
    Ok(resp)
}

/// Connect to the socket at `path` and perform a single request/response.
fn request_at(path: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(path)
        .with_context(|| format!("connecting to daemon socket {}", path.display()))?;
    exchange(stream, req)
}

// ---------------------------------------------------------------------------
// Auto-spawn race (mirrors maestro-cli::daemon).
// ---------------------------------------------------------------------------

/// The result of a liveness probe against the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelloOutcome {
    /// A daemon answered `Hello` with a matching protocol version.
    Compatible,
    /// A daemon answered `Hello` with a mismatched version. FAIL LOUD; never
    /// spawn a second daemon (ADR-006).
    Incompatible { reported: u32 },
}

/// Probe the daemon socket once. `None` means "no daemon reachable".
fn try_connect() -> Option<HelloOutcome> {
    let resp = request_at(&paths::socket_path(), &Request::Hello).ok()?;
    match resp {
        Response::Hello {
            protocol_version, ..
        } => {
            if protocol_version == PROTOCOL_VERSION {
                Some(HelloOutcome::Compatible)
            } else {
                Some(HelloOutcome::Incompatible {
                    reported: protocol_version,
                })
            }
        }
        // Any other answer to Hello is a broken daemon; treat as unreachable.
        _ => None,
    }
}

/// Resolve the `maestro-daemon` binary (ADR-006 order): `$MAESTRO_DAEMON_BIN`,
/// else a sibling of the current exe (checking both `parent()` and its parent),
/// else `maestro-daemon` on `$PATH`.
fn resolve_daemon_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("MAESTRO_DAEMON_BIN") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut candidates = vec![dir.join("maestro-daemon")];
            if let Some(up) = dir.parent() {
                candidates.push(up.join("maestro-daemon"));
            }
            for c in candidates {
                if c.is_file() {
                    return c;
                }
            }
        }
    }
    PathBuf::from("maestro-daemon")
}

/// Spawn the daemon detached, forwarding `--profile` so it resolves the same
/// active profile. stdio → `data_dir()/daemon.log` (fallback `/dev/null`).
fn spawn_daemon(profile: Option<&str>) -> Result<()> {
    let bin = resolve_daemon_bin();

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
    Ok(())
}

/// Poll `probe` up to `tries` times with `interval` between attempts.
fn wait_until_live(
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

/// An RAII flock guard; dropping the file releases the lock.
struct FileLock {
    _file: std::fs::File,
}

/// Acquire `flock(LOCK_EX)` on [`paths::lock_path`], creating the file if needed.
/// The lockfile is the spawn mutex and is NEVER unlinked (ADR-006).
fn acquire_spawn_lock() -> Result<FileLock> {
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

/// The loud version-skew error (ADR-006): never resolved by spawning.
fn version_skew_error(reported: u32) -> anyhow::Error {
    anyhow::anyhow!(
        "daemon protocol version mismatch: daemon reports {reported}, this MCP proxy speaks \
         {PROTOCOL_VERSION}. The single-daemon-per-machine invariant means a second daemon \
         will NOT be spawned to resolve this. Run `maestro daemon restart` to restart the \
         daemon at the current version."
    )
}

/// The auto-spawn race (ADR-006 "Auto-spawn race resolution"): connect; on
/// failure take the lock, re-check, spawn once, poll until live, release.
fn ensure_daemon(profile: Option<&str>) -> Result<()> {
    // 1. Common path: a live compatible daemon already answers.
    match try_connect() {
        Some(HelloOutcome::Compatible) => return Ok(()),
        Some(HelloOutcome::Incompatible { reported }) => return Err(version_skew_error(reported)),
        None => {}
    }

    // 2. Contend for the spawn mutex.
    let _lock = acquire_spawn_lock().context("acquiring daemon spawn lock")?;

    // 3. Re-check under the lock — a racer may have spawned a live daemon.
    match try_connect() {
        Some(HelloOutcome::Compatible) => return Ok(()),
        Some(HelloOutcome::Incompatible { reported }) => return Err(version_skew_error(reported)),
        None => {}
    }

    // 4. Still dead under the lock → we win: spawn exactly once, poll until live.
    spawn_daemon(profile).context("spawning daemon")?;
    match wait_until_live(try_connect, 100, std::time::Duration::from_millis(50)) {
        Some(HelloOutcome::Compatible) => Ok(()),
        Some(HelloOutcome::Incompatible { reported }) => Err(version_skew_error(reported)),
        None => bail!(
            "spawned the daemon but it did not become reachable in time; check the daemon log \
             at {}",
            paths::data_dir().join("daemon.log").display()
        ),
    }
    // `_lock` drops here, releasing the flock.
}

/// Production transport: ensures a daemon is live, then one exchange per call.
pub struct SocketTransport {
    profile: Option<String>,
}

impl SocketTransport {
    /// Build a socket transport that forwards `profile` to any spawned daemon.
    pub fn new(profile: Option<String>) -> Self {
        Self { profile }
    }
}

impl DaemonTransport for SocketTransport {
    fn call(&self, req: &Request) -> Result<Response> {
        ensure_daemon(self.profile.as_deref())?;
        request_at(&paths::socket_path(), req)
    }
}
