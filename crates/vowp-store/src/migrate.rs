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

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use vowp_protocol::{ProjectName, Timestamp};

use crate::db::PROJECT_SCHEMA;
use crate::error::StoreError;
use crate::paths::NodePaths;
use crate::session_log::{pid_alive, valid_pid};

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
            if version == Some(2) {
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

/// Refuses to migrate while a live supervisor holds the home.
fn refuse_live_supervisor(paths: &NodePaths) -> Result<(), StoreError> {
    let path = paths.supervisor_lock_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(StoreError::io(path.display(), err)),
    };
    if let Some(pid) = text.lines().next().and_then(|line| line.trim().parse::<u32>().ok())
        && valid_pid(pid)
        && pid_alive(pid)
    {
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
         INSERT INTO main.nodes (name, ssh_target, is_local, capabilities, last_seen_at, created_at)
           SELECT name, ssh_target, is_local, capabilities, last_seen_at, created_at FROM old.nodes;
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
