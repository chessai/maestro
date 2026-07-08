//! The MCP server core (ADR-006): transport-agnostic JSON-RPC 2.0 handling over
//! newline-delimited stdio. This is the testable heart — [`McpServer::handle_line`]
//! is the unit-test seam; a fake [`DaemonTransport`] drives the whole flow with no
//! real daemon.

use anyhow::Result;
use serde_json::{json, Value};

use maestro_journal::proto::{InboxItem, PsRow, Request, Response};
use maestro_journal::spec::TaskSpec;

use crate::transport::DaemonTransport;

/// The protocol version advertised when the client sends none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "maestro";
const SERVER_VERSION: &str = "0.0.0";

/// JSON-RPC error codes we emit for protocol-level problems.
const CODE_PARSE_ERROR: i64 = -32700;
const CODE_METHOD_NOT_FOUND: i64 = -32601;

/// The MCP server: holds the daemon transport, the active profile, and the
/// lazily-minted advisor session id (minted on the first tool call).
pub struct McpServer {
    transport: Box<dyn DaemonTransport>,
    profile: Option<String>,
    advisor_session_id: Option<String>,
}

impl McpServer {
    /// Build a server over `transport`, forwarding `profile` to `RegisterAdvisor`.
    pub fn new(transport: Box<dyn DaemonTransport>, profile: Option<String>) -> Self {
        Self {
            transport,
            profile,
            advisor_session_id: None,
        }
    }

