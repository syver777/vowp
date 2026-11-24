//! Garbage collection: dry-run reporting by class and the guarded `--apply` pass.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10.2. `--apply` deletes only
//! `closed_session`, `run_output`, `worktree`, `job_window`, `sample` and `cache` entries whose
//! expiry has passed. Production, truth, open work and secrets are never deleted, even when a
//! catalog row carries an expiry. `--older-than` shortens the closed-session and run-output
//! windows for one explicit pass; a live lease, an active job or a running orchestrator keeps its
//! session.
//!
//! The run-output cap and rotation from `docs/design/architecture.md` §3 live here and in
//! [`super::tick`]: completed run files are capped at 50 MB, gzip after one day and removed after
//! fourteen days.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use vowp_protocol::{CatalogClass, CatalogEntry, GcClassReport, GcReport, Timestamp};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::{CommandSpec, Deps};

use super::classes::{self, ClassifiedItem, GcRefused, ItemAction, Retention};
use super::compact::compact_session;
use super::{
    CLOSED_SESSION_RETENTION, RUN_OUTPUT_CAP_BYTES, RUN_OUTPUT_MAX_AGE, SAMPLE_FULL_RETENTION, SAMPLE_MINUTE_RETENTION,
};

/// Last wall-clock cap check per node home. Using wall time keeps the supervisor tick cheap and
/// makes the cap independent of the injected test clock.
static CAP_CHECKS: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> = OnceLock::new();

/// Minimum wall-clock gap between automatic cap scans for one home.
const CAP_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Largest accepted `--older-than` window.
///
/// Ten years is far past any real retention period and keeps an input typo such as
/// `999999999d` from silently disabling collection.
pub const MAX_OLDER_THAN: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// Options for `vowp gc`.
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Delete eligible files instead of only reporting.
    pub apply: bool,
    /// Restrict to one class.
    pub class: Option<CatalogClass>,
    /// Clock override for tests.
    pub now: Option<Timestamp>,
}

/// Totals for one class in a GC pass, including what an apply pass would collect now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcClassOutcome {
    /// Class the totals describe.
    pub class: CatalogClass,
    /// Number of files.
    pub files: u64,
    /// Total bytes.
    pub bytes: u64,
    /// Files an apply pass would delete or compact now; compression is not counted.
    pub eligible_files: u64,
    /// Bytes those files currently occupy; an estimate of what a dry run would free.
    pub eligible_bytes: u64,
    /// Up to five example paths.
    pub sample_paths: Vec<String>,
}

/// One GC pass: the totals reported to the owner, plus refused and eligible detail.
///
/// The protocol [`GcReport`] is the stable wire shape for the nightly pass. This type adds the
/// per-class eligible bytes and the refused candidates an explicit `--older-than` pass lists;
/// [`GcOutcome::report`] projects it back to the protocol shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcOutcome {
    /// When the report was generated.
    pub generated_at: Timestamp,
    /// Whether the run was a dry run.
    pub dry_run: bool,
    /// Totals per class.
    pub classes: Vec<GcClassOutcome>,
    /// Candidates a live lease, an active job or a running orchestrator protected.
    pub refused: Vec<GcRefused>,
    /// Number of files deleted (zero on a dry run).
    pub deleted_files: u64,
    /// Bytes freed (zero on a dry run).
    pub freed_bytes: u64,
    /// Number of closed sessions compacted.
    pub compacted_sessions: u64,
    /// Number of tagged job windows closed.
    pub closed_windows: u64,
    /// Errors encountered while applying.
    pub errors: Vec<String>,
}

impl GcOutcome {
    /// Projects this outcome onto the protocol report shape used by the nightly pass.
    pub fn report(&self) -> GcReport {
        GcReport {
            generated_at: self.generated_at,
            dry_run: self.dry_run,
            classes: self
                .classes
                .iter()
                .map(|class| GcClassReport {
                    class: class.class,
                    files: class.files,
                    bytes: class.bytes,
                    sample_paths: class.sample_paths.clone(),
                })
                .collect(),
            deleted_files: self.deleted_files,
            freed_bytes: self.freed_bytes,
            compacted_sessions: self.compacted_sessions,
            errors: self.errors.clone(),
        }
    }
}

