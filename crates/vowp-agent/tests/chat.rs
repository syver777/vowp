//! Local chat tests for the Mac-side agent client.
//!
//! The fake home and the fake adapter speak the public protocol traits in memory: no SSH,
//! network, store, harness process or real `~/.vowp-agent` is touched by a test.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, ChatClient, ChatTurnRequest, ChatTurnStatus, FrameRead,
    FrameStream, NodeKey, PairedToken, Spool, Transport,
};
use vowp_protocol::adapter::{Adapter, AdapterCapabilities, AdapterError, AdapterRun, ProbeResult, RunSummary};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentFrame, AssistantMessage, Cost, EventBody, EventId, Executor, HarnessEvent, HarnessKind,
    NodeName, PathRef, RemoteLine, RunId, RunOutcome, RunPurpose, SessionId, StartInput, StopReason, SurfaceId,
    Timestamp,
};

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

fn session() -> SessionId {
    SessionId::parse(SESSION).expect("session id")
}

fn run() -> RunId {
    RunId::parse(RUN).expect("run id")
}

fn node() -> NodeName {
    NodeName::parse("mac-studio").expect("node slug")
}

fn token() -> String {
    format!("vowp_surf_{}", "A".repeat(43))
}

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

/// How one fake connection answers frames.
#[derive(Debug, Clone)]
enum Behavior {
    /// Acknowledge every event.
    AckAll,
    /// Acknowledge the first N events, then drop the transport.
    DropAfter(usize),
    /// The first event is refused because the lease was taken over.
    LeaseRevoked,
    /// Check that the event is already in the spool when it arrives.
    SpoolCheck(PathBuf),
    /// Drop the connection when the first run line (the hello) arrives.
    DropOnFirstRemoteLine,
    /// Answer the first lease heartbeat with LeaseLost.
    LeaseLostOnHeartbeat,
    /// Send a Credit before acknowledging the first event.
    CreditBeforeAck,
}

#[derive(Default)]
struct FakeState {
    connections: usize,
    received: Vec<AgentFrame>,
    per_connection: Vec<Vec<AgentFrame>>,
    seqs: BTreeMap<String, u64>,
    next_seq: u64,
    runs: BTreeSet<String>,
}

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<FakeState>>,
    behaviors: Vec<Behavior>,
}

impl FakeHome {
    fn new(behaviors: Vec<Behavior>) -> Self {
        Self {
            shared: Arc::new(Mutex::new(FakeState::default())),
            behaviors,
        }
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
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = state.connections;
        state.connections += 1;
        state.per_connection.push(Vec::new());
        let behavior = self.behaviors.get(connection).cloned().unwrap_or(Behavior::AckAll);
        drop(state);
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            behavior,
            connection,
            outbound: VecDeque::new(),
            closed: false,
            drop_on_next_send: false,
            acked_events: 0,
            next_seq: 1,
            active_runs: BTreeSet::new(),
            dropped_initial: false,
            credit_sent: false,
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<FakeState>>,
    behavior: Behavior,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
    closed: bool,
    drop_on_next_send: bool,
    acked_events: usize,
    next_seq: u64,
    active_runs: BTreeSet<String>,
    dropped_initial: bool,
    credit_sent: bool,
}

impl FakeStream {
    fn record(&self, frame: &AgentFrame) {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.received.push(frame.clone());
        if let Some(connection) = state.per_connection.get_mut(self.connection) {
            connection.push(frame.clone());
        }
    }

