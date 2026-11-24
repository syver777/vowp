//! Phase 0 home gateway tests.
//!
//! Every test uses a temporary node home and an in-memory duplex; no test opens the real
//! `~/.vowp`, starts tmux, touches the network or signs with a real node key.

mod support;

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use support::fake_git::FakeGit;
use tempfile::TempDir;
use vowp_core::agent::enroll::replace_pending;
use vowp_core::agent::gateway::{self, GatewayOptions, GatewayOutcome};
use vowp_core::agent::{
    AgentFrameSpool, EnrollRequest, RotationPolicy, SURFACE_TOKEN_OVERLAP, SURFACE_TOKEN_TTL, auth, create_pending,
    create_pending_surface, hash_code, issue_surface_token, redeem, replace_pending_surface, revoke, revoke_surface,
    rotate_surface_token, rotate_surface_with_policy, surface_list_row, verify_surface_token,
};
use vowp_core::surface::orchestrator::{CommandSpool, NewOrchestrator, Orchestrator, OrchestratorTransport};
use vowp_core::{
    AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, CoreError, Deps, FixedRng, SystemGit, SystemRunner,
};
use vowp_protocol::agent::surface::{
    SurfaceAttach, SurfaceDetach, SurfaceEvent, SurfaceTurn, SurfaceTurnState, ToolCall as SurfaceToolCall,
    ToolError as SurfaceToolError,
};
use vowp_protocol::{
    Actor, AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, Clock, Cost, ErrorKind, EventBody, EventId,
    EventSource, EventType, FakeClock, HarnessKind, JobId, JobKind, JobState, LeaseMode, LedgerKind, Level, MemoryLog,
    NewJob, NodeName, NodePresence, NodeStatus, OrchestratorApp, OrchestratorNodeState, OrchestratorResult, PathRef,
    Placement, Project, ProjectName, RemoteEvent, RemoteLine, RemoteRunState, Role, RunId, RunOutcome, RunPurpose,
    SecretName, Session, SessionId, SessionRole, SessionState, StopReason, SurfaceId, SurfaceKind, SurfaceScope,
    SurfaceState, Timestamp, TurnSummary, VowpConfig,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{LedgerFilter, NodePath, NodePaths, StateChange, Store, hash_lease_token};

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7_u8; 32])
}

fn public_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn signature_hex(key: &SigningKey, nonce: &str, home: &str, connection: &str) -> String {
    key.sign(&auth::handshake_payload(nonce, home, connection))
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn capabilities() -> AgentCapabilities {
    let mut harnesses = std::collections::BTreeMap::new();
    harnesses.insert("opencode".to_owned(), "1.0.0".to_owned());
    AgentCapabilities {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.3.0".to_owned(),
        harnesses,
        git_version: Some("git version 2.47.0".to_owned()),
        labels: vec!["test".to_owned()],
        secret_names: vec![SecretName::parse("BUILD_TOKEN").expect("secret name")],
        projects: vec![ProjectName::parse("vowp").expect("project")],
    }
}

fn test_deps(root: &Path, clock: Arc<FakeClock>, log: Arc<MemoryLog>) -> Deps {
    test_deps_with_config(root, clock, log, VowpConfig::with_defaults_for_tests())
}

fn test_deps_with_config(root: &Path, clock: Arc<FakeClock>, log: Arc<MemoryLog>, config: VowpConfig) -> Deps {
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(config),
        node: NodeName::parse("home-1").expect("home node"),
        clock,
        log,
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(FakeGit::new().with_head_sha("0123456789abcdef0123456789abcdef01234567")),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

fn test_store(root: &Path, clock: &Arc<FakeClock>) -> Store {
    Store::open(&NodePaths::new(root))
        .expect("store opens")
        .with_clock(clock.clone())
}

fn node_name(name: &str) -> NodeName {
    NodeName::parse(name).expect("node slug")
}

fn seed_session(store: &Store, node: &NodeName, project: &str, id: &str, now: Timestamp) -> SessionId {
    let id = SessionId::parse(id).expect("session id");
    store
        .upsert_session(&Session {
            id: id.clone(),
            project: Some(ProjectName::parse(project).expect("project")),
            job_id: None,
            role: Some(SessionRole::User),
            title: "surface pairing".to_owned(),
            parent_session_id: None,
            node: node.clone(),
            cwd: "/tmp/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        })
        .expect("session");
    id
}

fn hello(name: &NodeName, key: &SigningKey) -> AgentFrame {
    AgentFrame::AgentHello {
        node: name.clone(),
        key_id: public_hex(key),
        agent_version: "0.3.0".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        labels: vec!["test".to_owned()],
        capabilities: capabilities(),
    }
}

fn enroll_frame(code: &str, key: &SigningKey) -> AgentFrame {
    AgentFrame::Enroll {
        code: code.to_owned(),
        public_key: public_hex(key),
        agent_version: "0.3.0".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        labels: vec!["test".to_owned()],
        capabilities: capabilities(),
        paths: vec![AgentPathMap {
            project: ProjectName::parse("vowp").expect("project"),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: None,
            allow: Vec::new(),
        }],
    }
}

/// Creates a pending node and redeems its code directly, leaving an active credential.
fn activate(root: &Path, clock: &Arc<FakeClock>, name: &NodeName, key: &SigningKey) {
    let store = test_store(root, clock);
    let pending = create_pending(&store, name, &["darwin-arm64".to_owned()], clock.now()).expect("pending node");
    let request = EnrollRequest {
        code: pending.code,
        public_key: public_hex(key),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    redeem(&store, &request, clock.now()).expect("redeem code");
}

/// Registers the reserved `_node` project and every named project, creating their databases.
///
/// This is the registry state a multi-project home has after `vowp init` and
/// `vowp project add`.
fn register_projects(root: &Path, clock: &Arc<FakeClock>, projects: &[&str]) {
    let paths = NodePaths::new(root);
    let store = Store::open_global(&paths)
        .expect("global store")
        .with_clock(clock.clone());
    let now = clock.now();
    let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT).expect("node project");
    if store.project(&node_project).expect("node project lookup").is_none() {
        store
            .upsert_project(&Project {
                name: node_project.clone(),
                path: paths.root().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("node project row");
    }
    Store::create_project_db(&paths, &node_project).expect("node project database");
    for name in projects {
        let project = ProjectName::parse(name).expect("project slug");
        let checkout = root.join(format!("{name}-checkout"));
        std::fs::create_dir_all(&checkout).expect("project checkout");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(checkout.display().to_string());
        std::fs::write(
            checkout.join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config serialises"),
        )
        .expect("vowp.toml");
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: checkout.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        Store::create_project_db(&paths, &project).expect("project database");
    }
}

/// Creates a session row for one project and an empty log file.
fn session_in(
    root: &Path,
    store: &Store,
    node: &NodeName,
    project: Option<&str>,
    id: &str,
    clock: &Arc<FakeClock>,
) -> SessionId {
    let id = SessionId::parse(id).expect("session id");
    let now = clock.now();
    store
        .upsert_session(&Session {
            id: id.clone(),
            project: project.map(|name| ProjectName::parse(name).expect("project")),
            job_id: None,
            role: Some(SessionRole::User),
            title: "multi-project chat".to_owned(),
            parent_session_id: None,
            node: node.clone(),
            cwd: "/tmp/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        })
        .expect("session");
    let log = SessionLog::create(&NodePaths::new(root), &id, clock.clone()).expect("session log");
    log.sync().expect("sync empty log");
    id
}

/// One controllable in-memory reader.
#[derive(Clone)]
struct TestReader {
    state: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
    closed: Arc<AtomicBool>,
}

impl TestReader {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn push(&self, frame: &AgentFrame) {
        let mut line = serde_json::to_vec(frame).expect("frame JSON");
        line.push(b'\n');
        let (queue, ready) = &*self.state;
        let mut queue = queue.lock().expect("input lock");
        queue.extend(line);
        ready.notify_all();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let (_, ready) = &*self.state;
        ready.notify_all();
    }
}

impl Read for TestReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let (queue, ready) = &*self.state;
        let mut queue = queue.lock().expect("input lock");
        while queue.is_empty() && !self.closed.load(Ordering::SeqCst) {
            queue = ready.wait(queue).expect("input wait");
        }
        if queue.is_empty() {
            return Ok(0);
        }
        let count = buf.len().min(queue.len());
        for slot in buf.iter_mut().take(count) {
            *slot = queue.pop_front().expect("queued byte");
        }
        Ok(count)
    }
}

/// One shared writer that tests can inspect without consuming it.
#[derive(Clone, Default)]
struct TestWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TestWriter {
    fn frames(&self) -> Vec<AgentFrame> {
        let bytes = self.bytes.lock().expect("output lock");
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn wait_frame(&self, predicate: impl Fn(&AgentFrame) -> bool, timeout: Duration) -> AgentFrame {
        self.wait_frame_after(0, predicate, timeout)
    }

    fn wait_frame_after(&self, start: usize, predicate: impl Fn(&AgentFrame) -> bool, timeout: Duration) -> AgentFrame {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.frames().into_iter().skip(start).find(|frame| predicate(frame)) {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a frame; saw {:?}",
                self.frames()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().expect("output lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One running gateway connection.
struct Gateway {
    input: TestReader,
    output: TestWriter,
    log: Arc<MemoryLog>,
    clock: Arc<FakeClock>,
    root: std::path::PathBuf,
    handle: Option<JoinHandle<Result<GatewayOutcome, CoreError>>>,
}

impl Gateway {
    fn start(root: &Path, clock: Arc<FakeClock>) -> Self {
        Self::start_with_config(root, clock, VowpConfig::with_defaults_for_tests())
    }

    fn start_with_config(root: &Path, clock: Arc<FakeClock>, config: VowpConfig) -> Self {
        let log = Arc::new(MemoryLog::new());
        let deps = test_deps_with_config(root, clock.clone(), log.clone(), config);
        Self::start_with_deps(root, clock, log, deps)
    }

    fn start_with_deps(root: &Path, clock: Arc<FakeClock>, log: Arc<MemoryLog>, deps: Deps) -> Self {
        let store = deps.open_store().expect("bound store");
        Self::start_with_store(root, clock, log, deps, store)
    }

    /// Starts one connection over an explicit store handle.
    ///
    /// The multi-project tests serve `Store::open_global` while one project still owns the
    /// run and job rows.
    fn start_with_store(root: &Path, clock: Arc<FakeClock>, log: Arc<MemoryLog>, deps: Deps, store: Store) -> Self {
        let input = TestReader::new();
        let output = TestWriter::default();
        let reader = BufReader::new(input.clone());
        let writer = output.clone();
        let options = GatewayOptions {
            home_id: "home-1".to_owned(),
            heartbeat_secs: 10,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(2),
        };
        let handle = thread::spawn(move || gateway::serve_with_options(&store, &deps, reader, writer, options));
        Self {
            input,
            output,
            log,
            clock,
            root: root.to_path_buf(),
            handle: Some(handle),
        }
    }

    /// Starts one connection over the home's global store.
    fn start_global(root: &Path, clock: Arc<FakeClock>) -> Self {
        let log = Arc::new(MemoryLog::new());
        let deps = test_deps(root, clock.clone(), log.clone());
        let store = Store::open_global(&NodePaths::new(root))
            .expect("global store")
            .with_clock(clock.clone());
        Self::start_with_store(root, clock, log, deps, store)
    }

    fn store(&self) -> Store {
        let log = Arc::new(MemoryLog::new());
        test_deps(&self.root, self.clock.clone(), log)
            .open_store()
            .expect("bound store")
    }

    fn send(&self, frame: &AgentFrame) {
        self.input.push(frame);
    }

    fn expect(&self, predicate: impl Fn(&AgentFrame) -> bool) -> AgentFrame {
        self.output.wait_frame(predicate, Duration::from_secs(2))
    }

    /// Waits up to `timeout` for a frame matching `predicate`.
    fn expect_within(&self, predicate: impl Fn(&AgentFrame) -> bool, timeout: Duration) -> AgentFrame {
        self.output.wait_frame(predicate, timeout)
    }

    fn frame_count(&self) -> usize {
        self.output.frames().len()
    }

    fn expect_after(&self, start: usize, predicate: impl Fn(&AgentFrame) -> bool) -> AgentFrame {
        self.output.wait_frame_after(start, predicate, Duration::from_secs(2))
    }

    fn finish(mut self) -> Result<GatewayOutcome, CoreError> {
        self.input.close();
        self.handle
            .take()
            .expect("gateway thread")
            .join()
            .expect("gateway thread panicked")
    }
}

fn authenticate(gateway: &Gateway, name: &NodeName, key: &SigningKey) {
    authenticate_within(gateway, name, key, Duration::from_secs(2));
}

/// Authenticates one connection, waiting up to `timeout` for each handshake frame.
///
/// Most tests keep the short default; the heartbeat dispatch test passes the same
/// generous bound as its dispatch wait because a workspace gate runs this suite
/// beside every other test binary and the gateway thread can be starved between
/// the challenge and the welcome.
fn authenticate_within(gateway: &Gateway, name: &NodeName, key: &SigningKey, timeout: Duration) {
    gateway.send(&hello(name, key));
    let AgentFrame::HomeChallenge {
        nonce,
        home_id,
        connection_id,
        ..
    } = gateway.expect_within(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }), timeout)
    else {
        panic!("expected HomeChallenge");
    };
    let signature = signature_hex(key, &nonce, &home_id, &connection_id);
    gateway.send(&AgentFrame::AgentAuth { signature });
    gateway.expect_within(|frame| matches!(frame, AgentFrame::HomeWelcome { .. }), timeout);
}

/// Longest a test waits for one gateway dispatch pass on a loaded host.
///
/// The pass itself takes milliseconds when the host is idle. A workspace gate
/// runs this suite beside every other test binary and other agents, and a
/// deliberately single-core loaded run took 2.30 s, so a two-second budget was
/// too tight. This is a hang guard, not a paced delay: the waits below finish as
/// soon as their observable condition holds.
const GATEWAY_WAIT: Duration = Duration::from_secs(30);

fn wait_until(condition: impl FnMut() -> bool) {
    wait_until_for(condition, Duration::from_secs(2));
}

/// Waits up to `timeout` for `condition`, polling every five milliseconds.
fn wait_until_for(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "condition was not met in time");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Pairs one surface and returns its id and plaintext surface token.
fn pair_surface(store: &Store, node: &NodeName, session: &SessionId, app: &str, now: Timestamp) -> (SurfaceId, String) {
    let pending =
        create_pending_surface(store, node, session, SurfaceKind::Opencode, Some(app), now).expect("pending surface");
    let issued = issue_surface_token(store, node, &pending.surface.id, session, app, &pending.code, now)
        .expect("issued surface token");
    (issued.surface_id, issued.token)
}

/// Sends one `LeaseAcquire` frame for a paired surface.
fn acquire_frame(surface: &SurfaceId, session: &SessionId, token: &str, mode: LeaseMode) -> AgentFrame {
    AgentFrame::LeaseAcquire {
        surface: surface.clone(),
        session: session.clone(),
        surface_token: token.to_owned(),
        mode,
    }
}

/// Reads the first `LeaseGrant` after `start`.
fn expect_grant(gateway: &Gateway, start: usize) -> (String, Timestamp, u64) {
    let AgentFrame::LeaseGrant {
        lease_token,
        expires_at,
        tail_seq,
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseGrant { .. }))
    else {
        panic!("expected LeaseGrant");
    };
    (lease_token, expires_at, tail_seq)
}

/// Creates a session row and an empty log file so `begin_turn` can append.
fn chat_session(root: &Path, store: &Store, node: &NodeName, id: &str, clock: &Arc<FakeClock>) -> SessionId {
    let session = seed_session(store, node, "vowp", id, clock.now());
    let log = SessionLog::create(&NodePaths::new(root), &session, clock.clone()).expect("session log");
    log.sync().expect("sync empty log");
    session
}

/// Builds a `TurnBegin` for the paired surface.
fn turn_begin_frame(surface: &SurfaceId, session: &SessionId, lease: &str, cwd_ref: PathRef) -> AgentFrame {
    AgentFrame::TurnBegin {
        surface: surface.clone(),
        session: session.clone(),
        lease_token: lease.to_owned(),
        prompt: "hello from the Mac".to_owned(),
        executor: "opencode:deepseek/deepseek-flash#max".to_owned(),
        cwd_ref,
        job: None,
        purpose: RunPurpose::Start,
    }
}

/// Reads the first `TurnGrant` after `start`.
fn expect_turn_grant(gateway: &Gateway, start: usize) -> (RunId, SessionId, String, u64) {
    let AgentFrame::TurnGrant {
        run_id,
        session,
        lease_token,
        tail_seq,
        ..
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::TurnGrant { .. }))
    else {
        panic!("expected TurnGrant");
    };
    (run_id, session, lease_token, tail_seq)
}

/// Builds the first `RemoteLine` of a run.
fn hello_line(session: &SessionId, run: &RunId, lease: &str) -> RemoteLine {
    RemoteLine::Hello {
        session_id: session.clone(),
        run_id: run.clone(),
        executor: "opencode:deepseek/deepseek-flash#max".to_owned(),
        node: node_name("mac-studio"),
        spool_id: "test-spool".to_owned(),
        lease: lease.to_owned(),
    }
}

/// Builds one assistant event with a fixed body.
fn assistant_event(id: &EventId, text: &str) -> RemoteEvent {
    let body = EventBody::AssistantMessage(vowp_protocol::AssistantMessage { text: text.to_owned() });
    RemoteEvent {
        id: id.clone(),
        ts: ts(),
        event_type: body.event_type(),
        actor: Actor::Assistant,
        source: EventSource {
            harness: HarnessKind::Opencode,
            model: Some("deepseek/deepseek-flash".to_owned()),
            node: Some(node_name("mac-studio")),
            native_session_id: None,
        },
        payload: body.to_value(),
        raw: None,
        replayable: true,
    }
}

/// Builds an event line for a run.
fn event_line(run: &RunId, event: RemoteEvent) -> AgentFrame {
    AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: RemoteLine::Event { event },
    }
}

/// Builds the terminal `TurnEnd` for a run.
fn turn_end_frame(run: &RunId) -> AgentFrame {
    AgentFrame::TurnEnd {
        run_id: run.clone(),
        summary: TurnSummary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: Some("ses_mac".to_owned()),
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
            usage: None,
        },
    }
}

/// Reads one session's canonical log.
fn read_log(root: &Path, session: &SessionId) -> Vec<vowp_protocol::EventEnvelope> {
    SessionLog::open_read(&NodePaths::new(root), session)
        .expect("open session log")
        .read_all()
        .expect("read session log")
}

/// Builds a `SurfaceAttach` frame.
fn attach_frame(surface: &SurfaceId, session: &SessionId) -> AgentFrame {
    AgentFrame::SurfaceAttach(SurfaceAttach {
        surface: surface.clone(),
        session: session.clone(),
    })
}

/// Builds a `SurfaceDetach` frame.
fn detach_frame(surface: &SurfaceId, session: &SessionId) -> AgentFrame {
    AgentFrame::SurfaceDetach(SurfaceDetach {
        surface: surface.clone(),
        session: session.clone(),
    })
}

/// Builds a `SurfaceTurn` boundary frame.
fn surface_turn_frame(
    surface: &SurfaceId,
    session: &SessionId,
    native_id: &str,
    state: SurfaceTurnState,
    at: Timestamp,
) -> AgentFrame {
    AgentFrame::SurfaceTurn(SurfaceTurn {
        surface: surface.clone(),
        session: session.clone(),
        native_id: native_id.to_owned(),
        state,
        at,
    })
}

/// Builds a `SurfaceEvent` frame.
fn surface_event_frame(surface: &SurfaceId, session: &SessionId, native_id: &str, body: EventBody) -> AgentFrame {
    AgentFrame::SurfaceEvent(SurfaceEvent {
        surface: surface.clone(),
        session: session.clone(),
        native_id: native_id.to_owned(),
        event: body,
    })
}

/// Builds a proxied `ToolCall` frame.
fn tool_call_frame(
    surface: &SurfaceId,
    session: &SessionId,
    request_id: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> AgentFrame {
    AgentFrame::ToolCall(SurfaceToolCall {
        request_id: request_id.to_owned(),
        surface: surface.clone(),
        session: session.clone(),
        tool: tool.to_owned(),
        arguments,
    })
}

/// Reads one `ToolResult` after `start`.
fn expect_tool_result(
    gateway: &Gateway,
    start: usize,
) -> (String, Option<serde_json::Value>, Option<SurfaceToolError>) {
    let AgentFrame::ToolResult(result) =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::ToolResult(_)))
    else {
        panic!("expected ToolResult");
    };
    (result.request_id, result.result, result.error)
}

/// Returns whether an MCP content envelope is a successful tool result.
fn tool_succeeded(result: &serde_json::Value) -> bool {
    result.get("isError").and_then(serde_json::Value::as_bool) == Some(false)
        && result
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| !items.is_empty())
}

/// Returns the session's event types in log order.
fn log_types(root: &Path, session: &SessionId) -> Vec<EventType> {
    read_log(root, session).iter().map(|event| event.event_type).collect()
}

/// Reads a project's run row from its bound database.
fn project_run(root: &Path, clock: &Arc<FakeClock>, project: &str, run: &RunId) -> vowp_protocol::Run {
    let project = ProjectName::parse(project).expect("project");
    Store::open_with_project(&NodePaths::new(root), &project)
        .expect("project store")
        .with_clock(clock.clone())
        .require_run(run)
        .expect("run row")
}

/// Waits for an `IngestAck` after `start`.
fn expect_ack(
    gateway: &Gateway,
    start: usize,
) -> (
    RunId,
    Option<EventId>,
    Option<u64>,
    bool,
    Option<String>,
    Option<String>,
) {
    let AgentFrame::IngestAck {
        run_id,
        event_id,
        seq,
        duplicate,
        lease_revoked,
        error,
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::IngestAck { .. }))
    else {
        panic!("expected IngestAck");
    };
    (run_id, event_id, seq, duplicate, lease_revoked, error)
}

#[test]
fn gateway_enrolls_a_node_with_a_valid_code() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    let store = test_store(home.path(), &clock);
    let pending = create_pending(&store, &name, &["darwin-arm64".to_owned()], clock.now()).expect("pending node");

    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&enroll_frame(&pending.code, &key));
    let AgentFrame::Enrolled {
        home_id,
        node,
        protocol,
        heartbeat_secs,
        ..
    } = gateway.expect(|frame| matches!(frame, AgentFrame::Enrolled { .. }))
    else {
        panic!("expected Enrolled");
    };
    assert_eq!(home_id, "home-1");
    assert_eq!(node, name);
    assert_eq!(protocol, vowp_protocol::AGENT_PROTOCOL);
    assert_eq!(heartbeat_secs, 10);
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Enrolled { node: name.clone() }
    );

    let store = test_store(home.path(), &clock);
    let card = store.node_card(&name, clock.now()).expect("card").expect("node exists");
    assert_eq!(card.status, NodeStatus::Active);
    assert_eq!(card.kind, vowp_protocol::NodeKind::Agent);
    assert_eq!(card.labels, vec!["darwin-arm64".to_owned()]);
    let credential = store
        .node_credential(&name, &public_hex(&key))
        .expect("credential")
        .expect("credential exists");
    assert_eq!(credential.public_key, public_hex(&key));
    assert!(credential.revoked_at.is_none());
    assert!(
        store
            .consume_enrollment_code(&hash_code(&pending.code), clock.now())
            .expect("code lookup")
            .is_none(),
        "the code must be single-use"
    );
    assert_eq!(store.list_node_paths(&name).expect("paths").len(), 1);
}

