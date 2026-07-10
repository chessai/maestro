//! maestro-journal — schema, migrations, typed queries, and shared domain /
//! config / protocol types (ADR-001). No daemon policy lives here: this crate
//! is a synchronous library with no scheduler, socket server, or async.
//!
//! Layout:
//! - [`domain`]  — enums + row structs stored in / derived from the schema.
//! - [`spec`]    — the immutable `TaskSpec` (ADR-003).
//! - [`report`]  — the frozen verifier report body (ADR-002).
//! - [`config`]  — config / profile parse types (ADR-007).
//! - [`proto`]   — daemon Unix-socket message types (ADR-006).
//! - [`schema`]  — DDL + `PRAGMA user_version` migration runner.
//! - [`Journal`] — a `rusqlite::Connection` wrapper with typed writers/readers.

pub mod config;
pub mod domain;
pub mod error;
pub mod paths;
pub mod progress;
pub mod proto;
pub mod report;
pub mod schema;
pub mod spec;

pub use error::{Error, Result};

use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::{Generator, Ulid};

use crate::domain::{
    Advisor, AdvisorEvent, AdvisorEventKind, ContainmentLevel, Event, EventKind, Independence,
    Role, Session, SessionKind, Task, Tier,
};
use crate::proto::PsRow;

/// An ISO-8601 (RFC 3339) UTC timestamp for the current instant.
pub fn now_iso8601() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("Rfc3339 formatting is infallible for a valid OffsetDateTime")
}

/// Process-wide monotonic ULID generator. Guarantees strictly increasing ULIDs
/// even for ids minted in the same millisecond, so `event_id` ordering matches
/// causal (insertion) order — the inbox cursor and event-chain reads depend on
/// this (ADR-001, the ordering guarantee). Serialized across threads by a mutex.
fn ulid_generator() -> &'static Mutex<Generator> {
    static G: OnceLock<Mutex<Generator>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Generator::new()))
}

/// Mint a fresh, monotonically-increasing ULID as a string identifier.
pub fn new_ulid() -> String {
    let mut g = ulid_generator().lock().expect("ulid generator poisoned");
    match g.generate() {
        Ok(u) => u.to_string(),
        // Monotonic overflow within a single millisecond (2^80 ids) is effectively
        // impossible; fall back to a plain ULID rather than panic.
        Err(_) => Ulid::new().to_string(),
    }
}

/// A handle to the journal database: a thin, synchronous wrapper over a
/// `rusqlite::Connection` exposing typed writers and readers.
pub struct Journal {
    conn: Connection,
}

