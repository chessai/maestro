//! Plan-vs-spec checkers for the plan-echo gate (ADR-003).
//!
//! The driven session's first turn must restate its plan; a [`PlanChecker`]
//! compares that plan against the [`TaskSpec`] and returns a [`PlanVerdict`].
//! On [`PlanVerdict::Reject`] the session is torn down BEFORE it makes any
//! edits. This is an early-abort efficiency gate, not the security control —
//! the load-bearing scope control is the post-hoc allowlist diff (ADR-003).

use std::time::Duration;

use maestro_journal::spec::TaskSpec;
use serde_json::{json, Value};

/// The outcome of a plan-vs-spec check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanVerdict {
    /// The plan is consistent with the spec; proceed to editing.
    Accept,
    /// The plan mismatches the spec; abort before any edits.
    Reject {
        /// Human-readable justification, surfaced in the `plan_rejected` event.
        reason: String,
    },
}

/// Compares a driven session's echoed plan against its [`TaskSpec`].
pub trait PlanChecker {
    /// Return [`PlanVerdict::Accept`] to proceed or
    /// [`PlanVerdict::Reject`] to abort before edits.
    fn check(&self, plan: &str, spec: &TaskSpec) -> PlanVerdict;
}

/// A deterministic checker for tests and offline runs.
///
/// Accepts iff the plan mentions "create" (case-insensitive) and does NOT
/// mention "delete"; otherwise rejects. So "create the file as specified" is
/// accepted, while "delete everything and rewrite unrelated files" is rejected.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockPlanChecker;

impl PlanChecker for MockPlanChecker {
    fn check(&self, plan: &str, _spec: &TaskSpec) -> PlanVerdict {
        let lower = plan.to_lowercase();
        if lower.contains("delete") {
            return PlanVerdict::Reject {
                reason: "plan proposes a destructive `delete`".to_string(),
            };
        }
        if lower.contains("create") {
            PlanVerdict::Accept
        } else {
            PlanVerdict::Reject {
                reason: "plan does not describe a concrete `create` action".to_string(),
            }
        }
    }
}

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const MAX_TOKENS: u64 = 1024;

/// A wired-but-not-live-tested Anthropic Messages API plan checker.
///
/// Mirrors the non-eager pattern in `maestro-implementer`: construction never
/// reads `ANTHROPIC_API_KEY`; the key is only read at [`PlanChecker::check`]
/// time. When the key is missing the checker is **permissive** — it returns
/// [`PlanVerdict::Accept`] rather than panicking or blocking work — because the
/// real scope control is the post-hoc allowlist diff, not this gate (ADR-003).
pub struct AnthropicPlanChecker {
    base_url_override: Option<String>,
    model: String,
}

impl AnthropicPlanChecker {
    /// Build a checker with an optional `base_url` override. Non-eager: it never
    /// reads or requires `ANTHROPIC_API_KEY`.
    pub fn new(base_url: Option<String>) -> Self {
        let base_url_override = base_url.filter(|s| !s.is_empty());
        Self {
            base_url_override,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Override the model id used for the check call (defaults to
    /// [`DEFAULT_MODEL`]).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// The effective base URL at request time: the explicit override, else
    /// `$ANTHROPIC_BASE_URL`, else the compiled-in default.
    fn effective_base_url(&self) -> String {
        if let Some(ref u) = self.base_url_override {
            return u.clone();
        }
        std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into())
    }

    /// Build the Messages API request body. Returns `(model, value)` so
    /// [`build_plan_check_body`] can be a pure, model-agnostic helper.
    fn request_body(&self, plan: &str, spec: &TaskSpec) -> Value {
        let mut body = build_plan_check_body(plan, spec);
        body["model"] = json!(self.model);
        body["max_tokens"] = json!(MAX_TOKENS);
        body
    }
}

impl PlanChecker for AnthropicPlanChecker {
    fn check(&self, plan: &str, spec: &TaskSpec) -> PlanVerdict {
        // Key read (and only required) here, not at construction. Missing key →
        // permissive Accept so a misconfigured key never blocks work; the real
        // scope control is the post-hoc allowlist diff (ADR-003).
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            eprintln!(
                "maestro-driver: ANTHROPIC_API_KEY unset; plan-echo check defaulting to Accept \
                 (allowlist diff remains the scope control)"
            );
            return PlanVerdict::Accept;
        }

        let base_url = self.effective_base_url();
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
        let body = self.request_body(plan, spec);

        match send(&url, &api_key, &body) {
            Ok(resp) => parse_verdict(&resp).unwrap_or_else(|note| {
                eprintln!("maestro-driver: plan-echo check unparsable ({note}); defaulting Accept");
                PlanVerdict::Accept
            }),
            Err(note) => {
                eprintln!("maestro-driver: plan-echo check request failed ({note}); Accept");
                PlanVerdict::Accept
            }
        }
    }
}

/// The `plan_verdict` tool the model must call to return a structured verdict.
fn plan_verdict_tool() -> Value {
    json!({
        "name": "plan_verdict",
        "description": "Return whether the restated plan is consistent with the task specification.",
        "input_schema": {
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["accept", "reject"] },
                "reason": { "type": "string" }
            },
            "required": ["verdict", "reason"]
        }
    })
}

