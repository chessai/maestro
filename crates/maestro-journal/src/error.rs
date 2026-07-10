//! The crate-wide error type.

/// Errors surfaced by this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A SQLite / rusqlite error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A JSON (de)serialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A config-parse error.
    #[error("config error: {0}")]
    Config(String),
    /// A value in the DB did not match a known enum / shape.
    #[error("invalid data: {0}")]
    InvalidData(String),
    /// A referenced row was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// The on-disk journal's schema version is incompatible with this binary:
    /// either a pre-versioning legacy DB, or one written by a NEWER binary. The
    /// message guides the operator to reset the journal.
    #[error("journal schema incompatible: {0}")]
    SchemaVersion(String),
}

/// The crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
