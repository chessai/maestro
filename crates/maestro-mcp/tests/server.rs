//! MCP server unit tests (ADR-006 AC4–AC7). All drive [`McpServer::handle_line`]
//! through an injected fake transport that records the requests it received and
//! returns scripted responses — NO real daemon is ever spawned.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{json, Value};

use maestro_journal::proto::{InboxItem, PsRow, Request, Response};
use maestro_mcp::{DaemonTransport, McpServer};

/// A fake transport: records every request and returns responses from a script.
/// The script is consumed front-to-back; if exhausted, an error is returned.
/// `recorded` is a shared handle so the test can inspect requests after handing
/// the fake to the server (which owns it inside a `Box`).
struct FakeTransport {
    recorded: Rc<RefCell<Vec<Request>>>,
    script: RefCell<Vec<Response>>,
}

impl FakeTransport {
    /// Returns the fake plus a shared handle onto its recording buffer.
    fn new(script: Vec<Response>) -> (Self, Rc<RefCell<Vec<Request>>>) {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let fake = Self {
            recorded: Rc::clone(&recorded),
            script: RefCell::new(script),
        };
        (fake, recorded)
    }
}

impl DaemonTransport for FakeTransport {
    fn call(&self, req: &Request) -> anyhow::Result<Response> {
        self.recorded.borrow_mut().push(req.clone());
        let mut script = self.script.borrow_mut();
        if script.is_empty() {
            anyhow::bail!("fake transport: script exhausted");
        }
        Ok(script.remove(0))
    }
}

fn parse(response: &str) -> Value {
    serde_json::from_str(response).expect("valid JSON-RPC response")
}

// AC4: initialize.
#[test]
fn ac4_initialize_echoes_id_and_identifies_server() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);

    let line = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" }
    })
    .to_string();

    let resp = server.handle_line(&line).expect("initialize returns a response");
    let v = parse(&resp);

    assert_eq!(v["id"], json!(1), "id must be echoed");
    assert_eq!(v["result"]["serverInfo"]["name"], "maestro");
    assert!(v["result"]["protocolVersion"].is_string());
    assert!(
        v["result"]["capabilities"]["tools"].is_object(),
        "capabilities.tools must be present"
    );
}

#[test]
fn ac4_initialize_defaults_protocol_version() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let line = json!({ "jsonrpc": "2.0", "id": "abc", "method": "initialize" }).to_string();
    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!("abc"));
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
}

// AC5: tools/list.
#[test]
fn ac5_tools_list_has_exactly_the_eight_tools() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let line = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string();
    let v = parse(&server.handle_line(&line).unwrap());

    let tools = v["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 8, "exactly eight tools (M1 + M2 + M3 + ADR-005 + merge_task)");

    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"delegate"));
    assert!(names.contains(&"task_status"));
    assert!(names.contains(&"close_task"));
    assert!(names.contains(&"merge_task"));
    assert!(names.contains(&"journal_query"));
    assert!(names.contains(&"kill_task"));
    assert!(names.contains(&"search"));
    assert!(names.contains(&"fetch_extract"));

    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object");
        assert!(
            t["inputSchema"]["properties"].is_object()
                && !t["inputSchema"]["properties"].as_object().unwrap().is_empty(),
            "inputSchema must be a non-empty object schema"
        );
    }
}

// AC6: tools/call delegate — advisor minted once, request forwarded, inbox appended.
#[test]
fn ac6_delegate_forwards_request_and_appends_inbox() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Delegate {
            task_id: "T1".into(),
        },
        Response::Inbox {
            items: vec![InboxItem {
                event_id: "E1".into(),
                task_id: "T1".into(),
                ts: "2026-07-07T00:00:00Z".into(),
                kind: "verify_passed".into(),
                summary: "task T1 verified".into(),
                detail: None,
            }],
        },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "delegate",
            "arguments": {
                "repo_path": "/repo/foo",
                "spec": {
                    "title": "add widget",
                    "tier": 1,
                    "base_ref": "main",
                    "file_allowlist": ["src/**"],
                    "instructions": "build the widget",
                    "acceptance_criteria": [
                        { "id": "AC1", "check": "cargo test", "kind": "command" }
                    ],
                    "check_commands": ["cargo test"],
                    "containment_min": 0
                }
            }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(3));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("T1"), "result mentions the task id: {text}");
    assert!(
        text.contains("task T1 verified"),
        "result appends the inbox summary: {text}"
    );

    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, Delegate, DrainInbox");

    // Advisor minted FIRST.
    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    // Delegate forwarded with matching repo_path and spec.title.
    match &recorded[1] {
        Request::Delegate {
            advisor_session_id,
            repo_path,
            spec,
        } => {
            assert_eq!(advisor_session_id, "adv-1");
            assert_eq!(repo_path, "/repo/foo");
            assert_eq!(spec.title, "add widget");
        }
        other => panic!("second call must be Delegate, got {other:?}"),
    }
    // Then the inbox drain.
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

