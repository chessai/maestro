//! maestro-daemon — one-per-machine daemon that owns the journal, serves the
//! Unix-socket control API, and (in later milestones) schedules tasks and
//! proxies API traffic (ADR-006). This M0 scope covers: startup + migrations,
//! socket bind with 0600 mode, config-profile resolution (ADR-007), and the
//! read-only control surface (`Hello` / `Ps` / `Doctor`).
//!
//! The crate is a library so integration tests can drive the server in-process
//! (see [`serve_on`]); `src/main.rs` is a thin wrapper over [`run`].

pub mod credentials;
pub mod delegate;
pub mod gate;
pub mod model_auth;
pub mod resolve;
pub mod shim;
pub mod startup;
pub mod verify_checkout;
pub mod worktree;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use maestro_journal::config::Config;
use maestro_journal::proto::{DoctorReport, Request, Response, PROTOCOL_VERSION};
use maestro_journal::{paths, Journal};

use crate::delegate::{DelegationState, DelegateError};
use crate::resolve::{resolve, resolved_profile_json};

/// Per-advisor inbox cursor map: advisor_session_id → last-drained event_id.
type InboxCursors = Mutex<HashMap<String, String>>;

/// Startup options for the daemon. All fields are optional; defaults reproduce
/// the production binary's behavior.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// `--profile <name>`: overrides `MAESTRO_PROFILE` and `default_profile`.
    pub profile: Option<String>,
    /// Detach from the controlling terminal via `setsid` (best-effort) so an
    /// auto-spawned daemon outlives its spawner. Off by default in tests.
    pub detach: bool,
}

/// Parse process args into [`Options`]. Recognizes `--profile <name>` /
/// `--profile=<name>`; unknown flags are ignored (M0).
pub fn options_from_args<I, S>(args: I) -> Options
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut opts = Options {
        detach: true,
        ..Default::default()
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let a = a.as_ref();
        if let Some(v) = a.strip_prefix("--profile=") {
            opts.profile = Some(v.to_string());
        } else if a == "--profile" {
            if let Some(v) = it.next() {
                opts.profile = Some(v.as_ref().to_string());
            }
        }
    }
    opts
}

/// Entry point for the binary: parse args, init logging, and run the server
/// until a termination signal. Blocks.
pub fn run() -> Result<()> {
    init_tracing();
    // Load credentials BEFORE Server::start (and before any worker threads
    // spawn) so set_var is called in a single-threaded context (ADR-007).
    credentials::load_credentials_into_env();
    let opts = options_from_args(std::env::args().skip(1));
    let server = Server::start(opts)?;
    server.serve_forever()
}

/// Initialize `tracing` to stderr. Respects `RUST_LOG` via `EnvFilter`, else
/// defaults to `info`. Safe to call more than once (errors are ignored).
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

/// Shared, cloneable runtime state passed to every connection handler and
/// background delegate worker (ADR-006). Holds the single journal writer behind
/// a mutex (via [`DelegationState`]), the per-advisor inbox cursors, and the
/// resolved startup options.
pub struct SharedState {
    /// Delegation pipeline state: the `Arc<Mutex<Journal>>` and concurrency cap.
    delegation: Arc<DelegationState>,
    /// Per-advisor inbox cursors (in-memory; advanced on each drain).
    inbox_cursors: InboxCursors,
    opts: Options,
}

/// A started daemon: owns the shared runtime state, the bound listener, and the
/// resolved startup options. Serving is split out so tests can start and stop it.
pub struct Server {
    state: Arc<SharedState>,
    listener: UnixListener,
    socket_path: std::path::PathBuf,
    shutdown: Arc<AtomicBool>,
}

impl Server {
    /// Start the daemon: create dirs, open+migrate the journal, optionally
    /// detach from the terminal, then bind + listen on the socket with mode
    /// 0600. Does not begin accepting; call [`Server::serve_forever`] or
    /// [`Server::serve_until`].
    pub fn start(opts: Options) -> Result<Self> {
        // 1. Ensure the data + runtime directories exist.
        let data_dir = paths::data_dir();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let runtime_dir = paths::runtime_dir();
        std::fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("creating runtime dir {}", runtime_dir.display()))?;

