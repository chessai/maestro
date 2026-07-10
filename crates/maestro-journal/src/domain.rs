//! Domain types: the enums and row structs derived from / stored in the
//! journal schema (ADR-001), plus the containment level enum this crate owns
//! canonically (ADR-004).

use serde::{Deserialize, Serialize};

/// Delegation tier (ADR-003). Serialized as the integer `0 | 1 | 2` that is
/// stored in `tasks.tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum Tier {
    /// Mechanical work and all shim work.
    T0,
    /// Bounded implementation: clear spec, contained blast radius.
    T1,
    /// Architecturally sensitive: cross-crate refactors, ambiguous specs.
    T2,
}

impl Tier {
    /// The integer stored in the DB column.
    pub fn as_int(self) -> u8 {
        match self {
            Tier::T0 => 0,
            Tier::T1 => 1,
            Tier::T2 => 2,
        }
    }
}

impl From<Tier> for u8 {
    fn from(t: Tier) -> u8 {
        t.as_int()
    }
}

impl TryFrom<u8> for Tier {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(Tier::T0),
            1 => Ok(Tier::T1),
            2 => Ok(Tier::T2),
            other => Err(format!("invalid tier {other}")),
        }
    }
}

/// Containment level (ADR-004). This crate owns the canonical enum; it is
/// serialized as the integer `0 | 1 | 2` stored in `tasks.containment_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum ContainmentLevel {
    /// Agent-native sandbox + universal post-hoc enforcement. Everywhere.
    L0,
    /// L0 + OS sandbox wrapper (workspace-only writes, network default-deny).
    L1,
    /// L1 + Nix devShell tool whitelist.
    L2,
}

impl ContainmentLevel {
    /// The integer stored in the DB column.
    pub fn as_int(self) -> u8 {
        match self {
            ContainmentLevel::L0 => 0,
            ContainmentLevel::L1 => 1,
            ContainmentLevel::L2 => 2,
        }
    }
}

impl From<ContainmentLevel> for u8 {
    fn from(l: ContainmentLevel) -> u8 {
        l.as_int()
    }
}

impl TryFrom<u8> for ContainmentLevel {
    type Error = String;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0 => Ok(ContainmentLevel::L0),
            1 => Ok(ContainmentLevel::L1),
            2 => Ok(ContainmentLevel::L2),
            other => Err(format!("invalid containment level {other}")),
        }
    }
}

/// Task lifecycle event kinds (ADR-001). Serialized as the snake_case string
/// stored in `events.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Created,
    /// Concurrency cap saturation.
    Queued,
    /// payload: requested_level, actual_level, tighten_applied.
    ContainmentDowngraded,
    Spawned,
    PlanSubmitted,
    PlanRejected,
    Iterating,
    ImplFinished,
    ChecksStarted,
    ChecksFailed,
    /// Mechanical gate passed (allowlist + check commands); M1 leaves the branch
    /// for human merge (model verifier arrives in M2).
    ChecksPassed,
    VerifyStarted,
    VerifyFailed,
    VerifyPassed,
    /// payload: from_tier, to_tier, reason.
    Escalated,
    Blocked,
    Merged,
    /// payload: `reason` (e.g. `daemon_restart`).
    Interrupted,
    /// payload: failure kind, optional `superseded_by` task_id.
    Failed,
    Pruned,
    /// Daemon-side stall detection fired (ADR-009 Phase 2): the driven session
    /// produced no PTY output for longer than `stall_timeout_seconds`.
    /// payload: `idle_seconds`, `stall_timeout_seconds`, `partial_diff`.
    StallDetected,
    /// Daemon auto-recovered from a detected stall (ADR-009 Phase 2): the stalled
    /// session was killed and a retry attempt was launched.
    /// payload: `action` (snapshot_kill_retry | flag_only), `has_edits`.
    AutoRecovered,
}

