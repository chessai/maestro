//! Acceptance tests for maestro-journal (AC3–AC7).

use maestro_journal::config::{Config, RoleModel};
use maestro_journal::domain::{
    ContainmentLevel, EventKind, ExitStatus, Independence, Role, SessionKind, Tier,
};
use maestro_journal::report::{CommandRun, Finding, ReportBody, Severity, Verdict};
use maestro_journal::schema;
use maestro_journal::Journal;
use rusqlite::Connection;

/// The ADR-007 example config, verbatim.
const ADR_007_TOML: &str = r#"
default_profile = "personal"

[defaults]
concurrency.machine_cap = 4
concurrency.advisor_cap = 2
watchdog_minutes = 10
shim.excerpt_cap_chars = 1500
shim.cache_ttl_hours = 24
downgrade_policy = "tighten"
tighten.allowlist_factor = 0.5
tighten.turn_factor = 0.6
advisor.writable_paths = []
lifetime.token_factor = 1.0
lifetime.wall_clock_minutes = 30

[profiles.personal]
advisor.model = "claude-fable-5"
advisor.context = "standard"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "codex", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-8"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 1, tier2 = 2 }
search.backend = "searxng"
search.endpoint = "https://searx.internal:8443"

[profiles.work]
advisor.model = "claude-opus-4-7"
advisor.context = "1m"
roles.tier0 = "claude-sonnet-4-6"
roles.tier1 = { model = "claude-sonnet-4-6", kind = "driven_cli", turn_budget = 25 }
roles.tier2 = "claude-opus-4-7"
roles.verifier_floor = "claude-sonnet-4-6"
containment_min = { tier0 = 0, tier1 = 0, tier2 = 1 }
downgrade_policy = "tighten"
"#;

// AC3: applying migrations twice is idempotent (a no-op via user_version).
#[test]
fn ac3_migrations_idempotent() {
    let conn = Connection::open_in_memory().unwrap();
    schema::migrate(&conn).unwrap();
    let v1: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v1, schema::SCHEMA_VERSION);

    // Second apply must be a no-op and must not error (tables already exist).
    schema::migrate(&conn).unwrap();
    let v2: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v2, schema::SCHEMA_VERSION);

    // Sanity: all seven tables exist exactly once.
    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'
               AND name IN ('advisors','tasks','events','advisor_events',
                            'sessions','verifier_reports','shim_cache')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 7);
}

// AC4: the ADR-007 example parses, both profiles, including roles-as-table.
#[test]
fn ac4_adr007_config_parses() {
    let cfg = Config::from_toml_str(ADR_007_TOML).expect("ADR-007 example must parse");
    assert_eq!(cfg.default_profile.as_deref(), Some("personal"));
    assert_eq!(cfg.defaults.concurrency.machine_cap, 4);
    assert_eq!(cfg.defaults.tighten.allowlist_factor, 0.5);
    assert_eq!(cfg.defaults.lifetime.wall_clock_minutes, 30);

    let personal = cfg.profiles.get("personal").expect("personal profile");
    // Bare-string role.
    assert!(matches!(
        personal.roles.tier0.as_ref().unwrap(),
        RoleModel::Bare(m) if m == "claude-sonnet-4-6"
    ));
    // Table-form role for tier1.
    match personal.roles.tier1.as_ref().unwrap() {
        RoleModel::Detailed(t) => {
            assert_eq!(t.model, "codex");
            assert_eq!(t.kind.as_deref(), Some("driven_cli"));
            assert_eq!(t.turn_budget, Some(25));
        }
        other => panic!("tier1 should be a table, got {other:?}"),
    }
    assert_eq!(personal.containment_min.tier2, Some(2));
    assert_eq!(personal.search.backend.as_deref(), Some("searxng"));

    let work = cfg.profiles.get("work").expect("work profile");
    assert_eq!(work.advisor.context.as_deref(), Some("1m"));
    // work tier1 is also a table.
    match work.roles.tier1.as_ref().unwrap() {
        RoleModel::Detailed(t) => assert_eq!(t.model, "claude-sonnet-4-6"),
        other => panic!("work tier1 should be a table, got {other:?}"),
    }
    assert_eq!(work.containment_min.tier2, Some(1));
    // search unset on work.
    assert!(work.search.backend.is_none());
}