#[test]
fn gateway_refuses_an_expired_or_used_code() {
    let key = signing_key();

    // Expired: the ten-minute window has passed.
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("expired-node");
    let store = test_store(home.path(), &clock);
    let pending = create_pending(&store, &name, &[], clock.now()).expect("pending node");
    clock.advance(Duration::from_secs(601));
    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&enroll_frame(&pending.code, &key));
    let AgentFrame::Error { retryable, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert!(!retryable);
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed { node: None }
    );

    // Used: the same code was already redeemed.
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("used-node");
    let store = test_store(home.path(), &clock);
    let pending = create_pending(&store, &name, &[], clock.now()).expect("pending node");
    let request = EnrollRequest {
        code: pending.code.clone(),
        public_key: public_hex(&key),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    redeem(&store, &request, clock.now()).expect("first redemption");
    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&enroll_frame(&pending.code, &key));
    let AgentFrame::Error { retryable, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert!(!retryable);
    gateway.finish().expect("gateway");
}

#[test]
fn gateway_auth_rejects_a_bad_signature_and_a_replayed_nonce() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);

    // A signature over the wrong bytes is refused.
    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&hello(&name, &key));
    let AgentFrame::HomeChallenge {
        home_id, connection_id, ..
    } = gateway.expect(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }))
    else {
        panic!("expected HomeChallenge");
    };
    let bad = signature_hex(&key, "another-nonce", &home_id, &connection_id);
    gateway.send(&AgentFrame::AgentAuth { signature: bad });
    let AgentFrame::Error { kind, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Auth);
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed { node: None }
    );

    // A signature captured on one challenge cannot authenticate a new connection.
    let first = Gateway::start(home.path(), clock.clone());
    first.send(&hello(&name, &key));
    let AgentFrame::HomeChallenge {
        nonce: first_nonce,
        home_id: first_home,
        connection_id: first_connection,
        ..
    } = first.expect(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }))
    else {
        panic!("expected first HomeChallenge");
    };
    let captured = signature_hex(&key, &first_nonce, &first_home, &first_connection);
    first.send(&AgentFrame::AgentAuth {
        signature: captured.clone(),
    });
    first.expect(|frame| matches!(frame, AgentFrame::HomeWelcome { .. }));
    first.finish().expect("first gateway");

    let replay = Gateway::start(home.path(), clock.clone());
    replay.send(&hello(&name, &key));
    let AgentFrame::HomeChallenge {
        nonce: replay_nonce, ..
    } = replay.expect(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }))
    else {
        panic!("expected replay HomeChallenge");
    };
    assert_ne!(replay_nonce, first_nonce, "each connection gets a fresh nonce");
    replay.send(&AgentFrame::AgentAuth { signature: captured });
    let AgentFrame::Error { kind, .. } = replay.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Auth);
    replay.finish().expect("replay gateway");
}

#[test]
fn gateway_heartbeat_updates_presence_and_capacity() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let capacity = vowp_protocol::AgentCapacity {
        cpu_cores: 12,
        load1: 0.5,
        mem_total_bytes: 32 * 1024 * 1024 * 1024,
        mem_free_bytes: 16 * 1024 * 1024 * 1024,
        disk_free_bytes: 200 * 1024 * 1024 * 1024,
        busy_slots: 1,
        max_slots: 4,
        on_battery: Some(false),
        thermal_state: Some("nominal".to_owned()),
    };
    gateway.send(&AgentFrame::Heartbeat {
        at: clock.now(),
        capacity: capacity.clone(),
        pending_spool_bytes: 42,
    });

    let store = gateway.store();
    wait_until(|| {
        store
            .node_card(&name, clock.now())
            .expect("card")
            .is_some_and(|card| card.capacity.is_some())
    });
    let card = store.node_card(&name, clock.now()).expect("card").expect("node");
    assert_eq!(card.presence, NodePresence::Online);
    assert_eq!(card.capacity, Some(capacity.clone()));
    assert_eq!(card.agent_version.as_deref(), Some("0.3.0"));
    assert_eq!(card.protocol.as_deref(), Some(vowp_protocol::AGENT_PROTOCOL));

    // Presence is computed from the home clock; an old heartbeat goes offline.
    clock.advance(Duration::from_secs(100));
    let card = store.node_card(&name, clock.now()).expect("card").expect("node");
    assert_eq!(card.presence, NodePresence::Offline);

    // A new heartbeat brings it back and replaces the capacity snapshot.
    let mut updated = capacity.clone();
    updated.busy_slots = 2;
    gateway.send(&AgentFrame::Heartbeat {
        at: clock.now(),
        capacity: updated.clone(),
        pending_spool_bytes: 0,
    });
    wait_until(|| {
        store
            .node_card(&name, clock.now())
            .expect("card")
            .is_some_and(|card| card.capacity.as_ref() == Some(&updated))
    });
    let card = store.node_card(&name, clock.now()).expect("card").expect("node");
    assert_eq!(card.presence, NodePresence::Online);
    assert_eq!(card.capacity, Some(updated));
    gateway.finish().expect("gateway");
}

#[test]
fn gateway_revoke_marks_and_closes() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let store = gateway.store();
    assert_eq!(revoke(&store, &name, clock.now()).expect("revoke"), 1);

    gateway.send(&AgentFrame::Heartbeat {
        at: clock.now(),
        capacity: vowp_protocol::AgentCapacity {
            cpu_cores: 4,
            load1: 0.1,
            mem_total_bytes: 1024,
            mem_free_bytes: 512,
            disk_free_bytes: 1024,
            busy_slots: 0,
            max_slots: 1,
            on_battery: None,
            thermal_state: None,
        },
        pending_spool_bytes: 0,
    });
    let AgentFrame::Revoked { reason } = gateway.expect(|frame| matches!(frame, AgentFrame::Revoked { .. })) else {
        panic!("expected Revoked");
    };
    assert!(reason.contains("revoked"));
    gateway.finish().expect("gateway");

    let card = store.node_card(&name, clock.now()).expect("card").expect("node");
    assert_eq!(card.status, NodeStatus::Revoked);
    assert_eq!(card.presence, NodePresence::Revoked);
    let credential = store
        .node_credential(&name, &public_hex(&key))
        .expect("credential")
        .expect("credential exists");
    assert!(credential.revoked_at.is_some());
}

#[test]
fn gateway_unknown_or_revoked_node_is_refused() {
    let key = signing_key();

    // Unknown: no node row at all.
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("ghost");
    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&hello(&name, &key));
    let AgentFrame::Error { kind, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::NotFound);
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed { node: None }
    );

    // Revoked: the node exists but its status is revoked.
    let name = node_name("revoked-node");
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    revoke(&store, &name, clock.now()).expect("revoke");
    let gateway = Gateway::start(home.path(), clock.clone());
    gateway.send(&hello(&name, &key));
    gateway.expect(|frame| matches!(frame, AgentFrame::Revoked { .. }));
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed { node: None }
    );
}

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

#[test]
fn gateway_surface_pair_issues_and_persists_hash() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&AgentFrame::SurfacePair {
        code: pending.code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::SurfaceIssued {
        surface_id,
        token,
        scopes,
        expires_at,
    } = gateway.expect(|frame| matches!(frame, AgentFrame::SurfaceIssued { .. }))
    else {
        panic!("expected SurfaceIssued");
    };
    assert_eq!(surface_id, pending.surface.id);
    assert!(token.starts_with("vowp_surf_"));
    assert_eq!(
        scopes,
        vec![
            SurfaceScope::new(format!("session.read:{SESSION}")),
            SurfaceScope::new(format!("session.write:{SESSION}")),
            SurfaceScope::new("project.read:vowp"),
            SurfaceScope::new("fleet.read"),
        ]
    );
    assert_eq!(expires_at, clock.now().plus(vowp_core::agent::SURFACE_TOKEN_TTL));

    let token_hash = hash_code(&token);
    let stored = store
        .surface_token_by_hash(&token_hash)
        .expect("hash lookup")
        .expect("token row");
    assert_eq!(stored.token_hash, token_hash);
    assert_ne!(stored.token_hash, token);
    assert_eq!(stored.surface_id, pending.surface.id);
    assert_eq!(stored.scopes, scopes);
    assert_eq!(stored.expires_at, Some(expires_at));
    assert!(store.surface_token_by_hash(&token).expect("plaintext lookup").is_none());
    assert_eq!(store.list_surface_tokens(&pending.surface.id).expect("tokens").len(), 1);
    let surface = store
        .surface_by_id(&pending.surface.id)
        .expect("surface lookup")
        .expect("surface row");
    assert_eq!(surface.state, SurfaceState::Attached);
    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed { node: Some(name) }
    );
}

