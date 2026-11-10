//! The SQLite connection wrapper: opening, pragmas, migrations, transactions and backup.
//! Spec: `docs/design/crates/vowp-store.md` §3.2–3.3 and §4.4.
//!
//! Every repository method is an `impl Store` method taking `&self`. A `Store` is `Send` but
//! not `Sync`: each thread opens its own connection, and WAL plus the busy timeout make short
//! concurrent writers safe.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};
use vowp_protocol::{
    Clock, Cost, JobId, JobKind, JobState, LimitKind, ProjectName, Role, RunOutcome, SessionState, SystemClock,
    Timestamp, Tokens,
};

use crate::convert::ts_to_text;
use crate::error::StoreError;
use crate::paths::NodePaths;

/// Schema version this build writes and accepts.
pub const SCHEMA_VERSION: i64 = 1;

/// Embedded migrations, indexed by the version they produce minus one.
const MIGRATIONS: &[&str] = &[include_str!("schema/001_init.sql")];

/// Filter for [`Store::list_jobs`].
#[derive(Debug, Clone, Default)]
pub struct JobFilter {
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Restrict to these lifecycle states; empty means every state.
    pub states: Vec<JobState>,
    /// Restrict to one job kind.
    pub kind: Option<JobKind>,
    /// Restrict to one role.
    pub role: Option<Role>,
    /// Restrict to ids beginning with this string.
    pub id_prefix: Option<String>,
    /// Maximum number of rows.
    pub limit: Option<u32>,
}

/// Extra fields written by [`Store::set_job_state`].
#[derive(Debug, Clone)]
pub struct StateChange {
    /// Replaces the job note when set.
    pub note: Option<String>,
    /// Parked reason; written only when the new state is `parked`, cleared otherwise.
    pub parked_reason: Option<vowp_protocol::ParkedReason>,
}

/// The terminal fields written by [`Store::finish_run`].
#[derive(Debug, Clone)]
pub struct RunFinish {
    /// Classified outcome of the run.
    pub outcome: RunOutcome,
    /// Process exit code, when the process produced one.
    pub exit_code: Option<i32>,
    /// Limit class that ended the run, when one did.
    pub limit_kind: Option<LimitKind>,
    /// Cost and token usage reported for the run.
    pub cost: Cost,
    /// Harness-side session id, when the harness reported one.
    pub native_session_id: Option<String>,
    /// Redacted detail; only the last 1500 characters are kept.
    pub detail: String,
}

/// Dimension used by [`Store::cost_totals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostGroup {
    /// Group by job id.
    Job,
    /// Group by run role.
    Role,
    /// Group by provider key.
    Provider,
    /// Group by model name.
    Model,
    /// Group by UTC day (`YYYY-MM-DD`).
    Day,
}

/// Filter for [`Store::cost_totals`].
#[derive(Debug, Clone, Default)]
pub struct CostQuery {
    /// Include rows at or after this instant.
    pub since: Option<Timestamp>,
    /// Include rows at or before this instant.
    pub until: Option<Timestamp>,
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Restrict to one job.
    pub job_id: Option<JobId>,
}

/// One row of [`Store::cost_totals`].
#[derive(Debug, Clone, PartialEq)]
pub struct CostRow {
    /// Job id, role, provider, model or `YYYY-MM-DD` (UTC) depending on the grouping.
    pub key: String,
    /// Sum of known USD amounts.
    pub usd: f64,
    /// Cost entries whose source was a harness receipt.
    pub receipt_entries: u64,
    /// Cost entries whose source was the configured price table.
    pub price_table_entries: u64,
    /// Cost entries whose USD amount is unknown (never counted as free).
    pub unknown_entries: u64,
    /// Token usage across the grouped rows.
    pub tokens: Tokens,
    /// Receipt, price-table and unknown entries combined.
    pub entries: u64,
}

/// Id of a CLI-to-supervisor request row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(pub String);

/// One drained CLI-to-supervisor request.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Request id.
    pub id: RequestId,
    /// Snake-case request kind, for example `retry_job`.
    pub kind: String,
    /// Opaque request payload.
    pub payload: serde_json::Value,
    /// When the request row was written.
    pub created_at: Timestamp,
}

/// Filter for [`Store::ledger_since`].
#[derive(Debug, Clone, Default)]
pub struct LedgerFilter {
    /// Restrict to one ledger kind.
    pub kind: Option<vowp_protocol::LedgerKind>,
    /// Restrict to one job.
    pub job_id: Option<JobId>,
    /// Include rows at or after this instant.
    pub since: Option<Timestamp>,
    /// Maximum number of rows.
    pub limit: Option<u32>,
}

/// Filter for [`Store::list_sessions`].
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Restrict to one job.
    pub job_id: Option<JobId>,
    /// Restrict to one session state.
    pub state: Option<SessionState>,
    /// Maximum number of rows.
    pub limit: Option<u32>,
}

/// One open SQLite database and the clock used to stamp rows.
pub struct Store {
    pub(crate) conn: Connection,
    clock: Arc<dyn Clock>,
    savepoints: AtomicU64,
}

