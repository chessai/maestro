//! The verifier report body schema (ADR-002), frozen. Stored as the JSON in
//! `verifier_reports.report`.

use serde::{Deserialize, Serialize};

/// Overall verdict (ADR-002). `fail` requires at least one `blocker` finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// Severity of a finding (ADR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Concern,
    Note,
}

/// A single finding (ADR-002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// Acceptance-criterion id, or `null` when not criterion-specific.
    pub criterion_id: Option<String>,
    /// Verbatim command output / diff hunk reference.
    pub evidence: String,
}

/// A command the verifier ran in its throwaway checkout (ADR-002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRun {
    pub cmd: String,
    pub exit: i64,
    pub output_digest: String,
}

/// The frozen verifier report body (ADR-002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportBody {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub out_of_scope_diff: bool,
    pub commands_run: Vec<CommandRun>,
}
