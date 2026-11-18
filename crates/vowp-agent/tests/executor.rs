//! Run-assignment loop tests for the Mac-side agent.
//!
//! The fake home, fake handler and temporary state directory are in memory or on disk
//! under `tempfile`; no SSH, network, store, harness process or real `~/.vowp-agent` is
//! touched.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vowp_agent::RunOutcome as ClientOutcome;
use vowp_agent::{
    AcceptedRun, AgentClient, AgentClock, AgentConfig, AgentError, EVENT_WINDOW, FrameRead, FrameStream, GitRunner,
    HandlerReject, NodeKey, PushStatus, QUARANTINE_FILE, RunHandler, RunSink, RunSummary, SystemGit, Transport,
};
use vowp_protocol::{
    AGENT_PROTOCOL, Actor, AgentFrame, AgentRole, AssignmentPayload, BuildMode, Cost, EventId, EventSource, EventType,
    Executor, HarnessKind, JobId, NodeName, PathRef, ProjectName, RemoteEvent, RemoteLine, RemoteRunState, RemoteUsage,
    ResultTransport, RunId, RunOutcome, RunPurpose, RunRejectReason, RunResumeResult, SecretName, SessionId,
    StopReason, Timestamp, Tokens, UsageSource,
};

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
const JOB: &str = "agent-run-loop";
const PROJECT: &str = "vowp";
const LEASE: &str = "vowp_run_lease_token_for_the_first_connection_0001";
const FRESH_LEASE: &str = "vowp_run_lease_token_for_the_second_connection_002";
const SECRET_VALUE: &str = "s3cr3t-value-that-must-never-be-written";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

fn session() -> SessionId {
    SessionId::parse(SESSION).expect("session id")
}

fn run() -> RunId {
    RunId::parse(RUN).expect("run id")
}

fn job() -> JobId {
    JobId::parse(JOB).expect("job id")
}

fn project() -> ProjectName {
    ProjectName::parse(PROJECT).expect("project name")
}

fn node() -> NodeName {
    NodeName::parse("mac-studio").expect("node slug")
}

fn worktree_ref() -> PathRef {
    PathRef::parse("worktree:vowp/agent-run-loop").expect("path ref")
}

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

/// One connection's scripted behaviour.
#[derive(Debug, Clone)]
struct ConnPlan {
    /// Send the assignment right after `HomeWelcome`.
    send_assignment: bool,
    /// Answer `Resume` with `ResumeOk` and a fresh `AssignRun`.
    resume: bool,
    resend_assignment: bool,
    /// The lease token this connection hands out.
    lease: String,
    /// How event lines are acknowledged.
    ack: AckMode,
    /// What the home does after the terminal summary.
    end: EndMode,
    /// Answer `BundleStart` with `BundleAccept`.
    accept_bundle: bool,
    /// Delay `BundleAccept` by this many empty polls after `BundleStart`.
    bundle_accept_after_polls: Option<usize>,
    /// Accept from the bytes already stored on an earlier connection.
    resume_bundle: bool,
    /// Refuse `BundleStart` with this reason.
    abort_bundle: Option<String>,
    /// Drop the transport after this many bundle chunks.
    drop_after_bundle_chunks: Option<usize>,
}

impl Default for ConnPlan {
    fn default() -> Self {
        Self {
            send_assignment: false,
            resume: false,
            resend_assignment: false,
            lease: LEASE.to_owned(),
            ack: AckMode::AckAll,
            end: EndMode::Summary,
            accept_bundle: false,
            bundle_accept_after_polls: None,
            resume_bundle: false,
            abort_bundle: None,
            drop_after_bundle_chunks: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum AckMode {
    /// Acknowledge every event.
    AckAll,
    /// Acknowledge the first event as an already-present duplicate.
    DuplicateFirst,
    /// Acknowledge the first event, then reject the second as a conflict.
    ConflictSecond,
    /// Acknowledge the first N events, then drop the transport before the next send.
    DropAfter(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndMode {
    /// Acknowledge the summary and revoke the node, ending the loop.
    Summary,
    /// Revoke after `RunRejected`.
    Rejected,
    /// After the summary, send Cleanup, a stale CancelRun and Revoked.
    Cleanup,
    /// Revoke after the conflicting acknowledgement.
    Conflict,
    /// Do not end automatically; the handler pushes the final frames.
    Never,
}

#[derive(Default)]
struct FakeState {
    connections: usize,
    received: Vec<AgentFrame>,
    per_connection: Vec<Vec<AgentFrame>>,
    seqs: BTreeMap<String, u64>,
    next_seq: u64,
    extras: VecDeque<AgentFrame>,
    spool_check: Option<PathBuf>,
    assignment: Option<AssignmentPayload>,
    bundle_received: u64,
    bundle_offsets: Vec<(usize, u64)>,
}

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<FakeState>>,
    plans: Vec<ConnPlan>,
}

impl FakeHome {
    fn new(plans: Vec<ConnPlan>) -> Self {
        let state = FakeState {
            next_seq: 1,
            ..FakeState::default()
        };
        Self {
            shared: Arc::new(Mutex::new(state)),
            plans,
        }
    }

    fn push(&self, frame: AgentFrame) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extras
            .push_back(frame);
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn frames_on(&self, connection: usize) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .per_connection
            .get(connection)
            .cloned()
            .unwrap_or_default()
    }

    fn connections(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connections
    }

    fn set_spool_check(&self, path: PathBuf) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .spool_check = Some(path);
    }

    fn set_assignment(&self, assignment: AssignmentPayload) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .assignment = Some(assignment);
    }

    fn bundle_offsets(&self) -> Vec<(usize, u64)> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bundle_offsets
            .clone()
    }
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = state.connections;
        state.connections += 1;
        state.per_connection.push(Vec::new());
        drop(state);
        let plan = self.plans.get(connection).cloned().unwrap_or_default();
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            plan,
            connection,
            outbound: VecDeque::new(),
            closed: false,
            drop_on_next_send: false,
            acked_events: 0,
            in_flight: 0,
            conflict_sent: false,
            bundle_chunks: 0,
            delayed_accept: None,
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<FakeState>>,
    plan: ConnPlan,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
    closed: bool,
    drop_on_next_send: bool,
    acked_events: usize,
    in_flight: u64,
    conflict_sent: bool,
    bundle_chunks: usize,
    delayed_accept: Option<(RunId, u64, usize)>,
}