impl Journal {
    /// Open the journal at `path`, applying the standard PRAGMAs
    /// (`journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
    /// `busy_timeout=5000`) and running migrations.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open an in-memory journal (tests, ephemeral use).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    /// Apply PRAGMAs + migrations to an already-open connection.
    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL is a no-op / harmless for `:memory:`; the others always apply.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        schema::migrate(&conn)?;
        Ok(Journal { conn })
    }

    /// Borrow the underlying connection (read-only escape hatch for callers
    /// that need a query this crate does not yet expose).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // ---- writers -------------------------------------------------------

    /// Insert an advisor row, minting a ULID and timestamp. Returns the id.
    pub fn create_advisor(
        &self,
        profile: &str,
        advisor_model: &str,
        advisor_context: &str,
    ) -> Result<String> {
        let id = new_ulid();
        self.conn.execute(
            "INSERT INTO advisors (advisor_session_id, profile, advisor_model, advisor_context, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, profile, advisor_model, advisor_context, now_iso8601()],
        )?;
        Ok(id)
    }

    /// Insert a task row, minting a ULID and `created_at`. `spec` is the
    /// serialized JSON TaskSpec (see [`spec::TaskSpec`]). Returns the id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        advisor_session_id: &str,
        tier: Tier,
        model: &str,
        containment_level: ContainmentLevel,
        spec_json: &str,
        base_ref: &str,
        workspace: Option<&str>,
        repo_path: Option<&str>,
        parent_task: Option<&str>,
    ) -> Result<String> {
        let id = new_ulid();
        let branch = format!("maestro/{id}");
        self.conn.execute(
            "INSERT INTO tasks
               (task_id, advisor_session_id, parent_task, depends_on, tier, model,
                containment_level, spec, workspace, repo_path, base_ref, branch, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                advisor_session_id,
                parent_task,
                tier.as_int(),
                model,
                containment_level.as_int(),
                spec_json,
                workspace,
                repo_path,
                base_ref,
                branch,
                now_iso8601(),
            ],
        )?;
        Ok(id)
    }

    /// Append a task event, assigning the next per-task monotonic `seq`
    /// (`max(seq)+1`, starting at 0). `payload` is optional kind-specific JSON.
    /// Returns the minted `event_id` and the assigned `seq`.
    pub fn append_event(
        &self,
        task_id: &str,
        kind: EventKind,
        payload: Option<&str>,
    ) -> Result<(String, i64)> {
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM events WHERE task_id = ?1",
            [task_id],
            |r| r.get(0),
        )?;
        let event_id = new_ulid();
        self.conn.execute(
            "INSERT INTO events (event_id, task_id, ts, seq, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![event_id, task_id, now_iso8601(), next, kind.as_str(), payload],
        )?;
        Ok((event_id, next))
    }

    /// Append an advisor-scoped event, assigning the next per-advisor `seq`.
    /// Returns the minted `event_id` and assigned `seq`.
    pub fn append_advisor_event(
        &self,
        advisor_session_id: &str,
        kind: AdvisorEventKind,
        payload: Option<&str>,
    ) -> Result<(String, i64)> {
        let next: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM advisor_events WHERE advisor_session_id = ?1",
            [advisor_session_id],
            |r| r.get(0),
        )?;
        let event_id = new_ulid();
        self.conn.execute(
            "INSERT INTO advisor_events (event_id, advisor_session_id, ts, seq, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                event_id,
                advisor_session_id,
                now_iso8601(),
                next,
                kind.as_str(),
                payload
            ],
        )?;
        Ok((event_id, next))
    }

    /// Insert a session row, minting a ULID and `started_at`. Returns the id.
    pub fn insert_session(
        &self,
        task_id: Option<&str>,
        advisor_session_id: Option<&str>,
        role: Role,
        model: &str,
        kind: SessionKind,
        workspace: Option<&str>,
    ) -> Result<String> {
        let id = new_ulid();
        self.conn.execute(
            "INSERT INTO sessions
               (session_id, task_id, advisor_session_id, role, model, kind, workspace, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                task_id,
                advisor_session_id,
                role.as_str(),
                model,
                kind.as_str(),
                workspace,
                now_iso8601(),
            ],
        )?;
        Ok(id)
    }

    /// Finish a session: record its outcome. Sets `ended_at = now`,
    /// `exit_status`, and the optional metering columns (`turns`, `tokens_in`,
    /// `tokens_out`). Sessions are mutable operational rows (not event-sourced
    /// task rows), so an in-place UPDATE is the correct write here.
    pub fn finish_session(
        &self,
        session_id: &str,
        exit_status: crate::domain::ExitStatus,
        turns: Option<i64>,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions
                SET ended_at = ?2, exit_status = ?3, turns = ?4,
                    tokens_in = ?5, tokens_out = ?6
              WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                now_iso8601(),
                exit_status.as_str(),
                turns,
                tokens_in,
                tokens_out,
            ],
        )?;
        Ok(())
    }

    /// Set (or update) a session's captured-log path (ADR-006 `sessions.log_path`).
    /// Used by driven (PTY) sessions to record where their PTY output was
    /// captured, so `maestro logs` can find it.
    pub fn set_session_log_path(&self, session_id: &str, log_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET log_path = ?2 WHERE session_id = ?1",
            rusqlite::params![session_id, log_path],
        )?;
        Ok(())
    }

    /// The sessions for a task, most-recent first. Used by `maestro logs` to
    /// locate the latest driven session's captured PTY log.
    pub fn sessions_for_task(&self, task_id: &str) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, task_id, advisor_session_id, role, model, kind, workspace,
                    started_at, ended_at, exit_status, turns, tokens_in, tokens_out, log_path
               FROM sessions
              WHERE task_id = ?1
              ORDER BY started_at DESC, session_id DESC",
        )?;
        let rows = stmt.query_map([task_id], Self::map_session_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Insert a verifier report row, minting a ULID. `report_json` is the
    /// serialized [`report::ReportBody`] (ADR-002). Returns the `report_id`.
    pub fn insert_verifier_report(
        &self,
        task_id: &str,
        session_id: &str,
        attempt: i64,
        independence: Independence,
        report_json: &str,
    ) -> Result<String> {
        let id = new_ulid();
        self.conn.execute(
            "INSERT INTO verifier_reports
               (report_id, task_id, session_id, attempt, independence, report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                task_id,
                session_id,
                attempt,
                independence.as_str(),
                report_json
            ],
        )?;
        Ok(id)
    }

    // ---- shim cache (ADR-005) ------------------------------------------

    /// Read a `shim_cache` entry by `(url, schema_hash)`. Returns
    /// `(retrieved_at, payload_json)` if present. TTL enforcement (24h) is
    /// daemon policy and applied by the caller against `retrieved_at`.
    pub fn shim_cache_get(
        &self,
        url: &str,
        schema_hash: &str,
    ) -> Result<Option<(String, String)>> {
        let row = self
            .conn
            .query_row(
                "SELECT retrieved_at, payload FROM shim_cache
                 WHERE url = ?1 AND schema_hash = ?2",
                rusqlite::params![url, schema_hash],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert or replace a `shim_cache` entry keyed by `(url, schema_hash)`.
    /// `payload` is the serialized JSON extraction result; `retrieved_at` is the
    /// fetch timestamp the TTL is measured from.
    pub fn shim_cache_put(
        &self,
        url: &str,
        schema_hash: &str,
        retrieved_at: &str,
        payload: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO shim_cache (url, schema_hash, retrieved_at, payload)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![url, schema_hash, retrieved_at, payload],
        )?;
        Ok(())
    }

    // ---- readers -------------------------------------------------------

    /// The verifier-report chain for a task (ADR-002), ordered by `attempt` ASC
    /// so the report sequence reads in escalation order.
    pub fn verifier_reports_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<crate::domain::VerifierReport>> {
        let mut stmt = self.conn.prepare(
            "SELECT report_id, task_id, session_id, attempt, independence, report
             FROM verifier_reports WHERE task_id = ?1 ORDER BY attempt ASC",
        )?;
        let rows = stmt.query_map([task_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (report_id, task_id, session_id, attempt, independence_s, report) = row?;
            let independence = match independence_s.as_str() {
                "cross_provider" => Independence::CrossProvider,
                "cross_model" => Independence::CrossModel,
                "fresh_context_only" => Independence::FreshContextOnly,
                o => return Err(Error::InvalidData(format!("unknown independence {o}"))),
            };
            out.push(crate::domain::VerifierReport {
                report_id,
                task_id,
                session_id,
                attempt,
                independence,
                report,
            });
        }
        Ok(out)
    }

    // ---- M6 budgets & telemetry (ADR-003 / ADR-001) --------------------

    /// SUM of `tokens_in` / `tokens_out` over every session row for a task
    /// (implementer + verifier), returning `(in, out)`. Null metering columns
    /// COALESCE to 0. Backs the lifetime-token ceiling (ADR-003).
    pub fn task_token_totals(&self, task_id: &str) -> Result<(i64, i64)> {
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
               FROM sessions WHERE task_id = ?1",
            [task_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(row)
    }

    /// The `ts` of the earliest `spawned` event for a task, if any. Used to
    /// anchor the lifetime wall-clock ceiling (ADR-003).
    pub fn first_spawn_ts(&self, task_id: &str) -> Result<Option<String>> {
        let ts: Option<String> = self
            .conn
            .query_row(
                "SELECT ts FROM events
                  WHERE task_id = ?1 AND kind = 'spawned'
                  ORDER BY seq ASC LIMIT 1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ts)
    }

    /// SUM of `tokens_in` / `tokens_out` plus session COUNT over all sessions
    /// whose `started_at` begins with `day_prefix` (e.g. `"2026-07-08"`).
    /// Returns `(in, out, session_count)`. Backs the daily inbox total.
    pub fn day_token_totals(&self, day_prefix: &str) -> Result<(i64, i64, i64)> {
        let like = format!("{day_prefix}%");
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0), COUNT(*)
               FROM sessions WHERE started_at LIKE ?1",
            [like],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )?;
        Ok(row)
    }

    /// Routing telemetry (ADR-001): aggregate the task history grouped by
    /// `(tier, model, containment_level)`. Each row carries the group's
    /// `total_tasks`, a `terminal_counts` breakdown keyed by each task's LAST
    /// event kind (with `failed` broken down by its payload `kind` so failure
    /// kinds surface), and the summed `tokens_in` / `tokens_out` from that
    /// group's tasks' sessions. Returns a JSON array of these rows.
    ///
    /// Implemented by fetching the per-task facts and aggregating in Rust: the
    /// terminal-kind breakdown needs to read each `failed` event's JSON payload,
    /// which is awkward in pure SQL. Correctness over cleverness.
    pub fn routing_report(&self) -> Result<serde_json::Value> {
        use std::collections::BTreeMap;

        // Per-group accumulator.
        #[derive(Default)]
        struct Group {
            total_tasks: i64,
            terminal_counts: BTreeMap<String, i64>,
            tokens_in: i64,
            tokens_out: i64,
        }

        // One row per task: its group key, its latest event (kind + payload),
        // and its session token totals.
        let mut stmt = self.conn.prepare(
            "SELECT t.tier, t.model, t.containment_level,
                    (SELECT e.kind FROM events e
                       WHERE e.task_id = t.task_id
                       ORDER BY e.seq DESC LIMIT 1)    AS last_kind,
                    (SELECT e.payload FROM events e
                       WHERE e.task_id = t.task_id
                       ORDER BY e.seq DESC LIMIT 1)    AS last_payload,
                    COALESCE((SELECT SUM(s.tokens_in) FROM sessions s
                                WHERE s.task_id = t.task_id), 0) AS tin,
                    COALESCE((SELECT SUM(s.tokens_out) FROM sessions s
                                WHERE s.task_id = t.task_id), 0) AS tout
               FROM tasks t",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, u8>(0)?,             // tier
                r.get::<_, String>(1)?,          // model
                r.get::<_, u8>(2)?,             // containment_level
                r.get::<_, Option<String>>(3)?, // last kind
                r.get::<_, Option<String>>(4)?, // last payload
                r.get::<_, i64>(5)?,             // tokens_in
                r.get::<_, i64>(6)?,             // tokens_out
            ))
        })?;

        // Group key: (tier, model, containment_level).
        let mut groups: BTreeMap<(u8, String, u8), Group> = BTreeMap::new();
        for row in rows {
            let (tier, model, cont, last_kind, last_payload, tin, tout) = row?;
            let g = groups.entry((tier, model, cont)).or_default();
            g.total_tasks += 1;
            g.tokens_in += tin;
            g.tokens_out += tout;
            // The terminal bucket: the last event kind, but for `failed` we key
            // on the failure `kind` in its payload so the report shows failure
            // kinds (scope_violation / verification_failed / budget_exhausted).
            let bucket = match last_kind.as_deref() {
                None => "created".to_string(),
                Some("failed") => last_payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(String::from))
                    .map(|k| format!("failed:{k}"))
                    .unwrap_or_else(|| "failed".to_string()),
                Some(other) => other.to_string(),
            };
            *g.terminal_counts.entry(bucket).or_insert(0) += 1;
        }

        let arr: Vec<serde_json::Value> = groups
            .into_iter()
            .map(|((tier, model, cont), g)| {
                let counts: serde_json::Map<String, serde_json::Value> = g
                    .terminal_counts
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::from(v)))
                    .collect();
                serde_json::json!({
                    "tier": tier,
                    "model": model,
                    "containment_level": cont,
                    "total_tasks": g.total_tasks,
                    "terminal_counts": counts,
                    "tokens_in": g.tokens_in,
                    "tokens_out": g.tokens_out,
                })
            })
            .collect();
        Ok(serde_json::Value::Array(arr))
    }

    /// The full event chain for a task, ordered by `seq` (ADR-001 ordering).
    pub fn event_chain(&self, task_id: &str) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, task_id, ts, seq, kind, payload
             FROM events WHERE task_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([task_id], |r| {
            let kind_str: String = r.get(4)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                kind_str,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (event_id, task_id, ts, seq, kind_str, payload) = row?;
            let kind = EventKind::from_str_kind(&kind_str)
                .ok_or_else(|| Error::InvalidData(format!("unknown event kind {kind_str}")))?;
            out.push(Event {
                event_id,
                task_id,
                ts,
                seq,
                kind,
                payload,
            });
        }
        Ok(out)
    }

    /// The derived current state of a task: the `kind` of the latest event by
    /// `seq`. `None` if the task has no events.
    pub fn current_state(&self, task_id: &str) -> Result<Option<EventKind>> {
        let kind_str: Option<String> = self
            .conn
            .query_row(
                "SELECT kind FROM events WHERE task_id = ?1 ORDER BY seq DESC LIMIT 1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?;
        match kind_str {
            None => Ok(None),
            Some(s) => EventKind::from_str_kind(&s)
                .map(Some)
                .ok_or_else(|| Error::InvalidData(format!("unknown event kind {s}"))),
        }
    }

    /// The `ps` read-model (ADR-006): one [`PsRow`] per task, carrying its
    /// derived current state. Ordered newest-first by `created_at`.
    pub fn list_tasks(&self) -> Result<Vec<PsRow>> {
        // Derived state via a correlated subquery on the latest seq.
        let mut stmt = self.conn.prepare(
            "SELECT t.task_id, t.tier, t.model, t.containment_level, t.spec, t.created_at,
                    (SELECT e.kind FROM events e
                       WHERE e.task_id = t.task_id
                       ORDER BY e.seq DESC LIMIT 1) AS state
             FROM tasks t
             ORDER BY t.created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,        // task_id
                r.get::<_, u8>(1)?,            // tier
                r.get::<_, String>(2)?,        // model
                r.get::<_, u8>(3)?,            // containment_level
                r.get::<_, String>(4)?,        // spec json
                r.get::<_, String>(5)?,        // created_at
                r.get::<_, Option<String>>(6)?, // state
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (task_id, tier_i, model, cont_i, spec_json, created_at, state) = row?;
            let tier = Tier::try_from(tier_i).map_err(Error::InvalidData)?;
            let containment = ContainmentLevel::try_from(cont_i).map_err(Error::InvalidData)?;
            // Title comes out of the immutable spec JSON.
            let title = serde_json::from_str::<serde_json::Value>(&spec_json)
                .ok()
                .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();
            out.push(PsRow {
                task_id,
                title,
                tier,
                model,
                containment,
                state: state.unwrap_or_else(|| "created".to_string()),
                created_at,
            });
        }
        Ok(out)
    }

    /// The `task_status` read-model for one advisor (ADR-006), optionally
    /// filtered to a single derived current state. Newest-first.
    pub fn list_tasks_for_advisor(
        &self,
        advisor_session_id: &str,
        state: Option<&str>,
    ) -> Result<Vec<PsRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.task_id, t.tier, t.model, t.containment_level, t.spec, t.created_at,
                    (SELECT e.kind FROM events e
                       WHERE e.task_id = t.task_id
                       ORDER BY e.seq DESC LIMIT 1) AS state
             FROM tasks t
             WHERE t.advisor_session_id = ?1
             ORDER BY t.created_at DESC",
        )?;
        let rows = stmt.query_map([advisor_session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u8>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, u8>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (task_id, tier_i, model, cont_i, spec_json, created_at, st) = row?;
            let derived = st.unwrap_or_else(|| "created".to_string());
            if let Some(want) = state {
                if derived != want {
                    continue;
                }
            }
            let tier = Tier::try_from(tier_i).map_err(Error::InvalidData)?;
            let containment = ContainmentLevel::try_from(cont_i).map_err(Error::InvalidData)?;
            let title = serde_json::from_str::<serde_json::Value>(&spec_json)
                .ok()
                .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();
            out.push(PsRow {
                task_id,
                title,
                tier,
                model,
                containment,
                state: derived,
                created_at,
            });
        }
        Ok(out)
    }

    /// Look up the `advisor_context` column for an advisor row. Returns `None`
    /// if the advisor does not exist (unknown id). This is used by the daemon to
    /// decide whether to inline event payloads in the inbox (ADR-007).
    pub fn advisor_context(&self, advisor_session_id: &str) -> Result<Option<String>> {
        let ctx: Option<String> = self
            .conn
            .query_row(
                "SELECT advisor_context FROM advisors WHERE advisor_session_id = ?1",
                [advisor_session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ctx)
    }

    /// Inbox events for an advisor since `after_event_id` (exclusive), across all
    /// of that advisor's tasks, ordered by `event_id` (ULID / time order). The
    /// daemon holds the per-advisor cursor in memory and advances it on drain.
    ///
    /// When `inline_detail` is `true` the full event payload is included in each
    /// item's `detail` field (truncated to 8000 chars, char-boundary safe). When
    /// `false`, `detail` is always `None`. This is the passive inbox only —
    /// `journal_query` (an explicit pull) is unchanged (ADR-007).
    pub fn advisor_inbox_since(
        &self,
        advisor_session_id: &str,
        after_event_id: Option<&str>,
        inline_detail: bool,
    ) -> Result<Vec<crate::proto::InboxItem>> {
        let cursor = after_event_id.unwrap_or("");
        let mut stmt = self.conn.prepare(
            "SELECT e.event_id, e.task_id, e.ts, e.kind, t.spec, e.payload
             FROM events e JOIN tasks t ON t.task_id = e.task_id
             WHERE t.advisor_session_id = ?1 AND e.event_id > ?2
             ORDER BY e.event_id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![advisor_session_id, cursor], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (event_id, task_id, ts, kind, spec_json, payload) = row?;
            let title = serde_json::from_str::<serde_json::Value>(&spec_json)
                .ok()
                .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_default();
            let summary = if title.is_empty() {
                kind.clone()
            } else {
                format!("{kind} — {title}")
            };
            // Inline the payload under "1m" context; truncate defensively to a
            // char boundary so we never split a multi-byte character.
            let detail = if inline_detail {
                payload.filter(|p| !p.is_empty()).map(|p| {
                    const CAP: usize = 8000;
                    if p.len() <= CAP {
                        p
                    } else {
                        // Walk back from the cap to a char boundary.
                        let mut end = CAP;
                        while !p.is_char_boundary(end) {
                            end -= 1;
                        }
                        p[..end].to_string()
                    }
                })
            } else {
                None
            };
            out.push(crate::proto::InboxItem {
                event_id,
                task_id,
                ts,
                kind,
                summary,
                detail,
            });
        }
        Ok(out)
    }

    /// Fetch a task row by id.
    pub fn get_task(&self, task_id: &str) -> Result<Task> {
        self.conn
            .query_row(
                "SELECT task_id, advisor_session_id, parent_task, depends_on, tier, model,
                        containment_level, spec, workspace, repo_path, base_ref, branch, created_at
                 FROM tasks WHERE task_id = ?1",
                [task_id],
                Self::map_task_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("task {task_id}")))?
    }

    /// The `(repo_path, base_ref)` for a task — the origin repository and the
    /// ref the branch was cut from — used by the advisor `merge_task` path to
    /// fast-forward `maestro/<task_id>` into its base (ADR-006). `repo_path` is
    /// `None` for rows delegated without a repo path. Errors with `NotFound` if
    /// the task does not exist.
    pub fn task_repo_and_base(&self, task_id: &str) -> Result<(Option<String>, String)> {
        self.conn
            .query_row(
                "SELECT repo_path, base_ref FROM tasks WHERE task_id = ?1",
                [task_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("task {task_id}")))
    }

    fn map_task_row(r: &rusqlite::Row) -> rusqlite::Result<Result<Task>> {
        let tier_i: u8 = r.get(4)?;
        let cont_i: u8 = r.get(6)?; // containment_level
        Ok((|| {
            Ok(Task {
                task_id: r.get(0)?,
                advisor_session_id: r.get(1)?,
                parent_task: r.get(2)?,
                depends_on: r.get(3)?,
                tier: Tier::try_from(tier_i).map_err(Error::InvalidData)?,
                model: r.get(5)?,
                containment_level: ContainmentLevel::try_from(cont_i)
                    .map_err(Error::InvalidData)?,
                spec: r.get(7)?,
                workspace: r.get(8)?,
                repo_path: r.get(9)?,
                base_ref: r.get(10)?,
                branch: r.get(11)?,
                created_at: r.get(12)?,
            })
        })())
    }

    /// Fetch an advisor row by id.
    pub fn get_advisor(&self, advisor_session_id: &str) -> Result<Advisor> {
        self.conn
            .query_row(
                "SELECT advisor_session_id, profile, advisor_model, advisor_context, started_at
                 FROM advisors WHERE advisor_session_id = ?1",
                [advisor_session_id],
                |r| {
                    Ok(Advisor {
                        advisor_session_id: r.get(0)?,
                        profile: r.get(1)?,
                        advisor_model: r.get(2)?,
                        advisor_context: r.get(3)?,
                        started_at: r.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("advisor {advisor_session_id}")))
    }

    /// The full advisor-event chain for an advisor, ordered by `seq`.
    pub fn advisor_event_chain(&self, advisor_session_id: &str) -> Result<Vec<AdvisorEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, advisor_session_id, ts, seq, kind, payload
             FROM advisor_events WHERE advisor_session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([advisor_session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (event_id, advisor_session_id, ts, seq, kind_str, payload) = row?;
            let kind = match kind_str.as_str() {
                "advisor_write" => AdvisorEventKind::AdvisorWrite,
                other => {
                    return Err(Error::InvalidData(format!(
                        "unknown advisor event kind {other}"
                    )))
                }
            };
            out.push(AdvisorEvent {
                event_id,
                advisor_session_id,
                ts,
                seq,
                kind,
                payload,
            });
        }
        Ok(out)
    }

    /// Fetch a session row by id.
    pub fn get_session(&self, session_id: &str) -> Result<Session> {
        self.conn
            .query_row(
                "SELECT session_id, task_id, advisor_session_id, role, model, kind, workspace,
                        started_at, ended_at, exit_status, turns, tokens_in, tokens_out, log_path
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                Self::map_session_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?
    }

    fn map_session_row(r: &rusqlite::Row) -> rusqlite::Result<Result<Session>> {
        let role_s: String = r.get(3)?;
        let kind_s: String = r.get(5)?;
        let exit_s: Option<String> = r.get(9)?;
        Ok((|| {
            use crate::domain::ExitStatus;
            let role = match role_s.as_str() {
                "implementer" => Role::Implementer,
                "verifier" => Role::Verifier,
                "plan_check" => Role::PlanCheck,
                "shim" => Role::Shim,
                o => return Err(Error::InvalidData(format!("unknown role {o}"))),
            };
            let kind = match kind_s.as_str() {
                "driven_pty" => SessionKind::DrivenPty,
                "one_shot_api" => SessionKind::OneShotApi,
                o => return Err(Error::InvalidData(format!("unknown session kind {o}"))),
            };
            let exit_status = match exit_s.as_deref() {
                None => None,
                Some("ok") => Some(ExitStatus::Ok),
                Some("error") => Some(ExitStatus::Error),
                Some("killed") => Some(ExitStatus::Killed),
                Some("wedged") => Some(ExitStatus::Wedged),
                Some(o) => return Err(Error::InvalidData(format!("unknown exit status {o}"))),
            };
            Ok(Session {
                session_id: r.get(0)?,
                task_id: r.get(1)?,
                advisor_session_id: r.get(2)?,
                role,
                model: r.get(4)?,
                kind,
                workspace: r.get(6)?,
                started_at: r.get(7)?,
                ended_at: r.get(8)?,
                exit_status,
                turns: r.get(10)?,
                tokens_in: r.get(11)?,
                tokens_out: r.get(12)?,
                log_path: r.get(13)?,
            })
        })())
    }
}

#[cfg(test)]
mod m6_tests {
    use super::*;
    use crate::domain::{ContainmentLevel, EventKind, ExitStatus, Role, SessionKind, Tier};

    fn open() -> Journal {
        Journal::open_in_memory().expect("in-memory journal")
    }

    fn advisor(j: &Journal) -> String {
        j.create_advisor("test", "mock", "standard").unwrap()
    }

    fn task(j: &Journal, adv: &str, tier: Tier, model: &str) -> String {
        let spec = serde_json::json!({ "title": "t" }).to_string();
        j.create_task(adv, tier, model, ContainmentLevel::L0, &spec, "HEAD", None, None, None)
            .unwrap()
    }

    // AC6: day_token_totals sums tokens + counts sessions started today.
    #[test]
    fn day_token_totals_sums_and_counts() {
        let j = open();
        let adv = advisor(&j);
        let t = task(&j, &adv, Tier::T0, "mock");

        // Two sessions with known tokens, both `started_at` = now (today).
        let s1 = j
            .insert_session(Some(&t), None, Role::Implementer, "mock", SessionKind::OneShotApi, None)
            .unwrap();
        j.finish_session(&s1, ExitStatus::Ok, Some(1), Some(100), Some(20)).unwrap();
        let s2 = j
            .insert_session(Some(&t), None, Role::Verifier, "mock", SessionKind::OneShotApi, None)
            .unwrap();
        j.finish_session(&s2, ExitStatus::Ok, Some(1), Some(50), Some(5)).unwrap();

        let today = &now_iso8601()[..10];
        let (tin, tout, n) = j.day_token_totals(today).unwrap();
        assert_eq!(tin, 150);
        assert_eq!(tout, 25);
        assert_eq!(n, 2);

        // A day with no sessions → zeros.
        let (z_in, z_out, z_n) = j.day_token_totals("1999-01-01").unwrap();
        assert_eq!((z_in, z_out, z_n), (0, 0, 0));

        // task_token_totals mirrors the per-task sums.
        assert_eq!(j.task_token_totals(&t).unwrap(), (150, 25));
        // A task with no sessions → (0, 0).
        let empty = task(&j, &adv, Tier::T0, "mock");
        assert_eq!(j.task_token_totals(&empty).unwrap(), (0, 0));
    }

    // first_spawn_ts returns the earliest `spawned` event's ts, else None.
    #[test]
    fn first_spawn_ts_earliest_or_none() {
        let j = open();
        let adv = advisor(&j);
        let t = task(&j, &adv, Tier::T0, "mock");
        assert_eq!(j.first_spawn_ts(&t).unwrap(), None);
        j.append_event(&t, EventKind::Created, None).unwrap();
        j.append_event(&t, EventKind::Spawned, None).unwrap();
        j.append_event(&t, EventKind::Spawned, None).unwrap();
        let ts = j.first_spawn_ts(&t).unwrap().expect("a spawn ts");
        // The recorded ts is a well-formed RFC3339 string.
        assert!(time::OffsetDateTime::parse(&ts, &Rfc3339).is_ok(), "ts parses: {ts}");
    }

    // AC5: routing_report groups by (tier, model, containment_level), carries
    // total_tasks + token sums + a terminal_counts breakdown that keys `failed`
    // by its payload failure kind.
    #[test]
    fn routing_report_shape_and_grouping() {
        let j = open();
        let adv = advisor(&j);

        // Task A: tier0/mock, terminal verify_passed, 120 tokens.
        let a = task(&j, &adv, Tier::T0, "mock");
        let sa = j
            .insert_session(Some(&a), None, Role::Implementer, "mock", SessionKind::OneShotApi, None)
            .unwrap();
        j.finish_session(&sa, ExitStatus::Ok, Some(1), Some(100), Some(20)).unwrap();
        j.append_event(&a, EventKind::Spawned, None).unwrap();
        j.append_event(&a, EventKind::VerifyPassed, None).unwrap();

        // Task B: tier0/mock (same group as A), terminal failed(budget_exhausted).
        let b = task(&j, &adv, Tier::T0, "mock");
        let sb = j
            .insert_session(Some(&b), None, Role::Implementer, "mock", SessionKind::OneShotApi, None)
            .unwrap();
        j.finish_session(&sb, ExitStatus::Ok, Some(1), Some(200), Some(40)).unwrap();
        j.append_event(
            &b,
            EventKind::Failed,
            Some(&serde_json::json!({ "kind": "budget_exhausted" }).to_string()),
        )
        .unwrap();

        let value = j.routing_report().unwrap();
        let arr = value.as_array().expect("routing_report is an array");
        assert!(!arr.is_empty(), "non-empty report");

        // Find the tier0/mock/L0 group: 2 tasks, tokens 300 in / 60 out.
        let row = arr
            .iter()
            .find(|r| r["tier"] == 0 && r["model"] == "mock" && r["containment_level"] == 0)
            .expect("tier0 mock group present");
        assert_eq!(row["total_tasks"], 2);
        assert_eq!(row["tokens_in"], 300);
        assert_eq!(row["tokens_out"], 60);
        let counts = row["terminal_counts"].as_object().expect("counts object");
        assert_eq!(counts["verify_passed"], 1);
        assert_eq!(counts["failed:budget_exhausted"], 1);
    }
}