#[test]
fn ac6b_advisor_minted_only_once_across_two_calls() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::TaskStatus {
            tasks: vec![PsRow {
                task_id: "T1".into(),
                title: "widget".into(),
                tier: maestro_journal::domain::Tier::T1,
                model: "m".into(),
                containment: maestro_journal::domain::ContainmentLevel::L0,
                state: "running".into(),
                created_at: "2026-07-07T00:00:00Z".into(),
            }],
        },
        Response::Inbox { items: vec![] },
        // Second task_status call: NO second RegisterAdvisor expected.
        Response::TaskStatus { tasks: vec![] },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "task_status", "arguments": {} }
    })
    .to_string();

    let v1 = parse(&server.handle_line(&call).unwrap());
    assert!(v1["result"]["content"][0]["text"].as_str().unwrap().contains("T1"));

    let v2 = parse(&server.handle_line(&call).unwrap());
    assert!(v2["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("no tasks"));

    let recorded = recorded.borrow();
    let register_count = recorded
        .iter()
        .filter(|r| matches!(r, Request::RegisterAdvisor { .. }))
        .count();
    assert_eq!(register_count, 1, "advisor minted exactly once");
}

// AC7: notification returns None; unknown method → -32601; malformed → -32700.
#[test]
fn ac7_notification_returns_none() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    assert!(server.handle_line(&line).is_none());
}

#[test]
fn ac7_unknown_method_is_method_not_found() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let line = json!({ "jsonrpc": "2.0", "id": 9, "method": "does/not/exist" }).to_string();
    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["error"]["code"], json!(-32601));
    assert_eq!(v["id"], json!(9));
}

#[test]
fn ac7_malformed_json_is_parse_error() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let v = parse(&server.handle_line("{not json").unwrap());
    assert_eq!(v["error"]["code"], json!(-32700));
}

#[test]
fn ping_replies_empty_object() {
    let (fake, _rec) = FakeTransport::new(vec![]);
    let fake = Box::new(fake);
    let mut server = McpServer::new(fake, None);
    let line = json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }).to_string();
    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(5));
    assert_eq!(v["result"], json!({}));
}

#[test]
fn delegate_daemon_error_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "base_ref not found".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);
    let line = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "delegate",
            "arguments": {
                "repo_path": "/repo",
                "spec": {
                    "title": "t", "tier": 0, "base_ref": "nope",
                    "instructions": "x",
                    "acceptance_criteria": [{ "id": "AC1", "check": "c", "kind": "command" }]
                }
            }
        }
    })
    .to_string();
    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("base_ref not found"));
}

// AC4 (new): close_task forwards CloseTask and returns "closed: <task_id>".
#[test]
fn ac4_new_close_task_forwards_request_and_returns_closed() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Closed {
            task_id: "T1".into(),
        },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {
            "name": "close_task",
            "arguments": {
                "task_id": "T1",
                "outcome": "abandoned"
            }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(10));

    // Result must not be an error.
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("T1"), "result text must contain task id: {text}");

    let recorded = recorded.borrow();
    // Expect: RegisterAdvisor, CloseTask, DrainInbox.
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, CloseTask, DrainInbox");

    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    match &recorded[1] {
        Request::CloseTask {
            advisor_session_id,
            task_id,
            outcome,
            successor,
        } => {
            assert_eq!(advisor_session_id, "adv-1");
            assert_eq!(task_id, "T1");
            assert_eq!(outcome, "abandoned");
            assert_eq!(*successor, None);
        }
        other => panic!("second call must be CloseTask, got {other:?}"),
    }
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

