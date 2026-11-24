//! Lifecycle classification: every node file and every database-derived class.
//!
//! Classes are derived from database state wherever possible, so no registration hook is needed
//! for runs, sessions, worktrees or host samples. The `catalog` table adds rows for files that
//! cannot be derived (backups, release banks, archived project directories). Both sources are
//! merged by path, and a protected class always wins over a disposable one.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10.1 and §10.2.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use vowp_protocol::{
    CatalogClass, CatalogFilter, JobId, JobState, Project, ProjectName, Session, SessionId, SessionRole, SessionState,
    Timestamp, VowpConfig,
};
use vowp_store::{JobFilter, SessionFilter, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::surface::lease::is_live;

use super::{
    CACHE_RETENTION, CLOSED_SESSION_RETENTION, ClassifiedFile, FAILED_WORKTREE_RETENTION, RUN_OUTPUT_COMPRESS_AFTER,
    RUN_OUTPUT_MAX_AGE, SAMPLE_FULL_RETENTION, SAMPLE_MINUTE_RETENTION,
};

/// What `vowp gc --apply` should do with one classified entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemAction {
    /// Keep the file; the class is protected or the entry has not expired.
    Keep,
    /// Delete one regular file.
    DeleteFile,
    /// Gzip one run-output file in place, keeping `<name>.gz`.
    CompressRunOutput,
    /// Compact one closed session and remove its full log and blobs.
    CompactSession(SessionId),
    /// Remove one git worktree; the branch stays.
    RemoveWorktree {
        /// Main checkout the worktree belongs to.
        repo: PathBuf,
        /// Worktree directory.
        worktree: PathBuf,
    },
    /// Close one tagged tmux window.
    KillWindow {
        /// Private tmux session that holds the window.
        session: String,
        /// Exact window name.
        window: String,
    },
    /// Prune the expired `host_samples` rows selected by the same cutoffs.
    PruneSamples,
}

/// One classified entry with its expiry and the action an apply pass may take.
#[derive(Debug, Clone)]
pub(crate) struct ClassifiedItem {
    /// The public classification record.
    pub(crate) file: ClassifiedFile,
    /// When the entry becomes collectable; `None` means never.
    pub(crate) expires_at: Option<Timestamp>,
    /// Action an apply pass may take once `expires_at` has passed.
    pub(crate) action: ItemAction,
}

/// Effective retention windows for one classification pass.
///
/// The defaults are the constants in [`super`]; only an explicit `vowp gc --older-than`
/// changes the two overridable classes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Retention {
    /// How long a closed session's full log and blobs are kept.
    pub(crate) closed_session: Duration,
    /// How long raw run output is kept.
    pub(crate) run_output: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            closed_session: CLOSED_SESSION_RETENTION,
            run_output: RUN_OUTPUT_MAX_AGE,
        }
    }
}

/// One candidate a GC pass left alone because a session is still owned or active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcRefused {
    /// Class the refused candidate belongs to.
    pub class: CatalogClass,
    /// Session that was refused, when the candidate is a session.
    pub session_id: Option<SessionId>,
    /// Path or row identity that was refused.
    pub path: String,
    /// Why the candidate stays.
    pub reason: String,
}

/// The classification result: entries to report and candidates a guard refused.
pub(crate) struct Classification {
    /// Classified files and their actions.
    pub(crate) items: Vec<ClassifiedItem>,
    /// Session candidates kept by a live lease, an active job or a running orchestrator.
    pub(crate) refused: Vec<GcRefused>,
}

/// Classifies every known node file by lifecycle class.
pub fn classify(deps: &Deps, store: &Store, now: Timestamp) -> Result<Vec<ClassifiedFile>, CoreError> {
    Ok(classify_items(deps, store, now, Retention::default())?
        .items
        .into_iter()
        .map(|item| item.file)
        .collect())
}

