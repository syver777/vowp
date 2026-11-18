//! The assignment run loop: validation, handler injection, spooled lines and replay.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §6.5, §8.3–§8.4, §9 and
//! `docs/design/crates/vowp-agent.md`. The library owns the spool and every frame; a
//! [`RunHandler`] only accepts an assignment, emits [`RemoteLine`]s through a [`RunSink`]
//! and returns one [`RunSummary`]. A run line is appended to the 0600 spool and fsynced
//! before the client sends it, and a reconnect replays the first unacknowledged line.
//! The lease token from `AssignRun` stays in memory: it is never part of [`AcceptedRun`],
//! never written to the spool and never reported in a status or error.
//!
//! A `bundle` assignment creates one `base..job/<id>` git bundle after the handler returns
//! `Ok` and streams it as `BundleStart`/`BundleChunk`/`BundleEnd`. Chunks are at most 256
//! KiB, a reconnect re-announces the same digest and size so the home answers with its
//! stored offset, and a home `BundleAbort` turns the terminal summary into a
//! `bundle_failed` error instead of a successful one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use vowp_protocol::{
    AgentFrame, AssignmentPayload, Cost, ErrorKind, JobId, LimitEvent, MAX_BUNDLE_CHUNK_BYTES, PathRef, ProjectName,
    RemoteLine, RemoteRunState, RemoteUsage, ResultTransport, RunId, RunOutcome, RunRejectReason, RunResume, SessionId,
    StopReason, Timestamp, TurnSummary,
};

use crate::client::AgentClient;
use crate::error::AgentError;
use crate::spool::{DEFAULT_SPOOL_CAP_BYTES, Spool, SpoolAppend};
use crate::worktree::{JobBundle, SystemGit, create_job_bundle};

/// File written into a run spool when a conflicting event id stops the stream.
pub const QUARANTINE_FILE: &str = "quarantine.json";

/// The default in-flight event window: 64 unacknowledged event lines.
pub const EVENT_WINDOW: u64 = 64;

/// How long the client waits for a run line or acknowledgement before polling again.
pub(crate) const RUN_POLL: Duration = Duration::from_millis(10);

/// Consecutive empty polls after which a missing acknowledgement is a transport loss.
const MAX_ACK_POLLS: u32 = 4;

/// Consecutive empty polls after which a missing `Credit` is a transport loss.
const MAX_CREDIT_POLLS: u32 = 16;

/// One assignment the handler accepted, plus the local metadata it needs.
///
/// The value never carries the run lease token or a secret value. `Debug` prints the
/// local paths as `[local path]` so a diagnostic line cannot leak an absolute path.
#[derive(Clone, PartialEq, Eq)]
pub struct AcceptedRun {
    /// The durable, token-free assignment.
    pub assignment: AssignmentPayload,
    /// Run epoch copied from the assignment.
    pub epoch: u64,
    /// Resolved local working directory.
    pub cwd: PathBuf,
    /// Resolved local worktree path for the job.
    pub worktree: PathBuf,
    /// Resolved local project root.
    pub project_root: PathBuf,
    /// Free-space reserve the handler applied, in bytes.
    pub reserve_bytes: u64,
}

impl fmt::Debug for AcceptedRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptedRun")
            .field("assignment", &self.assignment)
            .field("epoch", &self.epoch)
            .field("cwd", &"[local path]")
            .field("worktree", &"[local path]")
            .field("project_root", &"[local path]")
            .field("reserve_bytes", &self.reserve_bytes)
            .finish()
    }
}

/// A refusal returned by [`RunHandler::accept`], sent to the home as `RunRejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerReject {
    /// Documented refusal class.
    pub reason: RunRejectReason,
    /// Human-readable detail; never a secret value or an absolute local path.
    pub detail: String,
}

impl HandlerReject {
    /// Builds one typed refusal.
    pub fn new(reason: RunRejectReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    /// Builds an `Other` refusal with a human-readable detail.
    pub fn other(detail: impl Into<String>) -> Self {
        Self::new(RunRejectReason::Other, detail)
    }
}

/// What happened to a result push, for the local summary and status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushStatus {
    /// The assignment did not ask for a push.
    NotRequested,
    /// The job branch was pushed.
    Pushed {
        /// Branch that was pushed.
        branch: String,
        /// Remote that received it.
        remote: String,
        /// Head commit that was pushed.
        head_commit: String,
    },
    /// The push failed; the worktree is kept for inspection.
    Failed {
        /// Human-readable failure; never a secret value.
        detail: String,
    },
}

/// The classified result of one run, mirroring [`TurnSummary`] plus result-return data.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    /// Process exit code, when the handler produced one.
    pub exit_code: Option<i32>,
    /// Classified run outcome.
    pub outcome: RunOutcome,
    /// Harness-side session id, when one was reported.
    pub native_session_id: Option<String>,
    /// Cost and token usage reported by the handler.
    pub cost: Cost,
    /// Provider limit that ended the run, when one was classified.
    pub limit: Option<LimitEvent>,
    /// Resource usage measured on the agent, when available.
    pub usage: Option<RemoteUsage>,
    /// Head commit the handler produced, when known.
    pub head_commit: Option<String>,
    /// Result-return status, when the handler pushed a branch.
    pub push_status: Option<PushStatus>,
    /// Final assistant text, when any.
    pub result_text: Option<String>,
    /// Why the run stopped.
    pub stop_reason: StopReason,
}

impl RunSummary {
    /// Converts the run result into the protocol's terminal `TurnSummary`.
    pub fn to_turn_summary(&self) -> TurnSummary {
        TurnSummary {
            exit_code: self.exit_code,
            outcome: self.outcome,
            native_session_id: self.native_session_id.clone(),
            cost: self.cost.clone(),
            limit: self.limit.clone(),
            result_text: self.result_text.clone(),
            stop_reason: self.stop_reason,
            usage: self.usage.clone(),
        }
    }
}

/// Why a [`RunSink`] call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunSinkError {
    /// The run was cancelled, fenced or lost; the handler must stop.
    #[error("the run stopped: {reason}")]
    Stopped {
        /// Human-readable, path-free reason.
        reason: String,
    },
    /// The line would grow the run spool past its cap; nothing was written or sent.
    #[error("the run spool is full: {bytes} bytes would exceed the {cap}-byte cap")]
    SpoolFull {
        /// Size the refused write would have reached.
        bytes: u64,
        /// Configured per-run cap.
        cap: u64,
    },
    /// The sink accepts events only; the library owns the hello and summary.
    #[error("the run sink does not accept {kind} lines")]
    Unsupported {
        /// Name of the refused line kind.
        kind: &'static str,
    },
}

/// The emission point a [`RunHandler`] uses while a run executes.
///
/// Implementations spool and send; a handler never touches the transport, the spool or the
/// lease token. [`RunSink::finish`] blocks until the home acknowledges the terminal
/// `TurnEnd`, the run is stopped, or the summary does not fit in the spool.
pub trait RunSink: Send {
    /// Appends one event line to the spool and queues it for sending.
    ///
    /// Only [`RemoteLine::Event`] and [`RemoteLine::Heartbeat`] are accepted. The hello
    /// (which carries the lease token) and the terminal summary are owned by the library;
    /// use [`RunSink::finish`] for the summary.
    fn line(&mut self, line: RemoteLine) -> Result<(), RunSinkError>;

    /// Reports a non-terminal run state to the home.
    ///
    /// `detail` must be human-readable and must not contain a secret value or an absolute
    /// local path.
    fn state(&mut self, state: RemoteRunState, detail: &str) -> Result<(), RunSinkError>;

    /// Spools the terminal summary and waits for the home's acknowledgement.
    fn finish(&mut self, summary: RunSummary) -> Result<(), RunSinkError>;