/// Helper: fresh journal + an advisor + a task, returning ids.
fn seed_task(j: &Journal) -> (String, String) {
    let advisor = j
        .create_advisor("personal", "claude-fable-5", "standard")
        .unwrap();
    let spec = serde_json::json!({
        "title": "test task",
        "tier": 1,
        "base_ref": "main",
        "file_allowlist": ["src/**"],
        "instructions": "do the thing",
        "acceptance_criteria": [
            { "id": "AC1", "check": "cargo test", "kind": "command" }
        ],
        "check_commands": ["cargo test"],
        "budget": { "turns": 25, "tokens": null },
        "lifetime_budget": { "tokens": null, "wall_clock_minutes": null },
        "containment_min": 1
    })
    .to_string();
    let task = j
        .create_task(
            &advisor,
            Tier::T1,
            "codex",
            ContainmentLevel::L1,
            &spec,
            "main",
            Some("/tmp/wt"),
            Some("/tmp/repo"),
            None,
        )
        .unwrap();
    (advisor, task)
}

// AC5: created -> spawned -> verify_passed yields derived state verify_passed,
// with seq 0,1,2 monotonic.
#[test]
fn ac5_event_sourcing_derived_state() {
    let j = Journal::open_in_memory().unwrap();
    let (_advisor, task) = seed_task(&j);

    let (_e0, s0) = j.append_event(&task, EventKind::Created, None).unwrap();
    let (_e1, s1) = j.append_event(&task, EventKind::Spawned, None).unwrap();
    let (_e2, s2) = j
        .append_event(&task, EventKind::VerifyPassed, None)
        .unwrap();

    assert_eq!((s0, s1, s2), (0, 1, 2));

    let state = j.current_state(&task).unwrap();
    assert_eq!(state, Some(EventKind::VerifyPassed));

    // list_tasks read-model reflects the derived state too.
    let rows = j.list_tasks().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].task_id, task);
    assert_eq!(rows[0].title, "test task");
    assert_eq!(rows[0].tier, Tier::T1);
    assert_eq!(rows[0].model, "codex");
    assert_eq!(rows[0].containment, ContainmentLevel::L1);
    assert_eq!(rows[0].state, "verify_passed");
}

// repo_path persists on the task row and is read back by both `get_task` and
// the focused `task_repo_and_base` accessor used by the advisor merge path.
#[test]
fn repo_path_and_base_ref_round_trip() {
    let j = Journal::open_in_memory().unwrap();
    let (_advisor, task) = seed_task(&j);

    let full = j.get_task(&task).unwrap();
    assert_eq!(full.repo_path.as_deref(), Some("/tmp/repo"));
    assert_eq!(full.base_ref, "main");

    let (repo_path, base_ref) = j.task_repo_and_base(&task).unwrap();
    assert_eq!(repo_path.as_deref(), Some("/tmp/repo"));
    assert_eq!(base_ref, "main");

    // A missing task is a NotFound error, not a silent None.
    assert!(j.task_repo_and_base("nonexistent").is_err());
}

// AC6: UNIQUE(task_id, seq) is enforced; event_chain returns seq order.
#[test]
fn ac6_unique_seq_and_chain_order() {
    let j = Journal::open_in_memory().unwrap();
    let (_advisor, task) = seed_task(&j);

    j.append_event(&task, EventKind::Created, None).unwrap();
    j.append_event(&task, EventKind::Spawned, None).unwrap();
    j.append_event(&task, EventKind::Iterating, None).unwrap();

    // Direct duplicate-seq insert must violate the UNIQUE constraint.
    let dup = j.connection().execute(
        "INSERT INTO events (event_id, task_id, ts, seq, kind, payload)
         VALUES ('DUPULID', ?1, '2026-01-01T00:00:00Z', 0, 'blocked', NULL)",
        [&task],
    );
    assert!(dup.is_err(), "duplicate (task_id, seq) must error");

    let chain = j.event_chain(&task).unwrap();
    let seqs: Vec<i64> = chain.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
    let kinds: Vec<EventKind> = chain.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![EventKind::Created, EventKind::Spawned, EventKind::Iterating]
    );
}

