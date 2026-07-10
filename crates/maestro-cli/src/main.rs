//! maestro — operator CLI: ps / kill / logs / doctor / daemon / init (ADR-006).
//!
//! M0 scope: `ps`, `doctor`, `init`, `daemon` control, and the auto-spawn race
//! client. `kill` and `logs` are declared but stubbed (real in M3).

mod advise;
mod client;
mod daemon;
mod init;
mod progress;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use maestro_journal::paths;
use maestro_journal::proto::{Request, Response};

/// The maestro operator CLI.
#[derive(Debug, Parser)]
#[command(
    name = "maestro",
    version,
    about = "maestro operator CLI (ps / doctor / init / daemon control)"
)]
struct Cli {
    /// Active profile override (highest precedence: flag > MAESTRO_PROFILE >
    /// default_profile). Forwarded to an auto-spawned daemon.
    #[arg(long, global = true)]
    profile: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Resolved profile + capability probe (auto-spawns the daemon).
    Doctor,
    /// List tasks (auto-spawns the daemon).
    Ps,
    /// Create the starter config + directories and ship the advisor's
    /// client-side lockdown into the current repo (idempotent).
    Init,
    /// Launch the advisor's Claude Code session inside a read-only-repo mount
    /// (ADR-006). The repo working tree is bind-mounted read-only; only the
    /// advisor scratch dir and the opt-in `advisor.writable_paths` are writable.
    Advise {
        /// Run `bash -lc <cmd>` inside the mount instead of interactive `claude`
        /// (for scripting/testing the read-only property).
        #[arg(long)]
        exec: Option<String>,
    },
    /// Delegate a task from a JSON TaskSpec (auto-spawns the daemon).
    Delegate {
        /// Repository the worktree branches from.
        #[arg(long)]
        repo: String,
        /// Path to a JSON TaskSpec file.
        #[arg(long)]
        spec: String,
    },
    /// Show an advisor's tasks and their derived state (auto-spawns the daemon).
    TaskStatus {
        /// Advisor session id (from `maestro delegate`).
        #[arg(long)]
        advisor: String,
        /// Optional derived-state filter (e.g. `checks_passed`, `failed`).
        #[arg(long)]
        state: Option<String>,
    },
    /// One-shot situational digest of an advisor's tasks (ADR-009): per-task
    /// state/tier/age + a needs-attention marker, grouped counts, an ACTIONABLE
    /// section, and a one-line summary. Always exits 0 (it is a report).
    Status {
        /// Advisor session id (from `maestro delegate`).
        #[arg(long)]
        advisor: String,
        /// Optionally scope the digest to a single task id.
        #[arg(long)]
        task: Option<String>,
    },
    /// Block until a tracked task needs advisor attention (ADR-009): polls
    /// `task-status` and exits 0 the moment any tracked task is actionable
    /// (`verify_passed`/`blocked`/`failed`), or all tracked tasks are terminal.
    /// Run it in the background so the harness re-invokes the advisor exactly
    /// when a decision is due.
    Watch {
        /// Advisor session id (from `maestro delegate`).
        #[arg(long)]
        advisor: String,
        /// Task id(s) to track. Repeatable. Default: all of the advisor's tasks
        /// that are non-terminal at the first poll.
        #[arg(long = "task")]
        tasks: Vec<String>,
        /// Poll interval in seconds (default 5).
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Optional backstop: give up after this many seconds and exit non-zero
        /// (code 2). Off by default (block indefinitely).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Close a `blocked` task, recording its outcome (ADR-003).
    CloseTask {
        /// Advisor session id.
        #[arg(long)]
        advisor: String,
        /// Task id to close (must be `blocked`).
        #[arg(long)]
        task: String,
        /// `abandoned` or `superseded`.
        #[arg(long)]
        outcome: String,
        /// Successor task id (for `superseded`).
        #[arg(long)]
        successor: Option<String>,
    },
    /// Fast-forward-merge a passed task's branch into its base ref (ADR-006).
    /// The task must be in the `verify_passed` state; merge is fast-forward-only.
    MergeTask {
        /// Advisor session id.
        #[arg(long)]
        advisor: String,
        /// Task id to merge (must be `verify_passed`).
        #[arg(long)]
        task: String,
    },
    /// Run a named, read-only journal query and pretty-print its JSON result.
    JournalQuery {
        /// Advisor session id.
        #[arg(long)]
        advisor: String,
        /// Query name (e.g. `verifier_reports`, `trace`).
        #[arg(long)]
        query: String,
        /// Task id the query is scoped to.
        #[arg(long)]
        task: String,
    },
    /// Daemon lifecycle control.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Break-glass kill path (ADR-006). M3 implements single-task kill; the
    /// `--advisor` / `--all` fan-out flags are parsed but not yet implemented.
    Kill {
        /// Task id to kill.
        task_id: Option<String>,
        /// Kill all tasks for an advisor session (not implemented in M3).
        #[arg(long)]
        advisor: Option<String>,
        /// Kill everything (not implemented in M3).
        #[arg(long)]
        all: bool,
    },
    /// Print a task's captured PTY log (its latest driven session's log_path).
    Logs {
        /// Task id whose log to show.
        task_id: Option<String>,
        /// Follow the log (not implemented in M3).
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Report whether the daemon is running, its pid, and protocol version.
    Status,
    /// Ensure the daemon is running (auto-spawn); print its pid.
    Start,
    /// Ask the daemon to terminate (SIGTERM to its pid).
    Stop,
    /// Stop (if running), wait for the socket to disappear, then start.
    Restart,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("maestro: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let profile = cli.profile.as_deref();

    match cli.command {
        Command::Doctor => cmd_doctor(profile),
        Command::Ps => cmd_ps(profile),
        Command::Init => cmd_init(),
        Command::Advise { exec } => advise::run(profile, exec.as_deref()),
        Command::Delegate { repo, spec } => cmd_delegate(profile, &repo, &spec),
        Command::TaskStatus { advisor, state } => {
            cmd_task_status(profile, &advisor, state.as_deref())
        }
        Command::Status { advisor, task } => cmd_status(profile, &advisor, task.as_deref()),
        Command::Watch {
            advisor,
            tasks,
            interval,
            timeout,
        } => cmd_watch(profile, &advisor, &tasks, interval, timeout),
        Command::CloseTask {
            advisor,
            task,
            outcome,
            successor,
        } => cmd_close_task(profile, &advisor, &task, &outcome, successor.as_deref()),
        Command::MergeTask { advisor, task } => cmd_merge_task(profile, &advisor, &task),
        Command::JournalQuery {
            advisor,
            query,
            task,
        } => cmd_journal_query(profile, &advisor, &query, &task),
        Command::Daemon { action } => match action {
            DaemonAction::Status => cmd_daemon_status(),
            DaemonAction::Start => cmd_daemon_start(profile),
            DaemonAction::Stop => cmd_daemon_stop(),
            DaemonAction::Restart => cmd_daemon_restart(profile),
        },
        Command::Kill {
            task_id,
            advisor,
            all,
        } => cmd_kill(profile, task_id.as_deref(), advisor.as_deref(), all),
        Command::Logs { task_id, follow } => cmd_logs(task_id.as_deref(), follow),
    }
}

/// Ensure a live, compatible daemon (auto-spawn race), then send `req`.
fn ensured_request(profile: Option<&str>, req: &Request) -> Result<Response> {
    daemon::ensure_daemon_real(profile).context("ensuring the daemon is running")?;
    client::request_at(&paths::socket_path(), req)
}

fn cmd_doctor(profile: Option<&str>) -> Result<()> {
    let resp = ensured_request(profile, &Request::Doctor)?;
    match resp {
        Response::Doctor(report) => {
            println!("active profile: {}", report.profile);
            println!();
            println!("resolved profile:");
            println!(
                "{}",
                serde_json::to_string_pretty(&report.resolved_profile)
                    .unwrap_or_else(|_| report.resolved_profile.to_string())
            );
            println!();
            println!("capability probe:");
            print_probe(&report.probe);
            println!();
            println!("model auth:");
            print_model_auth(&report.model_auth);
            Ok(())
        }
        Response::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected response to Doctor: {other:?}"),
    }
}

/// Pretty-print the capability probe. Known keys are surfaced in a readable
/// order; the full JSON is printed as a fallback for anything unrecognized.
fn print_probe(probe: &serde_json::Value) {
    const KEYS: &[&str] = &[
        "os",
        "nix_flakes",
        "bwrap",
        "seatbelt",
        "container_runtime",
        "container_runtime_functional",
        "max_level_available",
    ];
    if let Some(obj) = probe.as_object() {
        for key in KEYS {
            if let Some(v) = obj.get(*key) {
                println!("  {key}: {}", render_scalar(v));
            }
        }
        // Any extra keys the daemon reported that we did not name explicitly.
        for (k, v) in obj {
            if !KEYS.contains(&k.as_str()) {
                println!("  {k}: {}", render_scalar(v));
            }
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(probe).unwrap_or_else(|_| probe.to_string())
        );
    }
}

/// Pretty-print the per-role model auth section. Each configured role is
/// printed on one line: `  <role>: <model> [<backend>] — <status>`.
/// Falls back to raw JSON if the value is not an object.
fn print_model_auth(model_auth: &serde_json::Value) {
    const ROLE_ORDER: &[&str] = &["tier0", "tier1", "tier2", "verifier_floor", "shim"];
    if let Some(obj) = model_auth.as_object() {
        // Print in canonical order first, then any unexpected extras.
        for role in ROLE_ORDER {
            if let Some(entry) = obj.get(*role) {
                let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let backend = entry.get("backend").and_then(|v| v.as_str()).unwrap_or("?");
                let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {role}: {model} [{backend}] \u{2014} {status}");
            }
        }
        // Any extra keys not in ROLE_ORDER.
        for (k, entry) in obj {
            if !ROLE_ORDER.contains(&k.as_str()) {
                let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let backend = entry.get("backend").and_then(|v| v.as_str()).unwrap_or("?");
                let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {k}: {model} [{backend}] \u{2014} {status}");
            }
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(model_auth).unwrap_or_else(|_| model_auth.to_string())
        );
    }
}

/// Render a scalar JSON value without surrounding quotes for strings.
fn render_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn cmd_ps(profile: Option<&str>) -> Result<()> {
    let resp = ensured_request(profile, &Request::Ps)?;
    match resp {
        Response::Ps { tasks } => {
            if tasks.is_empty() {
                println!("no tasks");
                return Ok(());
            }
            // Simple table: task_id, tier, state, title.
            println!("{:<28} {:<5} {:<16} TITLE", "TASK_ID", "TIER", "STATE");
            for t in tasks {
                println!(
                    "{:<28} {:<5} {:<16} {}",
                    t.task_id,
                    t.tier.as_int(),
                    t.state,
                    t.title
                );
            }
            Ok(())
        }
        Response::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected response to Ps: {other:?}"),
    }
}

/// `maestro delegate --repo <path> --spec <spec.json>`: register an advisor,
/// read the spec, delegate it, and print both the task id and advisor id so the
/// operator can poll with `maestro task-status --advisor <id>`.
fn cmd_delegate(profile: Option<&str>, repo: &str, spec_path: &str) -> Result<()> {
    // Canonicalize the repo to an absolute path.
    let repo_abs = std::fs::canonicalize(repo)
        .with_context(|| format!("resolving repo path {repo}"))?;

    // Read + parse the spec.
    let spec_text = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading spec file {spec_path}"))?;
    let spec: maestro_journal::spec::TaskSpec = serde_json::from_str(&spec_text)
        .with_context(|| format!("parsing TaskSpec from {spec_path}"))?;

    // Register an advisor session.
    let advisor_session_id = match ensured_request(
        profile,
        &Request::RegisterAdvisor {
            profile: profile.map(String::from),
        },
    )? {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        Response::Error { message } => bail!("daemon error registering advisor: {message}"),
        other => bail!("unexpected response to RegisterAdvisor: {other:?}"),
    };

    // Delegate.
    let repo_path = repo_abs.to_string_lossy().to_string();
    let resp = ensured_request(
        profile,
        &Request::Delegate {
            advisor_session_id: advisor_session_id.clone(),
            repo_path,
            spec: Box::new(spec),
        },
    )?;
    match resp {
        Response::Delegate { task_id } => {
            println!("task_id: {task_id}");
            println!("advisor: {advisor_session_id}");
            println!("poll with: maestro task-status --advisor {advisor_session_id}");
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on delegate: {message}"),
        other => bail!("unexpected response to Delegate: {other:?}"),
    }
}

/// `maestro task-status --advisor <id> [--state <s>]`: print the advisor's tasks
/// as a table (task_id, tier, state, title).
fn cmd_task_status(profile: Option<&str>, advisor: &str, state: Option<&str>) -> Result<()> {
    let resp = ensured_request(
        profile,
        &Request::TaskStatus {
            advisor_session_id: advisor.to_string(),
            state: state.map(String::from),
        },
    )?;
    match resp {
        Response::TaskStatus { tasks } => {
            if tasks.is_empty() {
                println!("no tasks");
                return Ok(());
            }
            println!("{:<28} {:<5} {:<16} TITLE", "TASK_ID", "TIER", "STATE");
            for t in tasks {
                println!(
                    "{:<28} {:<5} {:<16} {}",
                    t.task_id,
                    t.tier.as_int(),
                    t.state,
                    t.title
                );
            }
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on task-status: {message}"),
        other => bail!("unexpected response to TaskStatus: {other:?}"),
    }
}

/// `maestro status --advisor <id> [--task <id>]`: a one-shot situational digest
/// (ADR-009 component C). Always exits 0 — it is a report. Reuses the read-side
/// `Request::TaskStatus`; all formatting lives in `progress::render_status`.
fn cmd_status(profile: Option<&str>, advisor: &str, task: Option<&str>) -> Result<()> {
    let mut send = |req: &Request| ensured_request(profile, req);
    progress::run_status(&mut send, advisor, task)
}

/// `maestro watch --advisor <id> [--task <id>]... [--interval <s>] [--timeout <s>]`:
/// block until a tracked task needs advisor attention (ADR-009 component B).
/// Exits 0 when a tracked task is actionable or all tracked tasks are terminal;
/// exits 2 if the optional `--timeout` backstop fires. Client-side poll loop over
/// `Request::TaskStatus` — no daemon protocol/control-flow change.
fn cmd_watch(
    profile: Option<&str>,
    advisor: &str,
    tasks: &[String],
    interval: u64,
    timeout: Option<u64>,
) -> Result<()> {
    let mut send = |req: &Request| ensured_request(profile, req);
    let mut sleep = |d: std::time::Duration| std::thread::sleep(d);
    let mut now = std::time::Instant::now;
    let outcome = progress::run_watch(
        &mut send,
        &mut sleep,
        &mut now,
        advisor,
        tasks,
        std::time::Duration::from_secs(interval.max(1)),
        timeout.map(std::time::Duration::from_secs),
    )?;
    match outcome {
        progress::WatchOutcome::Returned => Ok(()),
        // A backstop timeout is a distinct, non-error exit code (2) so a caller
        // can tell "decision due" (0) from "gave up waiting" (2).
        progress::WatchOutcome::TimedOut => std::process::exit(2),
    }
}

/// `maestro close-task --advisor <id> --task <id> --outcome <o> [--successor <id>]`:
/// close a blocked task, recording its outcome (ADR-003).
fn cmd_close_task(
    profile: Option<&str>,
    advisor: &str,
    task: &str,
    outcome: &str,
    successor: Option<&str>,
) -> Result<()> {
    let resp = ensured_request(
        profile,
        &Request::CloseTask {
            advisor_session_id: advisor.to_string(),
            task_id: task.to_string(),
            outcome: outcome.to_string(),
            successor: successor.map(String::from),
        },
    )?;
    match resp {
        Response::Closed { task_id } => {
            println!("closed task {task_id} ({outcome})");
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on close-task: {message}"),
        other => bail!("unexpected response to CloseTask: {other:?}"),
    }
}

/// `maestro merge-task --advisor <id> --task <id>`: fast-forward-merge a passed
/// task's branch into its base ref (ADR-006). Fast-forward-only; requires the
/// task to be in the `verify_passed` state.
fn cmd_merge_task(profile: Option<&str>, advisor: &str, task: &str) -> Result<()> {
    let resp = ensured_request(
        profile,
        &Request::MergeTask {
            advisor_session_id: advisor.to_string(),
            task_id: task.to_string(),
        },
    )?;
    match resp {
        Response::Merged { task_id } => {
            println!("merged task {task_id}");
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on merge-task: {message}"),
        other => bail!("unexpected response to MergeTask: {other:?}"),
    }
}

/// `maestro journal-query --advisor <id> --query <name> --task <id>`: run a named
/// read-only journal query and pretty-print its JSON result.
fn cmd_journal_query(profile: Option<&str>, advisor: &str, query: &str, task: &str) -> Result<()> {
    let resp = ensured_request(
        profile,
        &Request::JournalQuery {
            advisor_session_id: advisor.to_string(),
            query: query.to_string(),
            params: serde_json::json!({ "task_id": task }),
        },
    )?;
    match resp {
        Response::JournalResult { value } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            );
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on journal-query: {message}"),
        other => bail!("unexpected response to JournalQuery: {other:?}"),
    }
}

/// `maestro kill <task-id>`: break-glass kill of a running driven session
/// (ADR-006). Talks to the daemon directly — no model in the path. The daemon
/// records the terminal `interrupted`/`failed(interrupted_human)` events.
fn cmd_kill(
    profile: Option<&str>,
    task_id: Option<&str>,
    advisor: Option<&str>,
    all: bool,
) -> Result<()> {
    if all || advisor.is_some() {
        bail!("maestro kill: --all / --advisor fan-out is not implemented in M3; kill by task id");
    }
    let task_id = task_id
        .ok_or_else(|| anyhow::anyhow!("maestro kill: a task id is required"))?;
    let resp = ensured_request(
        profile,
        &Request::KillTask {
            task_id: task_id.to_string(),
            kind: "human".to_string(),
        },
    )?;
    match resp {
        Response::Killed { task_id } => {
            println!("kill requested for task {task_id} (interrupted_human)");
            Ok(())
        }
        Response::Error { message } => bail!("daemon error on kill: {message}"),
        other => bail!("unexpected response to Kill: {other:?}"),
    }
}

/// `maestro logs <task-id>`: print the task's latest driven session's captured
/// PTY log (`sessions.log_path`). Reads the journal DB directly (WAL allows a
/// concurrent reader alongside the daemon). `--follow` is not implemented in M3.
fn cmd_logs(task_id: Option<&str>, follow: bool) -> Result<()> {
    if follow {
        eprintln!("maestro logs: --follow is not implemented in M3; printing the log once");
    }
    let task_id = task_id
        .ok_or_else(|| anyhow::anyhow!("maestro logs: a task id is required"))?;
    let db_path = paths::journal_db_path();
    let journal = maestro_journal::Journal::open(
        db_path.to_str().context("journal db path is not valid UTF-8")?,
    )
    .with_context(|| format!("opening journal at {}", db_path.display()))?;
    let sessions = journal
        .sessions_for_task(task_id)
        .with_context(|| format!("reading sessions for task {task_id}"))?;
    let log_path = sessions
        .iter()
        .find_map(|s| s.log_path.clone())
        .ok_or_else(|| anyhow::anyhow!("no captured log for task {task_id}"))?;
    let contents = std::fs::read_to_string(&log_path)
        .with_context(|| format!("reading log {log_path}"))?;
    print!("{contents}");
    Ok(())
}

fn cmd_init() -> Result<()> {
    let report = init::run()?;
    let config = paths::config_path();
    if report.wrote_config {
        println!("wrote starter config: {}", config.display());
    } else {
        println!("config already exists, left intact: {}", config.display());
    }
    println!("ensured directories:");
    println!("  data:  {}", paths::data_dir().display());
    println!("  state: {}", paths::state_dir().display());

    let repo = std::env::current_dir().unwrap_or_default();
    let ld = &report.lockdown;
    println!("advisor lockdown in {}:", repo.display());
    println!("  .claude/settings.json  ({}) — deny Edit/Write/Bash", ld.settings.label());
    println!("  .mcp.json              ({}) — registered maestro MCP server", ld.mcp.label());
    println!("  CLAUDE.md              ({}) — advisor role pointer", ld.claude_md.label());
    println!(
        "note: deny rules are client-side defense-in-depth; the load-bearing control is the \
         read-only mount from `maestro advise`."
    );
    Ok(())
}

fn cmd_daemon_status() -> Result<()> {
    match daemon::try_connect() {
        Some(daemon::HelloOutcome::Compatible { pid }) => {
            println!(
                "daemon: running (pid {pid}, protocol version {})",
                maestro_journal::proto::PROTOCOL_VERSION
            );
        }
        Some(daemon::HelloOutcome::Incompatible { reported }) => {
            println!(
                "daemon: running but INCOMPATIBLE (reports protocol version {reported}, this CLI \
                 speaks {}). Run `maestro daemon restart`.",
                maestro_journal::proto::PROTOCOL_VERSION
            );
        }
        None => println!("daemon: not running"),
    }
    Ok(())
}

fn cmd_daemon_start(profile: Option<&str>) -> Result<()> {
    let out = daemon::ensure_daemon_real(profile)?;
    if out.spawned > 0 {
        println!("daemon: started (pid {})", out.pid);
    } else {
        println!("daemon: already running (pid {})", out.pid);
    }
    Ok(())
}

fn cmd_daemon_stop() -> Result<()> {
    let resp = match client::request_at(&paths::socket_path(), &Request::Hello) {
        Ok(r) => r,
        Err(_) => {
            println!("daemon: not running");
            return Ok(());
        }
    };
    let pid = match resp {
        Response::Hello { pid, .. } => pid,
        Response::Error { message } => bail!("daemon error on Hello: {message}"),
        other => bail!("unexpected response to Hello: {other:?}"),
    };
    kill_pid(pid, rustix::process::Signal::Term)
        .with_context(|| format!("sending SIGTERM to daemon pid {pid}"))?;
    println!("daemon: sent SIGTERM to pid {pid}");
    Ok(())
}

fn cmd_daemon_restart(profile: Option<&str>) -> Result<()> {
    // Stop if running.
    let was_running = match client::request_at(&paths::socket_path(), &Request::Hello) {
        Ok(Response::Hello { pid, .. }) => {
            kill_pid(pid, rustix::process::Signal::Term)
                .with_context(|| format!("sending SIGTERM to daemon pid {pid}"))?;
            println!("daemon: sent SIGTERM to pid {pid}");
            true
        }
        _ => {
            println!("daemon: not running (nothing to stop)");
            false
        }
    };

    if was_running {
        // Wait for the socket to become unresponsive before restarting.
        wait_for_socket_gone(std::time::Duration::from_millis(50), 100);
    }

    let out = daemon::ensure_daemon_real(profile).context("starting daemon after restart")?;
    println!("daemon: restarted (pid {})", out.pid);
    Ok(())
}

/// Poll until the daemon socket stops answering `Hello` (bounded).
fn wait_for_socket_gone(interval: std::time::Duration, tries: u32) {
    for i in 0..tries {
        if daemon::try_connect().is_none() {
            return;
        }
        if i + 1 < tries {
            std::thread::sleep(interval);
        }
    }
}

/// Send `signal` to `pid` via rustix. `pid` is a u32 from the wire handshake.
fn kill_pid(pid: u32, signal: rustix::process::Signal) -> Result<()> {
    let raw = i32::try_from(pid).context("daemon pid out of range")?;
    let pid = rustix::process::Pid::from_raw(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid daemon pid {pid}"))?;
    rustix::process::kill_process(pid, signal).context("kill_process")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    use maestro_journal::proto::PROTOCOL_VERSION;

    /// AC6: the wire client and daemon protocol agree. A throwaway
    /// `UnixListener` reads one line, asserts it parses as `Request::Hello`, and
    /// writes a `Response::Hello`; the client parses it back. No daemon binary.
    #[test]
    fn wire_round_trip_hello() {
        let dir = std::env::temp_dir().join(format!("maestro-wire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("wire.sock");
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let req: Request = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(req, Request::Hello, "server must receive Hello");

            let resp = Response::Hello {
                protocol_version: PROTOCOL_VERSION,
                pid: 12345,
            };
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            let mut w = stream;
            w.write_all(out.as_bytes()).unwrap();
            w.flush().unwrap();
        });

        let resp = client::request_at(&sock, &Request::Hello).unwrap();
        match resp {
            Response::Hello {
                protocol_version,
                pid,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(pid, 12345);
            }
            other => panic!("expected Hello, got {other:?}"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
