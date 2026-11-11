//! Lifecycle classes, garbage collection, compaction and backup.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10 and §13.2. Classes are derived from the
//! database (jobs, sessions, runs and host samples) and merged with the durable `catalog` rows.
//! `vowp gc` reports by class and only `--apply` deletes the five disposable classes; production,
//! truth, open work and secrets are never touched.
//!
//! The nightly pass is the `gc-nightly` L9 schedule (`every 24h`, `vowp gc --apply`) that
//! [`tick`] creates once per project. The same tick enforces the 50 MB run-output cap; the
//! fourteen-day deletion and one-day gzip live in [`gc`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use vowp_protocol::{
    ActiveState, CatalogClass, MissedPolicy, ProjectName, Schedule, ScheduleAction, ScheduleId, ScheduleSpecKind,
    Timestamp,
};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;
use crate::schedules::ScheduleSpec;

/// Projects whose `gc-nightly` schedule this process has already checked.
static NIGHTLY_SCHEDULES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

pub mod backup;
pub mod classes;
pub mod compact;
pub mod gc;

pub use backup::{BackupOptions, backup, restore};
pub use classes::{class_name, classify, parse_class};
pub use compact::{CompactReport, compact_session};
pub use gc::{GcOptions, gc};

/// Largest raw run-output file kept on disk; older bytes are trimmed from the front.
pub const RUN_OUTPUT_CAP_BYTES: u64 = 50 * 1024 * 1024;

/// How long raw run output is kept before `gc --apply` deletes it.
pub const RUN_OUTPUT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How long raw run output stays uncompressed before `gc --apply` gzips it.
pub const RUN_OUTPUT_COMPRESS_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a closed session's full log and blobs are kept before compaction.
pub const CLOSED_SESSION_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a failed, cancelled or replanned job's worktree is kept.
pub const FAILED_WORKTREE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long full-rate host samples are kept.
pub const SAMPLE_FULL_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// How long one-minute host-sample rows are kept.
pub const SAMPLE_MINUTE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a disposable temporary file may sit in `tmp/`.
pub const CACHE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Name of the nightly GC schedule every project receives.
pub const GC_NIGHTLY_SCHEDULE: &str = "gc-nightly";

/// One file with its lifecycle class.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedFile {
    /// File path.
    pub path: PathBuf,
    /// Lifecycle class.
    pub class: CatalogClass,
    /// Project the file belongs to, when bound.
    pub project: Option<ProjectName>,
    /// Size in bytes.
    pub bytes: u64,
    /// Why the file got this class.
    pub reason: String,
}

/// One node-level lifecycle pass: create the nightly schedule and cap completed run output.
///
/// Called from the supervisor tick. The cap scan is automatic at most once a minute per home;
/// `vowp gc --apply` forces one. Errors are returned so the tick can record the failing step; the
/// later steps still run.
pub fn tick(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError> {
    ensure_nightly_schedule(deps, store, now)?;
    gc::enforce_run_output_cap(deps, store, false)?;
    Ok(())
}

/// Creates the `gc-nightly` schedule when the bound project has none.
///
/// The per-process cache removes the schedule query from every supervisor tick. A schedule the
/// owner removes at runtime stays removed until the supervisor restarts, which matches the
/// explicit-owner-action rule; startup checks the database again.
fn ensure_nightly_schedule(deps: &Deps, store: &Store, now: Timestamp) -> Result<(), CoreError> {
    let projects: Vec<ProjectName> = match store.bound_project() {
        Some(project) => vec![project.clone()],
        None => store
            .list_projects()?
            .into_iter()
            .map(|project| project.name)
            .filter(|project| project.as_str() != vowp_store::RESERVED_NODE_PROJECT)
            .collect(),
    };
    for project in projects {
        let key = deps.paths.root().join(project.as_str());
        let checked = NIGHTLY_SCHEDULES.get_or_init(|| Mutex::new(BTreeSet::new()));
        if checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&key)
        {
            continue;
        }
        let scoped = if store.bound_project() == Some(&project) {
            None
        } else {
            match Store::open_with_project(&deps.paths, &project) {
                Ok(store) => Some(store),
                Err(_) => continue,
            }
        };
        let target = scoped.as_ref().unwrap_or(store);
        if target.schedule(GC_NIGHTLY_SCHEDULE)?.is_none() {
            let parsed = ScheduleSpec::parse(ScheduleSpecKind::Interval, "every 24h", "UTC")?;
            let schedule = Schedule {
                id: ScheduleId::new(),
                project: Some(project),
                name: GC_NIGHTLY_SCHEDULE.to_owned(),
                spec_kind: ScheduleSpecKind::Interval,
                spec: "every 24h".to_owned(),
                tz: "UTC".to_owned(),
                action: ScheduleAction::Command {
                    argv: vec!["vowp".to_owned(), "gc".to_owned(), "--apply".to_owned()],
                    cwd: None,
                    timeout_secs: 1_800,
                },
                missed_policy: MissedPolicy::RunOnce,
                state: ActiveState::Active,
                last_run_at: None,
                next_run_at: parsed.next_after(now),
                last_status: String::new(),
                created_at: now,
            };
            target.insert_schedule(&schedule)?;
        }
        checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key);
    }
    Ok(())
}