        // 2. Open the journal (runs migrations).
        let db_path = paths::journal_db_path();
        let db_str = db_path
            .to_str()
            .context("journal db path is not valid UTF-8")?;
        let journal = Journal::open(db_str)
            .with_context(|| format!("opening journal at {}", db_path.display()))?;
        tracing::info!(db = %db_path.display(), "journal opened + migrated");

        // 3. Detach from the controlling terminal (best-effort). Only when
        //    requested (the production binary); tests keep their session.
        if opts.detach {
            match rustix::process::setsid() {
                Ok(_) => tracing::debug!("detached from controlling terminal (setsid)"),
                Err(e) => tracing::debug!(error = %e, "setsid failed (best-effort, continuing)"),
            }
        }

        // 4. Bind the socket: unlink any stale file, bind, listen, chmod 0600.
        let socket_path = paths::socket_path();
        let listener = bind_socket(&socket_path)?;
        tracing::info!(socket = %socket_path.display(), pid = std::process::id(), "listening");

        // 5. Resolve the machine concurrency cap from the active profile, then
        //    build the shared delegation state around the journal.
        let machine_cap = resolve_machine_cap(opts.profile.as_deref());
        let delegation =
            DelegationState::new(journal, machine_cap, opts.profile.clone());

        // 5a. Reconcile orphaned in-flight tasks from a prior daemon instance
        //     BEFORE serving begins (ADR-006). Any task in a non-terminal state
        //     was being driven by the dead prior process.
        startup::reconcile_orphaned_tasks(&delegation.journal);
        let state = Arc::new(SharedState {
            delegation,
            inbox_cursors: Mutex::new(HashMap::new()),
            opts,
        });

        Ok(Server {
            state,
            listener,
            socket_path,
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The bound socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// A handle that, when set, causes the accept loop to stop after its next
    /// wakeup. Used by [`Server::serve_until`] and the signal handler.
    pub fn shutdown_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    /// Serve until a SIGTERM/SIGINT arrives, then clean up and return. This is
    /// what the binary calls.
    pub fn serve_forever(self) -> Result<()> {
        let shutdown = self.shutdown_handle();
        install_signal_handler(shutdown, self.socket_path.clone());
        self.accept_loop()
    }

    /// Serve until the shared `shutdown` flag is set (test-friendly). Callers
    /// obtain the flag via [`Server::shutdown_handle`] before spawning the
    /// serving thread.
    pub fn serve_until(self) -> Result<()> {
        self.accept_loop()
    }

    /// The accept loop. One connection at a time is acceptable for M0; each is
    /// handled to completion (single request/response). A malformed request or
    /// a client I/O error never tears the loop down.
    fn accept_loop(self) -> Result<()> {
        // Non-blocking-ish shutdown: poll the flag between accepts by using a
        // short accept timeout via set_nonblocking + a tiny sleep loop.
        self.listener
            .set_nonblocking(true)
            .context("setting listener non-blocking")?;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    if let Err(e) = handle_connection(stream, &self.state) {
                        tracing::warn!(error = %e, "connection handler error");
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept error");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }

        // Clean shutdown: remove the socket file.
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!("daemon shut down cleanly");
        Ok(())
    }
}

/// Unlink any stale socket file, then bind + listen, then set mode 0600
/// (ADR-006). The CLI's auto-spawn race guarantees we only get here when no
/// live daemon exists, so a plain unlink-then-bind is correct.
fn bind_socket(socket_path: &Path) -> Result<UnixListener> {
    // Remove a stale socket file if present (ignore ENOENT).
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("unlinking stale socket {}", socket_path.display())
            })
        }
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding socket {}", socket_path.display()))?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", socket_path.display()))?;
    Ok(listener)
}

