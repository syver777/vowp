//! Capacity admission: the node-wide gate before any run starts.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.2. The rules are deterministic and
//! ordered: brake, provider limit, target, RAM, CPU, disk. A wait never changes a job's
//! state; [`crate::loops::dispatch::admit`] records the reason on the job note.

use std::collections::HashMap;
use std::time::Duration;

use vowp_protocol::{
    AdmitDecision, AdmitRequest, CapacityAdvice, CapacityReport, CapacityTarget, HostSample, InboxId, InboxItem,
    InboxKind, InboxState, Job, JobId, JobKind, JobState, ParkedReason, ProjectName, Run, RunRole, TargetMode,
    Timestamp, UsageEstimate, WaitCode, WaitReason, WaitingJob, WorkerKind,
};
use vowp_store::StateChange;
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

use super::advisor::AdviceTrigger;
use super::scope::{DEFAULT_MEAN_CPU_CORES, DEFAULT_PEAK_RSS_BYTES};
use super::{brakes, estimates, targets};

/// One gibibyte, the unit of the spec's capacity defaults.
const GIB: u64 = 1024 * 1024 * 1024;

/// Minimum free disk kept for the node.
///
/// The §6.2 minimum is `max(5 GiB, 10% of the filesystem)`; `HostSample` has no filesystem
/// total yet, so only the 5 GiB floor can be enforced here. See `brakes::MIN_DISK_BYTES`.
pub const MIN_DISK_BYTES: u64 = 5 * GIB;

/// Fraction of the CPU quota admitted runs may use.
pub const CPU_HEADROOM: f64 = 0.85;

/// How long an OOM kill keeps raising the estimate for the next admission.
const OOM_RAISE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Fraction added to the p90 RAM estimate after a kernel OOM kill.
const OOM_RAISE_FRACTION: u64 = 4;

/// The key that records the most recently admitted project for fairness.
const LAST_PROJECT_KEY: &str = "capacity:last_project";

/// Returns whether one run may start.
pub fn admit(deps: &Deps, store: &Store, req: &AdmitRequest) -> Result<AdmitDecision, CoreError> {
    CapacityManager::new(deps.clone()).admit(store, req, deps.clock.now())
}

/// Builds the admission request for `job`.
pub fn request_for(deps: &Deps, store: &Store, job: &Job) -> Result<AdmitRequest, CoreError> {
    let now = deps.clock.now();
    let limited: HashMap<String, Option<Timestamp>> = store
        .active_limits(now)?
        .into_iter()
        .map(|limit| (limit.provider, limit.resets_at))
        .collect();
    let profile = deps.config.profiles.get(&job.profile);
    let executor = profile
        .and_then(|profile| profile.pick(&limited, now))
        .map(|(_, executor)| executor.clone())
        .or_else(|| profile.and_then(|profile| profile.ladder.first().cloned()));
    let node = profile
        .and_then(|profile| profile.node.clone())
        .unwrap_or_else(|| deps.node.clone());
    Ok(AdmitRequest {
        project: job.project.clone(),
        job: Some(job.id.clone()),
        kind: job.kind,
        role: match job.role {
            vowp_protocol::Role::Planner => RunRole::Planner,
            vowp_protocol::Role::Keeper => RunRole::Keeper,
            vowp_protocol::Role::Worker => RunRole::Worker,
            vowp_protocol::Role::Verifier => RunRole::Verifier,
        },
        harness: executor.as_ref().map(|executor| executor.harness),
        model: executor.as_ref().map(|executor| executor.model.clone()),
        provider: executor.map(|executor| executor.provider()),
        node,
    })
}

/// The node-wide capacity manager.
pub struct CapacityManager {
    deps: Deps,
}

impl CapacityManager {
    /// Creates a manager over `deps`.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Samples the node, refreshes estimates and handles OOM-killed runs.
    pub fn sample(&self, store: &Store, now: Timestamp) -> Result<Option<HostSample>, CoreError> {
        let sample = super::sampler::sample(&self.deps, store, now)?;
        let _ = self.recompute_estimates(store, now)?;
        self.handle_oom_kills(store, now)?;
        let target = targets::current_for(&self.deps, store, now)?;
        self.record_auto_target(store, &target, now)?;
        if target.target_workers > self.deps.config.node.max_workers {
            self.ensure_worker_slots(store, target.target_workers)?;
        }
        Ok(sample)
    }

