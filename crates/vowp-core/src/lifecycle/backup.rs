//! Home backup and restore.
//!
//! One backup is one directory (or, for the legacy `--to <file>.db` form, one global-database
//! file). A directory backup contains the global database, every project database, the
//! authoritative session content and an informational catalog file, with a `manifest.json`
//! carrying SHA-256 hashes and row counts.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.10 and §10.3.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Date, Month};
use vowp_protocol::{BackupFile, BackupReport, Project, ProjectName, SessionId, Timestamp};
use vowp_store::{JobFilter, LedgerFilter, NodePaths, SessionFilter, Store, StoreError};

use crate::CoreError;
use crate::deps::{CommandOutput, CommandSpec, Deps};

const BACKUP_FORMAT: &str = "vowp-backup/1";
const SESSION_LOG: &str = "log.jsonl";
const AGE_BINARY: &str = "age";
const RSYNC_BINARY: &str = "rsync";
const GIT_BINARY: &str = "git";
const TOOL_TIMEOUT: Duration = Duration::from_secs(1800);

/// Options for `vowp backup` and `vowp backup --restore`.
#[derive(Debug, Clone, Default)]
pub struct BackupOptions {
    /// Destination directory or single-DB file.
    pub to: Option<String>,
    /// Explicit offsite target.
    pub offsite: Option<String>,
    /// `age` recipient, when the backup is encrypted.
    pub encrypt: Option<String>,
    /// Backup directory to restore.
    pub restore: Option<PathBuf>,
    /// Fresh home to restore into.
    pub to_home: Option<PathBuf>,
    /// Overwrite an existing restore target.
    pub force: bool,
}

/// One file written into a backup directory, before encryption turns it into a `.age` file.
#[derive(Debug, Clone)]
struct ContentFile {
    path: String,
    bytes: u64,
    kind: String,
    sha256: String,
}

/// The backup manifest written next to the content files.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    format: String,
    at: Timestamp,
    version: String,
    global_schema: i64,
    project_schema: i64,
    files: Vec<ManifestFile>,
    row_counts: RowCounts,
    skipped: Vec<String>,
    encrypted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipient: Option<String>,
}

/// One content file in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    bytes: u64,
    kind: String,
    sha256: String,
}

/// Row counts for one database, keyed by table name.
type Counts = BTreeMap<String, u64>;

/// Row counts for every project database, keyed by project name.
type ProjectCounts = BTreeMap<String, Counts>;

/// Per-database row counts used by the restore drill.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RowCounts {
    global: Counts,
    projects: ProjectCounts,
}

/// Writes one backup.
pub fn backup(deps: &Deps, store: &Store, opts: &BackupOptions) -> Result<BackupReport, CoreError> {
    if opts.restore.is_some() || opts.to_home.is_some() {
        return Err(CoreError::Invalid(
            "backup cannot be combined with --restore/--to-home; use the restore form".to_owned(),
        ));
    }
    if opts.force {
        return Err(CoreError::Invalid("--force is only valid with --restore".to_owned()));
    }
    if let Some(recipient) = opts.encrypt.as_deref()
        && recipient.trim().is_empty()
    {
        return Err(CoreError::Invalid(
            "--encrypt needs a non-empty age recipient".to_owned(),
        ));
    }

    let mut offsite = opts.offsite.clone();
    let mut directory = None;
    let mut legacy_file = None;
    if let Some(to) = &opts.to {
        let path = PathBuf::from(to);
        if is_offsite_target(to) {
            if offsite.is_some() {
                return Err(CoreError::Invalid(
                    "two offsite targets were given; use either --to or --offsite".to_owned(),
                ));
            }
            offsite = Some(to.clone());
        } else if to.ends_with(".db") && !path.is_dir() {
            legacy_file = Some(path);
        } else {
            directory = Some(path);
        }
    }

    if let Some(path) = legacy_file {
        return backup_single_db(deps, store, &path, opts, offsite);
    }

    let now = deps.clock.now();
    let uses_default_destination = directory.is_none();
    let destination = match directory {
        Some(path) => path,
        None => deps.paths.backups_dir().join(compact_timestamp(now)),
    };
    check_destination(&destination)?;
    let staging = staging_path(&destination);
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|error| io_error(&staging, error))?;
    }

    let mut report = match write_full_backup(deps, store, &staging, now, opts.encrypt.as_deref()) {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Err(error) = swap_destination(&staging, &destination) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    report.directory = destination.display().to_string();

    if uses_default_destination {
        rotate_backups(&deps.paths.backups_dir(), &destination)?;
    }

    if let Some(target) = &offsite {
        copy_offsite_dir(deps, &destination, target)?;
        report.offsite = Some(target.clone());
    }
    Ok(report)
}