#[test]
fn gateway_surface_pair_refuses_a_bad_or_replayed_code() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);

    // Malformed code.
    gateway.send(&AgentFrame::SurfacePair {
        code: "not-a-code".to_owned(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::Error { kind, retryable, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::InvalidInput);
    assert!(!retryable);

    // Well-shaped but unknown code.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::SurfacePair {
        code: "a".repeat(52),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::Error { kind, retryable, .. } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);
    assert!(!retryable);

    // A refusal keeps the authenticated connection open.
    gateway.send(&AgentFrame::Ping);
    gateway.expect(|frame| matches!(frame, AgentFrame::Pong));

    // First use succeeds.
    gateway.send(&AgentFrame::SurfacePair {
        code: pending.code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    gateway.expect(|frame| matches!(frame, AgentFrame::SurfaceIssued { .. }));

    // A replay of the same code is refused and does not mint a second token.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::SurfacePair {
        code: pending.code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::Error {
        kind,
        message,
        retryable,
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);
    assert!(!retryable);
    assert!(!message.contains(&pending.code));
    assert_eq!(store.list_surface_tokens(&pending.surface.id).expect("tokens").len(), 1);

    // An expired code is refused too.
    let expired = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("expired surface");
    clock.advance(Duration::from_secs(601));
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::SurfacePair {
        code: expired.code.clone(),
        surface_id: expired.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);
    assert!(
        store
            .list_surface_tokens(&expired.surface.id)
            .expect("tokens")
            .is_empty()
    );

    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");
}

#[test]
fn gateway_surface_pair_binds_the_connection_node() {
    let key = signing_key();
    let other_key = SigningKey::from_bytes(&[9_u8; 32]);
    let mac_studio = node_name("mac-studio");
    let mac_mini = node_name("mac-mini");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &mac_studio, &key);
    activate(home.path(), &clock, &mac_mini, &other_key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &mac_studio, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &mac_mini,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &mac_studio, &key);
    gateway.send(&AgentFrame::SurfacePair {
        code: pending.code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::Error { kind, .. } = gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. })) else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);
    assert!(
        store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .is_empty()
    );
    assert_eq!(
        store
            .surface_by_id(&pending.surface.id)
            .expect("surface lookup")
            .expect("surface row")
            .state,
        SurfaceState::Pending
    );
    gateway.finish().expect("gateway");

    // The refusal rolled the code back: the surface's own node can still redeem it.
    let issued = issue_surface_token(
        &store,
        &mac_mini,
        &pending.surface.id,
        &session,
        "opencode",
        &pending.code,
        clock.now(),
    )
    .expect("code survives a wrong-node refusal");
    assert_eq!(issued.surface_id, pending.surface.id);
}

#[test]
fn gateway_surface_pair_refuses_a_revoked_surface() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));
    revoke_surface(&store, &deps, &pending.surface.id, clock.now()).expect("revoke surface");

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&AgentFrame::SurfacePair {
        code: pending.code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::SurfaceRevoked { surface_id, reason } =
        gateway.expect(|frame| matches!(frame, AgentFrame::SurfaceRevoked { .. }))
    else {
        panic!("expected SurfaceRevoked");
    };
    assert_eq!(surface_id, pending.surface.id);
    assert!(reason.contains("revoked"));
    assert!(
        store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .is_empty()
    );

    // The surface refusal keeps the connection open.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");
}

#[test]
fn surface_rotate_sends_surface_issued_on_the_live_connection() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let other = node_name("mac-mini");
    let other_key = SigningKey::from_bytes(&[9_u8; 32]);
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    activate(home.path(), &clock, &other, &other_key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, old_token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));
    let rotated =
        rotate_surface_token(&store, &deps, &surface, SURFACE_TOKEN_OVERLAP, clock.now()).expect("rotated token");
    let spool = AgentFrameSpool::new(home.path());
    spool
        .queue(
            &name,
            surface.0.as_str(),
            &AgentFrame::SurfaceIssued {
                surface_id: surface.clone(),
                token: rotated.token.clone(),
                scopes: rotated.scopes.clone(),
                expires_at: rotated.expires_at,
            },
        )
        .expect("queued frame");
    let other_surface = SurfaceId::new();
    spool
        .queue(
            &other,
            other_surface.0.as_str(),
            &AgentFrame::SurfaceIssued {
                surface_id: other_surface.clone(),
                token: "vowp_surf_other-node".to_owned(),
                scopes: Vec::new(),
                expires_at: clock.now().plus(SURFACE_TOKEN_OVERLAP),
            },
        )
        .expect("other node's frame");

    let frame_path = home
        .path()
        .join("agent-frames")
        .join(name.as_str())
        .join(format!("{}.json", surface.0));
    assert!(frame_path.is_file(), "the frame waits for the live connection");

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let AgentFrame::SurfaceIssued {
        surface_id,
        token,
        scopes,
        expires_at,
    } = gateway.expect(|frame| matches!(frame, AgentFrame::SurfaceIssued { .. }))
    else {
        panic!("expected SurfaceIssued");
    };
    assert_eq!(surface_id, surface);
    assert_eq!(token, rotated.token);
    assert_eq!(scopes, rotated.scopes);
    assert_eq!(expires_at, clock.now().plus(vowp_core::agent::SURFACE_TOKEN_TTL));
    wait_until_for(|| !frame_path.exists(), Duration::from_secs(2));
    assert!(!frame_path.exists(), "a sent frame is removed from the spool");

    let tokens = store.list_surface_tokens(&surface).expect("tokens");
    let old = tokens
        .iter()
        .find(|row| row.token_hash == hash_code(&old_token))
        .expect("old token row");
    assert_eq!(old.expires_at, Some(clock.now().plus(SURFACE_TOKEN_OVERLAP)));
    let new = tokens
        .iter()
        .find(|row| row.token_hash == hash_code(&rotated.token))
        .expect("new token row");
    assert_eq!(
        new.expires_at,
        Some(clock.now().plus(vowp_core::agent::SURFACE_TOKEN_TTL))
    );

    let overlap = clock.now().plus(Duration::from_secs(9 * 60));
    assert!(vowp_core::agent::verify_surface_token(&store, &old_token, overlap).is_ok());
    assert!(vowp_core::agent::verify_surface_token(&store, &token, overlap).is_ok());
    let after = clock.now().plus(Duration::from_secs(11 * 60));
    assert!(vowp_core::agent::verify_surface_token(&store, &old_token, after).is_err());
    assert!(vowp_core::agent::verify_surface_token(&store, &token, after).is_ok());

    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");

    assert!(
        home.path()
            .join("agent-frames")
            .join(other.as_str())
            .join(format!("{}.json", other_surface.0))
            .is_file(),
        "a frame for another node is never sent or removed"
    );
}

#[test]
fn surface_rotate_wait_returns_when_the_frame_is_drained() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, _old_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));

    let spool = AgentFrameSpool::new(home.path());
    let drain = {
        let spool = spool.clone();
        let name = name.clone();
        let request = surface.0.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if spool.read(&name, &request).expect("read queued frame").is_some() {
                    spool.complete(&name, &request).expect("complete queued frame");
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("the rotation frame was never queued");
        })
    };

    let rotation = rotate_surface_with_policy(
        &store,
        &deps,
        &surface,
        RotationPolicy {
            grace: SURFACE_TOKEN_OVERLAP,
            wait: Some(Duration::from_secs(5)),
        },
        Timestamp::now(),
    )
    .expect("rotation waits for delivery");
    assert!(rotation.delivered, "the helper returns as soon as the frame is drained");
    drain.join().expect("drain thread");
    assert!(
        spool.read(&name, &surface.0).expect("read after drain").is_none(),
        "the drained frame is gone"
    );

    // A frame that stays queued makes the wait return false after the timeout.
    let queued = rotate_surface_with_policy(&store, &deps, &surface, RotationPolicy::default(), Timestamp::now())
        .expect("rotation for an offline node");
    assert!(!queued.delivered, "no wait means the frame is still queued");
    let drained = spool
        .wait(&name, &surface.0, Duration::from_millis(50))
        .expect("short wait");
    assert!(!drained, "a queued frame is not reported as delivered");
}

#[test]
fn surface_rotate_writes_one_ledger_note_without_the_token() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, _old_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));

    let rotated =
        rotate_surface_token(&store, &deps, &surface, SURFACE_TOKEN_OVERLAP, clock.now()).expect("rotated token");
    let notes = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            job_id: None,
            since: None,
            limit: None,
        })
        .expect("ledger rows");
    assert_eq!(notes.len(), 1, "one rotation writes exactly one ledger note");
    let note = &notes[0];
    assert_eq!(note.kind, LedgerKind::Note);
    assert_eq!(note.summary, format!("surface {surface} rotated"));
    assert_eq!(note.detail["action"].as_str(), Some("rotate"));
    assert_eq!(note.detail["surface"].as_str(), Some(surface.0.as_str()));
    assert_eq!(note.detail["node"].as_str(), Some(name.as_str()));
    assert_eq!(note.detail["prefix"].as_str(), Some(rotated.prefix.as_str()));
    assert_eq!(
        note.detail["expires_at"].as_str(),
        Some(rotated.expires_at.as_rfc3339().as_str())
    );
    assert_eq!(
        note.detail["old_expires_at"].as_str(),
        Some(rotated.old_expires_at.as_rfc3339().as_str())
    );
    let detail = note.detail.to_string();
    assert!(
        !detail.contains(&rotated.token),
        "the ledger note never carries the token"
    );
    assert!(
        !detail.contains(&hash_code(&rotated.token)),
        "the ledger note never carries the token hash"
    );
    assert!(
        !detail.contains(home.path().to_str().expect("home path")),
        "the ledger note never carries an absolute path"
    );
}

#[test]
fn surface_revoke_writes_one_ledger_note_without_the_token() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));

    let revoked = revoke_surface(&store, &deps, &surface, clock.now()).expect("revoked surface");
    assert_eq!(revoked, 1);
    let notes = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            job_id: None,
            since: None,
            limit: None,
        })
        .expect("ledger rows");
    assert_eq!(notes.len(), 1, "one revoke writes exactly one ledger note");
    let note = &notes[0];
    assert_eq!(note.kind, LedgerKind::Note);
    assert_eq!(note.summary, format!("surface {surface} revoked"));
    assert_eq!(note.detail["action"].as_str(), Some("revoke"));
    assert_eq!(note.detail["surface"].as_str(), Some(surface.0.as_str()));
    assert_eq!(note.detail["node"].as_str(), Some(name.as_str()));
    let random_part = token.strip_prefix("vowp_surf_").expect("token scheme");
    let prefix: String = random_part.chars().take(12).collect();
    assert_eq!(note.detail["prefix"].as_str(), Some(prefix.as_str()));
    assert!(
        note.detail["expires_at"].is_string(),
        "the revoke note keeps the active expiry"
    );
    let detail = note.detail.to_string();
    assert!(!detail.contains(&token), "the ledger note never carries the token");
    assert!(
        !detail.contains(&hash_code(&token)),
        "the ledger note never carries the token hash"
    );
    assert!(
        !detail.contains(home.path().to_str().expect("home path")),
        "the ledger note never carries an absolute path"
    );
    assert!(
        verify_surface_token(&store, &token, clock.now()).is_err(),
        "the revoked token is refused"
    );
}

#[test]
fn surface_ls_prints_the_prefix_and_expiry_but_never_the_token() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");
    let issued = issue_surface_token(
        &store,
        &name,
        &pending.surface.id,
        &session,
        "opencode",
        &pending.code,
        clock.now(),
    )
    .expect("issued token");
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));

    let row = surface_list_row(&store, &deps, &pending.surface.id, clock.now())
        .expect("surface row")
        .expect("surface exists");
    let random_part = issued.token.strip_prefix("vowp_surf_").expect("token scheme");
    let expected_prefix: String = random_part.chars().take(12).collect();
    assert_eq!(row.prefix.as_deref(), Some(expected_prefix.as_str()));
    assert_eq!(row.expires_at, Some(clock.now().plus(SURFACE_TOKEN_TTL)));
    assert!(row.overlap_until.is_none());
    assert!(!row.pending);
    let json = serde_json::to_string(&row).expect("surface row JSON");
    assert!(json.contains("\"prefix\""), "{json}");
    assert!(json.contains("\"expires_at\""), "{json}");
    assert!(!json.contains(&issued.token), "surface ls never carries the token");
    assert!(
        !json.contains(&hash_code(&issued.token)),
        "surface ls never carries the token hash"
    );
    assert!(
        !json.contains("needs_pair"),
        "needs_pair is agent state, not a home field"
    );

    let rotation = rotate_surface_with_policy(
        &store,
        &deps,
        &pending.surface.id,
        RotationPolicy::default(),
        clock.now(),
    )
    .expect("queued rotation");
    let row = surface_list_row(&store, &deps, &pending.surface.id, clock.now())
        .expect("surface row")
        .expect("surface exists");
    assert!(row.pending, "an undelivered rotation is pending");
    assert_eq!(row.prefix.as_deref(), Some(rotation.rotated.prefix.as_str()));
    assert_eq!(row.expires_at, Some(rotation.rotated.expires_at));
    assert_eq!(row.overlap_until, Some(rotation.old_expires_at));
    assert_eq!(row.queued_at, Some(clock.now()));
    assert_eq!(row.delivery_deadline, Some(rotation.rotated.expires_at));
    let json = serde_json::to_string(&row).expect("surface row JSON");
    assert!(
        !json.contains(&rotation.rotated.token),
        "surface ls never carries the token"
    );
    assert!(!json.contains(&issued.token), "surface ls never carries the old token");
    assert!(
        !json.contains(&hash_code(&rotation.rotated.token)),
        "surface ls never carries the token hash"
    );
    assert!(
        !json.contains("needs_pair"),
        "needs_pair is agent state, not a home field"
    );
}

#[test]
fn surface_pair_replace_revokes_the_old_surface_and_queues_no_stale_frame() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, old_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let deps = test_deps(home.path(), clock.clone(), Arc::new(MemoryLog::new()));
    let rotation = rotate_surface_with_policy(&store, &deps, &surface, RotationPolicy::default(), clock.now())
        .expect("queued rotation");
    let spool = AgentFrameSpool::new(home.path());
    let queued = spool
        .read(&name, &surface.0)
        .expect("read queued frame")
        .expect("the old surface has a queued frame");
    assert_eq!(queued.delivery_deadline(), Some(rotation.rotated.expires_at));

    let pending = replace_pending_surface(
        &store,
        &deps,
        &surface,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("replacement surface");
    assert_eq!(pending.surface.state, SurfaceState::Pending);
    assert_ne!(pending.surface.id, surface);
    assert_eq!(pending.surface.node, name);
    assert_eq!(pending.surface.metadata["session"].as_str(), Some(session.as_str()));

    let old = store
        .surface_by_id(&surface)
        .expect("old surface lookup")
        .expect("old surface row");
    assert_eq!(old.state, SurfaceState::Revoked);
    assert!(
        store
            .list_surface_tokens(&surface)
            .expect("old tokens")
            .iter()
            .all(|token| token.revoked_at.is_some()),
        "every old token is revoked"
    );
    assert!(
        verify_surface_token(&store, &old_token, clock.now()).is_err(),
        "the old token is refused immediately"
    );
    assert!(
        spool.read(&name, &surface.0).expect("read after replace").is_none(),
        "the replace removes the old surface's queued frame"
    );
    assert!(
        spool.pending(&name).expect("node queue").is_empty(),
        "the pending replacement has no queued token"
    );

    let issued = issue_surface_token(
        &store,
        &name,
        &pending.surface.id,
        &session,
        "opencode",
        &pending.code,
        clock.now(),
    )
    .expect("replacement pairing");
    assert_eq!(issued.surface_id, pending.surface.id);
}