    /// Returns whether the run has been stopped by a cancel, fence or spool-full event.
    fn is_stopped(&self) -> bool;

    /// Returns the number of bytes currently in the run spool.
    fn spool_bytes(&self) -> u64;
}

/// The injected executor behind the assignment loop.
///
/// The real harness runner (adapters, worktree creation and push) implements this trait in
/// the executor job. This crate supplies the transport half and tests it with a fake
/// handler. `cancel` and `cleanup` may be called from another thread than `run`.
pub trait RunHandler: Send + Sync {
    /// Validates one assignment and resolves the local metadata the run needs.
    ///
    /// The handler performs the node-specific checks: path map, writable roots, harness
    /// binary, environment and secret names, disk reserve and worktree conflict. A refused
    /// assignment is sent as `RunRejected` and [`RunHandler::run`] is never called.
    fn accept(&self, assignment: &AssignmentPayload) -> Result<AcceptedRun, HandlerReject>;

    /// Runs one accepted assignment until it finishes, fails or is stopped.
    ///
    /// Event ids must be stable across retries and unique within the session; the sink
    /// spools and replays them as given and the home dedupes by id.
    fn run(&self, accepted: AcceptedRun, sink: &mut dyn RunSink);

    /// Stops a running assignment; the spool and worktree are kept.
    fn cancel(&self, run: &RunId);

    /// Releases one run's local state, including its own worktree.
    fn cleanup(&self, run: &RunId);
}

/// A handler used by the Phase 0 `run` API, which has no executor behind it.
struct RejectingHandler;

impl RunHandler for RejectingHandler {
    fn accept(&self, _assignment: &AssignmentPayload) -> Result<AcceptedRun, HandlerReject> {
        Err(HandlerReject::other("this agent has no run handler"))
    }

    fn run(&self, _accepted: AcceptedRun, _sink: &mut dyn RunSink) {}

    fn cancel(&self, _run: &RunId) {}

    fn cleanup(&self, _run: &RunId) {}
}

/// A message from a run thread or the sink to the frame loop.
enum DriverEvent {
    /// A sink write was refused because the run spool reached its cap.
    SpoolFull { run: RunId, bytes: u64, cap: u64 },
    /// The handler appended one event line to the spool.
    Line { run: RunId, frame: Box<AgentFrame> },
    /// The handler spooled its terminal summary and is waiting for the acknowledgement.
    Finished { run: RunId, summary: Box<RunSummary> },
    /// The handler returned; `panicked` is true when it unwound.
    HandlerDone { run: RunId, panicked: bool },
    /// The handler asked to report a non-terminal state.
    State {
        run: RunId,
        state: RemoteRunState,
        detail: String,
    },
    /// The handler produced a liveness line, which is never spooled.
    Heartbeat { run: RunId, at: Timestamp },
}

/// State shared by one run's handler thread and the frame loop.
struct RunShared {
    run: RunId,
    epoch: u64,
    spool: Mutex<Spool>,
    bytes: AtomicU64,
    stopped: AtomicBool,
    finished: AtomicBool,
    online: AtomicBool,
    stop_reason: Mutex<String>,
    summary_acked: AtomicBool,
    finish_reply: Mutex<Option<mpsc::SyncSender<Result<(), RunSinkError>>>>,
}

impl RunShared {
    fn set_stopped(&self, reason: &str) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            *self
                .stop_reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = reason.to_owned();
        }
        let reply = self
            .finish_reply
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(reply) = reply {
            let _ = reply.send(Err(RunSinkError::Stopped {
                reason: reason.to_owned(),
            }));
        }
    }

    fn stop_reason(&self) -> String {
        self.stop_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The sink handed to a handler for one run.
struct Sink {
    shared: Arc<RunShared>,
    events: Sender<DriverEvent>,
}

impl Sink {
    fn stopped(&self) -> RunSinkError {
        RunSinkError::Stopped {
            reason: self.shared.stop_reason(),
        }
    }

    fn append(&mut self, frame: &AgentFrame) -> Result<(), RunSinkError> {
        let mut spool = self
            .shared
            .spool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match spool.append(frame) {
            Ok(SpoolAppend::Written) => {
                self.shared.bytes.store(spool.bytes(), Ordering::SeqCst);
                Ok(())
            }
            Ok(SpoolAppend::Full { bytes, cap }) => {
                drop(spool);
                self.shared.set_stopped("spool_full");
                let _ = self.events.send(DriverEvent::SpoolFull {
                    run: self.shared.run.clone(),
                    bytes,
                    cap,
                });
                Err(RunSinkError::SpoolFull { bytes, cap })
            }
            Err(_) => {
                drop(spool);
                self.shared.set_stopped("spool_io");
                Err(RunSinkError::Stopped {
                    reason: "the run spool could not be written".to_owned(),
                })
            }
        }
    }
}

impl RunSink for Sink {
    fn line(&mut self, line: RemoteLine) -> Result<(), RunSinkError> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Err(self.stopped());
        }
        if self.shared.finished.load(Ordering::SeqCst) {
            return Err(RunSinkError::Stopped {
                reason: "the run already finished".to_owned(),
            });
        }
        match line {
            line @ RemoteLine::Event { .. } => {
                let frame = AgentFrame::RemoteLine {
                    run_id: self.shared.run.clone(),
                    epoch: self.shared.epoch,
                    line,
                };
                self.append(&frame)?;
                // While the connection is down the spool is the queue: sending a copy of
                // every offline line would grow memory with the spool cap. The reconnect
                // replays the durable lines instead.
                if self.shared.online.load(Ordering::SeqCst) {
                    let _ = self.events.send(DriverEvent::Line {
                        run: self.shared.run.clone(),
                        frame: Box::new(frame),
                    });
                }
                Ok(())
            }
            RemoteLine::Heartbeat { at } => {
                if !self.shared.online.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let _ = self.events.send(DriverEvent::Heartbeat {
                    run: self.shared.run.clone(),
                    at,
                });
                Ok(())
            }
            RemoteLine::Hello { .. } => Err(RunSinkError::Unsupported { kind: "hello" }),
            RemoteLine::Summary { .. } => Err(RunSinkError::Unsupported { kind: "summary" }),
        }
    }

    fn state(&mut self, state: RemoteRunState, detail: &str) -> Result<(), RunSinkError> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Err(self.stopped());
        }
        if self.shared.finished.load(Ordering::SeqCst) {
            return Err(RunSinkError::Stopped {
                reason: "the run already finished".to_owned(),
            });
        }
        if !self.shared.online.load(Ordering::SeqCst) {
            // A non-terminal state is liveness information; it is not spooled and a
            // disconnected run has nothing to report until it resumes.
            return Ok(());
        }
        let _ = self.events.send(DriverEvent::State {
            run: self.shared.run.clone(),
            state,
            detail: detail.to_owned(),
        });
        Ok(())
    }

    fn finish(&mut self, summary: RunSummary) -> Result<(), RunSinkError> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Err(self.stopped());
        }
        if self.shared.summary_acked.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.shared.finished.swap(true, Ordering::SeqCst) {
            return Err(RunSinkError::Stopped {
                reason: "the run already finished".to_owned(),
            });
        }
        let (reply, wait) = mpsc::sync_channel(1);
        *self
            .shared
            .finish_reply
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply);
        let frame = AgentFrame::TurnEnd {
            run_id: self.shared.run.clone(),
            summary: summary.to_turn_summary(),
        };
        let appended = self.append(&frame);
        if let Err(error) = appended {
            self.shared
                .finish_reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            return Err(error);
        }
        let _ = self.events.send(DriverEvent::Finished {
            run: self.shared.run.clone(),
            summary: Box::new(summary),
        });
        match wait.recv() {
            Ok(result) => result,
            Err(_) => Err(RunSinkError::Stopped {
                reason: "the agent stopped before the summary was acknowledged".to_owned(),
            }),
        }
    }

    fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst)
    }

    fn spool_bytes(&self) -> u64 {
        self.shared.bytes.load(Ordering::SeqCst)
    }
}