    /// Builds the `capacity show` report.
    pub fn report(&self, store: &Store, now: Timestamp) -> Result<CapacityReport, CoreError> {
        self.report_filtered(store, now, None, None)
    }

    /// Builds a report with the `--since` sample filter and `--limit` estimate cap.
    pub fn report_filtered(
        &self,
        store: &Store,
        now: Timestamp,
        since: Option<Timestamp>,
        limit: Option<u32>,
    ) -> Result<CapacityReport, CoreError> {
        let host = latest_sample(store, since)?;
        let target = targets::current_for(&self.deps, store, now)?;
        let running = self.running_capacity(store)?;
        let mut estimates = store.list_usage_estimates(None)?;
        estimates.sort_by(|left, right| {
            right
                .ram_p90_bytes
                .cmp(&left.ram_p90_bytes)
                .then_with(|| right.samples.cmp(&left.samples))
        });
        if let Some(limit) = limit {
            estimates.truncate(limit as usize);
        }
        let reserve_ram_bytes = host
            .as_ref()
            .map(|sample| targets::reserve_ram_bytes(sample.mem_total_bytes))
            .unwrap_or(targets::RESERVE_RAM_MIN_BYTES);
        Ok(CapacityReport {
            generated_at: now,
            host,
            target,
            running,
            brakes: brakes::evaluate_with_config(&self.deps, store, now)?,
            reserve_ram_bytes,
            min_disk_bytes: MIN_DISK_BYTES,
            estimates,
            waiting: self.waiting_jobs(store, now)?,
            limits: store.active_limits(now)?,
        })
    }

    /// Returns whether one run may start.
    pub fn admit(&self, store: &Store, req: &AdmitRequest, now: Timestamp) -> Result<AdmitDecision, CoreError> {
        self.maintain(store, now)?;
        let brake = brakes::evaluate_with_config(&self.deps, store, now)?;
        if brake.active {
            let detail = brake
                .reasons
                .first()
                .map(|reason| reason.detail.clone())
                .unwrap_or_else(|| "brake active".to_owned());
            return self.wait(
                store,
                req,
                now,
                WaitReason {
                    code: WaitCode::Brake,
                    detail,
                },
            );
        }

        let limits = store.active_limits(now)?;
        if let Some(provider) = &req.provider
            && let Some(limit) = limits.iter().find(|limit| &limit.provider == provider)
        {
            return self.wait(
                store,
                req,
                now,
                WaitReason {
                    code: WaitCode::Provider,
                    detail: format!("provider {provider} limited: {}", limit.detail),
                },
            );
        }

        let target = targets::current_for(&self.deps, store, now)?;
        let running = self.running_capacity(store)?;
        if running.runs >= target.target_workers {
            return self.wait(
                store,
                req,
                now,
                WaitReason {
                    code: WaitCode::Target,
                    detail: format!(
                        "{} worker(s) running; target is {}",
                        running.runs, target.target_workers
                    ),
                },
            );
        }

        let mut estimate = estimates::estimate_for(store, req)?;
        if consume_oom_raise(store, req, now)? {
            let raise = estimate.ram_p90_bytes / OOM_RAISE_FRACTION;
            estimate.ram_p90_bytes = estimate.ram_p90_bytes.saturating_add(raise);
        }

        if let Some(sample) = latest_sample(store, None)?
            && sample.source == vowp_protocol::SampleSource::Proc
            && sample.mem_total_bytes > 0
        {
            let reserve = targets::reserve_ram_bytes(sample.mem_total_bytes);
            let free_after = sample.mem_available_bytes.saturating_sub(estimate.ram_p90_bytes);
            if free_after < reserve {
                let short = reserve.saturating_sub(free_after);
                return self.wait(
                    store,
                    req,
                    now,
                    WaitReason {
                        code: WaitCode::Ram,
                        detail: format!("RAM {:.1} GB short", short as f64 / GIB as f64),
                    },
                );
            }
            let quota = sample
                .cpu_quota_cores
                .unwrap_or_else(|| f64::from(sample.cpu_count.max(1)))
                .max(0.01);
            let demand = running.cpu_p90_cores + estimate.cpu_p90_cores;
            if demand / quota > CPU_HEADROOM {
                let allowed = quota * CPU_HEADROOM;
                let short = (demand - allowed).max(0.0);
                return self.wait(
                    store,
                    req,
                    now,
                    WaitReason {
                        code: WaitCode::Cpu,
                        detail: format!("CPU {short:.2} core(s) short"),
                    },
                );
            }
        }

        if let Some(sample) = latest_sample(store, None)? {
            let disk_short = sample.disk_free_home_bytes < MIN_DISK_BYTES
                || sample
                    .disk_free_worktrees_bytes
                    .is_some_and(|free| free < MIN_DISK_BYTES);
            if disk_short {
                let free = sample
                    .disk_free_worktrees_bytes
                    .map(|free| free.min(sample.disk_free_home_bytes))
                    .unwrap_or(sample.disk_free_home_bytes);
                return self.wait(
                    store,
                    req,
                    now,
                    WaitReason {
                        code: WaitCode::Disk,
                        detail: format!(
                            "disk {:.1} GB free; minimum is {:.1} GB",
                            free as f64 / GIB as f64,
                            MIN_DISK_BYTES as f64 / GIB as f64
                        ),
                    },
                );
            }
        }

        self.clear_wait(store, req)?;
        store.kv_set(LAST_PROJECT_KEY, req.project.as_str())?;
        if target.target_workers > self.deps.config.node.max_workers {
            self.ensure_worker_slots(store, target.target_workers)?;
        }
        Ok(AdmitDecision::Admit)
    }