impl FakeStream {
    fn record(&self, frame: &AgentFrame) {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.received.push(frame.clone());
        if let Some(connection) = state.per_connection.get_mut(self.connection) {
            connection.push(frame.clone());
        }
    }

    fn assignment(&self) -> AssignmentPayload {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .assignment
            .clone()
            .unwrap_or_else(|| assignment_with_lease(&self.plan.lease))
    }

    fn ack_event(&mut self, event: &RemoteEvent) {
        let seq = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let seq = if let Some(seq) = state.seqs.get(event.id.as_str()).copied() {
                seq
            } else {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.seqs.insert(event.id.as_str().to_owned(), seq);
                seq
            };
            state.next_seq = state.next_seq.max(seq + 1);
            seq
        };
        self.in_flight += 1;
        if self.in_flight + 1 == EVENT_WINDOW {
            self.outbound.push_back(AgentFrame::Credit {
                run_id: run(),
                count: EVENT_WINDOW,
            });
        }
        self.outbound.push_back(AgentFrame::IngestAck {
            run_id: run(),
            event_id: Some(event.id.clone()),
            seq: Some(seq),
            duplicate: false,
            lease_revoked: None,
            error: None,
        });
    }

    fn accept_bundle(&mut self, run_id: &RunId, epoch: u64) {
        let offset = if self.plan.resume_bundle {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .bundle_received
        } else {
            0
        };
        self.outbound.push_back(AgentFrame::BundleAccept {
            run_id: run_id.clone(),
            epoch,
            offset,
        });
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bundle_offsets
            .push((self.connection, offset));
    }

    fn revoke(&mut self) {
        self.outbound.push_back(AgentFrame::Revoked {
            reason: "test complete".to_owned(),
        });
    }
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        if let Some(frame) = self.outbound.pop_front() {
            return Ok(FrameRead::Frame(frame));
        }
        if let Some(frame) = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extras
            .pop_front()
        {
            return Ok(FrameRead::Frame(frame));
        }
        if let Some((run_id, epoch, remaining)) = self.delayed_accept.take() {
            if remaining == 0 {
                self.accept_bundle(&run_id, epoch);
                if let Some(frame) = self.outbound.pop_front() {
                    return Ok(FrameRead::Frame(frame));
                }
            } else {
                self.delayed_accept = Some((run_id, epoch, remaining - 1));
            }
        }
        if self.closed {
            return Ok(FrameRead::Closed);
        }
        Ok(FrameRead::Timeout)
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        if self.closed {
            return Err(AgentError::Transport("connection closed".to_owned()));
        }
        if self.drop_on_next_send {
            self.closed = true;
            return Err(AgentError::Transport("connection dropped by the test".to_owned()));
        }
        self.record(frame);
        match frame {
            AgentFrame::AgentHello { .. } => {
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: format!("nonce-{}", self.connection),
                    home_id: "home-1".to_owned(),
                    connection_id: format!("connection-{}", self.connection),
                    server_time: ts(),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                self.outbound.push_back(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: node(),
                    connection_id: format!("connection-{}", self.connection),
                });
                if self.plan.send_assignment {
                    let assignment = self.assignment();
                    self.outbound.push_back(AgentFrame::AssignRun {
                        assignment,
                        lease_token: self.plan.lease.clone(),
                        lease_expires_at: ts().plus(Duration::from_secs(600)),
                    });
                }
            }
            AgentFrame::Heartbeat { .. } => {}
            AgentFrame::Resume { runs, .. } => {
                let results = runs
                    .iter()
                    .map(|resume| RunResumeResult {
                        run_id: resume.run_id.clone(),
                        epoch: resume.epoch,
                        accepted: self.plan.resume,
                        reason: if self.plan.resume {
                            None
                        } else {
                            Some("the run is not current on this node".to_owned())
                        },
                        tail_seq: resume.last_acked,
                    })
                    .collect();
                self.outbound.push_back(AgentFrame::ResumeOk {
                    runs: results,
                    surfaces: Vec::new(),
                });
                if self.plan.resend_assignment {
                    let assignment = self.assignment();
                    self.outbound.push_back(AgentFrame::AssignRun {
                        assignment,
                        lease_token: self.plan.lease.clone(),
                        lease_expires_at: ts().plus(Duration::from_secs(600)),
                    });
                }
            }
            AgentFrame::RemoteLine {
                line: RemoteLine::Event { event },
                ..
            } => {
                if let Some(path) = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .spool_check
                    .clone()
                {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    assert!(
                        text.contains(event.id.as_str()),
                        "event {} was not spooled before it was sent: {text}",
                        event.id
                    );
                }
                if matches!(self.plan.ack, AckMode::ConflictSecond) && self.acked_events == 1 && !self.conflict_sent {
                    self.conflict_sent = true;
                    self.outbound.push_back(AgentFrame::IngestAck {
                        run_id: run(),
                        event_id: Some(event.id.clone()),
                        seq: None,
                        duplicate: false,
                        lease_revoked: None,
                        error: Some("event id is already present with different content".to_owned()),
                    });
                    if self.plan.end == EndMode::Conflict {
                        self.revoke();
                    }
                    return Ok(());
                }
                if matches!(self.plan.ack, AckMode::DuplicateFirst) && self.acked_events == 0 {
                    let seq = {
                        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let seq = if let Some(seq) = state.seqs.get(event.id.as_str()).copied() {
                            seq
                        } else {
                            let seq = state.next_seq;
                            state.next_seq += 1;
                            state.seqs.insert(event.id.as_str().to_owned(), seq);
                            seq
                        };
                        state.next_seq = state.next_seq.max(seq + 1);
                        seq
                    };
                    self.outbound.push_back(AgentFrame::IngestAck {
                        run_id: run(),
                        event_id: Some(event.id.clone()),
                        seq: Some(seq),
                        duplicate: true,
                        lease_revoked: None,
                        error: None,
                    });
                    self.acked_events += 1;
                    return Ok(());
                }
                self.ack_event(event);
                self.acked_events += 1;
                if let AckMode::DropAfter(count) = self.plan.ack
                    && self.acked_events >= count
                {
                    self.drop_on_next_send = true;
                }
            }
            AgentFrame::BundleStart { run_id, epoch, .. } => {
                if let Some(reason) = &self.plan.abort_bundle {
                    self.outbound.push_back(AgentFrame::BundleAbort {
                        run_id: run_id.clone(),
                        epoch: *epoch,
                        reason: reason.clone(),
                    });
                } else if self.plan.accept_bundle {
                    match self.plan.bundle_accept_after_polls {
                        Some(polls) => {
                            self.delayed_accept = Some((run_id.clone(), *epoch, polls));
                        }
                        None => self.accept_bundle(run_id, *epoch),
                    }
                }
            }
            AgentFrame::BundleChunk { data, .. } => {
                {
                    let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.bundle_received = state
                        .bundle_received
                        .saturating_add(u64::try_from(data.len() / 2).unwrap_or(u64::MAX));
                }
                self.bundle_chunks += 1;
                if self
                    .plan
                    .drop_after_bundle_chunks
                    .is_some_and(|count| self.bundle_chunks >= count)
                {
                    self.drop_on_next_send = true;
                }
            }
            AgentFrame::BundleEnd { .. } => {}
            AgentFrame::TurnEnd { run_id, .. } => {
                let seq = self.next_seq();
                self.outbound.push_back(AgentFrame::IngestAck {
                    run_id: run_id.clone(),
                    event_id: None,
                    seq: Some(seq),
                    duplicate: false,
                    lease_revoked: None,
                    error: None,
                });
                match self.plan.end {
                    EndMode::Summary => self.revoke(),
                    EndMode::Cleanup => {
                        self.outbound.push_back(AgentFrame::Cleanup {
                            run_id: run_id.clone(),
                            job: job(),
                            worktree: worktree_ref(),
                        });
                        self.outbound.push_back(AgentFrame::CancelRun {
                            run_id: run_id.clone(),
                            epoch: 1,
                            reason: "after cleanup".to_owned(),
                        });
                        self.revoke();
                    }
                    EndMode::Conflict | EndMode::Rejected | EndMode::Never => {}
                }
            }
            AgentFrame::RunRejected { .. } => {
                if self.plan.end == EndMode::Rejected {
                    self.revoke();
                }
            }
            AgentFrame::RunState { detail, .. } if detail.as_str() == "spool_full" => {
                if self.plan.end != EndMode::Never {
                    self.revoke();
                }
            }
            AgentFrame::Ping => {}
            _ => {}
        }
        Ok(())
    }
}

