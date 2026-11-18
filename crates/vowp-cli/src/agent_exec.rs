//! The Mac-side assignment executor: worktree, adapter, events and result push.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3–§6.6, §7.3, §10.6 and §14.3.
//! [`CoreRunHandler`] is the real [`RunHandler`] behind `vowp agent run`. It resolves one
//! assignment against the saved project path map, creates or reuses the job worktree, starts
//! the adapter in it, streams redacted events through the run sink, pushes only `job/<id>`
//! and reports the head commit in the terminal summary.
//!
//! Only names cross the wire: the assignment carries `env_names` and `secret_names`, this
//! module resolves the values locally and replaces every injected value with
//! `[REDACTED:<NAME>]` before a line reaches the sink. Absolute worktree and project paths
//! are replaced with their `PathRef` spelling for the same reason. A refusal carries a typed
//! [`RunRejectReason`] and a value-free detail, never an absolute path or a secret value.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use vowp_agent::{
    AcceptedRun, AgentConfig, DEFAULT_DISK_RESERVE_BYTES, HandlerReject, PathMap, PushStatus, RunHandler, RunSink,
    RunSinkError, RunSummary, WorktreeError, WorktreeManager, WorktreeSpec,
};
use vowp_protocol::{
    AdapterRun, AgentPathMap, AssignmentPayload, Cost, EventBody, EventId, EventSource, Executor, HarnessEvent, JobId,
    NodeName, ProjectName, RemoteEvent, RemoteLine, RemoteRunState, RunId, RunOutcome, RunRejectReason,
    RunSummary as AdapterSummary, StartInput, StopReason, Timestamp, VowpConfig,
};
use vowp_store::NodePaths;

/// How often the run loop checks the adapter event channel and cancellation.
const EVENT_POLL: Duration = Duration::from_millis(100);

/// How often a running assignment sends a liveness line while the adapter is quiet.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);

/// How long [`CoreRunHandler::cleanup`] waits for a cancelled run to leave its worktree.
const CLEANUP_WAIT: Duration = Duration::from_secs(10);

/// The real assignment handler behind `vowp agent run`.
///
/// One handler lives for one agent connection. It owns no transport and no spool: the
/// injected [`RunSink`] spools every line, and the worktree manager owns the git side.
/// `accept` is deliberately cheap and performs the local preflight; the worktree and the
/// adapter process start only in `run`.
pub struct CoreRunHandler {
    config: AgentConfig,
    reserve_bytes: u64,
    environment: Environment,
    runs: Mutex<BTreeMap<RunId, Arc<RunControl>>>,
}

impl CoreRunHandler {
    /// Builds a handler for an enrolled agent config.
    ///
    /// The config supplies the run's node identity and a fallback path map. `accept` reads
    /// `agent.json` so a project mapped while the agent is connected is visible without a
    /// restart. Environment and secret names resolve from the process environment.
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            reserve_bytes: DEFAULT_DISK_RESERVE_BYTES,
            environment: Environment::Process,
            runs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replaces the free-space reserve checked before an assignment is accepted.
    ///
    /// The default is [`DEFAULT_DISK_RESERVE_BYTES`]; tests use a small reserve so a
    /// temporary filesystem does not need five free gigabytes.
    pub fn with_reserve(mut self, reserve_bytes: u64) -> Self {
        self.reserve_bytes = reserve_bytes;
        self
    }

    /// Replaces the process environment with a fixed name-to-value map.
    ///
    /// Tests use this so they never mutate the real process environment. A name absent
    /// from the map is unresolved, exactly like an unset environment variable.
    pub fn with_env(mut self, values: BTreeMap<String, String>) -> Self {
        self.environment = Environment::Fixed(values);
        self
    }

    /// Returns the run control inserted by `accept`.
    ///
    /// A missing control means `cleanup` won a race with the handler thread: the run must
    /// not start after its worktree was released.
    fn control(&self, run: &RunId) -> Option<Arc<RunControl>> {
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .cloned()
    }

    /// Loads the checkout's `vowp.toml` without touching the real home configuration.
    fn project_config(&self, project_root: &Path) -> Result<VowpConfig, crate::CliError> {
        crate::ctx::load_project_config(&project_root.join("vowp.toml"), &NodePaths::new(&self.config.state_dir))
    }