/// Parses a duration such as `36h`, `7d` or `2w`.
///
/// Supported units are seconds (`s`), minutes (`m`), hours (`h`), days (`d`) and weeks
/// (`w`). The value must be a positive whole number; zero, negative, unterminated and
/// larger-than-[`MAX_OLDER_THAN`] inputs are rejected.
pub fn parse_older_than(text: &str) -> Result<Duration, CoreError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Invalid(
            "--older-than needs a duration such as 7d".to_owned(),
        ));
    }
    if trimmed.starts_with('-') {
        return Err(CoreError::Invalid(format!(
            "--older-than must be positive, not {trimmed:?}"
        )));
    }
    let mut chars = trimmed.chars();
    let unit = chars
        .next_back()
        .ok_or_else(|| CoreError::Invalid("--older-than needs a duration such as 7d".to_owned()))?;
    let digits = chars.as_str();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CoreError::Invalid(format!(
            "--older-than {trimmed:?} must be a whole number followed by s, m, h, d or w (for example 7d)"
        )));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| CoreError::Invalid(format!("--older-than {trimmed:?} is too large")))?;
    if value == 0 {
        return Err(CoreError::Invalid("--older-than must be greater than zero".to_owned()));
    }
    let seconds_per_unit = match unit.to_ascii_lowercase() {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        'w' => 7 * 24 * 60 * 60,
        other => {
            return Err(CoreError::Invalid(format!(
                "--older-than {trimmed:?} has unknown unit {other:?}; expected s, m, h, d or w"
            )));
        }
    };
    let seconds = value
        .checked_mul(seconds_per_unit)
        .ok_or_else(|| CoreError::Invalid(format!("--older-than {trimmed:?} is too large")))?;
    let age = Duration::from_secs(seconds);
    if age > MAX_OLDER_THAN {
        return Err(CoreError::Invalid(format!(
            "--older-than {trimmed:?} is absurd; the maximum is 3650d"
        )));
    }
    Ok(age)
}

/// Runs one GC pass with the default retention windows.
///
/// This is the pass the nightly `gc-nightly` schedule runs; it returns the protocol report.
/// An explicit `vowp gc --older-than` uses [`gc_with_window`] instead.
pub fn gc(deps: &Deps, store: &Store, opts: &GcOptions) -> Result<GcReport, CoreError> {
    Ok(gc_with_window(deps, store, opts, None)?.report())
}