impl FakeStream {
    fn next_seq(&self) -> u64 {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = state.next_seq;
        state.next_seq += 1;
        seq
    }
}

fn assignment_with_lease(_lease: &str) -> AssignmentPayload {
    AssignmentPayload {
        assignment_id: format!("assignment-{RUN}"),
        run_id: run(),
        session_id: session(),
        project: project(),
        job: job(),
        executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
        purpose: RunPurpose::Start,
        prompt: "implement the run loop".to_owned(),
        cwd_ref: worktree_ref(),
        base_commit: "a".repeat(40),
        remote: "origin".to_owned(),
        remote_url: "https://example.com/vowp.git".to_owned(),
        result_transport: vowp_protocol::ResultTransport::Push,
        env_names: vec!["API_KEY".to_owned()],
        secret_names: vec![SecretName::parse("API_KEY").expect("secret name")],
        epoch: 1,
        deadline: None,
        spool_cap_bytes: 2 * 1024 * 1024,
    }
}

fn assignment() -> AssignmentPayload {
    assignment_with_lease(LEASE)
}

fn state_dir(dir: &Path) -> PathBuf {
    let config = AgentConfig::new("home-1", node(), dir.to_path_buf(), "home.example");
    config.write().expect("agent.json");
    NodeKey::load_or_generate(dir).expect("node key");
    dir.to_path_buf()
}

fn agent_client(dir: &Path, home: FakeHome) -> AgentClient {
    let config = AgentConfig::load(dir).expect("agent config");
    let key = NodeKey::load(dir).expect("node key");
    AgentClient::new(
        dir.to_path_buf(),
        key,
        config,
        Box::new(home),
        Arc::new(FixedClock(ts())),
    )
}

fn remote_event(text: &str) -> RemoteEvent {
    RemoteEvent {
        id: EventId::new(),
        ts: ts(),
        event_type: EventType::AssistantMessage,
        actor: Actor::Assistant,
        source: EventSource {
            harness: HarnessKind::Opencode,
            model: Some("deepseek/deepseek-flash".to_owned()),
            node: Some(node()),
            native_session_id: None,
        },
        payload: serde_json::json!({ "text": text }),
        raw: None,
        replayable: true,
    }
}

fn event_line(text: &str) -> RemoteLine {
    RemoteLine::Event {
        event: remote_event(text),
    }
}

fn summary() -> RunSummary {
    RunSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: Some("ses_mac".to_owned()),
        cost: Cost::receipt(
            0.25,
            Tokens {
                input: 10,
                output: 20,
                ..Tokens::default()
            },
        ),
        limit: None,
        usage: Some(RemoteUsage {
            peak_rss_bytes: 1024,
            anon_peak_bytes: None,
            file_peak_bytes: None,
            kernel_peak_bytes: None,
            cpu_usec: 10,
            mean_cpu_cores: 0.5,
            max_cpu_cores: 1.0,
            build_mode: BuildMode::Unknown,
            oom_killed: false,
            source: UsageSource::Default,
        }),
        head_commit: Some("b".repeat(40)),
        push_status: Some(PushStatus::Pushed {
            branch: format!("job/{JOB}"),
            remote: "origin".to_owned(),
            head_commit: "b".repeat(40),
        }),
        result_text: Some("finished".to_owned()),
        stop_reason: StopReason::EndTurn,
    }
}

struct HandlerBehavior {
    reject: Option<HandlerReject>,
    events: Vec<RemoteLine>,
    summary: RunSummary,
    push_after_events: Option<AgentFrame>,
    wait_for_stop: bool,
    push_after_wait: Option<AgentFrame>,
    worktree: Option<PathBuf>,
}