/// Install a SIGTERM/SIGINT handler that flips the shutdown flag and removes
/// the socket file, so the accept loop exits and the binary returns 0. Uses a
/// dedicated thread over the `signal-hook`-free rustix path via a self-pipe is
/// overkill for M0; instead we register a plain handler that sets the flag.
fn install_signal_handler(shutdown: Arc<AtomicBool>, socket_path: std::path::PathBuf) {
    // Store globals the extern "C" handler can reach. We only ever install one
    // handler per process, so statics are acceptable here.
    SHUTDOWN_FLAG.get_or_init(|| shutdown.clone());
    let _ = SOCKET_PATH.set(socket_path);

    // SAFETY: `handle_signal` is async-signal-safe — it only stores into an
    // AtomicBool via `OnceLock::get`. Socket removal happens in the accept loop
    // after it observes the flag, not in the handler.
    unsafe {
        libc_signal(SIG_TERM, handle_signal);
        libc_signal(SIG_INT, handle_signal);
    }
}

// Minimal libc `signal(2)` binding (avoids the `libc` crate; rustix does not
// expose signal-handler installation). SIGTERM=15, SIGINT=2 on Linux and macOS.
const SIG_TERM: i32 = 15;
const SIG_INT: i32 = 2;

type SigHandler = extern "C" fn(i32);

extern "C" {
    #[link_name = "signal"]
    fn libc_signal(signum: i32, handler: SigHandler) -> SigHandler;
}

static SHUTDOWN_FLAG: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
static SOCKET_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Async-signal-safe handler: just set the shutdown flag. The accept loop polls
/// it and performs the actual cleanup.
extern "C" fn handle_signal(_sig: i32) {
    if let Some(flag) = SHUTDOWN_FLAG.get() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Handle exactly one connection: read one JSON line, dispatch, write one JSON
/// line, flush, close. Malformed input yields `Response::Error` rather than a
/// panic (ADR-006).
fn handle_connection(stream: UnixStream, state: &SharedState) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("cloning stream")?);
    let mut write_half = stream;

    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("reading request line")?;

    let response = if n == 0 {
        // Client closed without sending anything.
        Response::Error {
            message: "empty request".to_string(),
        }
    } else {
        match serde_json::from_str::<Request>(line.trim_end()) {
            Ok(req) => dispatch(req, state),
            Err(e) => Response::Error {
                message: format!("malformed request: {e}"),
            },
        }
    };

    let mut out = serde_json::to_string(&response).context("serializing response")?;
    out.push('\n');
    write_half
        .write_all(out.as_bytes())
        .context("writing response")?;
    write_half.flush().context("flushing response")?;
    Ok(())
}

/// Dispatch a parsed request to its handler, producing a `Response`. All
/// journal access goes through the shared `Arc<Mutex<Journal>>`.
fn dispatch(req: Request, state: &SharedState) -> Response {
    let journal = &state.delegation.journal;
    match req {
        Request::Hello => Response::Hello {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
        },
        Request::Ps => {
            let j = journal.lock().expect("journal mutex poisoned");
            match j.list_tasks() {
                Ok(tasks) => Response::Ps { tasks },
                Err(e) => Response::Error {
                    message: format!("ps failed: {e}"),
                },
            }
        }
        Request::Doctor => doctor(&state.opts),
        Request::RegisterAdvisor { profile } => register_advisor(state, profile),
        Request::Delegate {
            advisor_session_id,
            repo_path,
            spec,
        } => delegate_handler(state, &advisor_session_id, &repo_path, *spec),
        Request::TaskStatus {
            advisor_session_id,
            state: filter,
        } => {
            let j = journal.lock().expect("journal mutex poisoned");
            match j.list_tasks_for_advisor(&advisor_session_id, filter.as_deref()) {
                Ok(tasks) => Response::TaskStatus { tasks },
                Err(e) => Response::Error {
                    message: format!("task_status failed: {e}"),
                },
            }
        }
        Request::DrainInbox { advisor_session_id } => drain_inbox(state, &advisor_session_id),
        Request::CloseTask {
            advisor_session_id,
            task_id,
            outcome,
            successor,
        } => close_task(state, &advisor_session_id, &task_id, &outcome, successor.as_deref()),
        Request::MergeTask {
            advisor_session_id,
            task_id,
        } => merge_task(state, &advisor_session_id, &task_id),
        Request::JournalQuery {
            advisor_session_id,
            query,
            params,
        } => journal_query(state, &advisor_session_id, &query, &params),
        Request::KillTask { task_id, kind } => kill_task(state, &task_id, &kind),
        Request::Search {
            advisor_session_id,
            queries,
        } => search_handler(state, &advisor_session_id, &queries),
        Request::FetchExtract {
            advisor_session_id,
            url,
            schema_fields,
        } => fetch_extract_handler(state, &advisor_session_id, &url, &schema_fields),
    }
}