/// Runs one GC pass, optionally overriding the closed-session and run-output windows.
///
/// `older_than` applies only to [`CatalogClass::ClosedSession`] and
/// [`CatalogClass::RunOutput`]; every other class keeps its default retention. Zero and
/// absurd windows, and an `older_than` combined with another class filter, are refused with
/// [`CoreError::Invalid`] before any file is touched.
pub fn gc_with_window(
    deps: &Deps,
    store: &Store,
    opts: &GcOptions,
    older_than: Option<Duration>,
) -> Result<GcOutcome, CoreError> {
    let now = opts.now.unwrap_or_else(|| deps.clock.now());
    let mut errors = Vec::new();

    if let Some(older_than) = older_than {
        if older_than.is_zero() {
            return Err(CoreError::Invalid("--older-than must be greater than zero".to_owned()));
        }
        if older_than > MAX_OLDER_THAN {
            return Err(CoreError::Invalid(
                "--older-than is absurd; the maximum is ten years".to_owned(),
            ));
        }
        if let Some(class) = opts.class
            && !matches!(class, CatalogClass::ClosedSession | CatalogClass::RunOutput)
        {
            return Err(CoreError::Invalid(format!(
                "--older-than applies only to closed_session and run_output, not {}",
                classes::class_name(class)
            )));
        }
    }

    if opts.apply && opts.class.is_none_or(|class| class == CatalogClass::RunOutput) {
        match enforce_run_output_cap(deps, store, true) {
            Ok(_) => {}
            Err(error) => errors.push(format!("run output cap: {error}")),
        }
    }

    let closed_retention = match (older_than, opts.class) {
        (Some(age), None | Some(CatalogClass::ClosedSession)) => age,
        _ => CLOSED_SESSION_RETENTION,
    };
    let run_output_retention = match (older_than, opts.class) {
        (Some(age), None | Some(CatalogClass::RunOutput)) => age,
        _ => RUN_OUTPUT_MAX_AGE,
    };
    let outcome = classes::classify_items(
        deps,
        store,
        now,
        Retention {
            closed_session: closed_retention,
            run_output: run_output_retention,
        },
    )?;
    let selected: Vec<ClassifiedItem> = outcome
        .items
        .into_iter()
        .filter(|item| opts.class.is_none_or(|class| item.file.class == class))
        .collect();
    let refused: Vec<GcRefused> = outcome
        .refused
        .into_iter()
        .filter(|entry| opts.class.is_none_or(|class| entry.class == class))
        .collect();

    let needs_open_runs = selected.iter().any(|item| {
        matches!(item.action, ItemAction::CompressRunOutput)
            || (item.action == ItemAction::DeleteFile && item.file.class == CatalogClass::RunOutput)
    });
    let open_runs = if needs_open_runs {
        open_run_stems(deps, store).unwrap_or(None)
    } else {
        Some(BTreeSet::new())
    };

    let mut report = GcOutcome {
        generated_at: now,
        dry_run: !opts.apply,
        classes: class_reports(&selected, now, open_runs.as_ref()),
        refused,
        deleted_files: 0,
        freed_bytes: 0,
        compacted_sessions: 0,
        closed_windows: 0,
        errors,
    };
    if !opts.apply {
        return Ok(report);
    }

    let mut compacted = BTreeSet::new();
    let mut samples_pruned = false;
    for item in &selected {
        if item.action == ItemAction::Keep || classes::is_protected(item.file.class) {
            continue;
        }
        if item.file.class == CatalogClass::RunOutput
            && let Some(stem) = run_stem(&item.file.path)
        {
            let still_open = open_runs.as_ref().is_none_or(|open| open.contains(stem));
            if still_open {
                // The run is still writing, or the open-run list is unknown. Leave it alone.
                continue;
            }
        }
        // Compression is due one day after a run finishes, long before its fourteen-day
        // deletion expiry. It is not a deletion and must not wait for `expires_at`.
        if item.action == ItemAction::CompressRunOutput {
            if let Err(error) = compress_run_output(deps, store, item, now) {
                report.errors.push(format!("{}: {error}", item.file.path.display()));
            }
            continue;
        }
        let Some(expires_at) = item.expires_at else {
            continue;
        };
        if now < expires_at {
            continue;
        }
        match &item.action {
            ItemAction::Keep => {}
            ItemAction::DeleteFile => match delete_file(deps, store, item, now) {
                Ok(()) => {
                    report.deleted_files = report.deleted_files.saturating_add(1);
                    report.freed_bytes = report.freed_bytes.saturating_add(item.file.bytes);
                }
                Err(error) => report.errors.push(format!("{}: {error}", item.file.path.display())),
            },
            ItemAction::CompressRunOutput => {}
            ItemAction::CompactSession(session) => {
                if !compacted.insert(session.clone()) {
                    continue;
                }
                let scoped = match item.file.project.as_ref() {
                    Some(project) if store.bound_project() != Some(project) => {
                        Store::open_with_project(&deps.paths, project).ok()
                    }
                    _ => None,
                };
                let session_store = scoped.as_ref().unwrap_or(store);
                let before = session_file_count(deps, session);
                match compact_session(deps, session_store, session, now) {
                    Ok(compact) => {
                        if !compact.skipped {
                            report.compacted_sessions = report.compacted_sessions.saturating_add(1);
                            report.freed_bytes = report.freed_bytes.saturating_add(compact.bytes_freed);
                            let after = session_file_count(deps, session);
                            report.deleted_files = report.deleted_files.saturating_add(before.saturating_sub(after));
                            mark_catalog_deleted(store, &item.file.path, now);
                        }
                    }
                    Err(error) => report.errors.push(format!("session {session}: {error}")),
                }
            }
            ItemAction::RemoveWorktree { repo, worktree } => {
                if !safe_worktree(repo, worktree) {
                    report.errors.push(format!(
                        "{}: refusing to remove a worktree that is the main checkout",
                        item.file.path.display()
                    ));
                    continue;
                }
                let bytes = item.file.bytes;
                match deps.git.worktree_remove(repo, worktree) {
                    Ok(()) => {
                        report.deleted_files = report.deleted_files.saturating_add(1);
                        report.freed_bytes = report.freed_bytes.saturating_add(bytes);
                        mark_catalog_deleted(store, &item.file.path, now);
                    }
                    Err(error) => report.errors.push(format!("{}: {error}", item.file.path.display())),
                }
            }
            ItemAction::KillWindow { session, window } => match deps.windows.kill_window(session, window) {
                Ok(()) => report.closed_windows = report.closed_windows.saturating_add(1),
                Err(vowp_tmux::TmuxError::WindowNotFound { .. }) => {}
                Err(error) => report.errors.push(format!("{session}:{window}: {error}")),
            },
            ItemAction::PruneSamples => {
                if samples_pruned {
                    continue;
                }
                samples_pruned = true;
                let full_before = now.minus(SAMPLE_FULL_RETENTION);
                let minute_before = now.minus(SAMPLE_MINUTE_RETENTION);
                match store.lifecycle_prune_samples(full_before, minute_before) {
                    Ok(deleted) => report.deleted_files = report.deleted_files.saturating_add(deleted),
                    Err(error) => report.errors.push(format!("host samples: {error}")),
                }
            }
        }
    }
    Ok(report)
}