impl Default for HandlerBehavior {
    fn default() -> Self {
        Self {
            reject: None,
            events: Vec::new(),
            summary: summary(),
            push_after_events: None,
            wait_for_stop: false,
            push_after_wait: None,
            worktree: None,
        }
    }
}

#[derive(Default)]
struct HandlerState {
    started: usize,
    finished: usize,
    accepted: Vec<String>,
    cancelled: Vec<String>,
    cleaned: Vec<String>,
    sink_errors: Vec<String>,
}

struct FakeHandler {
    behavior: HandlerBehavior,
    state: Arc<Mutex<HandlerState>>,
    home: FakeHome,
    stop: Arc<(Mutex<bool>, Condvar)>,
}

impl FakeHandler {
    fn new(home: FakeHome, behavior: HandlerBehavior) -> Self {
        Self {
            behavior,
            state: Arc::new(Mutex::new(HandlerState::default())),
            home,
            stop: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HandlerState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RunHandler for FakeHandler {
    fn accept(&self, assignment: &AssignmentPayload) -> Result<AcceptedRun, HandlerReject> {
        if let Some(reject) = &self.behavior.reject {
            return Err(reject.clone());
        }
        Ok(AcceptedRun {
            assignment: assignment.clone(),
            epoch: assignment.epoch,
            cwd: PathBuf::from("/private/tmp/vowp-agent-test/work"),
            worktree: self
                .behavior
                .worktree
                .clone()
                .unwrap_or_else(|| PathBuf::from("/private/tmp/vowp-agent-test/worktree")),
            project_root: PathBuf::from("/private/tmp/vowp-agent-test/root"),
            reserve_bytes: 5 * 1024 * 1024 * 1024,
        })
    }

    fn run(&self, accepted: AcceptedRun, sink: &mut dyn RunSink) {
        {
            let mut state = self.state();
            state.started += 1;
            state.accepted.push(format!("{accepted:?}"));
        }
        for line in &self.behavior.events {
            if let Err(error) = sink.line(line.clone()) {
                self.state().sink_errors.push(error.to_string());
                return;
            }
        }
        if let Some(frame) = &self.behavior.push_after_events {
            self.home.push(frame.clone());
        }
        if self.behavior.wait_for_stop {
            let (lock, cvar) = &*self.stop;
            let mut stopped = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*stopped {
                stopped = cvar.wait(stopped).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(stopped);
            if let Some(frame) = &self.behavior.push_after_wait {
                self.home.push(frame.clone());
            }
        }
        if sink.finish(self.behavior.summary.clone()).is_ok() {
            self.state().finished += 1;
        }
    }

    fn cancel(&self, run: &RunId) {
        self.state().cancelled.push(run.to_string());
        let (lock, cvar) = &*self.stop;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        cvar.notify_all();
    }

    fn cleanup(&self, run: &RunId) {
        self.state().cleaned.push(run.to_string());
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition was not reached before the test timeout");
}

fn run_client(client: &mut AgentClient, handler: &Arc<FakeHandler>) -> ClientOutcome {
    client
        .run_with_handler(
            &vowp_agent::RunOptions::new(AgentRole::Executor),
            Arc::clone(handler) as Arc<dyn RunHandler>,
        )
        .expect("run loop")
}

fn event_ids(frames: &[AgentFrame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::RemoteLine {
                line: RemoteLine::Event { event },
                ..
            } => Some(event.id.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

fn count_matching(frames: &[AgentFrame], mut predicate: impl FnMut(&AgentFrame) -> bool) -> usize {
    frames.iter().filter(|frame| predicate(frame)).count()
}

#[test]
fn assign_run_rejects_with_the_documented_reason_and_does_not_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        end: EndMode::Rejected,
        ..ConnPlan::default()
    }]);
    let handler = Arc::new(FakeHandler::new(
        home.clone(),
        HandlerBehavior {
            reject: Some(HandlerReject::new(
                RunRejectReason::HarnessMissing,
                "the opencode binary is not installed",
            )),
            ..HandlerBehavior::default()
        },
    ));
    let mut client = agent_client(dir.path(), home.clone());

    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    let frames = home.frames();
    let rejected: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::RunRejected {
                assignment_id,
                run_id,
                epoch,
                reason,
                detail,
            } => Some((assignment_id.clone(), run_id.clone(), *epoch, *reason, detail.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 1, "exactly one rejection: {frames:?}");
    assert_eq!(rejected[0].0, format!("assignment-{RUN}"));
    assert_eq!(rejected[0].1, run());
    assert_eq!(rejected[0].2, 1);
    assert_eq!(rejected[0].3, RunRejectReason::HarnessMissing);
    assert_eq!(rejected[0].4, "the opencode binary is not installed");
    assert_eq!(handler.state().started, 0, "a rejected run never starts");
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::RunAccepted { .. })),
        0
    );
    assert_eq!(event_ids(&frames).len(), 0);
}

#[test]
fn accepted_run_spools_every_line_before_sending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = state_dir(dir.path());
    let spool = vowp_agent::spool::spool_dir(&state, &session(), &run()).join(vowp_agent::EVENTS_FILE);
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ..ConnPlan::default()
    }]);
    // The transport asserts inside `send` that the event is already in the spool.
    let behavior = HandlerBehavior {
        events: vec![event_line("first"), event_line("second")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    home.set_spool_check(spool.clone());

    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);

    let frames = home.frames();
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::RunAccepted { .. })),
        1
    );
    let running = frames
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::RunState { state, detail, .. } if *state == RemoteRunState::Running => Some(detail.clone()),
            _ => None,
        })
        .expect("a start state");
    assert_eq!(running, "started");
    assert_eq!(event_ids(&frames).len(), 2);
    let hello = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                AgentFrame::RemoteLine {
                    line: RemoteLine::Hello { .. },
                    ..
                }
            )
        })
        .expect("a hello");
    let first_event = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                AgentFrame::RemoteLine {
                    line: RemoteLine::Event { .. },
                    ..
                }
            )
        })
        .expect("an event");
    assert!(hello < first_event, "the hello precedes every event");
    let spool = vowp_agent::Spool::open(&state, &session(), &run()).expect("spool");
    assert!(spool.summary_acked());
    assert!(spool.unacked().expect("unacked").is_empty());
}

