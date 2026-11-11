//! L1 supervisor: the ordered tick, request application, dispatch, gates, verify and merge.
//!
//! Spec: `docs/design/loops.md` §1.1 (steps 0–13), `docs/design/crates/vowp-core.md` §7.
//! Every step is wrapped: one failing step is logged, bumps the L1 fail streak and the tick
//! continues with the later steps.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vowp_graph::{
    DispatchCaps, DispatchChoice, Effect, Graph, JobEvent, RepeatGuard, RepeatVerdict, StallInput, StallJudge,
    StallThresholds, StallVerdict, TransitionBounds, plan_dispatch,
};
use vowp_protocol::{
    ActiveState, CancelToken, GateResult, InboxId, InboxItem, InboxKind, InboxState, Job, JobId, JobKind, JobState,
    Level, LimitRowKind, LoopBounds, NewSession, Node, NodeFile, ParkedReason, Profile, ProjectEntry, ProjectName,
    ProjectState, Run, RunId, RunPurpose, SessionId, SessionRole, Timestamp, VowpConfig, Worker, WorkerKind,
    WorkerState,
};
use vowp_store::{CrossDbReconcile, JobFilter, MigrationReport, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::gates::{GateCtx, GateRunner};
use crate::ledger::{BoardSnapshot, Ledger};
use crate::schedules::ScheduleEngine;
use crate::session::service::SessionService;
use crate::surface::orchestrator::Orchestrator;

use super::l2_worker::ReportOutcome;
use super::l4_planner::{self, SessionChoice};
use super::l5_keeper;
use super::l6_failover::{Failover, ProbeVerdict};
use super::l7_inbox::{self, NewInboxItem};
use super::merge_queue::{MergeQueue, MergeStep};
use super::runner::{RunnerPool, SessionRunPool, WakeHandle};
use super::{Applied, apply_event, dispatch, l3_verify, l8_watchdog, reconcile};

/// Counters reported by [`Supervisor::startup`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Open runs finalised as crash.
    pub orphaned_runs: u32,
    /// Switch rows recovered by the session service.
    pub switches_recovered: u32,
    /// Missed schedules fired once.
    pub schedules_caught_up: u32,
    /// tmux windows recreated.
    pub windows_recreated: u32,
}

/// One tick step's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    /// Step name (`requests`, `harvest`, …).
    pub name: &'static str,
    /// False when the step returned an error or panicked.
    pub ok: bool,
    /// Short human-readable detail.
    pub detail: String,
}

/// Everything one L1 tick did.
#[derive(Debug, Clone, PartialEq)]
pub struct TickReport {
    /// Instant the tick started.
    pub at: Timestamp,
    /// One entry per step, in order.
    pub steps: Vec<StepReport>,
    /// Jobs dispatched this tick.
    pub dispatched: Vec<JobId>,
    /// State transitions applied this tick.
    pub transitions: Vec<(JobId, JobState, JobState)>,
    /// Jobs merged this tick.
    pub merged: Vec<JobId>,
}

/// Which project steps one node-supervisor pass runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectTickMode {
    /// A normal tick: every step, including dispatch and schedules.
    Active,
    /// A paused project's maintenance tick: finish running work, start nothing new.
    Maintenance,
}

/// Result of one background gate run.
struct GateTaskResult {
    job_id: JobId,
    results: Vec<GateResult>,
    all_passed: bool,
}

/// Runs one job's gates on a runner thread with its own store connection.
fn spawn_gate_task(
    deps: Deps,
    project: vowp_protocol::ProjectName,
    job: Job,
) -> Result<JoinHandle<Result<GateTaskResult, CoreError>>, CoreError> {
    Ok(thread::spawn(move || {
        let store = deps.open_store()?;
        let repo = super::main_repo(&deps, &store, &project)?;
        let worktree = job.worktree.as_ref().map(PathBuf::from).unwrap_or_else(|| repo.clone());
        let ctx = GateCtx {
            deps: &deps,
            job: &job,
            run_id: store.last_run_of_job(&job.id)?.map(|run| run.id),
            worktree: &worktree,
            repo: &repo,
            store: &store,
            attempt: job.attempts,
        };
        let outcome = GateRunner::new(deps.clone()).evaluate_all(&ctx)?;
        let ledger = Ledger::new(deps.clone());
        for result in &outcome.results {
            ledger.record_gate(&store, result)?;
        }
        Ok(GateTaskResult {
            job_id: job.id.clone(),
            results: outcome.results,
            all_passed: outcome.all_passed_before_review,
        })
    }))
}

/// The L1 supervisor.
pub struct Supervisor {
    pub(crate) deps: Deps,
    pub(crate) project: vowp_protocol::ProjectName,
    pub(crate) store: Store,
    pub(crate) runners: RunnerPool,
    pub(crate) ledger: Ledger,
    pub(crate) schedules: ScheduleEngine,
    pub(crate) sessions: SessionService,
    pub(crate) wake: WakeHandle,
    wake_rx: Option<Receiver<()>>,
    handled_runs: HashSet<RunId>,
    gate_tasks: HashMap<JobId, JoinHandle<Result<GateTaskResult, CoreError>>>,
    merge_task: Option<JoinHandle<Result<MergeStep, CoreError>>>,
    verify_retries: HashSet<(JobId, u32)>,
    session_runners: SessionRunPool,
    probe_tasks: HashMap<String, JoinHandle<Result<(), CoreError>>>,
    repeat_guards: HashMap<JobId, RepeatGuard>,
    repeat_tripped: Option<String>,
    suppress_job_inbox: bool,
    start_purpose_override: Option<RunPurpose>,
    transitions: Vec<(JobId, JobState, JobState)>,
    dispatched: Vec<JobId>,
    merged: Vec<JobId>,
    last_checkpoint: Option<Timestamp>,
}

impl Supervisor {
    /// Creates a supervisor for one project.
    pub fn new(deps: Deps, project: vowp_protocol::ProjectName) -> Result<Self, CoreError> {
        let store = deps.open_store()?;
        let (wake, wake_rx) = WakeHandle::channel();
        reconcile::ensure_loops(&store, &deps)?;
        store.ensure_worker_slots(&deps.node, WorkerKind::Worker, deps.config.node.max_workers)?;
        store.ensure_worker_slots(&deps.node, WorkerKind::Verifier, deps.config.node.max_verifiers)?;
        Ok(Self {
            sessions: SessionService::new(deps.clone()),
            ledger: Ledger::new(deps.clone()),
            schedules: ScheduleEngine::new(deps.clone()),
            runners: RunnerPool::new(),
            deps,
            project,
            store,
            wake,
            wake_rx: Some(wake_rx),
            handled_runs: HashSet::new(),
            gate_tasks: HashMap::new(),
            merge_task: None,
            verify_retries: HashSet::new(),
            session_runners: SessionRunPool::new(),
            probe_tasks: HashMap::new(),
            repeat_guards: HashMap::new(),
            repeat_tripped: None,
            suppress_job_inbox: false,
            start_purpose_override: None,
            transitions: Vec::new(),
            dispatched: Vec::new(),
            merged: Vec::new(),
            last_checkpoint: None,
        })
    }

    /// Startup reconciliation (loops.md §1 step 1 and §11 catch-up).
    pub fn startup(&mut self) -> Result<ReconcileReport, CoreError> {
        self.handled_runs.clear();
        let (open, active) = self.open_runs_snapshot()?;
        let report = reconcile::startup(&self.deps, &self.store, &open, &active)?;
        self.store
            .ensure_worker_slots(&self.deps.node, WorkerKind::Worker, self.deps.config.node.max_workers)?;
        self.store.ensure_worker_slots(
            &self.deps.node,
            WorkerKind::Verifier,
            self.deps.config.node.max_verifiers,
        )?;
        Ok(report)
    }

    /// One full L1 tick, steps 0–13 of loops.md §1.1.
    ///
    /// The node-level hooks (capacity sampling, lease sweep, lifecycle tick and the
    /// snapshot) belong to [`NodeSupervisor`] and are not run here. This method keeps the
    /// v0.1 project-local board and heartbeat so single-project supervisors and their
    /// tests behave exactly as before.
    pub fn tick(&mut self) -> Result<TickReport, CoreError> {
        let now = self.deps.clock.now();
        let mut report = self.tick_steps(ProjectTickMode::Active, now)?;
        report.steps.push(self.step("board", |this| this.step_board(now)));
        report
            .steps
            .push(self.step("heartbeat", |this| this.step_heartbeat(now)));
        if let Err(error) = self.update_loop_rows(now, &report.steps) {
            self.deps
                .log
                .log(Level::Error, "l1", &format!("cannot update loop rows: {error}"));
        }
        self.record_tick(&report, now);
        Ok(report)
    }

    /// Runs the project steps for one node-supervisor pass.
    ///
    /// `Active` runs the full L1 order. `Maintenance` keeps requests, reconcile, harvest,
    /// stalls, states, gates, verify and merge, and skips the steps that create work
    /// (dispatch, triggers and schedules). Board and heartbeat are node-level and are
    /// written once by [`NodeSupervisor`].
    pub(crate) fn tick_project(&mut self, mode: ProjectTickMode, now: Timestamp) -> Result<TickReport, CoreError> {
        let report = self.tick_steps(mode, now)?;
        // L2–L7 and L9 come from this project's steps. L8 is node-wide: the node
        // supervisor records it from the node heartbeat after every project has ticked.
        if let Err(error) = self.update_loop_rows(now, &report.steps) {
            self.deps
                .log
                .log(Level::Error, "l1", &format!("cannot update loop rows: {error}"));
        }
        self.record_tick(&report, now);
        Ok(report)
    }

    /// Runs the ordered project steps without recording the L1 loop row.
    fn tick_steps(&mut self, mode: ProjectTickMode, now: Timestamp) -> Result<TickReport, CoreError> {
        self.transitions.clear();
        self.dispatched.clear();
        self.merged.clear();
        self.repeat_tripped = None;
        let node_paused = self.store.kv_get("paused")?.as_deref() == Some("1");
        let mut steps = Vec::new();

        steps.push(self.step("requests", Self::step_requests));
        steps.push(self.step("reconcile", |this| this.step_reconcile(now)));
        if !node_paused {
            steps.push(self.step("harvest", |this| this.step_harvest(now)));
            steps.push(self.step("stalls", |this| this.step_stalls(now)));
            steps.push(self.step("states", |this| this.step_states(now)));
            steps.push(self.step("gates", |this| this.step_gates(now)));
            steps.push(self.step("verify", |this| this.step_verify(now)));
            steps.push(self.step("merge", |this| this.step_merge(now)));
            if mode == ProjectTickMode::Active {
                steps.push(self.step("dispatch", |this| this.step_dispatch(now)));
                steps.push(self.step("triggers", |this| this.step_triggers(now)));
                steps.push(self.step("probes", |this| this.step_probes(now)));
                steps.push(self.step("schedules", |this| this.step_schedules(now)));
            }
        }
        // §4.2: board/checkpoint is in the maintenance subset. A paused project keeps its
        // own checkpoint fresh; the node-level board.json stays the single-project
        // compatibility copy written by [`NodeSupervisor`].
        if mode == ProjectTickMode::Maintenance {
            steps.push(self.step("board", |this| this.step_checkpoint(now)));
        }
        Ok(TickReport {
            at: now,
            steps,
            dispatched: std::mem::take(&mut self.dispatched),
            transitions: std::mem::take(&mut self.transitions),
            merged: std::mem::take(&mut self.merged),
        })
    }

    /// Records the L1 loop row for one project pass.
    fn record_tick(&self, report: &TickReport, now: Timestamp) {
        let steps = &report.steps;
        let ok = steps.iter().all(|step| step.ok) && self.repeat_tripped.is_none();
        let failed: Vec<&str> = steps.iter().filter(|step| !step.ok).map(|step| step.name).collect();
        let status = match (&self.repeat_tripped, failed.is_empty()) {
            (Some(job), _) => format!("repeat guard tripped: {job}"),
            (None, true) => "ok".to_owned(),
            (None, false) => format!("degraded: {}", failed.join(", ")),
        };
        let next = now.plus(Duration::from_secs(self.deps.config.node.tick_secs.max(1)));
        if let Err(error) = self
            .store
            .record_loop_run("l1-supervisor", now, Some(next), &status, ok)
        {
            self.deps
                .log
                .log(Level::Error, "l1", &format!("cannot record loop row: {error}"));
        }
    }

    /// Writes the project board and checkpoint outside a full tick.
    pub(crate) fn write_board(&mut self, now: Timestamp) -> Result<String, CoreError> {
        self.step_board(now)
    }

    /// Stops project runner threads and marks their open runs as stalled.
    pub(crate) fn shutdown(&mut self) -> Result<(), CoreError> {
        self.runners.cancel_all();
        self.session_runners.cancel_all();
        for run in self.store.open_runs()? {
            if run.node != self.deps.node {
                continue;
            }
            let _ = self.store.finish_run(
                &run.id,
                &vowp_store::RunFinish {
                    outcome: vowp_protocol::RunOutcome::Stalled,
                    exit_code: run.exit_code,
                    limit_kind: run.limit_kind,
                    cost: run.cost.clone(),
                    native_session_id: run.native_session_id.clone(),
                    detail: "supervisor stopping".to_owned(),
                },
            );
        }
        Ok(())
    }

    /// Startup then tick every `[node].tick_secs` or on wake, until `cancel`.
    pub fn run(&mut self, cancel: &CancelToken) -> Result<(), CoreError> {
        self.startup()?;
        let tick = Duration::from_secs(self.deps.config.node.tick_secs.max(1));
        while !cancel.is_cancelled() {
            self.tick()?;
            if !wait_for_next_tick(self.wake_rx.as_ref(), tick, &self.store, cancel)? {
                break;
            }
        }
        // Mark run rows whose threads were interrupted as stalled, as the crate doc requires.
        self.shutdown()
    }