    /// Writes one capacity target row.
    pub fn set_target(
        &self,
        store: &Store,
        mode: TargetMode,
        target: u32,
        is_override: bool,
        by: &str,
        reason: &str,
    ) -> Result<CapacityTarget, CoreError> {
        let now = self.deps.clock.now();
        let advised = targets::current_for(&self.deps, store, now)?
            .advised_workers
            .unwrap_or(targets::MIN_TARGET);
        let target_workers = match mode {
            TargetMode::Auto => advised,
            TargetMode::Fixed => target.max(targets::MIN_TARGET),
        };
        if mode == TargetMode::Fixed && target_workers > advised && !is_override {
            return Err(CoreError::Refused(format!(
                "target {target_workers} is above the advised {advised}; pass --override to accept it"
            )));
        }
        let written = CapacityTarget {
            mode,
            target_workers,
            advised_workers: Some(advised),
            is_override: is_override && target_workers > advised,
            set_by: by.to_owned(),
            set_at: now,
            reason: reason.to_owned(),
        };
        store.insert_capacity_target(&written)?;
        if let Some(advice) = store.latest_open_advice()?
            && advice.proposed_target == target_workers
        {
            let _ = store.finish_capacity_advice(&advice.id, vowp_protocol::AdviceState::Applied, by, now);
        }
        self.ensure_worker_slots(store, target_workers)?;
        Ok(written)
    }

    /// Produces one capacity advice row, when the advisor decides to.
    ///
    /// This is the per-trigger API; [`super::advisor::advise_tick`] is the node supervisor's
    /// one-call entry point for the timer and the four events.
    pub fn advise(
        &self,
        store: &Store,
        now: Timestamp,
        trigger: AdviceTrigger,
    ) -> Result<Option<CapacityAdvice>, CoreError> {
        super::advisor::maybe_advise(&self.deps, store, now, trigger)
    }

    /// Recomputes every pooled usage estimate.
    pub fn recompute_estimates(&self, store: &Store, now: Timestamp) -> Result<u32, CoreError> {
        estimates::recompute(store, now)
    }

    /// Refreshes the admission side effects before one dispatch pass.
    ///
    /// The node tick already calls [`CapacityManager::sample`] for estimates, OOM handling,
    /// auto-target history and worker slots. An admission can land between ticks, so this
    /// repeats that maintenance on the dispatch path and keeps the estimate recompute
    /// throttled to once a minute.
    fn maintain(&self, store: &Store, now: Timestamp) -> Result<(), CoreError> {
        self.refresh_estimates(store, now)?;
        self.handle_oom_kills(store, now)?;
        let target = targets::current_for(&self.deps, store, now)?;
        self.record_auto_target(store, &target, now)?;
        if target.target_workers > self.deps.config.node.max_workers {
            self.ensure_worker_slots(store, target.target_workers)?;
        }
        Ok(())
    }