#[test]
fn credit_replenishes_the_event_window() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: (0..EVENT_WINDOW + 6)
            .map(|index| event_line(&format!("event {index}")))
            .collect(),
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);
    assert_eq!(
        event_ids(&home.frames()).len(),
        usize::try_from(EVENT_WINDOW + 6).expect("count"),
        "the 64-event window is replenished instead of stalling"
    );
}

#[test]
fn duplicate_ack_is_not_resent_and_conflict_stops_the_stream() {
    // A duplicate acknowledgement leaves the line in the log once and the replay skips it.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ack: AckMode::DuplicateFirst,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("duplicate")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);
    let first = event_ids(&home.frames());
    assert_eq!(first.len(), 1, "a duplicate is still sent once: {first:?}");
    let spool = vowp_agent::Spool::open(&state, &session(), &run()).expect("spool");
    assert!(
        spool.unacked().expect("unacked").is_empty(),
        "duplicate id was recorded as acked"
    );
    assert!(spool.summary_acked());

    // A conflicting acknowledgement stops the stream and quarantines the spool.
    let dir = tempfile::tempdir().expect("temp dir");
    let state = state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ack: AckMode::ConflictSecond,
        end: EndMode::Conflict,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("accepted"), event_line("conflict")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| !handler.state().cancelled.is_empty());
    let frames = home.frames();
    assert_eq!(
        event_ids(&frames).len(),
        2,
        "the conflict is reported after the event is sent"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        0,
        "a conflicting stream never sends a summary"
    );
    let quarantine = vowp_agent::spool::spool_dir(&state, &session(), &run()).join(QUARANTINE_FILE);
    let text = std::fs::read_to_string(&quarantine).expect("quarantine.json");
    assert!(text.contains("different content"), "{text}");
    let spool = vowp_agent::Spool::open(&state, &session(), &run()).expect("spool");
    assert_eq!(
        spool.unacked_events().expect("unacked"),
        1,
        "the unacknowledged event is kept"
    );
    let events = vowp_agent::spool::spool_dir(&state, &session(), &run()).join(vowp_agent::EVENTS_FILE);
    let text = std::fs::read_to_string(events).expect("events file");
    assert!(text.contains("accepted"), "the acknowledged event is kept: {text}");
    assert!(text.contains("conflict"), "the refused event is kept: {text}");
}

#[test]
fn run_state_and_turn_end_carry_the_summary() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ..ConnPlan::default()
    }]);
    let mut expected = summary();
    expected.exit_code = Some(7);
    expected.result_text = Some("the final text".to_owned());
    expected.head_commit = Some("c".repeat(40));
    let behavior = HandlerBehavior {
        events: vec![event_line("work")],
        summary: expected.clone(),
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);

    let frames = home.frames();
    let finishing = frames
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::RunState {
                state: RemoteRunState::Finishing,
                head_commit,
                ..
            } => Some(head_commit.clone()),
            _ => None,
        })
        .expect("a finishing state");
    assert_eq!(finishing, Some("c".repeat(40)));
    let turns: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::TurnEnd { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(turns.len(), 1, "one terminal summary");
    let summary = &turns[0];
    assert_eq!(summary.exit_code, Some(7));
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.result_text.as_deref(), Some("the final text"));
    assert_eq!(summary.stop_reason, StopReason::EndTurn);
    assert_eq!(summary.cost.usd, Some(0.25));
    assert_eq!(summary.usage.as_ref().map(|usage| usage.peak_rss_bytes), Some(1024));
}

#[test]
fn cancel_run_stops_the_handler_and_keeps_the_spool() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        end: EndMode::Never,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("before cancel")],
        push_after_events: Some(AgentFrame::CancelRun {
            run_id: run(),
            epoch: 1,
            reason: "the owner cancelled it".to_owned(),
        }),
        push_after_wait: Some(AgentFrame::Revoked {
            reason: "test complete".to_owned(),
        }),
        wait_for_stop: true,
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| !handler.state().cancelled.is_empty());

    let state = handler.state();
    assert_eq!(state.started, 1);
    assert_eq!(state.cancelled, vec![RUN.to_owned()]);
    assert!(state.cleaned.is_empty(), "cancel leaves cleanup to the handler/home");
    drop(state);
    let frames = home.frames();
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            AgentFrame::RunState { detail, .. } if detail.contains("cancelled")
        )),
        "a cancelled state is reported: {frames:?}"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        0,
        "the cancelled handler did not produce a summary"
    );
    let spool = vowp_agent::spool::spool_dir(&state_dir(dir.path()), &session(), &run()).join(vowp_agent::EVENTS_FILE);
    let text = std::fs::read_to_string(&spool).expect("kept spool");
    assert!(text.contains("before cancel"), "the spool is kept: {text}");
    assert!(
        !spool.with_file_name(QUARANTINE_FILE).exists(),
        "a cancel is not a conflict"
    );
}

#[test]
fn run_lost_fences_the_handler() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        end: EndMode::Never,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("before the fence")],
        push_after_events: Some(AgentFrame::RunLost {
            run_id: run(),
            reason: "the home fenced the run".to_owned(),
        }),
        push_after_wait: Some(AgentFrame::Revoked {
            reason: "test complete".to_owned(),
        }),
        wait_for_stop: true,
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| !handler.state().cancelled.is_empty());

    let state = handler.state();
    assert_eq!(state.started, 1);
    assert_eq!(state.cancelled.len(), 1, "the handler was stopped");
    assert!(state.cleaned.is_empty());
    drop(state);
    let frames = home.frames();
    assert!(
        event_ids(&frames).len() <= 1,
        "the fence stops further run lines: {frames:?}"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        0
    );
    let spool = vowp_agent::spool::spool_dir(&state_dir(dir.path()), &session(), &run()).join(vowp_agent::EVENTS_FILE);
    let text = std::fs::read_to_string(&spool).expect("kept spool");
    assert!(text.contains("before the fence"), "the spool is kept: {text}");
    assert!(
        !spool.with_file_name(QUARANTINE_FILE).exists(),
        "a fence is not a conflict"
    );
}