    /// Returns a cloneable handle that ends the idle wait early.
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// Runs one tick step under `catch_unwind` and turns failures into a report entry.
    fn step<F>(&mut self, name: &'static str, f: F) -> StepReport
    where
        F: FnOnce(&mut Self) -> Result<String, CoreError>,
    {
        let result = catch_unwind(AssertUnwindSafe(|| f(self)));
        match result {
            Ok(Ok(detail)) => StepReport { name, ok: true, detail },
            Ok(Err(error)) => {
                self.deps
                    .log
                    .log(Level::Error, "l1", &format!("step {name} failed: {error}"));
                StepReport {
                    name,
                    ok: false,
                    detail: error.to_string(),
                }
            }
            Err(payload) => {
                let detail = panic_text(payload);
                self.deps
                    .log
                    .log(Level::Error, "l1", &format!("step {name} panicked: {detail}"));
                StepReport {
                    name,
                    ok: false,
                    detail: format!("panic: {detail}"),
                }
            }
        }
    }

    /// Step 0: pause flag and every queued CLI request.
    fn step_requests(&mut self) -> Result<String, CoreError> {
        let requests = crate::requests::drain(&self.store)?;
        let mut applied = 0;
        for (id, request) in requests {
            match self.apply_request(&request) {
                Ok(()) => applied += 1,
                Err(error) => self.deps.log.log(
                    Level::Warn,
                    "requests",
                    &format!("dropped request {} {}: {error}", id.0, request_name(&request)),
                ),
            }
        }
        Ok(format!("{applied} request(s) applied"))
    }

    /// Applies one drained request and logs the loop-request ledger rows.
    fn apply_request(&mut self, request: &crate::requests::Request) -> Result<(), CoreError> {
        use crate::requests::Request;
        match request {
            Request::ResolveJob { job } => self.apply_simple(job, JobEvent::Resolved, None),
            Request::RetryJob { job } => {
                let current = self.store.require_job(job)?;
                let event = match current.state {
                    JobState::Failed => JobEvent::Retry,
                    JobState::Stalled => JobEvent::KeeperRestart,
                    other => {
                        return Err(CoreError::Invalid(format!(
                            "retry is not allowed in state {}",
                            other.as_str()
                        )));
                    }
                };
                self.apply_simple(job, event, None)
            }
            Request::ParkJob { job, reason } => self.apply_simple(job, JobEvent::HumanPark, Some(reason.clone())),
            Request::CancelJob { job } => self.apply_simple(job, JobEvent::Cancel, None),
            Request::NudgeJob { job } => self.apply_simple(job, JobEvent::KeeperNudge { text: None }, None),
            Request::RestartJob { job } => self.apply_simple(job, JobEvent::KeeperRestart, None),
            Request::ReplanJob { job } => self.apply_simple(job, JobEvent::KeeperReplan, None),
            Request::FailJob { job, reason } => self.apply_simple(job, JobEvent::KeeperFail, Some(reason.clone())),
            Request::AnswerInbox { id } => self.apply_inbox_answer(id),
            Request::PauseLoop { name } | Request::ResumeLoop { name } => {
                let pause = matches!(request, Request::PauseLoop { .. });
                if name == "l1-supervisor" {
                    return Err(CoreError::Invalid(
                        "the l1-supervisor loop cannot be paused or resumed".to_owned(),
                    ));
                }
                let state = if pause {
                    ActiveState::Paused
                } else {
                    ActiveState::Active
                };
                self.store.set_loop_state(name, state)?;
                let (from, to) = if pause {
                    ("active", "paused")
                } else {
                    ("paused", "active")
                };
                super::record_loop_request(&self.store, &self.project, name, from, to, request_name(request))?;
                Ok(())
            }
            Request::RunLoop { name } => {
                let mut loop_ = self
                    .store
                    .loop_(name)?
                    .ok_or_else(|| CoreError::NotFound(format!("loop {name}")))?;
                let now = self.deps.clock.now();
                loop_.next_run_at = Some(now);
                loop_.updated_at = now;
                self.store.upsert_loop(&loop_)?;
                super::record_loop_request(
                    &self.store,
                    &self.project,
                    name,
                    "scheduled",
                    "due",
                    request_name(request),
                )
            }
            Request::Wake => super::record_loop_request(
                &self.store,
                &self.project,
                "l1-supervisor",
                "idle",
                "wake",
                request_name(request),
            ),
        }
    }

    /// Applies one job event with no findings and optional note.
    pub(crate) fn apply_simple(&mut self, job: &JobId, event: JobEvent, note: Option<String>) -> Result<(), CoreError> {
        self.apply_with(job, event, None, Vec::new(), note).map(|_| ())
    }

    /// Applies an inbox answer: writes the answer file/note and resolves every affected job.
    fn apply_inbox_answer(&mut self, id: &vowp_protocol::InboxId) -> Result<(), CoreError> {
        let item = self
            .store
            .inbox_item(id)?
            .ok_or_else(|| CoreError::NotFound(format!("inbox item {id}")))?;
        let effect = l7_inbox::record_answer(&self.store, &item)?;
        if let Some(provider) = &effect.clear_limit {
            self.store.clear_limit(provider)?;
        }
        for job_id in &effect.jobs {
            if let Ok(job) = self.store.require_job(job_id)
                && job.state == JobState::Parked
            {
                self.apply_simple(job_id, JobEvent::Resolved, None)?;
            }
        }
        l4_planner::raise_wake(&self.store, "planner")?;
        Ok(())
    }

    /// Step 1: finalise open runs with no live runner thread.
    fn step_reconcile(&mut self, _now: Timestamp) -> Result<String, CoreError> {
        let (open, active) = self.open_runs_snapshot()?;
        let count = reconcile::orphaned_runs(&self.deps, &self.store, &open, &active)?;
        Ok(format!("{count} orphaned run(s)"))
    }

    /// Step 2: harvest finished runs and feed their outcome to the job state machine.
    fn step_harvest(&mut self, now: Timestamp) -> Result<String, CoreError> {
        let mut harvested = 0;
        self.runners.poll();
        self.session_runners.poll();
        self.apply_park_requests()?;
        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            ..JobFilter::default()
        })?;
        for job in jobs {
            let Some(run) = self.store.last_run_of_job(&job.id)? else {
                continue;
            };
            if run.ended_at.is_none() || self.handled_runs.contains(&run.id) {
                continue;
            }
            // The harvested marker is durable: a supervisor restart must not replay an
            // old limit run and re-park a job the owner already resolved (loops.md §8.3.4).
            let marker = self.store.kv_get(&format!("l6:{}", run.id))?;
            if marker.as_deref() == Some("harvested") {
                continue;
            }
            // L6 runs on the runner thread before the wake. Wait for its marker on runs
            // whose policy matters, so a credits/auth/quota limit is never raced by the
            // job event (loops.md §1 step 2). Only a still-running job can need that
            // policy; a ready/parked job has already been harvested or resolved.
            if job.state == JobState::Running {
                if marker.as_deref() == Some("orphan") {
                    // Reconcile finalised this run without a live thread, so L6 never ran.
                    self.apply_l6_for_run(&run)?;
                    self.apply_park_requests()?;
                } else if run.limit_kind.is_some() && marker.is_none() {
                    let live = self.runners.is_active(&run.id)
                        || run
                            .session_id
                            .as_ref()
                            .is_some_and(|session| self.session_runners.session_running(session));
                    if live {
                        // The runner thread is still applying L6; wait for its marker.
                        continue;
                    }
                    self.apply_l6_for_run(&run)?;
                    self.apply_park_requests()?;
                }
            }
            self.handled_runs.insert(run.id.clone());
            self.release_worker(&run.id)?;
            if job.state != JobState::Running {
                self.mark_run_harvested(&run.id)?;
                continue;
            }
            if matches!(
                run.limit_kind,
                Some(vowp_protocol::LimitKind::Credits) | Some(vowp_protocol::LimitKind::Auth)
            ) && let Some(provider) = super::l6_failover::provider_of(&run)
            {
                let reason = if run.limit_kind == Some(vowp_protocol::LimitKind::Credits) {
                    ParkedReason::Credits
                } else {
                    ParkedReason::Auth
                };
                self.park_provider_jobs(&provider, reason)?;
            }
            let current = self.store.require_job(&job.id)?;
            if current.state != JobState::Running {
                self.mark_run_harvested(&run.id)?;
                continue;
            }
            let (event, note) = self.event_for_run(&current, &run, now)?;
            let applied = self.apply_with(&current.id, event.clone(), None, Vec::new(), note)?;
            self.mark_run_harvested(&run.id)?;
            self.deps.log.log(
                Level::Info,
                "l1",
                &format!(
                    "harvest {} run {} -> {} ({})",
                    current.id,
                    run.id,
                    applied.to.as_str(),
                    super::event_name(&event)
                ),
            );
            harvested += 1;
        }
        Ok(format!("{harvested} run(s) harvested"))
    }

    /// Persists that a finished run has been harvested, so a restart never replays it.
    fn mark_run_harvested(&self, run: &RunId) -> Result<(), CoreError> {
        self.store.kv_set(&format!("l6:{run}"), "harvested")?;
        Ok(())
    }

    /// Drains `park:<provider>` requests written by L6 and parks the matching jobs.
    fn apply_park_requests(&mut self) -> Result<u32, CoreError> {
        let mut parked = 0;
        for (key, reason) in self.store.kv_prefix("park:")? {
            let Some(provider) = key.strip_prefix("park:") else {
                continue;
            };
            self.store.kv_delete(&key)?;
            let reason = match reason.as_str() {
                "credits" => ParkedReason::Credits,
                "auth" => ParkedReason::Auth,
                _ => ParkedReason::ProviderBlocked,
            };
            parked += self.park_provider_jobs(provider, reason)?;
        }
        Ok(parked)
    }

    /// Parks every job whose current rung uses `provider`, one inbox item per provider/kind.
    fn park_provider_jobs(&mut self, provider: &str, reason: ParkedReason) -> Result<u32, CoreError> {
        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            ..JobFilter::default()
        })?;
        let mut parked = 0;
        self.suppress_job_inbox = true;
        let result = (|| -> Result<u32, CoreError> {
            let mut count = 0;
            for job in jobs {
                if !matches!(
                    job.state,
                    JobState::Pending | JobState::Ready | JobState::Running | JobState::Stalled
                ) {
                    continue;
                }
                if self.provider_of_job(&job)? != provider {
                    continue;
                }
                let note = match reason {
                    ParkedReason::Credits => {
                        format!("credits exhausted for {provider}; top up and run `vowp inbox answer <id> resolved`")
                    }
                    ParkedReason::Auth => format!(
                        "authentication failed for {provider}; log in again, then `vowp inbox answer <id> resolved`"
                    ),
                    _ => format!("provider {provider} is blocked; resolve it and answer `resolved`"),
                };
                match self.apply_with(&job.id, JobEvent::Blocked { reason }, None, Vec::new(), Some(note)) {
                    Ok(_) => count += 1,
                    Err(error) => self.deps.log.log(
                        Level::Warn,
                        "l6",
                        &format!("cannot park {} for {provider}: {error}", job.id),
                    ),
                }
            }
            Ok(count)
        })();
        self.suppress_job_inbox = false;
        parked += result?;
        if parked > 0 {
            let (title, body) = match reason {
                ParkedReason::Credits => (
                    format!("credits exhausted: {provider}"),
                    format!("credits exhausted for {provider}; top up and run `vowp inbox answer <id> resolved`"),
                ),
                ParkedReason::Auth => (
                    format!("auth required: {provider}"),
                    format!(
                        "authentication failed for {provider}; log in again with the harness's own login, then `vowp inbox answer <id> resolved`"
                    ),
                ),
                _ => (
                    format!("provider blocked: {provider}"),
                    format!("provider {provider} is blocked; resolve it and answer `resolved`"),
                ),
            };
            l7_inbox::open_inbox(
                &self.store,
                &self.deps,
                NewInboxItem {
                    project: Some(self.project.clone()),
                    job_id: None,
                    kind: InboxKind::Question,
                    title,
                    body,
                    options: vec!["resolved".to_owned()],
                },
            )?;
        }
        Ok(parked)
    }

    /// Returns the provider of a job's current rung.
    fn provider_of_job(&self, job: &Job) -> Result<String, CoreError> {
        if let Some(run) = self.store.last_run_of_job(&job.id)?
            && let Some(provider) = super::l6_failover::provider_of(&run)
        {
            return Ok(provider);
        }
        let profile = self
            .deps
            .config
            .profiles
            .get(&job.profile)
            .ok_or_else(|| CoreError::NotFound(format!("profile {}", job.profile)))?;
        let now = self.store.clock().now();
        // loops.md §8.3: a job that never ran is on `Profile::pick`'s current rung.
        // Credits/auth never advance the ladder, so those rows do not pick a rung.
        if let Some((_, executor)) = profile.pick(&self.current_rung_limits(now)?, now) {
            return Ok(executor.provider());
        }
        // Every rung is limited: keep the job on the first rung so a park request
        // for that provider still reaches it.
        profile
            .ladder
            .first()
            .map(|executor| executor.provider())
            .ok_or_else(|| CoreError::Invalid(format!("profile {} has an empty ladder", job.profile)))
    }

    /// Limits that decide the current rung of a job that never ran.
    ///
    /// Quota/rate/unavailable rows and probe-back blocks make a rung unavailable;
    /// credits/auth are parking reasons and must not advance the ladder (loops.md §8.3).
    fn current_rung_limits(&self, now: Timestamp) -> Result<HashMap<String, Option<Timestamp>>, CoreError> {
        let mut limited: HashMap<String, Option<Timestamp>> = self
            .store
            .active_limits(now)?
            .into_iter()
            .filter(|limit| !matches!(limit.kind, LimitRowKind::Credits | LimitRowKind::Auth))
            .map(|limit| (limit.provider, limit.resets_at))
            .collect();
        for (key, value) in self.store.kv_prefix("probe_back:")? {
            if value != "1" {
                continue;
            }
            let provider = key.trim_start_matches("probe_back:").to_owned();
            if self.store.limit_for(&provider)?.is_some() {
                limited.insert(provider, None);
            }
        }
        for (key, _) in self.store.kv_prefix("probe_pending:")? {
            limited.insert(key.trim_start_matches("probe_pending:").to_owned(), None);
        }
        Ok(limited)
    }

    /// Applies L6 for a finished run whose runner thread left no marker (manual rows).
    fn apply_l6_for_run(&self, run: &Run) -> Result<(), CoreError> {
        let Some(provider) = super::l6_failover::provider_of(run) else {
            return Ok(());
        };
        let existing = self.store.limit_for(&provider)?;
        let limit = run.limit_kind.map(|kind| vowp_protocol::LimitEvent {
            kind,
            provider: provider.clone(),
            resets_at: existing.as_ref().and_then(|row| {
                if row.kind == vowp_protocol::LimitRowKind::Quota {
                    row.resets_at.map(|at| at.minus(Duration::from_secs(120)))
                } else {
                    None
                }
            }),
            message: run.detail.clone(),
            http_status: None,
        });
        let summary = vowp_protocol::RunSummary {
            exit_code: run.exit_code,
            outcome: run.outcome.unwrap_or(vowp_protocol::RunOutcome::Error),
            native_session_id: run.native_session_id.clone(),
            cost: run.cost.clone(),
            limit,
            result_text: None,
            stop_reason: vowp_protocol::StopReason::EndTurn,
        };
        let action = Failover::new(self.deps.clone()).on_run_outcome(&self.store, run, &summary)?;
        self.deps.log.log(
            Level::Info,
            "l6",
            &format!("failover for harvested run {}: {action:?}", run.id),
        );
        self.store.kv_set(&format!("l6:{}", run.id), "harvest")?;
        Ok(())
    }

    /// Chooses the job event for one finished run, plus the note its inbox item carries.
    fn event_for_run(&self, job: &Job, run: &Run, now: Timestamp) -> Result<(JobEvent, Option<String>), CoreError> {
        Ok(match run.outcome {
            Some(vowp_protocol::RunOutcome::Ok) => {
                let worktree = match job.worktree.as_ref() {
                    Some(path) => PathBuf::from(path),
                    None => return Ok((JobEvent::RunEndedNoReport, None)),
                };
                match super::l2_worker::classify_report(&worktree, job, run.started_at) {
                    ReportOutcome::Ready => (JobEvent::ReportReady, None),
                    ReportOutcome::Blocked(questions) => (
                        JobEvent::Blocked {
                            reason: ParkedReason::Question,
                        },
                        Some(questions.join("\n")),
                    ),
                    ReportOutcome::NoReport => (JobEvent::RunEndedNoReport, None),
                }
            }
            Some(vowp_protocol::RunOutcome::Rate) => {
                let until = run
                    .provider
                    .as_deref()
                    .and_then(|provider| self.store.limit_for(provider).ok().flatten())
                    .and_then(|limit| limit.resets_at)
                    .filter(|until| *until > now)
                    .unwrap_or_else(|| now.plus(Duration::from_secs(600)));
                (
                    JobEvent::RunRateLimited { until },
                    Some(format!("rate-limited; retry after {}", until.as_rfc3339())),
                )
            }
            Some(vowp_protocol::RunOutcome::Limit)
            | Some(vowp_protocol::RunOutcome::Credits)
            | Some(vowp_protocol::RunOutcome::Auth)
            | Some(vowp_protocol::RunOutcome::Context) => (JobEvent::RunEndedNoReport, None),
            Some(vowp_protocol::RunOutcome::Crash)
            | Some(vowp_protocol::RunOutcome::Error)
            | Some(vowp_protocol::RunOutcome::Killed)
            | Some(vowp_protocol::RunOutcome::Stalled)
            | None => (JobEvent::RunEndedNoReport, None),
        })
    }

    /// Step 3: stall detection for live worker/verifier runs.
    fn step_stalls(&mut self, now: Timestamp) -> Result<String, CoreError> {
        let thresholds = StallThresholds {
            stall_minutes: self.bounds().stall_minutes.unwrap_or(20),
            silent_minutes: self.bounds().silent_minutes.unwrap_or(8),
        };
        let open = self.store.open_runs()?;
        let mut stalled = 0;
        for run in open {
            let session_backed = matches!(
                run.role,
                vowp_protocol::RunRole::Planner | vowp_protocol::RunRole::Keeper
            );
            if run.role != vowp_protocol::RunRole::Worker
                && run.role != vowp_protocol::RunRole::Verifier
                && !session_backed
            {
                continue;
            }
            if self.handled_runs.contains(&run.id) {
                continue;
            }
            if session_backed {
                let last_event_at = match &run.session_id {
                    Some(session) => self.store.session(session)?.map(|session| session.updated_at),
                    None => None,
                };
                let verdict = StallJudge::judge(
                    &StallInput {
                        now,
                        run_started_at: run.started_at,
                        last_event_at,
                        last_commit_at: None,
                        output_bytes: super::file_len(&self.deps.paths.run_raw_path(&run.id)),
                    },
                    &thresholds,
                );
                if matches!(verdict, StallVerdict::Stalled { .. }) {
                    if let Some(session) = &run.session_id {
                        self.session_runners.cancel_and_wait(session);
                    }
                    self.handled_runs.insert(run.id.clone());
                    let role = if run.role == vowp_protocol::RunRole::Planner {
                        "planner"
                    } else {
                        "keeper"
                    };
                    l4_planner::mark_turn(&self.store, role, now)?;
                    stalled += 1;
                }
                continue;
            }
            let Some(job_id) = &run.job_id else {
                continue;
            };
            let Ok(job) = self.store.require_job(job_id) else {
                continue;
            };
            if job.state != JobState::Running {
                continue;
            }
            let last_event_at = match &run.session_id {
                Some(session) => self.store.session(session)?.map(|session| session.updated_at),
                None => None,
            };
            let last_commit_at = match (&job.worktree, &job.branch) {
                (Some(worktree), Some(branch)) => self
                    .deps
                    .git
                    .last_commit_time(std::path::Path::new(worktree), branch)
                    .ok()
                    .flatten(),
                _ => None,
            };
            let output_bytes = super::file_len(&self.deps.paths.run_raw_path(&run.id));
            let verdict = StallJudge::judge(
                &StallInput {
                    now,
                    run_started_at: run.started_at,
                    last_event_at,
                    last_commit_at,
                    output_bytes,
                },
                &thresholds,
            );
            if let StallVerdict::Stalled { silent, .. } = verdict {
                self.runners.cancel(&run.id);
                self.handled_runs.insert(run.id.clone());
                self.apply_simple(&job.id, JobEvent::Stalled { silent }, None)?;
                // The run was stopped; its busy slot must be free for the next restart.
                self.release_worker(&run.id)?;
                stalled += 1;
            }
        }
        Ok(format!("{stalled} run(s) stalled"))
    }

    /// Step 4: pending → ready and delayed rate-limit continues.
    fn step_states(&mut self, now: Timestamp) -> Result<String, CoreError> {
        let (graph, _jobs) = self.graph()?;
        let mut changed = 0;
        for id in graph.ready_jobs() {
            self.apply_simple(&id, JobEvent::NeedsSatisfied, None)?;
            changed += 1;
        }
        if !self.loop_active("l2-worker") {
            return Ok(format!("{changed} state change(s); l2-worker paused"));
        }
        for (key, value) in self.store.kv_prefix("delay:")? {
            let Some(until) = super::parse_ts(&value) else {
                self.store.kv_delete(&key)?;
                continue;
            };
            if until > now {
                continue;
            }
            let job_id = JobId::parse(key.trim_start_matches("delay:"))?;
            let Ok(job) = self.store.require_job(&job_id) else {
                self.store.kv_delete(&key)?;
                continue;
            };
            if job.state != JobState::Running {
                self.store.kv_delete(&key)?;
                continue;
            }
            if self
                .store
                .open_runs()?
                .iter()
                .any(|run| run.job_id.as_ref() == Some(&job_id))
            {
                continue;
            }
            let kind = if job.role == vowp_protocol::Role::Verifier {
                WorkerKind::Verifier
            } else {
                WorkerKind::Worker
            };
            let has_free_worker = self
                .store
                .list_workers(&self.deps.node)?
                .iter()
                .any(|worker| worker.kind == kind && worker.state == WorkerState::Idle);
            if !has_free_worker {
                // Leave the delay in place; the next tick with a free slot retries it.
                continue;
            }
            let choice = match self.pick_choice(&job) {
                Ok(choice) => choice,
                Err(error) => {
                    self.deps.log.log(
                        Level::Warn,
                        "l2",
                        &format!("delayed continue for {job_id} waits: {error}"),
                    );
                    continue;
                }
            };
            let purpose = dispatch::purpose_for_dispatch(&self.store, &job)?;
            match self.start_run(&job, purpose, &choice, Vec::new()) {
                Ok(_) => {
                    self.store.kv_delete(&key)?;
                    changed += 1;
                }
                Err(error) => self.deps.log.log(
                    Level::Error,
                    "l2",
                    &format!("cannot start delayed continue for {job_id}: {error}"),
                ),
            }
        }
        Ok(format!("{changed} state change(s)"))
    }

    /// Step 5: spawn gate runners and collect the ones that finished.
    fn step_gates(&mut self, _now: Timestamp) -> Result<String, CoreError> {
        let mut applied = 0;
        let finished = self
            .gate_tasks
            .iter()
            .filter(|(_, handle)| handle.is_finished())
            .map(|(job, _)| job.clone())
            .collect::<Vec<_>>();
        for job_id in finished {
            let Some(handle) = self.gate_tasks.remove(&job_id) else {
                continue;
            };
            match handle.join() {
                Ok(Ok(result)) => {
                    let job = self.store.require_job(&result.job_id)?;
                    if job.state != JobState::Gating {
                        continue;
                    }
                    if result.all_passed {
                        self.repeat_guards.remove(&job.id);
                        self.apply_simple(&job.id, JobEvent::GatesPassed, None)?;
                    } else {
                        let max = self.bounds().same_action_max.unwrap_or(3).max(1);
                        let fingerprint = l5_keeper::gate_fingerprint(&result.results);
                        let verdict = {
                            let guard = self
                                .repeat_guards
                                .entry(job.id.clone())
                                .or_insert_with(|| RepeatGuard::new(max));
                            guard.observe("gate", &fingerprint)
                        };
                        if let RepeatVerdict::Tripped { count } = verdict {
                            let now = self.deps.clock.now();
                            self.trip_repeat_guard(&job, "gate", count, now)?;
                        } else {
                            let findings = result
                                .results
                                .iter()
                                .filter(|gate| !gate.passed)
                                .map(dispatch::gate_finding)
                                .collect::<Vec<_>>();
                            self.apply_with(&job.id, JobEvent::GatesFailed, None, findings, None)?;
                        }
                    }
                    applied += 1;
                }
                Ok(Err(error)) => self.deps.log.log(
                    Level::Error,
                    "gates",
                    &format!("gate runner for {job_id} failed: {error}"),
                ),
                Err(_) => self
                    .deps
                    .log
                    .log(Level::Error, "gates", &format!("gate runner for {job_id} panicked")),
            }
        }

        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            states: vec![JobState::Gating],
            ..JobFilter::default()
        })?;
        let mut started = 0;
        for job in jobs {
            if self.gate_tasks.contains_key(&job.id) {
                continue;
            }
            let handle = spawn_gate_task(self.deps.clone(), self.project.clone(), job.clone())?;
            self.gate_tasks.insert(job.id.clone(), handle);
            started += 1;
        }
        Ok(format!(
            "{applied} gate result(s) applied, {started} gate runner(s) started"
        ))
    }

    /// Step 6: ensure one verify job per reviewing attempt and deliver its verdict.
    fn step_verify(&mut self, _now: Timestamp) -> Result<String, CoreError> {
        if !self.loop_active("l3-verify") {
            return Ok("l3-verify paused".to_owned());
        }
        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            states: vec![JobState::Reviewing],
            ..JobFilter::default()
        })?;
        let mut handled = 0;
        for original in jobs {
            let verify = self.find_verify_job(&original)?;
            match verify {
                None => {
                    let evidence = self.store.gate_results_for(&original.id, original.attempts)?;
                    let diff = self.diff_summary(&original);
                    let verify = l3_verify::create_verify_job(&self.store, &self.deps, &original, &evidence, &diff)?;
                    self.apply_simple(&verify.id, JobEvent::NeedsSatisfied, None)?;
                    handled += 1;
                }
                Some(verify) if verify.state == JobState::Done => {
                    match l3_verify::on_verify_done_with_findings(&self.store, &verify) {
                        Ok((event, findings)) => {
                            self.apply_with(&original.id, event, None, findings, None)?;
                        }
                        Err(error) => {
                            self.deps.log.log(
                                Level::Warn,
                                "l3",
                                &format!("verify job {} has no usable verdict: {error}", verify.id),
                            );
                            self.retry_or_park_verify(&original, &error)?;
                        }
                    }
                    handled += 1;
                }
                Some(verify) if verify.state == JobState::Failed => {
                    self.apply_with(
                        &original.id,
                        JobEvent::VerifierFailed,
                        None,
                        Vec::new(),
                        Some(format!("verifier could not produce a verdict for {}", original.id)),
                    )?;
                    handled += 1;
                }
                Some(_) => {}
            }
        }
        Ok(format!("{handled} review(s) advanced"))
    }

    /// Retries one verify job with an unusable verdict, then parks the original (T37).
    pub(crate) fn retry_or_park_verify(&mut self, original: &Job, error: &CoreError) -> Result<(), CoreError> {
        let retry_id = JobId::parse(&format!("{}-verify-{}-r1", original.id, original.attempts))?;
        if let Some(retry) = self.store.job(&retry_id)? {
            // A restart may find the retry already created. A pending retry only needs to be
            // made ready; a finished retry means the single retry was already spent.
            if retry.state == JobState::Pending {
                return self.apply_simple(&retry.id, JobEvent::NeedsSatisfied, None);
            }
            if retry.state != JobState::Done {
                return Ok(());
            }
            return self.park_verifier_failed(original);
        }
        let key = (original.id.clone(), original.attempts);
        if self.verify_retries.insert(key) {
            let evidence = self.store.gate_results_for(&original.id, original.attempts)?;
            let diff = self.diff_summary(original);
            let retry = l3_verify::create_verify_retry_job(&self.store, &self.deps, original, &evidence, &diff)?;
            self.deps.log.log(
                Level::Warn,
                "l3",
                &format!(
                    "verify job for {} had no usable verdict ({error}); retrying once as {}",
                    original.id, retry.id
                ),
            );
            if retry.state == JobState::Pending {
                self.apply_simple(&retry.id, JobEvent::NeedsSatisfied, None)
            } else {
                Ok(())
            }
        } else {
            self.park_verifier_failed(original)
        }
    }

    /// Parks the original after its one verify retry was spent (T37).
    fn park_verifier_failed(&mut self, original: &Job) -> Result<(), CoreError> {
        self.apply_with(
            &original.id,
            JobEvent::VerifierFailed,
            None,
            Vec::new(),
            Some(format!("verifier could not produce a verdict for {}", original.id)),
        )
        .map(|_| ())
    }

    /// Step 7: start at most one merge runner and collect its finished result.
    fn step_merge(&mut self, _now: Timestamp) -> Result<String, CoreError> {
        if let Some(handle) = self.merge_task.take() {
            if !handle.is_finished() {
                self.merge_task = Some(handle);
                return Ok("merge runner in progress".to_owned());
            }
            return match handle.join() {
                Ok(Ok(step)) => self.apply_merge_step(step),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(CoreError::Command("merge runner panicked".to_owned())),
            };
        }
        let merging = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            states: vec![JobState::Merging],
            ..JobFilter::default()
        })?;
        if merging.is_empty() {
            return Ok("merge queue idle".to_owned());
        }
        let deps = self.deps.clone();
        self.merge_task = Some(thread::spawn(move || {
            let store = deps.open_store()?;
            MergeQueue::new(deps.clone()).step(&store)
        }));
        Ok("merge runner started".to_owned())
    }

    /// Applies one finished merge-queue result.
    fn apply_merge_step(&mut self, step: MergeStep) -> Result<String, CoreError> {
        match step {
            MergeStep::Idle | MergeStep::InProgress => Ok("merge queue idle".to_owned()),
            MergeStep::Merged(job) => {
                self.apply_simple(&job, JobEvent::MergeSucceeded, None)?;
                self.merged.push(job);
                Ok("merged one job".to_owned())
            }
            MergeStep::Conflict { job, files } => {
                let findings = files
                    .iter()
                    .map(|file| format!("rebase conflict in {file}; resolve it and continue the rebase"))
                    .collect::<Vec<_>>();
                self.apply_with(&job, JobEvent::MergeConflict, None, findings, None)?;
                Ok(format!("merge conflict in {}", job))
            }
            MergeStep::ChecksFailed { job } => {
                let current = self.store.require_job(&job)?;
                let results = self.store.gate_results_for(&current.id, current.attempts)?;
                let mut findings = results
                    .iter()
                    .filter(|result| !result.passed)
                    .map(dispatch::gate_finding)
                    .collect::<Vec<_>>();
                if findings.is_empty() {
                    findings.push(format!("merge checks failed for {}", current.id));
                }
                self.apply_with(&current.id, JobEvent::MergeChecksFailed, None, findings, None)?;
                Ok(format!("merge checks failed for {}", current.id))
            }
            MergeStep::Manual { job, detail } => {
                self.apply_with(&job, JobEvent::HumanPark, None, Vec::new(), Some(detail))?;
                Ok(format!("merge blocked for {}", job))
            }
        }
    }

    /// Step 8: plan and dispatch ready jobs, starting each runner thread.
    fn step_dispatch(&mut self, now: Timestamp) -> Result<String, CoreError> {
        if !self.loop_active("l2-worker") {
            return Ok("l2-worker paused".to_owned());
        }
        let (graph, jobs) = self.graph()?;
        let workers = self.store.list_workers(&self.deps.node)?;
        let free_workers = workers
            .iter()
            .filter(|worker| worker.kind == WorkerKind::Worker && worker.state == WorkerState::Idle)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        let free_verifiers = workers
            .iter()
            .filter(|worker| worker.kind == WorkerKind::Verifier && worker.state == WorkerState::Idle)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        let open_runs = self.store.open_runs()?;
        let mut provider_free = BTreeMap::new();
        for profile in self.deps.config.profiles.values() {
            for executor in &profile.ladder {
                let provider = executor.provider();
                if provider_free.contains_key(&provider) {
                    continue;
                }
                if let Some(config) = self.deps.config.providers.get(&provider) {
                    let running = open_runs
                        .iter()
                        .filter(|run| run.provider.as_deref() == Some(provider.as_str()))
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX);
                    provider_free.insert(provider, config.max_concurrent.saturating_sub(running));
                }
            }
        }
        let limited = self
            .store
            .active_limits(now)?
            .into_iter()
            .map(|limit| (limit.provider, limit.resets_at))
            .collect::<HashMap<_, _>>();
        for limit in self.store.active_limits(now)? {
            if let (Some(cap), Some(cap_until)) = (limit.concurrency_cap, limit.cap_until)
                && cap_until > now
            {
                provider_free
                    .entry(limit.provider)
                    .and_modify(|free| *free = (*free).min(cap));
            }
        }
        let caps = DispatchCaps {
            free_workers,
            free_verifiers,
            provider_free: provider_free.into_iter().collect(),
            limited,
            profiles: self
                .deps
                .config
                .profiles
                .clone()
                .into_iter()
                .collect::<BTreeMap<String, Profile>>(),
            now,
        };
        let choices = plan_dispatch(&graph, &jobs, &caps);
        let mut dispatched = 0;
        for choice in choices {
            let job = self.store.require_job(&choice.job_id)?;
            // Ask capacity admission before dispatch; a job that must wait stays queued.
            if !dispatch::admit(&self.deps, &self.store, &job)? {
                continue;
            }
            self.dispatched.push(job.id.clone());
            let resume = dispatch::purpose_for_dispatch(&self.store, &job)?;
            self.start_purpose_override = Some(resume);
            let applied = self.apply_with(&job.id, JobEvent::Dispatched, Some(choice), Vec::new(), None);
            self.start_purpose_override = None;
            applied?;
            dispatched += 1;
        }
        Ok(format!("{dispatched} job(s) dispatched"))
    }

    /// Step 9: start at most one planner turn and one keeper turn.
    fn step_triggers(&mut self, now: Timestamp) -> Result<String, CoreError> {
        if !self.store.kv_prefix("probe:active:")?.is_empty() {
            // Probe-back must finish before a planner/keeper turn can pick the provider
            // again; the next quiet tick starts the hand-back turn (loops.md §8.5).
            return Ok("probe in flight; triggers wait".to_owned());
        }
        let planner_running = self.session_runners.is_running(vowp_protocol::RunRole::Planner)
            || self
                .store
                .open_runs()?
                .iter()
                .any(|run| run.role == vowp_protocol::RunRole::Planner);
        let status = l4_planner::planner_status(&self.deps, &self.store, now, planner_running)?;
        // Coverage follows the planner's provider state, not `status.wanted`: a planner run
        // that just recorded a limit may still be finishing, and the keeper turn woken by
        // that limit must carry the planner duties (loops.md §7).
        let covering = status.provider_limited;
        let mut started = 0;

        if status.wanted
            && self.loop_active("l4-planner")
            && let Some(choice) = status.choice.clone()
        {
            self.start_session(
                vowp_protocol::RunRole::Planner,
                RunPurpose::Plan,
                choice,
                status.prompt.clone(),
                "planner",
            )?;
            l4_planner::mark_turn(&self.store, "planner", now)?;
            l4_planner::consume_wake(&self.store, "planner")?;
            self.record_role_loop("l4-planner", now, "planning")?;
            started += 1;
        }

        let keeper_running = self.session_runners.is_running(vowp_protocol::RunRole::Keeper)
            || self
                .store
                .open_runs()?
                .iter()
                .any(|run| run.role == vowp_protocol::RunRole::Keeper);
        if self.loop_active("l5-keeper")
            && let Some(turn) = l5_keeper::keeper_turn(
                &self.deps,
                &self.store,
                now,
                covering,
                status.digest_since,
                keeper_running,
            )?
        {
            self.start_session(
                vowp_protocol::RunRole::Keeper,
                RunPurpose::Keep,
                turn.choice,
                turn.prompt,
                "keeper",
            )?;
            l4_planner::mark_turn(&self.store, "keeper", now)?;
            l4_planner::consume_wake(&self.store, "keeper")?;
            if covering {
                l4_planner::consume_wake(&self.store, "planner")?;
            }
            self.record_role_loop("l5-keeper", now, if covering { "covering" } else { "keeping" })?;
            started += 1;
        }
        Ok(format!("{started} planner/keeper turn(s) started"))
    }

    /// Creates/reuses the role session and spawns one session-backed turn.
    fn start_session(
        &mut self,
        role: vowp_protocol::RunRole,
        purpose: RunPurpose,
        choice: SessionChoice,
        prompt: String,
        window: &str,
    ) -> Result<(), CoreError> {
        let session_role = match role {
            vowp_protocol::RunRole::Planner => SessionRole::Planner,
            vowp_protocol::RunRole::Keeper => SessionRole::Keeper,
            _ => SessionRole::Worker,
        };
        let profile = if role == vowp_protocol::RunRole::Planner {
            "planner"
        } else {
            "keeper"
        };
        let repo = super::main_repo(&self.deps, &self.store, &self.project)?;
        let session = match self.find_role_session(session_role)? {
            Some(session) => session,
            None => self.sessions.create_session(NewSession {
                project: Some(self.project.clone()),
                job_id: None,
                role: session_role,
                title: format!("{profile} {}", self.project),
                cwd: repo,
                parent: None,
                executor: Some(choice.executor.clone()),
            })?,
        };
        self.session_runners.spawn(
            self.deps.clone(),
            session,
            role,
            purpose,
            prompt,
            choice.executor,
            choice.node,
            choice.rung,
            Some(profile.to_owned()),
            Some(window.to_owned()),
            self.wake.clone(),
        )
    }

    /// Returns the newest session of `role` for this project.
    fn find_role_session(&self, role: SessionRole) -> Result<Option<SessionId>, CoreError> {
        Ok(self
            .store
            .list_sessions(&vowp_store::SessionFilter {
                project: Some(self.project.clone()),
                ..vowp_store::SessionFilter::default()
            })?
            .into_iter()
            .find(|session| session.role == Some(role))
            .map(|session| session.id))
    }

    /// Records a loop row after a planner/keeper turn starts.
    fn record_role_loop(&self, name: &str, now: Timestamp, status: &str) -> Result<(), CoreError> {
        let loop_ = self.store.loop_(name)?;
        let interval = loop_.as_ref().map(|loop_| loop_.interval_secs).unwrap_or(0);
        let next = (interval > 0).then(|| now.plus(Duration::from_secs(interval)));
        self.store.record_loop_run(name, now, next, status, true)?;
        Ok(())
    }

    /// Step 10: start due L6 probes on their own threads.
    fn step_probes(&mut self, now: Timestamp) -> Result<String, CoreError> {
        // Drop locks left by a crashed probe thread when no probe run is open.
        let open_probe_runs = self
            .store
            .open_runs()?
            .into_iter()
            .filter(|run| run.role == vowp_protocol::RunRole::Probe)
            .collect::<Vec<_>>();
        for (key, _) in self.store.kv_prefix("probe:active:")? {
            let provider = key.trim_start_matches("probe:active:");
            if !self.probe_tasks.contains_key(provider)
                && !open_probe_runs
                    .iter()
                    .any(|run| run.provider.as_deref() == Some(provider))
            {
                self.store.kv_delete(&key)?;
            }
        }
        let finished = self
            .probe_tasks
            .iter()
            .filter(|(_, handle)| handle.is_finished())
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>();
        let mut applied = 0;
        for provider in finished {
            let Some(handle) = self.probe_tasks.remove(&provider) else {
                continue;
            };
            let _ = self.store.kv_delete(&format!("probe:active:{provider}"));
            match handle.join() {
                Ok(Ok(())) => applied += 1,
                Ok(Err(error)) => {
                    self.deps
                        .log
                        .log(Level::Error, "l6", &format!("probe for {provider} failed: {error}"))
                }
                Err(_) => self
                    .deps
                    .log
                    .log(Level::Error, "l6", &format!("probe for {provider} panicked")),
            }
        }
        if !self.loop_active("l6-failover") {
            return Ok(format!("{applied} probe(s) applied; l6-failover paused"));
        }
        // A probe that succeeded while a planner/keeper turn was still live defers the
        // hand-back to the next quiet tick; retry it before starting new probes.
        let busy = self.store.open_runs()?.iter().any(|run| {
            matches!(
                run.role,
                vowp_protocol::RunRole::Planner | vowp_protocol::RunRole::Keeper
            )
        });
        if !busy {
            let failover = Failover::new(self.deps.clone());
            for (key, _) in self.store.kv_prefix("probe_pending:")? {
                let provider = key.trim_start_matches("probe_pending:").to_owned();
                if failover.probe_back(&self.store, &provider)? {
                    self.store.kv_delete(&key)?;
                }
            }
        }
        let due = Failover::new(self.deps.clone()).probe_due(&self.store, now)?;
        let mut started = 0;
        for provider in due {
            if self.probe_tasks.contains_key(&provider) {
                continue;
            }
            let deps = self.deps.clone();
            let thread_provider = provider.clone();
            self.store.kv_set(&format!("probe:active:{provider}"), "1")?;
            let handle = thread::spawn(move || -> Result<(), CoreError> {
                let store = deps.open_store()?;
                let failover = Failover::new(deps.clone());
                let verdict = failover.run_probe(&store, &thread_provider)?;
                if verdict == ProbeVerdict::Available {
                    let complete = failover.probe_back(&store, &thread_provider)?;
                    if !complete {
                        store.kv_set(&format!("probe_pending:{thread_provider}"), "1")?;
                    }
                } else {
                    deps.log
                        .log(Level::Info, "l6", &format!("probe for {thread_provider}: {verdict:?}"));
                }
                store.kv_delete(&format!("probe:active:{thread_provider}"))?;
                Ok(())
            });
            self.probe_tasks.insert(provider, handle);
            started += 1;
        }
        Ok(format!("{applied} probe(s) applied, {started} probe(s) started"))
    }

    /// Applies the repeat guard's third identical failure: fail the job and raise a decision.
    fn trip_repeat_guard(&mut self, job: &Job, action: &str, count: u32, now: Timestamp) -> Result<(), CoreError> {
        let note = format!("no progress: {action} x{count}");
        let from = self.store.set_job_state(
            &job.id,
            JobState::Failed,
            &vowp_store::StateChange {
                note: Some(note.clone()),
                parked_reason: None,
            },
        )?;
        self.store.append_ledger(&vowp_protocol::LedgerEntry {
            id: None,
            ts: now,
            kind: vowp_protocol::LedgerKind::State,
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("{}: {} -> failed (repeat guard)", job.id, from.as_str()),
            detail: serde_json::json!({
                "from": from.as_str(),
                "to": "failed",
                "why": "repeat guard",
            }),
        })?;
        self.transitions.push((job.id.clone(), from, JobState::Failed));
        self.repeat_guards.remove(&job.id);
        let decision = dispatch::create_decision_job(&self.deps, &self.store, job)?;
        self.apply_with(&decision.id, JobEvent::NeedsSatisfied, None, Vec::new(), None)?;
        let failed = self.store.require_job(&job.id)?;
        self.open_job_inbox(&failed, InboxKind::Decision)?;
        self.repeat_tripped = Some(job.id.to_string());
        if let Some(loop_) = self.store.loop_("l1-supervisor")? {
            self.store.record_loop_run(
                "l1-supervisor",
                now,
                loop_.next_run_at,
                &format!("repeat guard: {}", job.id),
                false,
            )?;
        }
        Ok(())
    }

    /// Step 13: refresh last_run/last_status/fail_streak for every unpaused loop row.
    fn update_loop_rows(&self, now: Timestamp, steps: &[StepReport]) -> Result<(), CoreError> {
        for loop_ in self.store.list_loops()? {
            if loop_.name == "l1-supervisor" || loop_.state == ActiveState::Paused {
                continue;
            }
            let step_name = match loop_.name.as_str() {
                "l2-worker" => "dispatch",
                "l3-verify" => "verify",
                "l4-planner" | "l5-keeper" => "triggers",
                "l6-failover" => "probes",
                "l7-inbox" => "requests",
                "l8-watchdog" => "heartbeat",
                "l9-schedules" => "schedules",
                _ => continue,
            };
            let Some(step) = steps.iter().find(|step| step.name == step_name) else {
                continue;
            };
            self.store
                .record_loop_run(&loop_.name, now, loop_.next_run_at, &step.detail, step.ok)?;
        }
        Ok(())
    }

    /// Records this project's L8 row from the node-level heartbeat result.
    ///
    /// The heartbeat file is written once per node, after every project has ticked; a
    /// project pass therefore has no `heartbeat` step of its own. The node supervisor
    /// calls this for each project so `l8-watchdog` still shows the real run time.
    pub(crate) fn record_heartbeat(&self, now: Timestamp, ok: bool, detail: &str) -> Result<(), CoreError> {
        let Some(loop_) = self.store.loop_("l8-watchdog")? else {
            return Ok(());
        };
        if loop_.state == ActiveState::Paused {
            return Ok(());
        }
        self.store
            .record_loop_run("l8-watchdog", now, loop_.next_run_at, detail, ok)?;
        Ok(())
    }

    /// Step 11: fire due schedules.
    fn step_schedules(&mut self, now: Timestamp) -> Result<String, CoreError> {
        if !self.loop_active("l9-schedules") {
            return Ok("l9-schedules paused".to_owned());
        }
        let due = self.schedules.tick(&self.store, now)?;
        let mut fired = 0;
        for due_run in &due {
            match self.schedules.fire(&self.store, due_run) {
                Ok(_) => fired += 1,
                Err(error) => self.deps.log.log(
                    Level::Error,
                    "l9",
                    &format!("cannot fire schedule {}: {error}", due_run.schedule.name),
                ),
            }
        }
        if fired > 0 {
            l4_planner::raise_wake(&self.store, "planner")?;
        }
        Ok(format!("{fired} schedule(s) fired"))
    }

    /// Step 12: board snapshot and checkpoint.
    fn step_board(&mut self, now: Timestamp) -> Result<String, CoreError> {
        self.write_board_snapshot(now)?;
        self.step_checkpoint(now)?;
        Ok("board written".to_owned())
    }

    /// Writes the board snapshot copy without touching the checkpoint.
    fn write_board_snapshot(&self, now: Timestamp) -> Result<(), CoreError> {
        let snapshot = BoardSnapshot::collect(&self.store, &self.deps.config, now)?;
        let json = board_json(&snapshot)?;
        self.atomic_write("board.json", &json)
    }

    /// Writes the project checkpoint when one is due.
    ///
    /// This is the maintenance subset's board step for a paused project: the node-level
    /// `board.json` belongs to the single-project compatibility path, while the
    /// checkpoint file is per project and can be written by every paused tick.
    fn step_checkpoint(&mut self, now: Timestamp) -> Result<String, CoreError> {
        let checkpoint_minutes = self.bounds().checkpoint_minutes.unwrap_or(120) as u64;
        let due = match self.last_checkpoint {
            None => checkpoint_minutes > 0,
            Some(last) => now.since(last) >= Duration::from_secs(checkpoint_minutes.saturating_mul(60)),
        };
        if !due || checkpoint_minutes == 0 {
            return Ok("checkpoint not due".to_owned());
        }
        let text = crate::ledger::report::checkpoint(&self.store, &self.deps.config, now)?;
        let project = super::main_repo(&self.deps, &self.store, &self.project)?;
        let dir = project.join(".vowp/checkpoints");
        std::fs::create_dir_all(&dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = dir.join(format!("{}.md", now.unix_secs()));
        std::fs::write(&path, text).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
        self.last_checkpoint = Some(now);
        Ok("checkpoint written".to_owned())
    }

    /// Step 13: heartbeat file and supervisor pid.
    fn step_heartbeat(&mut self, _now: Timestamp) -> Result<String, CoreError> {
        l8_watchdog::write_heartbeat(&self.deps)?;
        self.store.kv_set("supervisor_pid", &std::process::id().to_string())?;
        Ok("heartbeat written".to_owned())
    }

    /// Applies one event and executes every effect it returned.
    pub(crate) fn apply_with(
        &mut self,
        job_id: &JobId,
        event: JobEvent,
        choice: Option<DispatchChoice>,
        findings: Vec<String>,
        note: Option<String>,
    ) -> Result<Applied, CoreError> {
        let bounds = self.transition_bounds();
        let applied = apply_event(&self.store, job_id, &event, bounds, note, None)?;
        self.note_planner_event(&event, applied.to)?;
        self.transitions.push((job_id.clone(), applied.from, applied.to));
        let mut findings = findings;
        for effect in applied.effects.clone() {
            self.execute_effect(job_id, &effect, choice.as_ref(), &mut findings)?;
        }
        Ok(applied)
    }

    /// Executes one state-machine effect.
    fn execute_effect(
        &mut self,
        job_id: &JobId,
        effect: &Effect,
        choice: Option<&DispatchChoice>,
        findings: &mut Vec<String>,
    ) -> Result<(), CoreError> {
        match effect {
            Effect::CreateWorktree => {
                let job = self.store.require_job(job_id)?;
                let repo = super::main_repo(&self.deps, &self.store, &self.project)?;
                dispatch::ensure_worktree(&self.deps, &self.store, &job, &repo)?;
            }
            Effect::StartRun { purpose } => {
                let job = self.store.require_job(job_id)?;
                let purpose = if *purpose == RunPurpose::Start {
                    self.start_purpose_override.take().unwrap_or(RunPurpose::Start)
                } else {
                    *purpose
                };
                let choice = match choice {
                    Some(choice) => choice.clone(),
                    None => self.pick_choice(&job)?,
                };
                if let Err(error) = self.start_run(&job, purpose, &choice, std::mem::take(findings)) {
                    self.deps
                        .log
                        .log(Level::Error, "l2", &format!("cannot start run for {}: {error}", job.id));
                    self.apply_with(job_id, JobEvent::RunEndedNoReport, None, Vec::new(), None)?;
                }
            }
            Effect::StopRun => {
                if let Some(run) = self.store.last_run_of_job(job_id)?
                    && run.ended_at.is_none()
                {
                    self.runners.cancel_and_wait(&run.id);
                }
            }
            Effect::RunGates | Effect::EnqueueMerge | Effect::UnblockDependents => {}
            Effect::IncrementContinues
            | Effect::ResetContinues
            | Effect::IncrementHangRestarts
            | Effect::IncrementAttempts
            | Effect::ResetCounters
            | Effect::SetParkedReason(_)
            | Effect::ClearParkedReason => {}
            Effect::StartVerify => {
                let job = self.store.require_job(job_id)?;
                let evidence = self.store.gate_results_for(&job.id, job.attempts)?;
                let diff = self.diff_summary(&job);
                let verify = l3_verify::create_verify_job(&self.store, &self.deps, &job, &evidence, &diff)?;
                self.apply_with(&verify.id, JobEvent::NeedsSatisfied, None, Vec::new(), None)?;
            }
            Effect::CreateFixJob { kind } => {
                let job = self.store.require_job(job_id)?;
                let child = dispatch::create_child_job(&self.deps, &self.store, &job, *kind, findings)?;
                self.apply_with(&child.id, JobEvent::NeedsSatisfied, None, Vec::new(), None)?;
            }
            Effect::CreateDecisionJob => {
                let job = self.store.require_job(job_id)?;
                let decision = dispatch::create_decision_job(&self.deps, &self.store, &job)?;
                self.apply_with(&decision.id, JobEvent::NeedsSatisfied, None, Vec::new(), None)?;
            }
            Effect::CleanupWorktree => {
                let job = self.store.require_job(job_id)?;
                if let Some(worktree) = &job.worktree {
                    let repo = super::main_repo(&self.deps, &self.store, &self.project)?;
                    let _ = self.deps.git.worktree_remove(&repo, std::path::Path::new(worktree));
                    let _ = self.deps.windows.kill_window(
                        &vowp_tmux::session_name_for_project(self.project.as_str()),
                        &super::window_name(&job.id),
                    );
                }
            }
            Effect::NotifyInbox { kind } => {
                if self.suppress_job_inbox {
                    return Ok(());
                }
                let job = self.store.require_job(job_id)?;
                self.open_job_inbox(&job, *kind)?;
            }
            Effect::NotifyParent => self.notify_parent(job_id)?,
            Effect::WakeKeeper => {
                l4_planner::raise_wake(&self.store, "keeper")?;
            }
            Effect::WakePlanner => {
                l4_planner::raise_wake(&self.store, "planner")?;
            }
            Effect::DelayRun { purpose: _, until } => {
                self.store.kv_set(&format!("delay:{job_id}"), &until.as_rfc3339())?;
            }
        }
        Ok(())
    }

    /// Marks the planner due after a job event it should see (done/failed/stalled/parked).
    fn note_planner_event(&self, event: &JobEvent, to: JobState) -> Result<(), CoreError> {
        let planner_event = matches!(
            to,
            JobState::Done | JobState::Failed | JobState::Stalled | JobState::Parked
        ) || matches!(
            event,
            JobEvent::GatesPassed
                | JobEvent::GatesFailed
                | JobEvent::VerifyPassed
                | JobEvent::VerifyFailed
                | JobEvent::MergeSucceeded
                | JobEvent::MergeConflict
                | JobEvent::MergeChecksFailed
                | JobEvent::FixDone { .. }
                | JobEvent::FixFailed
                | JobEvent::Blocked { .. }
                | JobEvent::KeeperFail
                | JobEvent::Retry
                | JobEvent::RunRateLimited { .. }
        );
        if planner_event {
            l4_planner::raise_wake(&self.store, "planner")?;
        }
        if matches!(event, JobEvent::RunRateLimited { .. }) {
            l4_planner::raise_wake(&self.store, "keeper")?;
        }
        Ok(())
    }

    /// Opens the inbox item implied by a transition effect.
    pub(crate) fn open_job_inbox(&mut self, job: &Job, kind: InboxKind) -> Result<(), CoreError> {
        let note = if job.note.is_empty() {
            String::new()
        } else {
            job.note.clone()
        };
        let (title, body, options) = match (kind, job.parked_reason) {
            (InboxKind::Decision, _) => (
                format!("decide: {}", job.id),
                format!(
                    "job {} exhausted its fix rounds. Choose retry, replan or cancel. {note}",
                    job.id
                ),
                vec!["retry".to_owned(), "replan".to_owned(), "cancel".to_owned()],
            ),
            (InboxKind::Question, Some(ParkedReason::Credits)) => (
                format!("credits exhausted: {}", job.id),
                "credits exhausted for the job's provider; top up and run `vowp inbox answer <id> resolved`".to_owned(),
                vec!["resolved".to_owned()],
            ),
            (InboxKind::Question, Some(ParkedReason::Auth)) => (
                format!("auth required: {}", job.id),
                "log in again with the harness's own login, then `vowp inbox answer <id> resolved`".to_owned(),
                vec!["resolved".to_owned()],
            ),
            (InboxKind::Question, Some(ParkedReason::ProviderBlocked)) => (
                format!("provider blocked: {}", job.id),
                "the provider is blocked; resolve it and answer `resolved`".to_owned(),
                vec!["resolved".to_owned()],
            ),
            (InboxKind::Question, _) => (
                format!("question: {}", job.id),
                if note.is_empty() {
                    format!("job {} needs a human answer", job.id)
                } else {
                    note
                },
                vec!["resolved".to_owned()],
            ),
            (InboxKind::Approval, _) => (
                format!("approval: {}", job.id),
                format!("approval requested for {} {note}", job.id),
                vec!["approve".to_owned(), "refuse".to_owned()],
            ),
            (InboxKind::Notice, _) => (
                format!("notice: {}", job.id),
                if note.is_empty() {
                    format!("job {} changed state", job.id)
                } else {
                    note
                },
                Vec::new(),
            ),
        };
        super::open_inbox(
            &self.store,
            &self.deps,
            NewInboxItem {
                project: Some(job.project.clone()),
                job_id: Some(job.id.clone()),
                kind,
                title,
                body,
                options,
            },
        )?;
        Ok(())
    }

    /// Delivers a finished child's event to its parent.
    fn notify_parent(&mut self, child_id: &JobId) -> Result<(), CoreError> {
        let child = self.store.require_job(child_id)?;
        let parent = self.parent_of(&child)?;
        let parent_job = self.store.require_job(&parent)?;
        match child.kind {
            JobKind::Verify => {
                if child.state == JobState::Done && parent_job.state == JobState::Reviewing {
                    match l3_verify::on_verify_done_with_findings(&self.store, &child) {
                        Ok((event, findings)) => {
                            self.apply_with(&parent, event, None, findings, None)?;
                        }
                        Err(error) => {
                            self.deps.log.log(
                                Level::Warn,
                                "l3",
                                &format!("verify job {} has no usable verdict: {error}", child.id),
                            );
                            self.retry_or_park_verify(&parent_job, &error)?;
                        }
                    }
                } else if child.state == JobState::Failed {
                    self.apply_with(
                        &parent,
                        JobEvent::VerifierFailed,
                        None,
                        Vec::new(),
                        Some(format!("verifier could not produce a verdict for {parent}")),
                    )?;
                }
            }
            JobKind::Fix | JobKind::Merge => {
                if parent_job.state != JobState::Fixing {
                    return Ok(());
                }
                if child.state == JobState::Done {
                    self.apply_simple(&parent, JobEvent::FixDone { kind: child.kind }, None)?;
                } else if child.state == JobState::Failed {
                    self.apply_simple(&parent, JobEvent::FixFailed, None)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Returns the parent (fixes/verifies destination) of a child job.
    fn parent_of(&self, child: &Job) -> Result<JobId, CoreError> {
        for edge in self.store.edges_of(&child.id)? {
            if edge.src != child.id {
                continue;
            }
            if matches!(
                edge.kind,
                vowp_protocol::EdgeKind::Fixes | vowp_protocol::EdgeKind::Verifies
            ) {
                return Ok(edge.dst);
            }
        }
        Err(CoreError::Invalid(format!("job {} has no parent edge", child.id)))
    }

    /// Starts one run and records its dispatch and worker slot.
    pub(crate) fn start_run(
        &mut self,
        job: &Job,
        purpose: RunPurpose,
        choice: &DispatchChoice,
        findings: Vec<String>,
    ) -> Result<RunId, CoreError> {
        let repo = super::main_repo(&self.deps, &self.store, &self.project)?;
        let job = dispatch::ensure_worktree(&self.deps, &self.store, job, &repo)?;
        // A continue/restart replaces the previous process on the same session; join it
        // first so `SessionLog`'s lock is free.
        if let Some(previous) = self.store.last_run_of_job(&job.id)?
            && previous.ended_at.is_none()
            && self.runners.is_active(&previous.id)
        {
            self.runners.cancel_and_wait(&previous.id);
        }
        let worktree = super::job_worktree(&job)?;
        dispatch::write_brief(&self.deps, &job, &worktree)?;
        if super::latest_session(&self.store, &job.id)?.is_none() {
            let role = match job.role {
                vowp_protocol::Role::Verifier => SessionRole::Verifier,
                vowp_protocol::Role::Planner => SessionRole::Planner,
                vowp_protocol::Role::Keeper => SessionRole::Keeper,
                vowp_protocol::Role::Worker => SessionRole::Worker,
            };
            self.sessions.create_session(NewSession {
                project: Some(job.project.clone()),
                job_id: Some(job.id.clone()),
                role,
                title: job.id.as_str().to_owned(),
                cwd: worktree.clone(),
                parent: None,
                executor: Some(choice.executor.clone()),
            })?;
        }
        self.ensure_job_window(&job, &repo)?;

        let prompt = match job.kind {
            JobKind::Verify => {
                let original_id = l3_verify::verify_parent(&self.store, &job)?;
                let original = self.store.require_job(&original_id)?;
                let evidence = self.store.gate_results_for(&original.id, original.attempts)?;
                let diff = self.diff_summary(&original);
                let brief = match &original.worktree {
                    Some(worktree) => PathBuf::from(worktree).join(&original.brief),
                    None => PathBuf::from(&original.brief),
                };
                l3_verify::verify_prompt(&job, &brief.display().to_string(), &evidence, &diff)
            }
            JobKind::Fix | JobKind::Merge => {
                let findings = if findings.is_empty() {
                    job.note
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                } else {
                    findings
                };
                super::l2_worker::fix_prompt(&job, &findings)
            }
            _ => match purpose {
                RunPurpose::Continue => super::l2_worker::continue_prompt(&job),
                RunPurpose::Fix => super::l2_worker::fix_prompt(&job, &findings),
                _ => {
                    let brief = super::brief_path(&job)?.display().to_string();
                    super::l2_worker::start_prompt(&job, &brief)
                }
            },
        };
        let run_id = self.runners.spawn_job(
            job.clone(),
            choice.clone(),
            purpose,
            Some(prompt),
            self.deps.clone(),
            self.wake.clone(),
        )?;
        self.assign_worker(&job, &run_id)?;
        if let Ok(run) = self.store.require_run(&run_id) {
            self.ledger.record_dispatch(&self.store, &job, &run)?;
        }
        Ok(run_id)
    }

    /// Creates the job window when the session exists and the window does not.
    fn ensure_job_window(&self, job: &Job, repo: &std::path::Path) -> Result<(), CoreError> {
        let session = vowp_tmux::session_name_for_project(self.project.as_str());
        self.deps.windows.ensure_session(&session)?;
        let window = super::window_name(&job.id);
        if self.deps.windows.window_exists(&session, &window)? {
            return Ok(());
        }
        let spec = vowp_tmux::WindowSpec {
            argv: vec![
                "vowp".to_owned(),
                "__window".to_owned(),
                "job".to_owned(),
                job.id.as_str().to_owned(),
            ],
            cwd: Some(repo.to_path_buf()),
            env: Vec::new(),
        };
        match self.deps.windows.new_window(&session, &window, &spec) {
            Ok(()) | Err(vowp_tmux::TmuxError::WindowExists { .. }) => Ok(()),
            Err(error) => Err(CoreError::Tmux(error)),
        }
    }

    /// Marks one idle worker/verifier slot busy for the run.
    fn assign_worker(&self, job: &Job, run_id: &RunId) -> Result<(), CoreError> {
        let kind = if job.role == vowp_protocol::Role::Verifier {
            WorkerKind::Verifier
        } else {
            WorkerKind::Worker
        };
        let Some(worker) = self
            .store
            .list_workers(&self.deps.node)?
            .into_iter()
            .find(|worker| worker.kind == kind && worker.state == WorkerState::Idle)
        else {
            return Ok(());
        };
        self.store.upsert_worker(&Worker {
            node: self.deps.node.clone(),
            slot: worker.slot,
            kind,
            state: WorkerState::Busy,
            run_id: Some(run_id.clone()),
            job_id: Some(job.id.clone()),
            window: Some(super::window_name(&job.id)),
            updated_at: self.deps.clock.now(),
        })?;
        Ok(())
    }

    /// Frees the worker slot that held `run`.
    pub(crate) fn release_worker(&self, run: &RunId) -> Result<(), CoreError> {
        for worker in self.store.list_workers(&self.deps.node)? {
            if worker.run_id.as_ref() == Some(run) {
                self.store.upsert_worker(&Worker {
                    node: self.deps.node.clone(),
                    slot: worker.slot,
                    kind: worker.kind,
                    state: WorkerState::Idle,
                    run_id: None,
                    job_id: None,
                    window: None,
                    updated_at: self.deps.clock.now(),
                })?;
            }
        }
        Ok(())
    }

    /// Picks the first non-limited profile rung for a job.
    pub(crate) fn pick_choice(&self, job: &Job) -> Result<DispatchChoice, CoreError> {
        let profile = self
            .deps
            .config
            .profiles
            .get(&job.profile)
            .ok_or_else(|| CoreError::NotFound(format!("profile {}", job.profile)))?;
        let limited = super::limited_map(&self.store, self.deps.clock.now())?
            .into_iter()
            .collect::<HashMap<_, _>>();
        let (rung, executor) = profile
            .pick(&limited, self.deps.clock.now())
            .ok_or_else(|| CoreError::Invalid(format!("no available executor for {}", job.id)))?;
        Ok(DispatchChoice {
            job_id: job.id.clone(),
            executor: executor.clone(),
            rung: u32::try_from(rung).unwrap_or(u32::MAX),
            provider: executor.provider(),
            node: profile.node.clone(),
        })
    }

    /// Returns the newest verify job for the original's current attempt.
    fn find_verify_job(&self, original: &Job) -> Result<Option<Job>, CoreError> {
        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            kind: Some(JobKind::Verify),
            ..JobFilter::default()
        })?;
        let suffix = format!("-verify-{}", original.attempts);
        let retry_suffix = format!("{suffix}-r1");
        let mut matches = Vec::new();
        for job in jobs {
            if !job.id.as_str().ends_with(&suffix) && !job.id.as_str().ends_with(&retry_suffix) {
                continue;
            }
            for edge in self.store.edges_of(&job.id)? {
                if edge.kind == vowp_protocol::EdgeKind::Verifies && edge.dst == original.id {
                    matches.push(job.clone());
                    break;
                }
            }
        }
        matches.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(matches.pop())
    }

    /// Builds the graph and the job list for one project.
    pub(crate) fn graph(&self) -> Result<(Graph, Vec<Job>), CoreError> {
        let jobs = self.store.list_jobs(&JobFilter {
            project: Some(self.project.clone()),
            ..JobFilter::default()
        })?;
        let edges = self.store.list_edges(Some(&self.project))?;
        let graph = Graph::new(jobs.clone(), edges)?;
        Ok((graph, jobs))
    }

    /// Returns the open runs and the ids that still have a live owner thread.
    ///
    /// Both come from one `open_runs` snapshot. Reconciliation must use this same snapshot:
    /// a run inserted after it has no owner id yet and is not an orphan.
    ///
    /// Job runs live in `RunnerPool`; planner/keeper turns live in `SessionRunPool` and are
    /// matched by session id; probe runs live in `step_probes` and are matched by provider.
    /// Reconciliation must see all three or it finalises a live run as an orphan crash.
    pub(crate) fn open_runs_snapshot(&self) -> Result<(Vec<Run>, HashSet<RunId>), CoreError> {
        let open = self.store.open_runs()?;
        let mut active: HashSet<RunId> = self
            .runners
            .jobs()
            .into_iter()
            .filter(|(run, _)| self.runners.is_active(run))
            .map(|(run, _)| run)
            .collect();
        let sessions = self.session_runners.live_sessions();
        let probes = self.probe_tasks.keys().map(String::as_str).collect::<HashSet<_>>();
        for run in &open {
            let session_live = run
                .session_id
                .as_ref()
                .is_some_and(|session| sessions.contains(session));
            let probe_live = run.role == vowp_protocol::RunRole::Probe
                && run
                    .provider
                    .as_deref()
                    .is_some_and(|provider| probes.contains(provider));
            if session_live || probe_live {
                active.insert(run.id.clone());
            }
        }
        Ok((open, active))
    }

    /// Returns true when `name` is missing or active (a paused row skips its step).
    pub(crate) fn loop_active(&self, name: &str) -> bool {
        self.store
            .loop_(name)
            .ok()
            .flatten()
            .is_none_or(|loop_| loop_.state == ActiveState::Active)
    }

    /// Returns the L1 bounds used by this tick.
    fn bounds(&self) -> LoopBounds {
        self.store
            .loop_("l1-supervisor")
            .ok()
            .flatten()
            .map(|loop_| loop_.bounds)
            .unwrap_or_default()
    }

    /// Returns the transition counters from the L1 bounds.
    pub(crate) fn transition_bounds(&self) -> TransitionBounds {
        let bounds = self.bounds();
        TransitionBounds {
            max_continues: bounds.max_continues.unwrap_or(2),
            max_hang_restarts: bounds.max_hang_restarts.unwrap_or(3),
        }
    }

    /// Returns a one-line diff summary for the verifier prompt.
    fn diff_summary(&self, job: &Job) -> String {
        let Ok(repo) = super::main_repo(&self.deps, &self.store, &self.project) else {
            return "diff unavailable".to_owned();
        };
        let Some(branch) = &job.branch else {
            return "diff unavailable (no branch)".to_owned();
        };
        match self
            .deps
            .git
            .diff_names(&repo, super::default_branch(&self.deps), branch)
        {
            Ok(files) => format!(
                "`git diff {}...{branch}` ({} file(s): {})",
                super::default_branch(&self.deps),
                files.len(),
                files.join(", ")
            ),
            Err(error) => format!("diff unavailable: {error}"),
        }
    }

    /// Writes a file under the node root atomically (temp file then rename).
    fn atomic_write(&self, name: &str, text: &str) -> Result<(), CoreError> {
        let root = self.deps.paths.root();
        std::fs::create_dir_all(root).map_err(|source| CoreError::Io {
            path: root.display().to_string(),
            source,
        })?;
        let tmp = root.join(format!(".{name}.tmp"));
        let path = root.join(name);
        std::fs::write(&tmp, text).map_err(|source| CoreError::Io {
            path: tmp.display().to_string(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(())
    }

    /// Returns gate results for a job's current attempt.
    pub fn gate_results(&self, job: &JobId) -> Result<Vec<GateResult>, CoreError> {
        let job = self.store.require_job(job)?;
        Ok(self.store.gate_results_for(&job.id, job.attempts)?)
    }
}

/// One project's supervisor inside the node supervisor.
pub struct ProjectSupervisor {
    deps: Deps,
    project: ProjectName,
    inner: Supervisor,
    started: bool,
}

impl ProjectSupervisor {
    /// Returns the project this supervisor runs.
    pub fn name(&self) -> &ProjectName {
        &self.project
    }

    /// Returns the per-project dependencies (its own `vowp.toml` plus node defaults).
    pub fn deps(&self) -> &Deps {
        &self.deps
    }

    /// Returns the project store handle.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }
}

/// What one node pass reconciled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeReconcileReport {
    /// Projects with a loaded supervisor.
    pub projects: u32,
    /// Cross-database repairs summed over every project.
    pub cross_db: CrossDbReconcile,
    /// Migration performed by this startup, when the CLI asked for one.
    pub migration: Option<MigrationReport>,
    /// Running orchestrators whose app hosts were resumed.
    pub orchestrators_resumed: u32,
    /// Per-project startup counters.
    pub per_project: BTreeMap<ProjectName, ReconcileReport>,
}

/// Everything one node tick did.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeTickReport {
    /// Instant the tick started.
    pub at: Timestamp,
    /// Node-level steps, in order.
    pub steps: Vec<StepReport>,
    /// One report per project that was ticked.
    pub projects: BTreeMap<ProjectName, TickReport>,
    /// Projects that were not ticked, with the reason.
    pub skipped: Vec<(ProjectName, String)>,
}

/// One supervisor for every active project on the node.
///
/// The node lock and the node-level hooks live here. Each project keeps its own `Deps`,
/// its own attached database and its own [`Supervisor`], so a broken project can only
/// park itself.
pub struct NodeSupervisor {
    deps: Deps,
    orchestrators: Orchestrator,
    global: Store,
    projects: BTreeMap<ProjectName, ProjectSupervisor>,
    order: Vec<ProjectName>,
    next: usize,
    started_at: Timestamp,
    wake: WakeHandle,
    wake_rx: Option<Receiver<()>>,
    last_failure: BTreeMap<ProjectName, String>,
    pending_migration: Option<MigrationReport>,
}

impl NodeSupervisor {
    /// Opens the node registry and loads every active or paused project.
    pub fn new(deps: Deps) -> Result<Self, CoreError> {
        let orchestrators = Orchestrator::new(deps.clone());
        Self::with_orchestrator(deps, orchestrators)
    }

    /// Creates a node supervisor with explicit orchestrator app hosts.
    ///
    /// Production uses [`NodeSupervisor::new`], which builds the real opencode and
    /// Claude Code hosts. Tests pass fake hosts so a startup resume never launches an
    /// app.
    pub fn with_orchestrator(deps: Deps, orchestrators: Orchestrator) -> Result<Self, CoreError> {
        let global = Store::open_global(&deps.paths)?.with_clock(deps.clock.clone());
        ensure_node_project(&deps, &global)?;
        let (wake, wake_rx) = WakeHandle::channel();
        let mut supervisor = Self {
            started_at: deps.clock.now(),
            deps,
            orchestrators,
            global,
            projects: BTreeMap::new(),
            order: Vec::new(),
            next: 0,
            wake,
            wake_rx: Some(wake_rx),
            last_failure: BTreeMap::new(),
            pending_migration: None,
        };
        supervisor.reload_projects()?;
        Ok(supervisor)
    }

    /// Records the migration `vowp up` performed before building this supervisor.
    ///
    /// The CLI migrates a v0.1 home before [`NodeSupervisor::new`] can open it, so the
    /// result would otherwise be lost. [`NodeSupervisor::startup`] returns it in
    /// [`NodeReconcileReport::migration`].
    pub fn record_migration(&mut self, report: MigrationReport) {
        self.pending_migration = Some(report);
    }

    /// Startup reconciliation for every project.
    pub fn startup(&mut self) -> Result<NodeReconcileReport, CoreError> {
        self.reload_projects()?;
        let mut report = NodeReconcileReport {
            migration: self.pending_migration.take(),
            ..NodeReconcileReport::default()
        };
        match self.orchestrators.resume_all() {
            Ok(resumed) => {
                report.orchestrators_resumed = u32::try_from(resumed.len()).unwrap_or(u32::MAX);
            }
            Err(error) => self
                .deps
                .log
                .log(Level::Warn, "node", &format!("orchestrator resume: {error}")),
        }
        let names: Vec<ProjectName> = self.projects.keys().cloned().collect();
        for name in names {
            let Some(project) = self.projects.get_mut(&name) else {
                continue;
            };
            if project.started {
                continue;
            }
            match project.inner.startup() {
                Ok(project_report) => {
                    project.started = true;
                    report.projects += 1;
                    report.per_project.insert(name.clone(), project_report);
                    match project.inner.store.reconcile_cross_db() {
                        Ok(cross) => add_cross_db(&mut report.cross_db, cross),
                        Err(error) => {
                            self.deps
                                .log
                                .log(Level::Warn, "node", &format!("cross-db reconcile for {name}: {error}"))
                        }
                    }
                }
                Err(error) => {
                    self.deps
                        .log
                        .log(Level::Error, "node", &format!("startup for {name} failed: {error}"));
                    self.park_broken(&name, &format!("startup failed: {error}"))?;
                }
            }
        }
        Ok(report)
    }

    /// One node pass: node hooks, then every project in round-robin order.
    pub fn tick(&mut self) -> Result<NodeTickReport, CoreError> {
        let now = self.deps.clock.now();
        self.reload_projects()?;
        let mut steps = Vec::new();
        steps.push(self.node_step("sample", |this| {
            crate::capacity::CapacityManager::new(this.deps.clone())
                .sample(&this.global, now)
                .map(|sample| {
                    if sample.is_some() {
                        "host sampled".to_owned()
                    } else {
                        "no sampler".to_owned()
                    }
                })
        }));
        steps.push(self.node_step("advice", |this| {
            crate::capacity::advisor::advise_tick(&this.deps, &this.global, now).map(|advice| {
                advice.map_or_else(
                    || "no advice due".to_owned(),
                    |advice| format!("advice {} ({} workers)", advice.id, advice.proposed_target),
                )
            })
        }));
        steps.push(self.node_step("leases", |this| {
            crate::surface::sweep_leases(&this.deps, &this.global, now).map(|count| format!("{count} lease(s) swept"))
        }));
        steps.push(self.node_step("lifecycle", |this| {
            crate::lifecycle::tick(&this.deps, &this.global, now).map(|()| "lifecycle ticked".to_owned())
        }));
        // §4.2 step 1: the node drains node-scoped requests itself. A project's
        // `step_requests` also accepts node keys, but whichever project ticked first would
        // then apply a request for another project's job to the wrong database.
        steps.push(self.node_step("global", |this| this.step_global_requests()));

        let rotation = self.rotation();
        let mut projects = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut ticked = Vec::new();
        for name in &rotation {
            let Some(entry) = super::registry::project_entry(&self.global, name)? else {
                skipped.push((name.clone(), "not registered".to_owned()));
                continue;
            };
            let mode = match entry.state {
                ProjectState::Active => ProjectTickMode::Active,
                ProjectState::Paused if entry.paused_reason.starts_with("broken:") => {
                    skipped.push((name.clone(), entry.paused_reason.clone()));
                    continue;
                }
                ProjectState::Paused => ProjectTickMode::Maintenance,
                ProjectState::Archived => {
                    skipped.push((name.clone(), "archived".to_owned()));
                    continue;
                }
            };
            let Some(project) = self.projects.get_mut(name) else {
                skipped.push((name.clone(), "not loaded".to_owned()));
                continue;
            };
            if !project.started {
                match project.inner.startup() {
                    Ok(_) => {
                        project.started = true;
                        // §3.8: a project added after `NodeSupervisor::startup` must
                        // reconcile its cross-file rows before its first L1 tick too.
                        if let Err(error) = project.inner.store.reconcile_cross_db() {
                            self.deps
                                .log
                                .log(Level::Warn, "node", &format!("cross-db reconcile for {name}: {error}"));
                        }
                    }
                    Err(error) => {
                        self.last_failure.insert(name.clone(), error.to_string());
                        skipped.push((name.clone(), error.to_string()));
                        self.deps
                            .log
                            .log(Level::Error, "node", &format!("startup for {name} failed: {error}"));
                        self.park_broken(name, &format!("startup failed: {error}"))?;
                        continue;
                    }
                }
            }
            match project.inner.tick_project(mode, now) {
                Ok(report) => {
                    if report.steps.iter().any(|step| !step.ok) {
                        let failed: Vec<String> = report
                            .steps
                            .iter()
                            .filter(|step| !step.ok)
                            .map(|step| format!("{}: {}", step.name, step.detail))
                            .collect();
                        self.last_failure.insert(name.clone(), failed.join("; "));
                    } else {
                        self.last_failure.remove(name);
                    }
                    projects.insert(name.clone(), report);
                    ticked.push(name.as_str().to_owned());
                }
                Err(error) => {
                    self.last_failure.insert(name.clone(), error.to_string());
                    skipped.push((name.clone(), error.to_string()));
                    self.deps
                        .log
                        .log(Level::Error, "node", &format!("project {name} tick failed: {error}"));
                }
            }
            self.refresh_fail_streak(name)?;
        }
        self.advance_rotation();
        // Registry entries that never got a supervisor (archived, parked broken at load)
        // are still reported, so the node tick explains why a project is missing.
        for entry in super::registry::project_entries(&self.global)? {
            if entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT
                || self.projects.contains_key(&entry.name)
                || skipped.iter().any(|(name, _)| name == &entry.name)
            {
                continue;
            }
            let reason = match entry.state {
                ProjectState::Archived => "archived".to_owned(),
                ProjectState::Paused if !entry.paused_reason.is_empty() => entry.paused_reason.clone(),
                ProjectState::Paused => "paused".to_owned(),
                ProjectState::Active => "not loaded".to_owned(),
            };
            skipped.push((entry.name, reason));
        }
        steps.push(StepReport {
            name: "projects",
            ok: true,
            detail: format!("{} project(s) ticked: {}", ticked.len(), ticked.join(", ")),
        });

        steps.push(self.node_step("snapshot", |this| {
            crate::fleet::write_snapshot(&this.deps, &this.global, now).map(|()| "snapshot written".to_owned())
        }));

        // §4.2 step 4: the v0.1 compatibility board is written only when one non-reserved
        // project needs a tick. An active project owns the copy when there is exactly one.
        // With no active project, a single paused project still writes it: its maintenance
        // subset includes board/checkpoint, and a home whose only project is paused must not
        // leave board.json stale. A broken project is parked and not ticked, so it cannot
        // write the board.
        let entries = super::registry::project_entries(&self.global)?;
        let active: Vec<ProjectName> = entries
            .iter()
            .filter(|entry| {
                entry.state == ProjectState::Active && entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT
            })
            .map(|entry| entry.name.clone())
            .collect();
        let paused: Vec<ProjectName> = entries
            .iter()
            .filter(|entry| {
                entry.state == ProjectState::Paused
                    && !entry.paused_reason.starts_with("broken:")
                    && entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT
            })
            .map(|entry| entry.name.clone())
            .collect();
        let board_project = match active.as_slice() {
            [only] => Some(only.clone()),
            [] if paused.len() == 1 => paused.first().cloned(),
            _ => None,
        };
        if let Some(only) = board_project {
            let result = match self.projects.get_mut(&only) {
                Some(project) => project.inner.write_board(now),
                None => Ok("project not loaded".to_owned()),
            };
            steps.push(match result {
                Ok(detail) => StepReport {
                    name: "board",
                    ok: true,
                    detail,
                },
                Err(error) => {
                    self.deps
                        .log
                        .log(Level::Error, "node", &format!("board failed: {error}"));
                    StepReport {
                        name: "board",
                        ok: false,
                        detail: error.to_string(),
                    }
                }
            });
        } else {
            steps.push(StepReport {
                name: "board",
                ok: true,
                detail: format!(
                    "node board deferred: {} active and {} paused project(s)",
                    active.len(),
                    paused.len()
                ),
            });
        }

        let heartbeat = self.node_step("heartbeat", |this| {
            l8_watchdog::write_heartbeat(&this.deps)?;
            this.global.kv_set("supervisor_pid", &std::process::id().to_string())?;
            Ok("heartbeat written".to_owned())
        });
        // The heartbeat is one node-wide step, but every project has an `l8-watchdog`
        // row. Record the node result in each project DB after the file is written, so
        // `vowp loops --all` shows the same run time as `l1-supervisor`.
        for project in self.projects.values() {
            if let Err(error) = project.inner.record_heartbeat(now, heartbeat.ok, &heartbeat.detail) {
                self.deps.log.log(
                    Level::Error,
                    "node",
                    &format!("cannot record the L8 row for {}: {error}", project.project),
                );
            }
        }
        steps.push(heartbeat);

        Ok(NodeTickReport {
            at: now,
            steps,
            projects,
            skipped,
        })
    }

    /// §4.2 step 1: drains `req:node:*` and applies each request once on the node.
    ///
    /// A request that names a job or an inbox item is routed to the project database that
    /// owns it. A node-scoped loop request applies to every loaded project, and `Wake`
    /// has no project effect. A request that matches no project is dropped with a warning,
    /// exactly like a project step's undecodable request.
    fn step_global_requests(&mut self) -> Result<String, CoreError> {
        let requests = crate::requests::drain(&self.global)?;
        let total = requests.len();
        let mut applied = 0;
        let mut dropped = 0;
        for (id, request) in requests {
            match self.apply_node_request(&request) {
                Ok(true) => applied += 1,
                Ok(false) => {
                    dropped += 1;
                    self.deps.log.log(
                        Level::Warn,
                        "requests",
                        &format!(
                            "dropped node request {} {}: no loaded project owns it",
                            id.0,
                            request_name(&request)
                        ),
                    );
                }
                Err(error) => {
                    dropped += 1;
                    self.deps.log.log(
                        Level::Warn,
                        "requests",
                        &format!("dropped node request {} {}: {error}", id.0, request_name(&request)),
                    );
                }
            }
        }
        Ok(format!(
            "{applied} node request(s) applied, {dropped} dropped ({total} drained)"
        ))
    }

    /// Applies one node-scoped request to the project that owns it.
    ///
    /// Returns `Ok(false)` when no loaded project owns it. `Wake` is node-wide and needs
    /// no project; an unbound loop request applies to every loaded project.
    fn apply_node_request(&mut self, request: &crate::requests::Request) -> Result<bool, CoreError> {
        use crate::requests::Request;
        match request {
            Request::Wake => Ok(true),
            Request::PauseLoop { .. } | Request::ResumeLoop { .. } | Request::RunLoop { .. } => {
                if self.projects.is_empty() {
                    return Ok(false);
                }
                for project in self.projects.values_mut() {
                    project.inner.apply_request(request)?;
                }
                Ok(true)
            }
            Request::AnswerInbox { id } => {
                for project in self.projects.values_mut() {
                    if project.inner.store.inbox_item(id)?.is_some() {
                        project.inner.apply_request(request)?;
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Request::ResolveJob { job, .. }
            | Request::RetryJob { job, .. }
            | Request::ParkJob { job, .. }
            | Request::CancelJob { job, .. }
            | Request::NudgeJob { job, .. }
            | Request::RestartJob { job, .. }
            | Request::ReplanJob { job, .. }
            | Request::FailJob { job, .. } => {
                for project in self.projects.values_mut() {
                    if project.inner.store.job(job)?.is_some() {
                        project.inner.apply_request(request)?;
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Startup then tick every `[node].tick_secs` or on wake, until `cancel`.
    pub fn run(&mut self, cancel: &CancelToken) -> Result<(), CoreError> {
        self.startup()?;
        let tick = Duration::from_secs(self.deps.config.node.tick_secs.max(1));
        while !cancel.is_cancelled() {
            self.tick()?;
            if !wait_for_next_tick(self.wake_rx.as_ref(), tick, &self.global, cancel)? {
                break;
            }
        }
        for project in self.projects.values_mut() {
            if let Err(error) = project.inner.shutdown() {
                self.deps.log.log(
                    Level::Error,
                    "node",
                    &format!("shutdown for {}: {error}", project.project),
                );
            }
        }
        Ok(())
    }

    /// Returns a cloneable handle that ends the idle wait early.
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// Returns when this supervisor was constructed.
    pub fn started_at(&self) -> Timestamp {
        self.started_at
    }

    /// Returns the registered project names in the order the next pass will tick them.
    pub fn order(&self) -> Vec<ProjectName> {
        self.rotation()
    }

    /// Returns the loaded project names.
    pub fn projects(&self) -> impl Iterator<Item = &ProjectName> {
        self.projects.keys()
    }

    /// Returns one loaded project's supervisor.
    pub fn project(&self, name: &ProjectName) -> Option<&ProjectSupervisor> {
        self.projects.get(name)
    }

    /// Reloads registry entries and builds supervisors for new projects.
    ///
    /// Archived or removed projects lose their supervisor and their runner threads are
    /// stopped. A project whose config or database cannot be opened is parked alone with
    /// a node-inbox notice; the other projects keep ticking.
    fn reload_projects(&mut self) -> Result<(), CoreError> {
        let entries = super::registry::project_entries(&self.global)?;
        let mut keep = BTreeSet::new();
        for entry in &entries {
            if entry.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
                continue;
            }
            if entry.state == ProjectState::Archived
                || (entry.state == ProjectState::Paused && entry.paused_reason.starts_with("broken:"))
            {
                continue;
            }
            keep.insert(entry.name.clone());
            if self.projects.contains_key(&entry.name) {
                continue;
            }
            match project_supervisor(&self.deps, entry) {
                Ok(project) => {
                    self.projects.insert(entry.name.clone(), project);
                }
                Err(error) => {
                    self.deps.log.log(
                        Level::Error,
                        "node",
                        &format!("cannot load project {}: {error}", entry.name),
                    );
                    self.park_broken(&entry.name, &format!("cannot load project: {error}"))?;
                }
            }
        }

        let stale: Vec<ProjectName> = self
            .projects
            .keys()
            .filter(|name| !keep.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            if let Some(mut project) = self.projects.remove(&name) {
                let _ = project.inner.shutdown();
            }
            self.last_failure.remove(&name);
        }
        self.order = self.projects.keys().cloned().collect();
        if self.order.is_empty() {
            self.next = 0;
        } else {
            self.next %= self.order.len();
        }
        Ok(())
    }

    /// Returns the tick order: fairness weight first, then the last admitted project.
    ///
    /// `capacity::fair::order` sorts by weight and moves the last admitted project to the
    /// back of its weight group. Groups with no recorded last admission keep the old
    /// per-tick start offset so equal projects still round-robin instead of one staying first.
    fn rotation(&self) -> Vec<ProjectName> {
        if self.order.is_empty() {
            return Vec::new();
        }
        let Ok(entries) = super::registry::project_entries(&self.global) else {
            return self.name_rotation();
        };
        let mut entries: Vec<ProjectEntry> = entries
            .into_iter()
            .filter(|entry| self.projects.contains_key(&entry.name))
            .collect();
        if entries.is_empty() {
            return self.name_rotation();
        }
        let last = crate::capacity::admit::last_project(&self.global).ok().flatten();
        crate::capacity::fair::order(&mut entries, last.as_ref());
        self.rotate_weight_groups(&mut entries, last.as_ref());
        entries.into_iter().map(|entry| entry.name).collect()
    }

    /// Advances equal-weight groups that `capacity:last_project` does not pin.
    fn rotate_weight_groups(&self, entries: &mut [ProjectEntry], last: Option<&ProjectName>) {
        let mut start = 0;
        while start < entries.len() {
            let weight = entries[start].weight;
            let mut end = start + 1;
            while end < entries.len() && entries[end].weight == weight {
                end += 1;
            }
            let pinned = last.is_some_and(|last| entries[start..end].iter().any(|project| &project.name == last));
            if !pinned && end - start > 1 {
                let offset = self.next % (end - start);
                entries[start..end].rotate_left(offset);
            }
            start = end;
        }
    }

    /// Returns the old name-order rotation, used only when the registry cannot be read.
    fn name_rotation(&self) -> Vec<ProjectName> {
        let start = self.next % self.order.len();
        let mut order = Vec::with_capacity(self.order.len());
        order.extend(self.order[start..].iter().cloned());
        order.extend(self.order[..start].iter().cloned());
        order
    }

    /// Advances the round-robin start offset after one pass.
    fn advance_rotation(&mut self) {
        if !self.order.is_empty() {
            self.next = (self.next + 1) % self.order.len();
        }
    }

    /// Copies the durable L1 fail streak for `name` and parks it at three.
    fn refresh_fail_streak(&mut self, name: &ProjectName) -> Result<(), CoreError> {
        let Some(project) = self.projects.get(name) else {
            return Ok(());
        };
        let streak = project
            .inner
            .store
            .loop_("l1-supervisor")?
            .map(|loop_| loop_.fail_streak)
            .unwrap_or(0);
        if streak >= 3 {
            let reason = self
                .last_failure
                .get(name)
                .cloned()
                .unwrap_or_else(|| "unknown failure".to_owned());
            self.park_broken(name, &format!("3 failed ticks: {reason}"))?;
        }
        Ok(())
    }

    /// Parks one project with a `broken:` reason and opens one node-inbox notice.
    fn park_broken(&mut self, name: &ProjectName, reason: &str) -> Result<(), CoreError> {
        let entry = super::registry::project_entry(&self.global, name)?;
        let Some(entry) = entry else {
            return Ok(());
        };
        if entry.state == ProjectState::Archived {
            return Ok(());
        }
        if entry.state == ProjectState::Paused && entry.paused_reason.starts_with("broken:") {
            return Ok(());
        }
        let detail = if let Some(failures) = reason.strip_prefix("3 failed ticks: ") {
            format!("project {name} paused after 3 failed ticks: {failures}")
        } else {
            format!("project {name} paused: {reason}")
        };
        super::registry::set_project_state(&self.global, name, ProjectState::Paused, &format!("broken: {reason}"))?;
        let title = format!("project {name} paused");
        let already_open = self
            .global
            .list_inbox(Some(InboxState::Open))?
            .iter()
            .any(|item| item.project.as_ref() == Some(name) && item.title == title);
        if !already_open {
            let now = self.deps.clock.now();
            self.global.insert_node_inbox(&InboxItem {
                id: InboxId::new(),
                project: Some(name.clone()),
                job_id: None,
                kind: InboxKind::Notice,
                title,
                body: detail.clone(),
                options: Vec::new(),
                state: InboxState::Open,
                answer: None,
                answered_by: None,
                created_at: now,
                answered_at: None,
            })?;
        }
        self.deps.log.log(Level::Warn, "node", &detail);
        Ok(())
    }

    /// Runs one node-level step under `catch_unwind`.
    fn node_step<F>(&mut self, name: &'static str, f: F) -> StepReport
    where
        F: FnOnce(&mut Self) -> Result<String, CoreError>,
    {
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(Ok(detail)) => StepReport { name, ok: true, detail },
            Ok(Err(error)) => {
                self.deps
                    .log
                    .log(Level::Error, "node", &format!("step {name} failed: {error}"));
                StepReport {
                    name,
                    ok: false,
                    detail: error.to_string(),
                }
            }
            Err(payload) => {
                let detail = panic_text(payload);
                self.deps
                    .log
                    .log(Level::Error, "node", &format!("step {name} panicked: {detail}"));
                StepReport {
                    name,
                    ok: false,
                    detail: format!("panic: {detail}"),
                }
            }
        }
    }
}

/// Ensures the reserved `_node` registry row and its database exist.
fn ensure_node_project(deps: &Deps, global: &Store) -> Result<(), CoreError> {
    if global.local_node()?.is_none() {
        global.upsert_node(&Node {
            name: deps.node.clone(),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({ "vowp": env!("CARGO_PKG_VERSION") }),
            last_seen_at: Some(deps.clock.now()),
            created_at: deps.clock.now(),
        })?;
    }
    let node = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    if global.project(&node)?.is_none() {
        global.upsert_project(&vowp_protocol::Project {
            name: node.clone(),
            path: deps.paths.root().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: deps.clock.now(),
            updated_at: deps.clock.now(),
        })?;
    } else {
        Store::create_project_db(&deps.paths, &node)?;
    }
    Ok(())
}

/// Builds one project's supervisor from its registry row and `vowp.toml`.
fn project_supervisor(base: &Deps, entry: &ProjectEntry) -> Result<ProjectSupervisor, CoreError> {
    let deps = project_deps(base, entry)?;
    let inner = Supervisor::new(deps.clone(), entry.name.clone())?;
    Ok(ProjectSupervisor {
        deps,
        project: entry.name.clone(),
        inner,
        started: false,
    })
}

/// Resolves the project-scoped [`Deps`] described by the spec (§3.8).
fn project_deps(base: &Deps, entry: &ProjectEntry) -> Result<Deps, CoreError> {
    let config_path = Path::new(&entry.path).join("vowp.toml");
    let loaded_from_project = config_path.is_file();
    let mut config = if loaded_from_project {
        let text = std::fs::read_to_string(&config_path).map_err(|source| CoreError::Io {
            path: config_path.display().to_string(),
            source,
        })?;
        let mut config = VowpConfig::from_toml(&text)?;
        let node_path = base.paths.root().join("config.toml");
        if node_path.is_file() {
            let node_text = std::fs::read_to_string(&node_path).map_err(|source| CoreError::Io {
                path: node_path.display().to_string(),
                source,
            })?;
            let node_file = NodeFile::from_toml(&node_text)?;
            config = config.with_node_defaults(&node_file);
        }
        config.validate()?;
        config
    } else {
        (*base.config).clone()
    };
    if config.project.name != entry.name {
        return Err(CoreError::Invalid(format!(
            "{} names project {} but the registry entry is {}",
            config_path.display(),
            config.project.name,
            entry.name
        )));
    }
    if config.project.path.is_none() {
        config.project.path = Some(entry.path.clone());
    }
    // A project without a `vowp.toml` keeps the caller's registry: that is the v0.1
    // single-project seam and the hermetic test seam where fake adapters are injected.
    let adapters = if loaded_from_project {
        Arc::new(crate::deps::AdapterRegistry::from_config(&config)?)
    } else {
        base.adapters.clone()
    };
    let mut deps = base.clone();
    deps.config = Arc::new(config);
    deps.adapters = adapters;
    Ok(deps)
}

/// Adds one cross-database reconcile result into an aggregate.
fn add_cross_db(total: &mut CrossDbReconcile, one: CrossDbReconcile) {
    total.runs_closed += one.runs_closed;
    total.runs_missing_session += one.runs_missing_session;
    total.sessions_repaired += one.sessions_repaired;
    total.terminal_events_repaired += one.terminal_events_repaired;
    total.ledgers_repaired += one.ledgers_repaired;
    total.leases_dropped += one.leases_dropped;
}

/// Renders the board snapshot as the JSON convenience copy written to `<node>/board.json`.
fn board_json(snapshot: &BoardSnapshot) -> Result<String, CoreError> {
    let running = snapshot
        .running
        .iter()
        .map(|run| {
            serde_json::json!({
                "job": run.job,
                "executor": run.executor.as_ref().map(ToString::to_string),
                "age_secs": run.age_secs,
                "last_event_age_secs": run.last_event_age_secs,
            })
        })
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "project": snapshot.project,
        "at": snapshot.at.as_rfc3339(),
        "jobs_by_state": snapshot.jobs_by_state,
        "running": running,
        "slots_busy": snapshot.slots_busy,
        "slots_total": snapshot.slots_total,
        "planner_executor": snapshot.planner_executor.as_ref().map(ToString::to_string),
        "keeper_executor": snapshot.keeper_executor.as_ref().map(ToString::to_string),
        "limits": snapshot.limits,
        "cost_today": snapshot.cost_today,
        "inbox_open": snapshot.inbox_open,
        "last_checkpoint": snapshot.last_checkpoint.map(|ts| ts.as_rfc3339()),
    });
    serde_json::to_string_pretty(&value).map_err(|error| CoreError::Invalid(error.to_string()))
}

/// Renders a panic payload as text.
fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Returns the snake-case request name used in logs and ledger details.
fn request_name(request: &crate::requests::Request) -> &'static str {
    use crate::requests::Request;
    match request {
        Request::ResolveJob { .. } => "resolve_job",
        Request::RunLoop { .. } => "run_loop",
        Request::RetryJob { .. } => "retry_job",
        Request::ParkJob { .. } => "park_job",
        Request::CancelJob { .. } => "cancel_job",
        Request::NudgeJob { .. } => "nudge_job",
        Request::RestartJob { .. } => "restart_job",
        Request::ReplanJob { .. } => "replan_job",
        Request::FailJob { .. } => "fail_job",
        Request::AnswerInbox { .. } => "answer_inbox",
        Request::PauseLoop { .. } => "pause_loop",
        Request::ResumeLoop { .. } => "resume_loop",
        Request::Wake => "wake",
    }
}

/// Waits until the next tick deadline, an early wake or a queued CLI request.
///
/// The wait is sliced into one-second polls so a `req:*` row pushed by the CLI ends the
/// wait early, but a quiet tick still runs at `[node].tick_secs` (loops.md §1.1), not once
/// per poll interval. Returns false when the wake channel disconnected.
fn wait_for_next_tick(
    receiver: Option<&Receiver<()>>,
    tick: Duration,
    store: &Store,
    cancel: &CancelToken,
) -> Result<bool, CoreError> {
    let deadline = Instant::now() + tick;
    loop {
        if cancel.is_cancelled() {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(true);
        }
        let slice = remaining.min(Duration::from_secs(1));
        match receiver {
            Some(receiver) => match receiver.recv_timeout(slice) {
                Ok(()) => return Ok(true),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !store.kv_prefix("req:")?.is_empty() {
                        return Ok(true);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
            },
            None => {
                std::thread::sleep(slice);
                if !store.kv_prefix("req:")?.is_empty() {
                    return Ok(true);
                }
            }
        }
    }
}

#[cfg(test)]
mod wait_tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use tempfile::tempdir;
    use vowp_protocol::CancelToken;
    use vowp_store::{NodePaths, Store};

    use super::wait_for_next_tick;

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempdir().expect("tempdir");
        let store = Store::open(&NodePaths::new(tmp.path().join("node"))).expect("store");
        (tmp, store)
    }

    #[test]
    fn tick_wait_does_not_shorten_the_configured_interval() {
        let (_tmp, store) = store();
        let (_tx, rx) = channel();
        let cancel = CancelToken::default();
        let started = std::time::Instant::now();
        let keep = wait_for_next_tick(Some(&rx), Duration::from_millis(1300), &store, &cancel).expect("wait");
        assert!(keep);
        assert!(
            started.elapsed() >= Duration::from_millis(1200),
            "a quiet tick waits for the deadline, not the 1 s poll: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn request_row_ends_the_wait_early() {
        let (_tmp, store) = store();
        store.push_request("wake", &serde_json::json!({})).expect("request");
        let (_tx, rx) = channel();
        let cancel = CancelToken::default();
        let started = std::time::Instant::now();
        assert!(wait_for_next_tick(Some(&rx), Duration::from_secs(30), &store, &cancel).expect("wait"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the queued request ends the wait early: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn channel_wake_ends_the_wait_immediately() {
        let (_tmp, store) = store();
        let (tx, rx) = channel();
        tx.send(()).expect("wake");
        let cancel = CancelToken::default();
        let started = std::time::Instant::now();
        assert!(wait_for_next_tick(Some(&rx), Duration::from_secs(30), &store, &cancel).expect("wait"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancel_ends_the_wait_before_the_deadline() {
        let (_tmp, store) = store();
        let (_tx, rx) = channel();
        let cancel = CancelToken::default();
        cancel.cancel();
        let started = std::time::Instant::now();
        assert!(wait_for_next_tick(Some(&rx), Duration::from_secs(30), &store, &cancel).expect("wait"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn tick_wait_without_a_receiver_also_waits_for_the_deadline() {
        let (_tmp, store) = store();
        let cancel = CancelToken::default();
        let started = std::time::Instant::now();
        assert!(wait_for_next_tick(None, Duration::from_millis(1300), &store, &cancel).expect("wait"));
        assert!(
            started.elapsed() >= Duration::from_millis(1200),
            "the no-receiver branch uses the same deadline: {:?}",
            started.elapsed()
        );
    }
}