    /// Records the first wait time and returns the wait decision.
    ///
    /// `dispatch::admit` rewrites the note every tick, so the note timestamp cannot tell
    /// when the job first waited. The `kv` marker keeps that instant stable until the job
    /// is admitted (or the note disappears).
    fn wait(
        &self,
        store: &Store,
        req: &AdmitRequest,
        now: Timestamp,
        reason: WaitReason,
    ) -> Result<AdmitDecision, CoreError> {
        if let Some(job) = &req.job {
            let key = wait_key(&req.project, job);
            if store.kv_get(&key)?.is_none() {
                store.kv_set(&key, &now.as_rfc3339())?;
            }
        }
        Ok(AdmitDecision::Wait(reason))
    }

    /// Removes the first-wait marker after a successful admission.
    fn clear_wait(&self, store: &Store, req: &AdmitRequest) -> Result<(), CoreError> {
        if let Some(job) = &req.job {
            store.kv_delete(&wait_key(&req.project, job))?;
        }
        Ok(())
    }

    /// Returns the first wait instant for one job, creating the marker when it is missing.
    fn wait_since(
        &self,
        store: &Store,
        project: &ProjectName,
        job: &Job,
        now: Timestamp,
    ) -> Result<Timestamp, CoreError> {
        let key = wait_key(project, &job.id);
        if let Some(text) = store.kv_get(&key)?
            && let Ok(since) = Timestamp::parse(&text)
        {
            return Ok(since);
        }
        // A note written before this marker existed still gets a stable instant; the old
        // `updated_at` is the best first-wait estimate and the next rewrite will not move it.
        let since = if job.updated_at <= now { job.updated_at } else { now };
        store.kv_set(&key, &since.as_rfc3339())?;
        Ok(since)
    }

    /// Recomputes estimates at most once a minute.
    fn refresh_estimates(&self, store: &Store, now: Timestamp) -> Result<(), CoreError> {
        const KEY: &str = "capacity:estimates_at";
        if let Some(text) = store.kv_get(KEY)?
            && let Ok(last) = Timestamp::parse(&text)
            && now.since(last) < Duration::from_secs(60)
        {
            return Ok(());
        }
        self.recompute_estimates(store, now)?;
        store.kv_set(KEY, &now.as_rfc3339())?;
        Ok(())
    }

    /// Sums the open runs and their p90 estimates across every active project.
    fn running_capacity(&self, store: &Store) -> Result<vowp_protocol::RunningCapacity, CoreError> {
        let mut running = vowp_protocol::RunningCapacity::default();
        for (project, run, kind) in open_runs(store)? {
            let request = AdmitRequest {
                project,
                job: run.job_id.clone(),
                kind,
                role: run.role,
                harness: run.executor.as_ref().map(|executor| executor.harness),
                model: run.executor.as_ref().map(|executor| executor.model.clone()),
                provider: run.provider.clone(),
                node: run.node.clone(),
            };
            let estimate = estimates::estimate_for(store, &request).unwrap_or_else(|_| default_estimate());
            running.runs = running.runs.saturating_add(1);
            running.ram_p90_bytes = running.ram_p90_bytes.saturating_add(estimate.ram_p90_bytes);
            running.cpu_p90_cores += estimate.cpu_p90_cores;
        }
        Ok(running)
    }

    /// Collects jobs whose note carries a `waiting:` line.
    fn waiting_jobs(&self, store: &Store, now: Timestamp) -> Result<Vec<WaitingJob>, CoreError> {
        let mut waiting = Vec::new();
        for (project, job) in all_jobs(store)? {
            let key = wait_key(&project, &job.id);
            let note = job.note.trim();
            let Some(detail) = note.strip_prefix("waiting:") else {
                // The note was cleared (or never existed); a later wait starts a new clock.
                store.kv_delete(&key)?;
                continue;
            };
            let detail = detail.trim();
            if detail.is_empty() {
                store.kv_delete(&key)?;
                continue;
            }
            waiting.push(WaitingJob {
                project: project.clone(),
                job: job.id.clone(),
                reason: WaitReason {
                    code: wait_code(detail),
                    detail: detail.to_owned(),
                },
                since: self.wait_since(store, &project, &job, now)?,
            });
        }
        Ok(waiting)
    }