/// One run the frame loop is driving.
struct ActiveRun {
    accepted: AcceptedRun,
    epoch: u64,
    lease: String,
    shared: Arc<RunShared>,
    thread: Option<JoinHandle<()>>,
    last_acked: u64,
    credits: u64,
    summary: Option<RunSummary>,
    pending: VecDeque<AgentFrame>,
    queued_ids: BTreeSet<String>,
    acked_ids: BTreeSet<String>,
    stopped: bool,
    fenced: bool,
    cancel_called: bool,
    bundle: Option<BundleOutgoing>,
}

/// One bundle transfer owned by a finished run.
///
/// The bundle file is created once when the handler returns `Ok`; `BundleAccept` supplies
/// the byte offset the home already stored, so a reconnect resumes instead of restarting.
struct BundleOutgoing {
    bundle: JobBundle,
    start_sent: bool,
    accepted_offset: Option<u64>,
    next_offset: u64,
    end_sent: bool,
}

/// What the outgoing bundle transfer should send next.
enum BundleStep {
    /// Send `BundleStart` and wait for `BundleAccept`.
    Start,
    /// Send this chunk at `offset`.
    Chunk {
        /// Byte offset the chunk starts at.
        offset: u64,
        /// Lowercase hex chunk data.
        data: String,
    },
    /// Every chunk was sent; send `BundleEnd`.
    End,
    /// Wait for the home's answer.
    Wait,
    /// The local bundle cannot be read; fail with this stable detail.
    Failed(String),
}

/// Decides the next bundle frame without touching the client.
fn bundle_step(run: &mut ActiveRun) -> BundleStep {
    let Some(bundle) = run.bundle.as_mut() else {
        return BundleStep::Wait;
    };
    if bundle.end_sent {
        return BundleStep::Wait;
    }
    if bundle.accepted_offset.is_none() {
        if bundle.start_sent {
            return BundleStep::Wait;
        }
        bundle.start_sent = true;
        return BundleStep::Start;
    }
    if bundle.next_offset >= bundle.bundle.bytes {
        bundle.end_sent = true;
        return BundleStep::End;
    }
    let offset = bundle.next_offset;
    match read_bundle_chunk(&bundle.bundle.path, offset) {
        Ok(chunk) if !chunk.is_empty() => {
            bundle.next_offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            BundleStep::Chunk {
                offset,
                data: hex::encode(chunk),
            }
        }
        _ => BundleStep::Failed("bundle_failed: the bundle could not be read".to_owned()),
    }
}

/// Builds the `BundleStart` frame of one run.
fn bundle_start_frame(run: &ActiveRun) -> Option<AgentFrame> {
    let bundle = run.bundle.as_ref()?;
    Some(AgentFrame::BundleStart {
        run_id: run.accepted.assignment.run_id.clone(),
        epoch: run.epoch,
        base: run.accepted.assignment.base_commit.trim().to_ascii_lowercase(),
        head: bundle.bundle.head.clone(),
        branch: bundle.bundle.branch.clone(),
        bytes: bundle.bundle.bytes,
        sha256: bundle.bundle.sha256.clone(),
    })
}

/// Builds the `BundleEnd` frame of one run.
fn bundle_end_frame(run: &ActiveRun) -> Option<AgentFrame> {
    let bundle = run.bundle.as_ref()?;
    Some(AgentFrame::BundleEnd {
        run_id: run.accepted.assignment.run_id.clone(),
        epoch: run.epoch,
        bytes: bundle.bundle.bytes,
        sha256: bundle.bundle.sha256.clone(),
    })
}

/// Fails one bundle transfer, removes its local file and turns the summary into an error.
///
/// A spooled `TurnEnd` from before the reconnect carries the old successful summary; it is
/// dropped so only the failure summary reaches the home.
fn fail_bundle(run: &mut ActiveRun, detail: &str) {
    if let Some(bundle) = run.bundle.take()
        && let Some(parent) = bundle.bundle.path.parent()
    {
        let _ = fs::remove_dir_all(parent);
    }
    run.pending.retain(|frame| !matches!(frame, AgentFrame::TurnEnd { .. }));
    if let Some(summary) = run.summary.as_mut() {
        summary.outcome = RunOutcome::Error;
        summary.stop_reason = StopReason::Error;
        summary.result_text = Some(detail.to_owned());
    }
}

/// Removes a run's local bundle staging directory, when it has one.
fn discard_local_bundle(run: &mut ActiveRun) {
    if let Some(bundle) = run.bundle.take()
        && let Some(parent) = bundle.bundle.path.parent()
    {
        let _ = fs::remove_dir_all(parent);
    }
}

/// Reads at most one chunk of the bundle starting at `offset`.
fn read_bundle_chunk(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0u8; MAX_BUNDLE_CHUNK_BYTES];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    Ok(buffer)
}

/// Keeps only a stable, path-free refusal code from the home.
fn sanitize_bundle_reason(reason: &str) -> String {
    let reason = reason.trim();
    let stable = ["bundle_failed:", "bundle_rejected:", "result_empty:"]
        .iter()
        .find(|prefix| reason.starts_with(**prefix))
        .copied();
    match stable {
        Some(_) if !reason.contains('/') => reason.chars().take(200).collect(),
        _ => "bundle_failed: the home refused the bundle".to_owned(),
    }
}

/// Why a run was stopped locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKind {
    Cancelled,
    Fenced,
    Conflict,
    SpoolFull,
}

/// Drives the run frames for one agent connection loop.
pub(crate) struct RunDriver {
    handler: Arc<dyn RunHandler>,
    runs: BTreeMap<RunId, ActiveRun>,
    jobs: BTreeMap<JobId, RunId>,
    events: Sender<DriverEvent>,
    inbox: Receiver<DriverEvent>,
    reaping: Vec<JoinHandle<()>>,
}

impl RunDriver {
    /// Builds a driver around one injected handler.
    pub(crate) fn new(handler: Arc<dyn RunHandler>) -> Self {
        let (events, inbox) = mpsc::channel();
        Self {
            handler,
            runs: BTreeMap::new(),
            jobs: BTreeMap::new(),
            events,
            inbox,
            reaping: Vec::new(),
        }
    }

    /// Builds the Phase 0 driver, which refuses every assignment instead of running it.
    pub(crate) fn rejecting() -> Self {
        Self::new(Arc::new(RejectingHandler))
    }

    /// Returns whether any run still needs the short poll interval.
    pub(crate) fn has_open_runs(&self) -> bool {
        self.runs
            .values()
            .any(|run| !run.stopped && !run.shared.summary_acked.load(Ordering::SeqCst))
    }

