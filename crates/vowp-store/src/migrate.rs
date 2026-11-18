//! The v0.1 → v0.2 split migration and its rollback.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.9.
//!
//! The migration writes fresh global and project files in a staging directory, verifies row
//! counts and foreign keys, and then swaps the files into place. It is restartable: a manifest
//! in `<home>/migration.json` records the stage and the backup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use vowp_protocol::{ProjectName, Timestamp};

use crate::db::{GLOBAL_SCHEMA_SQL, GLOBAL_SCHEMA_VERSION, PROJECT_SCHEMA, PROJECT_SCHEMA_VERSION};
use crate::error::StoreError;
use crate::paths::NodePaths;
use crate::session_log::{pid_alive, valid_pid};

/// Every table in the v2 global schema, in copy and count order.
const V02_GLOBAL_TABLES: [&str; 21] = [
    "kv",
    "nodes",
    "projects",
    "sessions",
    "session_events",
    "session_natives",
    "native_events",
    "capsules",
    "switches",
    "limits",
    "node_inbox",
    "node_ledger",
    "surfaces",
    "leases",
    "catalog",
    "host_samples",
    "run_usage",
    "usage_estimates",
    "capacity_targets",
    "capacity_advice",
    "secret_index",
];

/// The tables added by the v3 global schema; a migrated v2 home starts with none.
const V03_NEW_TABLES: [&str; 3] = ["node_credentials", "enrollment_codes", "node_paths"];

/// Every table in the v3 global schema, in copy and count order.
const V03_GLOBAL_TABLES: [&str; 24] = [
    "kv",
    "nodes",
    "node_credentials",
    "enrollment_codes",
    "node_paths",
    "projects",
    "sessions",
    "session_events",
    "session_natives",
    "native_events",
    "capsules",
    "switches",
    "limits",
    "node_inbox",
    "node_ledger",
    "surfaces",
    "leases",
    "catalog",
    "host_samples",
    "run_usage",
    "usage_estimates",
    "capacity_targets",
    "capacity_advice",
    "secret_index",
];

/// The tables added by the v4 global schema; a migrated v3 home starts with none.
const V04_NEW_TABLES: [&str; 1] = ["surface_tokens"];

/// Every table in the v4 global schema, in copy and count order.
const V04_GLOBAL_TABLES: [&str; 25] = [
    "kv",
    "nodes",
    "node_credentials",
    "enrollment_codes",
    "node_paths",
    "projects",
    "sessions",
    "session_events",
    "session_natives",
    "native_events",
    "capsules",
    "switches",
    "limits",
    "node_inbox",
    "node_ledger",
    "surfaces",
    "leases",
    "surface_tokens",
    "catalog",
    "host_samples",
    "run_usage",
    "usage_estimates",
    "capacity_targets",
    "capacity_advice",
    "secret_index",
];

/// Every table in the v5 global schema, in copy and count order.
const V05_GLOBAL_TABLES: [&str; 25] = V04_GLOBAL_TABLES;

/// The tables added by the v6 global schema; a migrated v5 home starts with none.
const V06_NEW_TABLES: [&str; 2] = ["agent_assignments", "node_limits"];

/// The tables added by the v7 global schema; a migrated v6 home starts with none.
const V07_NEW_TABLES: [&str; 1] = ["capacity_rollups"];

/// Schema version written by the v2 → v3 migration.
const V03_SCHEMA_VERSION: i64 = 3;

/// Schema version written by the v3 → v4 migration.
const V04_SCHEMA_VERSION: i64 = 4;

/// Schema version written by the v4 → v5 migration.
const V05_SCHEMA_VERSION: i64 = 5;

/// Schema version written by the v5 → v6 migration.
const V06_SCHEMA_VERSION: i64 = 6;

/// Schema version written by the v6 → v7 migration.
const V07_SCHEMA_VERSION: i64 = 7;

/// Every table in the project v1 schema; all must exist before the v2 upgrade.
const PROJECT_V1_TABLES: [&str; 10] = [
    "jobs",
    "edges",
    "runs",
    "gate_results",
    "workers",
    "loops",
    "schedules",
    "inbox",
    "ledger",
    "releases",
];

/// The one table added by the project v1 → v2 migration.
const PROJECT_V2_DDL: &str = "CREATE TABLE job_placement (
  job        TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
  placement  TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;";

/// Old and new row counts for one table or table family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCount {
    /// Rows in the v0.1 file.
    pub before: u64,
    /// Rows in the split files.
    pub after: u64,
}

/// What one migration or rollback did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    /// Source schema version.
    pub from_version: i64,
    /// Destination schema version.
    pub to_version: i64,
    /// Backup file the migration created (or would create).
    pub backup: String,
    /// Projects found in the v0.1 registry, excluding the reserved `_node` row.
    pub projects: Vec<ProjectName>,
    /// Per-table counts, keyed `global:<table>` or `project:<name>:<table>`.
    pub tables: BTreeMap<String, MigrationCount>,
    /// All integrity, foreign-key and row-count checks passed.
    pub verified: bool,
    /// Nothing was written.
    pub dry_run: bool,
    /// Worker slots that could not be assigned to exactly one project.
    pub dropped_workers: u32,
    /// Worker rows that were dropped, as `node:kind:slot`.
    pub dropped_worker_rows: Vec<String>,
    /// Job-less runs moved to the primary project.
    pub primary_runs: Vec<String>,
    /// Node-wide loops moved to the primary project.
    pub node_wide_loops: Vec<String>,
    /// Node-wide schedules moved to the primary project.
    pub node_wide_schedules: Vec<String>,
    /// Human-readable notes for the report.
    pub warnings: Vec<String>,
}

/// One registry row read from the v0.1 file.
struct SourceProject {
    name: ProjectName,
}

/// Preflight result shared by dry-run and the real migration.
struct Source {
    projects: Vec<SourceProject>,
    primary: ProjectName,
    backup: PathBuf,
    counts: BTreeMap<String, u64>,
    dropped_worker_rows: Vec<String>,
    primary_runs: Vec<String>,
    node_wide_loops: Vec<String>,
    node_wide_schedules: Vec<String>,
}

/// A manifest persisted in `<home>/migration.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    from: i64,
    to: i64,
    backup: String,
    stage: String,
    state: String,
}

/// Migrates a v0.1 single-file home into the global + per-project layout.
pub fn migrate_v01(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let source = preflight(paths)?;
    let backup = source.backup.clone();
    let stage = paths.tmp_dir().join(format!("migrate-{}", ulid::Ulid::generate()));
    write_manifest(
        paths,
        &Manifest {
            from: 1,
            to: 2,
            backup: backup.display().to_string(),
            stage: stage.display().to_string(),
            state: "copying".to_owned(),
        },
    )?;

    let result = (|| {
        backup_v01(paths, &backup)?;
        create_stage(&stage, paths, &source)?;
        let mut report = copy_and_verify(paths, &stage, &source)?;
        report.backup = backup.display().to_string();
        swap(paths, &stage, &backup)?;
        Ok(report)
    })();

    if result.is_err() {
        // Before the swap the original home is untouched; after it starts, keep the manifest
        // so `resume_migration` can finish or recover on the next open.
        let swapping = std::fs::read(paths.migration_manifest_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .is_some_and(|manifest| manifest.state == "swapping");
        if !swapping {
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_file(paths.migration_manifest_path());
        }
    }
    result
}

/// Computes the migration plan and counts without writing anything.
pub fn migrate_v01_dry_run(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    let source = preflight(paths)?;
    let mut tables = BTreeMap::new();
    for (key, before) in &source.counts {
        tables.insert(
            key.clone(),
            MigrationCount {
                before: *before,
                after: *before,
            },
        );
    }
    tables.insert(
        "workers_dropped".to_owned(),
        MigrationCount {
            before: u64::try_from(source.dropped_worker_rows.len()).unwrap_or(u64::MAX),
            after: u64::try_from(source.dropped_worker_rows.len()).unwrap_or(u64::MAX),
        },
    );
    Ok(MigrationReport {
        from_version: 1,
        to_version: 2,
        backup: source.backup.display().to_string(),
        projects: source.projects.iter().map(|project| project.name.clone()).collect(),
        tables,
        verified: true,
        dry_run: true,
        dropped_workers: u32::try_from(source.dropped_worker_rows.len()).unwrap_or(u32::MAX),
        dropped_worker_rows: source.dropped_worker_rows.clone(),
        primary_runs: source.primary_runs.clone(),
        node_wide_loops: source.node_wide_loops.clone(),
        node_wide_schedules: source.node_wide_schedules.clone(),
        warnings: Vec::new(),
    })
}

/// Migrates an existing v2 global database to the v3 local-node schema.
///
/// The project databases and session logs are untouched. The migration builds a fresh v3 file
/// in `tmp/`, copies every global row into it, verifies counts, integrity and foreign keys,
/// writes a backup of the original file and then swaps the staged file into place. A manifest
/// in `<home>/migration.json` makes an interrupted swap restartable.
pub fn migrate_v02(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let version = db_version(&paths.db_path())?;
    match version {
        Some(version) if version >= V03_SCHEMA_VERSION => {
            return Ok(MigrationReport {
                from_version: version,
                to_version: version,
                backup: String::new(),
                projects: Vec::new(),
                tables: BTreeMap::new(),
                verified: true,
                dry_run: false,
                dropped_workers: 0,
                dropped_worker_rows: Vec::new(),
                primary_runs: Vec::new(),
                node_wide_loops: Vec::new(),
                node_wide_schedules: Vec::new(),
                warnings: vec![format!(
                    "the global database is already at schema version {version}, not v2"
                )],
            });
        }
        Some(2) => {}
        Some(1) => return Err(StoreError::MigrationRequired),
        Some(found) => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: V03_SCHEMA_VERSION,
            });
        }
        None => {
            return Err(StoreError::not_found(
                "v0.2 database",
                paths.db_path().display().to_string(),
            ));
        }
    }
    refuse_live_supervisor(paths)?;
    validate_v02_source(paths)?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.3-{}.db", Timestamp::now().unix_millis()));
    let stage = paths.tmp_dir().join(format!("migrate-v3-{}", ulid::Ulid::generate()));
    write_manifest(
        paths,
        &Manifest {
            from: 2,
            to: 3,
            backup: backup.display().to_string(),
            stage: stage.display().to_string(),
            state: "copying".to_owned(),
        },
    )?;

    let result = (|| {
        backup_global(paths, &backup)?;
        let tables = create_v02_stage(&stage, paths)?;
        write_manifest(
            paths,
            &Manifest {
                from: 2,
                to: 3,
                backup: backup.display().to_string(),
                stage: stage.display().to_string(),
                state: "swapping".to_owned(),
            },
        )?;
        swap_global_from_stage(paths, &stage)?;
        let _ = std::fs::remove_file(paths.migration_manifest_path());
        Ok(MigrationReport {
            from_version: 2,
            to_version: 3,
            backup: backup.display().to_string(),
            projects: Vec::new(),
            tables,
            verified: true,
            dry_run: false,
            dropped_workers: 0,
            dropped_worker_rows: Vec::new(),
            primary_runs: Vec::new(),
            node_wide_loops: Vec::new(),
            node_wide_schedules: Vec::new(),
            warnings: Vec::new(),
        })
    })();

    if result.is_err() {
        let swapping = std::fs::read(paths.migration_manifest_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .is_some_and(|manifest| manifest.state == "swapping");
        if !swapping {
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_file(paths.migration_manifest_path());
        }
    }
    result
}