#[test]
fn surface_token_never_appears_in_logs() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let pending = create_pending_surface(
        &store,
        &name,
        &session,
        SurfaceKind::Opencode,
        Some("opencode"),
        clock.now(),
    )
    .expect("pending surface");
    let code = pending.code.clone();

    let gateway = Gateway::start(home.path(), clock.clone());
    let log = gateway.log.clone();
    authenticate(&gateway, &name, &key);
    gateway.send(&AgentFrame::SurfacePair {
        code: code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    let AgentFrame::SurfaceIssued { token, .. } =
        gateway.expect(|frame| matches!(frame, AgentFrame::SurfaceIssued { .. }))
    else {
        panic!("expected SurfaceIssued");
    };

    // A refused replay must not log the code or the live token either.
    gateway.send(&AgentFrame::SurfacePair {
        code: code.clone(),
        surface_id: pending.surface.id.clone(),
        session: session.clone(),
        app: "opencode".to_owned(),
    });
    gateway.expect(|frame| matches!(frame, AgentFrame::Error { .. }));
    gateway.finish().expect("gateway");

    let lines = log.lines();
    assert!(
        lines.iter().any(|(_, _, message)| message.contains("paired")),
        "the pairing itself is logged: {lines:?}"
    );
    for (_, target, message) in &lines {
        assert!(!target.contains(&code), "the code must not appear in a log target");
        assert!(!message.contains(&code), "the code must not appear in a log message");
        assert!(!target.contains(&token), "the token must not appear in a log target");
        assert!(!message.contains(&token), "the token must not appear in a log message");
    }

    assert!(
        store.surface_token_by_hash(&token).expect("plaintext lookup").is_none(),
        "the plaintext token is not a stored hash"
    );
    let stored = store
        .surface_token_by_hash(&hash_code(&token))
        .expect("hash lookup")
        .expect("token row");
    assert_ne!(stored.token_hash, token);
}

#[test]
fn lease_acquire_grants_and_heartbeats() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, surface_token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease_token, expires_at, tail_seq) = expect_grant(&gateway, 0);

    assert_eq!(lease_token.len(), 52, "two ULIDs");
    assert_eq!(expires_at, clock.now().plus(Duration::from_secs(60)));
    assert_eq!(tail_seq, 0);
    let stored = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(stored.holder, "opencode");
    assert_eq!(stored.surface_id.as_ref(), Some(&surface));
    assert_eq!(stored.token, hash_lease_token(&lease_token));
    assert_ne!(stored.token, lease_token, "the plaintext token is not stored");
    assert_eq!(stored.token.len(), 64);

    let acquired = stored.heartbeat_at;
    clock.advance(Duration::from_secs(5));
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface: surface.clone(),
        session: session.clone(),
        token: lease_token.clone(),
    });
    let heartbeat_store = gateway.store();
    wait_until(|| {
        heartbeat_store
            .lease(&session)
            .expect("lease lookup")
            .is_some_and(|lease| lease.heartbeat_at > acquired)
    });
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    assert!(
        !gateway
            .output
            .frames()
            .iter()
            .any(|frame| { matches!(frame, AgentFrame::LeaseLost { .. } | AgentFrame::LeaseRevoked { .. }) }),
        "a valid heartbeat produces no loss notice"
    );
    gateway.finish().expect("gateway");
}

#[test]
fn lease_conflict_without_takeover() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (first, first_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let (second, second_token) = pair_surface(&store, &name, &session, "claude", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&first, &session, &first_token, LeaseMode::Acquire));
    let (first_lease_token, _, _) = expect_grant(&gateway, 0);
    let before = store.lease(&session).expect("lease lookup").expect("held");

    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&second, &session, &second_token, LeaseMode::Acquire));
    let AgentFrame::Error {
        kind,
        message,
        retryable,
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Conflict);
    assert!(!retryable);
    assert!(message.contains("opencode"), "conflict names the holder: {message}");
    assert!(!message.contains(&first_lease_token), "no lease token is echoed");
    assert!(!message.contains(&second_token), "no surface token is echoed");

    let after = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(after.surface_id.as_ref(), Some(&first));
    assert_eq!(after.token, before.token, "the refused acquire changed nothing");

    // The refusal keeps the authenticated connection open.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");
}

#[test]
fn lease_takeover_pushes_lease_revoked() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (old, old_surface_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let (new, new_surface_token) = pair_surface(&store, &name, &session, "claude", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&old, &session, &old_surface_token, LeaseMode::Acquire));
    let (old_lease_token, _, _) = expect_grant(&gateway, 0);

    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&new, &session, &new_surface_token, LeaseMode::Takeover));
    let AgentFrame::LeaseRevoked { reason, holder } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }))
    else {
        panic!("expected LeaseRevoked");
    };
    assert!(reason.contains("taken over"), "{reason}");
    assert_eq!(holder, "claude");
    assert!(!reason.contains(&old_lease_token));
    let (new_lease_token, _, _) = expect_grant(&gateway, start);

    let current = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(current.surface_id.as_ref(), Some(&new));
    assert_eq!(current.holder, "claude");
    assert_eq!(current.token, hash_lease_token(&new_lease_token));

    // The old surface's next heartbeat is refused and names the new holder.
    clock.advance(Duration::from_secs(5));
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface: old.clone(),
        session: session.clone(),
        token: old_lease_token.clone(),
    });
    let AgentFrame::LeaseRevoked { reason, holder } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }))
    else {
        panic!("expected LeaseRevoked");
    };
    assert_eq!(holder, "claude");
    assert!(reason.contains("read-only"), "{reason}");
    assert_eq!(
        store.lease(&session).expect("lease lookup").expect("held").token,
        hash_lease_token(&new_lease_token),
        "the old heartbeat did not touch the new row"
    );

    // The old surface's release leaves the new holder's row alone.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseRelease {
        surface: old.clone(),
        session: session.clone(),
        token: old_lease_token,
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }));
    assert_eq!(
        store.lease(&session).expect("lease lookup").expect("held").holder,
        "claude"
    );

    // The new holder can release normally.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseRelease {
        surface: new,
        session: session.clone(),
        token: new_lease_token,
    });
    let release_store = gateway.store();
    wait_until(|| release_store.lease(&session).expect("lease lookup").is_none());
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");
}

#[test]
fn lease_release_removes_the_row() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, surface_token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(&gateway, 0);
    assert!(store.lease(&session).expect("lease lookup").is_some());

    gateway.send(&AgentFrame::LeaseRelease {
        surface: surface.clone(),
        session: session.clone(),
        token: lease_token.clone(),
    });
    let release_store = gateway.store();
    wait_until(|| release_store.lease(&session).expect("lease lookup").is_none());
    assert!(store.lease(&session).expect("lease lookup").is_none());

    // A second release is idempotent; a heartbeat after release is LeaseLost.
    gateway.send(&AgentFrame::LeaseRelease {
        surface: surface.clone(),
        session: session.clone(),
        token: lease_token.clone(),
    });
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface,
        session: session.clone(),
        token: lease_token,
    });
    let AgentFrame::LeaseLost { reason } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseLost { .. }))
    else {
        panic!("expected LeaseLost");
    };
    assert!(reason.contains("read-only"), "{reason}");
    gateway.finish().expect("gateway");
}

#[test]
fn surface_token_must_match_the_paired_surface() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let other_key = SigningKey::from_bytes(&[9_u8; 32]);
    let other_name = node_name("mac-mini");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    activate(home.path(), &clock, &other_name, &other_key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (first, first_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let (second, second_token) = pair_surface(&store, &name, &session, "claude", clock.now());
    let other_session = seed_session(&store, &name, "vowp", "01ARZ3NDEKTSV4RRFFQ69G5FAZ", clock.now());
    let (_, other_session_token) = pair_surface(&store, &name, &other_session, "opencode", clock.now());
    let (foreign, foreign_token) = pair_surface(&store, &other_name, &session, "opencode", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);

    // A token for surface B cannot acquire as surface A.
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&first, &session, &second_token, LeaseMode::Acquire));
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);

    // A token paired for another session cannot acquire this one.
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(
        &first,
        &session,
        &other_session_token,
        LeaseMode::Acquire,
    ));
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);

    // A surface on another node cannot acquire over this connection.
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&foreign, &session, &foreign_token, LeaseMode::Acquire));
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);

    // The real pairing still works and no refused attempt created a lease.
    assert!(store.lease(&session).expect("lease lookup").is_none());
    gateway.send(&acquire_frame(&first, &session, &first_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(&gateway, start);
    assert_eq!(lease_token.len(), 52);
    assert_eq!(
        store
            .lease(&session)
            .expect("lease lookup")
            .expect("held")
            .surface_id
            .as_ref(),
        Some(&first)
    );
    let _ = second;
    gateway.finish().expect("gateway");
}

#[test]
fn lease_token_is_stored_only_as_a_hash() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (surface, surface_token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    let log = gateway.log.clone();
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(&gateway, 0);
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface,
        session: session.clone(),
        token: lease_token.clone(),
    });
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    gateway.finish().expect("gateway");

    let stored = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(stored.token.len(), 64, "SHA-256 hex is 64 characters");
    assert!(
        stored
            .token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "the stored value is lowercase hex: {}",
        stored.token
    );
    assert_eq!(stored.token, hash_lease_token(&lease_token));
    assert_ne!(stored.token, lease_token, "the plaintext token is not stored");
    for (_, target, message) in log.lines() {
        assert!(!target.contains(&lease_token), "the lease token reached a log target");
        assert!(!message.contains(&lease_token), "the lease token reached a log message");
    }
}

#[test]
fn lease_stale_after_sixty_seconds() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = seed_session(&store, &name, "vowp", SESSION, clock.now());
    let (old, old_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let (new, new_token) = pair_surface(&store, &name, &session, "claude", clock.now());

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&acquire_frame(&old, &session, &old_token, LeaseMode::Acquire));
    let (old_lease_token, _, _) = expect_grant(&gateway, 0);

    // 59.999 seconds old is live.
    clock.advance(Duration::from_secs(60) - Duration::from_millis(1));
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&new, &session, &new_token, LeaseMode::Acquire));
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Conflict);

    // Exactly 60 seconds is stale and may be replaced without takeover.
    clock.advance(Duration::from_millis(1));
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&new, &session, &new_token, LeaseMode::Acquire));
    let (new_lease_token, expires_at, _) = expect_grant(&gateway, start);
    assert_eq!(expires_at, clock.now().plus(Duration::from_secs(60)));
    assert_ne!(new_lease_token, old_lease_token);
    let current = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(current.surface_id.as_ref(), Some(&new));
    assert_eq!(current.token, hash_lease_token(&new_lease_token));

    // The old token is gone; its heartbeat is refused.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface: old,
        session: session.clone(),
        token: old_lease_token,
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }));
    gateway.finish().expect("gateway");
}

/// Pairs a surface, acquires its lease and begins one local chat turn.
fn begin_local_chat(
    gateway: &Gateway,
    store: &Store,
    name: &NodeName,
    session: &SessionId,
    cwd_ref: PathRef,
) -> (SurfaceId, String, RunId, String) {
    let (surface, surface_token) = pair_surface(store, name, session, "chat", ts());
    gateway.send(&acquire_frame(&surface, session, &surface_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(gateway, 0);
    let start = gateway.frame_count();
    gateway.send(&turn_begin_frame(&surface, session, &lease_token, cwd_ref));
    let (run, granted_session, granted_lease, _) = expect_turn_grant(gateway, start);
    assert_eq!(granted_session, *session);
    assert_eq!(granted_lease, lease_token);
    (surface, surface_token, run, lease_token)
}

#[test]
fn local_chat_turn_appends_to_the_home_log() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (_, _, run, lease) = begin_local_chat(
        &gateway,
        &store,
        &name,
        &session,
        PathRef::parse("project:vowp").expect("path ref"),
    );

    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });
    let event = assistant_event(&EventId::new(), "the turn landed");
    let event_id = event.id.clone();
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, event));
    let (ack_run, ack_id, ack_seq, duplicate, _, error) = expect_ack(&gateway, start);
    assert_eq!(ack_run, run);
    assert_eq!(ack_id, Some(event_id.clone()));
    assert!(!duplicate);
    assert!(error.is_none());
    assert!(ack_seq.is_some());

    let start = gateway.frame_count();
    gateway.send(&turn_end_frame(&run));
    let (_, _, _, duplicate, _, error) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(error.is_none());

    let events = read_log(home.path(), &session);
    for pair in events.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1, "the home log is gapless");
    }
    let assistants: Vec<_> = events
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::AssistantMessage(message)) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(assistants, vec!["the turn landed".to_owned()]);
    let run_row = gateway.store().require_run(&run).expect("run row");
    assert_eq!(run_row.outcome, Some(RunOutcome::Ok));
    assert_eq!(run_row.session_id.as_ref(), Some(&session));
    gateway.finish().expect("gateway");
    for event in &events {
        let text = serde_json::to_string(event).expect("event JSON");
        assert!(!text.contains("/Users/"), "no Mac absolute path: {text}");
    }
}

#[test]
fn gateway_serves_a_multi_project_home_from_the_global_store() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["alpha", "beta"]);
    activate(home.path(), &clock, &name, &key);
    let paths = NodePaths::new(home.path());

    // The gateway is started the way an SSH command on a multi-project home starts it:
    // the global store, no selected project, and node-level deps.
    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let capacity = AgentCapacity {
        cpu_cores: 8,
        load1: 0.1,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        mem_free_bytes: 8 * 1024 * 1024 * 1024,
        disk_free_bytes: 100 * 1024 * 1024 * 1024,
        busy_slots: 0,
        max_slots: 4,
        on_battery: Some(false),
        thermal_state: Some("nominal".to_owned()),
    };
    gateway.send(&AgentFrame::Heartbeat {
        at: clock.now(),
        capacity,
        pending_spool_bytes: 0,
    });
    gateway.send(&AgentFrame::Ping);
    gateway.expect(|frame| matches!(frame, AgentFrame::Pong));

    let global = Store::open_global(&paths).expect("global store");
    let card = global.node_card(&name, clock.now()).expect("card").expect("node");
    assert_eq!(card.status, NodeStatus::Active);
    assert_eq!(card.presence, NodePresence::Online);

    // A paired surface attached to beta's session begins a chat turn. Its run row must
    // land in beta's database, not in the global handle and not in alpha.
    let beta = ProjectName::parse("beta").expect("beta slug");
    let beta_store = Store::open_with_project(&paths, &beta).expect("beta store");
    let session = session_in(home.path(), &beta_store, &name, Some("beta"), SESSION, &clock);
    let (surface, surface_token) = pair_surface(&beta_store, &name, &session, "chat", clock.now());
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(&gateway, 0);
    let start = gateway.frame_count();
    gateway.send(&turn_begin_frame(
        &surface,
        &session,
        &lease_token,
        PathRef::parse("project:beta").expect("path ref"),
    ));
    let (run, granted_session, granted_lease, _) = expect_turn_grant(&gateway, start);
    assert_eq!(granted_session, session);
    assert_eq!(granted_lease, lease_token);

    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease_token),
    });
    let event = assistant_event(&EventId::new(), "served from beta");
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, event));
    let (_, _, _, duplicate, _, error) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(error.is_none());
    let start = gateway.frame_count();
    gateway.send(&turn_end_frame(&run));
    let (_, _, _, duplicate, _, error) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(error.is_none());

    let run_row = beta_store.require_run(&run).expect("run row in beta");
    assert_eq!(run_row.outcome, Some(RunOutcome::Ok));
    assert_eq!(run_row.session_id.as_ref(), Some(&session));
    let alpha = ProjectName::parse("alpha").expect("alpha slug");
    let alpha_store = Store::open_with_project(&paths, &alpha).expect("alpha store");
    assert!(
        alpha_store.run(&run).expect("alpha run lookup").is_none(),
        "the run must not land in another project's database"
    );

    assert_eq!(
        gateway.finish().expect("gateway"),
        GatewayOutcome::Closed {
            node: Some(name.clone())
        }
    );
}

