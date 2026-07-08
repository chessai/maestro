//! Shared low-level PTY primitives for driven sessions (ADR-006).
//!
//! Both the interactive [`crate::session::DrivenSession`] (prompt-on-stdin,
//! mid-run plan echo, then supervise) and the two-phase
//! [`crate::claude::run_claude_driven`] (prompt-as-arg, run to completion twice)
//! build on the same primitives here:
//!
//! - [`PtyChild`] — opens a PTY, spawns a program in its own session /
//!   process-group (portable-pty `setsid`s the slave), and starts a reader
//!   thread pumping PTY output to a log file (append) + a shared in-memory
//!   buffer while stamping `last_output_at` for the watchdog.
//! - [`teardown`] — SIGTERM the child's process group, wait up to 5s, SIGKILL.
//! - [`run_pty_command`] — the run-to-completion helper used by the Claude
//!   adapter: spawn `program args` in `cwd`, supervise under a no-output
//!   watchdog and an external-kill receiver, and return the captured output
//!   plus why it ended ([`PtyRunOutcome`]).

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};
use rustix::process::{kill_process_group, Pid, Signal};

use crate::session::KillKind;

/// SIGKILL grace period after SIGTERM during teardown (ADR-006).
pub(crate) const TERM_GRACE: Duration = Duration::from_secs(5);
/// Supervision poll granularity.
pub(crate) const POLL: Duration = Duration::from_millis(50);

/// Shared PTY output buffer plus the last-output timestamp for the watchdog.
pub(crate) struct Shared {
    pub(crate) buf: Mutex<Vec<u8>>,
    pub(crate) last_output_at: Mutex<Instant>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(Vec::new()),
            last_output_at: Mutex::new(Instant::now()),
        })
    }
}

/// A spawned child over a PTY, with its reader thread and master handle owned so
/// the caller can supervise the child, drive its stdin, then cleanly tear down
/// (drop the master → reader EOF → join).
pub(crate) struct PtyChild {
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) child_pid: Option<i32>,
    pub(crate) shared: Arc<Shared>,
    /// PTY master writer, for interactive prompt-on-stdin callers. Taken in
    /// [`PtyChild::drop`] so the master closes and the reader hits EOF.
    writer: Option<Box<dyn Write + Send>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    reader_handle: Option<JoinHandle<()>>,
    reader_stop: Arc<AtomicBool>,
}

impl PtyChild {
    /// Open a PTY, spawn `program args` in `cwd` (its own session /
    /// process-group), and start the reader thread appending output to
    /// `log_path` and a shared buffer.
    ///
    /// `env_remove` lists env var names to strip from the child's inherited
    /// environment (ADR-006 `metered: false`). The daemon's own process env
    /// is NOT touched; `CommandBuilder` snapshotted it at construction time
    /// and `env_remove` operates only on that snapshot.
    pub(crate) fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        log_path: &Path,
        env_remove: &[String],
    ) -> Result<Self, String> {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty failed: {e}"))?;
        let master = pty.master;
        let slave = pty.slave;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        for key in env_remove {
            cmd.env_remove(key);
        }

        let mut child: Box<dyn Child + Send + Sync> = slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn `{program}` failed: {e}"))?;
        let child_pid = child.process_id().map(|p| p as i32);

        // Drop the slave so the master sees EOF when the child exits.
        drop(slave);

        let reader = master.try_clone_reader().map_err(|e| {
            teardown(child.as_mut(), child_pid);
            format!("clone reader failed: {e}")
        })?;
        let writer = master.take_writer().map_err(|e| {
            teardown(child.as_mut(), child_pid);
            format!("take writer failed: {e}")
        })?;

        let shared = Shared::new();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let reader_handle = spawn_reader(
            reader,
            shared.clone(),
            log_path.to_path_buf(),
            reader_stop.clone(),
        );

        Ok(PtyChild {
            child,
            child_pid,
            shared,
            writer: Some(writer),
            master: Some(master),
            reader_handle: Some(reader_handle),
            reader_stop,
        })
    }

    /// Write bytes to the child's PTY stdin (interactive prompt-on-stdin path).
    pub(crate) fn write_stdin(&mut self, bytes: &[u8]) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// How long since the child last produced output (for the watchdog).
    pub(crate) fn idle(&self) -> Duration {
        self.shared.last_output_at.lock().unwrap().elapsed()
    }

    /// Snapshot of the captured output so far.
    pub(crate) fn output(&self) -> String {
        String::from_utf8_lossy(&self.shared.buf.lock().unwrap()).into_owned()
    }

    /// SIGTERM→SIGKILL the child's process group and reap it.
    pub(crate) fn teardown(&mut self) {
        teardown(self.child.as_mut(), self.child_pid);
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        // Ensure the child is gone first so the PTY slave closes; then drop the
        // writer & master so the reader thread hits EOF, then join it. Dropping
        // the master (not just the stop flag) is what unblocks a reader that is
        // parked inside a blocking `read`.
        self.reader_stop.store(true, Ordering::SeqCst);
        // Reap the child if the supervision loop didn't already; harmless if it
        // already exited (try_wait/wait return immediately).
        teardown(self.child.as_mut(), self.child_pid);
        drop(self.writer.take());
        drop(self.master.take());
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the reader thread. Reads until EOF/error, appending every chunk to the
/// log file and the shared buffer and stamping `last_output_at`.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    shared: Arc<Shared>,
    log_path: PathBuf,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let mut chunk = [0u8; 4096];
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match reader.read(&mut chunk) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let bytes = &chunk[..n];
                    if let Some(ref mut f) = log {
                        let _ = f.write_all(bytes);
                        let _ = f.flush();
                    }
                    shared.buf.lock().unwrap().extend_from_slice(bytes);
                    *shared.last_output_at.lock().unwrap() = Instant::now();
                }
                Err(_) => break,
            }
        }
    })
}