/// `Search` (ADR-005 / ADR-007, amended): resolve the active profile's search
/// backend from `search.backend` (default `"anthropic"` when unset — nothing is
/// auto-disabled), then run the metadata-only search. There is **no fallback
/// between backends** — each is explicit:
///
/// - `"anthropic"` (or unset, the default) → Anthropic's server-side
///   `web_search` tool via the shim model (`roles.shim`), returning url+title
///   metadata only. A missing `ANTHROPIC_API_KEY` yields `backend_unavailable`.
/// - `"searxng"` → the self-hosted SearXNG instance at `search.endpoint`; an
///   unset/unreachable endpoint yields `backend_unavailable`.
/// - `"none"` → search is explicitly disabled: a loud `backend_unavailable`
///   without constructing a backend.
/// - any other value → `backend_unavailable: unknown search backend` (fail-loud).
fn search_handler(state: &SharedState, advisor_session_id: &str, queries: &[String]) -> Response {
    use maestro_journal::config::RoleModel;
    use maestro_shim::{AnthropicSearchBackend, SearxngBackend};

    let rp = match resolved_profile(state) {
        Ok(rp) => rp,
        Err(msg) => return Response::Error { message: msg },
    };

    // Default to "anthropic" when unset — nothing is auto-disabled per profile.
    let backend_kind = rp.search.backend.as_deref().unwrap_or("anthropic");

    match backend_kind {
        "anthropic" => {
            // Reuse the shim model; the shim role table may override the base-url.
            let model_name = rp.roles.shim_model().to_string();
            let base_url = match &rp.roles.shim {
                Some(RoleModel::Detailed(t)) => t.base_url.clone(),
                _ => None,
            };
            let backend = AnthropicSearchBackend::new(model_name.clone(), base_url);
            shim::run_search(
                &state.delegation.journal,
                advisor_session_id,
                &backend,
                &model_name,
                queries,
            )
        }
        "searxng" => {
            let backend = SearxngBackend::new(rp.search.endpoint.clone());
            shim::run_search(
                &state.delegation.journal,
                advisor_session_id,
                &backend,
                "searxng",
                queries,
            )
        }
        "none" => Response::Error {
            message: "backend_unavailable: search disabled on this profile (search.backend = none)"
                .to_string(),
        },
        other => Response::Error {
            message: format!("backend_unavailable: unknown search backend {other}"),
        },
    }
}

/// `FetchExtract` (ADR-005): resolve the shim extraction model
/// (`roles.shim` or the Haiku default) and its base-url override, then run the
/// fetch → readability → extract → validate pipeline with the production HTTP
/// fetcher via [`shim::run_fetch_extract`].
fn fetch_extract_handler(
    state: &SharedState,
    advisor_session_id: &str,
    url: &str,
    schema_fields: &[String],
) -> Response {
    use maestro_journal::config::RoleModel;
    use maestro_shim::AnthropicExtractionModel;

    let rp = match resolved_profile(state) {
        Ok(rp) => rp,
        Err(msg) => return Response::Error { message: msg },
    };
    let model_name = rp.roles.shim_model().to_string();
    // A `roles.shim = { model, base_url }` table can override the API base-url.
    let base_url = match &rp.roles.shim {
        Some(RoleModel::Detailed(t)) => t.base_url.clone(),
        _ => None,
    };
    let model = AnthropicExtractionModel::new(model_name.clone(), base_url);
    let fetcher = shim::http_fetcher();
    shim::run_fetch_extract(
        &state.delegation.journal,
        advisor_session_id,
        &fetcher,
        &model,
        &model_name,
        url,
        schema_fields,
        rp.shim.excerpt_cap_chars as usize,
    )
}