impl EventKind {
    /// The snake_case string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Queued => "queued",
            EventKind::ContainmentDowngraded => "containment_downgraded",
            EventKind::Spawned => "spawned",
            EventKind::PlanSubmitted => "plan_submitted",
            EventKind::PlanRejected => "plan_rejected",
            EventKind::Iterating => "iterating",
            EventKind::ImplFinished => "impl_finished",
            EventKind::ChecksStarted => "checks_started",
            EventKind::ChecksFailed => "checks_failed",
            EventKind::ChecksPassed => "checks_passed",
            EventKind::VerifyStarted => "verify_started",
            EventKind::VerifyFailed => "verify_failed",
            EventKind::VerifyPassed => "verify_passed",
            EventKind::Escalated => "escalated",
            EventKind::Blocked => "blocked",
            EventKind::Merged => "merged",
            EventKind::Interrupted => "interrupted",
            EventKind::Failed => "failed",
            EventKind::Pruned => "pruned",
            EventKind::StallDetected => "stall_detected",
            EventKind::AutoRecovered => "auto_recovered",
        }
    }

    /// Parse the snake_case string stored in the DB column.
    pub fn from_str_kind(s: &str) -> Option<Self> {
        Some(match s {
            "created" => EventKind::Created,
            "queued" => EventKind::Queued,
            "containment_downgraded" => EventKind::ContainmentDowngraded,
            "spawned" => EventKind::Spawned,
            "plan_submitted" => EventKind::PlanSubmitted,
            "plan_rejected" => EventKind::PlanRejected,
            "iterating" => EventKind::Iterating,
            "impl_finished" => EventKind::ImplFinished,
            "checks_started" => EventKind::ChecksStarted,
            "checks_failed" => EventKind::ChecksFailed,
            "checks_passed" => EventKind::ChecksPassed,
            "verify_started" => EventKind::VerifyStarted,
            "verify_failed" => EventKind::VerifyFailed,
            "verify_passed" => EventKind::VerifyPassed,
            "escalated" => EventKind::Escalated,
            "blocked" => EventKind::Blocked,
            "merged" => EventKind::Merged,
            "interrupted" => EventKind::Interrupted,
            "failed" => EventKind::Failed,
            "pruned" => EventKind::Pruned,
            "stall_detected" => EventKind::StallDetected,
            "auto_recovered" => EventKind::AutoRecovered,
            _ => return None,
        })
    }
}

/// Advisor-scoped event kinds (ADR-001), stored in `advisor_events.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorEventKind {
    /// payload: `path`; advisor write to an allowlisted in-repo path (ADR-006).
    AdvisorWrite,
}

impl AdvisorEventKind {
    /// The snake_case string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            AdvisorEventKind::AdvisorWrite => "advisor_write",
        }
    }
}

/// The frozen failure taxonomy (ADR-001). Exactly one is carried by a terminal
/// `failed` event. Adding a variant after freeze requires an ADR amendment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// Orchestrator rejected the TaskSpec (schema/validation) before spawn.
    SpecRejected,
    /// Codex plan-echo failed the spec check; no edits made.
    PlanRejected,
    /// Blocked task closed after top-tier verify failure.
    VerificationFailed,
    /// Out-of-allowlist diff detected post-session.
    ScopeViolation,
    /// Turn or token budget hit.
    BudgetExhausted,
    /// Configured model missing/unauthenticated at delegation time.
    ModelUnavailable,
    /// Containment layer terminated the session.
    SandboxKilled,
    /// PTY unresponsive past watchdog timeout.
    SessionWedged,
    /// maestro bug or environment fault.
    InternalError,
    /// CLI kill.
    InterruptedHuman,
    /// Advisor `kill_task`.
    InterruptedAdvisor,
}

/// Session role (ADR-001), stored in `sessions.role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Implementer,
    Verifier,
    PlanCheck,
    Shim,
}

impl Role {
    /// The string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Implementer => "implementer",
            Role::Verifier => "verifier",
            Role::PlanCheck => "plan_check",
            Role::Shim => "shim",
        }
    }
}

/// Session kind (ADR-001), stored in `sessions.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    DrivenPty,
    OneShotApi,
}

impl SessionKind {
    /// The string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::DrivenPty => "driven_pty",
            SessionKind::OneShotApi => "one_shot_api",
        }
    }
}

/// Session exit status (ADR-001), stored in `sessions.exit_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Ok,
    Error,
    Killed,
    Wedged,
}