/// Teardown = SIGTERM the child's process group, wait up to 5s, then SIGKILL
/// the group (ADR-006). Falls back to portable-pty's own killer if we have no
/// pid. Ensures no lingering child by reaping it.
pub(crate) fn teardown(child: &mut dyn Child, child_pid: Option<i32>) {
    if let Some(pid) = child_pid.and_then(Pid::from_raw) {
        // Negative pid → whole process group (the child is its own group leader
        // because portable-pty setsid'd it).
        let _ = kill_process_group(pid, Signal::Term);

        let deadline = Instant::now() + TERM_GRACE;
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                return; // exited & reaped within grace.
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        // Escalate: SIGKILL the group, then reap.
        let _ = kill_process_group(pid, Signal::Kill);
    } else {
        // No pid: best-effort via portable-pty.
        let _ = child.kill();
    }
    // Reap so no zombie/lingering child remains.
    let _ = child.wait();
}

/// Why a [`run_pty_command`] invocation ended.
pub(crate) enum PtyRunOutcome {
    /// The child exited on its own with this status code (`None` if unknown).
    Exited(Option<i32>),
    /// No output past the watchdog → wedged (child torn down).
    Wedged,
    /// External kill request honored (child torn down).
    Killed(KillKind),
    /// Spawn / PTY setup error.
    SpawnError(String),
}

/// The captured output plus the terminal outcome of a run-to-completion PTY
/// command.
pub(crate) struct PtyRun {
    pub(crate) output: String,
    pub(crate) outcome: PtyRunOutcome,
}

/// Run `program args` in `cwd` under a PTY to completion, streaming output to
/// `log_path` (append) + an in-memory buffer, resetting a no-output watchdog,
/// and honoring an external kill via `kill_rx`. `pid_slot` is updated with the
/// child's pid so an external [`crate::session::SessionHandle`] can target the
/// currently-running child. Returns the captured output and why it ended.
///
/// `env_remove` lists env var names to strip from the child's inherited
/// environment before spawn (ADR-006 `metered: false`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pty_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    log_path: &Path,
    watchdog: Duration,
    kill_rx: &Receiver<KillKind>,
    pid_slot: &Arc<Mutex<Option<i32>>>,
    env_remove: &[String],
) -> PtyRun {
    let mut pty = match PtyChild::spawn(program, args, cwd, log_path, env_remove) {
        Ok(p) => p,
        Err(e) => {
            return PtyRun {
                output: String::new(),
                outcome: PtyRunOutcome::SpawnError(e),
            }
        }
    };

    // Publish the pid so the handle targets THIS phase's child.
    *pid_slot.lock().unwrap() = pty.child_pid;

    let outcome = loop {
        // (c) external kill.
        if let Ok(kind) = kill_rx.try_recv() {
            pty.teardown();
            break PtyRunOutcome::Killed(kind);
        }
        // (a) child exit.
        match pty.child.try_wait() {
            Ok(Some(status)) => {
                break PtyRunOutcome::Exited(exit_code(&status));
            }
            Ok(None) => {}
            Err(e) => {
                pty.teardown();
                break PtyRunOutcome::SpawnError(format!("try_wait failed: {e}"));
            }
        }
        // (b) watchdog.
        if pty.idle() > watchdog {
            pty.teardown();
            break PtyRunOutcome::Wedged;
        }
        std::thread::sleep(POLL);
    };

    let output = pty.output();
    // `pty` drops here: reader joined, master/writer closed.
    PtyRun { output, outcome }
}

/// Best-effort exit code from a portable-pty exit status.
fn exit_code(status: &portable_pty::ExitStatus) -> Option<i32> {
    // portable-pty exposes a raw u32 exit code; success() distinguishes zero.
    if status.success() {
        Some(0)
    } else {
        // exit_code() is u32; clamp into i32 for our EndReason mapping.
        Some(status.exit_code() as i32)
    }
}