/// Resolve the active profile (ADR-007) for a shim request. Config is loaded
/// fresh so on-disk edits apply without a restart. An explicit-but-missing
/// profile is a loud error surfaced to the advisor.
fn resolved_profile(state: &SharedState) -> std::result::Result<resolve::ResolvedProfile, String> {
    let config = load_config();
    let env = std::env::var("MAESTRO_PROFILE").ok();
    resolve(state.opts.profile.as_deref(), env.as_deref(), &config).resolved
}

/// `KillTask`: break-glass kill of a running driven (PTY) session (ADR-006). Look
/// up the task's live [`maestro_driver::SessionHandle`]; if present, fire the kill
/// with the requested [`KillKind`] and reply `Killed`. Absent → the task is not
/// running a driven session, so reply `Error`. The worker records the terminal
/// `interrupted`/`failed` events when the driver returns.
fn kill_task(state: &SharedState, task_id: &str, kind: &str) -> Response {
    use maestro_driver::KillKind;
    let kill_kind = match kind {
        "human" => KillKind::Human,
        "advisor" => KillKind::Advisor,
        other => {
            return Response::Error {
                message: format!("kill_task: invalid kind {other:?} (want human|advisor)"),
            };
        }
    };
    match state.delegation.session_handle(task_id) {
        Some(handle) => {
            handle.request_kill(kill_kind);
            Response::Killed {
                task_id: task_id.to_string(),
            }
        }
        None => Response::Error {
            message: format!("task {task_id} is not a running driven session"),
        },
    }
}

/// `CloseTask`: resolve a `blocked` task (ADR-003). Requires the task's current
/// state to be `blocked` (else `Error`); `outcome` must be `abandoned` or
/// `superseded` (else `Error`). Records a terminal `failed(verification_failed)`
/// with `{outcome, superseded_by}`, and replies `Closed{task_id}`. Creating the
/// successor task is the advisor's separate `delegate`.
fn close_task(
    state: &SharedState,
    _advisor_session_id: &str,
    task_id: &str,
    outcome: &str,
    successor: Option<&str>,
) -> Response {
    use maestro_journal::domain::EventKind;

    if outcome != "abandoned" && outcome != "superseded" {
        return Response::Error {
            message: format!("close_task: invalid outcome {outcome:?} (want abandoned|superseded)"),
        };
    }

    let j = state.delegation.journal.lock().expect("journal mutex poisoned");
    match j.current_state(task_id) {
        Ok(Some(EventKind::Blocked)) => {}
        Ok(_) => {
            return Response::Error {
                message: format!("close_task: task {task_id} is not blocked"),
            };
        }
        Err(e) => {
            return Response::Error {
                message: format!("close_task: {e}"),
            };
        }
    }

    let payload = serde_json::json!({
        "kind": "verification_failed",
        "outcome": outcome,
        "superseded_by": successor,
    })
    .to_string();
    if let Err(e) = j.append_event(task_id, EventKind::Failed, Some(&payload)) {
        return Response::Error {
            message: format!("close_task: append failed: {e}"),
        };
    }
    Response::Closed {
        task_id: task_id.to_string(),
    }
}