    /// Handle one JSON-RPC message line. Returns `Some(response_json)` for
    /// requests (which carry an `id`) and `None` for notifications (no `id`).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => {
                // Parse error: id unknown, so it is null per JSON-RPC.
                return Some(error_response(Value::Null, CODE_PARSE_ERROR, "parse error"));
            }
        };

        let method = msg.get("method").and_then(Value::as_str);
        let has_id = msg.get("id").is_some();
        let id = msg.get("id").cloned().unwrap_or(Value::Null);

        // Notifications carry no `id`; we answer nothing.
        if !has_id {
            // (e.g. notifications/initialized) — no response.
            return None;
        }

        let method = match method {
            Some(m) => m,
            None => {
                return Some(error_response(
                    id,
                    CODE_METHOD_NOT_FOUND,
                    "missing method",
                ));
            }
        };

        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => Some(ok_response(id, Self::initialize(&params))),
            "ping" => Some(ok_response(id, json!({}))),
            "tools/list" => Some(ok_response(id, Self::tools_list())),
            "tools/call" => Some(ok_response(id, self.tools_call(&params))),
            _ => Some(error_response(
                id,
                CODE_METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            )),
        }
    }

    /// `initialize`: echo the client's protocolVersion (else our default),
    /// advertise the `tools` capability, and identify the server.
    fn initialize(params: &Value) -> Value {
        let protocol_version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        json!({
            "protocolVersion": protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })
    }

    /// `tools/list`: the M1 + M2 + M3 + ADR-005 advisor tools.
    fn tools_list() -> Value {
        json!({ "tools": [ delegate_tool(), task_status_tool(), close_task_tool(), journal_query_tool(), kill_task_tool(), search_tool(), fetch_extract_tool() ] })
    }

    /// `tools/call`: mint the advisor on first call, dispatch, then drain the
    /// inbox and append it to the result text.
    fn tools_call(&mut self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);

        // Mint the advisor session lazily on the first tool call.
        let advisor_id = match self.ensure_advisor() {
            Ok(id) => id,
            Err(e) => {
                return tool_error(format!("advisor registration failed: {e:#}"));
            }
        };

        let mut result = match name {
            "delegate" => self.call_delegate(&advisor_id, args),
            "task_status" => self.call_task_status(&advisor_id, args),
            "close_task" => self.call_close_task(&advisor_id, args),
            "journal_query" => self.call_journal_query(&advisor_id, args),
            "kill_task" => self.call_kill_task(args),
            "search" => self.call_search(&advisor_id, args),
            "fetch_extract" => self.call_fetch_extract(&advisor_id, args),
            other => tool_error(format!("unknown tool: {other}")),
        };

        // Best-effort inbox drain appended to a SUCCESSFUL tool result. A drain
        // failure must never fail the tool call (ADR-006).
        if !is_error_result(&result) {
            let inbox = self.drain_inbox(&advisor_id);
            append_inbox(&mut result, &inbox);
        }
        result
    }

    /// Mint (once) and return the advisor session id via `RegisterAdvisor`.
    fn ensure_advisor(&mut self) -> Result<String> {
        if let Some(id) = &self.advisor_session_id {
            return Ok(id.clone());
        }
        let req = Request::RegisterAdvisor {
            profile: self.profile.clone(),
        };
        match self.transport.call(&req)? {
            Response::RegisterAdvisor { advisor_session_id } => {
                self.advisor_session_id = Some(advisor_session_id.clone());
                Ok(advisor_session_id)
            }
            Response::Error { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected daemon response to RegisterAdvisor: {other:?}"
            )),
        }
    }

    fn call_delegate(&mut self, advisor_id: &str, args: &Value) -> Value {
        let repo_path = match args.get("repo_path").and_then(Value::as_str) {
            Some(p) => p.to_string(),
            None => return tool_error("delegate: missing required `repo_path` string".into()),
        };
        let spec_val = match args.get("spec") {
            Some(s) => s.clone(),
            None => return tool_error("delegate: missing required `spec` object".into()),
        };
        let spec: TaskSpec = match serde_json::from_value(spec_val) {
            Ok(s) => s,
            Err(e) => return tool_error(format!("delegate: invalid `spec`: {e}")),
        };

        let req = Request::Delegate {
            advisor_session_id: advisor_id.to_string(),
            repo_path,
            spec: Box::new(spec),
        };
        match self.transport.call(&req) {
            Ok(Response::Delegate { task_id }) => tool_text(format!("delegated: task {task_id}")),
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("delegate failed: {e:#}")),
        }
    }

    fn call_task_status(&mut self, advisor_id: &str, args: &Value) -> Value {
        let state = args
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string);
        let req = Request::TaskStatus {
            advisor_session_id: advisor_id.to_string(),
            state,
        };
        match self.transport.call(&req) {
            Ok(Response::TaskStatus { tasks }) => tool_text(format_ps_rows(&tasks)),
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("task_status failed: {e:#}")),
        }
    }

    fn call_close_task(&mut self, advisor_id: &str, args: &Value) -> Value {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return tool_error("close_task: missing required `task_id` string".into()),
        };
        let outcome = match args.get("outcome").and_then(Value::as_str) {
            Some(o) => o.to_string(),
            None => return tool_error("close_task: missing required `outcome` string".into()),
        };
        let successor = args
            .get("successor")
            .and_then(Value::as_str)
            .map(str::to_string);

        let req = Request::CloseTask {
            advisor_session_id: advisor_id.to_string(),
            task_id,
            outcome,
            successor,
        };
        match self.transport.call(&req) {
            Ok(Response::Closed { task_id }) => tool_text(format!("closed: {task_id}")),
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("close_task failed: {e:#}")),
        }
    }

    fn call_journal_query(&mut self, advisor_id: &str, args: &Value) -> Value {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) => q.to_string(),
            None => return tool_error("journal_query: missing required `query` string".into()),
        };
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return tool_error("journal_query: missing required `task_id` string".into()),
        };

        let req = Request::JournalQuery {
            advisor_session_id: advisor_id.to_string(),
            query,
            params: serde_json::json!({ "task_id": task_id }),
        };
        match self.transport.call(&req) {
            Ok(Response::JournalResult { value }) => {
                let text = serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|e| format!("serialization error: {e}"));
                tool_text(text)
            }
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("journal_query failed: {e:#}")),
        }
    }

    fn call_kill_task(&mut self, args: &Value) -> Value {
        let task_id = match args.get("task_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => return tool_error("kill_task: missing required `task_id` string".into()),
        };

        let req = Request::KillTask {
            task_id,
            kind: "advisor".to_string(),
        };
        match self.transport.call(&req) {
            Ok(Response::Killed { task_id }) => tool_text(format!("killed: {task_id}")),
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("kill_task failed: {e:#}")),
        }
    }

    fn call_search(&mut self, advisor_id: &str, args: &Value) -> Value {
        let queries: Vec<String> = match args.get("queries").and_then(Value::as_array) {
            Some(arr) => {
                let mut v = Vec::with_capacity(arr.len());
                for item in arr {
                    match item.as_str() {
                        Some(s) => v.push(s.to_string()),
                        None => return tool_error("search: `queries` must be an array of strings".into()),
                    }
                }
                v
            }
            None => return tool_error("search: missing required `queries` array".into()),
        };

        let req = Request::Search {
            advisor_session_id: advisor_id.to_string(),
            queries,
        };
        match self.transport.call(&req) {
            Ok(Response::SearchResults { results }) => {
                let text = serde_json::to_string_pretty(&results)
                    .unwrap_or_else(|e| format!("serialization error: {e}"));
                tool_text(text)
            }
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("search failed: {e:#}")),
        }
    }

    fn call_fetch_extract(&mut self, advisor_id: &str, args: &Value) -> Value {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(u) => u.to_string(),
            None => return tool_error("fetch_extract: missing required `url` string".into()),
        };
        let schema_fields: Vec<String> = match args.get("schema_fields").and_then(Value::as_array) {
            Some(arr) => {
                let mut v = Vec::with_capacity(arr.len());
                for item in arr {
                    match item.as_str() {
                        Some(s) => v.push(s.to_string()),
                        None => return tool_error("fetch_extract: `schema_fields` must be an array of strings".into()),
                    }
                }
                v
            }
            None => return tool_error("fetch_extract: missing required `schema_fields` array".into()),
        };

        let req = Request::FetchExtract {
            advisor_session_id: advisor_id.to_string(),
            url,
            schema_fields,
        };
        match self.transport.call(&req) {
            Ok(Response::Extraction { extraction }) => {
                let text = serde_json::to_string_pretty(&extraction)
                    .unwrap_or_else(|e| format!("serialization error: {e}"));
                tool_text(text)
            }
            Ok(Response::Error { message }) => tool_error(message),
            Ok(other) => tool_error(format!("unexpected daemon response: {other:?}")),
            Err(e) => tool_error(format!("fetch_extract failed: {e:#}")),
        }
    }

    /// Drain the advisor inbox (best-effort). On any failure, log to stderr and
    /// return an empty list so the tool call still succeeds.
    fn drain_inbox(&mut self, advisor_id: &str) -> Vec<InboxItem> {
        let req = Request::DrainInbox {
            advisor_session_id: advisor_id.to_string(),
        };
        match self.transport.call(&req) {
            Ok(Response::Inbox { items }) => items,
            Ok(other) => {
                eprintln!("maestro-mcp: unexpected DrainInbox response: {other:?}");
                Vec::new()
            }
            Err(e) => {
                eprintln!("maestro-mcp: DrainInbox failed (ignored): {e:#}");
                Vec::new()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC envelopes.
// ---------------------------------------------------------------------------

fn ok_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Tool results (MCP `content` shape).
// ---------------------------------------------------------------------------

/// A successful tool result: one text content block, no `isError`.
fn tool_text(text: String) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ] })
}