// AC7: a VerifierReport body round-trips through JSON per the ADR-002 schema.
#[test]
fn ac7_verifier_report_roundtrip() {
    let body = ReportBody {
        verdict: Verdict::Fail,
        findings: vec![
            Finding {
                severity: Severity::Blocker,
                criterion_id: Some("AC1".into()),
                evidence: "test tls::handshake FAILED".into(),
            },
            Finding {
                severity: Severity::Note,
                criterion_id: None,
                evidence: "minor style nit".into(),
            },
        ],
        out_of_scope_diff: false,
        commands_run: vec![CommandRun {
            cmd: "cargo test".into(),
            exit: 101,
            output_digest: "sha256:abc".into(),
        }],
    };

    let json = serde_json::to_string(&body).unwrap();
    // Shape check against the frozen schema.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["verdict"], "fail");
    assert_eq!(v["findings"][0]["severity"], "blocker");
    assert_eq!(v["findings"][0]["criterion_id"], "AC1");
    assert!(v["findings"][1]["criterion_id"].is_null());
    assert_eq!(v["out_of_scope_diff"], false);
    assert_eq!(v["commands_run"][0]["exit"], 101);

    let back: ReportBody = serde_json::from_str(&json).unwrap();
    assert_eq!(back, body);

    // And it stores/reads back through the journal unchanged.
    let j = Journal::open_in_memory().unwrap();
    let (advisor, task) = seed_task(&j);
    let session = j
        .insert_session(
            Some(&task),
            Some(&advisor),
            Role::Verifier,
            "claude-sonnet-4-6",
            SessionKind::OneShotApi,
            None,
        )
        .unwrap();
    let report_id = j
        .insert_verifier_report(&task, &session, 1, Independence::CrossProvider, &json)
        .unwrap();
    let stored: String = j
        .connection()
        .query_row(
            "SELECT report FROM verifier_reports WHERE report_id = ?1",
            [&report_id],
            |r| r.get(0),
        )
        .unwrap();
    let stored_body: ReportBody = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored_body, body);
}

// AC (ADR-008): finish_session records the metering outcome onto the session
// row: ended_at set, exit_status/turns/tokens_in/tokens_out written.
#[test]
fn finish_session_records_outcome() {
    let j = Journal::open_in_memory().unwrap();
    let (advisor, task) = seed_task(&j);
    let session = j
        .insert_session(
            Some(&task),
            Some(&advisor),
            Role::Implementer,
            "mock",
            SessionKind::OneShotApi,
            Some("/tmp/wt"),
        )
        .unwrap();

    // Before finishing: outcome columns are NULL.
    let before = j.get_session(&session).unwrap();
    assert!(before.ended_at.is_none());
    assert!(before.exit_status.is_none());
    assert!(before.turns.is_none());

    j.finish_session(&session, ExitStatus::Ok, Some(3), Some(100), Some(20))
        .unwrap();

    let s = j.get_session(&session).unwrap();
    assert!(s.ended_at.is_some(), "ended_at must be set");
    assert_eq!(s.exit_status, Some(ExitStatus::Ok));
    assert_eq!(s.turns, Some(3));
    assert_eq!(s.tokens_in, Some(100));
    assert_eq!(s.tokens_out, Some(20));

    // And the raw column stores the enum's string form.
    let exit_raw: String = j
        .connection()
        .query_row(
            "SELECT exit_status FROM sessions WHERE session_id = ?1",
            [&session],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exit_raw, "ok");
}