/// Groups the selected entries into one report row per class, most protective first.
fn class_reports(
    items: &[ClassifiedItem],
    now: Timestamp,
    open_runs: Option<&BTreeSet<String>>,
) -> Vec<GcClassOutcome> {
    let mut totals: BTreeMap<u8, GcClassOutcome> = BTreeMap::new();
    for item in items {
        let entry = totals
            .entry(classes::class_rank(item.file.class))
            .or_insert_with(|| GcClassOutcome {
                class: item.file.class,
                files: 0,
                bytes: 0,
                eligible_files: 0,
                eligible_bytes: 0,
                sample_paths: Vec::with_capacity(5),
            });
        entry.files = entry.files.saturating_add(1);
        entry.bytes = entry.bytes.saturating_add(item.file.bytes);
        if collectable(item, now, open_runs) {
            entry.eligible_files = entry.eligible_files.saturating_add(1);
            entry.eligible_bytes = entry.eligible_bytes.saturating_add(item.file.bytes);
        }
        if entry.sample_paths.len() < 5 {
            entry.sample_paths.push(item.file.path.display().to_string());
        }
    }
    totals.into_values().collect()
}

/// Returns true when an apply pass would delete or compact this item now.
///
/// Compression is excluded because it keeps the bytes; a run whose writer is still open or
/// whose open-run list is unknown is excluded because the apply pass skips it.
fn collectable(item: &ClassifiedItem, now: Timestamp, open_runs: Option<&BTreeSet<String>>) -> bool {
    let action_removes = matches!(
        item.action,
        ItemAction::DeleteFile
            | ItemAction::CompactSession(_)
            | ItemAction::RemoveWorktree { .. }
            | ItemAction::KillWindow { .. }
            | ItemAction::PruneSamples
    );
    if !action_removes || item.expires_at.is_none_or(|expires_at| now < expires_at) {
        return false;
    }
    if item.file.class == CatalogClass::RunOutput
        && let Some(stem) = run_stem(&item.file.path)
        && open_runs.is_none_or(|open| open.contains(stem))
    {
        return false;
    }
    true
}

/// Deletes one regular file inside the node home.
///
/// A path whose canonical form escapes the node home is refused: catalog rows can name files
/// anywhere, but a lifecycle pass only owns home state.
fn delete_file(deps: &Deps, store: &Store, item: &ClassifiedItem, now: Timestamp) -> Result<(), CoreError> {
    let path = &item.file.path;
    if !inside_home(deps.paths.root(), path) {
        return Err(CoreError::Refused(format!(
            "{} is outside the node home",
            path.display()
        )));
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            mark_catalog_deleted(store, path, now);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CoreError::Io {
            path: path.display().to_string(),
            source: error,
        }),
    }
}

