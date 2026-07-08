//! Protocol / IPC message types for the daemon Unix-socket API (ADR-006), M0
//! scope. Length-delimited framing is the daemon's concern; this module only
//! defines the message types and the handshake version constant.

use serde::{Deserialize, Serialize};

use crate::domain::{ContainmentLevel, Tier};
use crate::spec::TaskSpec;

/// Protocol version carried in the handshake (ADR-006). A client reaching a
/// running daemon of an incompatible version must fail loud rather than spawn a
/// second daemon.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request from a client (CLI / MCP proxy) to the daemon. M0 covers `Ps` and
/// `Doctor`; more variants land with later milestones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Liveness + version handshake (ADR-006). Sent by the auto-spawn client to
    /// confirm a reachable, version-compatible daemon.
    Hello,
    /// `maestro ps` — request the task read-model list.
    Ps,
    /// `maestro doctor` — resolved profile + capability probe.
    Doctor,
    /// Register an advisor session (MCP proxy startup); the daemon creates the
    /// advisor row and returns its minted `advisor_session_id`.
    RegisterAdvisor { profile: Option<String> },
    /// Advisor `delegate(task_spec)` (ADR-003 / ADR-006). `repo_path` is the
    /// project the worktree branches from.
    Delegate {
        advisor_session_id: String,
        repo_path: String,
        spec: Box<TaskSpec>,
    },
    /// Advisor `task_status(filter?)` — read model for the advisor's tasks.
    TaskStatus {
        advisor_session_id: String,
        state: Option<String>,
    },
    /// Drain the advisor's inbox: events since its last drain (ADR-006). The
    /// MCP proxy calls this after each advisor tool result and appends the items.
    DrainInbox { advisor_session_id: String },
    /// Advisor `close_task(task_id, outcome, successor?)` (ADR-003). Resolves a
    /// `blocked` task: records a terminal `failed(verification_failed)`. `outcome`
    /// is `abandoned` or `superseded`; a `superseded` successor sets its
    /// `parent_task` to this blocked task (recorded separately by the advisor's
    /// `delegate`).
    CloseTask {
        advisor_session_id: String,
        task_id: String,
        outcome: String,
        successor: Option<String>,
    },
    /// A named, read-only journal query (ADR-001 telemetry). `query` selects a
    /// canned query (e.g. `verifier_reports`, `trace`); `params` carries its
    /// arguments (e.g. `{ "task_id": "…" }`).
    JournalQuery {
        advisor_session_id: String,
        query: String,
        params: serde_json::Value,
    },
    /// Break-glass kill of a running driven (PTY) session (ADR-006). `kind` is
    /// `"human"` (`maestro kill`) or `"advisor"` (advisor `kill_task`); the two
    /// paths converge on one code path but the journal distinguishes them
    /// (`interrupted_human` vs `interrupted_advisor`). Replies [`Response::Killed`]
    /// if a live driven session exists for the task, else [`Response::Error`].
    KillTask { task_id: String, kind: String },
    /// Advisor `search(queries)` (ADR-005): metadata-only web search via the
    /// active profile's search backend. No model involvement; results are not
    /// cached (freshness is the point). An unset/unreachable backend replies
    /// [`Response::Error`] with a `backend_unavailable` message — never silent.
    Search {
        advisor_session_id: String,
        queries: Vec<String>,
    },
    /// Advisor `fetch_extract(url, schema_fields)` (ADR-005): the daemon fetches
    /// the URL, runs readability, calls the shim extraction model for verbatim
    /// spans, then validates each offset against the fetched content and rejects
    /// a fabricated extraction. Cached in `shim_cache` keyed by `(url,
    /// schema_hash)` with a 24h TTL.
    FetchExtract {
        advisor_session_id: String,
        url: String,
        schema_fields: Vec<String>,
    },
}

/// A response from the daemon to a client. Errors are carried as an explicit
/// variant so framing does not need out-of-band error signalling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Hello`]: the daemon's protocol version and pid.
    Hello { protocol_version: u32, pid: u32 },
    /// Reply to [`Request::Ps`].
    Ps { tasks: Vec<PsRow> },
    /// Reply to [`Request::Doctor`].
    Doctor(DoctorReport),
    /// Reply to [`Request::RegisterAdvisor`].
    RegisterAdvisor { advisor_session_id: String },
    /// Reply to [`Request::Delegate`]: the created task's id.
    Delegate { task_id: String },
    /// Reply to [`Request::TaskStatus`].
    TaskStatus { tasks: Vec<PsRow> },
    /// Reply to [`Request::DrainInbox`]: pending inbox items since last drain.
    Inbox { items: Vec<InboxItem> },
    /// Reply to [`Request::CloseTask`]: the closed task's id.
    Closed { task_id: String },
    /// Reply to [`Request::JournalQuery`]: the query's JSON result.
    JournalResult { value: serde_json::Value },
    /// Reply to [`Request::KillTask`]: the kill request was delivered to the
    /// task's live driven session. The worker records the terminal
    /// `interrupted`/`failed` events when the driver returns.
    Killed { task_id: String },
    /// Reply to [`Request::Search`] (ADR-005): the metadata-only search results,
    /// serialized by the daemon from the shim `SearchResult` list (so this crate
    /// carries no dependency on `maestro-shim`).
    SearchResults { results: serde_json::Value },
    /// Reply to [`Request::FetchExtract`] (ADR-005): the validated extraction,
    /// serialized by the daemon from the shim `Extraction` type (verbatim spans
    /// with offsets + content digest). Rejections/backend/model failures are a
    /// [`Response::Error`], not this variant.
    Extraction { extraction: serde_json::Value },
    /// A structured error for any request.
    Error { message: String },
}

/// One advisor inbox line (ADR-006): a task lifecycle event the advisor has not
/// yet seen. Ordered by `event_id` (ULID) across all of the advisor's tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub event_id: String,
    pub task_id: String,
    pub ts: String,
    /// Event kind (e.g. `verify_passed`, `escalated`, `blocked`, `checks_failed`).
    pub kind: String,
    /// One-line human summary for the advisor (daemon-composed).
    pub summary: String,
}

/// One row of the `ps` read-model (ADR-006), presented to `maestro ps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PsRow {
    pub task_id: String,
    pub title: String,
    pub tier: Tier,
    pub model: String,
    pub containment: ContainmentLevel,
    /// Derived current state: the latest event kind for the task, as a string.
    pub state: String,
    pub created_at: String,
}

/// The `maestro doctor` payload (ADR-006 / ADR-007 / ADR-004). The daemon fills
/// `probe` with its capability-probe result; the shape is left generic here so
/// this crate carries no daemon policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// The active profile name.
    pub profile: String,
    /// The fully resolved profile, as an opaque JSON value filled by the daemon.
    pub resolved_profile: serde_json::Value,
    /// The capability-probe payload, filled by the daemon.
    pub probe: serde_json::Value,
}