    /// Records a supervisor auto-target row when the measurement moved by more than one.
    fn record_auto_target(&self, store: &Store, target: &CapacityTarget, now: Timestamp) -> Result<(), CoreError> {
        if target.mode != TargetMode::Auto {
            return Ok(());
        }
        let latest = store.latest_capacity_target()?;
        let moved = latest
            .as_ref()
            .is_none_or(|latest| latest.target_workers.abs_diff(target.target_workers) > 1);
        if moved {
            store.insert_capacity_target(&CapacityTarget {
                mode: TargetMode::Auto,
                target_workers: target.target_workers,
                advised_workers: target.advised_workers,
                is_override: false,
                set_by: "supervisor".to_owned(),
                set_at: now,
                reason: "auto target from measurements".to_owned(),
            })?;
        }
        Ok(())
    }

    /// Grows worker slots to the owner's target when it exceeds the config preference.
    fn ensure_worker_slots(&self, store: &Store, target: u32) -> Result<(), CoreError> {
        let count = self.deps.config.node.max_workers.max(target);
        if store.bound_project().is_some() {
            store.ensure_worker_slots(&self.deps.node, WorkerKind::Worker, count)?;
            return Ok(());
        }
        let Some(paths) = store.paths() else {
            return Ok(());
        };
        for entry in store.list_project_entries()? {
            if entry.state == vowp_protocol::ProjectState::Archived {
                continue;
            }
            let Ok(project_store) = Store::open_with_project(paths, &entry.name) else {
                continue;
            };
            let _ = project_store.ensure_worker_slots(&self.deps.node, WorkerKind::Worker, count);
        }
        Ok(())
    }

    /// Parks a job whose run the kernel OOM-killed, once, and writes the inbox notice.
    fn handle_oom_kills(&self, store: &Store, now: Timestamp) -> Result<(), CoreError> {
        let rows = store.run_usage_for_group(
            &vowp_store::repo_capacity::UsageQuery {
                since: Some(now.minus(OOM_RAISE_WINDOW)),
                ..vowp_store::repo_capacity::UsageQuery::default()
            },
            u32::MAX,
        )?;
        for row in rows.into_iter().filter(|row| row.oom_killed) {
            let key = format!("capacity:oom:{}", row.run_id);
            if store.kv_get(&key)?.is_some() {
                continue;
            }
            store.kv_set(&key, "1")?;
            let Some(job_id) = &row.job_id else {
                continue;
            };
            if let Some(paths) = store.paths()
                && let Ok(project_store) = Store::open_with_project(paths, &row.project)
                && let Some(job) = project_store.job(job_id)?
                && !job.state.is_terminal()
                && job.state != JobState::Parked
            {
                project_store.set_job_state(
                    job_id,
                    JobState::Parked,
                    &StateChange {
                        note: Some("parked: kernel OOM-killed this run; the next estimate is raised 25%".to_owned()),
                        // `ParkedReason` has no OOM variant; `Manual` is the only schema value
                        // that means a human must look. A `Resources` variant is a protocol
                        // follow-up, not something this job may invent.
                        parked_reason: Some(ParkedReason::Manual),
                    },
                )?;
            }
            store.insert_node_inbox(&InboxItem {
                id: InboxId::new(),
                project: Some(row.project.clone()),
                job_id: Some(job_id.clone()),
                kind: InboxKind::Notice,
                title: format!("OOM killed: {job_id}"),
                body: format!(
                    "The kernel OOM killer ended run {} for job {job_id} (peak {} bytes). The job is parked and its RAM estimate is raised by 25% for the next same-project, same-kind admission.",
                    row.run_id, row.peak_rss_bytes
                ),
                options: Vec::new(),
                state: InboxState::Open,
                answer: None,
                answered_by: None,
                created_at: now,
                answered_at: None,
            })?;
        }
        Ok(())
    }
}

/// Returns the newest host sample, optionally at or after `since`.
fn latest_sample(store: &Store, since: Option<Timestamp>) -> Result<Option<HostSample>, CoreError> {
    let since = since.unwrap_or_else(|| Timestamp::from_unix_secs(0));
    Ok(store.host_samples_since(since)?.into_iter().last())
}

