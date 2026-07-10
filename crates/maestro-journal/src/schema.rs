//! Schema DDL (ADR-001, with the seq/advisor_events/interrupted-reason
//! amendments) and a `PRAGMA user_version` migration runner.

use rusqlite::Connection;

use crate::error::{Error, Result};

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

/// `true` if the DB already contains user tables (any row in `sqlite_master` of
/// type `table` that is not an internal `sqlite_*` table). Used to distinguish a
/// truly fresh DB (safe to apply the v1 DDL) from a PRE-VERSIONING legacy DB
/// (already has tables but `user_version` is still 0), which must NOT have the
/// v1 DDL run over it (that would raise a raw `table ... already exists`).
fn has_user_tables(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Guidance appended to every schema-incompatibility error: how an operator
/// recovers. The journal is an operational log, so a reset is acceptable.
const RESET_HINT: &str = "reset the journal (move or delete the journal.db under \
     the maestro data dir, e.g. $XDG_DATA_HOME/maestro/journal.db) and re-run; a \
     fresh journal is created at the current schema version";

/// Apply all pending migrations. Tracks the applied version via
/// `PRAGMA user_version`; applying to an already-migrated DB is a no-op.
///
/// Two incompatibilities fail LOUD with a guided [`Error::SchemaVersion`] instead
/// of a raw sqlite error (operating-lesson L2):
/// - **newer DB**: `user_version > SCHEMA_VERSION` — the DB was written by a newer
///   binary; this (older) binary must not touch it.
/// - **pre-versioning legacy DB**: `user_version == 0` yet user tables already
///   exist — an older binary created the schema before the `user_version`
///   mechanism, so its shape may differ (e.g. a missing `repo_path` column).
///   Running the v1 DDL over it would raise a raw `table ... already exists`; we
///   surface a clear versioned error with a rebuild hint instead.
pub fn migrate(conn: &Connection) -> Result<()> {
    let current = user_version(conn)?;

    // A DB from a NEWER binary — never silently downgrade or write it.
    if current > SCHEMA_VERSION {
        return Err(Error::SchemaVersion(format!(
            "journal user_version {current} is newer than this binary supports \
             (max {SCHEMA_VERSION}); upgrade maestro, or {RESET_HINT}"
        )));
    }

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    // current == 0 here. Distinguish a fresh DB (apply v1 DDL) from a
    // pre-versioning legacy DB (tables exist but version was never stamped).
    if current == 0 && has_user_tables(conn)? {
        return Err(Error::SchemaVersion(format!(
            "journal has tables but no schema version (a pre-versioning legacy DB); \
             its shape may be incompatible with this binary (schema v{SCHEMA_VERSION}). \
             To fix: {RESET_HINT}"
        )));
    }

    // Each migration step is guarded by the version it upgrades *from*.
    if current < 1 {
        conn.execute_batch(V1_DDL)?;
        set_user_version(conn, 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh in-memory DB migrates to the current version, and re-running is a
    /// no-op (idempotent).
    #[test]
    fn migrate_fresh_db_stamps_version_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        assert_eq!(user_version(&conn).unwrap(), 0, "fresh DB starts at 0");
        migrate(&conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        // The core tables exist after migration.
        assert!(has_user_tables(&conn).unwrap());
        // Re-running is a no-op and does not error (no `table already exists`).
        migrate(&conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    /// L2 regression: a PRE-VERSIONING legacy DB — one that already has tables but
    /// whose `user_version` was never stamped (still 0), and whose shape differs
    /// from v1 (here `tasks` is missing the `repo_path` column) — must fail with a
    /// clear, guided `SchemaVersion` error, NOT a raw sqlite error and NOT a later
    /// `no column named repo_path` at insert time.
    #[test]
    fn migrate_legacy_unversioned_db_fails_loud_with_guidance() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate an old binary's schema: a `tasks` table WITHOUT `repo_path`,
        // and user_version left at 0 (the pre-versioning state).
        conn.execute_batch(
            "CREATE TABLE tasks (task_id TEXT PRIMARY KEY, base_ref TEXT NOT NULL);",
        )
        .unwrap();
        assert_eq!(user_version(&conn).unwrap(), 0);

        let err = migrate(&conn).expect_err("legacy DB must be rejected");
        match err {
            Error::SchemaVersion(msg) => {
                assert!(
                    msg.contains("legacy") || msg.contains("no schema version"),
                    "message names the legacy cause: {msg}"
                );
                assert!(msg.contains("reset the journal"), "message guides a rebuild: {msg}");
            }
            other => panic!("expected a guided SchemaVersion error, got {other:?}"),
        }
        // Crucially NOT the raw sqlite "table already exists" — the DDL never ran.
    }

    /// A DB written by a NEWER binary (`user_version > SCHEMA_VERSION`) is rejected
    /// with a clear "newer than this binary" error rather than being silently
    /// touched.
    #[test]
    fn migrate_newer_db_is_rejected() {
        let conn = Connection::open_in_memory().unwrap();
        set_user_version(&conn, SCHEMA_VERSION + 5).unwrap();
        let err = migrate(&conn).expect_err("a newer DB must be rejected");
        match err {
            Error::SchemaVersion(msg) => {
                assert!(msg.contains("newer"), "message names the newer-binary cause: {msg}");
                assert!(msg.contains("reset the journal"), "message guides recovery: {msg}");
            }
            other => panic!("expected SchemaVersion, got {other:?}"),
        }
    }
}