    /// Returns the current saved map for one project.
    ///
    /// `vowp agent paths add|rm` rewrites `agent.json` while the agent is connected, and the
    /// client already reports the change to the home. Reading the file here keeps the handler
    /// in step; the config captured at construction is the fallback for a transient bad read.
    fn path_map(&self, project: &ProjectName) -> Option<AgentPathMap> {
        match AgentConfig::load(&self.config.state_dir) {
            // A successfully read file is authoritative: a project removed by `agent paths
            // rm` must not still resolve through the startup snapshot.
            Ok(config) => config.path_map(project).cloned(),
            // A transient bad read (for example a half-written file) keeps the last good map.
            Err(_) => self.config.path_map(project).cloned(),
        }
    }

    /// Builds the accepted-run metadata and the per-run control slot.
    fn preflight(&self, assignment: &AssignmentPayload) -> Result<(AcceptedRun, Arc<RunControl>), HandlerReject> {
        let map = self.path_map(&assignment.project).ok_or_else(|| {
            HandlerReject::new(
                RunRejectReason::ProjectUnmapped,
                format!("project {} is not mapped on this node", assignment.project),
            )
        })?;
        let path_map =
            PathMap::new(&map).map_err(|error| HandlerReject::new(error.reject_reason(), reject_detail(&error)))?;
        let cwd = path_map
            .resolve(&assignment.project, &assignment.cwd_ref)
            .map_err(|error| HandlerReject::new(error.reject_reason(), reject_detail(&error)))?;
        let config = self
            .project_config(path_map.local_root())
            .map_err(|_| HandlerReject::other("the project checkout has no readable vowp.toml"))?;
        let adapter = crate::chat::agent::adapter_for(&config, &assignment.executor).map_err(|_| {
            HandlerReject::new(
                RunRejectReason::HarnessMissing,
                format!(
                    "harness {} has no adapter on this node",
                    assignment.executor.harness.as_str()
                ),
            )
        })?;
        if !adapter.probe().ok {
            return Err(HandlerReject::new(
                RunRejectReason::HarnessMissing,
                format!(
                    "harness {} is not installed or not runnable on this node",
                    assignment.executor.harness.as_str()
                ),
            ));
        }
        resolve_environment(assignment, &self.environment)?;
        let free = disk_free_bytes(path_map.worktree_root());
        if free < self.reserve_bytes {
            return Err(HandlerReject::new(
                RunRejectReason::DiskLow,
                "free disk is below the node reserve",
            ));
        }
        {
            let runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            if runs
                .iter()
                .any(|(run, slot)| run != &assignment.run_id && slot.job == assignment.job)
            {
                return Err(HandlerReject::new(
                    RunRejectReason::WorktreeConflict,
                    format!("another run already holds the worktree for job {}", assignment.job),
                ));
            }
        }
        let worktree = path_map
            .worktree_path(&assignment.job)
            .map_err(|error| HandlerReject::new(error.reject_reason(), reject_detail(&error)))?;
        if std::fs::symlink_metadata(&worktree).is_ok() {
            // The path already exists. Let the authoritative worktree manager apply its
            // ownership, branch, cleanliness and base checks now, so an unowned directory is
            // a typed refusal instead of a failed run. A missing path stays untouched until
            // `run` creates it, keeping `accept` cheap for the common fresh-job case.
            let spec = WorktreeSpec::new(assignment.job.clone(), assignment.base_commit.clone())
                .with_remote(assignment.remote.clone())
                .with_reserve(self.reserve_bytes);
            WorktreeManager::new(path_map.clone())
                .create(&spec)
                .map_err(|error| HandlerReject::new(error.reject_reason(), reject_detail(&error)))?;
        }
        let control = Arc::new(RunControl {
            path_map: path_map.clone(),
            job: assignment.job.clone(),
            cancel: AtomicBool::new(false),
            started: AtomicBool::new(false),
            adapter: Mutex::new(None),
            finished: AtomicBool::new(false),
        });
        self.runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(assignment.run_id.clone(), Arc::clone(&control));
        let accepted = AcceptedRun {
            assignment: assignment.clone(),
            epoch: assignment.epoch,
            cwd,
            worktree,
            project_root: path_map.local_root().to_path_buf(),
            reserve_bytes: self.reserve_bytes,
        };
        Ok((accepted, control))
    }