/// Returns the wire name of a lifecycle class.
pub fn class_name(class: CatalogClass) -> &'static str {
    match class {
        CatalogClass::Production => "production",
        CatalogClass::Truth => "truth",
        CatalogClass::OpenWork => "open_work",
        CatalogClass::Secrets => "secrets",
        CatalogClass::ClosedSession => "closed_session",
        CatalogClass::RunOutput => "run_output",
        CatalogClass::Worktree => "worktree",
        CatalogClass::JobWindow => "job_window",
        CatalogClass::Sample => "sample",
        CatalogClass::Cache => "cache",
    }
}

/// Parses a lifecycle class from its wire name; `open-work` is accepted as an alias.
pub fn parse_class(text: &str) -> Result<CatalogClass, CoreError> {
    let normal = text.trim().to_ascii_lowercase().replace('-', "_");
    match normal.as_str() {
        "production" => Ok(CatalogClass::Production),
        "truth" => Ok(CatalogClass::Truth),
        "open_work" => Ok(CatalogClass::OpenWork),
        "secrets" => Ok(CatalogClass::Secrets),
        "closed_session" => Ok(CatalogClass::ClosedSession),
        "run_output" => Ok(CatalogClass::RunOutput),
        "worktree" => Ok(CatalogClass::Worktree),
        "job_window" => Ok(CatalogClass::JobWindow),
        "sample" => Ok(CatalogClass::Sample),
        "cache" => Ok(CatalogClass::Cache),
        other => Err(CoreError::Invalid(format!(
            "unknown lifecycle class {other:?}; expected production, truth, open_work, secrets, \
             closed_session, run_output, worktree, job_window, sample or cache"
        ))),
    }
}

/// Returns true for the four classes `vowp gc` must never delete.
pub(crate) fn is_protected(class: CatalogClass) -> bool {
    matches!(
        class,
        CatalogClass::Production | CatalogClass::Truth | CatalogClass::OpenWork | CatalogClass::Secrets
    )
}

/// Orders classes from most protective to most disposable.
pub(crate) fn class_rank(class: CatalogClass) -> u8 {
    match class {
        CatalogClass::Production => 0,
        CatalogClass::Truth => 1,
        CatalogClass::Secrets => 2,
        CatalogClass::OpenWork => 3,
        CatalogClass::ClosedSession => 4,
        CatalogClass::RunOutput => 5,
        CatalogClass::Worktree => 6,
        CatalogClass::JobWindow => 7,
        CatalogClass::Sample => 8,
        CatalogClass::Cache => 9,
    }
}

/// Orders worktree states so the merge queue's immediate cleanup stays a special case.
fn worktree_expiry(now: Timestamp, job: &vowp_protocol::Job) -> (CatalogClass, Option<Timestamp>) {
    match job.state {
        JobState::Done => (CatalogClass::Worktree, Some(now)),
        JobState::Failed | JobState::Cancelled | JobState::Replanned => (
            CatalogClass::Worktree,
            Some(job.state_changed_at.plus(FAILED_WORKTREE_RETENTION)),
        ),
        _ => (CatalogClass::OpenWork, None),
    }
}

/// Classifies every known file and returns the internal entries used by `gc`.
pub(crate) fn classify_items(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    retention: Retention,
) -> Result<Classification, CoreError> {
    let mut classifier = Classifier::new(deps, store, now, retention);
    classifier.add_node_files()?;
    classifier.add_projects()?;
    classifier.add_job_windows();
    classifier.add_sessions()?;
    classifier.add_run_output()?;
    classifier.add_cache()?;
    classifier.add_samples()?;
    classifier.add_catalog()?;
    classifier.finish()
}

/// State shared by the classification passes.
struct Classifier<'a> {
    deps: &'a Deps,
    store: &'a Store,
    now: Timestamp,
    retention: Retention,
    items: Vec<ClassifiedItem>,
    refused: Vec<GcRefused>,
    by_path: BTreeMap<String, usize>,
    protected_blobs: BTreeSet<String>,
    projects: BTreeMap<ProjectName, Project>,
    jobs: BTreeMap<JobId, JobState>,
}