/// Runs [`migrate_v02`] when an existing home still has the v2 global schema.
///
/// A v0.1 file is left for [`migrate_v01`] to handle; a future version is left for the normal
/// open path to refuse with [`StoreError::SchemaTooNew`].
pub(crate) fn migrate_v02_if_needed(paths: &NodePaths) -> Result<(), StoreError> {
    if db_version(&paths.db_path())? == Some(2) {
        migrate_v02(paths)?;
    }
    Ok(())
}

/// Migrates an existing v3 global database to the v4 surface-token schema.
///
/// The project databases and session logs are untouched. The migration builds a fresh v4 file
/// in `tmp/`, copies every v3 global row into it (rebuilding `surfaces` to add `state` and
/// `revoked_at`), verifies counts, integrity and foreign keys, writes a backup of the original
/// file and then swaps the staged file into place. A manifest in `<home>/migration.json` makes
/// an interrupted swap restartable.
pub fn migrate_v03(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let version = db_version(&paths.db_path())?;
    match version {
        Some(GLOBAL_SCHEMA_VERSION) => {
            return Ok(MigrationReport {
                from_version: GLOBAL_SCHEMA_VERSION,
                to_version: GLOBAL_SCHEMA_VERSION,
                backup: String::new(),
                projects: Vec::new(),
                tables: BTreeMap::new(),
                verified: true,
                dry_run: false,
                dropped_workers: 0,
                dropped_worker_rows: Vec::new(),
                primary_runs: Vec::new(),
                node_wide_loops: Vec::new(),
                node_wide_schedules: Vec::new(),
                warnings: vec![format!(
                    "the global database is already at schema version {GLOBAL_SCHEMA_VERSION}"
                )],
            });
        }
        Some(V03_SCHEMA_VERSION) => {}
        Some(found) if found > GLOBAL_SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: GLOBAL_SCHEMA_VERSION,
            });
        }
        Some(_) => return Err(StoreError::MigrationRequired),
        None => {
            return Err(StoreError::not_found(
                "v0.3 database",
                paths.db_path().display().to_string(),
            ));
        }
    }
    refuse_live_supervisor(paths)?;
    validate_v03_source(paths)?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.4-{}.db", Timestamp::now().unix_millis()));
    let stage = paths.tmp_dir().join(format!("migrate-v4-{}", ulid::Ulid::generate()));
    write_manifest(
        paths,
        &Manifest {
            from: 3,
            to: 4,
            backup: backup.display().to_string(),
            stage: stage.display().to_string(),
            state: "copying".to_owned(),
        },
    )?;

    let result = (|| {
        backup_global(paths, &backup)?;
        let tables = create_v03_stage(&stage, paths)?;
        write_manifest(
            paths,
            &Manifest {
                from: 3,
                to: 4,
                backup: backup.display().to_string(),
                stage: stage.display().to_string(),
                state: "swapping".to_owned(),
            },
        )?;
        swap_global_from_stage(paths, &stage)?;
        let _ = std::fs::remove_file(paths.migration_manifest_path());
        Ok(MigrationReport {
            from_version: 3,
            to_version: 4,
            backup: backup.display().to_string(),
            projects: Vec::new(),
            tables,
            verified: true,
            dry_run: false,
            dropped_workers: 0,
            dropped_worker_rows: Vec::new(),
            primary_runs: Vec::new(),
            node_wide_loops: Vec::new(),
            node_wide_schedules: Vec::new(),
            warnings: Vec::new(),
        })
    })();

    if result.is_err() {
        let swapping = std::fs::read(paths.migration_manifest_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .is_some_and(|manifest| manifest.state == "swapping");
        if !swapping {
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_file(paths.migration_manifest_path());
        }
    }
    result
}

/// Runs [`migrate_v03`] when an existing home still has the v3 global schema.
///
/// A v0.1 or v0.2 file is handled by the earlier migrations before this check. A future
/// version is left for the normal open path to refuse with [`StoreError::SchemaTooNew`].
pub(crate) fn migrate_v03_if_needed(paths: &NodePaths) -> Result<(), StoreError> {
    if db_version(&paths.db_path())? == Some(V03_SCHEMA_VERSION) {
        migrate_v03(paths)?;
    }
    Ok(())
}

/// Migrates an existing v4 global database to the v5 lease-hash schema.
///
/// The project databases and session logs are untouched. The migration builds a fresh v5
/// file in `tmp/`, copies every v4 global row except `leases`, verifies counts, integrity
/// and foreign keys, writes a backup of the original file and then swaps the staged file
/// into place. Leases are cleared because a v4 row holds a plaintext token and every lease
/// is stale within 60 seconds; surfaces and every other row are preserved. A manifest in
/// `<home>/migration.json` makes an interrupted swap restartable.
pub fn migrate_v04(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let version = db_version(&paths.db_path())?;
    match version {
        Some(GLOBAL_SCHEMA_VERSION) => {
            return Ok(MigrationReport {
                from_version: GLOBAL_SCHEMA_VERSION,
                to_version: GLOBAL_SCHEMA_VERSION,
                backup: String::new(),
                projects: Vec::new(),
                tables: BTreeMap::new(),
                verified: true,
                dry_run: false,
                dropped_workers: 0,
                dropped_worker_rows: Vec::new(),
                primary_runs: Vec::new(),
                node_wide_loops: Vec::new(),
                node_wide_schedules: Vec::new(),
                warnings: vec![format!(
                    "the global database is already at schema version {GLOBAL_SCHEMA_VERSION}"
                )],
            });
        }
        Some(V04_SCHEMA_VERSION) => {}
        Some(found) if found > GLOBAL_SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: GLOBAL_SCHEMA_VERSION,
            });
        }
        Some(_) => return Err(StoreError::MigrationRequired),
        None => {
            return Err(StoreError::not_found(
                "v0.4 database",
                paths.db_path().display().to_string(),
            ));
        }
    }
    refuse_live_supervisor(paths)?;
    validate_v04_source(paths)?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.5-{}.db", Timestamp::now().unix_millis()));
    let stage = paths.tmp_dir().join(format!("migrate-v5-{}", ulid::Ulid::generate()));
    write_manifest(
        paths,
        &Manifest {
            from: 4,
            to: 5,
            backup: backup.display().to_string(),
            stage: stage.display().to_string(),
            state: "copying".to_owned(),
        },
    )?;

    let result = (|| {
        backup_global(paths, &backup)?;
        let tables = create_v04_stage(&stage, paths)?;
        write_manifest(
            paths,
            &Manifest {
                from: 4,
                to: 5,
                backup: backup.display().to_string(),
                stage: stage.display().to_string(),
                state: "swapping".to_owned(),
            },
        )?;
        swap_global_from_stage(paths, &stage)?;
        let _ = std::fs::remove_file(paths.migration_manifest_path());
        let mut warnings = Vec::new();
        if let Some(count) = tables.get("global:leases")
            && count.before > 0
        {
            warnings.push(format!(
                "cleared {} ephemeral lease(s); surfaces re-acquire on their next frame",
                count.before
            ));
        }
        Ok(MigrationReport {
            from_version: 4,
            to_version: 5,
            backup: backup.display().to_string(),
            projects: Vec::new(),
            tables,
            verified: true,
            dry_run: false,
            dropped_workers: 0,
            dropped_worker_rows: Vec::new(),
            primary_runs: Vec::new(),
            node_wide_loops: Vec::new(),
            node_wide_schedules: Vec::new(),
            warnings,
        })
    })();

    if result.is_err() {
        let swapping = std::fs::read(paths.migration_manifest_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .is_some_and(|manifest| manifest.state == "swapping");
        if !swapping {
            let _ = std::fs::remove_dir_all(&stage);
            let _ = std::fs::remove_file(paths.migration_manifest_path());
        }
    }
    result
}

/// Runs [`migrate_v04`] when an existing home still has the v4 global schema.
///
/// Earlier versions are handled by the previous migrations before this check. A future
/// version is left for the normal open path to refuse with [`StoreError::SchemaTooNew`].
pub(crate) fn migrate_v04_if_needed(paths: &NodePaths) -> Result<(), StoreError> {
    if db_version(&paths.db_path())? == Some(V04_SCHEMA_VERSION) {
        migrate_v04(paths)?;
    }
    Ok(())
}