#[test]
fn resume_replays_from_the_first_unacked_line() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![
        ConnPlan {
            send_assignment: true,
            ack: AckMode::DropAfter(1),
            end: EndMode::Never,
            ..ConnPlan::default()
        },
        ConnPlan {
            resume: true,
            resend_assignment: true,
            lease: FRESH_LEASE.to_owned(),
            ..ConnPlan::default()
        },
    ]);
    let behavior = HandlerBehavior {
        events: vec![event_line("one"), event_line("two"), event_line("three")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);
    assert_eq!(home.connections(), 2, "one reconnect");

    let first = event_ids(&home.frames_on(0));
    let second = event_ids(&home.frames_on(1));
    assert_eq!(first.len(), 1, "only the first event was acknowledged before the drop");
    assert_eq!(second.len(), 2, "the unacked tail is replayed once: {second:?}");
    assert!(!second.contains(&first[0]), "the acknowledged line is not replayed");

    let resumed = home
        .frames_on(1)
        .into_iter()
        .find_map(|frame| match frame {
            AgentFrame::Resume { runs, .. } => Some(runs),
            _ => None,
        })
        .expect("Resume on the new connection");
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].run_id, run());
    assert_eq!(resumed[0].epoch, 1);
    assert_eq!(resumed[0].last_acked, 1, "Resume names the last acknowledged seq");

    let hello = home
        .frames_on(1)
        .into_iter()
        .find_map(|frame| match frame {
            AgentFrame::RemoteLine {
                line: RemoteLine::Hello { lease, .. },
                ..
            } => Some(lease),
            _ => None,
        })
        .expect("hello after the reconnect");
    assert_eq!(hello, FRESH_LEASE, "the fresh AssignRun token replaced the old one");
}

#[test]
fn cleanup_releases_the_run_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        end: EndMode::Cleanup,
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("done")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);

    let state = handler.state();
    assert_eq!(state.cleaned, vec![RUN.to_owned()], "cleanup reached the handler");
    assert!(
        state.cancelled.is_empty(),
        "a released run ignores the stale CancelRun: {:?}",
        state.cancelled
    );
}

#[test]
fn lease_token_and_secret_values_never_reach_the_spool_or_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = state_dir(dir.path());
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        lease: LEASE.to_owned(),
        ..ConnPlan::default()
    }]);
    let behavior = HandlerBehavior {
        events: vec![event_line("the handler redacted the value before emitting")],
        summary: RunSummary {
            result_text: Some("finished without the value".to_owned()),
            ..summary()
        },
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| handler.state().finished == 1);

    let spool_dir = vowp_agent::spool::spool_dir(&state, &session(), &run());
    let mut written = String::new();
    for name in [vowp_agent::EVENTS_FILE, vowp_agent::ACKED_FILE] {
        written.push_str(&std::fs::read_to_string(spool_dir.join(name)).unwrap_or_default());
    }
    written.push_str(&std::fs::read_to_string(state.join(vowp_agent::STATUS_FILE)).unwrap_or_default());
    for forbidden in [LEASE, SECRET_VALUE, "/private/tmp/vowp-agent-test"] {
        assert!(
            !written.contains(forbidden),
            "{forbidden:?} was written to the spool or status"
        );
    }
    let accepted = handler.state().accepted.clone();
    assert_eq!(accepted.len(), 1);
    assert!(!accepted[0].contains("lease_token"), "{:?}", accepted[0]);
    assert!(
        !accepted[0].contains("/private/tmp/vowp-agent-test"),
        "{:?}",
        accepted[0]
    );

    // Only the protocol hello may carry the lease token; no run frame repeats it.
    let frames = home.frames();
    for frame in &frames {
        if matches!(
            frame,
            AgentFrame::RemoteLine {
                line: RemoteLine::Hello { .. },
                ..
            }
        ) {
            continue;
        }
        let text = serde_json::to_string(frame).expect("frame json");
        assert!(
            !text.contains(LEASE),
            "a non-hello frame leaked the lease token: {text}"
        );
        assert!(
            !text.contains("/private/tmp/vowp-agent-test"),
            "a frame leaked an absolute local path: {text}"
        );
    }
}

#[test]
fn spool_full_stops_the_run_and_records_the_reason() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = state_dir(dir.path());
    let mut assignment = assignment();
    assignment.spool_cap_bytes = 32;
    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        ..ConnPlan::default()
    }]);
    home.set_assignment(assignment);
    let behavior = HandlerBehavior {
        events: vec![event_line("too large for the cap")],
        ..HandlerBehavior::default()
    };
    let handler = Arc::new(FakeHandler::new(home.clone(), behavior));
    let mut client = agent_client(dir.path(), home.clone());
    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    wait_for(|| !handler.state().sink_errors.is_empty());

    let frames = home.frames();
    assert_eq!(event_ids(&frames).len(), 0, "an unspooled line is never sent");
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            AgentFrame::Error { message, .. } if message.contains("spool_full")
        )),
        "the home receives the spool_full reason: {frames:?}"
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            AgentFrame::RunState { detail, .. } if detail == "spool_full"
        )),
        "the run state records spool_full: {frames:?}"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        0,
        "the terminal summary cannot be spooled"
    );
    let events = vowp_agent::spool::spool_dir(&state, &session(), &run()).join(vowp_agent::EVENTS_FILE);
    assert_eq!(std::fs::metadata(events).expect("events file").len(), 0);
}