impl<'a> Classifier<'a> {
    fn new(deps: &'a Deps, store: &'a Store, now: Timestamp, retention: Retention) -> Self {
        Self {
            deps,
            store,
            now,
            retention,
            items: Vec::new(),
            refused: Vec::new(),
            by_path: BTreeMap::new(),
            protected_blobs: BTreeSet::new(),
            projects: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }

    fn finish(mut self) -> Result<Classification, CoreError> {
        self.items.sort_by(|left, right| {
            (class_rank(left.file.class), &left.file.path).cmp(&(class_rank(right.file.class), &right.file.path))
        });
        self.refused
            .sort_by(|left, right| (class_rank(left.class), &left.path).cmp(&(class_rank(right.class), &right.path)));
        Ok(Classification {
            items: self.items,
            refused: self.refused,
        })
    }

    /// Adds or merges one entry. A protected class always wins over a disposable one.
    ///
    /// The arguments mirror [`ClassifiedFile`] plus the expiry and action a GC pass needs; this
    /// is one internal call site family, not a public API, so the field list stays explicit.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        path: PathBuf,
        class: CatalogClass,
        project: Option<ProjectName>,
        bytes: u64,
        reason: String,
        expires_at: Option<Timestamp>,
        action: ItemAction,
    ) {
        if !matches!(class, CatalogClass::Sample | CatalogClass::JobWindow) && !path.exists() {
            return;
        }
        let key = path.to_string_lossy().into_owned();
        if let Some(index) = self.by_path.get(&key).copied() {
            let existing = &mut self.items[index];
            let existing_rank = class_rank(existing.file.class);
            let new_rank = class_rank(class);
            if new_rank < existing_rank {
                existing.file.class = class;
                existing.file.project = project;
                existing.file.bytes = bytes.max(existing.file.bytes);
                existing.file.reason = reason;
                existing.expires_at = expires_at;
                existing.action = action;
                return;
            }
            if new_rank == existing_rank {
                existing.expires_at = earliest(existing.expires_at, expires_at);
                existing.action = stronger_action(&existing.action, &action);
                if bytes > existing.file.bytes {
                    existing.file.bytes = bytes;
                }
            }
            return;
        }
        let index = self.items.len();
        self.by_path.insert(key, index);
        self.items.push(ClassifiedItem {
            file: ClassifiedFile {
                path,
                class,
                project,
                bytes,
                reason,
            },
            expires_at,
            action,
        });
    }