/// `MergeTask`: explicit, advisor-initiated fast-forward merge of a passed task's
/// branch into its `base_ref` (ADR-006). This is NOT auto-merge — the daemon
/// never merges on its own; this runs ONLY on this request and is gated on the
/// task resting in `verify_passed` (passed, committed, awaiting merge). On a
/// successful fast-forward it emits `merged`, removes the worktree, best-effort
/// deletes the task branch, and replies `Merged`. A non-fast-forward / missing
/// branch / non-branch base is an `Error` and NO `merged` event is written.
fn merge_task(state: &SharedState, _advisor_session_id: &str, task_id: &str) -> Response {
    use maestro_journal::domain::EventKind;

    // Gate on the resting verify_passed state, then read (repo_path, base_ref),
    // releasing the journal lock before shelling out to git (the merge does no
    // journal I/O and we re-lock to record `merged`).
    let (repo_path, base_ref) = {
        let j = state.delegation.journal.lock().expect("journal mutex poisoned");
        match j.current_state(task_id) {
            Ok(Some(EventKind::VerifyPassed)) => {}
            Ok(Some(EventKind::Merged)) => {
                return Response::Error {
                    message: format!("merge_task: task {task_id} is already merged"),
                };
            }
            Ok(_) => {
                return Response::Error {
                    message: format!(
                        "merge_task: task {task_id} is not in a merge-ready (verify_passed) state"
                    ),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: format!("merge_task: {e}"),
                };
            }
        }
        match j.task_repo_and_base(task_id) {
            Ok((Some(repo), base)) => (repo, base),
            Ok((None, _)) => {
                return Response::Error {
                    message: format!(
                        "merge_task: task {task_id} has no recorded repo path; merge manually"
                    ),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: format!("merge_task: {e}"),
                };
            }
        }
    };

    let repo = std::path::PathBuf::from(&repo_path);
    let outcome = match worktree::merge_task_branch(&repo, &base_ref, task_id) {
        Ok(o) => o,
        Err(e) => {
            // No `merged` event on failure.
            return Response::Error {
                message: format!("merge_task: {e:#}"),
            };
        }
    };

    // Record the terminal `merged` event.
    let payload = serde_json::json!({
        "base_ref": outcome.base_ref,
        "branch": outcome.branch,
        "merged_sha": outcome.merged_sha,
    })
    .to_string();
    {
        let j = state.delegation.journal.lock().expect("journal mutex poisoned");
        if let Err(e) = j.append_event(task_id, EventKind::Merged, Some(&payload)) {
            return Response::Error {
                message: format!("merge_task: append merged failed: {e}"),
            };
        }
    }

    // Best-effort cleanup: detach the task's worktree (which still has the branch
    // checked out) then delete the merged branch. The branch was merged (base_ref
    // now contains it), so deletion is safe. The worktree lives at the
    // conventional `<state_dir>/worktrees/<task_id>` path.
    worktree::remove(&repo, &worktree::worktree_path(task_id));
    worktree::delete_branch(&repo, task_id);

    Response::Merged {
        task_id: task_id.to_string(),
    }
}

/// `JournalQuery`: named, read-only journal queries (ADR-001 telemetry). Unknown
/// query → `Error`. Supported:
/// - `verifier_reports` (`{task_id}`) → the report chain (attempt, independence,
///   verdict, findings);
/// - `trace` (`{task_id}`) → the task's event chain (seq, kind, ts, payload).
/// - `routing_report` (no params) → the ADR-001 telemetry aggregate grouped by
///   `(tier, model, containment_level)` with terminal + failure-kind counts and
///   token sums.
fn journal_query(
    state: &SharedState,
    _advisor_session_id: &str,
    query: &str,
    params: &serde_json::Value,
) -> Response {
    let task_id = params.get("task_id").and_then(|v| v.as_str());
    let j = state.delegation.journal.lock().expect("journal mutex poisoned");
    match query {
        "verifier_reports" => {
            let Some(task_id) = task_id else {
                return Response::Error {
                    message: "journal_query verifier_reports: missing params.task_id".into(),
                };
            };
            match j.verifier_reports_for_task(task_id) {
                Ok(rows) => {
                    let arr: Vec<serde_json::Value> = rows
                        .into_iter()
                        .map(|r| {
                            let report: serde_json::Value = serde_json::from_str(&r.report)
                                .unwrap_or(serde_json::Value::Null);
                            let verdict = report.get("verdict").cloned().unwrap_or(serde_json::Value::Null);
                            let findings = report.get("findings").cloned().unwrap_or(serde_json::Value::Null);
                            serde_json::json!({
                                "attempt": r.attempt,
                                "independence": r.independence.as_str(),
                                "verdict": verdict,
                                "findings": findings,
                            })
                        })
                        .collect();
                    Response::JournalResult {
                        value: serde_json::Value::Array(arr),
                    }
                }
                Err(e) => Response::Error {
                    message: format!("journal_query verifier_reports: {e}"),
                },
            }
        }
        "trace" => {
            let Some(task_id) = task_id else {
                return Response::Error {
                    message: "journal_query trace: missing params.task_id".into(),
                };
            };
            match j.event_chain(task_id) {
                Ok(events) => {
                    let arr: Vec<serde_json::Value> = events
                        .into_iter()
                        .map(|e| {
                            let payload = e
                                .payload
                                .as_deref()
                                .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
                            serde_json::json!({
                                "seq": e.seq,
                                "kind": e.kind.as_str(),
                                "ts": e.ts,
                                "payload": payload,
                            })
                        })
                        .collect();
                    Response::JournalResult {
                        value: serde_json::Value::Array(arr),
                    }
                }
                Err(e) => Response::Error {
                    message: format!("journal_query trace: {e}"),
                },
            }
        }
        "routing_report" => match j.routing_report() {
            Ok(value) => Response::JournalResult { value },
            Err(e) => Response::Error {
                message: format!("journal_query routing_report: {e}"),
            },
        },
        other => Response::Error {
            message: format!("journal_query: unknown query {other:?}"),
        },
    }
}

