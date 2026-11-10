//! The merge queue: rebase one `merging` job, re-run repo-wide checks, fast-forward and push.
//!
//! Spec: `docs/design/loops.md` §6. The queue performs the git and check work only; the
//! supervisor applies the resulting `MergeSucceeded` / `MergeConflict` / `MergeChecksFailed`
//! event to the job row.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vowp_protocol::{CmdGate, Gate, GateResult, InboxKind, Job, JobId, JobKind, JobState, LedgerEntry, LedgerKind};
use vowp_store::{JobFilter, Store};

use crate::CoreError;
use crate::deps::{CommandSpec, Deps, RebaseResult};
use crate::gates::{GateCtx, GateRunner};
use crate::ledger::Ledger;

/// The result of one merge-queue step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStep {
    /// Nothing was merging.
    Idle,
    /// The job was merged (and pushed when configured).
    Merged(JobId),
    /// The rebase conflicted on `files`.
    Conflict {
        /// Job that needs a merge-fix child.
        job: JobId,
        /// Conflicted paths.
        files: Vec<String>,
    },
    /// The repo-wide checks or the fast-forward failed.
    ChecksFailed {
        /// Job returned to `fixing`.
        job: JobId,
    },
    /// Another merge is already running.
    InProgress,
    /// The main checkout was not clean; the job must be parked manually.
    Manual {
        /// Job to park.
        job: JobId,
        /// Why the queue refused to touch the checkout.
        detail: String,
    },
}

/// One-project merge queue.
pub struct MergeQueue {
    deps: Deps,
    gate_runner: GateRunner,
    ledger: Ledger,
}

impl MergeQueue {
    /// Creates a queue over the injected dependencies.
    pub fn new(deps: Deps) -> Self {
        Self {
            gate_runner: GateRunner::new(deps.clone()),
            ledger: Ledger::new(deps.clone()),
            deps,
        }
    }