    /// Classifies the node-level files every home has.
    fn add_node_files(&mut self) -> Result<(), CoreError> {
        let root = self.deps.paths.root();
        for name in ["vowp.db", "vowp.db-wal", "vowp.db-shm", "config.toml", "migration.json"] {
            let path = root.join(name);
            if path.exists() {
                self.push(
                    path.clone(),
                    CatalogClass::Truth,
                    None,
                    file_size(&path),
                    "node database or configuration".to_owned(),
                    None,
                    ItemAction::Keep,
                );
            }
        }
        for name in ["supervisor.lock", "heartbeat", "stop", "board.json", "snapshot.json"] {
            let path = root.join(name);
            if path.exists() {
                self.push(
                    path.clone(),
                    CatalogClass::OpenWork,
                    None,
                    file_size(&path),
                    "live node state".to_owned(),
                    None,
                    ItemAction::Keep,
                );
            }
        }

        self.add_tree(
            &root.join("secrets"),
            CatalogClass::Secrets,
            None,
            "secret file; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        self.add_tree(
            &root.join("orchestrators"),
            CatalogClass::OpenWork,
            None,
            "orchestrator state; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        self.add_tree(
            &root.join("backups"),
            CatalogClass::Truth,
            None,
            "backup; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        self.add_tree(
            &root.join("gates"),
            CatalogClass::Truth,
            None,
            "gate evidence; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        self.add_tree(
            &root.join("archive"),
            CatalogClass::Truth,
            None,
            "archived project data; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        Ok(())
    }

    /// Classifies project databases, releases and job worktrees.
    fn add_projects(&mut self) -> Result<(), CoreError> {
        let projects = self.store.list_projects()?;
        for project in projects {
            self.projects.insert(project.name.clone(), project.clone());
            let project_store = match Store::open_with_project(&self.deps.paths, &project.name) {
                Ok(store) => store.with_clock(self.deps.clock.clone()),
                Err(_) => {
                    self.add_project_db_files(&project);
                    continue;
                }
            };
            self.add_project_db_files(&project);

            for job in project_store.list_jobs(&JobFilter::default())? {
                self.jobs.insert(job.id.clone(), job.state);
                let Some(worktree) = job.worktree.as_ref().map(PathBuf::from) else {
                    continue;
                };
                let (class, expires_at) = worktree_expiry(self.now, &job);
                let action = if class == CatalogClass::Worktree {
                    ItemAction::RemoveWorktree {
                        repo: PathBuf::from(&project.path),
                        worktree: worktree.clone(),
                    }
                } else {
                    ItemAction::Keep
                };
                let reason = match class {
                    CatalogClass::Worktree => {
                        format!("job {} is {}; worktree is removable", job.id, job_state_name(job.state))
                    }
                    _ => format!("job {} is {}; worktree is open work", job.id, job_state_name(job.state)),
                };
                self.push(
                    worktree.clone(),
                    class,
                    Some(project.name.clone()),
                    path_size(&worktree),
                    reason,
                    expires_at,
                    action,
                );
            }

            for release in project_store.list_releases(&project.name, None)? {
                let Some(snapshot) = release.snapshot_path else {
                    continue;
                };
                let path = PathBuf::from(snapshot);
                self.push(
                    path.clone(),
                    CatalogClass::Production,
                    Some(project.name.clone()),
                    path_size(&path),
                    format!("release {} bank", release.id),
                    None,
                    ItemAction::Keep,
                );
            }

            if let Ok(blobs) = project_store.lifecycle_passing_gate_blobs() {
                self.protected_blobs.extend(blobs);
            }

            if let Ok(config) = serde_json::from_value::<VowpConfig>(project.config.clone()) {
                for (env, config) in config.env {
                    let checkout = PathBuf::from(config.checkout);
                    self.push(
                        checkout.clone(),
                        CatalogClass::Production,
                        Some(project.name.clone()),
                        path_size(&checkout),
                        format!("environment {env} checkout"),
                        None,
                        ItemAction::Keep,
                    );
                }
            }
        }
        Ok(())
    }

    /// Adds tagged `job-<id>` windows whose job is missing or no longer active.
    ///
    /// Listing the tmux session is best effort: a home without tmux, a foreign server or a
    /// window race must not make garbage collection fail. The kill itself also refuses an
    /// untagged window, so a foreign window that happens to carry a job-like name is safe.
    fn add_job_windows(&mut self) {
        let projects: Vec<ProjectName> = self.projects.keys().cloned().collect();
        for project in projects {
            let session = vowp_tmux::session_name_for_project(project.as_str());
            let windows = match self.deps.windows.list_windows(&session) {
                Ok(windows) => windows,
                Err(error) => {
                    self.deps.log.log(
                        vowp_protocol::Level::Warn,
                        "lifecycle",
                        &format!("cannot list tmux windows for {session}: {error}"),
                    );
                    continue;
                }
            };
            for window in windows {
                if !window.tagged {
                    continue;
                }
                let Some(raw_id) = window.name.strip_prefix("job-") else {
                    continue;
                };
                let Ok(job_id) = JobId::parse(raw_id) else {
                    continue;
                };
                // Hashed long names do not parse back to the id; match them forward.
                let owner = self
                    .jobs
                    .iter()
                    .find(|(id, _)| vowp_tmux::window_name_for_job(id.as_str()) == window.name);
                let (job_id, state) = match owner {
                    Some((id, state)) => (id.clone(), Some(*state)),
                    None => (job_id, None),
                };
                if state.is_some_and(|state| state.needs_window()) {
                    continue;
                }
                let reason = match state {
                    Some(state) => format!("job {job_id} is {}", job_state_name(state)),
                    None => format!("job {job_id} no longer exists"),
                };
                self.push(
                    PathBuf::from(format!("tmux/{session}/{}", window.name)),
                    CatalogClass::JobWindow,
                    Some(project.clone()),
                    0,
                    reason,
                    Some(self.now),
                    ItemAction::KillWindow {
                        session: session.clone(),
                        window: window.name,
                    },
                );
            }
        }
    }

    /// Adds the database, WAL and lock paths of one project.
    fn add_project_db_files(&mut self, project: &Project) {
        let dir = self.deps.paths.project_dir(&project.name);
        for name in ["project.db", "project.db-wal", "project.db-shm"] {
            let path = dir.join(name);
            self.push(
                path.clone(),
                CatalogClass::Truth,
                Some(project.name.clone()),
                file_size(&path),
                "project database".to_owned(),
                None,
                ItemAction::Keep,
            );
        }
        let lock = dir.join(".lock");
        self.push(
            lock.clone(),
            CatalogClass::OpenWork,
            Some(project.name.clone()),
            file_size(&lock),
            "project database writer lock".to_owned(),
            None,
            ItemAction::Keep,
        );
    }

    /// Classifies session logs, blobs, capsules and side files.
    fn add_sessions(&mut self) -> Result<(), CoreError> {
        let sessions = self.store.list_sessions(&SessionFilter::default())?;
        for session in sessions {
            self.add_session(&session);
        }
        Ok(())
    }

    fn add_session(&mut self, session: &Session) {
        let dir = self.deps.paths.session_dir(&session.id);
        if !dir.is_dir() {
            return;
        }

        let job_done = session
            .job_id
            .as_ref()
            .and_then(|job| self.jobs.get(job))
            .is_some_and(|state| *state == JobState::Done);
        let orchestrator = session.role == Some(SessionRole::Orchestrator);
        // A stale `done` job must never make a live session compactable. Only idle or explicitly
        // closed sessions move to the closed-session class.
        let live = matches!(
            session.state,
            SessionState::Running | SessionState::Switching | SessionState::Blocked
        );
        let closed = !live && (session.state == SessionState::Closed || job_done);
        let (class, expires_at, action) = if orchestrator || !closed {
            if orchestrator
                && matches!(
                    session.state,
                    SessionState::Running | SessionState::Switching | SessionState::Blocked
                )
            {
                let expiry = session.updated_at.plus(self.retention.closed_session);
                if self.now >= expiry {
                    self.refuse(
                        session,
                        CatalogClass::OpenWork,
                        format!("is a running orchestrator ({})", session_state_name(session.state)),
                    );
                }
            }
            (CatalogClass::OpenWork, None, ItemAction::Keep)
        } else {
            let expiry = session.updated_at.plus(self.retention.closed_session);
            let reasons = if self.now >= expiry {
                self.refusal_reasons(session)
            } else {
                Vec::new()
            };
            if !reasons.is_empty() {
                self.refuse(session, CatalogClass::ClosedSession, reasons.join("; "));
            }
            let action = if self.now >= expiry && reasons.is_empty() {
                ItemAction::CompactSession(session.id.clone())
            } else {
                ItemAction::Keep
            };
            (CatalogClass::ClosedSession, Some(expiry), action)
        };
        let project = session.project.clone();
        let reason = if closed && !orchestrator {
            format!("session {} is closed", session.id)
        } else {
            format!("session {} is open work", session.id)
        };

        let log = self.deps.paths.log_path(&session.id);
        self.push(
            log.clone(),
            class,
            project.clone(),
            file_size(&log),
            reason.clone(),
            expires_at,
            action.clone(),
        );

        self.add_session_blobs(&session.id, class, project.clone(), expires_at, &action, &reason);
        let lock = self.deps.paths.lock_path(&session.id);
        self.push(
            lock.clone(),
            class,
            project.clone(),
            file_size(&lock),
            reason,
            expires_at,
            action,
        );

        self.add_tree(
            &self.deps.paths.capsules_dir(&session.id),
            CatalogClass::Truth,
            project.clone(),
            "session capsule; never auto-deleted",
            None,
            ItemAction::Keep,
        );
        for name in ["adapters.json", "summary.md", "import.json"] {
            let path = dir.join(name);
            self.push(
                path.clone(),
                CatalogClass::Truth,
                project.clone(),
                file_size(&path),
                "session side file; never auto-deleted".to_owned(),
                None,
                ItemAction::Keep,
            );
        }
    }

    /// Records a session candidate that a guard keeps out of collection.
    fn refuse(&mut self, session: &Session, class: CatalogClass, reason: String) {
        self.refused.push(GcRefused {
            class,
            session_id: Some(session.id.clone()),
            path: self.deps.paths.log_path(&session.id).display().to_string(),
            reason,
        });
    }

    /// Returns why a closed session must stay, or an empty list when it may be collected.
    fn refusal_reasons(&self, session: &Session) -> Vec<String> {
        let mut reasons = Vec::new();
        match self.store.lease(&session.id) {
            Ok(Some(lease)) if is_live(self.now, &lease) => reasons.push(format!(
                "holds a live lease ({holder} on {node}, heartbeat {heartbeat})",
                holder = lease.holder,
                node = lease.node,
                heartbeat = lease.heartbeat_at.as_rfc3339()
            )),
            Ok(_) => {}
            Err(error) => reasons.push(format!("lease state could not be read: {error}")),
        }
        if let Some(job_id) = &session.job_id
            && let Some(state) = self.jobs.get(job_id)
            && state.holds_lock()
        {
            reasons.push(format!("belongs to job {job_id}, which is {}", job_state_name(*state)));
        }
        reasons
    }

    fn add_session_blobs(
        &mut self,
        session: &SessionId,
        class: CatalogClass,
        project: Option<ProjectName>,
        expires_at: Option<Timestamp>,
        action: &ItemAction,
        reason: &str,
    ) {
        let blobs = self.deps.paths.blobs_dir(session);
        let Ok(entries) = std::fs::read_dir(&blobs) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if self.protected_blobs.contains(name) {
                self.push(
                    path.clone(),
                    CatalogClass::Truth,
                    project.clone(),
                    file_size(&path),
                    "blob is passing gate evidence; never auto-deleted".to_owned(),
                    None,
                    ItemAction::Keep,
                );
            } else {
                self.push(
                    path.clone(),
                    class,
                    project.clone(),
                    file_size(&path),
                    reason.to_owned(),
                    expires_at,
                    action.clone(),
                );
            }
        }
    }