/// Migrates an existing v5 global database to the v6 offload schema.
///
/// The upgrade is additive: `agent_assignments`, its node index and `node_limits` are created
/// in one transaction, after the original file is copied to `backups/pre-0.6-<ms>.db`. Every
/// existing table's rows are counted before and after, and integrity and foreign keys are
/// checked before `user_version = 6` commits; a failure rolls the whole transaction back and
/// leaves the v5 file readable.
pub fn migrate_v05(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let version = db_version(&paths.db_path())?;
    match version {
        Some(version) if (V06_SCHEMA_VERSION..=GLOBAL_SCHEMA_VERSION).contains(&version) => {
            return Ok(MigrationReport {
                from_version: version,
                to_version: version,
                backup: String::new(),
                projects: Vec::new(),
                tables: BTreeMap::new(),
                verified: true,
                dry_run: false,
                dropped_workers: 0,
                dropped_worker_rows: Vec::new(),
                primary_runs: Vec::new(),
                node_wide_loops: Vec::new(),
                node_wide_schedules: Vec::new(),
                warnings: vec![format!(
                    "the global database is already at schema version {version}, not v5"
                )],
            });
        }
        Some(V05_SCHEMA_VERSION) => {}
        Some(1..=4) => return Err(StoreError::MigrationRequired),
        Some(found) => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: GLOBAL_SCHEMA_VERSION,
            });
        }
        None => {
            return Err(StoreError::not_found(
                "v0.5 database",
                paths.db_path().display().to_string(),
            ));
        }
    }
    refuse_live_supervisor(paths)?;
    validate_v05_source(paths)?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.6-{}.db", Timestamp::now().unix_millis()));
    backup_global(paths, &backup)?;

    let conn = Connection::open(paths.db_path())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
    let before = table_counts(&tx, "main")?;
    tx.execute_batch(
        "CREATE TABLE agent_assignments (
           id           TEXT PRIMARY KEY,
           run_id       TEXT NOT NULL,
           session_id   TEXT NOT NULL,
           project      TEXT NOT NULL,
           node         TEXT NOT NULL,
           epoch        INTEGER NOT NULL,
           state        TEXT NOT NULL CHECK (state IN
                          ('queued','sent','accepted','running','disconnected',
                           'finished','lost','cancelled','rejected')),
           payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
           created_at   TEXT NOT NULL,
           claimed_at   TEXT,
           expires_at   TEXT,
           detail       TEXT NOT NULL DEFAULT ''
         ) STRICT;
         CREATE INDEX agent_assignments_node ON agent_assignments(node, state);
         CREATE TABLE node_limits (
           provider    TEXT NOT NULL,
           node        TEXT NOT NULL REFERENCES nodes(name) ON DELETE CASCADE,
           kind        TEXT NOT NULL CHECK (kind IN ('quota','credits','rate','context','auth','unavailable')),
           resets_at   TEXT,
           detail      TEXT NOT NULL DEFAULT '',
           since       TEXT NOT NULL,
           updated_at  TEXT NOT NULL,
           PRIMARY KEY (provider, node)
         ) STRICT;",
    )?;
    tx.pragma_update(None, "user_version", V06_SCHEMA_VERSION)?;

    let after = table_counts(&tx, "main")?;
    let mut tables = BTreeMap::new();
    for (table, before_rows) in &before {
        let after_rows = after.get(table).copied().ok_or_else(|| StoreError::Corrupt {
            path: format!("stage/global.db:{table}"),
            line: None,
            reason: "existing table disappeared during the v6 migration".to_owned(),
        })?;
        if after_rows != *before_rows {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("row count changed: before {before_rows}, after {after_rows}"),
            });
        }
        tables.insert(
            format!("global:{table}"),
            MigrationCount {
                before: *before_rows,
                after: after_rows,
            },
        );
    }
    for table in V06_NEW_TABLES {
        let after_rows = after.get(table).copied().ok_or_else(|| StoreError::Corrupt {
            path: format!("stage/global.db:{table}"),
            line: None,
            reason: "new table is missing after the v6 migration".to_owned(),
        })?;
        if after_rows != 0 {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("new table is not empty: {after_rows} rows"),
            });
        }
        tables.insert(
            format!("global:{table}"),
            MigrationCount {
                before: 0,
                after: after_rows,
            },
        );
    }
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: "global.db:integrity_check".to_owned(),
            line: None,
            reason: integrity,
        });
    }
    {
        let mut statement = tx.prepare("PRAGMA foreign_key_check")?;
        if statement.query([])?.next()?.is_some() {
            return Err(StoreError::Corrupt {
                path: "global.db:foreign_key_check".to_owned(),
                line: None,
                reason: "foreign key violations".to_owned(),
            });
        }
    }
    tx.commit()?;
    Ok(MigrationReport {
        from_version: 5,
        to_version: 6,
        backup: backup.display().to_string(),
        projects: Vec::new(),
        tables,
        verified: true,
        dry_run: false,
        dropped_workers: 0,
        dropped_worker_rows: Vec::new(),
        primary_runs: Vec::new(),
        node_wide_loops: Vec::new(),
        node_wide_schedules: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Runs [`migrate_v05`] when an existing home still has the v5 global schema.
///
/// Earlier versions are handled by the previous migrations before this check. A future
/// version is left for the normal open path to refuse with [`StoreError::SchemaTooNew`].
pub(crate) fn migrate_v05_if_needed(paths: &NodePaths) -> Result<(), StoreError> {
    if db_version(&paths.db_path())? == Some(V05_SCHEMA_VERSION) {
        migrate_v05(paths)?;
    }
    Ok(())
}

/// Migrates an existing v6 global database to the v7 capacity-history schema.
///
/// The upgrade is additive: `capacity_rollups` and its bucket index are created in one
/// immediate transaction, after the original file is copied to `backups/pre-0.7-<ms>.db`.
/// Every existing table's rows are counted before and after, and integrity and foreign keys
/// are checked before `user_version = 7` commits; a failure rolls the whole transaction back
/// and leaves the v6 file readable. Calling it again on a v7 home is a no-op.
pub fn migrate_v06(paths: &NodePaths) -> Result<MigrationReport, StoreError> {
    resume_migration(paths)?;
    let version = db_version(&paths.db_path())?;
    match version {
        Some(V07_SCHEMA_VERSION) => {
            return Ok(MigrationReport {
                from_version: V07_SCHEMA_VERSION,
                to_version: V07_SCHEMA_VERSION,
                backup: String::new(),
                projects: Vec::new(),
                tables: BTreeMap::new(),
                verified: true,
                dry_run: false,
                dropped_workers: 0,
                dropped_worker_rows: Vec::new(),
                primary_runs: Vec::new(),
                node_wide_loops: Vec::new(),
                node_wide_schedules: Vec::new(),
                warnings: vec![format!(
                    "the global database is already at schema version {V07_SCHEMA_VERSION}, not v6"
                )],
            });
        }
        Some(V06_SCHEMA_VERSION) => {}
        Some(1..=5) => return Err(StoreError::MigrationRequired),
        Some(found) => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: V07_SCHEMA_VERSION,
            });
        }
        None => {
            return Err(StoreError::not_found(
                "v0.6 database",
                paths.db_path().display().to_string(),
            ));
        }
    }
    refuse_live_supervisor(paths)?;
    validate_v06_source(paths)?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.7-{}.db", Timestamp::now().unix_millis()));
    backup_global(paths, &backup)?;

    let conn = Connection::open(paths.db_path())?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
    let before = table_counts(&tx, "main")?;
    tx.execute_batch(
        "CREATE TABLE capacity_rollups (
           bucket                   TEXT NOT NULL,
           bucket_secs              INTEGER NOT NULL,
           samples                  INTEGER NOT NULL,
           mem_available_min_bytes  INTEGER NOT NULL,
           mem_available_avg_bytes  INTEGER NOT NULL,
           mem_available_max_bytes  INTEGER NOT NULL,
           load1_avg                REAL NOT NULL,
           disk_free_home_min_bytes INTEGER NOT NULL,
           source                   TEXT NOT NULL CHECK (source IN ('proc','fallback','mixed')),
           PRIMARY KEY (bucket, bucket_secs)
         ) STRICT;
         CREATE INDEX capacity_rollups_bucket ON capacity_rollups(bucket);",
    )?;
    tx.pragma_update(None, "user_version", V07_SCHEMA_VERSION)?;

    let after = table_counts(&tx, "main")?;
    let mut tables = BTreeMap::new();
    for (table, before_rows) in &before {
        let after_rows = after.get(table).copied().ok_or_else(|| StoreError::Corrupt {
            path: format!("stage/global.db:{table}"),
            line: None,
            reason: "existing table disappeared during the v7 migration".to_owned(),
        })?;
        if after_rows != *before_rows {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("row count changed: before {before_rows}, after {after_rows}"),
            });
        }
        tables.insert(
            format!("global:{table}"),
            MigrationCount {
                before: *before_rows,
                after: after_rows,
            },
        );
    }
    for table in V07_NEW_TABLES {
        let after_rows = after.get(table).copied().ok_or_else(|| StoreError::Corrupt {
            path: format!("stage/global.db:{table}"),
            line: None,
            reason: "new table is missing after the v7 migration".to_owned(),
        })?;
        if after_rows != 0 {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("new table is not empty: {after_rows} rows"),
            });
        }
        tables.insert(
            format!("global:{table}"),
            MigrationCount {
                before: 0,
                after: after_rows,
            },
        );
    }
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: "global.db:integrity_check".to_owned(),
            line: None,
            reason: integrity,
        });
    }
    {
        let mut statement = tx.prepare("PRAGMA foreign_key_check")?;
        if statement.query([])?.next()?.is_some() {
            return Err(StoreError::Corrupt {
                path: "global.db:foreign_key_check".to_owned(),
                line: None,
                reason: "foreign key violations".to_owned(),
            });
        }
    }
    tx.commit()?;
    Ok(MigrationReport {
        from_version: 6,
        to_version: 7,
        backup: backup.display().to_string(),
        projects: Vec::new(),
        tables,
        verified: true,
        dry_run: false,
        dropped_workers: 0,
        dropped_worker_rows: Vec::new(),
        primary_runs: Vec::new(),
        node_wide_loops: Vec::new(),
        node_wide_schedules: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Runs [`migrate_v06`] when an existing home still has the v6 global schema.
///
/// Earlier versions are handled by the previous migrations before this check. A future
/// version is left for the normal open path to refuse with [`StoreError::SchemaTooNew`].
pub(crate) fn migrate_v06_if_needed(paths: &NodePaths) -> Result<(), StoreError> {
    if db_version(&paths.db_path())? == Some(V06_SCHEMA_VERSION) {
        migrate_v06(paths)?;
    }
    Ok(())
}

/// Checks that the source really is a healthy v2 global database.
fn validate_v02_source(paths: &NodePaths) -> Result<(), StoreError> {
    validate_global_source(paths, 2, &V02_GLOBAL_TABLES, "v2")
}

/// Checks that the source really is a healthy v3 global database.
fn validate_v03_source(paths: &NodePaths) -> Result<(), StoreError> {
    validate_global_source(paths, V03_SCHEMA_VERSION, &V03_GLOBAL_TABLES, "v3")
}

/// Checks that the source really is a healthy v4 global database.
fn validate_v04_source(paths: &NodePaths) -> Result<(), StoreError> {
    validate_global_source(paths, V04_SCHEMA_VERSION, &V04_GLOBAL_TABLES, "v4")
}

/// Checks that the source really is a healthy v5 global database.
fn validate_v05_source(paths: &NodePaths) -> Result<(), StoreError> {
    validate_global_source(paths, V05_SCHEMA_VERSION, &V05_GLOBAL_TABLES, "v5")
}

/// Checks that the source really is a healthy v6 global database.
fn validate_v06_source(paths: &NodePaths) -> Result<(), StoreError> {
    let mut tables: Vec<&str> = V05_GLOBAL_TABLES.to_vec();
    tables.extend(V06_NEW_TABLES);
    validate_global_source(paths, V06_SCHEMA_VERSION, &tables, "v6")
}

/// Migrates one project database from v1 to v2 in place.
///
/// The caller holds an open read-write connection to the project file. The file is copied to
/// `backups/project-<name>-pre-0.6-<ms>.db` first; `job_placement` is then created and
/// `user_version = 2` set in one transaction. Every existing table's row count is verified
/// before the commit, so a failure rolls back and leaves the v1 file readable.
pub(crate) fn migrate_project_v1_to_v2(
    paths: &NodePaths,
    project: &ProjectName,
    conn: &Connection,
) -> Result<(), StoreError> {
    let db_path = paths.project_db_path(project);
    validate_project_v1_source(conn, &db_path)?;
    let backup = paths.backups_dir().join(format!(
        "project-{}-pre-0.6-{}.db",
        project.as_str(),
        Timestamp::now().unix_millis()
    ));
    std::fs::create_dir_all(paths.backups_dir()).map_err(|err| StoreError::io(paths.backups_dir().display(), err))?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    conn.backup(rusqlite::MAIN_DB, &backup, None)?;
    sync_dir(&paths.backups_dir())?;

    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let before = table_counts(&tx, "main")?;
    tx.execute_batch(PROJECT_V2_DDL)?;
    tx.pragma_update(None, "user_version", PROJECT_SCHEMA_VERSION)?;
    let after = table_counts(&tx, "main")?;
    for (table, before_rows) in &before {
        let after_rows = after.get(table).copied().ok_or_else(|| StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("existing table {table} disappeared during the v2 migration"),
        })?;
        if after_rows != *before_rows {
            return Err(StoreError::Corrupt {
                path: db_path.display().to_string(),
                line: None,
                reason: format!("{table} row count changed: before {before_rows}, after {after_rows}"),
            });
        }
    }
    let placement_rows = after.get("job_placement").copied().ok_or_else(|| StoreError::Corrupt {
        path: db_path.display().to_string(),
        line: None,
        reason: "job_placement is missing after the v2 migration".to_owned(),
    })?;
    if placement_rows != 0 {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("new job_placement table is not empty: {placement_rows} rows"),
        });
    }
    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("integrity_check: {integrity}"),
        });
    }
    {
        let mut statement = tx.prepare("PRAGMA foreign_key_check")?;
        if statement.query([])?.next()?.is_some() {
            return Err(StoreError::Corrupt {
                path: db_path.display().to_string(),
                line: None,
                reason: "foreign key violations".to_owned(),
            });
        }
    }
    tx.commit()?;
    Ok(())
}