    /// Handles one `AssignRun` frame.
    pub(crate) fn handle_assign(
        &mut self,
        client: &mut AgentClient,
        assignment: AssignmentPayload,
        lease: String,
        _lease_expires_at: Timestamp,
    ) -> Result<(), AgentError> {
        self.reap();
        let previous = self.runs.get(&assignment.run_id).map(|run| (run.epoch, run.stopped));
        if let Some((epoch, stopped)) = previous {
            if epoch == assignment.epoch && !stopped {
                let current = self
                    .runs
                    .get(&assignment.run_id)
                    .map(|run| run.accepted.assignment.assignment_id.clone());
                if current.as_deref() != Some(assignment.assignment_id.as_str()) {
                    return reject(
                        client,
                        &assignment,
                        RunRejectReason::Other,
                        "the assignment id does not match the active run",
                    );
                }
                if let Some(existing) = self.runs.get_mut(&assignment.run_id) {
                    existing.lease = lease;
                }
                return client.send_frame(&AgentFrame::RunAccepted {
                    assignment_id: assignment.assignment_id,
                    run_id: assignment.run_id,
                    epoch: assignment.epoch,
                });
            }
            if epoch > assignment.epoch {
                return reject(
                    client,
                    &assignment,
                    RunRejectReason::Other,
                    format!(
                        "stale assignment epoch {}; run {} is at epoch {}",
                        assignment.epoch, assignment.run_id, epoch
                    ),
                );
            }
            // Equal-but-stopped or a higher epoch supersedes the current dispatch. Stop the
            // handler, keep the spool and start the new epoch; replay only current-epoch lines.
            let superseded = assignment.run_id.clone();
            if !stopped {
                self.stop_run(&superseded, StopKind::Fenced, "superseded by a higher assignment epoch");
            }
            self.runs.remove(&superseded);
        }
        if let Some(other) = self.jobs.get(&assignment.job)
            && *other != assignment.run_id
        {
            return reject(
                client,
                &assignment,
                RunRejectReason::WorktreeConflict,
                "another run already holds this job's worktree",
            );
        }
        if let Some(deadline) = assignment.deadline
            && deadline <= client.clock_now()
        {
            return reject(
                client,
                &assignment,
                RunRejectReason::Other,
                "the assignment deadline has already passed",
            );
        }
        if path_ref_project(&assignment.cwd_ref) != Some(&assignment.project) {
            return reject(
                client,
                &assignment,
                RunRejectReason::Other,
                "the working-directory ref names another project",
            );
        }

        let accepted = match self.handler.accept(&assignment) {
            Ok(accepted) => accepted,
            Err(rejected) => {
                return reject(client, &assignment, rejected.reason, rejected.detail);
            }
        };
        if accepted.epoch != assignment.epoch {
            return reject(
                client,
                &assignment,
                RunRejectReason::Other,
                format!(
                    "the handler accepted epoch {} for assignment epoch {}",
                    accepted.epoch, assignment.epoch
                ),
            );
        }

        let cap = if assignment.spool_cap_bytes == 0 {
            DEFAULT_SPOOL_CAP_BYTES
        } else {
            assignment.spool_cap_bytes
        };
        let shared = match open_shared(
            client.state_dir(),
            &assignment.session_id,
            &assignment.run_id,
            assignment.epoch,
            cap,
        ) {
            Ok(shared) => shared,
            Err(_) => {
                return reject(
                    client,
                    &assignment,
                    RunRejectReason::Other,
                    "the local run spool could not be opened",
                );
            }
        };

        let run = assignment.run_id.clone();
        let thread_run = run.clone();
        let thread_shared = Arc::clone(&shared);
        let thread_events = self.events.clone();
        let handler = Arc::clone(&self.handler);
        let thread_accepted = accepted.clone();
        let thread = std::thread::Builder::new()
            .name(format!("vowp-run-{run}"))
            .spawn(move || {
                let mut sink = Sink {
                    shared: thread_shared,
                    events: thread_events.clone(),
                };
                let panicked = panic::catch_unwind(AssertUnwindSafe(|| {
                    handler.run(thread_accepted, &mut sink);
                }))
                .is_err();
                let _ = thread_events.send(DriverEvent::HandlerDone {
                    run: thread_run,
                    panicked,
                });
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => {
                return reject(
                    client,
                    &assignment,
                    RunRejectReason::Other,
                    "the local run handler could not start",
                );
            }
        };

        let detail = "started".to_owned();
        self.runs.insert(
            run.clone(),
            ActiveRun {
                accepted,
                epoch: assignment.epoch,
                lease,
                shared: Arc::clone(&shared),
                thread: Some(thread),
                last_acked: 0,
                credits: EVENT_WINDOW,
                summary: None,
                pending: VecDeque::new(),
                queued_ids: BTreeSet::new(),
                acked_ids: BTreeSet::new(),
                stopped: false,
                fenced: false,
                cancel_called: false,
                bundle: None,
            },
        );
        self.jobs.insert(assignment.job.clone(), run.clone());

        client.send_frame(&AgentFrame::RunAccepted {
            assignment_id: assignment.assignment_id.clone(),
            run_id: run.clone(),
            epoch: assignment.epoch,
        })?;
        client.send_frame(&run_state(
            &shared,
            assignment.epoch,
            RemoteRunState::Running,
            detail,
            None,
        ))?;
        self.send_hello(client, &run)?;
        Ok(())
    }

    /// Handles one `CancelRun` frame; a stale epoch is ignored.
    pub(crate) fn handle_cancel(&mut self, client: &mut AgentClient, run_id: &RunId, epoch: u64, reason: &str) {
        let Some(run) = self.runs.get(run_id) else {
            return;
        };
        if run.epoch != epoch || run.stopped {
            return;
        }
        self.stop_run(run_id, StopKind::Cancelled, "cancelled by the home");
        let detail = format!("cancelled: {reason}");
        if let Some(run) = self.runs.get(run_id) {
            let frame = run_state(&run.shared, run.epoch, RemoteRunState::Finishing, detail, None);
            let _ = client.send_frame(&frame);
        }
    }

    /// Handles one `RunLost` frame: fence the handler and keep the spool.
    pub(crate) fn handle_run_lost(&mut self, _client: &mut AgentClient, run_id: &RunId, reason: &str) {
        let detail = if reason.is_empty() {
            "run lost"
        } else {
            "run fenced by the home"
        };
        self.stop_run(run_id, StopKind::Fenced, detail);
        if let Some(run) = self.runs.get_mut(run_id) {
            run.fenced = true;
        }
    }

    /// Handles one `Cleanup` frame: release the run state and let the handler remove its worktree.
    pub(crate) fn handle_cleanup(&mut self, _client: &mut AgentClient, run_id: &RunId, job: &JobId) {
        if let Some(run) = self.runs.get(run_id) {
            if !run.stopped {
                self.stop_run(run_id, StopKind::Cancelled, "cleanup requested by the home");
            }
        }
        if let Some(mut run) = self.runs.remove(run_id) {
            discard_local_bundle(&mut run);
            if let Some(thread) = run.thread.take() {
                self.reaping.push(thread);
            }
            if self.jobs.get(job) == Some(run_id) {
                self.jobs.remove(job);
            }
        }
        self.handler.cleanup(run_id);
    }

    /// Adds one `Credit` grant to a run's window.
    pub(crate) fn handle_credit(&mut self, run_id: &RunId, count: u64) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.credits = run.credits.saturating_add(count);
        }
    }

    /// Accepts the home's resume offset for one bundle transfer.
    pub(crate) fn handle_bundle_accept(&mut self, run_id: &RunId, epoch: u64, offset: u64) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        if run.epoch != epoch {
            return;
        }
        let Some(bundle) = run.bundle.as_ref() else {
            return;
        };
        if offset > bundle.bundle.bytes {
            fail_bundle(run, "bundle_failed: the home asked for an offset past the bundle");
            return;
        }
        let Some(bundle) = run.bundle.as_mut() else {
            return;
        };
        bundle.accepted_offset = Some(offset);
        bundle.next_offset = offset;
    }

    /// Stops one bundle transfer after a home refusal.
    pub(crate) fn handle_bundle_abort(&mut self, run_id: &RunId, epoch: u64, reason: &str) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        if run.epoch != epoch || run.bundle.is_none() {
            return;
        }
        fail_bundle(run, &sanitize_bundle_reason(reason));
    }

    /// Applies an acknowledgement that arrived outside a send wait.
    pub(crate) fn handle_ack(
        &mut self,
        run_id: &RunId,
        event_id: Option<&vowp_protocol::EventId>,
        seq: Option<u64>,
        duplicate: bool,
        lease_revoked: Option<String>,
        error: Option<String>,
    ) {
        if let Some(reason) = lease_revoked {
            self.stop_run(run_id, StopKind::Fenced, &format!("lease revoked: {reason}"));
            if let Some(run) = self.runs.get_mut(run_id) {
                run.fenced = true;
            }
            return;
        }
        if let Some(message) = error {
            self.quarantine(run_id, &message);
            self.stop_run(run_id, StopKind::Conflict, "conflicting event id");
            return;
        }
        self.mark_ack(run_id, event_id, seq, duplicate);
    }

    /// Re-registers the runs with the home after a reconnect and replays their unacked lines.
    pub(crate) fn resume(&mut self, client: &mut AgentClient) -> Result<(), AgentError> {
        let resumes: Vec<RunResume> = self
            .runs
            .values()
            .filter(|run| !run.stopped && !run.fenced && !run.shared.summary_acked.load(Ordering::SeqCst))
            .map(|run| RunResume {
                run_id: run.accepted.assignment.run_id.clone(),
                epoch: run.epoch,
                last_acked: run.last_acked,
            })
            .collect();
        if resumes.is_empty() {
            return Ok(());
        }
        client.send_frame(&AgentFrame::Resume {
            runs: resumes,
            surfaces: Vec::new(),
        })?;
        let mut empty = 0;
        let results = loop {
            match client.recv_frame(Some(RUN_POLL))? {
                crate::transport::FrameRead::Frame(AgentFrame::ResumeOk { runs, .. }) => break runs,
                crate::transport::FrameRead::Frame(frame) => self.handle_control(client, frame)?,
                crate::transport::FrameRead::Timeout => {
                    empty += 1;
                    if empty >= MAX_CREDIT_POLLS {
                        return Err(AgentError::Transport("the home did not answer Resume".to_owned()));
                    }
                }
                crate::transport::FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the home closed the connection during Resume".to_owned(),
                    ));
                }
            }
        };
        for result in &results {
            if !result.accepted {
                let reason = result.reason.as_deref().unwrap_or("the run is not current");
                self.stop_run(&result.run_id, StopKind::Fenced, reason);
                if let Some(run) = self.runs.get_mut(&result.run_id) {
                    run.fenced = true;
                }
            }
        }
        // The home dispatches a fresh `AssignRun` (with a fresh lease token) right after
        // `ResumeOk`. Read what is already queued so the hello uses the current token.
        for _ in 0..8 {
            match client.recv_frame(Some(Duration::from_millis(2)))? {
                crate::transport::FrameRead::Frame(frame) => self.handle_control(client, frame)?,
                crate::transport::FrameRead::Timeout | crate::transport::FrameRead::Closed => break,
            }
        }
        let accepted: Vec<RunId> = results
            .iter()
            .filter(|result| result.accepted)
            .map(|result| result.run_id.clone())
            .collect();
        for run_id in &accepted {
            let Some(run) = self.runs.get_mut(run_id) else {
                continue;
            };
            if run.stopped {
                continue;
            }
            // Mark the run online before taking the spool snapshot. An event appended
            // after this store is also sent on the driver channel; an event appended
            // before it is in the snapshot. The overlap (an append that raced the store)
            // is skipped by event id when the same line was already queued.
            run.shared.online.store(true, Ordering::SeqCst);
            let frames = read_unacked(&run.shared)?;
            run.pending.clear();
            run.queued_ids.clear();
            for frame in frames {
                let keep = match &frame {
                    AgentFrame::RemoteLine {
                        epoch,
                        line: RemoteLine::Event { event },
                        ..
                    } => *epoch == run.epoch && !run.acked_ids.contains(event.id.as_str()),
                    AgentFrame::TurnEnd { .. } => true,
                    _ => false,
                };
                if keep {
                    if let Some(id) = event_id(&frame) {
                        run.queued_ids.insert(id.as_str().to_owned());
                    }
                    run.pending.push_back(frame);
                }
            }
            // The home opens a fresh ingest stream on this connection, so the old window
            // credit is meaningless. Start every resumable run with a full window.
            run.credits = EVENT_WINDOW;
            // A bundle transfer is resumed by re-announcing the same digest and size; the
            // home answers `BundleAccept` with its stored offset and the chunks continue
            // from there. The local file is still the source of truth.
            if let Some(bundle) = run.bundle.as_mut() {
                bundle.start_sent = false;
                bundle.accepted_offset = None;
                bundle.end_sent = false;
            }
        }
        for run_id in accepted {
            self.send_hello(client, &run_id)?;
        }
        Ok(())
    }

    /// Processes queued handler events and sends at most one spooled line per run.
    pub(crate) fn pump(&mut self, client: &mut AgentClient) -> Result<(), AgentError> {
        self.process_events(client)?;
        let runs: Vec<RunId> = self.runs.keys().cloned().collect();
        for run in runs {
            self.send_next(client, &run)?;
        }
        self.reap();
        Ok(())
    }

    /// Best-effort `RunState(disconnected)` for every open run before a reconnect.
    pub(crate) fn note_disconnect(&mut self, client: &mut AgentClient) {
        for run in self.runs.values().filter(|run| !run.stopped) {
            run.shared.online.store(false, Ordering::SeqCst);
        }
        for run in self.runs.values().filter(|run| !run.stopped) {
            let frame = run_state(
                &run.shared,
                run.epoch,
                RemoteRunState::Disconnected,
                "the connection dropped; the run is spooling locally",
                run.summary.as_ref().and_then(|summary| summary.head_commit.clone()),
            );
            if client.send_frame(&frame).is_err() {
                break;
            }
        }
    }

    fn process_events(&mut self, client: &mut AgentClient) -> Result<(), AgentError> {
        loop {
            match self.inbox.try_recv() {
                Ok(DriverEvent::SpoolFull { run, bytes, cap }) => {
                    if let Some(active) = self.runs.get_mut(&run)
                        && !active.stopped
                    {
                        let message =
                            format!("spool_full: run {run} reached its {cap}-byte spool cap at {bytes} bytes");
                        client.send_frame(&AgentFrame::Error {
                            kind: ErrorKind::Guard,
                            message,
                            retryable: false,
                        })?;
                        let frame = run_state(
                            &active.shared,
                            active.epoch,
                            RemoteRunState::Disconnected,
                            "spool_full",
                            None,
                        );
                        client.send_frame(&frame)?;
                        active.stopped = true;
                        active.shared.set_stopped("spool_full");
                        self.cancel_handler(&run);
                    }
                }
                Ok(DriverEvent::Line { run, frame }) => {
                    if let Some(active) = self.runs.get_mut(&run) {
                        if active.stopped {
                            continue;
                        }
                        if let AgentFrame::RemoteLine { epoch, .. } = &*frame
                            && *epoch != active.epoch
                        {
                            // A superseded handler can append one last line before it sees
                            // the stop; it stays in the spool but is never sent under the new
                            // epoch.
                            continue;
                        }
                        if let Some(id) = event_id(&frame) {
                            if active.acked_ids.contains(id.as_str()) || active.queued_ids.contains(id.as_str()) {
                                continue;
                            }
                            active.queued_ids.insert(id.as_str().to_owned());
                        }
                        active.pending.push_back(*frame);
                    }
                }
                Ok(DriverEvent::Finished { run, summary }) => {
                    let mut summary = *summary;
                    let bundle_transport = self
                        .runs
                        .get(&run)
                        .is_some_and(|active| active.accepted.assignment.result_transport == ResultTransport::Bundle);
                    if bundle_transport
                        && summary.outcome == RunOutcome::Ok
                        && summary
                            .head_commit
                            .as_deref()
                            .is_some_and(|head| head.eq_ignore_ascii_case(&self.base_of(&run)))
                    {
                        // No commit: a bundle would be empty, and the home classifies the
                        // run as `result_empty`.
                        summary.outcome = RunOutcome::Error;
                        summary.stop_reason = StopReason::Error;
                        summary.result_text = Some("result_empty: the run produced no commit".to_owned());
                    } else if bundle_transport && summary.outcome == RunOutcome::Ok {
                        match self.create_outgoing_bundle(client, &run) {
                            Ok(bundle) => {
                                summary.head_commit = Some(bundle.head.clone());
                                if let Some(active) = self.runs.get_mut(&run) {
                                    active.bundle = Some(BundleOutgoing {
                                        next_offset: 0,
                                        bundle,
                                        start_sent: false,
                                        accepted_offset: None,
                                        end_sent: false,
                                    });
                                }
                            }
                            Err(detail) => {
                                summary.outcome = RunOutcome::Error;
                                summary.stop_reason = StopReason::Error;
                                summary.result_text = Some(detail);
                            }
                        }
                    }
                    let head = summary.head_commit.clone();
                    if let Some(active) = self.runs.get_mut(&run) {
                        active.summary = Some(summary);
                        let frame = run_state(
                            &active.shared,
                            active.epoch,
                            RemoteRunState::Finishing,
                            "finishing",
                            head,
                        );
                        client.send_frame(&frame)?;
                    }
                }
                Ok(DriverEvent::State { run, state, detail }) => {
                    if let Some(active) = self.runs.get(&run) {
                        let frame = run_state(&active.shared, active.epoch, state, detail, None);
                        client.send_frame(&frame)?;
                    }
                }
                Ok(DriverEvent::HandlerDone { run, panicked }) => {
                    self.handler_done(client, &run, panicked)?;
                }
                Ok(DriverEvent::Heartbeat { run, at }) => {
                    if let Some(active) = self.runs.get(&run)
                        && !active.stopped
                    {
                        client.send_frame(&AgentFrame::RemoteLine {
                            run_id: run.clone(),
                            epoch: active.epoch,
                            line: RemoteLine::Heartbeat { at },
                        })?;
                    }
                }
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
        Ok(())
    }

    fn handler_done(&mut self, client: &mut AgentClient, run_id: &RunId, panicked: bool) -> Result<(), AgentError> {
        let Some(active) = self.runs.get(run_id) else {
            return Ok(());
        };
        if active.stopped || active.summary.is_some() {
            return Ok(());
        }
        let detail = if panicked {
            "the run handler stopped unexpectedly"
        } else {
            "the run handler returned without a summary"
        };
        let state = run_state(&active.shared, active.epoch, RemoteRunState::Finishing, detail, None);
        client.send_frame(&state)?;
        let summary = RunSummary {
            exit_code: None,
            outcome: if panicked { RunOutcome::Crash } else { RunOutcome::Error },
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            usage: None,
            head_commit: None,
            push_status: None,
            result_text: None,
            stop_reason: if panicked { StopReason::Crash } else { StopReason::Error },
        };
        let frame = AgentFrame::TurnEnd {
            run_id: run_id.clone(),
            summary: summary.to_turn_summary(),
        };
        let appended = {
            let mut spool = active
                .shared
                .spool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let appended = spool.append(&frame);
            if matches!(appended, Ok(SpoolAppend::Written)) {
                active.shared.bytes.store(spool.bytes(), Ordering::SeqCst);
            }
            appended
        };
        if matches!(appended, Ok(SpoolAppend::Written)) {
            if let Some(active) = self.runs.get_mut(run_id) {
                active.summary = Some(summary);
            }
        } else {
            self.stop_run(run_id, StopKind::SpoolFull, "spool_full");
        }
        Ok(())
    }

    fn send_next(&mut self, client: &mut AgentClient, run_id: &RunId) -> Result<(), AgentError> {
        let next_is_event = matches!(
            self.runs.get(run_id).and_then(|run| run.pending.front()),
            Some(AgentFrame::RemoteLine {
                line: RemoteLine::Event { .. },
                ..
            })
        );
        if next_is_event && !self.take_credit(client, run_id)? {
            return Ok(());
        }
        let Some(run) = self.runs.get_mut(run_id) else {
            return Ok(());
        };
        if run.stopped {
            return Ok(());
        }
        // A spooled TurnEnd after a reconnect must wait for the bundle: the summary may
        // claim success, and the home settles it against a complete bundle. Events may
        // still flow before the transfer.
        let bundle_pending = run.bundle.as_ref().is_some_and(|bundle| !bundle.end_sent);
        let mut frame = match run.pending.front() {
            Some(AgentFrame::TurnEnd { .. }) if bundle_pending => None,
            _ => run.pending.pop_front(),
        };
        if frame.is_none() {
            match bundle_step(run) {
                BundleStep::Start => frame = bundle_start_frame(run),
                BundleStep::Chunk { offset, data } => {
                    frame = Some(AgentFrame::BundleChunk {
                        run_id: run_id.clone(),
                        epoch: run.epoch,
                        offset,
                        data,
                    });
                }
                BundleStep::End => frame = bundle_end_frame(run),
                BundleStep::Failed(detail) => fail_bundle(run, &detail),
                BundleStep::Wait => {}
            }
        }
        // The summary may only follow a finished bundle. While `BundleStart` waits for its
        // `BundleAccept`, or chunks are still flowing, the terminal frame stays pending.
        let bundle_pending = run.bundle.as_ref().is_some_and(|bundle| !bundle.end_sent);
        if frame.is_none()
            && !bundle_pending
            && let Some(summary) = &run.summary
        {
            frame = Some(AgentFrame::TurnEnd {
                run_id: run_id.clone(),
                summary: summary.to_turn_summary(),
            });
        }
        let Some(frame) = frame else {
            return Ok(());
        };
        let needs_ack = matches!(
            &frame,
            AgentFrame::RemoteLine {
                line: RemoteLine::Event { .. },
                ..
            } | AgentFrame::TurnEnd { .. }
        );
        client.send_frame(&frame)?;
        if needs_ack
            && let AckOutcome::Acked = self.wait_for_ack(client, run_id, &frame)?
            && let Some(id) = event_id(&frame)
        {
            self.absorb_ack(run_id, &id);
        }
        Ok(())
    }

    /// Creates the outgoing bundle for one finished run.
    fn create_outgoing_bundle(&self, client: &AgentClient, run_id: &RunId) -> Result<JobBundle, String> {
        let Some(active) = self.runs.get(run_id) else {
            return Err("bundle_failed: the run is not active".to_owned());
        };
        let base = active.accepted.assignment.base_commit.trim().to_owned();
        let dir = client.state_dir().join("run-bundles").join(run_id.as_str());
        create_job_bundle(
            &SystemGit::new(),
            &active.accepted.worktree,
            &active.accepted.assignment.job,
            &base,
            &dir,
        )
        .map_err(|_| "bundle_failed: the job branch could not be bundled".to_owned())
    }

    /// Returns the lowercased base commit of one active run.
    fn base_of(&self, run_id: &RunId) -> String {
        self.runs
            .get(run_id)
            .map(|active| active.accepted.assignment.base_commit.trim().to_ascii_lowercase())
            .unwrap_or_default()
    }

    fn take_credit(&mut self, client: &mut AgentClient, run_id: &RunId) -> Result<bool, AgentError> {
        let mut empty = 0;
        loop {
            let Some(run) = self.runs.get_mut(run_id) else {
                return Ok(false);
            };
            if run.stopped {
                return Ok(false);
            }
            if run.credits > 0 {
                run.credits -= 1;
                return Ok(true);
            }
            match client.recv_frame(Some(RUN_POLL))? {
                crate::transport::FrameRead::Frame(frame) => {
                    self.handle_control(client, frame)?;
                    if self.runs.get(run_id).is_none_or(|run| run.stopped) {
                        return Ok(false);
                    }
                }
                crate::transport::FrameRead::Timeout => {
                    empty += 1;
                    if empty >= MAX_CREDIT_POLLS {
                        return Err(AgentError::Transport(
                            "the home did not replenish the run's event window".to_owned(),
                        ));
                    }
                }
                crate::transport::FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the connection closed while waiting for Credit".to_owned(),
                    ));
                }
            }
        }
    }

    fn wait_for_ack(
        &mut self,
        client: &mut AgentClient,
        run_id: &RunId,
        frame: &AgentFrame,
    ) -> Result<AckOutcome, AgentError> {
        let expected = event_id(frame);
        let is_summary = matches!(frame, AgentFrame::TurnEnd { .. });
        let mut empty = 0;
        loop {
            match client.recv_frame(Some(RUN_POLL))? {
                crate::transport::FrameRead::Frame(AgentFrame::IngestAck {
                    run_id: acknowledged,
                    event_id,
                    seq,
                    duplicate,
                    lease_revoked,
                    error,
                }) => {
                    if acknowledged != *run_id {
                        self.handle_ack(&acknowledged, event_id.as_ref(), seq, duplicate, lease_revoked, error);
                        continue;
                    }
                    if let Some(reason) = lease_revoked {
                        self.stop_run(run_id, StopKind::Fenced, &format!("lease revoked: {reason}"));
                        self.report_stopped(client, run_id, "lease_revoked");
                        return Ok(AckOutcome::Stopped);
                    }
                    if let Some(message) = error {
                        self.quarantine(run_id, &message);
                        self.stop_run(run_id, StopKind::Conflict, "conflicting event id");
                        self.report_stopped(client, run_id, "conflict");
                        return Ok(AckOutcome::Stopped);
                    }
                    match (event_id, seq) {
                        (Some(id), Some(seq)) => {
                            let Some(shared) = self.runs.get(run_id).map(|run| Arc::clone(&run.shared)) else {
                                return Ok(AckOutcome::Stopped);
                            };
                            if mark_acked(&shared, id.as_str(), seq).is_err() {
                                return Err(AgentError::Transport(
                                    "the run spool acknowledgement could not be recorded".to_owned(),
                                ));
                            }
                            self.absorb_ack(run_id, &id);
                            self.set_last_acked(run_id, seq);
                            if expected.as_ref().is_some_and(|expected| *expected == id) {
                                return Ok(AckOutcome::Acked);
                            }
                        }
                        (None, seq) => {
                            let Some(shared) = self.runs.get(run_id).map(|run| Arc::clone(&run.shared)) else {
                                return Ok(AckOutcome::Stopped);
                            };
                            let marked = mark_summary_acked(&shared);
                            if marked.is_err() {
                                return Err(AgentError::Transport(
                                    "the run spool acknowledgement could not be recorded".to_owned(),
                                ));
                            }
                            self.set_last_acked(run_id, seq.unwrap_or_default());
                            self.summary_acked(run_id);
                            if is_summary {
                                return Ok(AckOutcome::Summary);
                            }
                        }
                        _ => {}
                    }
                }
                crate::transport::FrameRead::Frame(AgentFrame::Error {
                    kind: ErrorKind::Conflict,
                    retryable: true,
                    ..
                }) => {
                    // The home refused the event at its flow-control window; a reconnect
                    // starts a fresh stream and the spool replays in order.
                    return Err(AgentError::Transport(
                        "the home's event window is exhausted; reconnecting".to_owned(),
                    ));
                }
                crate::transport::FrameRead::Frame(AgentFrame::Error {
                    kind,
                    message,
                    retryable,
                }) => {
                    if matches!(kind, ErrorKind::Refused | ErrorKind::Conflict) {
                        self.quarantine(run_id, &message);
                        self.stop_run(run_id, StopKind::Conflict, "the home refused the run line");
                        self.report_stopped(client, run_id, "conflict");
                        return Ok(AckOutcome::Stopped);
                    }
                    return Err(remote_error(kind, message, retryable));
                }
                crate::transport::FrameRead::Frame(frame) => {
                    self.handle_control(client, frame)?;
                    if self.runs.get(run_id).is_none_or(|run| run.stopped) {
                        return Ok(AckOutcome::Stopped);
                    }
                }
                crate::transport::FrameRead::Timeout => {
                    empty += 1;
                    if empty >= MAX_ACK_POLLS {
                        return Err(AgentError::Transport(
                            "the home did not acknowledge the run line".to_owned(),
                        ));
                    }
                }
                crate::transport::FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the connection closed while waiting for a run acknowledgement".to_owned(),
                    ));
                }
            }
        }
    }

    /// Records an event acknowledgement in the local replay bookkeeping.
    fn absorb_ack(&mut self, run_id: &RunId, id: &vowp_protocol::EventId) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.queued_ids.remove(id.as_str());
            run.acked_ids.insert(id.as_str().to_owned());
            run.pending.retain(|frame| event_id(frame).as_ref() != Some(id));
        }
    }

    fn handle_control(&mut self, client: &mut AgentClient, frame: AgentFrame) -> Result<(), AgentError> {
        match frame {
            AgentFrame::Ping => client.send_frame(&AgentFrame::Pong),
            AgentFrame::AssignRun {
                assignment,
                lease_token,
                lease_expires_at,
            } => self.handle_assign(client, assignment, lease_token, lease_expires_at),
            AgentFrame::CancelRun { run_id, epoch, reason } => {
                self.handle_cancel(client, &run_id, epoch, &reason);
                Ok(())
            }
            AgentFrame::RunLost { run_id, reason } => {
                self.handle_run_lost(client, &run_id, &reason);
                Ok(())
            }
            AgentFrame::Cleanup { run_id, job, .. } => {
                self.handle_cleanup(client, &run_id, &job);
                Ok(())
            }
            AgentFrame::Credit { run_id, count } => {
                self.handle_credit(&run_id, count);
                Ok(())
            }
            AgentFrame::BundleAccept { run_id, epoch, offset } => {
                self.handle_bundle_accept(&run_id, epoch, offset);
                Ok(())
            }
            AgentFrame::BundleAbort { run_id, epoch, reason } => {
                self.handle_bundle_abort(&run_id, epoch, &reason);
                Ok(())
            }
            AgentFrame::IngestAck {
                run_id,
                event_id,
                seq,
                duplicate,
                lease_revoked,
                error,
            } => {
                self.handle_ack(&run_id, event_id.as_ref(), seq, duplicate, lease_revoked, error);
                Ok(())
            }
            AgentFrame::Revoked { reason } => Err(AgentError::Revoked(reason)),
            AgentFrame::Superseded { connection_id } => Err(AgentError::Superseded(connection_id)),
            AgentFrame::Error {
                kind,
                message,
                retryable,
            } => Err(remote_error(kind, message, retryable)),
            AgentFrame::ResumeOk { .. } => Ok(()),
            _ => Ok(()),
        }
    }

    fn mark_ack(
        &mut self,
        run_id: &RunId,
        event_id: Option<&vowp_protocol::EventId>,
        seq: Option<u64>,
        _duplicate: bool,
    ) {
        let Some(shared) = self.runs.get(run_id).map(|run| Arc::clone(&run.shared)) else {
            return;
        };
        match (event_id, seq) {
            (Some(id), Some(seq)) => {
                if mark_acked(&shared, id.as_str(), seq).is_ok() {
                    self.absorb_ack(run_id, id);
                    self.set_last_acked(run_id, seq);
                }
            }
            (None, seq) => {
                let marked = mark_summary_acked(&shared);
                if marked.is_err() {
                    return;
                }
                self.set_last_acked(run_id, seq.unwrap_or_default());
                self.summary_acked(run_id);
            }
            _ => {}
        }
    }

    fn set_last_acked(&mut self, run_id: &RunId, seq: u64) {
        if let Some(run) = self.runs.get_mut(run_id) {
            run.last_acked = run.last_acked.max(seq);
        }
    }

    fn summary_acked(&mut self, run_id: &RunId) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        let _ = run.shared.summary_acked.swap(true, Ordering::SeqCst);
        discard_local_bundle(run);
        let reply = run
            .shared
            .finish_reply
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(reply) = reply {
            let _ = reply.send(Ok(()));
        }
        if let Some(mut run) = self.runs.remove(run_id) {
            if let Some(thread) = run.thread.take() {
                self.reaping.push(thread);
            }
            if self.jobs.get(&run.accepted.assignment.job) == Some(run_id) {
                self.jobs.remove(&run.accepted.assignment.job);
            }
        }
    }

    fn stop_run(&mut self, run_id: &RunId, kind: StopKind, reason: &str) {
        let Some(run) = self.runs.get_mut(run_id) else {
            return;
        };
        if run.stopped {
            return;
        }
        run.stopped = true;
        match kind {
            StopKind::Cancelled => {}
            StopKind::Fenced | StopKind::Conflict | StopKind::SpoolFull => run.fenced = true,
        }
        run.shared.set_stopped(reason);
        discard_local_bundle(run);
        let cancel = !run.cancel_called;
        run.cancel_called = true;
        if cancel {
            self.handler.cancel(run_id);
        }
    }

    fn report_stopped(&self, client: &mut AgentClient, run_id: &RunId, detail: &str) {
        if let Some(run) = self.runs.get(run_id) {
            let frame = run_state(
                &run.shared,
                run.epoch,
                RemoteRunState::Finishing,
                detail.to_owned(),
                None,
            );
            let _ = client.send_frame(&frame);
        }
    }

    fn cancel_handler(&mut self, run_id: &RunId) {
        let cancel = self
            .runs
            .get_mut(run_id)
            .map(|run| {
                let first = !run.cancel_called;
                run.cancel_called = true;
                first
            })
            .unwrap_or(false);
        if cancel {
            self.handler.cancel(run_id);
        }
    }

    fn quarantine(&self, run_id: &RunId, message: &str) {
        let Some(run) = self.runs.get(run_id) else {
            return;
        };
        let Ok(spool) = run.shared.spool.lock() else {
            return;
        };
        let dir = spool.dir().to_path_buf();
        drop(spool);
        let body = serde_json::json!({
            "run_id": run.accepted.assignment.run_id.as_str(),
            "epoch": run.epoch,
            "reason": message,
        });
        let path = dir.join(QUARANTINE_FILE);
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            if serde_json::to_vec_pretty(&body)
                .ok()
                .is_some_and(|bytes| file.write_all(&bytes).and_then(|()| file.sync_all()).is_ok())
            {
                let _ = fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    fn send_hello(&self, client: &mut AgentClient, run_id: &RunId) -> Result<(), AgentError> {
        let Some(run) = self.runs.get(run_id) else {
            return Ok(());
        };
        if run.stopped || run.fenced {
            return Ok(());
        }
        let session = run.accepted.assignment.session_id.clone();
        client.send_frame(&AgentFrame::RemoteLine {
            run_id: run_id.clone(),
            epoch: run.epoch,
            line: RemoteLine::Hello {
                session_id: session.clone(),
                run_id: run_id.clone(),
                executor: run.accepted.assignment.executor.to_string(),
                node: client.node(),
                spool_id: format!("{session}-{run_id}"),
                lease: run.lease.clone(),
            },
        })
    }

    fn reap(&mut self) {
        let mut reaping = Vec::new();
        for handle in self.reaping.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                reaping.push(handle);
            }
        }
        self.reaping = reaping;
    }
}