#[test]
fn bundle_result_is_chunked_and_resumed_after_a_disconnect() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("worktree");
    let git = SystemGit::new();
    std::fs::create_dir_all(&repo).expect("repo");
    git_init(&git, &repo);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "base"]);
    let base = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);
    git_ok(&git, &repo, &["checkout", "-B", &format!("job/{JOB}")]);
    // Varied enough that the bundle needs several 256 KiB chunks.
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut text = String::with_capacity(4 * 1024 * 1024);
    while text.len() < 4 * 1024 * 1024 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        for byte in seed.to_le_bytes() {
            text.push(char::from(b'a' + (byte % 26)));
        }
    }
    std::fs::write(repo.join("large.txt"), text).expect("large file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "job work"]);
    let head = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);

    let home = FakeHome::new(vec![
        ConnPlan {
            send_assignment: true,
            accept_bundle: true,
            drop_after_bundle_chunks: Some(1),
            end: EndMode::Never,
            ..ConnPlan::default()
        },
        ConnPlan {
            resume: true,
            resend_assignment: true,
            accept_bundle: true,
            resume_bundle: true,
            ..ConnPlan::default()
        },
    ]);
    let mut assignment = assignment_with_lease(LEASE);
    assignment.result_transport = ResultTransport::Bundle;
    assignment.remote_url = String::new();
    assignment.base_commit = base.clone();
    home.set_assignment(assignment);
    let mut run_summary = summary();
    run_summary.head_commit = Some(head.clone());
    run_summary.push_status = None;
    let handler = Arc::new(FakeHandler::new(
        home.clone(),
        HandlerBehavior {
            summary: run_summary,
            worktree: Some(repo.clone()),
            ..HandlerBehavior::default()
        },
    ));
    let dir = tempfile::tempdir().expect("state dir");
    state_dir(dir.path());
    let mut client = agent_client(dir.path(), home.clone());

    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");
    assert_eq!(home.connections(), 2, "the first connection dropped mid-transfer");

    let first = home.frames_on(0);
    let second = home.frames_on(1);
    let first_start = first
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::BundleStart {
                base: start_base,
                head: start_head,
                branch,
                sha256,
                bytes,
                ..
            } => Some((
                start_base.clone(),
                start_head.clone(),
                branch.clone(),
                sha256.clone(),
                *bytes,
            )),
            _ => None,
        })
        .expect("first BundleStart");
    assert_eq!(first_start.0, base);
    assert_eq!(first_start.1, head);
    assert_eq!(first_start.2, format!("job/{JOB}"));
    assert!(first_start.4 > 256 * 1024, "the bundle needs more than one chunk");

    let first_chunks = first
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::BundleChunk { offset, data, .. } => Some((*offset, data.len() / 2)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(first_chunks.len(), 1, "the connection dropped after one chunk");
    assert_eq!(first_chunks[0].0, 0, "the first chunk starts at zero");
    let stored = first_chunks[0].1 as u64;
    assert_eq!(stored, 256 * 1024, "one chunk is at most 256 KiB");

    let second_accept = home
        .bundle_offsets()
        .into_iter()
        .find_map(|(connection, offset)| (connection == 1).then_some(offset))
        .expect("the home accepts the resume");
    assert_eq!(second_accept, stored, "the home answers with the stored bytes");

    let second_chunks = second
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::BundleChunk { offset, data, .. } => Some((*offset, data.len() / 2)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!second_chunks.is_empty(), "the transfer continues after the resume");
    assert_eq!(
        second_chunks[0].0, stored,
        "the first resumed chunk starts at the offset"
    );
    let mut next = stored;
    for (offset, len) in &second_chunks {
        assert_eq!(*offset, next, "chunk offsets stay contiguous");
        next = next.saturating_add(*len as u64);
    }

    let end = second
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::BundleEnd { bytes, sha256, .. } => Some((*bytes, sha256.clone())),
            _ => None,
        })
        .expect("one BundleEnd after the resume");
    assert_eq!(end.0, next, "every bundle byte was sent exactly once");
    assert_eq!(end.1, first_start.3, "the end digest is the start digest");

    let ends = count_matching(&home.frames(), |frame| matches!(frame, AgentFrame::BundleEnd { .. }));
    assert_eq!(ends, 1, "the dropped transfer never sent an end");
    let turn_ends = home
        .frames()
        .into_iter()
        .filter_map(|frame| match frame {
            AgentFrame::TurnEnd { summary, .. } => Some(summary),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(turn_ends.len(), 1, "one terminal summary after the bundle");
    assert_eq!(turn_ends[0].outcome, RunOutcome::Ok);

    let bundles = dir.path().join("run-bundles");
    assert!(
        !bundles.exists() || std::fs::read_dir(&bundles).expect("bundle dir").next().is_none(),
        "the local bundle staging file is removed after the summary is acknowledged"
    );
}

#[test]
fn a_home_bundle_abort_fails_the_run_without_chunks() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("worktree");
    let git = SystemGit::new();
    std::fs::create_dir_all(&repo).expect("repo");
    git_init(&git, &repo);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "base"]);
    let base = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);
    git_ok(&git, &repo, &["checkout", "-B", &format!("job/{JOB}")]);
    std::fs::write(repo.join("work.txt"), "work\n").expect("work file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "job work"]);
    let head = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);

    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        abort_bundle: Some("bundle_rejected: the reported head is foreign".to_owned()),
        ..ConnPlan::default()
    }]);
    let mut assignment = assignment_with_lease(LEASE);
    assignment.result_transport = ResultTransport::Bundle;
    assignment.remote_url = String::new();
    assignment.base_commit = base;
    home.set_assignment(assignment);
    let mut run_summary = summary();
    run_summary.head_commit = Some(head);
    run_summary.push_status = None;
    let handler = Arc::new(FakeHandler::new(
        home.clone(),
        HandlerBehavior {
            summary: run_summary,
            worktree: Some(repo),
            ..HandlerBehavior::default()
        },
    ));
    let dir = tempfile::tempdir().expect("state dir");
    state_dir(dir.path());
    let mut client = agent_client(dir.path(), home.clone());

    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");

    let frames = home.frames();
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::BundleChunk { .. })),
        0,
        "a refused start sends no chunk"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::BundleEnd { .. })),
        0
    );
    let summary = frames
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::TurnEnd { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("one terminal summary");
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert!(
        summary
            .result_text
            .as_deref()
            .is_some_and(|text| text.starts_with("bundle_rejected:")),
        "the abort reason is kept: {:?}",
        summary.result_text
    );
}