/// A tool *failure* result: normal result with `isError: true` (ADR-006 / MCP).
fn tool_error(text: String) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": true })
}

fn is_error_result(result: &Value) -> bool {
    result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Append the drained inbox items to the first text content block of `result`.
fn append_inbox(result: &mut Value, items: &[InboxItem]) {
    let section = if items.is_empty() {
        "\n\n— inbox — (empty)".to_string()
    } else {
        let mut s = String::from("\n\n— inbox —");
        for item in items {
            s.push_str("\n- ");
            s.push_str(&item.summary);
        }
        s
    };
    if let Some(text_slot) = result
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .and_then(|arr| arr.first_mut())
        .and_then(|block| block.get_mut("text"))
    {
        if let Some(existing) = text_slot.as_str() {
            *text_slot = Value::String(format!("{existing}{section}"));
        }
    }
}

/// Format `PsRow`s as readable text: `task_id  T<tier>  <state>  <title>`.
fn format_ps_rows(rows: &[PsRow]) -> String {
    if rows.is_empty() {
        return "no tasks".to_string();
    }
    let mut out = String::new();
    for row in rows {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{}  T{}  {}  {}",
            row.task_id,
            row.tier.as_int(),
            row.state,
            row.title
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Tool schemas.
// ---------------------------------------------------------------------------

fn delegate_tool() -> Value {
    json!({
        "name": "delegate",
        "description": "Delegate a task to a daemon-managed worktree. Provide the repo path and \
            a TaskSpec; the daemon creates the task and returns its id.",
        "inputSchema": {
            "type": "object",
            "required": ["repo_path", "spec"],
            "properties": {
                "repo_path": {
                    "type": "string",
                    "description": "Absolute path to the repo the worktree branches from."
                },
                "spec": {
                    "type": "object",
                    "description": "The immutable TaskSpec (ADR-003).",
                    "required": ["title", "tier", "base_ref", "instructions", "acceptance_criteria"],
                    "properties": {
                        "title": { "type": "string" },
                        "tier": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 2,
                            "description": "0 = mechanical, 1 = bounded impl, 2 = architectural."
                        },
                        "base_ref": { "type": "string" },
                        "file_allowlist": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Paths/globs enforced by the mechanical gate."
                        },
                        "instructions": { "type": "string" },
                        "acceptance_criteria": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["id", "check", "kind"],
                                "properties": {
                                    "id": { "type": "string" },
                                    "check": { "type": "string" },
                                    "kind": { "type": "string", "enum": ["command", "invariant"] }
                                }
                            }
                        },
                        "check_commands": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "containment_min": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Can only RAISE the containment floor (ADR-004)."
                        }
                    }
                }
            }
        }
    })
}