/// Checks that a project file really is a healthy v1 database.
fn validate_project_v1_source(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
        return Err(StoreError::InvalidInput(format!(
            "project migration source has schema version {version}, expected 1"
        )));
    }
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: path.display().to_string(),
            line: None,
            reason: format!("integrity_check: {integrity}"),
        });
    }
    for table in PROJECT_V1_TABLES {
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        if found == 0 {
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("v1 project schema is missing table {table}"),
            });
        }
    }
    let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::Corrupt {
            path: path.display().to_string(),
            line: None,
            reason: "foreign key violations in the v1 project database".to_owned(),
        });
    }
    Ok(())
}

/// Checks a global file's version, integrity, WAL mode, tables and foreign keys.
fn validate_global_source(paths: &NodePaths, expected: i64, tables: &[&str], label: &str) -> Result<(), StoreError> {
    let db_path = paths.db_path();
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != expected {
        return Err(StoreError::InvalidInput(format!(
            "migration source has schema version {version}, expected {expected}"
        )));
    }
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("integrity_check: {integrity}"),
        });
    }
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") {
        return Err(StoreError::InvalidInput(format!(
            "migration requires journal_mode=WAL, found {journal}"
        )));
    }
    let mut missing = Vec::new();
    for table in tables {
        let found: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        if found == 0 {
            missing.push(*table);
        }
    }
    if !missing.is_empty() {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("{label} schema is missing tables: {}", missing.join(", ")),
        });
    }
    let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("foreign key violations in the {label} database"),
        });
    }
    Ok(())
}

/// Copies a global file to the backup path and fsyncs the directory.
fn backup_global(paths: &NodePaths, backup: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(paths.backups_dir()).map_err(|err| StoreError::io(paths.backups_dir().display(), err))?;
    let conn = Connection::open(paths.db_path())?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    conn.backup(rusqlite::MAIN_DB, backup, None)?;
    sync_dir(&paths.backups_dir())
}

/// Builds the staged v3 database and returns per-table counts.
///
/// The embedded schema is v7, so the stage is first shaped back to the v3 `surfaces`
/// definition (no `state`/`revoked_at`, no `agent` kind, no `surface_tokens`) and the newer
/// tables are dropped. The v2 rows then copy with their original column lists and the next
/// migration brings the stage to v4.
fn create_v02_stage(stage: &Path, paths: &NodePaths) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    std::fs::create_dir_all(stage).map_err(|err| StoreError::io(stage.display(), err))?;
    let stage_db = stage.join("global.db");
    let conn = Connection::open(&stage_db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(GLOBAL_SCHEMA_SQL)?;
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;
         DROP TABLE surface_tokens;
         CREATE TABLE surfaces_v3 (
           id            TEXT PRIMARY KEY,
           kind          TEXT NOT NULL CHECK (kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node          TEXT NOT NULL,
           holder        TEXT NOT NULL,
           app           TEXT,
           cwd           TEXT,
           metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
           created_at    TEXT NOT NULL,
           last_seen_at  TEXT NOT NULL
         ) STRICT;
         DROP TABLE surfaces;
         ALTER TABLE surfaces_v3 RENAME TO surfaces;
         DROP TABLE leases;
         CREATE TABLE leases (
           session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
           surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
           surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node         TEXT NOT NULL,
           holder       TEXT NOT NULL,
           token        TEXT NOT NULL,
           acquired_at  TEXT NOT NULL,
           heartbeat_at TEXT NOT NULL,
           CHECK (length(token) = 52)
         ) STRICT;
         CREATE INDEX leases_heartbeat ON leases(heartbeat_at);",
    )?;
    conn.pragma_update(None, "user_version", V03_SCHEMA_VERSION)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS old",
        [paths.db_path().to_string_lossy().as_ref()],
    )?;
    let tables = copy_v02_global(&conn)?;
    conn.execute_batch("DETACH DATABASE old")?;
    verify_global(&conn)?;
    sync_dir(stage)?;
    Ok(tables)
}

/// Copies every v2 global row into the staged v3 file and checks counts.
fn copy_v02_global(conn: &Connection) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT INTO main.nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at, kind)
           SELECT name, ssh_target, is_local, capabilities, last_seen_at, created_at,
                  CASE WHEN is_local = 1 THEN 'home' ELSE 'ssh' END
           FROM old.nodes;",
    )?;
    for table in V02_GLOBAL_TABLES.iter().filter(|table| **table != "nodes") {
        conn.execute(&format!("INSERT INTO main.{table} SELECT * FROM old.{table}"), [])?;
    }
    conn.execute_batch("COMMIT")?;

    let mut counts = BTreeMap::new();
    for table in V02_GLOBAL_TABLES {
        let before = count_sql(conn, &format!("SELECT COUNT(*) FROM old.{table}"))?;
        let after = count_sql(conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        if before != after {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("row count changed: before {before}, after {after}"),
            });
        }
        counts.insert(format!("global:{table}"), MigrationCount { before, after });
    }
    for table in V03_NEW_TABLES {
        let after = count_sql(conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        if after != 0 {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("new table is not empty: {after} rows"),
            });
        }
        counts.insert(format!("global:{table}"), MigrationCount { before: 0, after });
    }
    let local_wrong = count_sql(
        conn,
        "SELECT COUNT(*) FROM main.nodes WHERE is_local = 1 AND kind <> 'home'",
    )?;
    if local_wrong > 0 {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:nodes.kind".to_owned(),
            line: None,
            reason: format!("{local_wrong} local rows are not kind 'home'"),
        });
    }
    Ok(counts)
}

/// Builds the staged v4 database and returns per-table counts.
fn create_v03_stage(stage: &Path, paths: &NodePaths) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    std::fs::create_dir_all(stage).map_err(|err| StoreError::io(stage.display(), err))?;
    let stage_db = stage.join("global.db");
    let conn = Connection::open(&stage_db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(GLOBAL_SCHEMA_SQL)?;
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;
         DROP TABLE leases;
         CREATE TABLE leases (
           session_id   TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
           surface_id   TEXT REFERENCES surfaces(id) ON DELETE SET NULL,
           surface_kind TEXT NOT NULL CHECK (surface_kind IN ('chat','opencode','claude-code','mcp','mirror','api')),
           node         TEXT NOT NULL,
           holder       TEXT NOT NULL,
           token        TEXT NOT NULL,
           acquired_at  TEXT NOT NULL,
           heartbeat_at TEXT NOT NULL,
           CHECK (length(token) = 52)
         ) STRICT;
         CREATE INDEX leases_heartbeat ON leases(heartbeat_at);",
    )?;
    conn.pragma_update(None, "user_version", V04_SCHEMA_VERSION)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS old",
        [paths.db_path().to_string_lossy().as_ref()],
    )?;
    let tables = copy_v03_global(&conn)?;
    conn.execute_batch("DETACH DATABASE old")?;
    verify_global(&conn)?;
    sync_dir(stage)?;
    Ok(tables)
}