/// Construct the Messages API request body for a plan-vs-spec check.
///
/// Pure and network-free so it can be unit-tested. The returned JSON embeds the
/// plan text and the spec title/instructions/allowlist so the check is grounded
/// in the actual spec. `model` and `max_tokens` are filled in by
/// [`AnthropicPlanChecker`]; this helper leaves `model` unset for callers that
/// only want to assert on the content.
pub fn build_plan_check_body(plan: &str, spec: &TaskSpec) -> Value {
    let allowlist = if spec.file_allowlist.is_empty() {
        "(none specified)".to_string()
    } else {
        spec.file_allowlist
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let system = "You are a strict plan reviewer for a coding agent. You are given a task \
        specification and the agent's restated plan. Decide whether the plan is consistent with \
        the spec's title, instructions, and file allowlist. Reject plans that propose destructive \
        or out-of-scope work (e.g. deleting or rewriting files outside the task). Respond ONLY by \
        calling the `plan_verdict` tool."
        .to_string();

    let user = format!(
        "# Task specification\n\
         ## Title\n{title}\n\n\
         ## Instructions\n{instructions}\n\n\
         ## File allowlist\n{allowlist}\n\n\
         # Agent's restated plan\n{plan}\n",
        title = spec.title,
        instructions = spec.instructions,
        allowlist = allowlist,
        plan = plan,
    );

    json!({
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": [ { "role": "user", "content": user } ],
        "tools": [ plan_verdict_tool() ],
        "tool_choice": { "type": "tool", "name": "plan_verdict" },
    })
}

/// Extract a [`PlanVerdict`] from a Messages API response's `plan_verdict`
/// tool_use block. `Err` carries a note for the permissive-Accept fallback.
fn parse_verdict(resp: &Value) -> Result<PlanVerdict, String> {
    let content = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "response missing `content` array".to_string())?;

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        if block.get("name").and_then(Value::as_str) != Some("plan_verdict") {
            continue;
        }
        let input = block
            .get("input")
            .ok_or_else(|| "plan_verdict block missing `input`".to_string())?;
        let verdict = input
            .get("verdict")
            .and_then(Value::as_str)
            .ok_or_else(|| "plan_verdict input missing `verdict`".to_string())?;
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason provided")
            .to_string();
        return match verdict {
            "accept" => Ok(PlanVerdict::Accept),
            "reject" => Ok(PlanVerdict::Reject { reason }),
            other => Err(format!("unknown verdict `{other}`")),
        };
    }
    Err("no plan_verdict tool_use block in response".to_string())
}