    /// Classifies raw run output: 14-day deletion and in-place gzip after one day.
    fn add_run_output(&mut self) -> Result<(), CoreError> {
        let runs = self.deps.paths.runs_dir();
        let Ok(entries) = std::fs::read_dir(&runs) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            let base = name.strip_suffix(".gz").unwrap_or(name);
            if !(base.ends_with(".jsonl") || base.ends_with(".jsonl.err") || base.ends_with(".stderr")) {
                continue;
            }
            let compressed = name.ends_with(".gz");
            let mtime = file_mtime(&path, self.now);
            let expiry = mtime.plus(self.retention.run_output);
            let action = if self.now >= expiry {
                ItemAction::DeleteFile
            } else if !compressed && self.now >= mtime.plus(RUN_OUTPUT_COMPRESS_AFTER) {
                ItemAction::CompressRunOutput
            } else {
                ItemAction::Keep
            };
            self.push(
                path.clone(),
                CatalogClass::RunOutput,
                None,
                file_size(&path),
                format!("raw run output (mtime {mtime})"),
                Some(expiry),
                action,
            );
        }
        Ok(())
    }

    /// Classifies disposable files under `tmp/`.
    fn add_cache(&mut self) -> Result<(), CoreError> {
        let tmp = self.deps.paths.tmp_dir();
        self.add_tree_with_mtime(&tmp, CatalogClass::Cache, "temporary file", CACHE_RETENTION)?;
        Ok(())
    }

    /// Classifies expired host sample rows as one entry per row.
    fn add_samples(&mut self) -> Result<(), CoreError> {
        let full_before = self.now.minus(SAMPLE_FULL_RETENTION);
        let minute_before = self.now.minus(SAMPLE_MINUTE_RETENTION);
        for sample in self.store.lifecycle_sample_rows(full_before, minute_before)? {
            let retention = if sample.interval_secs < 60 {
                SAMPLE_FULL_RETENTION
            } else {
                SAMPLE_MINUTE_RETENTION
            };
            self.push(
                PathBuf::from(format!("host_samples/{}", sample.id)),
                CatalogClass::Sample,
                None,
                0,
                format!(
                    "host sample at {} ({}s interval)",
                    sample.at.as_rfc3339(),
                    sample.interval_secs
                ),
                Some(sample.at.plus(retention)),
                ItemAction::PruneSamples,
            );
        }
        Ok(())
    }

    /// Merges durable catalog rows for files that cannot be derived from other tables.
    fn add_catalog(&mut self) -> Result<(), CoreError> {
        for entry in self.store.list_catalog(&CatalogFilter::default())? {
            if entry.deleted_at.is_some() {
                continue;
            }
            let path = PathBuf::from(&entry.path);
            if !path.exists() {
                continue;
            }
            let key = path.to_string_lossy().into_owned();
            if let Some(index) = self.by_path.get(&key).copied()
                && class_rank(entry.class) >= class_rank(self.items[index].file.class)
            {
                // The class derived from database state or a real file wins over the catalog.
                // A catalog row may only strengthen protection, never downgrade it.
                continue;
            }
            let expires_at = entry
                .expires_at
                .or_else(|| default_catalog_expiry(entry.class, entry.created_at, self.retention));
            let (action, reason) = if is_protected(entry.class) {
                (
                    ItemAction::Keep,
                    format!("catalog row: protected {}", class_name(entry.class)),
                )
            } else {
                match entry.class {
                    CatalogClass::Worktree => match entry
                        .project
                        .as_ref()
                        .and_then(|name| self.projects.get(name))
                        .map(|project| PathBuf::from(&project.path))
                    {
                        Some(project) => (
                            ItemAction::RemoveWorktree {
                                repo: project,
                                worktree: path.clone(),
                            },
                            "catalog row: expired worktree".to_owned(),
                        ),
                        None => (ItemAction::Keep, "catalog worktree has no project".to_owned()),
                    },
                    CatalogClass::Sample => (ItemAction::PruneSamples, "catalog row: expired sample".to_owned()),
                    _ => (
                        ItemAction::DeleteFile,
                        format!("catalog row: expired {}", class_name(entry.class)),
                    ),
                }
            };
            let bytes = entry.bytes.unwrap_or_else(|| path_size(&path));
            self.push(path, entry.class, entry.project, bytes, reason, expires_at, action);
        }
        Ok(())
    }

    /// Adds every regular file under `dir` with a fixed class and expiry.
    fn add_tree(
        &mut self,
        dir: &Path,
        class: CatalogClass,
        project: Option<ProjectName>,
        reason: &str,
        expires_at: Option<Timestamp>,
        action: ItemAction,
    ) {
        for path in regular_files(dir) {
            self.push(
                path.clone(),
                class,
                project.clone(),
                file_size(&path),
                reason.to_owned(),
                expires_at,
                action.clone(),
            );
        }
    }

    /// Adds every regular file under `dir`, expiring it `retention` after its mtime.
    fn add_tree_with_mtime(
        &mut self,
        dir: &Path,
        class: CatalogClass,
        reason: &str,
        retention: std::time::Duration,
    ) -> Result<(), CoreError> {
        for path in regular_files(dir) {
            let expiry = file_mtime(&path, self.now).plus(retention);
            let action = if self.now >= expiry {
                ItemAction::DeleteFile
            } else {
                ItemAction::Keep
            };
            self.push(
                path.clone(),
                class,
                None,
                file_size(&path),
                reason.to_owned(),
                Some(expiry),
                action,
            );
        }
        Ok(())
    }
}