impl Store {
    /// Opens (or creates) the node database at `paths.db_path()`.
    ///
    /// Creates the node directories, applies the connection pragmas and runs pending
    /// migrations. Uses [`SystemClock`].
    pub fn open(paths: &NodePaths) -> Result<Store, StoreError> {
        paths.ensure()?;
        Self::open_path(&paths.db_path())
    }

    /// Opens (or creates) a database at `path`; the parent directory must already exist.
    pub fn open_path(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.is_dir()
        {
            return Err(StoreError::io(
                parent.display(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "parent directory does not exist"),
            ));
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn, path.display().to_string(), true)
    }

    /// Opens an in-memory database for tests.
    pub fn open_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, ":memory:".to_owned(), false)
    }

    /// Replaces the clock used for new timestamps.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Store {
        self.clock = clock;
        self
    }

    /// Returns the clock used to stamp rows.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// Returns the database's `PRAGMA user_version`.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Runs `f` inside `BEGIN IMMEDIATE ... COMMIT`, rolling back on error.
    ///
    /// When called from inside an open transaction the closure runs inside a savepoint, so a
    /// repository method that opens its own transaction is safe to call from another one.
    pub fn transaction<T>(&self, f: impl FnOnce(&Store) -> Result<T, StoreError>) -> Result<T, StoreError> {
        if self.conn.is_autocommit() {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            match f(self) {
                Ok(value) => {
                    tx.commit()?;
                    Ok(value)
                }
                Err(err) => {
                    let _ = tx.rollback();
                    Err(err)
                }
            }
        } else {
            let name = format!("vowp_sp_{}", self.savepoints.fetch_add(1, Ordering::Relaxed));
            self.conn.execute_batch(&format!("SAVEPOINT {name}"))?;
            match f(self) {
                Ok(value) => {
                    self.conn.execute_batch(&format!("RELEASE {name}"))?;
                    Ok(value)
                }
                Err(err) => {
                    let _ = self.conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
                    Err(err)
                }
            }
        }
    }

    /// Writes an online SQLite backup to `to`, atomically replacing it via a temporary file.
    ///
    /// The parent directory of `to` must exist.
    pub fn backup(&self, to: &Path) -> Result<(), StoreError> {
        let parent = match to.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if !parent.is_dir() {
            return Err(StoreError::io(
                parent.display(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "backup parent directory does not exist"),
            ));
        }
        let file_name = to
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "vowp.db".to_owned());
        let tmp = parent.join(format!(".{file_name}.tmp-{}", ulid::Ulid::generate()));
        let result = (|| {
            self.conn.backup(rusqlite::MAIN_DB, &tmp, None)?;
            std::fs::rename(&tmp, to).map_err(|err| StoreError::io(to.display(), err))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Runs `PRAGMA integrity_check` and `PRAGMA foreign_key_check`.
    pub fn integrity_check(&self) -> Result<(), StoreError> {
        let mut statement = self.conn.prepare("PRAGMA integrity_check")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut problems = Vec::new();
        for row in rows {
            let line = row?;
            if !line.eq_ignore_ascii_case("ok") {
                problems.push(line);
            }
        }
        if !problems.is_empty() {
            return Err(StoreError::Corrupt {
                path: "integrity_check".to_owned(),
                line: None,
                reason: problems.join("; "),
            });
        }
        let mut statement = self.conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = statement.query([])?;
        if rows.next()?.is_some() {
            return Err(StoreError::Corrupt {
                path: "foreign_key_check".to_owned(),
                line: None,
                reason: "foreign key violations".to_owned(),
            });
        }
        Ok(())
    }

    /// Builds a store from an already-open connection, configuring and migrating it.
    fn from_connection(conn: Connection, path: String, wal: bool) -> Result<Store, StoreError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Check the version before enabling WAL: opening a future database must not touch it.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        if wal {
            let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::Conflict(format!(
                    "could not enable WAL: journal_mode is {mode}"
                )));
            }
        }

        let clock = SystemClock;
        migrate(&conn, &path, &clock)?;
        Ok(Store {
            conn,
            clock: Arc::new(clock),
            savepoints: AtomicU64::new(0),
        })
    }
}

/// Applies every migration newer than `PRAGMA user_version` and seeds fresh database state.
fn migrate(conn: &Connection, path: &str, clock: &dyn Clock) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if version == 0 && tables > 0 {
        return Err(StoreError::Corrupt {
            path: path.to_owned(),
            line: None,
            reason: "unversioned database".to_owned(),
        });
    }

    let fresh = version == 0;
    for migration in MIGRATIONS.iter().skip(version as usize) {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        tx.execute_batch(migration)?;
        tx.commit()?;
    }

    if fresh {
        let now = ts_to_text(clock.now());
        conn.execute(
            "INSERT OR IGNORE INTO kv (key, value, updated_at) VALUES ('schema_created_at', ?1, ?1)",
            [&now],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO kv (key, value, updated_at) VALUES ('node_id', ?1, ?2)",
            rusqlite::params![ulid::Ulid::generate().to_string(), now],
        )?;
    }
    Ok(())
}