/// Returns every open run with its project and job kind.
fn open_runs(store: &Store) -> Result<Vec<(ProjectName, Run, JobKind)>, CoreError> {
    let mut runs = Vec::new();
    if let Some(paths) = store.paths() {
        for entry in store.list_project_entries()? {
            if entry.state == vowp_protocol::ProjectState::Archived {
                continue;
            }
            let Ok(project_store) = Store::open_read_only(paths, Some(&entry.name)) else {
                continue;
            };
            for run in project_store.open_runs()? {
                let kind = run
                    .job_id
                    .as_ref()
                    .and_then(|job| project_store.job(job).ok().flatten())
                    .map(|job| job.kind)
                    .unwrap_or(JobKind::Build);
                runs.push((entry.name.clone(), run, kind));
            }
        }
        return Ok(runs);
    }
    if let Some(project) = store.bound_project() {
        for run in store.open_runs()? {
            let kind = run
                .job_id
                .as_ref()
                .and_then(|job| store.job(job).ok().flatten())
                .map(|job| job.kind)
                .unwrap_or(JobKind::Build);
            runs.push((project.clone(), run, kind));
        }
    }
    Ok(runs)
}

/// Returns every job in every non-archived project.
fn all_jobs(store: &Store) -> Result<Vec<(ProjectName, Job)>, CoreError> {
    let mut jobs = Vec::new();
    if let Some(paths) = store.paths() {
        for entry in store.list_project_entries()? {
            if entry.state == vowp_protocol::ProjectState::Archived {
                continue;
            }
            let Ok(project_store) = Store::open_read_only(paths, Some(&entry.name)) else {
                continue;
            };
            for job in project_store.list_jobs(&vowp_store::JobFilter::default())? {
                jobs.push((entry.name.clone(), job));
            }
        }
        return Ok(jobs);
    }
    if let Some(project) = store.bound_project() {
        for job in store.list_jobs(&vowp_store::JobFilter::default())? {
            jobs.push((project.clone(), job));
        }
    }
    Ok(jobs)
}

/// Returns whether an OOM kill still owes this project/kind one raised estimate.
///
/// The 24 h window is a cap on how stale the evidence may be, not a promise to raise every
/// admission for a day: the first matching admission consumes the row so only the next
/// same-project, same-kind request gets the 25% raise.
fn consume_oom_raise(store: &Store, req: &AdmitRequest, now: Timestamp) -> Result<bool, CoreError> {
    let rows = store.run_usage_for_group(
        &vowp_store::repo_capacity::UsageQuery {
            project: Some(req.project.clone()),
            job_kind: Some(req.kind),
            since: Some(now.minus(OOM_RAISE_WINDOW)),
            ..vowp_store::repo_capacity::UsageQuery::default()
        },
        50,
    )?;
    for row in rows.into_iter().filter(|row| row.oom_killed) {
        let key = format!("capacity:oom_raise:{}", row.run_id);
        if store.kv_get(&key)?.is_none() {
            store.kv_set(&key, "1")?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Returns the most recently admitted project for fairness ordering.
pub(crate) fn last_project(store: &Store) -> Result<Option<ProjectName>, CoreError> {
    let Some(text) = store.kv_get(LAST_PROJECT_KEY)? else {
        return Ok(None);
    };
    Ok(ProjectName::parse(&text).ok())
}

/// Returns the `kv` key that stores one job's first wait instant.
fn wait_key(project: &ProjectName, job: &JobId) -> String {
    format!("capacity:waiting:{}:{}", project.as_str(), job.as_str())
}

/// Maps a waiting detail back onto its stable code for the snapshot.
fn wait_code(detail: &str) -> WaitCode {
    let lower = detail.to_ascii_lowercase();
    if lower.starts_with("ram") {
        WaitCode::Ram
    } else if lower.starts_with("cpu") {
        WaitCode::Cpu
    } else if lower.starts_with("disk") {
        WaitCode::Disk
    } else if lower.starts_with("provider") {
        WaitCode::Provider
    } else if lower.starts_with("target") || lower.contains("worker(s) running") {
        WaitCode::Target
    } else {
        WaitCode::Brake
    }
}

/// The compiled default estimate used when a stored estimate cannot be read.
fn default_estimate() -> UsageEstimate {
    UsageEstimate {
        project: None,
        job_kind: None,
        harness: None,
        model: None,
        build_mode: None,
        samples: 0,
        ram_p50_bytes: DEFAULT_PEAK_RSS_BYTES,
        ram_p90_bytes: DEFAULT_PEAK_RSS_BYTES,
        cpu_p50_cores: DEFAULT_MEAN_CPU_CORES,
        cpu_p90_cores: DEFAULT_MEAN_CPU_CORES,
        duration_p50_ms: 0,
        duration_p90_ms: 0,
        source: vowp_protocol::EstimateSource::Default,
    }
}