// AC4 (new): close_task with daemon Error → isError result.
#[test]
fn ac4_new_close_task_daemon_error_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "task not blocked".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 11, "method": "tools/call",
        "params": {
            "name": "close_task",
            "arguments": { "task_id": "T1", "outcome": "abandoned" }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("task not blocked"));
}

// AC5 (new): journal_query forwards JournalQuery and returns pretty-printed JSON.
#[test]
fn ac5_new_journal_query_forwards_request_and_returns_json() {
    let result_value = serde_json::json!([{"report": "ok"}, {"report": "fail"}]);
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::JournalResult {
            value: result_value.clone(),
        },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {
            "name": "journal_query",
            "arguments": {
                "query": "verifier_reports",
                "task_id": "T1"
            }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(20));
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    // Result text must contain the serialised array.
    assert!(
        text.contains("report"),
        "result text must contain array contents: {text}"
    );

    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, JournalQuery, DrainInbox");

    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    match &recorded[1] {
        Request::JournalQuery {
            advisor_session_id,
            query,
            params,
        } => {
            assert_eq!(advisor_session_id, "adv-1");
            assert_eq!(query, "verifier_reports");
            assert_eq!(params["task_id"], "T1");
        }
        other => panic!("second call must be JournalQuery, got {other:?}"),
    }
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

// AC5 (new): journal_query with daemon Error → isError result.
#[test]
fn ac5_new_journal_query_daemon_error_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "unknown query".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 21, "method": "tools/call",
        "params": {
            "name": "journal_query",
            "arguments": { "query": "bad_query", "task_id": "T1" }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unknown query"));
}

// AC4 (kill_task): kill_task forwards KillTask{task_id, kind="advisor"} and returns "killed: <task_id>".
#[test]
fn ac4_kill_task_forwards_request_and_returns_killed() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Killed {
            task_id: "T1".into(),
        },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 30, "method": "tools/call",
        "params": {
            "name": "kill_task",
            "arguments": { "task_id": "T1" }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(30));

    // Result must not be an error.
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("T1"), "result text must contain task id: {text}");
    assert!(text.contains("killed"), "result text must contain 'killed': {text}");

    let recorded = recorded.borrow();
    // Expect: RegisterAdvisor, KillTask, DrainInbox.
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, KillTask, DrainInbox");

    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    match &recorded[1] {
        Request::KillTask { task_id, kind } => {
            assert_eq!(task_id, "T1");
            assert_eq!(kind, "advisor");
        }
        other => panic!("second call must be KillTask, got {other:?}"),
    }
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

// AC4 (kill_task): kill_task with daemon Error → isError result.
#[test]
fn ac4_kill_task_daemon_error_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "task is not a running driven session".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 31, "method": "tools/call",
        "params": {
            "name": "kill_task",
            "arguments": { "task_id": "T1" }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("task is not a running driven session"));
}

// AC4 (search): search forwards Request::Search and returns pretty-printed results.
#[test]
fn ac4_search_forwards_request_and_returns_results() {
    let results_value = serde_json::json!([
        {"url": "https://doc.rust-lang.org/", "title": "Rust Ownership", "snippet": "..."}
    ]);
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::SearchResults {
            results: results_value.clone(),
        },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 40, "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["rust ownership"] }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(40));
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("Rust Ownership"),
        "result text must contain search results: {text}"
    );
    assert!(
        text.contains("https://doc.rust-lang.org/"),
        "result text must contain the url: {text}"
    );

    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, Search, DrainInbox");

    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    match &recorded[1] {
        Request::Search {
            advisor_session_id,
            queries,
        } => {
            assert_eq!(advisor_session_id, "adv-1");
            assert_eq!(queries, &["rust ownership"]);
        }
        other => panic!("second call must be Search, got {other:?}"),
    }
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

// AC4 (search): search with daemon Error{message:"backend_unavailable: ..."} → isError with message.
#[test]
fn ac4_search_backend_unavailable_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "backend_unavailable: no search backend configured".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 41, "method": "tools/call",
        "params": {
            "name": "search",
            "arguments": { "queries": ["rust ownership"] }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("backend_unavailable"),
        "error message must surface verbatim: {text}"
    );
}