/// Restores a fresh home from a backup directory.
///
/// The backup is written into a temporary staging home next to `--to-home`, where every
/// hash and row count is validated; only a fully verified home is swapped into place. A
/// corrupt or incomplete backup therefore leaves the target untouched and needs no
/// `--force` retry. Restore reads no live store, so it also works from a machine whose
/// serving home is lost or not initialised.
pub fn restore(deps: &Deps, opts: &BackupOptions) -> Result<BackupReport, CoreError> {
    if opts.to.is_some() || opts.offsite.is_some() || opts.encrypt.is_some() {
        return Err(CoreError::Invalid(
            "restore accepts --restore, --to-home and --force only".to_owned(),
        ));
    }
    let source = opts
        .restore
        .as_ref()
        .ok_or_else(|| CoreError::Invalid("restore needs --restore <backup-dir>".to_owned()))?;
    let to_home = opts
        .to_home
        .as_ref()
        .ok_or_else(|| CoreError::Invalid("restore needs --to-home <fresh-home>".to_owned()))?;
    if source == to_home {
        return Err(CoreError::Invalid(
            "the restore target must be a different directory from the backup".to_owned(),
        ));
    }
    if to_home == deps.paths.root() {
        return Err(CoreError::Store(StoreError::Conflict(
            "refusing to restore over the home that is serving this command; choose a fresh --to-home".to_owned(),
        )));
    }
    if !source.is_dir() {
        return Err(CoreError::NotFound(format!("backup directory {}", source.display())));
    }
    let manifest = read_manifest(source)?;
    check_restore_target(to_home, opts.force)?;

    let staging = sibling_home_path(to_home, "restore");
    let staged = (|| {
        let paths = NodePaths::new(&staging);
        paths.ensure()?;

        let mut files = Vec::new();
        let mut skipped = manifest.skipped.clone();
        for file in &manifest.files {
            let Some(destination) = restore_path(&paths, &file.path) else {
                skipped.push(format!("{}: not part of a node home", file.path));
                continue;
            };
            let source_path = if manifest.encrypted {
                with_suffix(source.join(&file.path), ".age")
            } else {
                source.join(&file.path)
            };
            if !source_path.is_file() {
                return Err(corrupt(
                    source_path.display().to_string(),
                    format!("manifest names a missing file: {}", file.path),
                ));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
            }
            if manifest.encrypted {
                decrypt_file(deps, &source_path, &destination)?;
            } else {
                fs::copy(&source_path, &destination).map_err(|error| io_error(&source_path, error))?;
            }
            let actual = sha256_file(&destination)?;
            if actual != file.sha256 {
                return Err(corrupt(
                    destination.display().to_string(),
                    format!(
                        "sha256 mismatch for {}: expected {}, got {actual}",
                        file.path, file.sha256
                    ),
                ));
            }
            let bytes = fs::metadata(&destination)
                .map_err(|error| io_error(&destination, error))?
                .len();
            files.push(BackupFile {
                path: file.path.clone(),
                bytes,
                kind: file.kind.clone(),
            });
        }

        verify_restored_home(&paths, &manifest)?;
        Ok((files, skipped))
    })();
    let (files, mut skipped) = match staged {
        Ok(staged) => staged,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Err(error) = install_staged_home(&staging, to_home) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    if !skipped.iter().any(|entry| entry.starts_with("secrets/")) {
        skipped.push("secrets/: never present in a backup; restore them with `vowp secret set`".to_owned());
    }
    Ok(BackupReport {
        at: deps.clock.now(),
        directory: source.display().to_string(),
        bytes: files.iter().map(|file| file.bytes).sum(),
        files,
        offsite: None,
        encrypted: manifest.encrypted,
        skipped,
    })
}

/// Writes the legacy single-file backup, which contains the global database only.
fn backup_single_db(
    deps: &Deps,
    store: &Store,
    path: &Path,
    opts: &BackupOptions,
    offsite: Option<String>,
) -> Result<BackupReport, CoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    store.backup(path)?;
    let mut final_path = path.to_path_buf();
    let mut encrypted = false;
    if let Some(recipient) = opts.encrypt.as_deref() {
        if recipient.trim().is_empty() {
            return Err(CoreError::Invalid(
                "--encrypt needs a non-empty age recipient".to_owned(),
            ));
        }
        let encrypted_path = with_suffix(path.to_path_buf(), ".age");
        if let Err(error) = encrypt_file(deps, path, &encrypted_path, recipient) {
            let _ = fs::remove_file(&encrypted_path);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        fs::remove_file(path).map_err(|error| io_error(path, error))?;
        final_path = encrypted_path;
        encrypted = true;
    }
    let bytes = fs::metadata(&final_path)
        .map_err(|error| io_error(&final_path, error))?
        .len();
    let mut report = BackupReport {
        at: deps.clock.now(),
        directory: final_path.display().to_string(),
        bytes,
        files: vec![BackupFile {
            path: final_path.display().to_string(),
            bytes,
            kind: "global-db".to_owned(),
        }],
        offsite: None,
        encrypted,
        skipped: vec![
            "project databases: the legacy --to <file>.db form copies the global database only".to_owned(),
            "session content: the legacy --to <file>.db form does not copy sessions".to_owned(),
            "secrets/: never backed up".to_owned(),
        ],
    };
    if let Some(target) = offsite {
        copy_offsite_file(deps, &final_path, &target)?;
        report.offsite = Some(target);
    }
    Ok(report)
}

/// Writes a complete backup directory.
fn write_full_backup(
    deps: &Deps,
    store: &Store,
    destination: &Path,
    now: Timestamp,
    recipient: Option<&str>,
) -> Result<BackupReport, CoreError> {
    fs::create_dir_all(destination).map_err(|error| io_error(destination, error))?;
    let mut files = Vec::new();
    let mut skipped = vec!["secrets/: never backed up".to_owned()];

    let global_path = destination.join("global.db");
    store.backup(&global_path)?;
    record_file(destination, &global_path, "global-db", &mut files)?;

    let mut project_snapshots = Vec::new();
    for project in projects_from_backup(deps, &global_path)? {
        let source = deps.paths.project_db_path(&project.name);
        if !source.is_file() {
            Store::create_project_db(&deps.paths, &project.name)?;
        }
        let target = destination
            .join("projects")
            .join(format!("{}.db", project.name.as_str()));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        copy_project_db(deps, &project.name, &target)?;
        record_file(destination, &target, "project-db", &mut files)?;
        project_snapshots.push((project.name.clone(), target));
    }

    // Counts must describe the files that were just written. Counting the live store
    // here would race with a running supervisor and make a successful backup unrestorable.
    let (global_counts, project_counts) = counts_from_backup(deps, &global_path, &project_snapshots)?;

    copy_sessions(deps, destination, &mut files, &mut skipped)?;

    let catalog_path = destination.join("catalog.json");
    let catalog = serde_json::json!({
        "format": "vowp-catalog/1",
        "source": "global.db",
        "note": "the catalog table is stored in global.db and restored with it; this file is informational",
    });
    fs::write(&catalog_path, serde_json::to_vec_pretty(&catalog).map_err(json_error)?)
        .map_err(|error| io_error(&catalog_path, error))?;
    record_file(destination, &catalog_path, "catalog", &mut files)?;

    let mut manifest = Manifest {
        format: BACKUP_FORMAT.to_owned(),
        at: now,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        global_schema: store.global_schema_version()?,
        project_schema: vowp_store::PROJECT_SCHEMA_VERSION,
        files: files
            .iter()
            .map(|file| ManifestFile {
                path: file.path.clone(),
                bytes: file.bytes,
                kind: file.kind.clone(),
                sha256: file.sha256.clone(),
            })
            .collect(),
        row_counts: RowCounts {
            global: global_counts,
            projects: project_counts,
        },
        skipped: skipped.clone(),
        encrypted: false,
        recipient: None,
    };
    write_manifest(destination, &manifest)?;

    if let Some(recipient) = recipient {
        if recipient.trim().is_empty() {
            return Err(CoreError::Invalid(
                "--encrypt needs a non-empty age recipient".to_owned(),
            ));
        }
        for file in &files {
            let plain = destination.join(&file.path);
            let sealed = with_suffix(plain.clone(), ".age");
            encrypt_file(deps, &plain, &sealed, recipient)?;
            fs::remove_file(&plain).map_err(|error| io_error(&plain, error))?;
        }
        manifest.encrypted = true;
        manifest.recipient = Some(recipient.to_owned());
        write_manifest(destination, &manifest)?;
    }

    let mut report_files = Vec::new();
    for file in &files {
        let path = if manifest.encrypted {
            with_suffix(destination.join(&file.path), ".age")
        } else {
            destination.join(&file.path)
        };
        let bytes = fs::metadata(&path).map_err(|error| io_error(&path, error))?.len();
        let report_path = if manifest.encrypted {
            format!("{}.age", file.path)
        } else {
            file.path.clone()
        };
        report_files.push(BackupFile {
            path: report_path,
            bytes,
            kind: file.kind.clone(),
        });
    }
    let bytes = report_files.iter().map(|file| file.bytes).sum();
    Ok(BackupReport {
        at: now,
        directory: destination.display().to_string(),
        files: report_files,
        bytes,
        offsite: None,
        encrypted: manifest.encrypted,
        skipped,
    })
}

/// Copies one project database with SQLite's online backup API.
///
/// A project database is attached as `project`; [`Store::backup`] only copies `main`, so
/// [`Store::backup_project`] backs up the attached schema directly. SQLite's online backup
/// sees a consistent snapshot even while a writer is committing and checkpointing.
fn copy_project_db(deps: &Deps, project: &ProjectName, destination: &Path) -> Result<(), CoreError> {
    let store = Store::open_with_project(&deps.paths, project)?;
    let mut last_error = None;
    for attempt in 0..3 {
        match store.backup_project(destination) {
            Ok(()) => return Ok(()),
            Err(StoreError::Busy) if attempt < 2 => {
                last_error = Some(StoreError::Busy);
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_error
        .map(CoreError::from)
        .unwrap_or_else(|| CoreError::Command("project database backup failed".to_owned())))
}

/// Copies the authoritative session files into the backup directory.
fn copy_sessions(
    deps: &Deps,
    destination: &Path,
    files: &mut Vec<ContentFile>,
    skipped: &mut Vec<String>,
) -> Result<(), CoreError> {
    let sessions = deps.paths.sessions_dir();
    let metadata = match fs::symlink_metadata(&sessions) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(&sessions, error)),
    };
    if metadata.file_type().is_symlink() {
        skipped.push(format!("{}: symlink not followed", sessions.display()));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&sessions)
        .map_err(|error| io_error(&sessions, error))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    let backup_sessions = destination.join("sessions");
    entries.sort();
    for session in entries {
        let metadata = fs::symlink_metadata(&session).map_err(|error| io_error(&session, error))?;
        if metadata.file_type().is_symlink() {
            skipped.push(format!("{}: symlink not followed", session.display()));
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        for name in [SESSION_LOG, "adapters.json", "summary.md"] {
            let source = session.join(name);
            if source.is_file() {
                let kind = match name {
                    SESSION_LOG => "session-log",
                    "adapters.json" => "session-adapters",
                    _ => "session-summary",
                };
                let target = backup_sessions.join(relative_posix(&sessions, &source)?);
                copy_and_record(&source, &target, destination, kind, files, skipped)?;
            }
        }
        for (name, kind) in [("blobs", "session-blob"), ("capsules", "session-capsule")] {
            let directory = session.join(name);
            let metadata = match fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(&directory, error)),
            };
            if metadata.file_type().is_symlink() {
                skipped.push(format!("{}: symlink not followed", directory.display()));
                continue;
            }
            if metadata.is_dir() {
                let target = backup_sessions.join(relative_posix(&sessions, &directory)?);
                copy_tree(&directory, &target, destination, kind, files, skipped)?;
            }
        }
    }
    Ok(())
}

