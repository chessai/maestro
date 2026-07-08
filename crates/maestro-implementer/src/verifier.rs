//! The verifier backend (ADR-002).
//!
//! A verifier judges an implementer's diff against the [`TaskSpec`]'s
//! acceptance criteria. Per ADR-002 the verifier **reports; it never mutates**:
//! it has no worktree and writes no files. It judges from the provided unified
//! diff plus the mechanical gate's command output. (ADR-002 allows a verifier
//! to run additional commands in a *throwaway checkout* of the implementation
//! branch; that option is deferred past M2 — this backend never runs commands
//! and always returns an empty `commands_run`.)
//!
//! Two backends are provided:
//! - [`MockVerifier`] — deterministic, driven by the spec's acceptance
//!   criteria. Selected when `task.model == "mock"`. Drives the M2 escalation
//!   tests: a spec with no `mock:pass` criterion fails every attempt.
//! - [`AnthropicVerifier`] — a real Anthropic Messages API client that runs a
//!   one-shot tool-use loop with a single read-only `emit_report` tool. Wired
//!   to the documented wire shape but never exercised live in CI.

use maestro_journal::report::{Finding, ReportBody, Severity, Verdict};
use maestro_journal::spec::TaskSpec;
use serde_json::{json, Value};

use crate::ImplementerError;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const MAX_TOKENS: u64 = 8000;
/// Small turn cap: at most one nudge after the first request.
const TURN_BUDGET: u32 = 4;

/// A unit of verification work handed to a [`VerifierBackend`].
///
/// The verifier has no worktree: it judges from `diff` + `gate_output` only.
pub struct VerifyTask {
    /// The immutable spec (acceptance_criteria + title) (ADR-003).
    pub spec: TaskSpec,
    /// Unified diff of the implementer's changes vs `spec.base_ref`.
    pub diff: String,
    /// Mechanical-gate command outputs (build/test/lint) (ADR-002).
    pub gate_output: String,
    /// The verifier model id, e.g. `"mock"` or `"claude-sonnet-4-6"`.
    pub model: String,
    /// Anthropic `base_url` override; else `$ANTHROPIC_BASE_URL`, else default.
    pub base_url: Option<String>,
    /// Earlier verifier reports, for escalation context (ADR-002 / ADR-003).
    pub prior_reports: Vec<ReportBody>,
}

/// A verifier's account of a verification: the report plus telemetry.
#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    /// The frozen report body (ADR-002).
    pub report: ReportBody,
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// The abstraction every verifier backend satisfies. Verifiers report; they
/// never mutate (ADR-002).
pub trait VerifierBackend {
    /// Judge `task` and produce a report. Never writes files.
    fn verify(&self, task: &VerifyTask) -> Result<VerifyOutcome, ImplementerError>;
}

/// A deterministic verifier for tests and the M2 escalation loop.
///
/// Rule: **pass iff any `acceptance_criteria[i].check` equals `"mock:pass"`
/// (case-insensitive); otherwise fail.** A spec with no `mock:pass` criterion
/// fails on every attempt, which is what drives the escalation tests.
pub struct MockVerifier;

impl VerifierBackend for MockVerifier {
    fn verify(&self, task: &VerifyTask) -> Result<VerifyOutcome, ImplementerError> {
        let passes = task
            .spec
            .acceptance_criteria
            .iter()
            .any(|c| c.check.eq_ignore_ascii_case("mock:pass"));

        let report = if passes {
            ReportBody {
                verdict: Verdict::Pass,
                findings: Vec::new(),
                out_of_scope_diff: false,
                commands_run: Vec::new(),
            }
        } else {
            // `fail` requires at least one `blocker` (ADR-002). Key it to the
            // first criterion's id, or null when there are none.
            let criterion_id = task
                .spec
                .acceptance_criteria
                .first()
                .map(|c| c.id.clone());
            ReportBody {
                verdict: Verdict::Fail,
                findings: vec![Finding {
                    severity: Severity::Blocker,
                    criterion_id,
                    evidence: "mock verifier: criterion not satisfied".into(),
                }],
                out_of_scope_diff: false,
                commands_run: Vec::new(),
            }
        };

        Ok(VerifyOutcome {
            report,
            turns: 1,
            tokens_in: 0,
            tokens_out: 0,
        })
    }
}