    fn ack_event(&mut self, event: &vowp_protocol::RemoteEvent) {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = event.id.as_str().to_owned();
        let existing = state.seqs.get(&id).copied();
        let (seq, duplicate, next) = match existing {
            Some(seq) => (seq, true, state.next_seq),
            None => {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.seqs.insert(id, seq);
                (seq, false, state.next_seq)
            }
        };
        drop(state);
        self.next_seq = next;
        self.outbound.push_back(AgentFrame::IngestAck {
            run_id: run(),
            event_id: Some(event.id.clone()),
            seq: Some(seq),
            duplicate,
            lease_revoked: None,
            error: None,
        });
    }
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        if self.closed {
            return Ok(FrameRead::Closed);
        }
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Timeout),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        if self.closed {
            return Err(AgentError::Transport("connection closed".to_owned()));
        }
        if matches!(&self.behavior, Behavior::DropOnFirstRemoteLine)
            && !self.dropped_initial
            && matches!(frame, AgentFrame::RemoteLine { .. })
        {
            self.dropped_initial = true;
            self.closed = true;
            return Err(AgentError::Transport("connection dropped".to_owned()));
        }
        if self.drop_on_next_send {
            self.closed = true;
            return Err(AgentError::Transport("connection dropped".to_owned()));
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
            }
            AgentFrame::LeaseAcquire { .. } => {
                self.outbound.push_back(AgentFrame::LeaseGrant {
                    lease_token: "L".repeat(52),
                    expires_at: ts().plus(Duration::from_secs(60)),
                    tail_seq: 0,
                });
            }
            AgentFrame::TurnBegin { .. } => {
                let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.runs.insert(run().to_string());
                drop(state);
                self.active_runs.insert(run().to_string());
                self.outbound.push_back(AgentFrame::TurnGrant {
                    run_id: run(),
                    session: session(),
                    lease_token: "L".repeat(52),
                    expires_at: ts().plus(Duration::from_secs(60)),
                    tail_seq: 0,
                });
            }
            AgentFrame::LeaseHeartbeat { .. } => {
                if matches!(&self.behavior, Behavior::LeaseLostOnHeartbeat) {
                    self.outbound.push_back(AgentFrame::LeaseLost {
                        reason: "the lease was released or expired; this surface is read-only".to_owned(),
                    });
                }
            }
            AgentFrame::RemoteLine {
                run_id,
                line: RemoteLine::Hello { .. },
                ..
            } => {
                let known = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .runs
                    .contains(run_id.as_str());
                if known {
                    self.active_runs.insert(run_id.to_string());
                } else {
                    self.outbound.push_back(AgentFrame::RunLost {
                        run_id: run_id.clone(),
                        reason: "run is not active on this connection".to_owned(),
                    });
                }
            }
            AgentFrame::RemoteLine {
                run_id,
                line: RemoteLine::Event { event },
                ..
            } => {
                if !self.active_runs.contains(run_id.as_str()) {
                    self.outbound.push_back(AgentFrame::RunLost {
                        run_id: run_id.clone(),
                        reason: "run is not active on this connection".to_owned(),
                    });
                    return Ok(());
                }
                if let Behavior::SpoolCheck(path) = &self.behavior {
                    let text = std::fs::read_to_string(path).unwrap_or_default();
                    assert!(
                        text.contains("\"remote_line\""),
                        "event was not spooled before send: {text}"
                    );
                }
                if matches!(&self.behavior, Behavior::LeaseRevoked) {
                    self.outbound.push_back(AgentFrame::IngestAck {
                        run_id: run(),
                        event_id: Some(event.id.clone()),
                        seq: None,
                        duplicate: false,
                        lease_revoked: Some("this surface is read-only; taken over by phone".to_owned()),
                        error: None,
                    });
                    return Ok(());
                }
                if matches!(&self.behavior, Behavior::CreditBeforeAck) && !self.credit_sent {
                    self.credit_sent = true;
                    self.outbound.push_back(AgentFrame::Credit {
                        run_id: run(),
                        count: 64,
                    });
                }
                self.ack_event(event);
                self.acked_events += 1;
                if let Behavior::DropAfter(count) = &self.behavior
                    && self.acked_events >= *count
                {
                    self.drop_on_next_send = true;
                }
            }
            AgentFrame::TurnEnd { run_id, .. } => {
                if !self.active_runs.remove(run_id.as_str()) {
                    self.outbound.push_back(AgentFrame::RunLost {
                        run_id: run_id.clone(),
                        reason: "run is not active on this connection".to_owned(),
                    });
                    return Ok(());
                }
                let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let seq = state.next_seq;
                state.next_seq += 1;
                drop(state);
                self.outbound.push_back(AgentFrame::IngestAck {
                    run_id: run(),
                    event_id: None,
                    seq: Some(seq),
                    duplicate: false,
                    lease_revoked: None,
                    error: None,
                });
            }
            AgentFrame::LeaseRelease { .. } => {}
            _ => {}
        }
        Ok(())
    }
}