/// Returns all regular files below `dir`, without following symlinks.
fn regular_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }
    files
}

/// Returns the size of a file, or zero when it disappeared.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Returns the recursive size of a file or directory without following symlinks.
fn path_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0_u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
            }
        }
    }
    total
}

/// Returns a file's modification time, falling back to `now` for unreadable metadata.
fn file_mtime(path: &Path, now: Timestamp) -> Timestamp {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(system_time_to_timestamp)
        .unwrap_or(now)
}

/// Converts a `SystemTime` into the canonical millisecond timestamp.
pub(crate) fn system_time_to_timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
            Timestamp::from_unix_millis(millis)
        }
        Err(_) => Timestamp::from_unix_millis(0),
    }
}

/// Returns the earlier of two optional expiries.
fn earliest(left: Option<Timestamp>, right: Option<Timestamp>) -> Option<Timestamp> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

/// Picks the action that does more work when two sources describe the same path.
fn stronger_action(left: &ItemAction, right: &ItemAction) -> ItemAction {
    fn rank(action: &ItemAction) -> u8 {
        match action {
            ItemAction::Keep => 0,
            ItemAction::CompressRunOutput => 1,
            ItemAction::PruneSamples => 2,
            ItemAction::RemoveWorktree { .. } => 3,
            ItemAction::CompactSession(_) => 4,
            ItemAction::DeleteFile => 5,
            ItemAction::KillWindow { .. } => 6,
        }
    }
    if rank(right) > rank(left) {
        right.clone()
    } else {
        left.clone()
    }
}