/// A real Anthropic Messages API verifier.
///
/// Construct with [`AnthropicVerifier::new`], which takes an optional `base_url`
/// override and does NOT require the API key eagerly — the key is only read at
/// request time in [`VerifierBackend::verify`], which returns
/// [`ImplementerError::Unavailable`] when it is missing.
///
/// The verifier is given a single read-only `emit_report` tool; it is offered
/// no file-writing tool, so "verifiers never mutate" (ADR-002) is structural.
pub struct AnthropicVerifier {
    base_url_override: Option<String>,
}

impl AnthropicVerifier {
    /// Build a verifier with an optional `base_url` override (ADR-008). This is
    /// non-eager: it never reads or requires `ANTHROPIC_API_KEY`; that check is
    /// deferred to [`VerifierBackend::verify`].
    pub fn new(base_url: Option<String>) -> Self {
        let base_url_override = base_url.filter(|s| !s.is_empty());
        Self { base_url_override }
    }

    /// The effective base URL at request time: the explicit override, else
    /// `$ANTHROPIC_BASE_URL`, else the compiled-in default.
    fn effective_base_url(&self, task: &VerifyTask) -> String {
        if let Some(ref u) = self.base_url_override {
            return u.clone();
        }
        if let Some(u) = task.base_url.as_ref().filter(|s| !s.is_empty()) {
            return u.clone();
        }
        std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into())
    }
}

impl VerifierBackend for AnthropicVerifier {
    fn verify(&self, task: &VerifyTask) -> Result<VerifyOutcome, ImplementerError> {
        // The API key is read (and required) here, not at construction time.
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            return Err(ImplementerError::Unavailable(
                "ANTHROPIC_API_KEY is unset or empty".into(),
            ));
        }

        let base_url = self.effective_base_url(task);
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let mut messages = vec![initial_user_message(task)];
        let mut turns: u32 = 0;
        let mut tokens_in: u64 = 0;
        let mut tokens_out: u64 = 0;
        // We allow exactly one nudge if the model fails to call the tool.
        let mut nudged = false;

        loop {
            if turns >= TURN_BUDGET {
                return Err(ImplementerError::Budget(format!(
                    "turn budget of {TURN_BUDGET} exhausted before a report was emitted"
                )));
            }
            turns += 1;

            let body = build_verify_request_body(task, &messages);
            let response = send(&url, &api_key, &body)?;

            if let Some(usage) = response.get("usage") {
                tokens_in += usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                tokens_out += usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }

            let content = response
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ImplementerError::Protocol("response missing `content` array".into())
                })?
                .clone();

            // Find an emit_report tool_use block, if any.
            let report_input = content.iter().find_map(|block| {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    return None;
                }
                if block.get("name").and_then(Value::as_str) != Some("emit_report") {
                    return None;
                }
                block.get("input").cloned()
            });

            if let Some(input) = report_input {
                let report = parse_report(&input)?;
                return Ok(VerifyOutcome {
                    report,
                    turns,
                    tokens_in,
                    tokens_out,
                });
            }

            // No emit_report call. Nudge once, else Protocol.
            if nudged {
                return Err(ImplementerError::Protocol(
                    "model stopped without calling `emit_report` after a nudge".into(),
                ));
            }
            nudged = true;
            messages.push(json!({ "role": "assistant", "content": content }));
            messages.push(json!({
                "role": "user",
                "content": "You did not call the `emit_report` tool. You must report your \
                            verdict by calling `emit_report` exactly once now.",
            }));
        }
    }
}

/// Parse an `emit_report` tool input into a [`ReportBody`] and enforce the
/// ADR-002 invariant.
///
/// Invariant handling: if `verdict == fail` but the report carries no `blocker`
/// finding, the report is malformed. We **normalize** by synthesizing a single
/// `blocker` finding rather than rejecting, so a well-intentioned "fail" is not
/// dropped on a technicality; the synthesized finding is clearly labelled. (The
/// spec permits either normalization or a `Protocol` error; normalization is
/// chosen to keep a genuine fail verdict actionable.)
fn parse_report(input: &Value) -> Result<ReportBody, ImplementerError> {
    let mut report: ReportBody = serde_json::from_value(input.clone()).map_err(|e| {
        ImplementerError::Protocol(format!("emit_report input is not a valid report: {e}"))
    })?;

    if report.verdict == Verdict::Fail
        && !report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker)
    {
        report.findings.push(Finding {
            severity: Severity::Blocker,
            criterion_id: None,
            evidence: "synthesized blocker: verifier returned verdict=fail with no blocker \
                       finding (ADR-002 invariant repair)"
                .into(),
        });
    }

    Ok(report)
}