/// Copies every v3 global row into the staged v4 file and checks counts.
///
/// Only `surfaces` needs an explicit column list: the v4 table adds `state` and `revoked_at`.
/// Every other v3 table keeps its definition, so `INSERT ... SELECT *` is exact.
fn copy_v03_global(conn: &Connection) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for table in V03_GLOBAL_TABLES {
        if table == "surfaces" {
            conn.execute(
                "INSERT INTO main.surfaces
                   (id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at)
                 SELECT id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at
                 FROM old.surfaces",
                [],
            )?;
        } else {
            conn.execute(&format!("INSERT INTO main.{table} SELECT * FROM old.{table}"), [])?;
        }
    }
    conn.execute_batch("COMMIT")?;

    let mut counts = BTreeMap::new();
    for table in V03_GLOBAL_TABLES {
        let before = count_sql(conn, &format!("SELECT COUNT(*) FROM old.{table}"))?;
        let after = count_sql(conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        if before != after {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("row count changed: before {before}, after {after}"),
            });
        }
        counts.insert(format!("global:{table}"), MigrationCount { before, after });
    }
    for table in V04_NEW_TABLES {
        let after = count_sql(conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        if after != 0 {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("new table is not empty: {after} rows"),
            });
        }
        counts.insert(format!("global:{table}"), MigrationCount { before: 0, after });
    }
    let unexpected_state = count_sql(
        conn,
        "SELECT COUNT(*) FROM main.surfaces WHERE state <> 'attached' OR revoked_at IS NOT NULL",
    )?;
    if unexpected_state > 0 {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:surfaces.state".to_owned(),
            line: None,
            reason: format!("{unexpected_state} migrated rows are not attached"),
        });
    }
    Ok(counts)
}

/// Builds the staged v5 database and returns per-table counts.
///
/// The embedded schema is v7, so the stage first drops the v6-only and v7-only tables and sets
/// `user_version = 5`. Every v4 row is copied except `leases`: a v4 lease token is plaintext
/// and leases are ephemeral, so the upgrade clears them and lets surfaces re-acquire.
fn create_v04_stage(stage: &Path, paths: &NodePaths) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    std::fs::create_dir_all(stage).map_err(|err| StoreError::io(stage.display(), err))?;
    let stage_db = stage.join("global.db");
    let conn = Connection::open(&stage_db)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(GLOBAL_SCHEMA_SQL)?;
    conn.execute_batch(
        "DROP TABLE agent_assignments;
         DROP TABLE node_limits;
         DROP TABLE capacity_rollups;",
    )?;
    conn.pragma_update(None, "user_version", V05_SCHEMA_VERSION)?;
    conn.execute(
        "ATTACH DATABASE ?1 AS old",
        [paths.db_path().to_string_lossy().as_ref()],
    )?;
    let tables = copy_v04_global(&conn)?;
    conn.execute_batch("DETACH DATABASE old")?;
    verify_global(&conn)?;
    sync_dir(stage)?;
    Ok(tables)
}

/// Copies every v4 global row except leases into the staged v5 file and checks counts.
fn copy_v04_global(conn: &Connection) -> Result<BTreeMap<String, MigrationCount>, StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    for table in V04_GLOBAL_TABLES.iter().filter(|table| **table != "leases") {
        conn.execute(&format!("INSERT INTO main.{table} SELECT * FROM old.{table}"), [])?;
    }
    conn.execute_batch("COMMIT")?;

    let mut counts = BTreeMap::new();
    for table in V04_GLOBAL_TABLES {
        let before = count_sql(conn, &format!("SELECT COUNT(*) FROM old.{table}"))?;
        let after = count_sql(conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        if table == "leases" {
            if after != 0 {
                return Err(StoreError::Corrupt {
                    path: "stage/global.db:leases".to_owned(),
                    line: None,
                    reason: format!("cleared lease table is not empty: {after} rows"),
                });
            }
        } else if before != after {
            return Err(StoreError::Corrupt {
                path: format!("stage/global.db:{table}"),
                line: None,
                reason: format!("row count changed: before {before}, after {after}"),
            });
        }
        counts.insert(format!("global:{table}"), MigrationCount { before, after });
    }
    let plaintext = count_sql(
        conn,
        "SELECT COUNT(*) FROM pragma_table_info('leases') WHERE name = 'token'",
    )?;
    if plaintext != 0 {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:leases".to_owned(),
            line: None,
            reason: "leases still declares a plaintext token column".to_owned(),
        });
    }
    Ok(counts)
}

fn swap_global_from_stage(paths: &NodePaths, stage: &Path) -> Result<(), StoreError> {
    let staged_global = stage.join("global.db");
    if !staged_global.is_file() {
        return Err(StoreError::Corrupt {
            path: staged_global.display().to_string(),
            line: None,
            reason: "staged v3 database is missing".to_owned(),
        });
    }
    if paths.db_path().is_file() {
        let old = stage.join("old.db");
        std::fs::rename(paths.db_path(), &old).map_err(|err| StoreError::io(old.display(), err))?;
    }
    // A stale WAL must never be replayed into the new file; the backup copy already captured
    // every committed page before the swap.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", paths.db_path().display()));
        let _ = std::fs::remove_file(sidecar);
    }
    std::fs::rename(&staged_global, paths.db_path()).map_err(|err| StoreError::io(paths.db_path().display(), err))?;
    sync_dir(paths.root())?;
    let _ = std::fs::remove_dir_all(stage);
    Ok(())
}

/// Rolls a split home back to the v0.1 backup produced by [`migrate_v01`].
pub fn migrate_rollback(paths: &NodePaths, backup: &Path) -> Result<MigrationReport, StoreError> {
    refuse_live_supervisor(paths)?;
    if !backup.is_file() {
        return Err(StoreError::not_found("migration backup", backup.display().to_string()));
    }
    let stamp = Timestamp::now().unix_millis();
    let failed = paths.backups_dir().join(format!("failed-0.2-{stamp}.db"));
    let projects_backup = paths.backups_dir().join(format!("0.2-projects-{stamp}"));
    if paths.db_path().is_file() {
        std::fs::rename(paths.db_path(), &failed).map_err(|err| StoreError::io(failed.display(), err))?;
    }
    std::fs::copy(backup, paths.db_path()).map_err(|err| StoreError::io(paths.db_path().display(), err))?;
    if paths.projects_dir().is_dir() {
        std::fs::rename(paths.projects_dir(), &projects_backup)
            .map_err(|err| StoreError::io(projects_backup.display(), err))?;
    }
    let _ = std::fs::remove_file(paths.migration_manifest_path());
    sync_dir(paths.root())?;
    Ok(MigrationReport {
        from_version: 2,
        to_version: 1,
        backup: backup.display().to_string(),
        projects: Vec::new(),
        tables: BTreeMap::new(),
        verified: true,
        dry_run: false,
        dropped_workers: 0,
        dropped_worker_rows: Vec::new(),
        primary_runs: Vec::new(),
        node_wide_loops: Vec::new(),
        node_wide_schedules: Vec::new(),
        warnings: vec![format!("current split database moved to {}", failed.display())],
    })
}

/// Finishes or cleans up a migration interrupted at a recorded step.
pub fn resume_migration(paths: &NodePaths) -> Result<(), StoreError> {
    let manifest_path = paths.migration_manifest_path();
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(StoreError::io(manifest_path.display(), err)),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|err| StoreError::Corrupt {
        path: manifest_path.display().to_string(),
        line: None,
        reason: format!("invalid migration manifest: {err}"),
    })?;
    let stage = PathBuf::from(&manifest.stage);
    match manifest.state.as_str() {
        "copying" => {
            if stage.starts_with(paths.tmp_dir()) {
                let _ = std::fs::remove_dir_all(&stage);
            }
            let _ = std::fs::remove_file(&manifest_path);
            Ok(())
        }
        "swapping" => {
            let version = db_version(&paths.db_path())?;
            if manifest.to >= 3 {
                if version == Some(manifest.to) {
                    let _ = std::fs::remove_dir_all(&stage);
                } else {
                    swap_global_from_stage(paths, &stage)?;
                }
            } else if version.is_some_and(|version| version >= 2) {
                // The v0.1 migration writes the current embedded schema, so a completed swap
                // may already report a later version here.
                finish_swap(paths, &stage)?;
            } else {
                swap_from_stage(paths, &stage, Path::new(&manifest.backup))?;
            }
            let _ = std::fs::remove_file(&manifest_path);
            Ok(())
        }
        "done" => {
            let _ = std::fs::remove_file(&manifest_path);
            Ok(())
        }
        other => Err(StoreError::Corrupt {
            path: manifest_path.display().to_string(),
            line: None,
            reason: format!("unknown migration state {other:?}"),
        }),
    }
}

/// Validates the v0.1 home and computes every count the migration must preserve.
fn preflight(paths: &NodePaths) -> Result<Source, StoreError> {
    refuse_live_supervisor(paths)?;
    let db_path = paths.db_path();
    if !db_path.is_file() {
        return Err(StoreError::not_found("v0.1 database", db_path.display().to_string()));
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
        return Err(StoreError::InvalidInput(format!(
            "migration source has schema version {version}, expected 1"
        )));
    }
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: db_path.display().to_string(),
            line: None,
            reason: format!("integrity_check: {integrity}"),
        });
    }
    let journal: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") {
        return Err(StoreError::InvalidInput(format!(
            "migration requires journal_mode=WAL, found {journal}"
        )));
    }

    let projects = read_projects(&conn)?;
    let known: HashSet<String> = projects
        .iter()
        .map(|project| project.name.as_str().to_owned())
        .collect();
    if known.contains(crate::RESERVED_NODE_PROJECT) {
        return Err(StoreError::InvalidInput(
            "the v0.1 registry already contains the reserved _node project".to_owned(),
        ));
    }
    validate_source(&conn, &known)?;
    let primary = primary_project(&conn, &projects)?;
    let (worker_assignments, dropped_worker_rows) = worker_plan(&conn, &primary, &known)?;
    let counts = source_counts(&conn, &projects, &primary, &worker_assignments)?;
    let primary_runs = string_column(&conn, "SELECT id FROM runs WHERE job_id IS NULL ORDER BY id")?;
    let node_wide_loops = string_column(&conn, "SELECT name FROM loops WHERE project IS NULL ORDER BY name")?;
    let node_wide_schedules = string_column(&conn, "SELECT name FROM schedules WHERE project IS NULL ORDER BY name")?;
    let backup = paths
        .backups_dir()
        .join(format!("pre-0.2-{}.db", Timestamp::now().unix_millis()));
    Ok(Source {
        projects,
        primary,
        backup,
        counts,
        dropped_worker_rows,
        primary_runs,
        node_wide_loops,
        node_wide_schedules,
    })
}

/// Returns the live supervisor pid recorded in `<home>/supervisor.lock`.
///
/// A missing lock, an unparsable first line or a pid that is no longer alive all mean no
/// supervisor holds the home. The caller decides whether this process *is* that supervisor.
pub(crate) fn live_supervisor_pid(paths: &NodePaths) -> Result<Option<u32>, StoreError> {
    let path = paths.supervisor_lock_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(StoreError::io(path.display(), err)),
    };
    let Some(pid) = text.lines().next().and_then(|line| line.trim().parse::<u32>().ok()) else {
        return Ok(None);
    };
    Ok((valid_pid(pid) && pid_alive(pid)).then_some(pid))
}

