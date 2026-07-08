//! A wired-but-not-live-tested Anthropic Messages API backend.
//!
//! This runs a one-shot tool-use loop against `POST {base_url}/v1/messages`
//! with a single `write_file` tool. Because CI has no API key, correctness
//! comes from matching the documented wire shape exactly — see
//! [`build_request_body`], which is a pure function so it can be unit-tested
//! without a network call.

use serde_json::{json, Value};

use crate::{
    write_within_worktree, ImplementerBackend, ImplementerError, ImplementerOutcome,
    ImplementerTask,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const MAX_TOKENS: u64 = 16000;
const DEFAULT_TURN_BUDGET: i64 = 25;

/// A real Anthropic Messages API client.
///
/// Construct with [`AnthropicBackend::new`], which takes an optional `base_url`
/// override and does NOT require the API key eagerly — the key is only read at
/// request time in [`ImplementerBackend::run`], which returns
/// [`ImplementerError::Unavailable`] when it is missing. This lets the daemon
/// build the backend from config without a key present (ADR-008).
pub struct AnthropicBackend {
    /// Explicit base-URL override from config. When `None`, the effective URL
    /// is resolved at request time from `$ANTHROPIC_BASE_URL`, else the default.
    base_url_override: Option<String>,
}

impl AnthropicBackend {
    /// Build a backend with an optional `base_url` override (ADR-008). This is
    /// non-eager: it never reads or requires `ANTHROPIC_API_KEY`; that check is
    /// deferred to [`ImplementerBackend::run`].
    pub fn new(base_url: Option<String>) -> Self {
        let base_url_override = base_url.filter(|s| !s.is_empty());
        Self { base_url_override }
    }

    /// Build from the environment. Delegates to [`AnthropicBackend::new`] with
    /// no explicit override, so the base URL is resolved from
    /// `$ANTHROPIC_BASE_URL` (else the default) at request time, and the API key
    /// is checked in `run`.
    pub fn from_env() -> Result<Self, ImplementerError> {
        Ok(Self::new(None))
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
}

impl ImplementerBackend for AnthropicBackend {
    fn run(&self, task: &ImplementerTask) -> Result<ImplementerOutcome, ImplementerError> {
        // The API key is read (and required) here, not at construction time, so
        // the daemon can build the backend from config without a key present.
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            return Err(ImplementerError::Unavailable(
                "ANTHROPIC_API_KEY is unset or empty".into(),
            ));
        }

        let turn_budget = task
            .spec
            .budget
            .turns
            .filter(|t| *t > 0)
            .unwrap_or(DEFAULT_TURN_BUDGET);

        let base_url = self.effective_base_url();
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let mut messages = vec![initial_user_message(task)];
        let mut files_written: Vec<String> = Vec::new();
        let mut turns: u32 = 0;
        let mut tokens_in: u64 = 0;
        let mut tokens_out: u64 = 0;

        loop {
            if (turns as i64) >= turn_budget {
                return Err(ImplementerError::Budget(format!(
                    "turn budget of {turn_budget} exhausted before completion"
                )));
            }
            turns += 1;

            let body = build_request_body(task, &messages);
            let response = send(&url, &api_key, &body)?;

            // Accumulate usage across turns.
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

            let stop_reason = response
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("");

            if stop_reason != "tool_use" {
                // The model is done. Record the assistant turn implicitly by
                // stopping; no more tool results needed.
                let notes = format!(
                    "anthropic finished with stop_reason={stop_reason:?} after {turns} turn(s)"
                );
                return Ok(ImplementerOutcome {
                    files_written,
                    turns,
                    tokens_in,
                    tokens_out,
                    notes,
                });
            }

            // Apply every write_file tool_use block and collect tool_result
            // blocks in reply.
            let mut tool_results: Vec<Value> = Vec::new();
            for block in &content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                if block.get("name").and_then(Value::as_str) != Some("write_file") {
                    continue;
                }
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    ImplementerError::Protocol("tool_use block missing `id`".into())
                })?;
                let input = block.get("input").ok_or_else(|| {
                    ImplementerError::Protocol("tool_use block missing `input`".into())
                })?;
                let path = input.get("path").and_then(Value::as_str).ok_or_else(|| {
                    ImplementerError::Protocol("write_file input missing `path`".into())
                })?;
                let file_content = input
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ImplementerError::Protocol("write_file input missing `content`".into())
                    })?;

                match write_within_worktree(&task.worktree, path, file_content) {
                    Ok(()) => {
                        files_written.push(path.to_string());
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": format!("wrote {path}"),
                        }));
                    }
                    Err(e) => {
                        tool_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": format!("failed to write {path}: {e}"),
                            "is_error": true,
                        }));
                    }
                }
            }

            // Append the assistant message (verbatim content) then the user
            // message carrying the tool results, and loop.
            messages.push(json!({ "role": "assistant", "content": content }));
            messages.push(json!({ "role": "user", "content": tool_results }));
        }
    }
}