impl Drop for RunDriver {
    fn drop(&mut self) {
        let runs: Vec<RunId> = self.runs.keys().cloned().collect();
        for run in runs {
            if let Some(active) = self.runs.get(&run)
                && !active.stopped
            {
                active.shared.set_stopped("the agent stopped");
                self.handler.cancel(&run);
            }
        }
    }
}

/// The result of waiting for one run line's acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckOutcome {
    /// The home appended the event line.
    Acked,
    /// The home appended the terminal summary; the run state was released.
    Summary,
    /// The run was stopped (cancel, fence, conflict or cleanup) before its ack.
    Stopped,
}

/// Opens one run's spool and its shared state.
fn open_shared(
    state_dir: &Path,
    session: &SessionId,
    run: &RunId,
    epoch: u64,
    cap: u64,
) -> Result<Arc<RunShared>, AgentError> {
    let spool = Spool::with_cap(state_dir, session, run, cap)?;
    let bytes = spool.bytes();
    Ok(Arc::new(RunShared {
        run: run.clone(),
        epoch,
        spool: Mutex::new(spool),
        bytes: AtomicU64::new(bytes),
        stopped: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        online: AtomicBool::new(true),
        stop_reason: Mutex::new(String::new()),
        summary_acked: AtomicBool::new(false),
        finish_reply: Mutex::new(None),
    }))
}