#[test]
fn gateway_serves_a_project_less_session_in_the_node_store() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["alpha", "beta"]);
    activate(home.path(), &clock, &name, &key);
    let paths = NodePaths::new(home.path());
    let global = Store::open_global(&paths)
        .expect("global store")
        .with_clock(clock.clone());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let session = session_in(home.path(), &global, &name, None, SESSION, &clock);
    let (surface, surface_token) = pair_surface(&global, &name, &session, "chat", clock.now());
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease_token, _, _) = expect_grant(&gateway, 0);
    let start = gateway.frame_count();
    gateway.send(&turn_begin_frame(
        &surface,
        &session,
        &lease_token,
        PathRef::parse("project:alpha").expect("path ref"),
    ));
    let (run, _, _, _) = expect_turn_grant(&gateway, start);
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease_token),
    });
    let start = gateway.frame_count();
    gateway.send(&event_line(
        &run,
        assistant_event(&EventId::new(), "global orchestrator"),
    ));
    let (_, _, _, duplicate, _, error) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(error.is_none());
    let start = gateway.frame_count();
    gateway.send(&turn_end_frame(&run));
    let (_, _, _, duplicate, _, error) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(error.is_none());

    let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT).expect("node project");
    let node_store = Store::open_with_project(&paths, &node_project).expect("node store");
    let run_row = node_store.require_run(&run).expect("run row in _node");
    assert_eq!(run_row.outcome, Some(RunOutcome::Ok));
    let alpha = ProjectName::parse("alpha").expect("alpha slug");
    let alpha_store = Store::open_with_project(&paths, &alpha).expect("alpha store");
    assert!(alpha_store.run(&run).expect("alpha run lookup").is_none());

    gateway.finish().expect("gateway");
}

#[test]
fn local_chat_turn_begin_requires_a_live_lease() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (surface, _) = pair_surface(&store, &name, &session, "chat", ts());
    let start = gateway.frame_count();
    gateway.send(&turn_begin_frame(
        &surface,
        &session,
        &"A".repeat(52),
        PathRef::parse("project:vowp").expect("path ref"),
    ));
    let AgentFrame::Error { kind, message, .. } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected an Error frame");
    };
    assert_eq!(kind, ErrorKind::Refused);
    assert!(message.contains("no live lease"), "{message}");
    assert!(
        !gateway
            .output
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::TurnGrant { .. })),
        "no turn is granted without a lease"
    );
    gateway.finish().expect("gateway");
}

#[test]
fn local_chat_replays_after_an_ssh_drop_without_gaps() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (surface, _, run, lease) = begin_local_chat(
        &gateway,
        &store,
        &name,
        &session,
        PathRef::parse("project:vowp").expect("path ref"),
    );
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });

    let first = EventId::new();
    let second = EventId::new();
    let third = EventId::new();
    let first_event = assistant_event(&first, "one");
    let second_event = assistant_event(&second, "two");

    let start = gateway.frame_count();
    gateway.send(&event_line(&run, first_event.clone()));
    let (_, _, first_seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(!duplicate);
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, second_event.clone()));
    let (_, _, second_seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(first_seq < second_seq);

    // The SSH transport dropped and the agent reconnected: a second gateway process
    // starts with an empty in-memory map. The heartbeat proves the lease, the fresh
    // hello resumes the run, and both unacknowledged lines replay as duplicates.
    gateway.finish().expect("first gateway");
    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface,
        session: session.clone(),
        token: lease.clone(),
    });
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });

    let start = gateway.frame_count();
    gateway.send(&event_line(&run, first_event));
    let (_, first_ack, _, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(duplicate, "the first replayed event is a duplicate");
    assert_eq!(first_ack, Some(first.clone()));
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, second_event));
    let (_, second_ack, _, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(duplicate, "the second replayed event is a duplicate");
    assert_eq!(second_ack, Some(second.clone()));

    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&third, "three")));
    let (_, _, third_seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(!duplicate);
    assert!(second_seq < third_seq);

    let start = gateway.frame_count();
    gateway.send(&turn_end_frame(&run));
    expect_ack(&gateway, start);
    gateway.finish().expect("gateway");

    let events = read_log(home.path(), &session);
    for pair in events.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1, "no gap after replay");
    }
    let ids: Vec<EventId> = events
        .iter()
        .map(|event| event.id.clone())
        .filter(|id| id == &first || id == &second || id == &third)
        .collect();
    assert_eq!(ids, vec![first, second, third], "each replayed id appears once");
}

#[test]
fn local_chat_enforces_the_64_event_window_and_replenishes_it() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (surface, _, run, lease) = begin_local_chat(
        &gateway,
        &store,
        &name,
        &session,
        PathRef::parse("project:vowp").expect("path ref"),
    );
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });

    let window_start = gateway.frame_count();
    let mut accepted = Vec::new();
    for index in 0..64 {
        let id = EventId::new();
        let start = gateway.frame_count();
        gateway.send(&event_line(&run, assistant_event(&id, &format!("event {index}"))));
        let (_, ack_id, _, duplicate, _, _) = expect_ack(&gateway, start);
        assert_eq!(ack_id, Some(id.clone()));
        assert!(!duplicate);
        accepted.push(id);
    }
    // The 64th event's Credit arrives before its acknowledgement.
    gateway.expect_after(window_start, |frame| {
        matches!(frame, AgentFrame::Credit { count: 64, .. })
    });

    let refused = EventId::new();
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&refused, "past the window")));
    let AgentFrame::Error {
        kind,
        message,
        retryable,
    } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected a window error");
    };
    assert_eq!(kind, ErrorKind::Conflict);
    assert!(retryable);
    assert!(message.contains("64-event in-flight window"), "{message}");
    assert!(
        !gateway.output.frames().iter().any(|frame| matches!(
            frame,
            AgentFrame::IngestAck { event_id: Some(id), .. } if id == &refused
        )),
        "the refused event was not appended"
    );

    // The lease heartbeat is the agent's confirmation that it consumed the Credit.
    gateway.send(&AgentFrame::LeaseHeartbeat {
        surface,
        session: session.clone(),
        token: lease.clone(),
    });
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&refused, "past the window")));
    let (_, ack_id, _, duplicate, _, _) = expect_ack(&gateway, start);
    assert_eq!(ack_id, Some(refused.clone()));
    assert!(!duplicate);
    accepted.push(refused);

    let start = gateway.frame_count();
    gateway.send(&turn_end_frame(&run));
    expect_ack(&gateway, start);
    gateway.finish().expect("gateway");

    let events = read_log(home.path(), &session);
    let appended: Vec<EventId> = events
        .iter()
        .map(|event| event.id.clone())
        .filter(|id| accepted.contains(id))
        .collect();
    assert_eq!(appended, accepted, "each windowed event appears exactly once");
}

#[test]
fn local_chat_rejects_a_duplicate_event_id_with_different_content() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (_, _, run, lease) = begin_local_chat(
        &gateway,
        &store,
        &name,
        &session,
        PathRef::parse("project:vowp").expect("path ref"),
    );
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });

    let event_id = EventId::new();
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&event_id, "original")));
    expect_ack(&gateway, start);

    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&event_id, "different")));
    let (_, ack_id, _, _, revoked, error) = expect_ack(&gateway, start);
    assert!(ack_id.is_none());
    assert!(revoked.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("duplicate-conflict")),
        "the ack names the conflict: {error:?}"
    );
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));

    // The run is fenced: a later line is never appended.
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&EventId::new(), "late")));
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    gateway.finish().expect("gateway");

    let events = read_log(home.path(), &session);
    let texts: Vec<_> = events
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::AssistantMessage(message)) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["original".to_owned()], "no conflicting or late content");
}

#[test]
fn local_chat_takeover_fences_the_old_run() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (_, _, run, lease) = begin_local_chat(
        &gateway,
        &store,
        &name,
        &session,
        PathRef::parse("project:vowp").expect("path ref"),
    );
    gateway.send(&AgentFrame::RemoteLine {
        run_id: run.clone(),
        epoch: 1,
        line: hello_line(&session, &run, &lease),
    });
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&EventId::new(), "before takeover")));
    expect_ack(&gateway, start);

    let (phone, phone_token) = pair_surface(&store, &name, &session, "chat", ts());
    let start = gateway.frame_count();
    gateway.send(&acquire_frame(&phone, &session, &phone_token, LeaseMode::Takeover));
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }));
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    expect_grant(&gateway, start);
    // The new surface holds the lease.
    let current = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(current.surface_id.as_ref(), Some(&phone));

    let late = EventId::new();
    let start = gateway.frame_count();
    gateway.send(&event_line(&run, assistant_event(&late, "after takeover")));
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    gateway.finish().expect("gateway");

    let events = read_log(home.path(), &session);
    assert!(
        !events.iter().any(|event| event.id == late),
        "a fenced run never appends a later line"
    );
    let texts: Vec<_> = events
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::AssistantMessage(message)) => Some(message.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["before takeover".to_owned()]);
}

#[test]
fn local_chat_uses_a_path_ref_not_an_absolute_path() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = chat_session(home.path(), &store, &name, SESSION, &clock);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let (surface, surface_token) = pair_surface(&store, &name, &session, "chat", ts());
    gateway.send(&acquire_frame(&surface, &session, &surface_token, LeaseMode::Acquire));
    let (lease, _, _) = expect_grant(&gateway, 0);

    let frame = turn_begin_frame(
        &surface,
        &session,
        &lease,
        PathRef::parse("worktree:vowp/local-chat").expect("path ref"),
    );
    let wire = serde_json::to_string(&frame).expect("TurnBegin JSON");
    assert!(wire.contains("\"cwd_ref\":\"worktree:vowp/local-chat\""), "{wire}");
    assert!(!wire.contains("/Users/"), "no absolute path in the frame: {wire}");

    // A path ref for another project is refused rather than silently used.
    let other = AgentFrame::TurnBegin {
        surface: surface.clone(),
        session: session.clone(),
        lease_token: lease.clone(),
        prompt: "wrong project".to_owned(),
        executor: "opencode:deepseek/deepseek-flash#max".to_owned(),
        cwd_ref: PathRef::parse("project:another").expect("path ref"),
        job: None,
        purpose: RunPurpose::Continue,
    };
    let start = gateway.frame_count();
    gateway.send(&other);
    let AgentFrame::Error { kind, .. } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
    else {
        panic!("expected Error");
    };
    assert_eq!(kind, ErrorKind::Refused);

    let start = gateway.frame_count();
    gateway.send(&frame);
    let (run, _, _, _) = expect_turn_grant(&gateway, start);

    // The home records the run without the Mac's working directory.
    let row = gateway.store().require_run(&run).expect("run row");
    if let Some(cwd) = &row.cwd {
        assert!(!cwd.contains("/Users/"), "the run row has no Mac path: {cwd}");
    }
    gateway.finish().expect("gateway");
}

/// Inserts one ready offload job under `project`.
fn insert_ready_offload_job(store: &Store, project: &ProjectName, id: &str) -> JobId {
    let new = NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("job {id}"),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: Some("worker".to_owned()),
        needs: Vec::new(),
        owns: vec!["src/**".to_owned()],
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    };
    let job = store.insert_job(project, &new, "test").expect("insert job");
    store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready job");
    job.id
}

/// One heartbeat with the given slot occupancy.
fn offload_heartbeat(busy_slots: u32, max_slots: u32) -> AgentFrame {
    AgentFrame::Heartbeat {
        at: ts(),
        capacity: AgentCapacity {
            cpu_cores: 8,
            load1: 0.1,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            mem_free_bytes: 8 * 1024 * 1024 * 1024,
            disk_free_bytes: 100 * 1024 * 1024 * 1024,
            busy_slots,
            max_slots,
            on_battery: Some(false),
            thermal_state: Some("nominal".to_owned()),
        },
        pending_spool_bytes: 0,
    }
}

/// Waits for one heartbeat's dispatch pass to settle.
///
/// The caller sends the heartbeat and then a `Ping`. Frames are processed in
/// order on one connection thread, so the `Pong` is written only after the
/// heartbeat's enqueue, claim and send pass has finished. `start` is the frame
/// count before the heartbeat, which separates this pass's `AssignRun` frames
/// from earlier ones.
///
/// The wait is condition-based and bounded for a loaded host, but it never waits
/// past an over-dispatch: one more `AssignRun` frame or `sent` row than the
/// slots allow panics immediately with the dispatch state.
fn wait_for_dispatch(
    gateway: &Gateway,
    store: &Store,
    name: &NodeName,
    start: usize,
    expected_assigns: usize,
    expected_sent: usize,
) {
    let deadline = Instant::now() + GATEWAY_WAIT;
    loop {
        let frames = gateway.output.frames();
        let window = &frames[start.min(frames.len())..];
        let assigns = window
            .iter()
            .filter(|frame| matches!(frame, AgentFrame::AssignRun { .. }))
            .count();
        let sent = store
            .assignments_for_node(name)
            .expect("assignments")
            .iter()
            .filter(|assignment| assignment.state == vowp_store::repo_offload::AgentAssignmentState::Sent)
            .count();
        assert!(
            assigns <= expected_assigns && sent <= expected_sent,
            "the node admitted more runs than its slots: expected at most {expected_assigns} new AssignRun \
             frames and {expected_sent} sent rows, saw {assigns} and {sent}; {}",
            dispatch_state(gateway, store, name)
        );
        if window.iter().any(|frame| matches!(frame, AgentFrame::Pong)) {
            assert_eq!(
                assigns,
                expected_assigns,
                "the dispatch settled with the wrong number of new runs: {}",
                dispatch_state(gateway, store, name)
            );
            assert_eq!(
                sent,
                expected_sent,
                "the dispatch settled with the wrong number of sent rows: {}",
                dispatch_state(gateway, store, name)
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the dispatch did not settle within {GATEWAY_WAIT:?}: {}",
            dispatch_state(gateway, store, name)
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Formats the frames and durable assignments for a failed dispatch wait.
fn dispatch_state(gateway: &Gateway, store: &Store, name: &NodeName) -> String {
    let frames = gateway.output.frames();
    let assigns = frames
        .iter()
        .filter(|frame| matches!(frame, AgentFrame::AssignRun { .. }))
        .count();
    let rows = store
        .assignments_for_node(name)
        .map(|assignments| {
            assignments
                .iter()
                .map(|assignment| format!("{}={}", assignment.payload.job, assignment.state.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|error| format!("<store error: {error}>"));
    format!("{assigns} AssignRun frames sent; assignments: [{rows}]")
}

#[test]
fn heartbeat_dispatches_at_most_the_node_slots() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let project = ProjectName::parse("vowp").expect("project");
    let setup = test_store(home.path(), &clock);
    setup
        .upsert_project(&Project {
            name: project.clone(),
            path: home.path().join("project").display().to_string(),
            remote_url: Some("https://example.invalid/acme/vowp.git".to_owned()),
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: ts(),
            updated_at: ts(),
        })
        .expect("project row");
    activate(home.path(), &clock, &name, &key);
    setup
        .upsert_node_path(&NodePath {
            node: name.clone(),
            project: project.clone(),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: "/srv/vowp-wt".to_owned(),
            allow: Vec::new(),
            updated_at: ts(),
        })
        .expect("path map");
    for id in ["offload-a", "offload-b", "offload-c"] {
        insert_ready_offload_job(&setup, &project, id);
    }

    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.project.path = Some(home.path().join("project").display().to_string());
    config.offload.enabled = true;
    config.offload.default = Placement::Any;
    // Keep the fleet cap above the node's two slots so this test measures the node limit.
    config.offload.max_remote_runs = 3;
    let gateway = Gateway::start_with_config(home.path(), clock.clone(), config);
    let store = gateway.store();
    authenticate_within(&gateway, &name, &key, GATEWAY_WAIT);

    let count_assign = |gateway: &Gateway| {
        gateway
            .output
            .frames()
            .into_iter()
            .filter(|frame| matches!(frame, AgentFrame::AssignRun { .. }))
            .count()
    };

    // The heartbeat and the Ping are queued together. The connection handles
    // frames in order, so the Pong proves this heartbeat's enqueue/claim/send
    // pass has finished; the wait then checks the runs and sent rows it made.
    let start = gateway.frame_count();
    gateway.send(&offload_heartbeat(0, 2));
    gateway.send(&AgentFrame::Ping);
    wait_for_dispatch(&gateway, &store, &name, start, 2, 2);
    assert_eq!(count_assign(&gateway), 2, "one heartbeat sends only the free slots");

    // The two sent rows occupy the slots even though the agent reports idle capacity.
    let start = gateway.frame_count();
    gateway.send(&offload_heartbeat(0, 2));
    gateway.send(&AgentFrame::Ping);
    wait_for_dispatch(&gateway, &store, &name, start, 0, 2);
    assert_eq!(count_assign(&gateway), 2, "a second heartbeat must not over-dispatch");

    let assignments = store.assignments_for_node(&name).expect("assignments");
    assert_eq!(assignments.len(), 3, "all three jobs are durably queued");
    assert_eq!(
        assignments
            .iter()
            .filter(|assignment| assignment.state == vowp_store::repo_offload::AgentAssignmentState::Sent)
            .count(),
        2,
        "only the claimed rows are sent"
    );
    gateway.finish().expect("gateway");
}

#[test]
fn agent_second_connection_supersedes_the_first() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    activate(home.path(), &clock, &name, &key);

    let first = Gateway::start(home.path(), clock.clone());
    authenticate(&first, &name, &key);
    let second = Gateway::start(home.path(), clock.clone());
    authenticate(&second, &name, &key);

    let AgentFrame::Superseded { connection_id } = first.expect(|frame| matches!(frame, AgentFrame::Superseded { .. }))
    else {
        panic!("expected Superseded");
    };
    assert!(!connection_id.is_empty());
    let outcome = first.finish().expect("first gateway closes cleanly");
    assert!(matches!(outcome, GatewayOutcome::Closed { node: Some(node) } if node == name));
    second.finish().expect("second gateway");
}

/// Runs git in `cwd`, asserting success.
fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let output = git_run(cwd, args);
    assert_eq!(output.exit_code, Some(0), "git {args:?} failed: {}", output.text());
    output.text()
}

/// Runs git in `cwd` and returns its merged output.
fn git_run(cwd: &Path, args: &[&str]) -> CommandOutput {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    SystemRunner
        .run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: git_env(),
            timeout: Duration::from_secs(60),
        })
        .expect("git")
}

/// Environment that keeps git hermetic: no user or system config, fixed identity.
fn git_env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_AUTHOR_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_AUTHOR_EMAIL".to_owned(), "test@example.invalid".to_owned()),
        ("GIT_COMMITTER_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_COMMITTER_EMAIL".to_owned(), "test@example.invalid".to_owned()),
    ]
}