/// `RegisterAdvisor`: create an advisor row (resolving its informational model
/// from the active profile) and return the minted `advisor_session_id`.
fn register_advisor(state: &SharedState, profile: Option<String>) -> Response {
    // The request's `profile` overrides the daemon flag for this advisor.
    let flag = profile.as_deref().or(state.opts.profile.as_deref());
    let config = load_config();
    let env = std::env::var("MAESTRO_PROFILE").ok();
    let res = resolve(flag, env.as_deref(), &config);
    let profile_name = res.profile.clone();
    let (advisor_model, advisor_context) = match &res.resolved {
        Ok(rp) => (
            rp.advisor.model.clone().unwrap_or_else(|| "unknown".to_string()),
            rp.advisor.context.clone().unwrap_or_else(|| "standard".to_string()),
        ),
        Err(_) => ("unknown".to_string(), "standard".to_string()),
    };
    let j = state.delegation.journal.lock().expect("journal mutex poisoned");
    match j.create_advisor(&profile_name, &advisor_model, &advisor_context) {
        Ok(advisor_session_id) => Response::RegisterAdvisor { advisor_session_id },
        Err(e) => Response::Error {
            message: format!("register_advisor failed: {e}"),
        },
    }
}

/// `Delegate`: hand off to the pipeline. On a pre-spawn validation failure we
/// still journal a terminal task (so `task_status` reflects it) and surface an
/// error to the caller.
fn delegate_handler(
    state: &SharedState,
    advisor_session_id: &str,
    repo_path: &str,
    spec: maestro_journal::spec::TaskSpec,
) -> Response {
    match delegate::delegate(&state.delegation, advisor_session_id, repo_path, spec) {
        Ok(task_id) => Response::Delegate { task_id },
        Err(e) => {
            // Record a terminal task so the lifecycle is observable even for a
            // pre-spawn rejection.
            if let Some(task_id) =
                delegate::record_rejected_task(&state.delegation, advisor_session_id, &e)
            {
                Response::Delegate { task_id }
            } else {
                Response::Error {
                    message: format!("delegate rejected ({}): {}", failure_kind_str(&e), e.message_public()),
                }
            }
        }
    }
}

/// The failure-taxonomy kind string for a [`DelegateError`], for the error path.
fn failure_kind_str(e: &DelegateError) -> &'static str {
    e.kind_str()
}