impl ExitStatus {
    /// The string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            ExitStatus::Ok => "ok",
            ExitStatus::Error => "error",
            ExitStatus::Killed => "killed",
            ExitStatus::Wedged => "wedged",
        }
    }
}

/// Verifier independence (ADR-002), stored in `verifier_reports.independence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Independence {
    /// Implementer and verifier from different providers.
    CrossProvider,
    /// Same provider, different model.
    CrossModel,
    /// Same model, fresh context — last resort, allowed but flagged.
    FreshContextOnly,
}

impl Independence {
    /// The string stored in the DB column.
    pub fn as_str(self) -> &'static str {
        match self {
            Independence::CrossProvider => "cross_provider",
            Independence::CrossModel => "cross_model",
            Independence::FreshContextOnly => "fresh_context_only",
        }
    }
}

/// A row of the `advisors` table (ADR-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisor {
    /// ULID, minted by the MCP proxy at startup.
    pub advisor_session_id: String,
    /// Config profile name.
    pub profile: String,
    /// e.g. `claude-opus-4-7`.
    pub advisor_model: String,
    /// `standard` | `1m`.
    pub advisor_context: String,
    /// ISO 8601 UTC.
    pub started_at: String,
}

/// A row of the `tasks` table (ADR-001). `spec` is the immutable JSON TaskSpec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// ULID.
    pub task_id: String,
    pub advisor_session_id: String,
    /// Reserved for DAG (v1: NULL).
    pub parent_task: Option<String>,
    /// JSON array of task_ids (v1: NULL).
    pub depends_on: Option<String>,
    pub tier: Tier,
    /// Explicit, from config; never inferred.
    pub model: String,
    pub containment_level: ContainmentLevel,
    /// JSON TaskSpec, immutable.
    pub spec: String,
    /// Worktree path.
    pub workspace: Option<String>,
    /// The origin repository the worktree branched from (the `delegate`'s
    /// `repo_path`). Needed to fast-forward-merge the task branch on an explicit
    /// advisor `merge_task` (ADR-006). Nullable for rows created before this
    /// field existed / rows delegated without a repo path.
    pub repo_path: Option<String>,
    pub base_ref: String,
    /// `maestro/<task-ulid>`.
    pub branch: String,
    pub created_at: String,
}

/// A row of the `events` table (ADR-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// ULID (identity).
    pub event_id: String,
    pub task_id: String,
    /// ISO 8601 UTC.
    pub ts: String,
    /// Per-task monotonic, daemon-assigned; the ordering key.
    pub seq: i64,
    pub kind: EventKind,
    /// JSON, kind-specific.
    pub payload: Option<String>,
}

/// A row of the `advisor_events` table (ADR-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisorEvent {
    /// ULID.
    pub event_id: String,
    pub advisor_session_id: String,
    /// ISO 8601 UTC.
    pub ts: String,
    /// Per-advisor monotonic, daemon-assigned.
    pub seq: i64,
    pub kind: AdvisorEventKind,
    /// JSON, kind-specific.
    pub payload: Option<String>,
}

/// A row of the `sessions` table (ADR-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// ULID.
    pub session_id: String,
    /// NULL for shim calls (no task, no workspace).
    pub task_id: Option<String>,
    /// Set when `task_id` is NULL.
    pub advisor_session_id: Option<String>,
    pub role: Role,
    pub model: String,
    pub kind: SessionKind,
    pub workspace: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_status: Option<ExitStatus>,
    pub turns: Option<i64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// Captured PTY output.
    pub log_path: Option<String>,
}

/// A row of the `verifier_reports` table (ADR-001). `report` is the JSON body
/// whose schema lives in [`crate::report::ReportBody`] (ADR-002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierReport {
    pub report_id: String,
    pub task_id: String,
    pub session_id: String,
    pub attempt: i64,
    pub independence: Independence,
    /// JSON, schema in ADR-002.
    pub report: String,
}

/// A row of the `shim_cache` table (ADR-001). Keyed by `(url, schema_hash)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimCacheEntry {
    pub url: String,
    pub schema_hash: String,
    pub retrieved_at: String,
    /// JSON extraction result.
    pub payload: String,
}