/// Creates a real repository on `main` with one initial commit.
fn init_result_repo(repo: &Path) {
    std::fs::create_dir_all(repo).expect("repo dir");
    git_ok(repo, &["init", "-b", "main"]);
    write_result_file(&repo.join("README.md"), "base\n");
    git_ok(repo, &["add", "-A"]);
    git_ok(repo, &["commit", "-m", "initial"]);
}

/// Creates a bare repository to act as `origin`.
fn init_result_bare(path: &Path) {
    std::fs::create_dir_all(path).expect("bare dir");
    git_ok(path, &["init", "--bare", "-b", "main"]);
}

/// Writes `text` to `path`, creating parent directories.
fn write_result_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(path, text).expect("write file");
}

/// Clones `origin` once, pushes one commit on `job/<id>` and returns the head.
fn push_result_branch(agent: &Path, origin: &Path, job: &JobId) -> String {
    let root = agent.parent().expect("agent parent");
    if !agent.join(".git").is_dir() {
        std::fs::create_dir_all(root).expect("agent root");
        git_ok(
            root,
            &["clone", &origin.display().to_string(), &agent.display().to_string()],
        );
    }
    let branch = format!("job/{job}");
    git_ok(agent, &["checkout", "-B", &branch, "origin/main"]);
    write_result_file(&agent.join("src/lib.rs"), "pub fn result() {}\n");
    git_ok(agent, &["add", "-A"]);
    git_ok(agent, &["commit", "-m", "job work"]);
    git_ok(agent, &["push", "-f", "origin", &branch]);
    git_ok(agent, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Builds one real offload home with a bare origin and a project clone.
fn offload_result_home(home: &Path, clock: &Arc<FakeClock>) -> (ProjectName, std::path::PathBuf, Store) {
    let project = ProjectName::parse("vowp").expect("project");
    let origin = home.join("origin.git");
    let repo = home.join("project");
    init_result_bare(&origin);
    init_result_repo(&repo);
    git_ok(&repo, &["remote", "add", "origin", &origin.display().to_string()]);
    git_ok(&repo, &["push", "-u", "origin", "main"]);
    let store = test_store(home, clock);
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: repo.display().to_string(),
            remote_url: Some(origin.display().to_string()),
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: ts(),
            updated_at: ts(),
        })
        .expect("project row");
    (project, origin, store)
}

/// Builds the config for the real-git gateway result test.
fn offload_result_config(project: &ProjectName, repo: &Path) -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.project.path = Some(repo.display().to_string());
    config.project.remote = "origin".to_owned();
    config.project.default_branch = "main".to_owned();
    config.offload.enabled = true;
    config.offload.default = Placement::Any;
    config
}

#[test]
fn gateway_returns_an_offloaded_result_to_the_gating_path() {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let (project, origin, setup) = offload_result_home(home.path(), &clock);
    activate(home.path(), &clock, &name, &key);
    setup
        .upsert_node_path(&NodePath {
            node: name.clone(),
            project: project.clone(),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: "/srv/vowp-wt".to_owned(),
            allow: Vec::new(),
            updated_at: ts(),
        })
        .expect("path map");
    let job_id = insert_ready_offload_job(&setup, &project, "result-job");
    let config = offload_result_config(&project, &home.path().join("project"));
    let log = Arc::new(MemoryLog::new());
    let mut deps = test_deps_with_config(home.path(), clock.clone(), log.clone(), config);
    deps.git = Arc::new(SystemGit::default());
    let gateway = Gateway::start_with_deps(home.path(), clock.clone(), log, deps);
    authenticate(&gateway, &name, &key);
    gateway.send(&offload_heartbeat(0, 4));

    let AgentFrame::AssignRun {
        assignment,
        lease_token,
        ..
    } = gateway.expect(|frame| matches!(frame, AgentFrame::AssignRun { .. }))
    else {
        panic!("expected AssignRun");
    };
    gateway.send(&AgentFrame::RunAccepted {
        assignment_id: assignment.assignment_id.clone(),
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
    });
    let head = push_result_branch(&home.path().join("agent"), &origin, &job_id);
    gateway.send(&AgentFrame::RemoteLine {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        line: RemoteLine::Hello {
            session_id: assignment.session_id.clone(),
            run_id: assignment.run_id.clone(),
            executor: assignment.executor.to_string(),
            node: name.clone(),
            spool_id: "result-spool".to_owned(),
            lease: lease_token,
        },
    });
    gateway.send(&AgentFrame::RunState {
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
        state: RemoteRunState::Finishing,
        spool_bytes: 0,
        head_commit: Some(head.clone()),
        detail: "the job branch is pushed".to_owned(),
    });
    gateway.send(&turn_end_frame(&assignment.run_id));

    wait_until_for(
        || {
            setup
                .require_job(&job_id)
                .map(|job| job.state == JobState::Gating)
                .unwrap_or(false)
                && setup
                    .require_run(&assignment.run_id)
                    .map(|run| run.outcome == Some(RunOutcome::Ok))
                    .unwrap_or(false)
        },
        Duration::from_secs(20),
    );
    let job = setup.require_job(&job_id).expect("job");
    assert_eq!(job.branch.as_deref(), Some(format!("job/{job_id}").as_str()));
    let worktree = std::path::PathBuf::from(job.worktree.expect("staged worktree"));
    assert!(worktree.is_dir(), "the home staged the pushed head");
    assert_eq!(
        git_ok(&worktree, &["rev-parse", "HEAD"]).trim(),
        head,
        "the staged worktree is the pushed head"
    );
    let run = setup.require_run(&assignment.run_id).expect("run row");
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert!(run.detail.contains(&head), "run detail: {}", run.detail);
    let assignment_row = setup
        .assignments_for_run(&assignment.run_id)
        .expect("assignments")
        .pop()
        .expect("assignment");
    assert_eq!(
        assignment_row.state,
        vowp_store::repo_offload::AgentAssignmentState::Finished
    );
    assert!(setup.lease(&assignment.session_id).expect("lease").is_none());
    assert_eq!(
        git_ok(&home.path().join("project"), &["status", "--porcelain"]),
        "",
        "the main checkout stays clean"
    );
    gateway.finish().expect("gateway");
}

/// Builds one path map for the gateway path-map tests.
fn reported_path_map(project: &str, local_root: &str, worktree_root: Option<&str>, allow: &[&str]) -> AgentPathMap {
    AgentPathMap {
        project: ProjectName::parse(project).expect("project"),
        local_root: local_root.to_owned(),
        worktree_root: worktree_root.map(str::to_owned),
        allow: allow.iter().map(|entry| (*entry).to_owned()).collect(),
    }
}

#[test]
fn path_map_update_is_persisted_and_acknowledged() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let vowp = ProjectName::parse("vowp").expect("project");
    let alpha = ProjectName::parse("alpha").expect("project");

    let start = gateway.frame_count();
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("vowp", "/srv/vowp", Some("/srv/vowp-wt"), &["src"]),
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let store = test_store(home.path(), &clock);
    let card = store.node_card(&name, clock.now()).expect("card").expect("present");
    assert_eq!(card.mapped, vec![vowp.clone()], "the new project is mapped");
    let paths = store.list_node_paths(&name).expect("paths");
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].local_root, "/srv/vowp");
    assert_eq!(paths[0].worktree_root, "/srv/vowp-wt");
    assert_eq!(paths[0].allow, vec!["src".to_owned()]);
    assert_eq!(paths[0].updated_at, clock.now());

    // Replacing the map keeps one row and updates every field without re-enrolling.
    clock.advance(Duration::from_secs(5));
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("vowp", "/srv/vowp-renamed", None, &["docs"]),
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let paths = store.list_node_paths(&name).expect("paths");
    assert_eq!(paths.len(), 1, "the upsert does not add a second row");
    assert_eq!(paths[0].local_root, "/srv/vowp-renamed");
    assert_eq!(paths[0].worktree_root, "/srv/vowp-renamed");
    assert_eq!(paths[0].allow, vec!["docs".to_owned()]);
    assert_eq!(paths[0].updated_at, clock.now());

    // A second project arrives in the same connection.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("alpha", "/srv/alpha", Some("/srv/alpha-wt"), &[]),
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let card = store.node_card(&name, clock.now()).expect("card").expect("present");
    assert_eq!(card.mapped, vec![alpha, vowp.clone()]);

    // Repeated updates in one burst are applied in order; the last one wins.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("vowp", "/srv/one", None, &[]),
    });
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("vowp", "/srv/two", None, &[]),
    });
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let mut pongs = 0;
    wait_until(|| {
        pongs = gateway
            .output
            .frames()
            .into_iter()
            .skip(start)
            .filter(|frame| matches!(frame, AgentFrame::Pong))
            .count();
        pongs >= 2
    });
    let paths = store.list_node_paths(&name).expect("paths");
    let vowp_path = paths.iter().find(|path| path.project == vowp).expect("vowp map");
    assert_eq!(vowp_path.local_root, "/srv/two", "the last update wins");

    let outcome = gateway.finish().expect("gateway");
    assert_eq!(outcome, GatewayOutcome::Closed { node: Some(name) });
}

#[test]
fn invalid_path_map_updates_are_refused_without_dropping_the_connection() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let invalid = [
        reported_path_map("vowp", "", None, &[]),
        reported_path_map("vowp", "relative/root", None, &[]),
        reported_path_map("vowp", "/srv/../etc", None, &[]),
        reported_path_map("vowp", "/srv/vowp\u{0}", None, &[]),
        reported_path_map("vowp", "/srv/vowp", Some(""), &[]),
        reported_path_map("vowp", "/srv/vowp", Some("/srv/../wt"), &[]),
        reported_path_map("vowp", "/srv/vowp", None, &[""]),
        reported_path_map("vowp", "/srv/vowp", None, &["../secret"]),
        reported_path_map("vowp", "/srv/vowp", None, &["/absolute"]),
        reported_path_map("vowp", "/srv/vowp", None, &["src/./nested"]),
        reported_path_map("vowp", "/srv/vowp", None, &["src", "src"]),
    ];
    for map in invalid {
        let start = gateway.frame_count();
        gateway.send(&AgentFrame::PathMapUpdate { map });
        let AgentFrame::Error {
            kind,
            retryable,
            message,
        } = gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }))
        else {
            panic!("expected an Error frame");
        };
        assert_eq!(kind, ErrorKind::InvalidInput);
        assert!(!retryable, "an invalid map is not retryable");
        assert!(
            !message.contains("/srv/"),
            "the refusal must not echo a local root: {message}"
        );
    }

    // The connection stayed open: a later frame is still answered.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let store = test_store(home.path(), &clock);
    assert!(
        store.list_node_paths(&name).expect("paths").is_empty(),
        "an invalid map must not be stored"
    );
    let warnings: Vec<String> = gateway
        .log
        .lines()
        .into_iter()
        .filter(|(level, _, message)| *level == Level::Warn && message.contains("path map"))
        .map(|(_, _, message)| message)
        .collect();
    assert!(!warnings.is_empty(), "an invalid map is logged");
    for warning in warnings {
        assert!(!warning.contains("/srv/../etc"), "{warning}");
        assert!(!warning.contains("relative/root"), "{warning}");
    }

    let outcome = gateway.finish().expect("gateway");
    assert_eq!(outcome, GatewayOutcome::Closed { node: Some(name) });
}

#[test]
fn path_map_update_from_a_revoked_node_is_refused() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let store = test_store(home.path(), &clock);
    store
        .set_node_status(&name, NodeStatus::Revoked, "revoked by owner", clock.now())
        .expect("revoke");
    gateway.send(&AgentFrame::PathMapUpdate {
        map: reported_path_map("vowp", "/srv/vowp", None, &[]),
    });
    let AgentFrame::Revoked { reason } = gateway.expect(|frame| matches!(frame, AgentFrame::Revoked { .. })) else {
        panic!("expected Revoked");
    };
    assert!(reason.contains("revoked"), "{reason}");
    let outcome = gateway.finish().expect("gateway");
    assert_eq!(
        outcome,
        GatewayOutcome::Closed {
            node: Some(name.clone())
        }
    );
    assert!(
        store.list_node_paths(&name).expect("paths").is_empty(),
        "a revoked node must not update its map"
    );
}

#[test]
fn unexpected_post_handshake_frames_are_logged_and_ignored() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let start = gateway.frame_count();
    // `Enrolled` is a home -> agent frame; an agent must never send it. A home that does
    // not know it yet must warn and keep the authenticated session.
    gateway.send(&AgentFrame::Enrolled {
        home_id: "home-1".to_owned(),
        node: name.clone(),
        protocol: "vowp-agent/1".to_owned(),
        server_time: clock.now(),
        heartbeat_secs: 10,
    });
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    let frames = gateway.output.frames();
    assert!(
        !frames.iter().any(|frame| matches!(
            frame,
            AgentFrame::Error {
                kind: ErrorKind::InvalidInput,
                ..
            }
        )),
        "an unexpected post-handshake frame produced InvalidInput: {frames:?}"
    );
    let warnings: Vec<(String, String)> = gateway
        .log
        .lines()
        .into_iter()
        .filter(|(level, _, _)| *level == Level::Warn)
        .map(|(_, target, message)| (target, message))
        .collect();
    assert!(
        warnings.iter().any(|(target, message)| target.contains("mac-studio")
            && message.contains("unexpected post-handshake frame enrolled")),
        "the warning names the node and frame kind: {warnings:?}"
    );
    assert!(
        !gateway.log.contains("vowp-agent/1"),
        "the warning must not echo frame payloads"
    );

    let outcome = gateway.finish().expect("gateway");
    assert_eq!(outcome, GatewayOutcome::Closed { node: Some(name) });
}