/// Refuses to migrate while a live supervisor holds the home.
fn refuse_live_supervisor(paths: &NodePaths) -> Result<(), StoreError> {
    if let Some(pid) = live_supervisor_pid(paths)? {
        return Err(StoreError::Conflict(format!(
            "supervisor pid {pid} is running; stop it before migrating"
        )));
    }
    Ok(())
}

/// Reads the v0.1 registry.
fn read_projects(conn: &Connection) -> Result<Vec<SourceProject>, StoreError> {
    let mut statement = conn.prepare("SELECT name FROM projects ORDER BY name")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut projects = Vec::new();
    for row in rows {
        projects.push(SourceProject {
            name: ProjectName::parse(&row?)?,
        });
    }
    Ok(projects)
}

/// Rejects rows that name a project with no registry row or cross projects.
fn validate_source(conn: &Connection, known: &HashSet<String>) -> Result<(), StoreError> {
    for table in ["jobs", "releases"] {
        let unknown = count_sql(
            conn,
            &format!(
                "SELECT COUNT(*) FROM {table} t
                 LEFT JOIN projects p ON t.project = p.name WHERE p.name IS NULL"
            ),
        )?;
        if unknown > 0 {
            return Err(StoreError::InvalidInput(format!(
                "{table} has {unknown} rows naming an unregistered project"
            )));
        }
    }
    for table in ["loops", "schedules", "inbox"] {
        let unknown = count_sql(
            conn,
            &format!(
                "SELECT COUNT(*) FROM {table} t
                 LEFT JOIN projects p ON t.project = p.name
                 WHERE t.project IS NOT NULL AND p.name IS NULL"
            ),
        )?;
        if unknown > 0 {
            return Err(StoreError::InvalidInput(format!(
                "{table} has {unknown} rows naming an unregistered project"
            )));
        }
    }
    let bad_inbox = count_sql(
        conn,
        "SELECT COUNT(*) FROM inbox i
         LEFT JOIN jobs j ON i.job_id = j.id
         WHERE i.job_id IS NOT NULL AND j.id IS NULL",
    )?;
    if bad_inbox > 0 {
        return Err(StoreError::InvalidInput(format!(
            "inbox has {bad_inbox} rows naming a missing job"
        )));
    }
    let cross_edges = count_sql(
        conn,
        "SELECT COUNT(*) FROM edges e
         JOIN jobs s ON e.src = s.id
         JOIN jobs d ON e.dst = d.id
         WHERE s.project <> d.project",
    )?;
    if cross_edges > 0 {
        return Err(StoreError::InvalidInput(format!(
            "edges has {cross_edges} cross-project rows"
        )));
    }
    let bad_project = count_sql(
        conn,
        "SELECT COUNT(*) FROM jobs j
         LEFT JOIN projects p ON j.project = p.name
         WHERE p.name IS NULL",
    )?;
    if bad_project > 0 && known.is_empty() {
        return Err(StoreError::InvalidInput(format!(
            "jobs has {bad_project} rows but the registry is empty"
        )));
    }
    Ok(())
}

/// Picks the project with the most jobs; ties are lexicographic. With no real project the
/// reserved `_node` project is primary.
fn primary_project(conn: &Connection, projects: &[SourceProject]) -> Result<ProjectName, StoreError> {
    let mut best: Option<(String, u64)> = None;
    let mut statement = conn.prepare("SELECT project, COUNT(*) FROM jobs GROUP BY project ORDER BY project")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
    for row in rows {
        let (project, count) = row?;
        let count = u64::try_from(count).unwrap_or(0);
        if best.as_ref().is_none_or(|(_, best_count)| count > *best_count) {
            best = Some((project, count));
        }
    }
    if let Some((project, _)) = best {
        return Ok(ProjectName::parse(&project)?);
    }
    if let Some(first) = projects.first() {
        return Ok(first.name.clone());
    }
    Ok(ProjectName::parse(crate::RESERVED_NODE_PROJECT)?)
}

/// Counts the workers each project receives and the rows that cannot be assigned.
fn worker_plan(
    conn: &Connection,
    primary: &ProjectName,
    known: &HashSet<String>,
) -> Result<(HashMap<String, u64>, Vec<String>), StoreError> {
    let mut assignments = HashMap::new();
    let mut total = 0_u64;
    for project in known {
        let assigned = count_sql(
            conn,
            &format!(
                "SELECT COUNT(*) FROM workers w
                 LEFT JOIN jobs j ON w.job_id = j.id
                 LEFT JOIN runs r ON w.run_id = r.id
                 LEFT JOIN jobs rj ON r.job_id = rj.id
                 WHERE j.project = '{project}'
                    OR (w.job_id IS NULL AND rj.project = '{project}')
                    OR (w.job_id IS NULL AND w.run_id IS NULL AND '{project}' = '{}')",
                primary.as_str()
            ),
        )?;
        total += assigned;
        assignments.insert(project.clone(), assigned);
    }
    if primary.as_str() == crate::RESERVED_NODE_PROJECT {
        let idle = count_sql(
            conn,
            "SELECT COUNT(*) FROM workers WHERE job_id IS NULL AND run_id IS NULL",
        )?;
        total += idle;
        assignments.insert(crate::RESERVED_NODE_PROJECT.to_owned(), idle);
    }
    let workers = count_sql(conn, "SELECT COUNT(*) FROM workers")?;
    let dropped = workers.saturating_sub(total);
    let mut dropped_rows = string_column(
        conn,
        "SELECT w.node || ':' || w.kind || ':' || w.slot FROM workers w
         LEFT JOIN jobs j ON w.job_id = j.id
         LEFT JOIN runs r ON w.run_id = r.id
         LEFT JOIN jobs rj ON r.job_id = rj.id
         WHERE NOT (
             j.project IS NOT NULL
             OR (w.job_id IS NULL AND rj.project IS NOT NULL)
             OR (w.job_id IS NULL AND w.run_id IS NULL)
         )
         ORDER BY w.node, w.kind, w.slot",
    )?;
    dropped_rows.truncate(dropped as usize);
    Ok((assignments, dropped_rows))
}

/// Counts every source table and every split destination family.
fn source_counts(
    conn: &Connection,
    projects: &[SourceProject],
    primary: &ProjectName,
    worker_assignments: &HashMap<String, u64>,
) -> Result<BTreeMap<String, u64>, StoreError> {
    let mut counts = BTreeMap::new();
    for table in [
        "kv",
        "nodes",
        "projects",
        "sessions",
        "session_events",
        "session_natives",
        "capsules",
        "switches",
        "limits",
    ] {
        counts.insert(
            format!("global:{table}"),
            count_sql(conn, &format!("SELECT COUNT(*) FROM {table}"))?,
        );
    }
    counts.insert(
        "global:node_inbox".to_owned(),
        count_sql(conn, "SELECT COUNT(*) FROM inbox WHERE project IS NULL")?,
    );
    counts.insert(
        "global:node_ledger".to_owned(),
        count_sql(conn, "SELECT COUNT(*) FROM ledger WHERE project IS NULL")?,
    );
    let mut destinations: Vec<String> = projects
        .iter()
        .map(|project| project.name.as_str().to_owned())
        .collect();
    destinations.push(crate::RESERVED_NODE_PROJECT.to_owned());
    for name in destinations {
        let is_node = name == crate::RESERVED_NODE_PROJECT;
        for table in [
            "jobs",
            "edges",
            "runs",
            "gate_results",
            "workers",
            "loops",
            "schedules",
            "inbox",
            "ledger",
            "releases",
        ] {
            let before = if is_node {
                if primary.as_str() != crate::RESERVED_NODE_PROJECT {
                    0
                } else {
                    match table {
                        "runs" => count_sql(conn, "SELECT COUNT(*) FROM runs WHERE job_id IS NULL")?,
                        "loops" | "schedules" => {
                            count_sql(conn, &format!("SELECT COUNT(*) FROM {table} WHERE project IS NULL"))?
                        }
                        "workers" => count_sql(
                            conn,
                            "SELECT COUNT(*) FROM workers WHERE job_id IS NULL AND run_id IS NULL",
                        )?,
                        _ => 0,
                    }
                }
            } else {
                match table {
                    "edges" => count_sql(
                        conn,
                        &format!("SELECT COUNT(*) FROM edges e JOIN jobs j ON e.src = j.id WHERE j.project = '{name}'"),
                    )?,
                    "gate_results" => count_sql(
                        conn,
                        &format!(
                            "SELECT COUNT(*) FROM gate_results g JOIN jobs j ON g.job_id = j.id WHERE j.project = '{name}'"
                        ),
                    )?,
                    "runs" => count_sql(
                        conn,
                        &format!(
                            "SELECT COUNT(*) FROM runs r LEFT JOIN jobs j ON r.job_id = j.id
                             WHERE j.project = '{name}' OR (r.job_id IS NULL AND '{name}' = '{}')",
                            primary.as_str()
                        ),
                    )?,
                    "workers" => *worker_assignments.get(&name).unwrap_or(&0),
                    "loops" | "schedules" => count_sql(
                        conn,
                        &format!(
                            "SELECT COUNT(*) FROM {table} WHERE project = '{name}' OR (project IS NULL AND '{name}' = '{}')",
                            primary.as_str()
                        ),
                    )?,
                    _ => count_sql(conn, &format!("SELECT COUNT(*) FROM {table} WHERE project = '{name}'"))?,
                }
            };
            counts.insert(format!("project:{name}:{table}"), before);
        }
    }
    Ok(counts)
}

/// Writes the v0.1 file to the backup path and fsyncs the directory.
fn backup_v01(paths: &NodePaths, backup: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(paths.backups_dir()).map_err(|err| StoreError::io(paths.backups_dir().display(), err))?;
    let conn = Connection::open(paths.db_path())?;
    conn.backup(rusqlite::MAIN_DB, backup, None)?;
    sync_dir(&paths.backups_dir())
}