/// Recursively copies a directory without following symlinks.
fn copy_tree(
    source: &Path,
    destination: &Path,
    root: &Path,
    kind: &str,
    files: &mut Vec<ContentFile>,
    skipped: &mut Vec<String>,
) -> Result<(), CoreError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_error(source, error))?;
    if metadata.file_type().is_symlink() {
        skipped.push(format!("{}: symlink not followed", source.display()));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(source)
        .map_err(|error| io_error(source, error))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
        if metadata.file_type().is_symlink() {
            skipped.push(format!("{}: symlink not followed", path.display()));
            continue;
        }
        let target = destination.join(
            path.strip_prefix(source)
                .map_err(|_| CoreError::Invalid(format!("cannot relativise {}", path.display())))?,
        );
        if metadata.is_dir() {
            copy_tree(&path, &target, root, kind, files, skipped)?;
        } else if metadata.is_file() {
            copy_and_record(&path, &target, root, kind, files, skipped)?;
        }
    }
    Ok(())
}

/// Copies one file and records its hash and size.
fn copy_and_record(
    source: &Path,
    destination: &Path,
    root: &Path,
    kind: &str,
    files: &mut Vec<ContentFile>,
    skipped: &mut Vec<String>,
) -> Result<(), CoreError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_error(source, error))?;
    if metadata.file_type().is_symlink() {
        skipped.push(format!("{}: symlink not followed", source.display()));
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    fs::copy(source, destination).map_err(|error| io_error(source, error))?;
    record_file(root, destination, kind, files)
}