/// Build the first user message: the acceptance criteria, the unified diff, the
/// mechanical-gate output, and a summary of any prior reports.
fn initial_user_message(task: &VerifyTask) -> Value {
    json!({ "role": "user", "content": user_message_text(task) })
}

/// The text of the first user message (pure; no network / filesystem).
fn user_message_text(task: &VerifyTask) -> String {
    let spec = &task.spec;

    let mut text = String::new();
    text.push_str(&format!("# Verify task: {}\n\n", spec.title));

    text.push_str("## Acceptance criteria\n");
    if spec.acceptance_criteria.is_empty() {
        text.push_str("(none specified)\n");
    } else {
        for c in &spec.acceptance_criteria {
            text.push_str(&format!("- {} ({:?}): {}\n", c.id, c.kind, c.check));
        }
    }

    text.push_str("\n## Unified diff (implementer's changes vs base)\n");
    text.push_str("```diff\n");
    text.push_str(&task.diff);
    text.push_str("\n```\n");

    text.push_str("\n## Mechanical gate output (build/test/lint)\n");
    text.push_str("```\n");
    text.push_str(&task.gate_output);
    text.push_str("\n```\n");

    if !task.prior_reports.is_empty() {
        text.push_str("\n## Prior verifier reports (escalation context)\n");
        for (i, r) in task.prior_reports.iter().enumerate() {
            let blockers = r
                .findings
                .iter()
                .filter(|f| f.severity == Severity::Blocker)
                .count();
            text.push_str(&format!(
                "- attempt {}: verdict={:?}, {} finding(s) ({} blocker(s)), out_of_scope_diff={}\n",
                i + 1,
                r.verdict,
                r.findings.len(),
                blockers,
                r.out_of_scope_diff,
            ));
            for f in &r.findings {
                text.push_str(&format!(
                    "    - {:?} [{}]: {}\n",
                    f.severity,
                    f.criterion_id.as_deref().unwrap_or("-"),
                    f.evidence,
                ));
            }
        }
    }

    text.push_str(
        "\nJudge whether the DIFF satisfies each acceptance criterion. Report by calling \
         `emit_report` exactly once.\n",
    );

    text
}

/// The single read-only `emit_report` tool (documented wire shape). It mirrors
/// the [`ReportBody`] schema. No file-writing tool is ever offered.
fn emit_report_tool() -> Value {
    json!({
        "name": "emit_report",
        "description": "Report the verification verdict. Call this exactly once. `verdict` \
                        must be \"pass\" or \"fail\"; a \"fail\" verdict requires at least one \
                        finding with severity \"blocker\".",
        "input_schema": {
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["pass", "fail"] },
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "severity": {
                                "type": "string",
                                "enum": ["blocker", "concern", "note"]
                            },
                            "criterion_id": { "type": ["string", "null"] },
                            "evidence": { "type": "string" }
                        },
                        "required": ["severity", "criterion_id", "evidence"]
                    }
                },
                "out_of_scope_diff": { "type": "boolean" },
                "commands_run": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "cmd": { "type": "string" },
                            "exit": { "type": "integer" },
                            "output_digest": { "type": "string" }
                        },
                        "required": ["cmd", "exit", "output_digest"]
                    }
                }
            },
            "required": ["verdict", "findings", "out_of_scope_diff", "commands_run"]
        }
    })
}

/// Construct the request body for a single Messages API call.
///
/// Pure and network-free so it can be unit-tested. `prior_messages` is the full
/// `messages` array accumulated so far (starting with the initial user turn).
pub fn build_verify_request_body(task: &VerifyTask, prior_messages: &[Value]) -> Value {
    let system = "You are a code-change verifier. You did NOT write this code. Judge whether \
         the provided unified DIFF satisfies each acceptance criterion, using the \
         mechanical-gate command output as evidence. Be skeptical: do not accept a \
         criterion as met unless the diff plainly demonstrates it, and watch for \
         out-of-scope changes. You have no ability to edit files. You MUST report your \
         verdict by calling the `emit_report` tool exactly once.";

    json!({
        "model": task.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": prior_messages,
        "tools": [emit_report_tool()],
    })
}