// AC7 (ADR-005): shim_cache_get / shim_cache_put round-trip. A put then get
// returns the stored payload and retrieved_at; INSERT OR REPLACE overwrites an
// existing (url, schema_hash) key; a missing key returns None.
#[test]
fn shim_cache_roundtrip() {
    let j = Journal::open_in_memory().unwrap();

    // Missing key → None.
    assert!(j
        .shim_cache_get("https://example.com", "sha256:abc")
        .unwrap()
        .is_none());

    // Put then get returns the payload + retrieved_at.
    j.shim_cache_put(
        "https://example.com",
        "sha256:abc",
        "2026-07-07T00:00:00Z",
        r#"{"k":"v1"}"#,
    )
    .unwrap();
    let (ts, payload) = j
        .shim_cache_get("https://example.com", "sha256:abc")
        .unwrap()
        .expect("cache hit after put");
    assert_eq!(ts, "2026-07-07T00:00:00Z");
    assert_eq!(payload, r#"{"k":"v1"}"#);

    // INSERT OR REPLACE overwrites the same key in place.
    j.shim_cache_put(
        "https://example.com",
        "sha256:abc",
        "2026-07-07T01:00:00Z",
        r#"{"k":"v2"}"#,
    )
    .unwrap();
    let (ts2, payload2) = j
        .shim_cache_get("https://example.com", "sha256:abc")
        .unwrap()
        .unwrap();
    assert_eq!(ts2, "2026-07-07T01:00:00Z");
    assert_eq!(payload2, r#"{"k":"v2"}"#);

    // A different schema_hash is a distinct key.
    assert!(j
        .shim_cache_get("https://example.com", "sha256:other")
        .unwrap()
        .is_none());
}

// ADR-007: advisor_context getter returns the stored context; None for unknown advisor.
#[test]
fn advisor_context_getter_returns_stored_value_or_none() {
    let j = Journal::open_in_memory().unwrap();
    let adv_standard = j.create_advisor("personal", "mock", "standard").unwrap();
    let adv_1m = j.create_advisor("work", "mock", "1m").unwrap();

    assert_eq!(
        j.advisor_context(&adv_standard).unwrap().as_deref(),
        Some("standard")
    );
    assert_eq!(
        j.advisor_context(&adv_1m).unwrap().as_deref(),
        Some("1m")
    );
    // Unknown advisor → None (not an error).
    assert_eq!(j.advisor_context("nonexistent-id").unwrap(), None);
}

// ADR-007: advisor_inbox_since with inline_detail=true → detail is Some(payload);
//          with inline_detail=false → detail is None.
#[test]
fn advisor_inbox_since_inline_detail_mode() {
    let j = Journal::open_in_memory().unwrap();
    let adv = j.create_advisor("work", "mock", "1m").unwrap();
    let spec = serde_json::json!({
        "title": "inline test",
        "tier": 0,
        "base_ref": "main",
        "instructions": "do it",
        "acceptance_criteria": [{ "id": "AC1", "check": "true", "kind": "command" }],
    })
    .to_string();
    let task = j
        .create_task(
            &adv,
            Tier::T0,
            "mock",
            ContainmentLevel::L0,
            &spec,
            "main",
            None,
            None,
            None,
        )
        .unwrap();

    // Append an event WITH a JSON payload (a `failed` event with a kind key).
    let payload = serde_json::json!({ "kind": "verification_failed", "outcome": "abandoned" })
        .to_string();
    j.append_event(&task, EventKind::Failed, Some(&payload)).unwrap();

    // Append an event WITHOUT a payload.
    j.append_event(&task, EventKind::Created, None).unwrap();

    // inline_detail = true → failed item carries detail, created item has None.
    let items_inlined = j.advisor_inbox_since(&adv, None, true).unwrap();
    assert_eq!(items_inlined.len(), 2);

    let failed_item = &items_inlined[0];
    assert_eq!(failed_item.kind, "failed");
    assert_eq!(
        failed_item.detail.as_deref(),
        Some(payload.as_str()),
        "inlined mode must carry the raw payload"
    );

    let created_item = &items_inlined[1];
    assert_eq!(created_item.kind, "created");
    assert!(
        created_item.detail.is_none(),
        "event with no payload has detail=None even in inlined mode"
    );

    // inline_detail = false → both items have detail=None.
    let items_standard = j.advisor_inbox_since(&adv, None, false).unwrap();
    assert_eq!(items_standard.len(), 2);
    for item in &items_standard {
        assert!(
            item.detail.is_none(),
            "standard mode: detail must be None for every item (kind={})",
            item.kind
        );
    }
}

// ADR-007: the 8000-char truncation is char-boundary-safe for multi-byte content.
#[test]
fn advisor_inbox_since_truncates_large_payload_at_char_boundary() {
    let j = Journal::open_in_memory().unwrap();
    let adv = j.create_advisor("work", "mock", "1m").unwrap();
    let spec = serde_json::json!({
        "title": "truncate test", "tier": 0, "base_ref": "main",
        "instructions": "x",
        "acceptance_criteria": [{ "id": "AC1", "check": "true", "kind": "command" }],
    })
    .to_string();
    let task = j
        .create_task(
            &adv,
            Tier::T0,
            "mock",
            ContainmentLevel::L0,
            &spec,
            "main",
            None,
            None,
            None,
        )
        .unwrap();

    // Build a payload that exceeds 8000 bytes: a JSON string with 9000 'a's.
    // Each 'a' is 1 byte, so len() == 9000 > 8000 and the truncation triggers.
    let big_payload = format!("\"{}\"", "a".repeat(9000));
    assert!(big_payload.len() > 8000);

    j.append_event(&task, EventKind::Iterating, Some(&big_payload)).unwrap();

    let items = j.advisor_inbox_since(&adv, None, true).unwrap();
    assert_eq!(items.len(), 1);
    let detail = items[0].detail.as_deref().expect("detail must be Some");
    assert!(
        detail.len() <= 8000,
        "truncated detail must not exceed 8000 bytes (len={})",
        detail.len()
    );
    // Must be valid UTF-8 (no partial multi-byte splits).
    assert!(std::str::from_utf8(detail.as_bytes()).is_ok(), "must be valid UTF-8");
}