/// Records one file that is already inside the backup directory.
fn record_file(root: &Path, path: &Path, kind: &str, files: &mut Vec<ContentFile>) -> Result<(), CoreError> {
    let bytes = fs::metadata(path).map_err(|error| io_error(path, error))?.len();
    files.push(ContentFile {
        path: relative_posix(root, path)?,
        bytes,
        kind: kind.to_owned(),
        sha256: sha256_file(path)?,
    });
    Ok(())
}

/// Writes the manifest atomically.
fn write_manifest(destination: &Path, manifest: &Manifest) -> Result<(), CoreError> {
    let path = destination.join("manifest.json");
    let temporary = destination.join("manifest.json.tmp");
    let text = serde_json::to_vec_pretty(manifest).map_err(json_error)?;
    fs::write(&temporary, text).map_err(|error| io_error(&temporary, error))?;
    fs::rename(&temporary, &path).map_err(|error| io_error(&path, error))
}

/// Reads and validates a backup manifest.
fn read_manifest(source: &Path) -> Result<Manifest, CoreError> {
    let path = source.join("manifest.json");
    let bytes = fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CoreError::NotFound(format!("backup manifest {}", path.display()))
        } else {
            io_error(&path, error)
        }
    })?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| corrupt(path.display().to_string(), format!("invalid manifest: {error}")))?;
    if manifest.format != BACKUP_FORMAT {
        return Err(corrupt(
            path.display().to_string(),
            format!("unsupported backup format {:?}", manifest.format),
        ));
    }
    Ok(manifest)
}