/// Compresses one run-output file in place, keeping `<name>.gz` and a catalog row.
fn compress_run_output(deps: &Deps, store: &Store, item: &ClassifiedItem, now: Timestamp) -> Result<(), CoreError> {
    let path = &item.file.path;
    if !inside_home(deps.paths.root(), path) {
        return Err(CoreError::Refused(format!(
            "{} is outside the node home",
            path.display()
        )));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CoreError::Invalid(format!("run output {} has no file name", path.display())))?;
    if name.ends_with(".gz") {
        return Ok(());
    }
    let mtime = std::fs::metadata(path).and_then(|meta| meta.modified()).ok();
    let spec = CommandSpec {
        shell: None,
        argv: vec!["gzip".to_owned(), "-f".to_owned(), path.display().to_string()],
        cwd: None,
        env: Vec::new(),
        timeout: Duration::from_secs(120),
    };
    let output = deps.cmd.run(&spec)?;
    if output.exit_code != Some(0) || output.timed_out {
        return Err(CoreError::Command(format!(
            "gzip {} failed: {}",
            path.display(),
            output.tail_lines(5)
        )));
    }
    let gz = path.with_file_name(format!("{name}.gz"));
    if !gz.is_file() {
        return Err(CoreError::Command(format!("gzip did not create {}", gz.display())));
    }
    if let Some(mtime) = mtime
        && let Ok(file) = std::fs::OpenOptions::new().write(true).open(&gz)
    {
        let _ = file.set_modified(mtime);
    }
    let gz_bytes = std::fs::metadata(&gz).map(|meta| meta.len()).unwrap_or(0);
    let entry = CatalogEntry {
        id: ulid::Ulid::generate().to_string(),
        path: gz.display().to_string(),
        class: CatalogClass::RunOutput,
        project: item.file.project.clone(),
        environment: "none".to_owned(),
        job_id: None,
        session_id: None,
        bytes: Some(gz_bytes),
        expires_at: item.expires_at,
        note: "compressed run output".to_owned(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    store.upsert_catalog(&entry)?;
    mark_catalog_deleted(store, path, now);
    Ok(())
}

/// Marks the catalog row for `path` deleted, when one exists.
fn mark_catalog_deleted(store: &Store, path: &Path, now: Timestamp) {
    let key = path.to_string_lossy();
    if let Ok(Some(entry)) = store.catalog_entry(&key) {
        // A stale deleted_at is harmless; the next gc tries again.
        let _ = store.mark_catalog_deleted(&entry.id, now);
    }
}

/// Returns true when `path` exists and its canonical form stays under `root`.
fn inside_home(root: &Path, path: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.starts_with(root)
}

/// Returns the run id stem of a run-output path, when it looks like one.
fn run_stem(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".gz") {
        return None;
    }
    name.strip_suffix(".jsonl.err")
        .or_else(|| name.strip_suffix(".jsonl"))
        .or_else(|| name.strip_suffix(".stderr"))
        .filter(|stem| !stem.is_empty())
}

/// Returns true when `worktree` is a distinct, non-root directory from its main checkout.
fn safe_worktree(repo: &Path, worktree: &Path) -> bool {
    if worktree.as_os_str().is_empty() || worktree.parent().is_none() || worktree == repo {
        return false;
    }
    match (repo.canonicalize(), worktree.canonicalize()) {
        (Ok(repo), Ok(worktree)) => worktree != repo && worktree.parent().is_some(),
        _ => false,
    }
}

/// Counts the removable content files of one session (log, blobs and lock).
fn session_file_count(deps: &Deps, session: &vowp_protocol::SessionId) -> u64 {
    let mut count = 0;
    if deps.paths.log_path(session).is_file() {
        count += 1;
    }
    if deps.paths.lock_path(session).is_file() {
        count += 1;
    }
    if let Ok(entries) = std::fs::read_dir(deps.paths.blobs_dir(session)) {
        count += entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count() as u64;
    }
    count
}

/// Caps run-output files at 50 MB.
///
/// A completed file keeps its last 50 MB, starting at a line boundary. An in-flight file cannot be
/// replaced without losing the writer's descriptor, so it is truncated to zero and the append
/// writer continues in the same inode; the canonical session log is unaffected. Returns the bytes
/// removed.
pub(crate) fn enforce_run_output_cap(deps: &Deps, store: &Store, force: bool) -> Result<u64, CoreError> {
    if store.paths().is_none() {
        return Ok(0);
    }
    let now = Instant::now();
    let root = deps.paths.root().to_path_buf();
    let checks = CAP_CHECKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    {
        let mut checks = checks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !force
            && let Some(last) = checks.get(&root)
            && now.duration_since(*last) < CAP_CHECK_INTERVAL
        {
            return Ok(0);
        }
        checks.insert(root, now);
    }
    let dir = deps.paths.runs_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut oversized = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(stem) = run_stem(&path).map(str::to_owned) else {
            continue;
        };
        if entry.metadata().is_ok_and(|meta| meta.len() > RUN_OUTPUT_CAP_BYTES) {
            oversized.push((path, stem));
        }
    }
    if oversized.is_empty() {
        return Ok(0);
    }

    let Some(open_runs) = open_run_stems(deps, store)? else {
        return Ok(0);
    };
    let mut trimmed = 0_u64;
    for (path, stem) in oversized {
        if open_runs.contains(&stem) {
            trimmed = trimmed.saturating_add(truncate_open_run(&path)?);
            continue;
        }
        trimmed = trimmed.saturating_add(truncate_tail(&path, RUN_OUTPUT_CAP_BYTES)?);
    }
    Ok(trimmed)
}