/// Creates the staging global and project databases.
fn create_stage(stage: &Path, paths: &NodePaths, source: &Source) -> Result<(), StoreError> {
    let projects_dir = stage.join("projects");
    std::fs::create_dir_all(&projects_dir).map_err(|err| StoreError::io(projects_dir.display(), err))?;
    let stage_global = stage.join("global.db");
    let conn = Connection::open(&stage_global)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(crate::db::GLOBAL_SCHEMA_SQL)?;
    drop(conn);

    for name in source
        .projects
        .iter()
        .map(|project| project.name.clone())
        .chain(std::iter::once(ProjectName::parse(crate::RESERVED_NODE_PROJECT)?))
    {
        let dir = projects_dir.join(name.as_str());
        std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
        let conn = Connection::open(dir.join("project.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(PROJECT_SCHEMA)?;
    }
    sync_dir(&paths.tmp_dir())?;
    Ok(())
}

/// Copies the source rows into the staged files and verifies them.
fn copy_and_verify(paths: &NodePaths, stage: &Path, source: &Source) -> Result<MigrationReport, StoreError> {
    let stage_global = stage.join("global.db");
    let conn = Connection::open(&stage_global)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute(
        "ATTACH DATABASE ?1 AS old",
        [paths.db_path().to_string_lossy().as_ref()],
    )?;
    copy_global(&conn, source, paths)?;

    let mut tables = BTreeMap::new();
    // Global counts.
    for table in [
        "kv",
        "nodes",
        "projects",
        "sessions",
        "session_events",
        "session_natives",
        "capsules",
        "switches",
        "limits",
        "node_inbox",
        "node_ledger",
    ] {
        let before = *source.counts.get(&format!("global:{table}")).unwrap_or(&0);
        let after = count_sql(&conn, &format!("SELECT COUNT(*) FROM main.{table}"))?;
        tables.insert(format!("global:{table}"), MigrationCount { before, after });
    }
    tables.insert(
        "workers_dropped".to_owned(),
        MigrationCount {
            before: u64::try_from(source.dropped_worker_rows.len()).unwrap_or(u64::MAX),
            after: u64::try_from(source.dropped_worker_rows.len()).unwrap_or(u64::MAX),
        },
    );

    let destinations: Vec<ProjectName> = source
        .projects
        .iter()
        .map(|project| project.name.clone())
        .chain(std::iter::once(ProjectName::parse(crate::RESERVED_NODE_PROJECT)?))
        .collect();
    for name in destinations {
        conn.execute(
            "ATTACH DATABASE ?1 AS project",
            [stage
                .join("projects")
                .join(name.as_str())
                .join("project.db")
                .to_string_lossy()
                .as_ref()],
        )?;
        conn.execute_batch("BEGIN IMMEDIATE; PRAGMA defer_foreign_keys = ON;")?;
        copy_project(&conn, &name, &source.primary)?;
        conn.execute_batch("COMMIT")?;

        for table in [
            "jobs",
            "edges",
            "runs",
            "gate_results",
            "workers",
            "loops",
            "schedules",
            "inbox",
            "ledger",
            "releases",
        ] {
            let before = *source.counts.get(&format!("project:{name}:{table}")).unwrap_or(&0);
            let after = count_sql(&conn, &format!("SELECT COUNT(*) FROM project.{table}"))?;
            tables.insert(format!("project:{name}:{table}"), MigrationCount { before, after });
        }
        verify_project(&conn, &name, &stage_global)?;
        conn.execute_batch("DETACH DATABASE project")?;
    }

    verify_global(&conn)?;
    conn.execute_batch("DETACH DATABASE old")?;
    for (key, count) in &tables {
        let expected = if key == "global:projects" {
            count.before + 1
        } else {
            count.before
        };
        if count.after != expected {
            return Err(StoreError::Corrupt {
                path: key.clone(),
                line: None,
                reason: format!(
                    "row count changed: before {}, after {} (expected {expected})",
                    count.before, count.after
                ),
            });
        }
    }
    Ok(MigrationReport {
        from_version: 1,
        to_version: 2,
        backup: source.backup.display().to_string(),
        projects: source.projects.iter().map(|project| project.name.clone()).collect(),
        tables,
        verified: true,
        dry_run: false,
        dropped_workers: u32::try_from(source.dropped_worker_rows.len()).unwrap_or(u32::MAX),
        dropped_worker_rows: source.dropped_worker_rows.clone(),
        primary_runs: source.primary_runs.clone(),
        node_wide_loops: source.node_wide_loops.clone(),
        node_wide_schedules: source.node_wide_schedules.clone(),
        warnings: if source.dropped_worker_rows.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "{} worker slot rows were unassignable and were dropped: {}",
                source.dropped_worker_rows.len(),
                source.dropped_worker_rows.join(", ")
            )]
        },
    })
}

/// Copies the global tables from the attached `old` database.
fn copy_global(conn: &Connection, _source: &Source, paths: &NodePaths) -> Result<(), StoreError> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT INTO main.kv (key, value, updated_at) SELECT key, value, updated_at FROM old.kv;
         INSERT INTO main.nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at, kind)
           SELECT name, ssh_target, is_local, capabilities, last_seen_at, created_at,
                  CASE WHEN is_local = 1 THEN 'home' ELSE 'ssh' END FROM old.nodes;
         INSERT INTO main.projects (name, path, remote_url, default_branch, config_json,
                                    state, weight, paused_reason, archived_at, created_at, updated_at)
           SELECT name, path, remote_url, default_branch, config_json,
                  'active', 1, '', NULL, created_at, updated_at FROM old.projects;
         INSERT INTO main.sessions (id, project, job_id, role, title, parent_session_id, node, cwd,
                                    state, executor, last_seq, last_capsule_id, log_bytes,
                                    created_at, updated_at)
           SELECT id, project, job_id, role, title, parent_session_id, node, cwd,
                  state, executor, last_seq, last_capsule_id, log_bytes, created_at, updated_at
           FROM old.sessions;
         INSERT INTO main.session_events (session_id, seq, id, ts, type, actor, harness, model,
                                          byte_offset, byte_len, summary)
           SELECT session_id, seq, id, ts, type, actor, harness, model,
                  byte_offset, byte_len, summary FROM old.session_events;
         INSERT INTO main.session_natives (session_id, harness, native_session_id, node, first_seq, created_at)
           SELECT session_id, harness, native_session_id, node, first_seq, created_at FROM old.session_natives;
         INSERT INTO main.capsules (id, session_id, at_seq, version, trigger, author, path, body_json, created_at)
           SELECT id, session_id, at_seq, version, trigger, author, path, body_json, created_at FROM old.capsules;
         INSERT INTO main.switches (id, session_id, from_executor, to_executor, from_node, to_node,
                                    capsule_id, reason, state, error, started_at, ended_at, at_seq)
           SELECT id, session_id, from_executor, to_executor, from_node, to_node, capsule_id,
                  reason, state, error, started_at, ended_at, at_seq FROM old.switches;
         INSERT INTO main.limits (provider, harness, kind, resets_at, detail, since, last_probe_at,
                                  probe_fail_streak, concurrency_cap, cap_until, updated_at)
           SELECT provider, harness, kind, resets_at, detail, since, last_probe_at,
                  probe_fail_streak, concurrency_cap, cap_until, updated_at FROM old.limits;
         INSERT INTO main.node_inbox (id, project, job_id, kind, title, body, options_json,
                                      state, answer, answered_by, created_at, answered_at)
           SELECT i.id,
                  (SELECT j.project FROM old.jobs j WHERE j.id = i.job_id),
                  i.job_id, i.kind, i.title, i.body, i.options_json,
                  i.state, i.answer, i.answered_by, i.created_at, i.answered_at
           FROM old.inbox i WHERE i.project IS NULL;
         INSERT INTO main.node_ledger (id, ts, kind, project, job_id, run_id, session_id, role,
                                       provider, harness, model, usd, tokens_in, tokens_out,
                                       tokens_reasoning, tokens_cache_read, tokens_cache_write,
                                       cost_source, summary, detail_json)
           SELECT id, ts, kind, NULL, job_id, run_id, session_id, role,
                  provider, harness, model, usd, tokens_in, tokens_out, tokens_reasoning,
                  tokens_cache_read, tokens_cache_write, cost_source, summary, detail_json
           FROM old.ledger WHERE old.ledger.project IS NULL;
         COMMIT;",
    )?;
    let now = Timestamp::now();
    conn.execute(
        "INSERT OR REPLACE INTO main.projects
           (name, path, remote_url, default_branch, config_json, state, weight, paused_reason,
            archived_at, created_at, updated_at)
         VALUES (?1, ?2, NULL, 'main', '{}', 'active', 1, '', NULL, ?3, ?3)",
        params![
            crate::RESERVED_NODE_PROJECT,
            paths.root().display().to_string(),
            now.as_rfc3339()
        ],
    )?;
    Ok(())
}

/// Copies one project's rows into the attached `project` database.
fn copy_project(conn: &Connection, project: &ProjectName, primary: &ProjectName) -> Result<(), StoreError> {
    let name = project.as_str();
    let primary = primary.as_str();
    conn.execute(
        "INSERT INTO project.jobs SELECT * FROM old.jobs WHERE project = ?1",
        [name],
    )?;
    conn.execute(
        "INSERT INTO project.edges
         SELECT e.* FROM old.edges e JOIN old.jobs j ON e.src = j.id WHERE j.project = ?1",
        [name],
    )?;
    conn.execute(
        "INSERT INTO project.runs
         SELECT r.* FROM old.runs r LEFT JOIN old.jobs j ON r.job_id = j.id
         WHERE j.project = ?1 OR (r.job_id IS NULL AND ?2 = ?1)",
        params![name, primary],
    )?;
    conn.execute(
        "INSERT INTO project.gate_results
         SELECT g.* FROM old.gate_results g JOIN old.jobs j ON g.job_id = j.id WHERE j.project = ?1",
        [name],
    )?;
    conn.execute(
        "INSERT INTO project.workers
         SELECT w.* FROM old.workers w
         LEFT JOIN old.jobs j ON w.job_id = j.id
         LEFT JOIN old.runs r ON w.run_id = r.id
         LEFT JOIN old.jobs rj ON r.job_id = rj.id
         WHERE j.project = ?1
            OR (w.job_id IS NULL AND rj.project = ?1)
            OR (w.job_id IS NULL AND w.run_id IS NULL AND ?2 = ?1)",
        params![name, primary],
    )?;
    conn.execute(
        "INSERT INTO project.loops
         SELECT * FROM old.loops WHERE project = ?1 OR (project IS NULL AND ?2 = ?1)",
        params![name, primary],
    )?;
    conn.execute(
        "INSERT INTO project.schedules
         SELECT * FROM old.schedules WHERE project = ?1 OR (project IS NULL AND ?2 = ?1)",
        params![name, primary],
    )?;
    conn.execute(
        "INSERT INTO project.inbox SELECT * FROM old.inbox WHERE project = ?1",
        [name],
    )?;
    conn.execute(
        "INSERT INTO project.ledger SELECT * FROM old.ledger WHERE project = ?1",
        [name],
    )?;
    conn.execute(
        "INSERT INTO project.releases SELECT * FROM old.releases WHERE project = ?1",
        [name],
    )?;
    Ok(())
}