/// Send one request and parse the JSON response, mapping transport/status
/// errors to [`ImplementerError::Http`] and parse failures to
/// [`ImplementerError::Protocol`].
fn send(url: &str, api_key: &str, body: &Value) -> Result<Value, ImplementerError> {
    let resp = ureq::post(url)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("content-type", "application/json")
        .send_json(body);

    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| ImplementerError::Protocol(format!("cannot parse response JSON: {e}"))),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r
                .into_string()
                .unwrap_or_else(|_| "<unreadable body>".into());
            Err(ImplementerError::Http(format!("status {code}: {detail}")))
        }
        Err(ureq::Error::Transport(t)) => {
            Err(ImplementerError::Http(format!("transport error: {t}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::domain::Tier;
    use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind};

    fn spec_with_criteria(criteria: Vec<AcceptanceCriterion>) -> TaskSpec {
        TaskSpec {
            title: "Add x".into(),
            tier: Tier::T0,
            base_ref: "main".into(),
            file_allowlist: vec!["src/lib.rs".into()],
            instructions: "Add a function x".into(),
            acceptance_criteria: criteria,
            check_commands: vec!["cargo build".into()],
            house_rules_ref: None,
            budget: Budget::default(),
            lifetime_budget: Default::default(),
            containment_min: 0,
        }
    }

    fn task(model: &str, spec: TaskSpec) -> VerifyTask {
        VerifyTask {
            spec,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+pub fn x() {}\n".into(),
            gate_output: "cargo build: ok\ncargo test: ok\n".into(),
            model: model.into(),
            base_url: None,
            prior_reports: Vec::new(),
        }
    }

    fn ac(id: &str, check: &str, kind: CriterionKind) -> AcceptanceCriterion {
        AcceptanceCriterion {
            id: id.into(),
            check: check.into(),
            kind,
        }
    }

    // AC4: no mock:pass criterion → Fail with exactly one blocker.
    #[test]
    fn mock_fails_without_mock_pass() {
        let spec = spec_with_criteria(vec![
            ac("AC1", "cargo build", CriterionKind::Command),
            ac("AC2", "x is exported", CriterionKind::Invariant),
        ]);
        let out = MockVerifier.verify(&task("mock", spec)).unwrap();
        assert_eq!(out.report.verdict, Verdict::Fail);
        let blockers: Vec<_> = out
            .report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
            .collect();
        assert_eq!(blockers.len(), 1, "expected exactly one blocker");
        assert_eq!(out.report.findings.len(), 1);
        // Keyed to the first criterion.
        assert_eq!(blockers[0].criterion_id.as_deref(), Some("AC1"));
        assert_eq!(out.turns, 1);
    }

    #[test]
    fn mock_fail_criterion_id_null_when_no_criteria() {
        let spec = spec_with_criteria(vec![]);
        let out = MockVerifier.verify(&task("mock", spec)).unwrap();
        assert_eq!(out.report.verdict, Verdict::Fail);
        assert_eq!(out.report.findings.len(), 1);
        assert_eq!(out.report.findings[0].criterion_id, None);
    }

    // AC5: a mock:pass criterion → Pass, zero blockers.
    #[test]
    fn mock_passes_with_mock_pass_criterion() {
        let spec = spec_with_criteria(vec![ac("AC1", "mock:pass", CriterionKind::Invariant)]);
        let out = MockVerifier.verify(&task("mock", spec)).unwrap();
        assert_eq!(out.report.verdict, Verdict::Pass);
        let blockers = out
            .report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
            .count();
        assert_eq!(blockers, 0);
        assert!(out.report.findings.is_empty());
        assert!(out.report.commands_run.is_empty());
        assert!(!out.report.out_of_scope_diff);
    }

    #[test]
    fn mock_pass_is_case_insensitive() {
        let spec = spec_with_criteria(vec![ac("AC1", "MOCK:PASS", CriterionKind::Invariant)]);
        let out = MockVerifier.verify(&task("mock", spec)).unwrap();
        assert_eq!(out.report.verdict, Verdict::Pass);
    }

    // AC6: construct without a key; verify with ANTHROPIC_API_KEY unset →
    // Unavailable (save/restore env).
    #[test]
    fn verify_unavailable_without_key() {
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let verifier = AnthropicVerifier::new(None);
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let res = verifier.verify(&task("claude-sonnet-4-6", spec));
        assert!(
            matches!(res, Err(ImplementerError::Unavailable(_))),
            "verify must fail Unavailable without a key, got {res:?}"
        );

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    // AC7: build_verify_request_body wire shape.
    #[test]
    fn build_verify_request_body_matches_wire_shape() {
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let t = task("claude-sonnet-4-6", spec);
        let messages = vec![initial_user_message(&t)];
        let body = build_verify_request_body(&t, &messages);

        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["tools"][0]["name"], "emit_report");
        assert_eq!(body["max_tokens"], MAX_TOKENS);

        // No file-writing tool is offered.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert_ne!(name, "write_file");
            assert!(
                !name.contains("write") && !name.contains("edit"),
                "no file-writing tool allowed, got {name}"
            );
        }

        // The first user message text contains the diff string.
        let first_user = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            first_user.contains(&t.diff),
            "first user message must contain the diff"
        );

        // The tool schema mirrors the ReportBody shape.
        let schema = &body["tools"][0]["input_schema"];
        assert_eq!(schema["properties"]["verdict"]["enum"][0], "pass");
        assert_eq!(schema["properties"]["verdict"]["enum"][1], "fail");
        assert!(schema["properties"]["findings"].is_object());
        assert!(schema["properties"]["out_of_scope_diff"].is_object());
        assert!(schema["properties"]["commands_run"].is_object());
    }

    #[test]
    fn base_url_precedence_override_then_task_then_env_then_default() {
        let saved = std::env::var("ANTHROPIC_BASE_URL").ok();
        std::env::remove_var("ANTHROPIC_BASE_URL");
        let spec = spec_with_criteria(vec![]);

        // Explicit override wins.
        let v = AnthropicVerifier::new(Some("http://override.example".into()));
        let mut t = task("claude-sonnet-4-6", spec.clone());
        t.base_url = Some("http://task.example".into());
        assert_eq!(v.effective_base_url(&t), "http://override.example");

        // No override → task.base_url.
        let v2 = AnthropicVerifier::new(None);
        assert_eq!(v2.effective_base_url(&t), "http://task.example");

        // No override, no task url → env.
        std::env::set_var("ANTHROPIC_BASE_URL", "http://env.example");
        let t2 = task("claude-sonnet-4-6", spec.clone());
        assert_eq!(v2.effective_base_url(&t2), "http://env.example");

        // Nothing → default.
        std::env::remove_var("ANTHROPIC_BASE_URL");
        assert_eq!(v2.effective_base_url(&t2), DEFAULT_BASE_URL);

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_BASE_URL", v);
        }
    }

    #[test]
    fn parse_report_normalizes_fail_without_blocker() {
        // verdict fail with only a `concern` → a blocker is synthesized.
        let input = json!({
            "verdict": "fail",
            "findings": [
                { "severity": "concern", "criterion_id": "AC1", "evidence": "weak" }
            ],
            "out_of_scope_diff": false,
            "commands_run": []
        });
        let report = parse_report(&input).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert!(report
            .findings
            .iter()
            .any(|f| f.severity == Severity::Blocker));
    }

    #[test]
    fn parse_report_passes_through_valid_pass() {
        let input = json!({
            "verdict": "pass",
            "findings": [],
            "out_of_scope_diff": false,
            "commands_run": [
                { "cmd": "cargo test", "exit": 0, "output_digest": "abc" }
            ]
        });
        let report = parse_report(&input).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.commands_run.len(), 1);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn user_message_includes_prior_reports_when_present() {
        let spec = spec_with_criteria(vec![ac("AC1", "cargo build", CriterionKind::Command)]);
        let mut t = task("claude-sonnet-4-6", spec);
        t.prior_reports.push(ReportBody {
            verdict: Verdict::Fail,
            findings: vec![Finding {
                severity: Severity::Blocker,
                criterion_id: Some("AC1".into()),
                evidence: "build broke".into(),
            }],
            out_of_scope_diff: false,
            commands_run: Vec::new(),
        });
        let text = user_message_text(&t);
        assert!(text.contains("Prior verifier reports"));
        assert!(text.contains("build broke"));
    }
}