    /// Runs one assignment and always reports a terminal summary.
    fn execute(&self, accepted: &AcceptedRun, control: &Arc<RunControl>, sink: &mut dyn RunSink) -> RunSummary {
        let assignment = &accepted.assignment;
        if control.cancel.load(Ordering::SeqCst) {
            return stopped_summary("cancelled", None);
        }
        if sink.is_stopped() {
            return stopped_summary("fenced", None);
        }
        // The accepted run keeps the map it was validated against, so a later `agent paths
        // rm` cannot move cleanup to a different root or strand this worktree.
        let manager = WorktreeManager::new(control.path_map.clone());
        let spec = WorktreeSpec::new(assignment.job.clone(), assignment.base_commit.clone())
            .with_remote(assignment.remote.clone())
            .with_reserve(accepted.reserve_bytes);
        let Ok(worktree) = manager.create(&spec) else {
            return error_summary("worktree_failed", None);
        };
        let head_after_create = worktree.head_commit().ok();
        if control.cancel.load(Ordering::SeqCst) {
            return stopped_summary("cancelled", head_after_create);
        }
        if sink.is_stopped() {
            return stopped_summary("fenced", head_after_create);
        }
        let Some(project_config) = self.project_config(manager.map().local_root()).ok() else {
            return error_summary("project_config_failed", head_after_create);
        };
        let Some(adapter) = crate::chat::agent::adapter_for(&project_config, &assignment.executor).ok() else {
            return error_summary("harness_missing", head_after_create);
        };
        let Ok(environment) = resolve_environment(assignment, &self.environment) else {
            return error_summary("env_missing", head_after_create);
        };
        let redactions = Redactions::new(
            &environment,
            worktree.path(),
            manager.map().local_root(),
            &assignment.project,
            &assignment.job,
            &self.config.state_dir,
        );
        let start = StartInput {
            session_id: assignment.session_id.clone(),
            executor: assignment.executor.clone(),
            cwd: worktree.path().to_path_buf(),
            prompt: assignment.prompt.clone(),
            resume: None,
            new_native_id: None,
            env: environment.clone(),
            permission_mode: None,
            system_note: None,
            event_log_hint: None,
            timeout: None,
        };
        let Ok(started) = adapter.start(start) else {
            return error_summary("adapter_start_failed", head_after_create);
        };
        {
            let mut adapter = control.adapter.lock().unwrap_or_else(PoisonError::into_inner);
            *adapter = Some(started);
        }

        let mut cancelled = false;
        let mut fenced = false;
        let mut spool_full = false;
        let mut last_heartbeat = Instant::now();
        loop {
            if control.cancel.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            if sink.is_stopped() {
                fenced = true;
                break;
            }
            let received = {
                let mut adapter = control.adapter.lock().unwrap_or_else(PoisonError::into_inner);
                match adapter.as_mut() {
                    Some(run) => run.events().recv_timeout(EVENT_POLL),
                    None => break,
                }
            };
            match received {
                Ok(event) => {
                    // The terminal TurnSummary is the canonical `turn.ended`; forwarding the
                    // adapter's own terminal event would append it twice.
                    if matches!(event.body, EventBody::TurnEnded(_)) {
                        continue;
                    }
                    let line = remote_event(&event, &assignment.executor, &self.config.node, &redactions);
                    if let Err(error) = sink.line(RemoteLine::Event { event: line }) {
                        match error {
                            RunSinkError::SpoolFull { .. } => {
                                spool_full = true;
                                break;
                            }
                            RunSinkError::Stopped { .. } | RunSinkError::Unsupported { .. } => {
                                fenced = true;
                                break;
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
                        last_heartbeat = Instant::now();
                        let _ = sink.line(RemoteLine::Heartbeat { at: Timestamp::now() });
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if cancelled || fenced || spool_full || control.cancel.load(Ordering::SeqCst) {
            if let Some(run) = control.adapter.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                let _ = run.stop();
            }
        }
        let adapter_summary = control
            .adapter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|run| run.wait().ok());
        let head = worktree.head_commit().ok().or(head_after_create);
        let mut summary = build_summary(adapter_summary.as_ref(), &redactions, head.clone());
        let stopped = cancelled || fenced || spool_full || control.cancel.load(Ordering::SeqCst) || sink.is_stopped();
        if spool_full {
            // A spool-full stop also makes the driver cancel the handler, but the cap is the
            // reason the home needs to see.
            apply_stop(&mut summary, StopCause::SpoolFull);
        } else if control.cancel.load(Ordering::SeqCst) || cancelled {
            apply_stop(&mut summary, StopCause::Cancelled);
        } else if fenced || sink.is_stopped() {
            apply_stop(&mut summary, StopCause::Fenced);
        }
        if !stopped && !assignment.remote_url.trim().is_empty() {
            let _ = sink.state(RemoteRunState::Finishing, "pushing the job branch");
            summary.push_status = Some(match worktree.push_job(&assignment.remote_url) {
                Ok(pushed) => PushStatus::Pushed {
                    branch: pushed.branch,
                    remote: pushed.remote,
                    head_commit: pushed.head_commit,
                },
                Err(error) => {
                    let detail = push_detail(&error);
                    summary.outcome = RunOutcome::Error;
                    summary.stop_reason = StopReason::Error;
                    summary.result_text = Some(redactions.text(&format!("git_push_failed: {detail}")));
                    PushStatus::Failed { detail }
                }
            });
        } else if !stopped {
            summary.push_status = Some(PushStatus::NotRequested);
        }
        summary.head_commit = head;
        summary
    }
}

impl RunHandler for CoreRunHandler {
    fn accept(&self, assignment: &AssignmentPayload) -> Result<AcceptedRun, HandlerReject> {
        self.preflight(assignment).map(|(accepted, _)| accepted)
    }

    fn run(&self, accepted: AcceptedRun, sink: &mut dyn RunSink) {
        let Some(control) = self.control(&accepted.assignment.run_id) else {
            let _ = sink.finish(stopped_summary("cancelled", None));
            return;
        };
        control.started.store(true, Ordering::SeqCst);
        let _finished = FinishGuard(Arc::clone(&control));
        let summary = self.execute(&accepted, &control, sink);
        let _ = sink.finish(summary);
    }

    fn cancel(&self, run: &RunId) {
        let control = {
            let runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            runs.get(run).cloned()
        };
        let Some(control) = control else {
            return;
        };
        control.cancel.store(true, Ordering::SeqCst);
        if let Some(adapter) = control.adapter.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            let _ = adapter.stop();
        }
    }

    fn cleanup(&self, run: &RunId) {
        let control = {
            let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
            runs.remove(run)
        };
        let Some(control) = control else {
            return;
        };
        let started = Instant::now();
        if control.started.load(Ordering::SeqCst) {
            while !control.finished.load(Ordering::SeqCst) && started.elapsed() < CLEANUP_WAIT {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = WorktreeManager::new(control.path_map.clone()).cleanup(&control.job);
    }
}

/// Per-run state shared by the handler thread, `cancel` and `cleanup`.
struct RunControl {
    path_map: PathMap,
    job: JobId,
    cancel: AtomicBool,
    started: AtomicBool,
    adapter: Mutex<Option<Box<dyn AdapterRun>>>,
    finished: AtomicBool,
}

/// Marks a run as finished when its thread leaves `run`, including on an early return.
struct FinishGuard(Arc<RunControl>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::SeqCst);
    }
}

/// Where a run's environment values come from.
enum Environment {
    /// The real process environment.
    Process,
    /// A fixed map used by tests.
    Fixed(BTreeMap<String, String>),
}

impl Environment {
    /// Returns the value for one name; an empty value counts as unset.
    fn get(&self, name: &str) -> Option<String> {
        let value = match self {
            Self::Process => std::env::var(name).ok(),
            Self::Fixed(values) => values.get(name).cloned(),
        };
        value.filter(|value| !value.is_empty())
    }
}

/// Resolves the assignment's name lists into the adapter's environment pairs.
fn resolve_environment(
    assignment: &AssignmentPayload,
    environment: &Environment,
) -> Result<Vec<(String, String)>, HandlerReject> {
    let mut resolved = Vec::new();
    let mut seen = BTreeSet::new();
    for name in &assignment.env_names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let value = environment.get(name).ok_or_else(|| {
            HandlerReject::new(
                RunRejectReason::EnvMissing,
                format!("environment name {name} is not set on this node"),
            )
        })?;
        resolved.push((name.clone(), value));
    }
    for name in &assignment.secret_names {
        let key = name.as_str();
        if !seen.insert(key.to_owned()) {
            continue;
        }
        let value = environment.get(key).ok_or_else(|| {
            HandlerReject::new(
                RunRejectReason::EnvMissing,
                format!("secret name {name} is not available on this node"),
            )
        })?;
        resolved.push((key.to_owned(), value));
    }
    Ok(resolved)
}

/// Builds one redacted remote event with a fresh, stable event id.
fn remote_event(event: &HarnessEvent, executor: &Executor, node: &NodeName, redactions: &Redactions) -> RemoteEvent {
    let mut payload = event.body.to_value();
    redactions.json(&mut payload);
    let mut raw = event.raw.clone();
    if let Some(raw) = raw.as_mut() {
        redactions.json(raw);
    }
    RemoteEvent {
        id: EventId::new(),
        ts: event.ts.unwrap_or_else(Timestamp::now),
        event_type: event.body.event_type(),
        actor: event.actor,
        source: EventSource {
            harness: executor.harness,
            model: Some(executor.model.clone()),
            node: Some(node.clone()),
            native_session_id: None,
        },
        payload,
        raw,
        replayable: !matches!(event.body, EventBody::AssistantReasoning(_)),
    }
}

/// Builds the summary from the adapter result, with values and paths redacted.
fn build_summary(adapter: Option<&AdapterSummary>, redactions: &Redactions, head: Option<String>) -> RunSummary {
    let mut limit = adapter.and_then(|summary| summary.limit.clone());
    if let Some(event) = limit.as_mut() {
        event.message = redactions.text(&event.message);
    }
    RunSummary {
        exit_code: adapter.and_then(|summary| summary.exit_code),
        outcome: adapter.map(|summary| summary.outcome).unwrap_or(RunOutcome::Error),
        native_session_id: adapter.and_then(|summary| summary.native_session_id.clone()),
        cost: adapter
            .map(|summary| summary.cost.clone())
            .unwrap_or_else(Cost::unknown),
        limit,
        usage: None,
        head_commit: head,
        push_status: None,
        result_text: adapter
            .and_then(|summary| summary.result_text.as_deref())
            .map(|text| redactions.text(text)),
        stop_reason: adapter.map(|summary| summary.stop_reason).unwrap_or(StopReason::Error),
    }
}

/// Why a run stopped before it could push.
#[derive(Clone, Copy)]
enum StopCause {
    Cancelled,
    Fenced,
    SpoolFull,
}

/// Applies a local stop cause to a summary, keeping the harness exit code when there was one.
fn apply_stop(summary: &mut RunSummary, cause: StopCause) {
    match cause {
        StopCause::Cancelled => {
            summary.outcome = RunOutcome::Killed;
            summary.stop_reason = StopReason::Interrupted;
            summary.result_text = Some("cancelled".to_owned());
        }
        StopCause::Fenced => {
            summary.outcome = RunOutcome::Killed;
            summary.stop_reason = StopReason::Interrupted;
            summary.result_text = Some("fenced".to_owned());
        }
        StopCause::SpoolFull => {
            summary.outcome = RunOutcome::Error;
            summary.stop_reason = StopReason::Error;
            summary.result_text = Some("spool_full".to_owned());
        }
    }
}

/// Builds a summary for a run that stopped before an adapter summary existed.
fn stopped_summary(cause: &str, head: Option<String>) -> RunSummary {
    RunSummary {
        exit_code: None,
        outcome: RunOutcome::Killed,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        usage: None,
        head_commit: head,
        push_status: None,
        result_text: Some(cause.to_owned()),
        stop_reason: StopReason::Interrupted,
    }
}

/// Builds a summary for a run that failed before it could start the adapter.
fn error_summary(cause: &str, head: Option<String>) -> RunSummary {
    RunSummary {
        exit_code: None,
        outcome: RunOutcome::Error,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        usage: None,
        head_commit: head,
        push_status: None,
        result_text: Some(cause.to_owned()),
        stop_reason: StopReason::Error,
    }
}

/// Returns a path-free refusal detail for one worktree error.
fn reject_detail(error: &WorktreeError) -> &'static str {
    match error {
        WorktreeError::ProjectMismatch { .. } => "the path map names another project",
        WorktreeError::InvalidPathRef { .. } => "the working-directory ref is invalid",
        WorktreeError::InvalidPathMapEntry { .. } => "the saved path map is invalid",
        WorktreeError::PathMissing { .. } => "a mapped root is missing",
        WorktreeError::PathNotWritable { .. } => "a mapped root is not writable",
        WorktreeError::NotADirectory { .. } => "a mapped root is not a directory",
        WorktreeError::PathOutsideRoot { .. }
        | WorktreeError::SymlinkEscape { .. }
        | WorktreeError::CaseOnlyEscape { .. } => "the working directory escapes the project root",
        WorktreeError::OutsideAllow { .. } => "the working directory is outside the project allow list",
        WorktreeError::UnsafeCheckout { .. } => "the project checkout is not private to the agent user",
        WorktreeError::NotAGitCheckout { .. } => "the project root is not a git checkout",
        WorktreeError::InvalidCommit { .. } => "the base commit is invalid",
        WorktreeError::InvalidRemote { .. } => "the git remote name is invalid",
        WorktreeError::BaseCommitMissing { .. } => "the base commit is not available locally",
        WorktreeError::DiskLow { .. } => "free disk is below the node reserve",
        WorktreeError::WorktreeConflict { .. } => "a conflicting worktree exists",
        WorktreeError::RemoteMismatch { .. } => "the local origin does not match the project remote",
        WorktreeError::GitFailed { .. } | WorktreeError::GitPushFailed { .. } => "a local git command failed",
        WorktreeError::Io(_) => "a local filesystem operation failed",
        WorktreeError::Json(_) => "a local worktree record is invalid",
        WorktreeError::Refused { .. } => "the local worktree is not owned by VOWP",
    }
}

/// Returns a path-free and value-free push failure detail.
fn push_detail(error: &WorktreeError) -> String {
    match error {
        WorktreeError::RemoteMismatch { .. } => "the local origin does not match the project remote".to_owned(),
        WorktreeError::GitPushFailed { .. } => "the job branch push failed".to_owned(),
        WorktreeError::InvalidRemote { .. } => "the git remote name is invalid".to_owned(),
        _ => "the job branch could not be pushed".to_owned(),
    }
}

/// Injected values and local paths replaced before an event or summary leaves the Mac.
struct Redactions {
    replacements: Vec<(String, String)>,
}

impl Redactions {
    /// Builds the replacement set for one run.
    fn new(
        environment: &[(String, String)],
        worktree: &Path,
        project_root: &Path,
        project: &ProjectName,
        job: &JobId,
        state_dir: &Path,
    ) -> Self {
        let mut replacements: Vec<(String, String)> = environment
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (value.clone(), format!("[REDACTED:{name}]")))
            .collect();
        replacements.extend([
            (
                worktree.to_string_lossy().into_owned(),
                format!("worktree:{project}/{job}"),
            ),
            (
                project_root.to_string_lossy().into_owned(),
                format!("project:{project}"),
            ),
            (state_dir.to_string_lossy().into_owned(), "<agent-home>".to_owned()),
        ]);
        // Longer matches first so one value that contains another (or a path) cannot leave
        // a tail behind; the worktree is normally below the project root.
        replacements.retain(|(value, _)| !value.is_empty());
        replacements.sort_by_key(|(value, _)| Reverse(value.len()));
        Self { replacements }
    }

    /// Replaces every injected value and known local path in one string.
    fn text(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for (value, marker) in &self.replacements {
            redacted = redacted.replace(value, marker);
        }
        redacted
    }

    /// Visits every string in a JSON value.
    fn json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.text(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.json(item);
                }
            }
            _ => {}
        }
    }
}

/// Returns free bytes on the filesystem holding `path`.
///
/// This is the accept-time preflight only; [`WorktreeManager::create`] repeats the check
/// authoritatively before it touches an existing worktree. A failing `df` is treated as
/// zero free bytes, so the node refuses rather than guesses.
fn disk_free_bytes(path: &Path) -> u64 {
    let Ok(output) = std::process::Command::new("df").arg("-Pk").arg(path).output() else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().nth(1) else {
        return 0;
    };
    line.split_whitespace()
        .nth(3)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .unwrap_or(0)
}
