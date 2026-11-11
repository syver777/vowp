//! Catalog repository: the lifecycle rows used by `vowp gc` and `vowp backup`.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10 and §13.3. The `catalog` table is the
//! durable record for files that cannot be derived from another table (backups, release banks,
//! archived project directories); `vowp gc` merges these rows with the classes it derives from
//! the database and reports the union by path.
//!
//! The store also keeps a few small lifecycle helpers here for tables that belong to other
//! feature repositories. They are deliberately named `lifecycle_*` so `repo_capacity.rs` can use
//! its natural `insert_host_sample`/`prune_host_samples` names without a collision.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{CatalogClass, CatalogEntry, CatalogFilter, HostSample, ProjectName, SessionId, Timestamp};

use crate::convert::{enum_from_text, enum_to_text, opt_ts_from_text, opt_ts_to_text, ts_from_text, ts_to_text};
use crate::db::Store;
use crate::error::StoreError;

/// Column list shared by every `catalog` query.
const CATALOG_COLUMNS: &str = "id, path, class, project, environment, job_id, session_id, bytes, expires_at, note, created_at, updated_at, deleted_at";

/// Raw values of one `catalog` row before protocol conversion.
struct CatalogRow {
    id: String,
    path: String,
    class: String,
    project: Option<String>,
    environment: String,
    job_id: Option<String>,
    session_id: Option<String>,
    bytes: Option<i64>,
    expires_at: Option<String>,
    note: String,
    created_at: String,
    updated_at: String,
    deleted_at: Option<String>,
}

/// Reads the textual columns of a `catalog` row.
fn read_catalog_row(row: &Row<'_>) -> rusqlite::Result<CatalogRow> {
    Ok(CatalogRow {
        id: row.get(0)?,
        path: row.get(1)?,
        class: row.get(2)?,
        project: row.get(3)?,
        environment: row.get(4)?,
        job_id: row.get(5)?,
        session_id: row.get(6)?,
        bytes: row.get(7)?,
        expires_at: row.get(8)?,
        note: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        deleted_at: row.get(12)?,
    })
}

/// Converts a raw `catalog` row into a protocol [`CatalogEntry`].
fn catalog_entry_from_row(raw: CatalogRow) -> Result<CatalogEntry, StoreError> {
    Ok(CatalogEntry {
        id: raw.id,
        path: raw.path,
        class: enum_from_text::<CatalogClass>(&raw.class, "catalog.class")?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        environment: raw.environment,
        job_id: raw.job_id.as_deref().map(vowp_protocol::JobId::parse).transpose()?,
        session_id: raw.session_id.as_deref().map(SessionId::parse).transpose()?,
        bytes: raw
            .bytes
            .map(|value| u64::try_from(value).map_err(|err| crate::convert::corrupt("catalog.bytes", err)))
            .transpose()?,
        expires_at: opt_ts_from_text(raw.expires_at.as_deref(), "catalog.expires_at")?,
        note: raw.note,
        created_at: ts_from_text(&raw.created_at, "catalog.created_at")?,
        updated_at: ts_from_text(&raw.updated_at, "catalog.updated_at")?,
        deleted_at: opt_ts_from_text(raw.deleted_at.as_deref(), "catalog.deleted_at")?,
    })
}

/// One expired host sample row selected by `vowp gc`.
///
/// `HostSample` has no row id, so the report uses this small projection when it needs to show
/// which rows a dry run would prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleSample {
    /// `host_samples.id`.
    pub id: i64,
    /// When the sample was taken.
    pub at: Timestamp,
    /// Sampling interval that produced the row.
    pub interval_secs: u64,
}