// AC5 (fetch_extract): fetch_extract forwards Request::FetchExtract and returns extraction.
#[test]
fn ac5_fetch_extract_forwards_request_and_returns_extraction() {
    let extraction_value = serde_json::json!({
        "price": { "text": "$9.99", "start": 42, "end": 47 }
    });
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Extraction {
            extraction: extraction_value.clone(),
        },
        Response::Inbox { items: vec![] },
    ];
    let (fake, recorded) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 50, "method": "tools/call",
        "params": {
            "name": "fetch_extract",
            "arguments": {
                "url": "https://x",
                "schema_fields": ["price"]
            }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(50));
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("price"),
        "result text must contain extracted field: {text}"
    );
    assert!(
        text.contains("$9.99"),
        "result text must contain extracted value: {text}"
    );

    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 3, "RegisterAdvisor, FetchExtract, DrainInbox");

    match &recorded[0] {
        Request::RegisterAdvisor { .. } => {}
        other => panic!("first call must be RegisterAdvisor, got {other:?}"),
    }
    match &recorded[1] {
        Request::FetchExtract {
            advisor_session_id,
            url,
            schema_fields,
        } => {
            assert_eq!(advisor_session_id, "adv-1");
            assert_eq!(url, "https://x");
            assert_eq!(schema_fields, &["price"]);
        }
        other => panic!("second call must be FetchExtract, got {other:?}"),
    }
    match &recorded[2] {
        Request::DrainInbox { advisor_session_id } => assert_eq!(advisor_session_id, "adv-1"),
        other => panic!("third call must be DrainInbox, got {other:?}"),
    }
}

// AC5 (fetch_extract): fetch_extract with daemon Error → isError result.
#[test]
fn ac5_fetch_extract_daemon_error_is_tool_error() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1".into(),
        },
        Response::Error {
            message: "model_unavailable: extraction model not configured".into(),
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0", "id": 51, "method": "tools/call",
        "params": {
            "name": "fetch_extract",
            "arguments": { "url": "https://x", "schema_fields": ["price"] }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["result"]["isError"], json!(true));
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("model_unavailable"));
}

// ADR-007: append_inbox renders `detail` (inlined payload) beneath the summary
// line when an item carries `detail: Some(...)`.
#[test]
fn append_inbox_renders_detail_when_present() {
    let script = vec![
        Response::RegisterAdvisor {
            advisor_session_id: "adv-1m".into(),
        },
        Response::Delegate {
            task_id: "T2".into(),
        },
        // Inbox with one item carrying a detail payload and one without.
        Response::Inbox {
            items: vec![
                InboxItem {
                    event_id: "E2".into(),
                    task_id: "T2".into(),
                    ts: "2026-07-08T00:00:00Z".into(),
                    kind: "failed".into(),
                    summary: "failed — my task".into(),
                    detail: Some(r#"{"kind":"verification_failed"}"#.into()),
                },
                InboxItem {
                    event_id: "E3".into(),
                    task_id: "T2".into(),
                    ts: "2026-07-08T00:00:01Z".into(),
                    kind: "verify_passed".into(),
                    summary: "verify_passed — my task".into(),
                    detail: None,
                },
            ],
        },
    ];
    let (fake, _rec) = FakeTransport::new(script);
    let mut server = McpServer::new(Box::new(fake), None);

    let line = json!({
        "jsonrpc": "2.0",
        "id": 60,
        "method": "tools/call",
        "params": {
            "name": "delegate",
            "arguments": {
                "repo_path": "/repo/bar",
                "spec": {
                    "title": "my task",
                    "tier": 0,
                    "base_ref": "main",
                    "instructions": "do it",
                    "acceptance_criteria": [
                        { "id": "AC1", "check": "true", "kind": "command" }
                    ]
                }
            }
        }
    })
    .to_string();

    let v = parse(&server.handle_line(&line).unwrap());
    assert_eq!(v["id"], json!(60));
    assert_ne!(v["result"]["isError"], json!(true));

    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");

    // The detail payload must appear beneath its summary line.
    assert!(
        text.contains("verification_failed"),
        "rendered text must include the inlined payload: {text}"
    );
    // The ↳ prefix must separate detail from the summary line.
    assert!(
        text.contains("↳"),
        "rendered text must use the ↳ prefix for detail: {text}"
    );
    // The item without detail must appear but NOT with a ↳ line.
    assert!(
        text.contains("verify_passed — my task"),
        "non-detail item summary must appear: {text}"
    );
}
