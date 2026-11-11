//! The SQLite connection wrapper: opening, pragmas, migrations, transactions and backup.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.1–§3.10.
//!
//! A [`Store`] handle is bound to the global database (`main`) plus at most one project
//! database attached under the fixed schema name `project`. Every repository method is an
//! `impl Store` method taking `&self`. A `Store` is `Send` but not `Sync`: each thread opens
//! its own connection, and WAL plus the busy timeout make short concurrent writers safe.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};
use vowp_protocol::{
    Clock, Cost, JobId, JobKind, JobState, LimitKind, ProjectName, Role, RunOutcome, SessionState, SystemClock,
    Timestamp, Tokens,
};

use crate::convert::ts_to_text;
use crate::error::StoreError;
use crate::paths::NodePaths;

/// Schema version of the global database written by this build.
pub const GLOBAL_SCHEMA_VERSION: i64 = 2;

/// Schema version of every project database written by this build.
pub const PROJECT_SCHEMA_VERSION: i64 = 1;

/// Compatibility alias for the v0.1 schema version.
///
/// New code that means the global file uses [`GLOBAL_SCHEMA_VERSION`]; this constant keeps its
/// historical value 1 so external users of the v0.1 API are not surprised.
pub const SCHEMA_VERSION: i64 = PROJECT_SCHEMA_VERSION;

/// Embedded global schema (user_version 2).
pub(crate) const GLOBAL_SCHEMA_SQL: &str = include_str!("schema/global.sql");

/// Embedded project schema (user_version 1).
pub(crate) const PROJECT_SCHEMA: &str = include_str!("schema/project.sql");

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
    /// Restrict to one project; `None` means the bound project.
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

/// One open SQLite database pair and the clock used to stamp rows.
pub struct Store {
    pub(crate) conn: Connection,
    pub(crate) clock: Arc<dyn Clock>,
    savepoints: AtomicU64,
    /// Node root for a file-backed handle; `None` for an in-memory test store.
    pub(crate) paths: Option<NodePaths>,
    /// The bound project, if any. `&'static` lets [`Store::bound_project`] return the keyed
    /// signature from the spec; rebinding leaks one tiny name object per handle at most.
    bound: Mutex<Option<&'static ProjectName>>,
    /// Whether `upsert_project` may auto-bind this handle. `open`/`open_memory` allow it;
    /// `open_global`, `open_with_project` and `open_read_only` never auto-bind.
    pub(crate) auto_bind: bool,
}

impl Store {
    /// Opens (or creates) the node database at `paths.db_path()`.
    ///
    /// The global schema is created or checked. If exactly one non-reserved project is
    /// registered, its database is attached (and created when absent). With zero or several
    /// projects the handle is fleet-only and project methods return [`StoreError::NoProject`].
    pub fn open(paths: &NodePaths) -> Result<Store, StoreError> {
        paths.ensure()?;
        let store = Self::open_global_unbound(paths, true)?;
        let projects = store.list_projects()?;
        let real: Vec<&vowp_protocol::Project> = projects
            .iter()
            .filter(|project| project.name.as_str() != crate::RESERVED_NODE_PROJECT)
            .collect();
        if let [only] = real.as_slice() {
            Self::create_project_db(paths, &only.name)?;
            store.attach_file(paths, &only.name)?;
        }
        Ok(store)
    }

    /// Opens the global database only; project-scoped methods return `NoProject`.
    pub fn open_global(paths: &NodePaths) -> Result<Store, StoreError> {
        paths.ensure()?;
        Self::open_global_unbound(paths, false)
    }

    /// Opens the global database and attaches exactly `project`.
    ///
    /// The project must be registered and its database must already exist.
    pub fn open_with_project(paths: &NodePaths, project: &ProjectName) -> Result<Store, StoreError> {
        paths.ensure()?;
        let store = Self::open_global_unbound(paths, true)?;
        if !store.project_row_exists(project)? {
            return Err(StoreError::not_found("project", project.as_str()));
        }
        store.attach_file(paths, project)?;
        Ok(store)
    }