/// Writes the agent config, key, paired token and session binding for a test.
fn agent_state(dir: &Path) -> PathBuf {
    let config = AgentConfig::new("home-1", node(), dir.to_path_buf(), "home.example");
    config.write().expect("agent.json");
    NodeKey::load_or_generate(dir).expect("node key");
    let surface = SurfaceId(SURFACE.to_owned());
    let surface_dir = AgentConfig::surface_dir(dir, &surface);
    std::fs::create_dir_all(&surface_dir).expect("surface dir");
    for (name, text) in [
        (vowp_agent::SURFACE_TOKEN_FILE, token()),
        (vowp_agent::SURFACE_SESSION_FILE, SESSION.to_owned()),
        (
            vowp_agent::SURFACE_EXPIRY_FILE,
            ts().plus(Duration::from_secs(3600)).as_rfc3339(),
        ),
    ] {
        let path = surface_dir.join(name);
        std::fs::write(&path, text).expect("surface file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    dir.to_path_buf()
}

/// Builds a chat client over a fake transport.
fn chat_client(dir: &Path, home: FakeHome) -> ChatClient {
    let config = AgentConfig::load(dir).expect("agent config");
    let key = NodeKey::load(dir).expect("node key");
    let client = AgentClient::new(
        dir.to_path_buf(),
        key,
        config,
        Box::new(home),
        Arc::new(FixedClock(ts())),
    );
    let surface = PairedToken {
        surface_id: SurfaceId(SURFACE.to_owned()),
        token: token(),
    };
    ChatClient::new(client, surface, session())
}

fn request() -> ChatTurnRequest {
    ChatTurnRequest {
        executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
        prompt: "summarise this".to_owned(),
        cwd: std::env::temp_dir(),
        cwd_ref: PathRef::parse("project:vowp").expect("path ref"),
        job: None,
        purpose: RunPurpose::Start,
    }
}

/// Builds one assistant harness event.
fn event(text: &str) -> HarnessEvent {
    HarnessEvent {
        ts: Some(ts()),
        actor: vowp_protocol::Actor::Assistant,
        body: EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() }),
        raw: None,
    }
}

struct FakeAdapter {
    events: Vec<HarnessEvent>,
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl FakeAdapter {
    fn new(events: Vec<HarnessEvent>) -> Self {
        Self {
            events,
            started: Arc::new(AtomicUsize::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Adapter for FakeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("test".to_owned()),
            path: None,
            detail: "fake".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = std::sync::mpsc::channel();
        for event in &self.events {
            sender.send(event.clone()).expect("test channel");
        }
        drop(sender);
        Ok(Box::new(FakeRun {
            receiver,
            stopped: Arc::clone(&self.stopped),
        }))
    }
}

struct FakeRun {
    receiver: std::sync::mpsc::Receiver<HarnessEvent>,
    stopped: Arc<AtomicBool>,
}

impl AdapterRun for FakeRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &std::sync::mpsc::Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(RunSummary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: Some("ses_mac".to_owned()),
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
        })
    }
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

#[test]
fn agent_chat_spools_before_sending() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = agent_state(dir.path());
    let spool_path = vowp_agent::spool::spool_dir(&state, &session(), &run()).join(vowp_agent::EVENTS_FILE);
    let home = FakeHome::new(vec![Behavior::SpoolCheck(spool_path)]);
    let mut chat = chat_client(dir.path(), home.clone());
    let adapter = FakeAdapter::new(vec![event("spooled first")]);
    let mut printed = 0;
    let status = chat
        .turn(&request(), &adapter, &mut |_| printed += 1, None)
        .expect("turn");
    assert_eq!(status, ChatTurnStatus::Done);
    assert_eq!(printed, 1);
    let events = event_ids(&home.frames());
    assert_eq!(events.len(), 1);
    let spool = Spool::open(&state, &session(), &run()).expect("spool");
    assert!(spool.summary_acked());
}