/// Build the first user message: the spec context plus, for any allowlisted
/// file that already exists in the worktree, its current contents.
fn initial_user_message(task: &ImplementerTask) -> Value {
    let spec = &task.spec;
    let criteria: Vec<String> = spec
        .acceptance_criteria
        .iter()
        .map(|c| format!("- {} ({:?}): {}", c.id, c.kind, c.check))
        .collect();

    let mut text = String::new();
    text.push_str(&format!("# Task: {}\n\n", spec.title));
    text.push_str("## Instructions\n");
    text.push_str(&spec.instructions);
    text.push_str("\n\n## File allowlist\n");
    if spec.file_allowlist.is_empty() {
        text.push_str("(none specified)\n");
    } else {
        for p in &spec.file_allowlist {
            text.push_str(&format!("- {p}\n"));
        }
    }
    text.push_str("\n## Acceptance criteria\n");
    if criteria.is_empty() {
        text.push_str("(none specified)\n");
    } else {
        text.push_str(&criteria.join("\n"));
        text.push('\n');
    }

    // Include current contents of any allowlisted file that already exists, so
    // the model can edit rather than blindly overwrite.
    let mut existing = String::new();
    for rel in &spec.file_allowlist {
        let candidate = task.worktree.join(rel);
        if candidate.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                existing.push_str(&format!(
                    "\n### Current contents of `{rel}`\n```\n{contents}\n```\n"
                ));
            }
        }
    }
    if !existing.is_empty() {
        text.push_str("\n## Existing files\n");
        text.push_str(&existing);
    }

    json!({ "role": "user", "content": text })
}

/// The `write_file` tool definition (documented wire shape).
fn write_file_tool() -> Value {
    json!({
        "name": "write_file",
        "description": "Create or overwrite a file (path relative to the repo root) with the given content.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }
    })
}

/// Construct the request body for a single Messages API call.
///
/// Pure and network-free so it can be unit-tested. `prior_messages` is the full
/// `messages` array accumulated so far (starting with the initial user turn).
pub fn build_request_body(task: &ImplementerTask, prior_messages: &[Value]) -> Value {
    let system = format!(
        "{house_rules}\n\n\
         You are an implementer. Implement the task specification by calling the \
         `write_file` tool. Only edit files within the allowlist. When you are \
         finished, stop calling tools.",
        house_rules = task.house_rules,
    );

    json!({
        "model": task.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": prior_messages,
        "tools": [write_file_tool()],
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
    use maestro_journal::spec::{AcceptanceCriterion, Budget, CriterionKind, TaskSpec};
    use tempfile::TempDir;

    fn task(model: &str, house_rules: &str) -> ImplementerTask {
        let dir = TempDir::new().unwrap();
        let worktree = dir.path().to_path_buf();
        // Keep the TempDir alive for the duration by leaking it into the task
        // path only; build_request_body does not touch the filesystem, so this
        // is fine for these tests.
        std::mem::forget(dir);
        ImplementerTask {
            spec: TaskSpec {
                title: "Add x".into(),
                tier: Tier::T0,
                base_ref: "main".into(),
                file_allowlist: vec!["src/lib.rs".into()],
                instructions: "Add a function x".into(),
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
            },
            worktree,
            house_rules: house_rules.into(),
            model: model.into(),
        }
    }

    #[test]
    fn run_unavailable_without_key() {
        // Construction is non-eager (ADR-008): building the backend never
        // requires a key. The key is required at `run` time, which returns
        // `Unavailable` when it is missing.
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        // from_env and new both construct without a key.
        assert!(AnthropicBackend::from_env().is_ok());
        let backend = AnthropicBackend::new(None);
        let t = task("claude-sonnet-4-6", "HR");
        let res = backend.run(&t);
        assert!(
            matches!(res, Err(ImplementerError::Unavailable(_))),
            "run must fail Unavailable without a key, got {res:?}"
        );

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }

    #[test]
    fn effective_base_url_precedence() {
        // Explicit override wins regardless of env.
        let saved = std::env::var("ANTHROPIC_BASE_URL").ok();
        std::env::set_var("ANTHROPIC_BASE_URL", "http://env.example");
        let b = AnthropicBackend::new(Some("http://override.example".into()));
        assert_eq!(b.effective_base_url(), "http://override.example");

        // No override → env is used.
        let b2 = AnthropicBackend::new(None);
        assert_eq!(b2.effective_base_url(), "http://env.example");

        // No override, no env → the compiled-in default.
        std::env::remove_var("ANTHROPIC_BASE_URL");
        let b3 = AnthropicBackend::new(None);
        assert_eq!(b3.effective_base_url(), DEFAULT_BASE_URL);

        if let Some(v) = saved {
            std::env::set_var("ANTHROPIC_BASE_URL", v);
        }
    }

    #[test]
    fn build_request_body_matches_wire_shape() {
        let t = task("claude-sonnet-4-6", "HR");
        let messages = vec![initial_user_message(&t)];
        let body = build_request_body(&t, &messages);

        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["tools"][0]["name"], "write_file");
        assert_eq!(body["max_tokens"], MAX_TOKENS);

        let system = body["system"].as_str().unwrap();
        assert!(system.contains("HR"), "system should contain house rules");

        // messages carried through verbatim.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");

        // tool schema shape.
        let schema = &body["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");
    }

    #[test]
    fn build_request_body_takes_model_from_task() {
        let t = task("some-other-model", "rules");
        let body = build_request_body(&t, &[]);
        assert_eq!(body["model"], "some-other-model");
    }
}
