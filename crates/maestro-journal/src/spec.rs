//! TaskSpec and its budgets (ADR-003). The spec is stored verbatim as the
//! immutable JSON in `tasks.spec`; these types are its serde shape.

use serde::{Deserialize, Serialize};

use crate::domain::Tier;

/// Kind of an acceptance criterion (ADR-003). Adjective-only criteria are
/// rejected by the daemon; every criterion is a command or a falsifiable
/// invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionKind {
    Command,
    Invariant,
}

/// A single acceptance criterion (ADR-003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    /// e.g. `AC1`.
    pub id: String,
    /// A command or a falsifiable statement.
    pub check: String,
    pub kind: CriterionKind,
}

/// Per-attempt budget (ADR-003). Re-derived from the tier on each escalation,
/// so it bounds a single attempt, not the whole ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Budget {
    /// Hard turn budget (default 25 for driven CLIs).
    #[serde(default)]
    pub turns: Option<i64>,
    /// Per-tier token budget.
    #[serde(default)]
    pub tokens: Option<i64>,
}

/// Task-lifetime ceiling (ADR-003). Bounds the sum across all attempts and
/// verifications for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LifetimeBudget {
    /// Total metered tokens across every implementer and verifier session.
    #[serde(default)]
    pub tokens: Option<i64>,
    /// Wall-clock from the first `spawned` event.
    #[serde(default)]
    pub wall_clock_minutes: Option<i64>,
}

/// The immutable TaskSpec (ADR-003), stored as JSON in `tasks.spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub title: String,
    pub tier: Tier,
    pub base_ref: String,
    /// Paths/globs — enforced by the mechanical gate.
    #[serde(default)]
    pub file_allowlist: Vec<String>,
    /// Goals and constraints, not steps for T1/T2.
    pub instructions: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    /// e.g. `cargo test -p …`.
    #[serde(default)]
    pub check_commands: Vec<String>,
    /// Path — injected verbatim.
    #[serde(default)]
    pub house_rules_ref: Option<String>,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub lifetime_budget: LifetimeBudget,
    /// Can only *raise* the containment floor (ADR-003 / ADR-004).
    #[serde(default)]
    pub containment_min: u8,
}