#[test]
fn replace_pending_rotates_a_pending_code_and_refuses_an_active_node() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let store = test_store(home.path(), &clock);
    let first = create_pending(&store, &name, &["old".to_owned()], clock.now()).expect("pending node");

    let second = replace_pending(&store, &name, &["new".to_owned()], clock.now()).expect("replace pending");
    assert_ne!(first.code, second.code, "the code rotates");
    let old_request = EnrollRequest {
        code: first.code,
        public_key: public_hex(&signing_key()),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    let error = redeem(&store, &old_request, clock.now()).expect_err("the old code is gone");
    assert!(matches!(error, CoreError::Refused(_)), "got {error:?}");
    let card = store.node_card(&name, clock.now()).expect("card").expect("present");
    assert_eq!(card.labels, vec!["new".to_owned()]);

    let request = EnrollRequest {
        code: second.code,
        public_key: public_hex(&signing_key()),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    redeem(&store, &request, clock.now()).expect("the replacement code enrolls");
    let error = replace_pending(&store, &name, &[], clock.now()).expect_err("an active node is refused");
    assert!(error.to_string().contains("active"), "{error}");
    assert!(error.to_string().contains("revoke"), "{error}");
}

#[test]
fn replace_pending_rotates_an_expired_pending_code_in_one_step() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let store = test_store(home.path(), &clock);
    let first = create_pending(&store, &name, &[], clock.now()).expect("pending node");
    clock.advance(Duration::from_secs(11 * 60));

    let second = replace_pending(&store, &name, &[], clock.now()).expect("replace expired pending");
    assert_ne!(first.code, second.code);
    let old = EnrollRequest {
        code: first.code,
        public_key: public_hex(&signing_key()),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    assert!(
        redeem(&store, &old, clock.now()).is_err(),
        "the expired code must not redeem"
    );
    let request = EnrollRequest {
        code: second.code,
        public_key: public_hex(&signing_key()),
        agent_version: "0.3.0".to_owned(),
        capabilities: capabilities(),
        paths: Vec::new(),
    };
    redeem(&store, &request, clock.now()).expect("the fresh code enrolls");
}

#[test]
fn surface_attach_mirrors_an_unbound_global_orchestrator_session() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &[]);
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    // The session was created on the home before it was paired to the Mac surface.
    let session = session_in(home.path(), &store, &node_name("home-1"), None, SESSION, &clock);
    let (surface, _token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&attach_frame(&surface, &session));
    wait_until(|| store.lease(&session).expect("lease lookup").is_some());
    let lease = store.lease(&session).expect("lease lookup").expect("held");
    assert_eq!(lease.surface_id.as_ref(), Some(&surface));
    assert_eq!(lease.node, name);

    let opened_at = clock.now();
    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &surface,
        &session,
        "turn-native-1",
        SurfaceTurnState::Started,
        opened_at,
    ));
    let (run_id, event_id, seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(event_id.is_some());
    assert!(seq.is_some());
    assert!(!duplicate, "a fresh turn start is not a duplicate");

    let events = [
        (
            "user-1",
            EventBody::UserMessage(vowp_protocol::UserMessage {
                text: "mirror me".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
        ),
        (
            "tool-call-1",
            EventBody::ToolCall(vowp_protocol::ToolCall {
                call_id: "call-1".to_owned(),
                name: "read".to_owned(),
                input: serde_json::json!({"path": "README.md"}),
            }),
        ),
        (
            "tool-result-1",
            EventBody::ToolResult(vowp_protocol::ToolResult {
                call_id: "call-1".to_owned(),
                ok: true,
                output: Some("ok".to_owned()),
                output_ref: None,
                exit_code: Some(0),
            }),
        ),
        (
            "assistant-1",
            EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
                text: "done".to_owned(),
            }),
        ),
        (
            "cost-1",
            EventBody::Cost(vowp_protocol::CostEvent {
                cost: Cost::receipt(
                    0.25,
                    vowp_protocol::Tokens {
                        input: 10,
                        output: 5,
                        ..vowp_protocol::Tokens::default()
                    },
                ),
                run_id: None,
            }),
        ),
    ];
    let mut first_user = None;
    for (native_id, body) in events {
        let start = gateway.frame_count();
        gateway.send(&surface_event_frame(&surface, &session, native_id, body));
        let (acked_run, event_id, seq, duplicate, _, _) = expect_ack(&gateway, start);
        assert_eq!(acked_run, run_id);
        assert!(event_id.is_some());
        assert!(seq.is_some());
        assert!(!duplicate);
        if native_id == "user-1" {
            first_user = Some((event_id.expect("event id"), seq.expect("seq")));
        }
    }

    // The same native id is skipped: no second log row, the same event id and seq.
    let log_before = read_log(home.path(), &session).len();
    let start = gateway.frame_count();
    gateway.send(&surface_event_frame(
        &surface,
        &session,
        "user-1",
        EventBody::UserMessage(vowp_protocol::UserMessage {
            text: "changed content".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    ));
    let (_, duplicate_id, duplicate_seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(duplicate, "a repeated native id is a duplicate");
    let (original_id, original_seq) = first_user.expect("user event ack");
    assert_eq!(duplicate_id, Some(original_id));
    assert_eq!(duplicate_seq, Some(original_seq));
    assert_eq!(read_log(home.path(), &session).len(), log_before);

    clock.advance(Duration::from_secs(5));
    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &surface,
        &session,
        "turn-native-1-end",
        SurfaceTurnState::Ended,
        clock.now(),
    ));
    let (ended_run, ended_event, ended_seq, duplicate, _, _) = expect_ack(&gateway, start);
    assert_eq!(ended_run, run_id);
    assert!(ended_event.is_some());
    assert!(ended_seq.is_some());
    assert!(!duplicate);

    let types = log_types(home.path(), &session);
    assert_eq!(
        types,
        vec![
            EventType::TurnStarted,
            EventType::UserMessage,
            EventType::ToolCall,
            EventType::ToolResult,
            EventType::AssistantMessage,
            EventType::Cost,
            EventType::TurnEnded,
        ]
    );
    let cost = read_log(home.path(), &session)
        .into_iter()
        .find(|event| event.event_type == EventType::Cost)
        .expect("cost row");
    let EventBody::Cost(cost) = cost.body().expect("cost body") else {
        panic!("expected a cost body");
    };
    assert_eq!(
        cost.run_id,
        Some(run_id.clone()),
        "the cost is attributed to the turn run"
    );

    let run = project_run(home.path(), &clock, "_node", &run_id);
    assert_eq!(run.node, name);
    assert_eq!(run.purpose, RunPurpose::Continue);
    assert_eq!(run.session_id.as_ref(), Some(&session));
    assert!(run.ended_at.is_some());
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.cost.usd, Some(0.25));
    assert_eq!(run.cost.tokens.input, 10);
    assert_eq!(
        store.require_session(&session).expect("session").state,
        SessionState::Idle
    );

    gateway.send(&detach_frame(&surface, &session));
    wait_until(|| store.lease(&session).expect("lease lookup").is_none());
    assert!(
        !gateway
            .output
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::Error { .. })),
        "a valid attach/turn/detach sequence produces no Error frame"
    );
    gateway.finish().expect("gateway");
}

#[test]
fn surface_turns_mirror_a_project_bound_session() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["vowp"]);
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = session_in(home.path(), &store, &node_name("home-1"), Some("vowp"), SESSION, &clock);
    let (surface, _token) = pair_surface(&store, &name, &session, "opencode", clock.now());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&attach_frame(&surface, &session));
    wait_until(|| store.lease(&session).expect("lease lookup").is_some());

    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &surface,
        &session,
        "turn-1",
        SurfaceTurnState::Started,
        clock.now(),
    ));
    let (run_id, _, _, _, _, _) = expect_ack(&gateway, start);
    let start = gateway.frame_count();
    gateway.send(&surface_event_frame(
        &surface,
        &session,
        "assistant-1",
        EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
            text: "bound".to_owned(),
        }),
    ));
    expect_ack(&gateway, start);
    let log_before = read_log(home.path(), &session).len();
    let start = gateway.frame_count();
    gateway.send(&surface_event_frame(
        &surface,
        &session,
        "assistant-1",
        EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
            text: "changed".to_owned(),
        }),
    ));
    let (_, _, _, duplicate, _, _) = expect_ack(&gateway, start);
    assert!(duplicate, "a project-bound duplicate native id is skipped");
    assert_eq!(read_log(home.path(), &session).len(), log_before);
    clock.advance(Duration::from_secs(2));
    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &surface,
        &session,
        "turn-1-end",
        SurfaceTurnState::Ended,
        clock.now(),
    ));
    expect_ack(&gateway, start);

    let run = project_run(home.path(), &clock, "vowp", &run_id);
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert!(run.ended_at.is_some());
    assert_eq!(
        store.require_session(&session).expect("session").state,
        SessionState::Idle
    );
    gateway.send(&detach_frame(&surface, &session));
    wait_until(|| store.lease(&session).expect("lease lookup").is_none());
    gateway.finish().expect("gateway");
}

#[test]
fn one_lease_enforcement_keeps_a_second_surface_read_only() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["vowp"]);
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = session_in(home.path(), &store, &node_name("home-1"), Some("vowp"), SESSION, &clock);
    let (first, first_token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let (second, second_token) = pair_surface(&store, &name, &session, "claude", clock.now());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&attach_frame(&first, &session));
    wait_until(|| {
        store
            .lease(&session)
            .expect("lease lookup")
            .is_some_and(|lease| lease.surface_id.as_ref() == Some(&first))
    });

    let start = gateway.frame_count();
    gateway.send(&attach_frame(&second, &session));
    let AgentFrame::LeaseRevoked { holder, .. } =
        gateway.expect_after(start, |frame| matches!(frame, AgentFrame::LeaseRevoked { .. }))
    else {
        panic!("expected LeaseRevoked for the second surface");
    };
    assert_eq!(holder, "opencode");
    assert_eq!(
        store
            .lease(&session)
            .expect("lease lookup")
            .expect("held")
            .surface_id
            .as_ref(),
        Some(&first),
        "an attach from a second surface must not overwrite the live lease"
    );

    // Read tools still work read-only for the second surface.
    for (request, tool, arguments) in [
        ("read-status", "fleet_status", serde_json::json!({})),
        ("read-resume", "session_resume", serde_json::json!({})),
    ] {
        let start = gateway.frame_count();
        gateway.send(&tool_call_frame(&second, &session, request, tool, arguments));
        let (request_id, result, error) = expect_tool_result(&gateway, start);
        assert_eq!(request_id, request);
        assert!(error.is_none(), "{tool} returned {error:?}");
        assert!(tool_succeeded(result.as_ref().expect("tool result")), "{tool} failed");
    }

    // A write from the read-only surface carries the exact takeover notice.
    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &second,
        &session,
        "write-capacity",
        "capacity_set",
        serde_json::json!({"target": 1}),
    ));
    let (request_id, result, error) = expect_tool_result(&gateway, start);
    assert_eq!(request_id, "write-capacity");
    assert!(result.is_none());
    let error = error.expect("a write from a read-only surface is refused");
    assert_eq!(error.code, "refused");
    assert!(error.message.starts_with("Taken over by opencode"), "{}", error.message);
    assert_eq!(
        store
            .lease(&session)
            .expect("lease lookup")
            .expect("held")
            .surface_id
            .as_ref(),
        Some(&first),
        "a refused write must not steal the lease"
    );

    // An explicit takeover moves the lease to the second surface and fences the first.
    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &second,
        &session,
        "takeover",
        "session_resume",
        serde_json::json!({"takeover": true}),
    ));
    gateway.expect_after(
        start,
        |frame| matches!(frame, AgentFrame::LeaseRevoked { holder, .. } if holder == "claude"),
    );
    let (request_id, result, error) = expect_tool_result(&gateway, start);
    assert_eq!(request_id, "takeover");
    assert!(error.is_none(), "the takeover resume failed: {error:?}");
    assert!(tool_succeeded(result.as_ref().expect("tool result")));
    assert_eq!(
        store
            .lease(&session)
            .expect("lease lookup")
            .expect("held")
            .surface_id
            .as_ref(),
        Some(&second),
        "the takeover transfers the lease to the second surface"
    );

    // The old token is untouched by the assertion above; keep both values live so the
    // compiler and readers see that only scoped tokens were used.
    assert_eq!(first_token.len(), second_token.len());
    gateway.finish().expect("gateway");
}

#[test]
fn surface_tool_calls_work_on_a_multi_project_home() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["vowp", "stackspirit"]);
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = session_in(home.path(), &store, &node_name("home-1"), None, SESSION, &clock);
    let (surface, _token) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let bound_session = session_in(
        home.path(),
        &store,
        &node_name("home-1"),
        Some("vowp"),
        "01ARZ3NDEKTSV4RRFFQ69G5FAW",
        &clock,
    );
    let (bound_surface, _bound_token) = pair_surface(&store, &name, &bound_session, "opencode", clock.now());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    gateway.send(&attach_frame(&surface, &session));
    gateway.send(&attach_frame(&bound_surface, &bound_session));
    wait_until(|| {
        store
            .lease(&session)
            .expect("lease lookup")
            .is_some_and(|lease| lease.surface_id.as_ref() == Some(&surface))
            && store
                .lease(&bound_session)
                .expect("lease lookup")
                .is_some_and(|lease| lease.surface_id.as_ref() == Some(&bound_surface))
    });

    for (request, tool, arguments) in [
        ("status", "fleet_status", serde_json::json!({})),
        ("resume", "session_resume", serde_json::json!({})),
        ("capacity", "capacity_set", serde_json::json!({"target": 2})),
    ] {
        let start = gateway.frame_count();
        gateway.send(&tool_call_frame(&surface, &session, request, tool, arguments));
        let (request_id, result, error) = expect_tool_result(&gateway, start);
        assert_eq!(request_id, request);
        assert!(error.is_none(), "{tool}: {error:?}");
        assert!(tool_succeeded(result.as_ref().expect("tool result")), "{tool} failed");
    }

    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &surface,
        &session,
        "job-add",
        "job_add",
        serde_json::json!({
            "project": "vowp",
            "id": "job-one",
            "title": "one job",
            "kind": "build",
            "role": "worker"
        }),
    ));
    let (request_id, result, error) = expect_tool_result(&gateway, start);
    assert_eq!(request_id, "job-add");
    assert!(error.is_none(), "job_add failed: {error:?}");
    assert!(tool_succeeded(result.as_ref().expect("tool result")));
    let vowp = Store::open_with_project(
        &NodePaths::new(home.path()),
        &ProjectName::parse("vowp").expect("project"),
    )
    .expect("vowp store");
    assert!(
        vowp.job(&JobId::parse("job-one").expect("job id"))
            .expect("job lookup")
            .is_some(),
        "job_add with an explicit project wrote the job to vowp"
    );
    assert!(
        home.path()
            .join("vowp-checkout")
            .join(".vowp/jobs/job-one/brief.md")
            .is_file(),
        "job_add wrote the brief under the project checkout"
    );

    // A bound session defaults to its own project; another project is refused.
    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &bound_surface,
        &bound_session,
        "job-default",
        "job_add",
        serde_json::json!({"id": "job-two", "title": "two", "kind": "build", "role": "worker"}),
    ));
    let (_, result, error) = expect_tool_result(&gateway, start);
    assert!(error.is_none(), "a bound session defaults to its project: {error:?}");
    assert!(tool_succeeded(result.as_ref().expect("tool result")));
    assert!(
        vowp.job(&JobId::parse("job-two").expect("job id"))
            .expect("job lookup")
            .is_some()
    );

    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &bound_surface,
        &bound_session,
        "job-other",
        "job_add",
        serde_json::json!({
            "project": "stackspirit",
            "id": "job-three",
            "title": "three",
            "kind": "build",
            "role": "worker"
        }),
    ));
    let (_, result, error) = expect_tool_result(&gateway, start);
    assert!(result.is_none());
    let error = error.expect("a bound surface cannot write another project");
    assert_eq!(error.code, "refused");

    // A project registered after the gateway opened is opened on first use.
    register_projects(home.path(), &clock, &["late"]);
    let late_session = session_in(
        home.path(),
        &store,
        &node_name("home-1"),
        Some("late"),
        "01ARZ3NDEKTSV4RRFFQ69G5FAX",
        &clock,
    );
    let (late_surface, _) = pair_surface(&store, &name, &late_session, "opencode", clock.now());
    gateway.send(&attach_frame(&late_surface, &late_session));
    wait_until(|| store.lease(&late_session).expect("lease lookup").is_some());
    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &late_surface,
        &late_session,
        "late-turn",
        SurfaceTurnState::Started,
        clock.now(),
    ));
    let (late_run, _, _, _, _, _) = expect_ack(&gateway, start);
    let start = gateway.frame_count();
    gateway.send(&surface_turn_frame(
        &late_surface,
        &late_session,
        "late-turn-end",
        SurfaceTurnState::Ended,
        clock.now(),
    ));
    expect_ack(&gateway, start);
    let late = project_run(home.path(), &clock, "late", &late_run);
    assert_eq!(late.outcome, Some(RunOutcome::Ok));
    assert!(late.ended_at.is_some());
    gateway.finish().expect("gateway");
}

