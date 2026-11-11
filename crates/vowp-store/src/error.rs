//! The single error type returned by every store operation.
//! Spec: `docs/design/crates/vowp-store.md` §5.
//!
//! [`StoreError::kind`] maps the error onto the protocol's machine-readable
//! [`ErrorKind`] vocabulary so callers can render `--json` output and exit codes without
//! matching on storage internals.

use vowp_protocol::{ErrorKind, ProtocolError};

/// An error raised by `vowp-store`.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A row with the requested id does not exist.
    #[error("not found: {what} {id}")]
    NotFound {
        /// What kind of row was requested, for example `job`.
        what: &'static str,
        /// The id that was looked up.
        id: String,
    },
    /// The operation conflicts with the current state (duplicate id, busy slot, ...).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The caller supplied a value that violates a store rule.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The database was created by a newer VOWP than this binary.
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew {
        /// Version found in the database.
        found: i64,
        /// Highest version this binary knows.
        supported: i64,
    },
    /// The operation named a project other than the one this handle is bound to.
    #[error("wrong project: handle is bound to {bound:?}, operation names {found:?}")]
    WrongProject {
        /// Project this handle is attached to.
        bound: vowp_protocol::ProjectName,
        /// Project the operation named.
        found: vowp_protocol::ProjectName,
    },
    /// A project-scoped operation ran on a handle with no project attached.
    #[error("no project bound to this store handle")]
    NoProject,
    /// The database is still the v0.1 single-file schema and must be migrated.
    #[error("schema v0.1 database: run `vowp migrate` before opening it")]
    MigrationRequired,
    /// Stored data or a file could not be parsed or failed an integrity check.
    #[error("corrupt data in {path:?} (line {line:?}): {reason}")]
    Corrupt {
        /// File or logical location of the bad data.
        path: String,
        /// One-based line number, when the source is a text file.
        line: Option<u64>,
        /// What is wrong.
        reason: String,
    },
    /// A session log has a gap or duplicate sequence number.
    #[error("sequence gap in session {session}: expected {expected}, got {got}")]
    SeqGap {
        /// Session whose log is inconsistent.
        session: String,
        /// The next sequence number the log requires.
        expected: u64,
        /// The sequence number that was supplied.
        got: u64,
    },
    /// An event belongs to a different session than the log being appended to.
    #[error("event belongs to session {got}, log is {expected}")]
    SessionMismatch {
        /// Session of the open log.
        expected: String,
        /// Session carried by the event.
        got: String,
    },
    /// Another process holds the session log's writer lock.
    #[error("session log is locked by pid {pid}")]
    Locked {
        /// Process id recorded in the lock file.
        pid: u32,
    },
    /// An earlier write error left the session log unusable until it is reopened.
    #[error("session log is poisoned by an earlier write error; reopen it")]
    Poisoned,
    /// SQLite reported `SQLITE_BUSY` after the busy timeout expired.
    #[error("database busy")]
    Busy,
    /// Any other SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(rusqlite::Error),
    /// A filesystem operation failed.
    #[error("io at {path}: {source}")]
    Io {
        /// Path the operation was working on.
        path: String,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// A JSON value could not be serialised or deserialised.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A protocol type failed to parse or validate.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    /// Neither `VOWP_HOME` nor `HOME` is set, so the node root cannot be located.
    #[error("home directory not determinable (set VOWP_HOME)")]
    NoHome,
}

impl StoreError {
    /// Maps the error onto the protocol's stable [`ErrorKind`] class.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::Conflict(_)
            | Self::Locked { .. }
            | Self::SeqGap { .. }
            | Self::SessionMismatch { .. }
            | Self::Poisoned => ErrorKind::Conflict,
            Self::InvalidInput(_) | Self::WrongProject { .. } | Self::NoProject => ErrorKind::InvalidInput,
            Self::MigrationRequired => ErrorKind::Conflict,
            Self::SchemaTooNew { .. } | Self::Sqlite(_) | Self::Busy => ErrorKind::Db,
            Self::Corrupt { .. } | Self::Json(_) => ErrorKind::Parse,
            Self::Io { .. } => ErrorKind::Io,
            Self::Protocol(err) => err.kind(),
            Self::NoHome => ErrorKind::Config,
        }
    }

    /// Builds a [`StoreError::NotFound`] for `what` and `id`.
    pub(crate) fn not_found(what: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound { what, id: id.into() }
    }

    /// Builds a [`StoreError::Io`] with the path that failed.
    pub(crate) fn io(path: impl std::fmt::Display, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_string(),
            source,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        use rusqlite::ffi::{
            ErrorCode, SQLITE_CONSTRAINT_FOREIGNKEY, SQLITE_CONSTRAINT_PRIMARYKEY, SQLITE_CONSTRAINT_UNIQUE,
        };

        let rusqlite::Error::SqliteFailure(ffi_error, message) = &err else {
            return Self::Sqlite(err);
        };
        match ffi_error.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Self::Busy,
            ErrorCode::ConstraintViolation => {
                let detail = message.clone().unwrap_or_else(|| "constraint violation".to_owned());
                match ffi_error.extended_code {
                    SQLITE_CONSTRAINT_FOREIGNKEY => Self::InvalidInput(detail),
                    SQLITE_CONSTRAINT_UNIQUE | SQLITE_CONSTRAINT_PRIMARYKEY => Self::Conflict(detail),
                    // Includes RAISE(ABORT, ...) from the ledger append-only triggers.
                    _ => Self::Conflict(detail),
                }
            }
            _ => Self::Sqlite(err),
        }
    }
}