/// Checks the restored databases and compares their row counts to the manifest.
fn verify_restored_home(paths: &NodePaths, manifest: &Manifest) -> Result<(), CoreError> {
    let global = Store::open_global(paths)?;
    global.integrity_check()?;
    let actual = global_counts(&global)?;
    compare_counts("global", &manifest.row_counts.global, &actual)?;

    for (name, expected) in &manifest.row_counts.projects {
        let project = ProjectName::parse(name)?;
        let store = Store::open_with_project(paths, &project)?;
        store.integrity_check()?;
        let actual = project_counts_for(&store, &project)?;
        compare_counts(name, expected, &actual)?;
    }
    Ok(())
}

/// Compares two row-count maps.
fn compare_counts(
    label: &str,
    expected: &BTreeMap<String, u64>,
    actual: &BTreeMap<String, u64>,
) -> Result<(), CoreError> {
    for (table, expected) in expected {
        let found = actual.get(table).copied().unwrap_or_default();
        if &found != expected {
            return Err(corrupt(
                label.to_owned(),
                format!("row count for {table} is {found}, manifest says {expected}"),
            ));
        }
    }
    Ok(())
}

/// Lists the projects registered in the global backup snapshot.
fn projects_from_backup(deps: &Deps, global: &Path) -> Result<Vec<Project>, CoreError> {
    let root = deps
        .paths
        .tmp_dir()
        .join(format!("backup-projects-{}", ulid::Ulid::generate()));
    let paths = NodePaths::new(&root);
    let result = (|| {
        fs::create_dir_all(&root).map_err(|error| io_error(&root, error))?;
        fs::copy(global, paths.db_path()).map_err(|error| io_error(global, error))?;
        let store = Store::open_read_only(&paths, None)?;
        Ok(store.list_projects()?)
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

/// Counts rows from the backup snapshots, not from the live home.
///
/// The manifest is written after the database files, so reading the live store here would
/// record rows that are not in the backup whenever the supervisor is running. The snapshots
/// are copied into a temporary home and opened read-only, which keeps the backup directory
/// untouched and makes the counts describe exactly the files a restore will see.
fn counts_from_backup(
    deps: &Deps,
    global: &Path,
    projects: &[(ProjectName, PathBuf)],
) -> Result<(Counts, ProjectCounts), CoreError> {
    let root = deps
        .paths
        .tmp_dir()
        .join(format!("backup-count-{}", ulid::Ulid::generate()));
    let paths = NodePaths::new(&root);
    let result = (|| {
        fs::create_dir_all(&root).map_err(|error| io_error(&root, error))?;
        fs::copy(global, paths.db_path()).map_err(|error| io_error(global, error))?;
        let global_store = Store::open_read_only(&paths, None)?;
        let global_counts = global_counts(&global_store)?;
        drop(global_store);

        let mut project_counts = BTreeMap::new();
        for (name, snapshot) in projects {
            let directory = paths.project_dir(name);
            fs::create_dir_all(&directory).map_err(|error| io_error(&directory, error))?;
            let target = paths.project_db_path(name);
            fs::copy(snapshot, &target).map_err(|error| io_error(snapshot, error))?;
            let store = Store::open_read_only(&paths, Some(name))?;
            project_counts.insert(name.as_str().to_owned(), project_counts_for(&store, name)?);
        }
        Ok((global_counts, project_counts))
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

/// Counts the rows a backup manifest records for the global database.
fn global_counts(store: &Store) -> Result<Counts, CoreError> {
    let sessions = store.list_sessions(&SessionFilter::default())?;
    let mut capsules = 0_u64;
    for session in &sessions {
        capsules += store.list_capsules(&session.id)?.len() as u64;
    }
    let mut counts = BTreeMap::new();
    counts.insert("nodes".to_owned(), store.list_nodes()?.len() as u64);
    counts.insert("projects".to_owned(), store.list_projects()?.len() as u64);
    counts.insert("sessions".to_owned(), sessions.len() as u64);
    counts.insert("session_events".to_owned(), store.count_session_events()?);
    counts.insert("capsules".to_owned(), capsules);
    Ok(counts)
}

/// Counts the rows a backup manifest records for one project database.
fn project_counts_for(store: &Store, project: &ProjectName) -> Result<Counts, CoreError> {
    let jobs = store.list_jobs(&JobFilter::default())?;
    let mut counts = BTreeMap::new();
    counts.insert("jobs".to_owned(), jobs.len() as u64);
    counts.insert("runs".to_owned(), store.count_runs()?);
    counts.insert("edges".to_owned(), store.list_edges(None)?.len() as u64);
    counts.insert("loops".to_owned(), store.list_loops()?.len() as u64);
    counts.insert("schedules".to_owned(), store.list_schedules()?.len() as u64);
    counts.insert("inbox".to_owned(), store.list_inbox(None)?.len() as u64);
    counts.insert(
        "ledger".to_owned(),
        store.ledger_since(&LedgerFilter::default())?.len() as u64,
    );
    counts.insert("releases".to_owned(), store.list_releases(project, None)?.len() as u64);
    Ok(counts)
}

/// Deletes old daily and weekly backups from the default backup directory.
///
/// The newest backup of each day is a daily candidate; the newest of those in each ISO week
/// is a weekly candidate. The union of the newest seven daily candidates and the newest four
/// weekly candidates is kept, and every other timestamped backup directory is removed.
fn rotate_backups(root: &Path, current: &Path) -> Result<(), CoreError> {
    let mut candidates: Vec<(String, Date)> = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(root, error)),
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() || !path.join("manifest.json").is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(date) = parse_backup_date(name) {
            candidates.push((name.to_owned(), date));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let mut newest_per_day: BTreeMap<Date, String> = BTreeMap::new();
    for (name, date) in &candidates {
        newest_per_day.insert(*date, name.clone());
    }
    // The weekly candidate is the newest *daily* candidate of its ISO week. Deriving it
    // from `newest_per_day` means several backups on the same date keep the newest name
    // for that date, not whichever name was seen first.
    let mut newest_per_week: BTreeMap<(i32, u8), (Date, String)> = BTreeMap::new();
    for (date, name) in &newest_per_day {
        let (week_year, week, _) = date.to_iso_week_date();
        let key = (week_year, week);
        match newest_per_week.get(&key) {
            Some((known_date, _)) if known_date >= date => {}
            _ => {
                newest_per_week.insert(key, (*date, name.clone()));
            }
        }
    }

    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut daily: Vec<(Date, String)> = newest_per_day.into_iter().collect();
    daily.sort_by_key(|entry| entry.0);
    for (_, name) in daily.iter().rev().take(7) {
        keep.insert(name.clone());
    }
    let mut weekly: Vec<((i32, u8), (Date, String))> = newest_per_week.into_iter().collect();
    weekly.sort_by_key(|entry| entry.1.0);
    for (_, (_, name)) in weekly.iter().rev().take(4) {
        keep.insert(name.clone());
    }
    if let Some(name) = current.file_name().and_then(|name| name.to_str()) {
        keep.insert(name.to_owned());
    }

    for (name, _) in candidates {
        if keep.contains(&name) {
            continue;
        }
        let path = root.join(&name);
        fs::remove_dir_all(&path).map_err(|error| io_error(&path, error))?;
    }
    Ok(())
}

/// Parses the compact UTC timestamp used as a backup directory name.
fn parse_backup_date(name: &str) -> Option<Date> {
    let bytes = name.as_bytes();
    if bytes.len() < 15 || bytes.get(8) != Some(&b'T') {
        return None;
    }
    let year = name.get(0..4)?.parse::<i32>().ok()?;
    let month = name.get(4..6)?.parse::<u8>().ok()?;
    let day = name.get(6..8)?.parse::<u8>().ok()?;
    let hour = name.get(9..11)?.parse::<u8>().ok()?;
    let minute = name.get(11..13)?.parse::<u8>().ok()?;
    let second = name.get(13..15)?.parse::<u8>().ok()?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()
}

/// Returns the compact UTC form used in backup directory names.
fn compact_timestamp(timestamp: Timestamp) -> String {
    timestamp
        .as_rfc3339()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

/// Returns the POSIX-style path of `path` relative to `root`.
fn relative_posix(root: &Path, path: &Path) -> Result<String, CoreError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| CoreError::Invalid(format!("{} is outside {}", path.display(), root.display())))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            _ => {
                return Err(CoreError::Invalid(format!(
                    "{} is not a plain relative path",
                    relative.display()
                )));
            }
        }
    }
    Ok(parts.join("/"))
}

/// Returns whether a target names an offsite copy rather than a local directory.
fn is_offsite_target(target: &str) -> bool {
    target.starts_with("rsync://")
        || target.starts_with("git:")
        || target.contains("://")
        || (target.contains('@') && target.contains(':'))
}

/// Adds a suffix to the last path component.
fn with_suffix(path: PathBuf, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Returns a restore destination for one manifest path, or `None` for informational files.
fn restore_path(paths: &NodePaths, relative: &str) -> Option<PathBuf> {
    if relative == "global.db" {
        return Some(paths.db_path());
    }
    if let Some(rest) = relative.strip_prefix("projects/") {
        let name = rest.strip_suffix(".db")?;
        if name.contains('/') || name.is_empty() {
            return None;
        }
        let project = ProjectName::parse(name).ok()?;
        return Some(paths.project_db_path(&project));
    }
    if let Some(rest) = relative.strip_prefix("sessions/") {
        let mut parts = rest.split('/');
        let id = parts.next()?;
        if id.is_empty() || parts.any(|part| part.is_empty() || part == "." || part == "..") {
            return None;
        }
        let id = SessionId::parse(id).ok()?;
        let mut path = paths.session_dir(&id);
        for part in rest.split('/').skip(1) {
            path.push(part);
        }
        return Some(path);
    }
    None
}

/// Checks that a destination can be written, without touching it.
fn check_destination(destination: &Path) -> Result<(), CoreError> {
    if destination.exists() {
        if !destination.is_dir() {
            return Err(CoreError::Store(StoreError::Conflict(format!(
                "backup destination {} is not a directory",
                destination.display()
            ))));
        }
        let entries: Vec<_> = fs::read_dir(destination)
            .map_err(|error| io_error(destination, error))?
            .filter_map(Result::ok)
            .collect();
        if !entries.is_empty() && !destination.join("manifest.json").is_file() {
            return Err(CoreError::Store(StoreError::Conflict(format!(
                "backup destination {} is not empty and is not a VOWP backup",
                destination.display()
            ))));
        }
    }
    if let Some(parent) = destination.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    Ok(())
}

/// Returns the staging directory used for one destination.
fn staging_path(destination: &Path) -> PathBuf {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".to_owned());
    parent.join(format!(".{name}.tmp-{}", ulid::Ulid::generate()))
}

/// Replaces `destination` with the finished staging directory.
fn swap_destination(staging: &Path, destination: &Path) -> Result<(), CoreError> {
    if !destination.exists() {
        return fs::rename(staging, destination).map_err(|error| io_error(destination, error));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".to_owned());
    let old = parent.join(format!(".{name}.old-{}", ulid::Ulid::generate()));
    fs::rename(destination, &old).map_err(|error| io_error(destination, error))?;
    if let Err(error) = fs::rename(staging, destination) {
        let _ = fs::rename(&old, destination);
        return Err(io_error(destination, error));
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// Checks that the restore target may be replaced, without touching it.
fn check_restore_target(home: &Path, force: bool) -> Result<(), CoreError> {
    if !home.exists() {
        return Ok(());
    }
    if !home.is_dir() {
        return Err(CoreError::Store(StoreError::Conflict(format!(
            "restore target {} is not a directory",
            home.display()
        ))));
    }
    if force {
        return Ok(());
    }
    let mut entries = fs::read_dir(home).map_err(|error| io_error(home, error))?;
    if entries.next().is_some() {
        return Err(CoreError::Store(StoreError::Conflict(format!(
            "{} is not empty; pass --force to overwrite",
            home.display()
        ))));
    }
    Ok(())
}

/// Returns a hidden sibling path used to stage or preserve one home directory.
fn sibling_home_path(home: &Path, tag: &str) -> PathBuf {
    let parent = match home.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = home
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "home".to_owned());
    parent.join(format!(".{name}.{tag}-{}", ulid::Ulid::generate()))
}

/// Replaces the target home with the fully validated staging directory.
fn install_staged_home(staging: &Path, home: &Path) -> Result<(), CoreError> {
    if !home.exists() {
        return fs::rename(staging, home).map_err(|error| io_error(home, error));
    }
    let old = sibling_home_path(home, "old");
    fs::rename(home, &old).map_err(|error| io_error(home, error))?;
    if let Err(error) = fs::rename(staging, home) {
        let _ = fs::rename(&old, home);
        return Err(io_error(home, error));
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// Copies one whole backup directory to an offsite target.
fn copy_offsite_dir(deps: &Deps, directory: &Path, target: &str) -> Result<(), CoreError> {
    if let Some(remote) = target.strip_prefix("git:") {
        return push_git_copy(deps, directory, remote);
    }
    let args = vec![
        "-a".to_owned(),
        format!("{}/", directory.display()),
        normalize_rsync_target(target),
    ];
    run_tool(deps, RSYNC_BINARY, &args, None, "the offsite backup copy")?;
    Ok(())
}

/// Copies one legacy backup file to an offsite target.
fn copy_offsite_file(deps: &Deps, file: &Path, target: &str) -> Result<(), CoreError> {
    if target.starts_with("git:") {
        return Err(CoreError::Invalid(
            "a git offsite target needs a full backup directory, not a single .db file".to_owned(),
        ));
    }
    let args = vec![
        "-a".to_owned(),
        file.display().to_string(),
        normalize_rsync_target(target),
    ];
    run_tool(deps, RSYNC_BINARY, &args, None, "the offsite backup copy")?;
    Ok(())
}

/// Pushes a backup directory to a git remote.
fn push_git_copy(deps: &Deps, directory: &Path, remote: &str) -> Result<(), CoreError> {
    if remote.trim().is_empty() {
        return Err(CoreError::Invalid("git: needs a remote name or URL".to_owned()));
    }
    let cwd = Some(directory);
    run_git(deps, cwd, &["init", "-q"])?;
    run_git(deps, cwd, &["add", "-A"])?;
    run_git(
        deps,
        cwd,
        &[
            "-c",
            "user.name=vowp",
            "-c",
            "user.email=vowp@localhost",
            "commit",
            "-q",
            "-m",
            "vowp backup",
        ],
    )?;
    run_git(deps, cwd, &["push", "--force", remote, "HEAD:refs/heads/main"])?;
    Ok(())
}

/// Runs one git command in `cwd`.
fn run_git(deps: &Deps, cwd: Option<&Path>, args: &[&str]) -> Result<CommandOutput, CoreError> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    run_tool(deps, GIT_BINARY, &args, cwd, "the offsite backup copy")
}

/// Returns an rsync destination with exactly one trailing slash.
fn normalize_rsync_target(target: &str) -> String {
    if target.starts_with("rsync://") || target.ends_with('/') {
        target.to_owned()
    } else {
        format!("{target}/")
    }
}

/// Encrypts one file with `age`.
fn encrypt_file(deps: &Deps, source: &Path, destination: &Path, recipient: &str) -> Result<(), CoreError> {
    let args = vec![
        "-r".to_owned(),
        recipient.to_owned(),
        "-o".to_owned(),
        destination.display().to_string(),
        source.display().to_string(),
    ];
    run_tool(deps, AGE_BINARY, &args, None, "backup encryption")?;
    Ok(())
}

/// Decrypts one `age` file.
fn decrypt_file(deps: &Deps, source: &Path, destination: &Path) -> Result<(), CoreError> {
    let args = vec![
        "--decrypt".to_owned(),
        "-o".to_owned(),
        destination.display().to_string(),
        source.display().to_string(),
    ];
    run_tool(deps, AGE_BINARY, &args, None, "backup decryption")?;
    Ok(())
}

/// Runs an external tool through the injected command runner, never through a shell.
fn run_tool(
    deps: &Deps,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    purpose: &str,
) -> Result<CommandOutput, CoreError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(program.to_owned());
    argv.extend(args.iter().cloned());
    let spec = CommandSpec {
        shell: None,
        argv,
        cwd: cwd.map(Path::to_path_buf),
        env: Vec::new(),
        timeout: TOOL_TIMEOUT,
    };
    match deps.cmd.run(&spec) {
        Ok(output) if output.exit_code == Some(0) => Ok(output),
        Ok(output) => Err(CoreError::Command(format!(
            "{program} failed while {purpose} (exit {:?}): {}",
            output.exit_code,
            output.tail_lines(20)
        ))),
        Err(CoreError::Io { path, source }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(CoreError::Refused(format!(
                "{path} is required for {purpose}; install it or use a local backup"
            )))
        }
        Err(error) => Err(error),
    }
}

/// Hashes one file with SHA-256.
fn sha256_file(path: &Path) -> Result<String, CoreError> {
    let mut file = fs::File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher.finalize()))
}

/// Formats bytes as lowercase hexadecimal.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.as_ref().len() * 2);
    for byte in bytes.as_ref() {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Builds an I/O error tagged with the path it touched.
fn io_error(path: &Path, source: std::io::Error) -> CoreError {
    CoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Builds a JSON error.
fn json_error(error: serde_json::Error) -> CoreError {
    CoreError::Store(StoreError::Json(error))
}

/// Builds a corrupt-data error.
fn corrupt(path: impl Into<String>, reason: impl Into<String>) -> CoreError {
    CoreError::Store(StoreError::Corrupt {
        path: path.into(),
        line: None,
        reason: reason.into(),
    })
}