#[test]
fn surface_tool_calls_refuse_a_revoked_or_expired_surface_token() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    register_projects(home.path(), &clock, &["vowp"]);
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let store = test_store(home.path(), &clock);
    let session = session_in(home.path(), &store, &node_name("home-1"), Some("vowp"), SESSION, &clock);
    let (revoked_surface, _) = pair_surface(&store, &name, &session, "opencode", clock.now());
    let revoked_tokens = store.list_surface_tokens(&revoked_surface).expect("tokens");
    store
        .revoke_surface_token(&revoked_tokens[0].id, clock.now())
        .expect("revoke token");
    let (expired_surface, _) = pair_surface(&store, &name, &session, "claude", clock.now());

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);

    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &revoked_surface,
        &session,
        "revoked",
        "fleet_status",
        serde_json::json!({}),
    ));
    let (_, result, error) = expect_tool_result(&gateway, start);
    assert!(result.is_none());
    let error = error.expect("a revoked token is refused");
    assert_eq!(error.code, "refused");
    assert!(
        error.message.starts_with("surface token"),
        "vowp-agent recognizes a surface-token refusal by this prefix: {}",
        error.message
    );

    clock.advance(Duration::from_secs(31 * 24 * 60 * 60));
    let start = gateway.frame_count();
    gateway.send(&tool_call_frame(
        &expired_surface,
        &session,
        "expired",
        "fleet_status",
        serde_json::json!({}),
    ));
    let (_, result, error) = expect_tool_result(&gateway, start);
    assert!(result.is_none());
    let error = error.expect("an expired token is refused");
    assert_eq!(error.code, "refused");
    assert!(
        error.message.starts_with("surface token"),
        "the refusal starts with the prefix vowp-agent checks: {}",
        error.message
    );
    assert!(error.message.contains("expired"), "{}", error.message);
    gateway.finish().expect("gateway");
}

#[test]
fn superseded_orchestrator_result_retires_the_stale_outbox_file() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);
    let log = Arc::new(MemoryLog::new());
    let deps = test_deps(home.path(), clock.clone(), log.clone());
    let orchestrator = Orchestrator::new_queued(deps);
    let info = orchestrator
        .create_on_node(
            &NewOrchestrator {
                name: "sandbox".to_owned(),
                project: None,
                app: OrchestratorApp::Chat,
                executor: None,
                force: false,
            },
            Some(name.clone()),
        )
        .expect("remote orchestrator");
    let outbox = home.path().join("agent-commands").join(name.as_str());
    let first = read_outbox_request(&outbox);
    let pending = CommandSpool::with_timeout(home.path(), Duration::ZERO)
        .pending(&name)
        .expect("pending commands");
    assert_eq!(pending.len(), 1);
    assert!(
        orchestrator
            .is_command_current(&name, &pending[0])
            .expect("current check"),
        "the initial start is current: {:?}",
        pending[0]
    );
    orchestrator.resume("sandbox").expect("resume on the remote node");
    let second = outbox_request_ids(&outbox)
        .into_iter()
        .find(|request_id| request_id != &first)
        .expect("the resume command is queued next to the superseded start");
    assert_ne!(first, second, "resume assigned a new request id");

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let AgentFrame::OrchestratorCommand(command) =
        gateway.expect(|frame| matches!(frame, AgentFrame::OrchestratorCommand(_)))
    else {
        panic!("expected OrchestratorCommand");
    };
    assert_eq!(command.request_id(), second, "only the current command is sent");
    wait_until(|| !outbox.join(format!("{first}.json")).exists());
    assert!(outbox.join(format!("{second}.json")).exists());

    // A stale result is accepted as "no record names it" and retires its command.
    let spool = CommandSpool::with_timeout(home.path(), Duration::ZERO);
    spool
        .send(
            &name,
            &vowp_protocol::OrchestratorCommand::Resume {
                request_id: first.clone(),
                name: "sandbox".to_owned(),
            },
        )
        .expect("queue the stale command");
    gateway.send(&AgentFrame::OrchestratorResult(OrchestratorResult::accepted(
        first.clone(),
        OrchestratorNodeState::Running,
    )));
    wait_until(|| !outbox.join(format!("{first}.json")).exists());

    // The current request id still publishes its result and retires its command.
    gateway.send(&AgentFrame::OrchestratorResult(OrchestratorResult::accepted(
        second.clone(),
        OrchestratorNodeState::Running,
    )));
    let inbox = home.path().join("agent-results").join(name.as_str());
    wait_until(|| inbox.join(format!("{second}.json")).exists());
    wait_until(|| !outbox.join(format!("{second}.json")).exists());
    assert!(
        !gateway
            .output
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::Error { .. })),
        "a superseded result is not an error"
    );
    gateway.finish().expect("gateway");
    let _ = info;
}

/// Lists the request ids waiting in one node's command outbox.
fn outbox_request_ids(outbox: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(outbox)
        .expect("outbox")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

/// Reads the single request id waiting in one node's command outbox.
fn read_outbox_request(outbox: &Path) -> String {
    let ids = outbox_request_ids(outbox);
    assert_eq!(ids.len(), 1, "expected one queued command, saw {ids:?}");
    ids[0].clone()
}

#[test]
fn unknown_surface_attach_keeps_the_connection_open() {
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let name = node_name("mac-studio");
    let key = signing_key();
    activate(home.path(), &clock, &name, &key);

    let gateway = Gateway::start_global(home.path(), clock.clone());
    authenticate(&gateway, &name, &key);
    let start = gateway.frame_count();
    gateway.send(&attach_frame(
        &SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned()),
        &SessionId::parse(SESSION).expect("session"),
    ));
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    assert!(
        !gateway
            .output
            .frames()
            .iter()
            .any(|frame| matches!(frame, AgentFrame::Error { .. })),
        "an unpaired surface attach must not send a connection-level Error: the agent treats one as terminal"
    );
    gateway.finish().expect("gateway");
}

/// One offloaded job dispatched through the gateway and accepted by the fake agent.
struct OwnedRun {
    home: TempDir,
    clock: Arc<FakeClock>,
    store: Store,
    gateway: Gateway,
    name: NodeName,
    assignment: vowp_protocol::AssignmentPayload,
    token: String,
    job: JobId,
}

/// Dispatches one ready job to `mac-studio` and sends `RunAccepted` for it.
fn owned_run() -> OwnedRun {
    let key = signing_key();
    let name = node_name("mac-studio");
    let home = TempDir::new().expect("temp home");
    let clock = Arc::new(FakeClock::at(ts()));
    let project = ProjectName::parse("vowp").expect("project");
    let store = test_store(home.path(), &clock);
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: home.path().join("project").display().to_string(),
            remote_url: Some("https://example.invalid/acme/vowp.git".to_owned()),
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: ts(),
            updated_at: ts(),
        })
        .expect("project row");
    activate(home.path(), &clock, &name, &key);
    store
        .upsert_node_path(&NodePath {
            node: name.clone(),
            project: project.clone(),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: "/srv/vowp-wt".to_owned(),
            allow: Vec::new(),
            updated_at: ts(),
        })
        .expect("path map");
    std::fs::create_dir_all(home.path().join("project")).expect("project dir");
    let job = insert_ready_offload_job(&store, &project, "owned-job");
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.project.path = Some(home.path().join("project").display().to_string());
    config.offload.enabled = true;
    config.offload.default = Placement::Any;
    // A fake keeper answers the capsule turn a takeover asks for; no real harness runs.
    let draft = serde_json::json!({
        "intent": "owned job",
        "constraints": [],
        "decisions": [],
        "state": {"done": [], "in_progress": ["work"], "blocked": [], "next_steps": []},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string();
    let keeper = support::fake_adapter::FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
            text: draft,
        })],
    );
    let log = Arc::new(MemoryLog::new());
    let mut deps = test_deps_with_config(home.path(), clock.clone(), log.clone(), config);
    deps.adapters = Arc::new(AdapterRegistry::new().with("opencode", Arc::new(keeper)));
    let gateway = Gateway::start_with_deps(home.path(), clock.clone(), log, deps);
    authenticate_within(&gateway, &name, &key, GATEWAY_WAIT);
    gateway.send(&offload_heartbeat(0, 2));
    let AgentFrame::AssignRun {
        assignment,
        lease_token,
        ..
    } = gateway.expect(|frame| matches!(frame, AgentFrame::AssignRun { .. }))
    else {
        panic!("expected AssignRun");
    };
    gateway.send(&AgentFrame::RunAccepted {
        assignment_id: assignment.assignment_id.clone(),
        run_id: assignment.run_id.clone(),
        epoch: assignment.epoch,
    });
    // The Pong proves the RunAccepted above has been applied.
    let start = gateway.frame_count();
    gateway.send(&AgentFrame::Ping);
    gateway.expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    OwnedRun {
        home,
        clock,
        store,
        gateway,
        name,
        assignment,
        token: lease_token,
        job,
    }
}

impl OwnedRun {
    /// Sends one frame and waits until the gateway has processed it.
    fn send_and_sync(&self, frame: &AgentFrame) {
        let start = self.gateway.frame_count();
        self.gateway.send(frame);
        self.gateway.send(&AgentFrame::Ping);
        self.gateway
            .expect_after(start, |frame| matches!(frame, AgentFrame::Pong));
    }

    fn run_state(&self) -> AgentFrame {
        AgentFrame::RunState {
            run_id: self.assignment.run_id.clone(),
            epoch: self.assignment.epoch,
            state: RemoteRunState::Running,
            spool_bytes: 0,
            head_commit: None,
            detail: "working".to_owned(),
        }
    }

    fn owner(&self) -> Option<vowp_store::repo_surfaces::OwnerRecord> {
        self.store.ownership(&self.assignment.session_id).expect("ownership")
    }
}

#[test]
fn run_state_renews_the_job_ownership_expiry() {
    let run = owned_run();
    let first = run.owner().expect("job owner").expires_at.expect("job expiry");

    // 500 s in, the run reports state: the owner and the assignment both move out.
    run.clock.advance(Duration::from_secs(500));
    run.send_and_sync(&run.run_state());
    let renewed = run.owner().expect("job owner").expires_at.expect("job expiry");
    assert_eq!(renewed, run.clock.now().plus(vowp_core::agent::dispatch::RUN_LEASE_TTL));
    assert!(renewed > first, "RunState extends the owner past the issue-time expiry");

    // Past the original expiry the renewed owner still writes.
    run.clock.advance(Duration::from_secs(200));
    vowp_core::surface::check_lease_with(&run.store, &run.assignment.session_id, Some(&run.token))
        .expect("a renewed job owner still writes after the first expiry");
    run.gateway.finish().expect("gateway");
}

#[test]
fn a_late_run_frame_after_expiry_is_fenced() {
    let run = owned_run();
    let session = run.assignment.session_id.clone();
    let hello = AgentFrame::RemoteLine {
        run_id: run.assignment.run_id.clone(),
        epoch: run.assignment.epoch,
        line: hello_line(&session, &run.assignment.run_id, &run.token),
    };
    run.send_and_sync(&hello);

    // The lease runs out with no activity; the next heartbeat sweeps and fences the run.
    run.clock
        .advance(vowp_core::agent::dispatch::RUN_LEASE_TTL + Duration::from_secs(1));
    let start = run.gateway.frame_count();
    run.gateway.send(&offload_heartbeat(1, 2));
    run.gateway
        .expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    let old = run
        .store
        .assignments_for_run(&run.assignment.run_id)
        .expect("assignments")
        .into_iter()
        .find(|row| row.epoch == run.assignment.epoch)
        .expect("old assignment");
    assert_eq!(old.state, vowp_store::repo_offload::AgentAssignmentState::Lost);

    // A frame from the fenced run is refused and never appended.
    let late = EventId::new();
    let start = run.gateway.frame_count();
    run.gateway
        .send(&event_line(&run.assignment.run_id, assistant_event(&late, "too late")));
    run.gateway
        .expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    // The old token no longer writes: the expired owner is gone and the re-dispatched
    // assignment has a new one.
    vowp_core::surface::check_lease_with(&run.store, &session, Some(&run.token))
        .expect_err("the fenced run's token is refused");
    run.gateway.finish().expect("gateway");
    let events = read_log(run.home.path(), &session);
    assert!(
        !events.iter().any(|event| event.id == late),
        "a fenced run appends nothing"
    );
}

#[test]
fn interactive_takeover_of_a_job_fences_the_run_and_writes_a_capsule() {
    let run = owned_run();
    let session = run.assignment.session_id.clone();
    run.send_and_sync(&AgentFrame::RemoteLine {
        run_id: run.assignment.run_id.clone(),
        epoch: run.assignment.epoch,
        line: hello_line(&session, &run.assignment.run_id, &run.token),
    });
    run.send_and_sync(&event_line(
        &run.assignment.run_id,
        assistant_event(&EventId::new(), "work before the takeover"),
    ));
    let (phone, phone_token) = pair_surface(&run.store, &run.name, &session, "opencode", run.clock.now());

    // Without --takeover the live job owner is never displaced.
    let start = run.gateway.frame_count();
    run.gateway
        .send(&acquire_frame(&phone, &session, &phone_token, LeaseMode::Acquire));
    run.gateway
        .expect_after(start, |frame| matches!(frame, AgentFrame::Error { .. }));
    assert_eq!(
        run.owner().expect("owner").mode,
        vowp_protocol::OwnershipMode::Job,
        "a plain acquire keeps the running job's owner"
    );

    // An explicit takeover fences the run and takes interactive ownership.
    let start = run.gateway.frame_count();
    run.gateway
        .send(&acquire_frame(&phone, &session, &phone_token, LeaseMode::Takeover));
    run.gateway
        .expect_after(start, |frame| matches!(frame, AgentFrame::RunLost { .. }));
    expect_grant(&run.gateway, start);
    let owner = run.owner().expect("owner");
    assert_eq!(owner.mode, vowp_protocol::OwnershipMode::Interactive);
    assert_eq!(owner.lease.surface_id.as_ref(), Some(&phone));

    let assignment = run
        .store
        .assignments_for_run(&run.assignment.run_id)
        .expect("assignments")
        .pop()
        .expect("assignment");
    assert_eq!(assignment.state, vowp_store::repo_offload::AgentAssignmentState::Lost);
    assert!(assignment.detail.contains("taken over"), "{:?}", assignment.detail);
    let job = run.store.require_job(&run.job).expect("job");
    assert_eq!(job.state, JobState::Ready, "the job is left ready, not switched");
    let session_row = run.store.require_session(&session).expect("session");
    let notes_now: Vec<String> = read_log(run.home.path(), &session)
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::Note(note)) => Some(note.text),
            _ => None,
        })
        .collect();
    assert!(
        session_row.last_capsule_id.is_some(),
        "the takeover wrote a capsule: {notes_now:?} {:?}",
        run.gateway.log.lines()
    );

    run.gateway.finish().expect("gateway");
    let notes: Vec<String> = read_log(run.home.path(), &session)
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::Note(note)) => Some(note.text),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes.iter().filter(|text| text.contains("taken over by")).count(),
        1,
        "one takeover note: {notes:?}"
    );
}