    /// Runs one merge for the oldest `merging` job, if any.
    pub fn step(&self, store: &Store) -> Result<MergeStep, CoreError> {
        let mut merging = store.list_jobs(&JobFilter {
            project: Some(self.deps.config.project.name.clone()),
            states: vec![JobState::Merging],
            ..JobFilter::default()
        })?;
        if merging.is_empty() {
            return Ok(MergeStep::Idle);
        }
        merging.sort_by(|left, right| {
            left.state_changed_at
                .cmp(&right.state_changed_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        let job = merging.remove(0);
        if matches!(job.kind, JobKind::Verify | JobKind::Fix | JobKind::Merge) {
            return Ok(MergeStep::Idle);
        }
        self.merge_one(store, &job)
    }

    /// Merges one job; returns the event the supervisor must apply.
    fn merge_one(&self, store: &Store, job: &Job) -> Result<MergeStep, CoreError> {
        let Some(worktree) = job.worktree.as_ref().map(PathBuf::from) else {
            return Ok(MergeStep::ChecksFailed { job: job.id.clone() });
        };
        let Some(branch) = job.branch.clone() else {
            return Ok(MergeStep::ChecksFailed { job: job.id.clone() });
        };
        let repo = super::main_repo(&self.deps, store, &job.project)?;
        let remote = self.deps.config.project.remote.clone();
        let base = self.deps.config.project.default_branch.clone();
        let base_ref = format!("{remote}/{base}");
        let trailer = job_trailer(store, job);
        let start = Instant::now();

        let mut push_attempts = 0u32;
        loop {
            push_attempts = push_attempts.saturating_add(1);
            self.deps.git.fetch(&repo, &remote)?;
            let main_before = self.deps.git.head_sha(&repo, &base_ref)?;
            let rebase_start = Instant::now();
            let mut lockfile_refreshed = false;
            if self.deps.config.merge.rebase {
                match self.deps.git.rebase(&worktree, &base_ref)? {
                    RebaseResult::Clean => {}
                    RebaseResult::Conflict { files } => {
                        if files.iter().all(|file| is_lockfile(&self.deps, file)) {
                            if let Err(step) =
                                self.continue_lockfile_rebase(&worktree, &base_ref, &files, job, &trailer)
                            {
                                return Ok(step);
                            }
                            lockfile_refreshed = true;
                        } else {
                            self.deps.git.rebase_abort(&worktree)?;
                            return Ok(MergeStep::Conflict {
                                job: job.id.clone(),
                                files,
                            });
                        }
                    }
                }
            }
            let rebase_ms = millis(rebase_start.elapsed());

            let checks_start = Instant::now();
            let ctx = GateCtx {
                deps: &self.deps,
                job,
                run_id: store.last_run_of_job(&job.id)?.map(|run| run.id),
                worktree: &worktree,
                repo: &repo,
                store,
                attempt: job.attempts,
            };
            let results = self.gate_runner.evaluate_merge_checks(&ctx)?;
            for result in &results {
                self.ledger.record_gate(store, result)?;
            }
            let checks_ms = millis(checks_start.elapsed());
            if results.iter().any(|result| !result.passed) {
                return Ok(MergeStep::ChecksFailed { job: job.id.clone() });
            }

            if !self.deps.git.status_clean(&repo)? {
                let detail = format!("main checkout {} is not clean", repo.display());
                self.record_merge_failure(store, job, &detail)?;
                super::open_inbox(
                    store,
                    &self.deps,
                    super::NewInbox {
                        project: Some(job.project.clone()),
                        job_id: Some(job.id.clone()),
                        kind: InboxKind::Notice,
                        title: format!("merge blocked: {}", job.id),
                        body: format!(
                            "the main checkout is not clean; clean it and resolve the job manually. {detail}"
                        ),
                        options: vec!["resolved".to_owned()],
                    },
                )?;
                return Ok(MergeStep::Manual {
                    job: job.id.clone(),
                    detail,
                });
            }
            if self.deps.git.merge_ff_only(&repo, &branch).is_err() {
                return Ok(MergeStep::ChecksFailed { job: job.id.clone() });
            }

            if self.deps.config.project.push {
                if let Err(error) = self.deps.git.push(&repo, &remote, &base) {
                    self.deps.log.log(
                        vowp_protocol::Level::Warn,
                        "merge",
                        &format!("push rejected for {} ({}), retrying", job.id, error),
                    );
                    self.deps.git.reset_hard(&repo, &main_before)?;
                    if push_attempts >= 3 {
                        return Ok(MergeStep::ChecksFailed { job: job.id.clone() });
                    }
                    continue;
                }
            }

            let main_after = self
                .deps
                .git
                .head_sha(&repo, "HEAD")
                .unwrap_or_else(|_| main_before.clone());
            let files = self
                .deps
                .git
                .diff_names(&repo, &main_before, &main_after)
                .unwrap_or_default();
            self.record_merge(
                store,
                job,
                &main_before,
                &main_after,
                files.len(),
                rebase_ms,
                checks_ms,
                millis(start.elapsed()),
                lockfile_refreshed,
            )?;
            if let Err(error) = self.deps.git.worktree_remove(&repo, &worktree) {
                self.deps.log.log(
                    vowp_protocol::Level::Warn,
                    "merge",
                    &format!("cannot remove worktree {}: {error}", worktree.display()),
                );
            }
            let _ = self.deps.git.branch_delete(&repo, &branch);
            let _ = self.deps.windows.kill_window(
                &vowp_tmux::session_name_for_project(job.project.as_str()),
                &super::window_name(&job.id),
            );
            return Ok(MergeStep::Merged(job.id.clone()));
        }
    }

    /// Takes the main version of a lockfile-only conflict and continues the rebase.
    fn continue_lockfile_rebase(
        &self,
        worktree: &Path,
        base_ref: &str,
        files: &[String],
        job: &Job,
        trailer: &str,
    ) -> Result<(), MergeStep> {
        for file in files {
            self.deps
                .git
                .checkout_path(worktree, base_ref, file)
                .map_err(|_| MergeStep::ChecksFailed { job: job.id.clone() })?;
        }
        self.deps
            .git
            .add(worktree, files)
            .map_err(|_| MergeStep::ChecksFailed { job: job.id.clone() })?;
        match self.deps.git.rebase_continue(worktree) {
            Ok(RebaseResult::Clean) => {}
            Ok(RebaseResult::Conflict { .. }) | Err(_) => {
                let _ = self.deps.git.rebase_abort(worktree);
                return Err(MergeStep::ChecksFailed { job: job.id.clone() });
            }
        }
        let _ = self.refresh_lockfile(worktree, trailer);
        Ok(())
    }

    /// Regenerates `Cargo.lock` with `cargo update -w` when the worktree has a manifest.
    fn refresh_lockfile(&self, worktree: &Path, trailer: &str) -> Result<(), CoreError> {
        if !worktree.join("Cargo.toml").is_file() {
            return Ok(());
        }
        let _ = self.deps.cmd.run(&CommandSpec {
            shell: None,
            argv: vec!["cargo".to_owned(), "update".to_owned(), "-w".to_owned()],
            cwd: Some(worktree.to_path_buf()),
            env: Vec::new(),
            timeout: Duration::from_secs(self.deps.config.merge.timeout_secs.max(1)),
        })?;
        if !self.deps.git.status_clean(worktree)? {
            self.deps.git.add(worktree, &["Cargo.lock".to_owned()])?;
            self.deps
                .git
                .commit(worktree, &format!("chore: refresh lockfile (merge queue)\n\n{trailer}"))?;
        }
        Ok(())
    }

    /// Appends a ledger `merge` row for a refused merge.
    fn record_merge_failure(&self, store: &Store, job: &Job, detail: &str) -> Result<(), CoreError> {
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Merge,
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("merge blocked {}: {detail}", job.id),
            detail: serde_json::json!({"failure": detail}),
        };
        self.ledger.record(store, entry)?;
        Ok(())
    }

    /// Appends the ledger `merge` row for a successful merge.
    #[allow(clippy::too_many_arguments)]
    fn record_merge(
        &self,
        store: &Store,
        job: &Job,
        main_before: &str,
        main_after: &str,
        files_changed: usize,
        rebase_ms: u64,
        checks_ms: u64,
        total_ms: u64,
        lockfile_refreshed: bool,
    ) -> Result<(), CoreError> {
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Merge,
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("merged {}", job.id),
            detail: serde_json::json!({
                "main_before": main_before,
                "main_after": main_after,
                "files_changed": files_changed,
                "rebase_ms": rebase_ms,
                "checks_ms": checks_ms,
                "total_ms": total_ms,
                "lockfile_refreshed": lockfile_refreshed,
            }),
        };
        self.ledger.record(store, entry)?;
        Ok(())
    }
}

/// Returns the AI trailer for a queue-authored commit, using the job's last executor.
fn job_trailer(store: &Store, job: &Job) -> String {
    let executor = store
        .last_run_of_job(&job.id)
        .ok()
        .flatten()
        .and_then(|run| run.executor)
        .map(|executor| executor.to_string())
        .unwrap_or_else(|| "supervisor".to_owned());
    format!("AI-Assisted: {executor} via VOWP")
}

/// Returns true when `path` is one of the project's regenerated lockfiles.
fn is_lockfile(deps: &Deps, path: &str) -> bool {
    deps.config.project.lock_files.iter().any(|lock| lock == path)
}

/// Converts a duration to whole milliseconds.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Builds the repo-wide check gate for a single command (used by tests and callers that want
/// the same shape as `GateRunner::evaluate_merge_checks`).
pub fn merge_check_gate(command: &str, timeout_secs: u64) -> Gate {
    Gate::Cmd(CmdGate {
        run: command.to_owned(),
        expect_exit: 0,
        expect_pattern: None,
        timeout_secs: timeout_secs.max(1),
        cwd: None,
    })
}

/// Returns the first failed merge-check result, for the supervisor's findings.
pub fn first_failure(results: &[GateResult]) -> Option<&GateResult> {
    results.iter().find(|result| !result.passed)
}
