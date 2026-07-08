//! Schema DDL (ADR-001, with the seq/advisor_events/interrupted-reason
//! amendments) and a `PRAGMA user_version` migration runner.

use rusqlite::Connection;

use crate::error::Result;

/// The current schema version. Applying migrations to a DB already at this
/// version is a no-op.
pub const SCHEMA_VERSION: u32 = 1;

/// The full v1 DDL (ADR-001). Every statement is idempotent-safe under the
/// migration runner because it only runs when `user_version < SCHEMA_VERSION`.
const V1_DDL: &str = r#"
CREATE TABLE advisors (
  advisor_session_id TEXT PRIMARY KEY,
  profile            TEXT NOT NULL,
  advisor_model      TEXT NOT NULL,
  advisor_context    TEXT NOT NULL,
  started_at         TEXT NOT NULL
);

CREATE TABLE tasks (
  task_id            TEXT PRIMARY KEY,
  advisor_session_id TEXT NOT NULL REFERENCES advisors(advisor_session_id),
  parent_task        TEXT REFERENCES tasks(task_id),
  depends_on         TEXT,
  tier               INTEGER NOT NULL,
  model              TEXT NOT NULL,
  containment_level  INTEGER NOT NULL,
  spec               TEXT NOT NULL,
  workspace          TEXT,
  repo_path          TEXT,
  base_ref           TEXT NOT NULL,
  branch             TEXT NOT NULL,
  created_at         TEXT NOT NULL
);

CREATE TABLE events (
  event_id  TEXT PRIMARY KEY,
  task_id   TEXT NOT NULL REFERENCES tasks(task_id),
  ts        TEXT NOT NULL,
  seq       INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  payload   TEXT,
  UNIQUE (task_id, seq)
);
CREATE INDEX events_task ON events(task_id, seq);

CREATE TABLE advisor_events (
  event_id           TEXT PRIMARY KEY,
  advisor_session_id TEXT NOT NULL REFERENCES advisors(advisor_session_id),
  ts                 TEXT NOT NULL,
  seq                INTEGER NOT NULL,
  kind               TEXT NOT NULL,
  payload            TEXT,
  UNIQUE (advisor_session_id, seq)
);
CREATE INDEX advisor_events_session ON advisor_events(advisor_session_id, seq);

CREATE TABLE sessions (
  session_id  TEXT PRIMARY KEY,
  task_id     TEXT REFERENCES tasks(task_id),
  advisor_session_id TEXT REFERENCES advisors(advisor_session_id),
  role        TEXT NOT NULL,
  model       TEXT NOT NULL,
  kind        TEXT NOT NULL,
  workspace   TEXT,
  started_at  TEXT NOT NULL,
  ended_at    TEXT,
  exit_status TEXT,
  turns       INTEGER,
  tokens_in   INTEGER,
  tokens_out  INTEGER,
  log_path    TEXT
);

CREATE TABLE verifier_reports (
  report_id  TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(task_id),
  session_id TEXT NOT NULL REFERENCES sessions(session_id),
  attempt    INTEGER NOT NULL,
  independence TEXT NOT NULL,
  report     TEXT NOT NULL
);

CREATE TABLE shim_cache (
  url          TEXT NOT NULL,
  schema_hash  TEXT NOT NULL,
  retrieved_at TEXT NOT NULL,
  payload      TEXT NOT NULL,
  PRIMARY KEY (url, schema_hash)
);
"#;

/// Read the SQLite `user_version` pragma.
fn user_version(conn: &Connection) -> Result<u32> {
    let v: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v)
}

/// Set the SQLite `user_version` pragma. `PRAGMA user_version = ?` does not
/// accept a bound parameter, so the version is formatted inline; it is always a
/// crate-internal `u32` constant, never user input.
fn set_user_version(conn: &Connection, v: u32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {v};"))?;
    Ok(())
}

/// Apply all pending migrations. Tracks the applied version via
/// `PRAGMA user_version`; applying to an already-migrated DB is a no-op.
pub fn migrate(conn: &Connection) -> Result<()> {
    let current = user_version(conn)?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    // Each migration step is guarded by the version it upgrades *from*.
    if current < 1 {
        conn.execute_batch(V1_DDL)?;
        set_user_version(conn, 1)?;
    }
    Ok(())
}