impl Store {
    /// Inserts or updates one catalog row, keyed by `path`.
    ///
    /// The original `id` and `created_at` are kept when the path already exists; every other
    /// field is replaced, including `deleted_at`. A caller that revives a deleted row can pass
    /// `deleted_at: None`.
    pub fn upsert_catalog(&self, e: &CatalogEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.catalog (
                id, path, class, project, environment, job_id, session_id, bytes, expires_at,
                note, created_at, updated_at, deleted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(path) DO UPDATE SET
                class = excluded.class,
                project = excluded.project,
                environment = excluded.environment,
                job_id = excluded.job_id,
                session_id = excluded.session_id,
                bytes = excluded.bytes,
                expires_at = excluded.expires_at,
                note = excluded.note,
                updated_at = excluded.updated_at,
                deleted_at = excluded.deleted_at",
            params![
                e.id,
                e.path,
                enum_to_text(&e.class)?,
                e.project.as_ref().map(ProjectName::as_str),
                e.environment,
                e.job_id.as_ref().map(vowp_protocol::JobId::as_str),
                e.session_id.as_ref().map(SessionId::as_str),
                e.bytes
                    .map(|value| i64::try_from(value).map_err(|err| crate::convert::corrupt("catalog.bytes", err)))
                    .transpose()?,
                opt_ts_to_text(e.expires_at),
                e.note,
                ts_to_text(e.created_at),
                ts_to_text(e.updated_at),
                opt_ts_to_text(e.deleted_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the catalog row for `path`, when it exists.
    pub fn catalog_entry(&self, path: &str) -> Result<Option<CatalogEntry>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {CATALOG_COLUMNS} FROM main.catalog WHERE path = ?1"),
                [path],
                read_catalog_row,
            )
            .optional()?;
        raw.map(catalog_entry_from_row).transpose()
    }

    /// Lists catalog rows matching `f`, ordered by class then path.
    pub fn list_catalog(&self, f: &CatalogFilter) -> Result<Vec<CatalogEntry>, StoreError> {
        let mut query = format!("SELECT {CATALOG_COLUMNS} FROM main.catalog WHERE 1 = 1");
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(class) = &f.class {
            query.push_str(" AND class = ?");
            values.push(Box::new(enum_to_text(class)?));
        }
        if let Some(project) = &f.project {
            query.push_str(" AND project = ?");
            values.push(Box::new(project.as_str().to_owned()));
        }
        if let Some(expired_before) = f.expired_before {
            query.push_str(" AND expires_at IS NOT NULL AND expires_at <= ?");
            values.push(Box::new(ts_to_text(expired_before)));
        }
        query.push_str(" ORDER BY class, path");
        if let Some(limit) = f.limit {
            query.push_str(" LIMIT ?");
            values.push(Box::new(i64::from(limit)));
        }

        let mut statement = self.conn.prepare(&query)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), read_catalog_row)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(catalog_entry_from_row(row?)?);
        }
        Ok(entries)
    }

    /// Marks one catalog row deleted at `at`.
    pub fn mark_catalog_deleted(&self, id: &str, at: Timestamp) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.catalog SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![ts_to_text(at), id],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("catalog entry", id));
        }
        Ok(())
    }

    /// Deletes one catalog row; returns false when it did not exist.
    pub fn delete_catalog(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute("DELETE FROM main.catalog WHERE id = ?1", [id])? > 0)
    }

    /// Returns `sessions.compacted_at` for `session`, or `None` when the row or column is absent.
    ///
    /// The public [`vowp_protocol::Session`] type predates the v0.2 column, so this helper exists
    /// for the lifecycle code and its tests rather than changing that frozen type.
    pub fn lifecycle_compacted_at(&self, session: &SessionId) -> Result<Option<Timestamp>, StoreError> {
        let value = self
            .conn
            .query_row(
                "SELECT compacted_at FROM main.sessions WHERE id = ?1",
                [session.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match value.flatten() {
            Some(text) => Ok(Some(ts_from_text(&text, "sessions.compacted_at")?)),
            None => Ok(None),
        }
    }

    /// Lists the blob names referenced by passing gate evidence in the bound project.
    ///
    /// `vowp gc` keeps these blobs as `truth` when it compacts a closed session.
    pub fn lifecycle_passing_gate_blobs(&self) -> Result<Vec<String>, StoreError> {
        self.require_bound()?;
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT output_blob FROM project.gate_results
             WHERE passed = 1 AND output_blob IS NOT NULL ORDER BY output_blob",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut blobs = Vec::new();
        for row in rows {
            blobs.push(row?);
        }
        Ok(blobs)
    }

    /// Inserts one host sample and returns its row id.
    ///
    /// This is the lifecycle-side writer used by the gc tests. The production insert path
    /// lives in `repo_capacity.rs`.
    pub fn lifecycle_insert_host_sample(&self, sample: &HostSample) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO main.host_samples (
                at, interval_secs, mem_total_bytes, mem_available_bytes, swap_used_bytes,
                load1, load5, load15, cpu_count, cpu_quota_cores, disk_free_home_bytes,
                disk_free_worktrees_bytes, pressure_cpu_avg10, pressure_mem_avg10,
                pressure_io_avg10, source
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                ts_to_text(sample.at),
                i64::try_from(sample.interval_secs)
                    .map_err(|err| crate::convert::corrupt("host_samples.interval_secs", err))?,
                sql_u64(sample.mem_total_bytes, "host_samples.mem_total_bytes")?,
                sql_u64(sample.mem_available_bytes, "host_samples.mem_available_bytes")?,
                sql_u64(sample.swap_used_bytes, "host_samples.swap_used_bytes")?,
                sample.load1,
                sample.load5,
                sample.load15,
                i64::from(sample.cpu_count),
                sample.cpu_quota_cores,
                sql_u64(sample.disk_free_home_bytes, "host_samples.disk_free_home_bytes")?,
                sample
                    .disk_free_worktrees_bytes
                    .map(|value| sql_u64(value, "host_samples.disk_free_worktrees_bytes"))
                    .transpose()?,
                sample.pressure_cpu_avg10,
                sample.pressure_mem_avg10,
                sample.pressure_io_avg10,
                enum_to_text(&sample.source)?,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Lists the host sample rows that `vowp gc` would prune.
    ///
    /// Full-rate rows (`interval_secs < 60`) older than `full_before` and minute rows
    /// (`interval_secs >= 60`) older than `minute_before` are returned, oldest first.
    pub fn lifecycle_sample_rows(
        &self,
        full_before: Timestamp,
        minute_before: Timestamp,
    ) -> Result<Vec<LifecycleSample>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, interval_secs FROM main.host_samples
             WHERE (interval_secs < 60 AND at < ?1) OR (interval_secs >= 60 AND at < ?2)
             ORDER BY id",
        )?;
        let rows = statement.query_map(params![ts_to_text(full_before), ts_to_text(minute_before)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?;
        let mut samples = Vec::new();
        for row in rows {
            let (id, at, interval_secs) = row?;
            samples.push(LifecycleSample {
                id,
                at: ts_from_text(&at, "host_samples.at")?,
                interval_secs: u64::try_from(interval_secs)
                    .map_err(|err| crate::convert::corrupt("host_samples.interval_secs", err))?,
            });
        }
        Ok(samples)
    }

    /// Deletes the host sample rows selected by [`Store::lifecycle_sample_rows`].
    ///
    /// Returns the number of deleted rows.
    pub fn lifecycle_prune_samples(&self, full_before: Timestamp, minute_before: Timestamp) -> Result<u64, StoreError> {
        self.transaction(|tx| {
            let full = tx.conn.execute(
                "DELETE FROM main.host_samples WHERE interval_secs < 60 AND at < ?1",
                [ts_to_text(full_before)],
            )?;
            let minute = tx.conn.execute(
                "DELETE FROM main.host_samples WHERE interval_secs >= 60 AND at < ?1",
                [ts_to_text(minute_before)],
            )?;
            Ok((full + minute) as u64)
        })
    }
}

/// Converts a byte count into SQLite's signed integer representation.
fn sql_u64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|err| crate::convert::corrupt(what, err))
}