#[test]
fn bundle_summary_waits_for_a_delayed_accept() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("worktree");
    let git = SystemGit::new();
    std::fs::create_dir_all(&repo).expect("repo");
    git_init(&git, &repo);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "base"]);
    let base = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);
    git_ok(&git, &repo, &["checkout", "-B", &format!("job/{JOB}")]);
    std::fs::write(repo.join("work.txt"), "work\n").expect("work file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "job work"]);
    let head = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);

    let home = FakeHome::new(vec![ConnPlan {
        send_assignment: true,
        accept_bundle: true,
        // Several RUN_POLLs pass before the answer arrives; a summary sent on the first
        // empty poll would overtake the transfer.
        bundle_accept_after_polls: Some(3),
        ..ConnPlan::default()
    }]);
    let mut assignment = assignment_with_lease(LEASE);
    assignment.result_transport = ResultTransport::Bundle;
    assignment.remote_url = String::new();
    assignment.base_commit = base;
    home.set_assignment(assignment);
    let mut run_summary = summary();
    run_summary.head_commit = Some(head);
    run_summary.push_status = None;
    let handler = Arc::new(FakeHandler::new(
        home.clone(),
        HandlerBehavior {
            summary: run_summary,
            worktree: Some(repo),
            ..HandlerBehavior::default()
        },
    ));
    let dir = tempfile::tempdir().expect("state dir");
    state_dir(dir.path());
    let mut client = agent_client(dir.path(), home.clone());

    let outcome = run_client(&mut client, &handler);
    assert!(matches!(outcome, ClientOutcome::Revoked { .. }), "{outcome:?}");

    let frames = home.frames();
    let start = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::BundleStart { .. }))
        .expect("one BundleStart");
    let first_chunk = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::BundleChunk { .. }))
        .expect("the delayed accept still starts the chunks");
    let end = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::BundleEnd { .. }))
        .expect("one BundleEnd");
    let turn_end = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::TurnEnd { .. }))
        .expect("one terminal summary");
    assert!(start < first_chunk, "a chunk follows the start");
    assert!(first_chunk < end, "the end follows the chunks");
    assert!(
        end < turn_end,
        "the terminal summary waits for the complete bundle, not the first RUN_POLL"
    );
    assert_eq!(
        count_matching(&frames, |frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        1,
        "only one terminal summary"
    );
}

/// Initialises a test repository with a commit identity local to it.
///
/// CI runners have no global `user.name`/`user.email`, and the tests must never read the
/// owner's `~/.gitconfig`; the identity lives in the temporary repository's own config.
fn git_init(git: &SystemGit, repo: &Path) {
    git_ok(git, repo, &["init", "-b", "main"]);
    git_ok(git, repo, &["config", "user.name", "vowp test"]);
    git_ok(git, repo, &["config", "user.email", "vowp@example.invalid"]);
}

/// Runs git in `cwd`, asserting success.
fn git_ok(git: &SystemGit, cwd: &Path, args: &[&str]) {
    let output = git.run(cwd, args).expect("git");
    assert!(output.success(), "git {args:?} failed: {}", output.stderr);
}

/// Runs git in `cwd` and returns trimmed stdout.
fn git_stdout(git: &SystemGit, cwd: &Path, args: &[&str]) -> String {
    let output = git.run(cwd, args).expect("git");
    assert!(output.success(), "git {args:?} failed: {}", output.stderr);
    output.stdout.trim().to_owned()
}

#[test]
fn create_job_bundle_returns_one_ref_and_only_committed_objects() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path().join("worktree");
    let git = SystemGit::new();
    std::fs::create_dir_all(&repo).expect("repo");
    git_init(&git, &repo);
    std::fs::write(repo.join("base.txt"), "base\n").expect("base file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "base"]);
    let base = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);
    git_ok(&git, &repo, &["checkout", "-B", &format!("job/{JOB}")]);
    std::fs::write(repo.join("change.txt"), "change\n").expect("change file");
    git_ok(&git, &repo, &["add", "-A"]);
    git_ok(&git, &repo, &["commit", "-m", "job work"]);
    // A tag and a dirty file are allowed but neither belongs to the bundle.
    git_ok(&git, &repo, &["tag", "v-bundle"]);
    std::fs::write(repo.join("dirty.txt"), "not committed\n").expect("dirty file");
    let head = git_stdout(&git, &repo, &["rev-parse", "HEAD"]);
    let dest = dir.path().join("bundles");

    let bundle = vowp_agent::worktree::create_job_bundle(&git, &repo, &job(), &base, &dest).expect("bundle");

    assert_eq!(bundle.branch, format!("job/{JOB}"));
    assert_eq!(bundle.head, head);
    assert_eq!(
        bundle.bytes,
        std::fs::metadata(&bundle.path).expect("bundle file").len()
    );
    assert_eq!(bundle.sha256.len(), 64, "the digest is lowercase hex sha256");
    assert!(bundle.path.starts_with(&dest), "the file stays in the staging dir");

    let listed = git_stdout(
        &git,
        &repo,
        &["bundle", "list-heads", bundle.path.to_str().expect("bundle path")],
    );
    let lines = listed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "exactly one ref: {listed}");
    let parts = lines[0].split_whitespace().collect::<Vec<_>>();
    assert_eq!(parts[0], head);
    assert_eq!(parts[1], format!("refs/heads/job/{JOB}"));

    let bare = dir.path().join("checkout.git");
    git_ok(
        &git,
        dir.path(),
        &["init", "-q", "--bare", bare.to_str().expect("bare")],
    );
    git_ok(&git, &bare, &["fetch", "-q", repo.to_str().expect("repo"), &base]);
    git_ok(
        &git,
        &bare,
        &[
            "fetch",
            "-q",
            bundle.path.to_str().expect("bundle path"),
            &format!("refs/heads/job/{JOB}:refs/vowp/jobs/{JOB}"),
        ],
    );
    assert_eq!(
        git_stdout(&git, &bare, &["rev-parse", &format!("refs/vowp/jobs/{JOB}")]),
        head
    );
    assert_eq!(
        git_stdout(&git, &bare, &["show", &format!("{head}:change.txt")]),
        "change"
    );
    let dirty = git
        .run(&bare, &["cat-file", "-e", &format!("{head}:dirty.txt")])
        .expect("git");
    assert!(!dirty.success(), "an uncommitted file is not in the bundle");
}