/// `DrainInbox`: return the advisor's inbox events since the in-memory cursor,
/// then advance the cursor to the last returned event_id. When the advisor's
/// `advisor_context` is `"1m"`, event payloads are inlined into each item's
/// `detail` field; any other value (including `"standard"` or absent) delivers
/// summary-only items (ADR-007).
fn drain_inbox(state: &SharedState, advisor_session_id: &str) -> Response {
    let cursor = {
        let cursors = state.inbox_cursors.lock().expect("inbox cursors poisoned");
        cursors.get(advisor_session_id).cloned()
    };
    // Resolve the advisor's context to determine inline mode.
    let inline_detail = {
        let j = state.delegation.journal.lock().expect("journal mutex poisoned");
        match j.advisor_context(advisor_session_id) {
            Ok(ctx) => ctx.as_deref() == Some("1m"),
            // Unknown advisor or DB error → default to no inlining.
            Err(_) => false,
        }
    };
    let items = {
        let j = state.delegation.journal.lock().expect("journal mutex poisoned");
        j.advisor_inbox_since(advisor_session_id, cursor.as_deref(), inline_detail)
    };
    match items {
        Ok(mut items) => {
            if let Some(last) = items.last() {
                let mut cursors = state.inbox_cursors.lock().expect("inbox cursors poisoned");
                cursors.insert(advisor_session_id.to_string(), last.event_id.clone());
            }
            // M6: append an advisory "daily total" line (ADR-001 telemetry). This
            // is best-effort synthetic context, NOT a real event — it is not
            // persisted, does not advance the cursor, and a query error is
            // swallowed so the drain never breaks. Today's UTC date prefix is the
            // first 10 chars of an RFC3339 timestamp (`YYYY-MM-DD`).
            let day_prefix = maestro_journal::now_iso8601();
            let day_prefix = &day_prefix[..10.min(day_prefix.len())];
            if let Ok((tin, tout, sessions)) = {
                let j = state.delegation.journal.lock().expect("journal mutex poisoned");
                j.day_token_totals(day_prefix)
            } {
                if sessions > 0 {
                    items.push(maestro_journal::proto::InboxItem {
                        event_id: String::new(),
                        task_id: String::new(),
                        ts: maestro_journal::now_iso8601(),
                        kind: "daily_total".to_string(),
                        summary: format!(
                            "today: {tin} in / {tout} out tokens across {sessions} sessions"
                        ),
                        detail: None,
                    });
                }
            }
            Response::Inbox { items }
        }
        Err(e) => Response::Error {
            message: format!("drain_inbox failed: {e}"),
        },
    }
}

/// Resolve the machine concurrency cap (ADR-003) from the active profile,
/// defaulting to 4 on any resolution failure.
fn resolve_machine_cap(profile_flag: Option<&str>) -> u32 {
    let config = load_config();
    let env = std::env::var("MAESTRO_PROFILE").ok();
    match resolve(profile_flag, env.as_deref(), &config).resolved {
        Ok(rp) => rp.concurrency.machine_cap,
        Err(_) => 4,
    }
}

/// Build the `Doctor` response: resolve the active profile (ADR-007) and attach
/// the capability probe (ADR-004). Config is loaded fresh so `doctor` reflects
/// on-disk edits without a restart. `maestro doctor` must work on a machine with
/// no config file at all.
fn doctor(opts: &Options) -> Response {
    let config = load_config();
    let env_profile = std::env::var("MAESTRO_PROFILE").ok();
    let res = resolve(
        opts.profile.as_deref(),
        env_profile.as_deref(),
        &config,
    );

    let resolved_profile = resolved_profile_json(&res);
    let probe = serde_json::to_value(maestro_sandbox::probe())
        .unwrap_or_else(|e| serde_json::json!({ "error": format!("probe serialize: {e}") }));

    // Build the per-role model-auth check (offline: credential presence +
    // PATH scan only, no network calls — ADR-004).
    let model_auth = match &res.resolved {
        Ok(rp) => model_auth::build_model_auth(
            &rp.roles,
            &model_auth::real_env_has,
            &model_auth::real_cmd_on_path,
        ),
        Err(_) => serde_json::json!({ "error": "profile resolution failed; cannot check model auth" }),
    };

    Response::Doctor(DoctorReport {
        profile: res.profile,
        resolved_profile,
        probe,
        model_auth,
    })
}

/// Load the config from `config_path()` if it exists; otherwise `Config::default()`.
/// A parse error is surfaced as a default config plus a logged warning (doctor
/// still works; the resolved view will reflect defaults). We intentionally do
/// NOT fail startup on a malformed config — the operator needs `doctor` to run.
fn load_config() -> Config {
    let path = paths::config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match Config::from_toml_str(&s) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config parse failed; using defaults");
                Config::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "config read failed; using defaults");
            Config::default()
        }
    }
}