/// Truncates a file whose writer is still appending, keeping the same inode.
fn truncate_open_run(path: &Path) -> Result<u64, CoreError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })?;
    let bytes = file
        .metadata()
        .map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })?
        .len();
    file.set_len(0).map_err(|error| CoreError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    let _ = file.sync_all();
    Ok(bytes)
}

/// Returns the run ids that are still open across every registered project.
///
/// A missing project database makes the answer incomplete, so the caller skips trimming instead
/// of risking a live file.
fn open_run_stems(deps: &Deps, store: &Store) -> Result<Option<BTreeSet<String>>, CoreError> {
    let projects = store.list_projects()?;
    let mut open = BTreeSet::new();
    for project in projects {
        let project_store = match Store::open_with_project(&deps.paths, &project.name) {
            Ok(store) => store,
            Err(_) => return Ok(None),
        };
        for run in project_store.open_runs()? {
            open.insert(run.id.to_string());
        }
    }
    Ok(Some(open))
}

/// Keeps the last `cap` bytes of `path`, starting at a line boundary when one exists.
fn truncate_tail(path: &Path, cap: u64) -> Result<u64, CoreError> {
    let length = std::fs::metadata(path)
        .map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })?
        .len();
    if length <= cap {
        return Ok(0);
    }
    let start = length - cap;
    let mut source = File::open(path).map_err(|error| CoreError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    source.seek(SeekFrom::Start(start)).map_err(|error| CoreError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    let mut reader = BufReader::new(source);
    let mut fragment = Vec::new();
    reader.read_until(b'\n', &mut fragment).map_err(|error| CoreError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    if !fragment.ends_with(b"\n") {
        // The tail is one long line; keep the whole capped slice rather than nothing.
        reader.seek(SeekFrom::Start(start)).map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })?;
    }

    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("run");
    let tmp = path.with_file_name(format!(".{name}.cap-{}", ulid::Ulid::generate()));
    let result = (|| -> Result<(), CoreError> {
        let mut dest = File::create(&tmp).map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        std::io::copy(&mut reader, &mut dest).map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        dest.sync_all().map_err(|error| CoreError::Io {
            path: tmp.display().to_string(),
            source: error,
        })?;
        std::fs::rename(&tmp, path).map_err(|error| CoreError::Io {
            path: path.display().to_string(),
            source: error,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    let new_length = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(cap);
    Ok(length.saturating_sub(new_length))
}