/// Send one Messages API request and parse the JSON response.
///
/// The call is bounded by connect + overall timeouts. This is load-bearing:
/// `check()` runs in-process on the driven thread *between* the plan and
/// execute phases, and nothing else watchdogs it — an unbounded request that
/// hangs (connection established, no response) would block `check()` forever,
/// stranding the task in `Iterating` with no worker process and no terminal
/// event. On timeout `ureq` returns `Transport`, which `check()` maps to a
/// permissive `Accept` (the post-hoc allowlist diff remains the scope control),
/// so a slow/hung checker fails safe instead of wedging the task.
fn send(url: &str, api_key: &str, body: &Value) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build();
    let resp = agent
        .post(url)
        .set("x-api-key", api_key)
        .set("anthropic-version", ANTHROPIC_VERSION)
        .set("content-type", "application/json")
        .send_json(body);

    match resp {
        Ok(r) => r
            .into_json::<Value>()
            .map_err(|e| format!("cannot parse response JSON: {e}")),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_else(|_| "<unreadable body>".into());
            Err(format!("status {code}: {detail}"))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("transport error: {t}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_journal::domain::Tier;
    use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind, TaskSpec};

    fn spec() -> TaskSpec {
        TaskSpec {
            title: "Add greeting module".into(),
            tier: Tier::T1,
            base_ref: "main".into(),
            file_allowlist: vec!["src/greeting.rs".into()],
            instructions: "Create a greeting function".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC1".into(),
                check: "cargo build".into(),
                kind: CriterionKind::Command,
            }],
            check_commands: vec!["cargo build".into()],
            house_rules_ref: None,
            budget: Budget::default(),
            lifetime_budget: Default::default(),
            containment_min: 0,
        }
    }

    #[test]
    fn mock_accepts_create_rejects_delete() {
        let c = MockPlanChecker;
        assert_eq!(
            c.check("Create the file as specified", &spec()),
            PlanVerdict::Accept
        );
        assert!(matches!(
            c.check("delete everything and rewrite unrelated files", &spec()),
            PlanVerdict::Reject { .. }
        ));
        // "delete" wins even alongside "create".
        assert!(matches!(
            c.check("create X then delete Y", &spec()),
            PlanVerdict::Reject { .. }
        ));
        // Neither keyword → reject.
        assert!(matches!(
            c.check("refactor some things", &spec()),
            PlanVerdict::Reject { .. }
        ));
    }

    #[test]
    fn anthropic_constructs_without_key_and_accepts_when_unavailable() {
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let checker = AnthropicPlanChecker::new(None);
        // Missing key → permissive Accept (no panic, no network).
        assert_eq!(checker.check("anything at all", &spec()), PlanVerdict::Accept);

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    #[test]
    fn build_plan_check_body_embeds_plan_and_title() {
        let s = spec();
        let body = build_plan_check_body("PLAN: create src/greeting.rs", &s);

        let user = body["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("PLAN: create src/greeting.rs"), "plan text present");
        assert!(user.contains("Add greeting module"), "spec title present");
        assert!(user.contains("src/greeting.rs"), "allowlist present");

        assert_eq!(body["tools"][0]["name"], "plan_verdict");
        assert_eq!(body["tool_choice"]["name"], "plan_verdict");
        let schema = &body["tools"][0]["input_schema"];
        assert_eq!(schema["properties"]["verdict"]["type"], "string");
    }

    #[test]
    fn request_body_sets_model() {
        let s = spec();
        let checker = AnthropicPlanChecker::new(None).with_model("claude-test-model");
        let body = checker.request_body("PLAN: create thing", &s);
        assert_eq!(body["model"], "claude-test-model");
        assert_eq!(body["max_tokens"], MAX_TOKENS);
    }

    #[test]
    fn parse_verdict_reads_tool_use() {
        let accept = json!({
            "content": [ {
                "type": "tool_use", "name": "plan_verdict",
                "input": { "verdict": "accept", "reason": "ok" }
            } ]
        });
        assert_eq!(parse_verdict(&accept).unwrap(), PlanVerdict::Accept);

        let reject = json!({
            "content": [ {
                "type": "tool_use", "name": "plan_verdict",
                "input": { "verdict": "reject", "reason": "out of scope" }
            } ]
        });
        assert_eq!(
            parse_verdict(&reject).unwrap(),
            PlanVerdict::Reject { reason: "out of scope".into() }
        );

        // No tool block → Err (drives the permissive fallback).
        let empty = json!({ "content": [ { "type": "text", "text": "hi" } ] });
        assert!(parse_verdict(&empty).is_err());
    }
}