/// Checks the staged global file's integrity and cross-database id rules.
fn verify_global(conn: &Connection) -> Result<(), StoreError> {
    let integrity: String = conn.query_row("PRAGMA main.integrity_check", [], |row| row.get(0))?;
    if !integrity.eq_ignore_ascii_case("ok") {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:integrity_check".to_owned(),
            line: None,
            reason: integrity,
        });
    }
    let mut statement = conn.prepare("PRAGMA main.foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:foreign_key_check".to_owned(),
            line: None,
            reason: "foreign key violations".to_owned(),
        });
    }
    let bad = count_sql(
        conn,
        "SELECT COUNT(*) FROM main.node_inbox
         WHERE job_id IS NOT NULL
           AND (project IS NULL OR project NOT IN (SELECT name FROM main.projects))",
    )?;
    if bad > 0 {
        return Err(StoreError::Corrupt {
            path: "stage/global.db:node_inbox".to_owned(),
            line: None,
            reason: format!("{bad} job-bearing rows have no registered project"),
        });
    }
    Ok(())
}

/// Checks one staged project file.
fn verify_project(conn: &Connection, project: &ProjectName, stage_global: &Path) -> Result<(), StoreError> {
    let name = project.as_str();
    let bad_jobs = count_sql(
        conn,
        &format!("SELECT COUNT(*) FROM project.jobs WHERE project <> '{name}'"),
    )?;
    if bad_jobs > 0 {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:jobs"),
            line: None,
            reason: format!("{bad_jobs} rows name another project"),
        });
    }
    let bad_releases = count_sql(
        conn,
        &format!("SELECT COUNT(*) FROM project.releases WHERE project <> '{name}'"),
    )?;
    if bad_releases > 0 {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:releases"),
            line: None,
            reason: format!("{bad_releases} rows name another project"),
        });
    }
    let bad_inbox = count_sql(
        conn,
        "SELECT COUNT(*) FROM project.inbox i
         LEFT JOIN project.jobs j ON i.job_id = j.id
         WHERE i.job_id IS NOT NULL AND j.id IS NULL",
    )?;
    if bad_inbox > 0 {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:inbox"),
            line: None,
            reason: format!("{bad_inbox} rows name a missing job"),
        });
    }
    let bad_workers = count_sql(
        conn,
        "SELECT COUNT(*) FROM project.workers w
         LEFT JOIN project.jobs j ON w.job_id = j.id
         LEFT JOIN project.runs r ON w.run_id = r.id
         WHERE (w.job_id IS NOT NULL AND j.id IS NULL)
            OR (w.run_id IS NOT NULL AND r.id IS NULL)",
    )?;
    if bad_workers > 0 {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:workers"),
            line: None,
            reason: format!("{bad_workers} rows name a missing job or run"),
        });
    }
    for table in [
        "jobs",
        "edges",
        "runs",
        "gate_results",
        "workers",
        "loops",
        "schedules",
        "inbox",
        "ledger",
        "releases",
    ] {
        let mut statement = conn.prepare(&format!("PRAGMA project.foreign_key_list('{table}')"))?;
        let rows = statement.query_map([], |row| row.get::<_, String>(2))?;
        for row in rows {
            let target = row?;
            if target == "projects" || target == "sessions" {
                return Err(StoreError::Corrupt {
                    path: format!("stage/projects/{name}/project.db:{table}"),
                    line: None,
                    reason: format!("project table still references the global {target} table"),
                });
            }
        }
    }
    let mut statement = conn.prepare("PRAGMA project.foreign_key_check")?;
    if statement.query([])?.next()?.is_some() {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:foreign_key_check"),
            line: None,
            reason: "foreign key violations".to_owned(),
        });
    }

    // Runs may name a global session but no project file may hold a cross-file foreign key.
    conn.execute("ATTACH DATABASE ?1 AS g", [stage_global.to_string_lossy().as_ref()])?;
    let bad_runs = count_sql(
        conn,
        "SELECT COUNT(*) FROM project.runs r
         LEFT JOIN g.sessions s ON r.session_id = s.id
         WHERE r.session_id IS NOT NULL AND s.id IS NULL",
    )?;
    conn.execute_batch("DETACH DATABASE g")?;
    if bad_runs > 0 {
        return Err(StoreError::Corrupt {
            path: format!("stage/projects/{name}/project.db:runs"),
            line: None,
            reason: format!("{bad_runs} rows name a missing session"),
        });
    }
    Ok(())
}

/// Renames the staged files into place, recording the intermediate state.
fn swap(paths: &NodePaths, stage: &Path, backup: &Path) -> Result<(), StoreError> {
    write_manifest(
        paths,
        &Manifest {
            from: 1,
            to: 2,
            backup: backup.display().to_string(),
            stage: stage.display().to_string(),
            state: "swapping".to_owned(),
        },
    )?;
    swap_from_stage(paths, stage, backup)?;
    let _ = std::fs::remove_file(paths.migration_manifest_path());
    Ok(())
}

/// Performs the crash-safe directory renames of the swap step.
fn swap_from_stage(paths: &NodePaths, stage: &Path, backup: &Path) -> Result<(), StoreError> {
    if paths.db_path().is_file() {
        std::fs::rename(paths.db_path(), backup).map_err(|err| StoreError::io(backup.display(), err))?;
    }
    let staged_global = stage.join("global.db");
    if staged_global.is_file() {
        std::fs::rename(&staged_global, paths.db_path())
            .map_err(|err| StoreError::io(paths.db_path().display(), err))?;
    }
    install_staged_projects(paths, &stage.join("projects"))?;
    sync_dir(paths.root())?;
    let _ = std::fs::remove_dir_all(stage);
    Ok(())
}

/// Installs the staged `projects/` directory, moving any old directory aside first.
///
/// The move-aside is the crash-safe step §3.9 calls for: a stale `projects/` must never make
/// the staged project databases disappear.
fn install_staged_projects(paths: &NodePaths, staged_projects: &Path) -> Result<(), StoreError> {
    if !staged_projects.exists() {
        return Ok(());
    }
    if paths.projects_dir().exists() {
        std::fs::create_dir_all(paths.backups_dir())
            .map_err(|err| StoreError::io(paths.backups_dir().display(), err))?;
        let mut aside = paths
            .backups_dir()
            .join(format!("projects-before-0.2-{}", Timestamp::now().unix_millis()));
        if aside.exists() {
            aside = paths
                .backups_dir()
                .join(format!("projects-before-0.2-{}", ulid::Ulid::generate()));
        }
        std::fs::rename(paths.projects_dir(), &aside).map_err(|err| StoreError::io(aside.display(), err))?;
    }
    std::fs::rename(staged_projects, paths.projects_dir())
        .map_err(|err| StoreError::io(paths.projects_dir().display(), err))
}

/// Finishes a swap that already put the v2 global file in place.
fn finish_swap(paths: &NodePaths, stage: &Path) -> Result<(), StoreError> {
    let staged_projects = stage.join("projects");
    if staged_projects.exists() {
        install_staged_projects(paths, &staged_projects)?;
    } else if !paths.projects_dir().exists() {
        // The global file is v2 but neither the staged nor the installed project directory
        // exists. Keep the manifest and stage so the caller can roll back instead of losing
        // the only copy.
        return Err(StoreError::MigrationRequired);
    }
    let _ = std::fs::remove_dir_all(stage);
    sync_dir(paths.root())?;
    Ok(())
}

/// Writes the migration manifest with mode 0600 where the platform supports it.
fn write_manifest(paths: &NodePaths, manifest: &Manifest) -> Result<(), StoreError> {
    let path = paths.migration_manifest_path();
    let bytes = serde_json::to_vec_pretty(manifest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| StoreError::io(path.display(), err))?;
        file.write_all(&bytes)
            .map_err(|err| StoreError::io(path.display(), err))?;
        file.sync_all().map_err(|err| StoreError::io(path.display(), err))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, bytes).map_err(|err| StoreError::io(path.display(), err))?;
    }
    Ok(())
}

/// Reads one string column from an internal constant query.
fn string_column(conn: &Connection, sql: &str) -> Result<Vec<String>, StoreError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

/// Counts rows for an internal constant query.
fn count_sql(conn: &Connection, sql: &str) -> Result<u64, StoreError> {
    let value: i64 = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(u64::try_from(value).unwrap_or(0))
}

/// Counts every application table in one attached schema, keyed by table name.
fn table_counts(conn: &Connection, schema: &str) -> Result<BTreeMap<String, u64>, StoreError> {
    let mut statement = conn.prepare(&format!(
        "SELECT name FROM {schema}.sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut names = Vec::new();
    for row in rows {
        names.push(row?);
    }
    drop(statement);
    let mut counts = BTreeMap::new();
    for name in names {
        let quoted = name.replace('"', "\"\"");
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {schema}.\"{quoted}\""), [], |row| {
            row.get(0)
        })?;
        counts.insert(name, u64::try_from(count).unwrap_or(0));
    }
    Ok(counts)
}

/// Reads `PRAGMA user_version` from a file, returning `None` when the file is absent.
fn db_version(path: &Path) -> Result<Option<i64>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .optional()?
        .unwrap_or(0);
    Ok(Some(version))
}

/// Fsyncs a directory entry.
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    let file = std::fs::File::open(dir).map_err(|err| StoreError::io(dir.display(), err))?;
    file.sync_all().map_err(|err| StoreError::io(dir.display(), err))
}

/// Formats a count map for tests and debugging.
#[allow(dead_code)]
fn counts_summary(counts: &BTreeMap<String, u64>) -> String {
    let mut out = String::new();
    for (key, value) in counts {
        let _ = writeln!(out, "{key}={value}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_state_names_are_stable() {
        let manifest = Manifest {
            from: 1,
            to: 2,
            backup: "/b".to_owned(),
            stage: "/s".to_owned(),
            state: "copying".to_owned(),
        };
        let json = serde_json::to_string(&manifest).expect("json");
        assert!(json.contains("\"state\":\"copying\""));
    }
}