    /// Creates `projects/<name>/project.db` with the project schema.
    ///
    /// Idempotent: an existing database with the right version is left untouched.
    pub fn create_project_db(paths: &NodePaths, project: &ProjectName) -> Result<(), StoreError> {
        let dir = paths.project_dir(project);
        let path = paths.project_db_path(project);
        if path.exists() {
            let conn = Connection::open(&path)?;
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if version == PROJECT_SCHEMA_VERSION && tables > 0 {
                return Ok(());
            }
            if version == 0 && tables == 0 {
                configure_connection(&conn, Path::new(&path), true)?;
                conn.execute_batch(PROJECT_SCHEMA)?;
                return Ok(());
            }
            if version > PROJECT_SCHEMA_VERSION {
                return Err(StoreError::SchemaTooNew {
                    found: version,
                    supported: PROJECT_SCHEMA_VERSION,
                });
            }
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("project database has schema version {version}, expected 1"),
            });
        }

        std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
        let conn = Connection::open(&path)?;
        configure_connection(&conn, Path::new(&path), true)?;
        conn.execute_batch(PROJECT_SCHEMA)?;
        Ok(())
    }

    /// Read-only variant for fleet snapshots: never migrates and never writes.
    pub fn open_read_only(paths: &NodePaths, project: Option<&ProjectName>) -> Result<Store, StoreError> {
        let path = paths.db_path();
        if !path.is_file() {
            return Err(StoreError::not_found("database", path.display().to_string()));
        }
        let conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "query_only", "ON")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > GLOBAL_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: GLOBAL_SCHEMA_VERSION,
            });
        }
        if version != GLOBAL_SCHEMA_VERSION {
            return Err(StoreError::MigrationRequired);
        }
        let store = Store {
            conn,
            clock: Arc::new(SystemClock),
            savepoints: AtomicU64::new(0),
            paths: Some(paths.clone()),
            bound: Mutex::new(None),
            auto_bind: false,
        };
        if let Some(name) = project {
            if !store.project_row_exists(name)? {
                return Err(StoreError::not_found("project", name.as_str()));
            }
            store.attach_read_only(paths, name)?;
        }
        Ok(store)
    }

    /// Opens an in-memory global database plus one lazily attached in-memory project.
    pub fn open_memory() -> Result<Store, StoreError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(GLOBAL_SCHEMA_SQL)?;
        seed_fresh(&conn, &SystemClock)?;
        Ok(Store {
            conn,
            clock: Arc::new(SystemClock),
            savepoints: AtomicU64::new(0),
            paths: None,
            bound: Mutex::new(None),
            auto_bind: true,
        })
    }

    /// Opens (or creates) a database at `path`; the parent directory must already exist.
    ///
    /// This is the global-file constructor used by backup and test helpers.
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
        Self::from_global_connection(conn, path.display().to_string(), true, None, false)
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

    /// Compatibility accessor: 1 for every handle, matching v0.1's "schema 1".
    ///
    /// New code that means the global file uses [`Store::global_schema_version`].
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(PROJECT_SCHEMA_VERSION)
    }

    /// Returns the global file's real `PRAGMA user_version` (2).
    pub fn global_schema_version(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("PRAGMA main.user_version", [], |row| row.get(0))?)
    }

    /// Returns the bound project's real `PRAGMA user_version` (1).
    pub fn project_schema_version(&self) -> Result<i64, StoreError> {
        self.require_bound()?;
        Ok(self
            .conn
            .query_row("PRAGMA project.user_version", [], |row| row.get(0))?)
    }

    /// Returns the bound project, when this handle has one.
    pub fn bound_project(&self) -> Option<&ProjectName> {
        *self.bound.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns the node paths of a file-backed handle.
    pub fn paths(&self) -> Option<&NodePaths> {
        self.paths.as_ref()
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

    /// Writes an online SQLite backup of the bound project database to `to`.
    ///
    /// [`Store::backup`] backs up `main`; a project database lives in the attached
    /// `project` schema. This uses SQLite's online backup API on that schema, so a
    /// concurrent writer or WAL checkpoint cannot tear the copy. The destination is
    /// replaced atomically through a temporary file in the same directory.
    pub fn backup_project(&self, to: &Path) -> Result<(), StoreError> {
        if self.bound_project().is_none() {
            return Err(StoreError::NoProject);
        }
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
            .unwrap_or_else(|| "project.db".to_owned());
        let tmp = parent.join(format!(".{file_name}.tmp-{}", ulid::Ulid::generate()));
        let result = (|| {
            self.conn.backup("project", &tmp, None)?;
            std::fs::rename(&tmp, to).map_err(|err| StoreError::io(to.display(), err))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Runs `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on every attached file.
    pub fn integrity_check(&self) -> Result<(), StoreError> {
        for schema in self.attached_schemas() {
            let prefix = if schema == "main" {
                String::new()
            } else {
                format!("{schema}.")
            };
            let mut statement = self.conn.prepare(&format!("PRAGMA {prefix}integrity_check"))?;
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
                    path: format!("{schema}:integrity_check"),
                    line: None,
                    reason: problems.join("; "),
                });
            }
            let mut statement = self.conn.prepare(&format!("PRAGMA {prefix}foreign_key_check"))?;
            let mut rows = statement.query([])?;
            if rows.next()?.is_some() {
                return Err(StoreError::Corrupt {
                    path: format!("{schema}:foreign_key_check"),
                    line: None,
                    reason: "foreign key violations".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Returns the number of indexed `session_events` rows in the global database.
    ///
    /// This is a real `COUNT(*)`, not the sum of [`vowp_protocol::Session::last_seq`]:
    /// a sequence gap still leaves fewer rows than `last_seq` suggests, and the backup
    /// manifest must record the rows a restore will actually see.
    pub fn count_session_events(&self) -> Result<u64, StoreError> {
        let value: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM main.session_events", [], |row| row.get(0))?;
        crate::convert::u64_from_i64(value, "session_events count")
    }

    /// Returns the number of `runs` rows in the bound project database.
    ///
    /// Counts every run row, including orphan runs whose job was deleted and whose
    /// `job_id` is therefore `NULL`; summing [`Store::runs_of_job`] would miss them.
    pub fn count_runs(&self) -> Result<u64, StoreError> {
        self.require_bound()?;
        let value: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM project.runs", [], |row| row.get(0))?;
        crate::convert::u64_from_i64(value, "runs count")
    }

    /// Returns the schema names attached to this connection, `main` first.
    fn attached_schemas(&self) -> Vec<String> {
        let mut schemas = vec!["main".to_owned()];
        if self.bound_project().is_some() {
            schemas.push("project".to_owned());
        }
        schemas
    }

    /// Opens the global database without binding a project.
    fn open_global_unbound(paths: &NodePaths, auto_bind: bool) -> Result<Store, StoreError> {
        crate::migrate::resume_migration(paths)?;
        let path = paths.db_path();
        let conn = Connection::open(&path)?;
        let store =
            Self::from_global_connection(conn, path.display().to_string(), true, Some(paths.clone()), auto_bind)?;
        Ok(store)
    }

    /// Builds a global store from an already-open connection, migrating or refusing it.
    fn from_global_connection(
        conn: Connection,
        path: String,
        wal: bool,
        paths: Option<NodePaths>,
        auto_bind: bool,
    ) -> Result<Store, StoreError> {
        // Check the version before enabling WAL: opening a future or v0.1 database must not
        // touch it.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        check_global_version(version, Path::new(&path))?;
        configure_connection(&conn, Path::new(&path), wal)?;
        if version == 0 {
            let tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if tables > 0 {
                return Err(StoreError::Corrupt {
                    path,
                    line: None,
                    reason: "unversioned database".to_owned(),
                });
            }
            conn.execute_batch(GLOBAL_SCHEMA_SQL)?;
            let clock = SystemClock;
            seed_fresh(&conn, &clock)?;
        }
        Ok(Store {
            conn,
            clock: Arc::new(SystemClock),
            savepoints: AtomicU64::new(0),
            paths,
            bound: Mutex::new(None),
            auto_bind,
        })
    }

    /// Attaches a file-backed project database under the `project` schema.
    pub(crate) fn attach_file(&self, paths: &NodePaths, name: &ProjectName) -> Result<(), StoreError> {
        if self.bound_project() == Some(name) {
            return Ok(());
        }
        self.detach_project()?;
        let path = paths.project_db_path(name);
        if !path.is_file() {
            return Err(StoreError::not_found("project database", path.display().to_string()));
        }
        self.conn
            .execute("ATTACH DATABASE ?1 AS project", [path.to_string_lossy().as_ref()])?;
        let version: i64 = self
            .conn
            .query_row("PRAGMA project.user_version", [], |row| row.get(0))?;
        if version != PROJECT_SCHEMA_VERSION {
            let _ = self.conn.execute_batch("DETACH DATABASE project");
            if version > PROJECT_SCHEMA_VERSION {
                return Err(StoreError::SchemaTooNew {
                    found: version,
                    supported: PROJECT_SCHEMA_VERSION,
                });
            }
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("project database has schema version {version}, expected 1"),
            });
        }
        self.set_bound(Some(name.clone()));
        Ok(())
    }

    /// Attaches a read-only project database under the `project` schema.
    fn attach_read_only(&self, paths: &NodePaths, name: &ProjectName) -> Result<(), StoreError> {
        self.detach_project()?;
        let path = paths.project_db_path(name);
        if !path.is_file() {
            return Err(StoreError::not_found("project database", path.display().to_string()));
        }
        self.conn
            .execute("ATTACH DATABASE ?1 AS project", [path.to_string_lossy().as_ref()])?;
        let version: i64 = self
            .conn
            .query_row("PRAGMA project.user_version", [], |row| row.get(0))?;
        if version != PROJECT_SCHEMA_VERSION {
            let _ = self.conn.execute_batch("DETACH DATABASE project");
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("project database has schema version {version}, expected 1"),
            });
        }
        self.set_bound(Some(name.clone()));
        Ok(())
    }

    /// Attaches a fresh in-memory project database under the `project` schema.
    pub(crate) fn attach_memory(&self, name: &ProjectName) -> Result<(), StoreError> {
        self.detach_project()?;
        self.conn.execute_batch("ATTACH DATABASE ':memory:' AS project")?;
        execute_schema_in_schema(&self.conn, "project", PROJECT_SCHEMA)?;
        self.set_bound(Some(name.clone()));
        Ok(())
    }

    /// Detaches the project database, if one is attached.
    fn detach_project(&self) -> Result<(), StoreError> {
        if self.bound_project().is_some() {
            self.conn.execute_batch("DETACH DATABASE project")?;
            self.set_bound(None);
        }
        Ok(())
    }

    /// Replaces the bound project name.
    fn set_bound(&self, name: Option<ProjectName>) {
        let leaked: Option<&'static ProjectName> = name.map(|name| Box::leak(Box::new(name)) as &'static ProjectName);
        *self.bound.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = leaked;
    }

    /// Returns the bound project or [`StoreError::NoProject`].
    pub(crate) fn require_bound(&self) -> Result<ProjectName, StoreError> {
        self.bound_project().cloned().ok_or(StoreError::NoProject)
    }

    /// Checks that `project` names the bound project.
    pub(crate) fn check_bound(&self, project: &ProjectName) -> Result<ProjectName, StoreError> {
        let bound = self.require_bound()?;
        if &bound != project {
            return Err(StoreError::WrongProject {
                bound,
                found: project.clone(),
            });
        }
        Ok(bound)
    }
}

/// Applies connection pragmas shared by every open path.
fn configure_connection(conn: &Connection, path: &Path, wal: bool) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if wal {
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Conflict(format!(
                "could not enable WAL: journal_mode is {mode}"
            )));
        }
    }
    let _ = path;
    Ok(())
}

/// Refuses a global file this build cannot open.
fn check_global_version(version: i64, _path: &Path) -> Result<(), StoreError> {
    if version == 0 || version == GLOBAL_SCHEMA_VERSION {
        return Ok(());
    }
    if version > GLOBAL_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: GLOBAL_SCHEMA_VERSION,
        });
    }
    // Version 1 is the v0.1 single-file schema; the migration must split it first.
    Err(StoreError::MigrationRequired)
}

/// Seeds the two rows every fresh home carries.
fn seed_fresh(conn: &Connection, clock: &dyn Clock) -> Result<(), StoreError> {
    let now = ts_to_text(clock.now());
    conn.execute(
        "INSERT OR IGNORE INTO main.kv (key, value, updated_at) VALUES ('schema_created_at', ?1, ?1)",
        [&now],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO main.kv (key, value, updated_at) VALUES ('node_id', ?1, ?2)",
        rusqlite::params![ulid::Ulid::generate().to_string(), now],
    )?;
    Ok(())
}

/// Executes a schema file into a named attached schema.
///
/// The project schema is written for `main`; `open_memory` attaches an in-memory project DB,
/// so this helper prefixes the DDL's object names with the schema name. It is only used for
/// the in-memory test path; file-backed project DBs execute the schema verbatim.
fn execute_schema_in_schema(conn: &Connection, schema: &str, sql: &str) -> Result<(), StoreError> {
    let mut transformed = String::with_capacity(sql.len() + 256);
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("PRAGMA user_version") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("CREATE TABLE ") {
            transformed.push_str(&format!(
                "{}CREATE TABLE {schema}.{}",
                &line[..line.len() - trimmed.len()],
                rest
            ));
        } else if let Some(rest) = trimmed.strip_prefix("CREATE UNIQUE INDEX ") {
            transformed.push_str(&format!(
                "{}CREATE UNIQUE INDEX {schema}.{rest}",
                &line[..line.len() - trimmed.len()]
            ));
        } else if let Some(rest) = trimmed.strip_prefix("CREATE INDEX ") {
            transformed.push_str(&format!(
                "{}CREATE INDEX {schema}.{rest}",
                &line[..line.len() - trimmed.len()]
            ));
        } else if let Some(rest) = trimmed.strip_prefix("CREATE TRIGGER ") {
            transformed.push_str(&format!(
                "{}CREATE TRIGGER {schema}.{}",
                &line[..line.len() - trimmed.len()],
                rest
            ));
        } else {
            transformed.push_str(line);
        }
        transformed.push('\n');
    }
    conn.execute_batch(&transformed)?;
    conn.pragma_update(Some(schema), "user_version", PROJECT_SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_versions_are_global_2_project_1() {
        assert_eq!(GLOBAL_SCHEMA_VERSION, 2);
        assert_eq!(PROJECT_SCHEMA_VERSION, 1);
        assert_eq!(SCHEMA_VERSION, 1);
        assert!(GLOBAL_SCHEMA_SQL.contains("PRAGMA user_version = 2;"));
        assert!(PROJECT_SCHEMA.contains("PRAGMA user_version = 1;"));
    }

    #[test]
    fn project_schema_transform_prefixes_ddl() {
        let conn = Connection::open_in_memory().expect("memory");
        conn.execute_batch("ATTACH DATABASE ':memory:' AS project")
            .expect("attach");
        execute_schema_in_schema(&conn, "project", PROJECT_SCHEMA).expect("schema");
        let version: i64 = conn
            .query_row("PRAGMA project.user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, PROJECT_SCHEMA_VERSION);
        let jobs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project.sqlite_master WHERE type = 'table' AND name = 'jobs'",
                [],
                |row| row.get(0),
            )
            .expect("jobs table");
        assert_eq!(jobs, 1);
    }
}