fn run_state(
    shared: &RunShared,
    epoch: u64,
    state: RemoteRunState,
    detail: impl Into<String>,
    head_commit: Option<String>,
) -> AgentFrame {
    AgentFrame::RunState {
        run_id: shared.run.clone(),
        epoch,
        state,
        spool_bytes: shared.bytes.load(Ordering::SeqCst),
        head_commit,
        detail: detail.into(),
    }
}

fn reject(
    client: &mut AgentClient,
    assignment: &AssignmentPayload,
    reason: RunRejectReason,
    detail: impl Into<String>,
) -> Result<(), AgentError> {
    client.send_frame(&AgentFrame::RunRejected {
        assignment_id: assignment.assignment_id.clone(),
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        reason,
        detail: detail.into(),
    })
}

fn mark_acked(shared: &RunShared, event_id: &str, seq: u64) -> Result<(), AgentError> {
    let mut spool = shared.spool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    spool.mark_acked(event_id, seq)
}

/// Reads the unacknowledged frames without exposing a local path in the error.
fn read_unacked(shared: &RunShared) -> Result<Vec<AgentFrame>, AgentError> {
    let spool = shared.spool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    spool
        .unacked()
        .map_err(|_| AgentError::Transport("the run spool could not be read".to_owned()))
}

fn mark_summary_acked(shared: &RunShared) -> Result<(), AgentError> {
    let mut spool = shared.spool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    spool.mark_summary_acked()
}

fn event_id(frame: &AgentFrame) -> Option<vowp_protocol::EventId> {
    match frame {
        AgentFrame::RemoteLine {
            line: RemoteLine::Event { event },
            ..
        } => Some(event.id.clone()),
        _ => None,
    }
}

fn path_ref_project(path: &PathRef) -> Option<&ProjectName> {
    match path {
        PathRef::Project { project } | PathRef::Worktree { project, .. } | PathRef::Subdir { project, .. } => {
            Some(project)
        }
    }
}

fn remote_error(kind: ErrorKind, message: String, retryable: bool) -> AgentError {
    if kind == ErrorKind::Auth {
        AgentError::Auth(message)
    } else {
        AgentError::Remote {
            kind,
            message,
            retryable,
        }
    }
}