fn task_status_tool() -> Value {
    json!({
        "name": "task_status",
        "description": "List the advisor's tasks, optionally filtered by state.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "state": {
                    "type": "string",
                    "description": "Optional state filter (e.g. running, verified, blocked)."
                }
            }
        }
    })
}

fn close_task_tool() -> Value {
    json!({
        "name": "close_task",
        "description": "Resolve a blocked task by recording a terminal outcome. \
            `outcome` must be `abandoned` (task is dropped) or `superseded` \
            (replaced by a successor task). Pass `successor` when superseding.",
        "inputSchema": {
            "type": "object",
            "required": ["task_id", "outcome"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The id of the blocked task to close."
                },
                "outcome": {
                    "type": "string",
                    "enum": ["abandoned", "superseded"],
                    "description": "Terminal outcome: `abandoned` or `superseded`."
                },
                "successor": {
                    "type": "string",
                    "description": "Optional id of the successor task (used when outcome is `superseded`)."
                }
            }
        }
    })
}

fn journal_query_tool() -> Value {
    json!({
        "name": "journal_query",
        "description": "Run a named, read-only journal query. Supported query names: \
            `verifier_reports` (verifier run records for a task) and \
            `trace` (full event log for a task). Both require `task_id`.",
        "inputSchema": {
            "type": "object",
            "required": ["query", "task_id"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Named query: `verifier_reports` or `trace`."
                },
                "task_id": {
                    "type": "string",
                    "description": "The task id to query."
                }
            }
        }
    })
}

fn kill_task_tool() -> Value {
    json!({
        "name": "kill_task",
        "description": "Break-glass kill of a running driven session started by this advisor. \
            Sends SIGTERM to the session and records `interrupted_advisor` in the journal. \
            Use only when a delegated task must be forcibly stopped.",
        "inputSchema": {
            "type": "object",
            "required": ["task_id"],
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The id of the running driven session to kill."
                }
            }
        }
    })
}

fn search_tool() -> Value {
    json!({
        "name": "search",
        "description": "Web search via the configured backend; returns metadata only \
            (url/title/snippet), never model-synthesized text.",
        "inputSchema": {
            "type": "object",
            "required": ["queries"],
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "One or more search query strings."
                }
            }
        }
    })
}

fn fetch_extract_tool() -> Value {
    json!({
        "name": "fetch_extract",
        "description": "Fetch a URL and return verbatim excerpts with character offsets for each \
            requested field; there is NO free-text summary — offsets are daemon-validated.",
        "inputSchema": {
            "type": "object",
            "required": ["url", "schema_fields"],
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch and extract from."
                },
                "schema_fields": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The field names to extract verbatim spans for."
                }
            }
        }
    })
}