#[test]
fn agent_chat_replays_from_the_first_unacked() {
    let dir = tempfile::tempdir().expect("temp dir");
    agent_state(dir.path());
    let home = FakeHome::new(vec![Behavior::DropAfter(1), Behavior::AckAll]);
    let mut chat = chat_client(dir.path(), home.clone());
    let adapter = FakeAdapter::new(vec![event("one"), event("two"), event("three")]);
    let status = chat.turn(&request(), &adapter, &mut |_| {}, None).expect("turn");
    assert_eq!(status, ChatTurnStatus::Done);
    assert_eq!(home.connections(), 2, "the transport was reopened once");
    let first = event_ids(&home.frames_on(0));
    let second = event_ids(&home.frames_on(1));
    assert_eq!(first.len(), 1, "the first event was acknowledged before the drop");
    assert_eq!(
        second.len(),
        2,
        "only the unacknowledged events were replayed: {second:?}"
    );
    assert!(!second.contains(&first[0]), "the acknowledged event is not replayed");
    assert!(
        home.frames_on(1).iter().any(|frame| matches!(
            frame,
            AgentFrame::RemoteLine {
                line: RemoteLine::Hello { .. },
                ..
            }
        )),
        "a replay starts with a fresh hello"
    );
    let spool = Spool::open(dir.path(), &session(), &run()).expect("spool");
    assert!(spool.unacked().expect("unacked").is_empty());
    assert!(spool.summary_acked());
}

#[test]
fn agent_chat_stops_when_the_initial_hello_reconnect_loses_the_lease() {
    let dir = tempfile::tempdir().expect("temp dir");
    agent_state(dir.path());
    let home = FakeHome::new(vec![Behavior::DropOnFirstRemoteLine, Behavior::LeaseLostOnHeartbeat]);
    let mut chat = chat_client(dir.path(), home.clone());
    let adapter = FakeAdapter::new(vec![event("must not run")]);
    let status = chat.turn(&request(), &adapter, &mut |_| {}, None).expect("turn");
    match status {
        ChatTurnStatus::ReadOnly { reason } => assert!(reason.contains("read-only"), "{reason}"),
        other => panic!("expected ReadOnly, got {other:?}"),
    }
    assert_eq!(home.connections(), 2, "the hello send was retried once");
    assert_eq!(
        adapter.started.load(Ordering::SeqCst),
        0,
        "the adapter never starts after a lost lease during the hello reconnect"
    );
    assert!(
        !home
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        "no summary is sent after the lease loss"
    );
}

#[test]
fn agent_chat_consumes_credit_by_refreshing_the_lease() {
    let dir = tempfile::tempdir().expect("temp dir");
    agent_state(dir.path());
    let home = FakeHome::new(vec![Behavior::CreditBeforeAck]);
    let mut chat = chat_client(dir.path(), home.clone());
    let adapter = FakeAdapter::new(vec![event("credited")]);
    let status = chat.turn(&request(), &adapter, &mut |_| {}, None).expect("turn");
    assert_eq!(status, ChatTurnStatus::Done);
    assert!(
        home.frames_on(0)
            .iter()
            .any(|frame| matches!(frame, AgentFrame::LeaseHeartbeat { .. })),
        "a Credit is confirmed with the run's lease heartbeat: {:?}",
        home.frames_on(0)
    );
}