/// Default expiry for a catalog row that did not carry one.
fn default_catalog_expiry(class: CatalogClass, created_at: Timestamp, retention: Retention) -> Option<Timestamp> {
    match class {
        CatalogClass::ClosedSession => Some(created_at.plus(retention.closed_session)),
        CatalogClass::RunOutput => Some(created_at.plus(retention.run_output)),
        CatalogClass::Worktree => Some(created_at.plus(FAILED_WORKTREE_RETENTION)),
        CatalogClass::JobWindow => Some(created_at),
        CatalogClass::Sample => Some(created_at.plus(SAMPLE_FULL_RETENTION)),
        CatalogClass::Cache => Some(created_at.plus(CACHE_RETENTION)),
        CatalogClass::Production | CatalogClass::Truth | CatalogClass::OpenWork | CatalogClass::Secrets => None,
    }
}

/// Returns a human name for a job state in a reason string.
fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::Ready => "ready",
        JobState::Running => "running",
        JobState::Stalled => "stalled",
        JobState::Gating => "gating",
        JobState::Reviewing => "reviewing",
        JobState::Fixing => "fixing",
        JobState::Merging => "merging",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Parked => "parked",
        JobState::Replanned => "replanned",
        JobState::Cancelled => "cancelled",
    }
}

/// Returns a human name for a session state in a reason string.
fn session_state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Running => "running",
        SessionState::Blocked => "blocked",
        SessionState::Switching => "switching",
        SessionState::Closed => "closed",
    }
}