#[test]
fn agent_chat_stops_on_lease_revoked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = agent_state(dir.path());
    let home = FakeHome::new(vec![Behavior::LeaseRevoked]);
    let mut chat = chat_client(dir.path(), home.clone());
    let adapter = FakeAdapter::new(vec![event("not accepted")]);
    let status = chat.turn(&request(), &adapter, &mut |_| {}, None).expect("turn");
    match status {
        ChatTurnStatus::ReadOnly { reason } => assert!(reason.contains("taken over"), "{reason}"),
        other => panic!("expected ReadOnly, got {other:?}"),
    }
    assert!(adapter.stopped.load(Ordering::SeqCst), "the adapter was stopped");
    assert!(
        !home
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::TurnEnd { .. })),
        "no summary is sent after the notice"
    );
    let spool = Spool::open(&state, &session(), &run()).expect("spool is kept");
    assert_eq!(spool.unacked_events().expect("unacked"), 1);
}

#[test]
fn agent_chat_stops_on_spool_full() {
    let dir = tempfile::tempdir().expect("temp dir");
    agent_state(dir.path());
    let home = FakeHome::new(vec![Behavior::AckAll]);
    let mut chat = chat_client(dir.path(), home.clone()).with_spool_cap(1);
    let adapter = FakeAdapter::new(vec![event("too large for the cap")]);
    let status = chat.turn(&request(), &adapter, &mut |_| {}, None).expect("turn");
    assert!(matches!(status, ChatTurnStatus::SpoolFull { .. }), "{status:?}");
    assert!(adapter.stopped.load(Ordering::SeqCst), "the adapter was stopped");
    assert!(
        event_ids(&home.frames()).is_empty(),
        "an event that was not durably spooled is never sent"
    );
}

#[test]
fn chat_requires_a_paired_surface_for_the_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    agent_state(dir.path());
    // Remove the session binding from every paired surface.
    for entry in std::fs::read_dir(AgentConfig::surfaces_dir(dir.path())).expect("surfaces") {
        let entry = entry.expect("entry");
        let _ = std::fs::remove_file(entry.path().join(vowp_agent::SURFACE_SESSION_FILE));
    }
    let result = ChatClient::from_state_dir(dir.path().to_path_buf(), session(), Arc::new(FixedClock(ts())));
    assert!(matches!(result, Err(AgentError::NotEnrolled(_))));
}

#[test]
fn spool_keeps_unacked_events_and_records_acks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state = agent_state(dir.path());
    let mut spool = Spool::open(&state, &session(), &run()).expect("spool");
    let frame = AgentFrame::RemoteLine {
        run_id: run(),
        epoch: 1,
        line: RemoteLine::Event {
            event: vowp_protocol::RemoteEvent {
                id: EventId::new(),
                ts: ts(),
                event_type: vowp_protocol::EventType::AssistantMessage,
                actor: vowp_protocol::Actor::Assistant,
                source: vowp_protocol::EventSource {
                    harness: HarnessKind::Opencode,
                    model: None,
                    node: Some(node()),
                    native_session_id: None,
                },
                payload: serde_json::json!({"text": "hello"}),
                raw: None,
                replayable: true,
            },
        },
    };
    spool.append(&frame).expect("append");
    assert_eq!(spool.unacked().expect("unacked").len(), 1);
    let event_id = match &frame {
        AgentFrame::RemoteLine {
            line: RemoteLine::Event { event },
            ..
        } => event.id.as_str().to_owned(),
        _ => unreachable!(),
    };
    spool.mark_acked(&event_id, 1).expect("ack");
    assert!(spool.unacked().expect("unacked").is_empty());
    assert!(spool.dir().ends_with(format!("{SESSION}/{RUN}")));
}

#[test]
fn error_kind_names_the_missing_pairing() {
    let error = AgentError::NotEnrolled(format!(
        "no paired surface for session {SESSION}; run `vowp agent pair` first"
    ));
    assert!(error.to_string().contains("vowp agent pair"));
    assert!(error.is_fatal());
}
