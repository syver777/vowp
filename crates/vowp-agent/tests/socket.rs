//! Local agent socket tests for the Mac-side agent.
//!
//! The fake home speaks the public [`Transport`]/[`FrameStream`] boundary in memory and a
//! real Unix socket is bound below a `tempfile` directory. No SSH, network, store, real
//! `~/.vowp-agent` or real harness process is touched.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vowp_agent::{
    AGENT_SOCKET_FILE, AgentClient, AgentConfig, AgentError, AgentSocket, AgentStatus, Cache,
    ConnectionState as AgentConnectionState, FrameRead, FrameStream, MAX_SOCKET_REQUEST_BYTES, NODE_KEY_FILE, NodeKey,
    RunOptions, RunOutcome, SystemAgentClock, TOOL_CALL_TIMEOUT, Transport,
};
use vowp_protocol::agent::surface::{ToolCall, ToolError};
use vowp_protocol::{AGENT_PROTOCOL, AgentFrame, AgentRole, NodeName, SessionId, SurfaceId, Timestamp};

const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SECRET: &str = "socket-argument-value-that-must-never-be-written";

fn surface() -> SurfaceId {
    SurfaceId(SURFACE.to_owned())
}

fn session() -> SessionId {
    SessionId::parse(SESSION).expect("session id")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Record tool calls and wait for the test to inspect them.
    Accept,
    /// Answer the first two tool calls in reverse arrival order.
    Reverse,
    /// Drop the connection instead of answering the first tool call.
    CloseOnCall,
    /// Answer with the home's `refused` tool error.
    Refuse,
    /// Answer every tool call with a small successful MCP read result.
    Answer,
    /// Answer the first tool call with a successful read, then close the connection.
    AnswerThenClose,
    /// Answer with a protocol-successful MCP envelope whose `isError` is true.
    ErrorEnvelope,
}

#[derive(Default)]
struct ConnectionState {
    frames: VecDeque<AgentFrame>,
    closed: bool,
}

struct Connection {
    state: Mutex<ConnectionState>,
    changed: Condvar,
}

impl Connection {
    fn new() -> Self {
        Self {
            state: Mutex::new(ConnectionState::default()),
            changed: Condvar::new(),
        }
    }

    fn push(&self, frame: AgentFrame) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames.push_back(frame);
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.changed.notify_all();
    }

    fn recv(&self, timeout: Option<Duration>) -> FrameRead {
        let deadline = timeout.map(|duration| Instant::now() + duration);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(frame) = state.frames.pop_front() {
                return FrameRead::Frame(frame);
            }
            if state.closed {
                return FrameRead::Closed;
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return FrameRead::Timeout;
                    }
                    let (guard, result) = self
                        .changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(|poison| poison.into_inner());
                    state = guard;
                    if result.timed_out() && state.frames.is_empty() && !state.closed {
                        return FrameRead::Timeout;
                    }
                }
                None => {
                    state = self.changed.wait(state).unwrap_or_else(|poison| poison.into_inner());
                }
            }
        }
    }
}

#[derive(Default)]
struct HomeState {
    connections: usize,
    received: Vec<AgentFrame>,
    current: Option<Arc<Connection>>,
    pending_calls: Vec<ToolCall>,
    revoke_after_welcome: Option<usize>,
    answered: usize,
}

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<HomeState>>,
    mode: Mode,
    heartbeat_secs: u64,
}

impl FakeHome {
    fn new(mode: Mode) -> Self {
        Self {
            shared: Arc::new(Mutex::new(HomeState::default())),
            mode,
            heartbeat_secs: 10,
        }
    }

    fn with_heartbeat_secs(mut self, heartbeat_secs: u64) -> Self {
        self.heartbeat_secs = heartbeat_secs;
        self
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn revoke(&self) {
        let current = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .clone();
        if let Some(connection) = current {
            connection.push(AgentFrame::Revoked {
                reason: "the test revoked the node".to_owned(),
            });
        }
    }

    fn revoke_after_welcome(&self, connection: usize) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revoke_after_welcome = Some(connection);
    }

    /// Stops the run whether the client is connected now or reconnects later.
    fn stop(&self) {
        let current = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.revoke_after_welcome = Some(state.connections + 1);
            state.current.clone()
        };
        if let Some(connection) = current {
            connection.push(AgentFrame::Revoked {
                reason: "the test stopped the home".to_owned(),
            });
        }
    }

    fn revoke_surface(&self, surface: SurfaceId) {
        let current = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .clone();
        if let Some(connection) = current {
            connection.push(AgentFrame::SurfaceRevoked {
                surface_id: surface,
                reason: "the test revoked the surface".to_owned(),
            });
        }
    }
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let (number, connection) = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connections += 1;
            let connection = Arc::new(Connection::new());
            state.current = Some(Arc::clone(&connection));
            (state.connections, connection)
        };
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            connection,
            number,
            mode: self.mode,
            heartbeat_secs: self.heartbeat_secs,
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<HomeState>>,
    connection: Arc<Connection>,
    number: usize,
    mode: Mode,
    heartbeat_secs: u64,
}

impl FakeStream {
    fn handle_tool_call(&self, call: &ToolCall) {
        match self.mode {
            Mode::Accept => {}
            Mode::Reverse => {
                let respond = {
                    let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.pending_calls.push(call.clone());
                    if state.pending_calls.len() >= 2 {
                        Some(state.pending_calls.drain(..).rev().collect::<Vec<_>>())
                    } else {
                        None
                    }
                };
                if let Some(calls) = respond {
                    for pending in calls {
                        let value = pending.arguments.get("value").cloned().unwrap_or(Value::Null);
                        self.connection
                            .push(AgentFrame::ToolResult(vowp_protocol::agent::surface::ToolResult {
                                request_id: pending.request_id,
                                result: Some(json!({ "value": value })),
                                error: None,
                            }));
                    }
                }
            }
            Mode::CloseOnCall => self.connection.close(),
            Mode::Refuse => self
                .connection
                .push(AgentFrame::ToolResult(vowp_protocol::agent::surface::ToolResult {
                    request_id: call.request_id.clone(),
                    result: None,
                    error: Some(ToolError {
                        code: "refused".to_owned(),
                        message: "surface is not attached".to_owned(),
                        retryable: false,
                    }),
                })),
            Mode::Answer => self
                .connection
                .push(AgentFrame::ToolResult(vowp_protocol::agent::surface::ToolResult {
                    request_id: call.request_id.clone(),
                    result: Some(read_result()),
                    error: None,
                })),
            Mode::AnswerThenClose => {
                let first = {
                    let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let first = state.answered == 0;
                    state.answered += 1;
                    first
                };
                if first {
                    self.connection
                        .push(AgentFrame::ToolResult(vowp_protocol::agent::surface::ToolResult {
                            request_id: call.request_id.clone(),
                            result: Some(read_result()),
                            error: None,
                        }));
                }
                self.connection.close();
            }
            Mode::ErrorEnvelope => {
                self.connection
                    .push(AgentFrame::ToolResult(vowp_protocol::agent::surface::ToolResult {
                        request_id: call.request_id.clone(),
                        result: Some(json!({
                            "content": [{"type": "text", "text": "refused: not attached"}],
                            "isError": true,
                        })),
                        error: None,
                    }))
            }
        }
    }
}

/// The successful home result for one cacheable read.
fn read_result() -> Value {
    json!({
        "content": [{"type": "text", "text": "{\"nodes\":[]}"}],
        "isError": false,
    })
}

impl FrameStream for FakeStream {
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        Ok(self.connection.recv(timeout))
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { .. } => {
                self.connection.push(AgentFrame::HomeChallenge {
                    nonce: format!("nonce-{}", self.number),
                    home_id: "home-1".to_owned(),
                    connection_id: format!("connection-{}", self.number),
                    server_time: Timestamp::from_unix_secs(1_700_000_000),
                    heartbeat_secs: self.heartbeat_secs,
                    max_frame_bytes: MAX_SOCKET_REQUEST_BYTES as u64,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                let revoke = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .revoke_after_welcome
                    .is_some_and(|after| self.number >= after);
                self.connection.push(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node slug"),
                    connection_id: format!("connection-{}", self.number),
                });
                if revoke {
                    self.connection.push(AgentFrame::Revoked {
                        reason: "the test revoked the node".to_owned(),
                    });
                }
            }
            AgentFrame::ToolCall(call) => self.handle_tool_call(call),
            _ => {}
        }
        Ok(())
    }
}

type RunHandle = JoinHandle<Result<RunOutcome, AgentError>>;

fn start_run(dir: &Path, home: FakeHome) -> RunHandle {
    start_run_with_timeout(dir, home, TOOL_CALL_TIMEOUT)
}

fn start_run_with_timeout(dir: &Path, home: FakeHome, timeout: Duration) -> RunHandle {
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        dir.to_path_buf(),
        "home.example",
    );
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir).expect("node key");
    let mut client = AgentClient::new(
        dir.to_path_buf(),
        key,
        config,
        Box::new(home),
        Arc::new(SystemAgentClock),
    )
    .with_tool_call_timeout(timeout);
    thread::spawn(move || client.run(&RunOptions::new(AgentRole::Both)))
}

fn stop_run(home: &FakeHome, run: RunHandle) {
    home.revoke();
    let outcome = run.join().expect("run thread").expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");
}

fn wait_for_frames<F>(home: &FakeHome, predicate: F) -> Vec<AgentFrame>
where
    F: Fn(&[AgentFrame]) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frames = home.frames();
        if predicate(&frames) {
            return frames;
        }
        assert!(Instant::now() < deadline, "timed out waiting for a home frame");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_connection(home: &FakeHome) {
    wait_for_frames(home, |frames| {
        frames.iter().any(|frame| matches!(frame, AgentFrame::Heartbeat { .. }))
    });
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "the agent socket was not bound");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until the agent has written a status document matching `predicate`.
fn wait_for_status<F>(dir: &Path, predicate: F) -> AgentStatus
where
    F: Fn(&AgentStatus) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(status)) = AgentStatus::load(dir)
            && predicate(&status)
        {
            return status;
        }
        assert!(Instant::now() < deadline, "timed out waiting for status.json");
        thread::sleep(Duration::from_millis(10));
    }
}

fn connect(path: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write timeout");
                return stream;
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "could not connect to the agent socket: {error}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn send_request(stream: &mut UnixStream, request: Value) {
    let mut line = serde_json::to_vec(&request).expect("request json");
    line.push(b'\n');
    stream.write_all(&line).expect("write request");
    stream.flush().expect("flush request");
}

fn read_response(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    let read = reader.read_line(&mut line).expect("read response");
    assert!(read > 0, "the server closed the socket before a response");
    serde_json::from_str(&line).expect("response json")
}

fn call_tool(path: &Path, request_id: &str, value: i64) -> Value {
    call_tool_named(path, request_id, "session.show", json!({ "value": value }))
}

fn call_tool_named(path: &Path, request_id: &str, tool: &str, arguments: Value) -> Value {
    let mut stream = connect(path);
    send_request(
        &mut stream,
        json!({
            "kind": "tool_call",
            "request_id": request_id,
            "surface": SURFACE,
            "session": SESSION,
            "tool": tool,
            "arguments": arguments,
        }),
    );
    let mut reader = BufReader::new(stream);
    read_response(&mut reader)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).expect("read state directory") {
            let entry = entry.expect("state entry");
            let file_type = entry.file_type().expect("state file type");
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() && entry.file_name().to_string_lossy() != NODE_KEY_FILE {
                // The generated test node key is a private value; it is not part of the
                // payload-leak check.
                files.push(entry.path());
            }
        }
    }
    files
}

#[test]
fn agent_socket_is_0600_and_refuses_a_second_live_server() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let socket = AgentSocket::bind(dir.path()).expect("bind");

    assert_eq!(socket.path(), path);
    let mode = fs::metadata(&path).expect("socket metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the agent socket must be 0600");

    let mut stream = connect(&path);
    send_request(&mut stream, json!({ "kind": "ping" }));
    let mut reader = BufReader::new(stream);
    assert_eq!(read_response(&mut reader), json!({ "kind": "pong" }));

    let second = AgentSocket::bind(dir.path());
    assert!(
        matches!(second.as_ref(), Err(AgentError::Config(_))),
        "a second live server must be refused"
    );
    drop(second);
    drop(reader);
    drop(socket);
    assert!(!path.exists(), "stopping the server removes its socket");

    let stale = UnixListener::bind(&path).expect("stale listener");
    drop(stale);
    assert!(path.exists(), "the dropped listener leaves a stale file");
    let rebound = AgentSocket::bind(dir.path()).expect("a stale socket file is replaced");
    assert_eq!(rebound.path(), path);

    let mut stream = connect(&path);
    send_request(&mut stream, json!({ "kind": "ping" }));
    let mut reader = BufReader::new(stream);
    assert_eq!(
        read_response(&mut reader),
        json!({ "kind": "pong" }),
        "the rebound server must serve the replaced socket"
    );
}

#[test]
fn agent_socket_answers_immediate_pings_on_many_connections() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = AgentSocket::bind(dir.path()).expect("bind");
    for index in 0..16 {
        // Send as soon as connect returns, before the accept thread may run.
        let mut stream = connect(socket.path());
        send_request(&mut stream, json!({ "kind": "ping" }));
        let mut reader = BufReader::new(stream);
        assert_eq!(
            read_response(&mut reader),
            json!({ "kind": "pong" }),
            "connection {index} did not receive a pong"
        );
    }
}

#[test]
fn agent_socket_refuses_a_regular_file_at_its_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    fs::write(&path, b"not an agent socket").expect("write regular file");

    let bound = AgentSocket::bind(dir.path());
    assert!(
        matches!(bound.as_ref(), Err(AgentError::Config(_))),
        "a regular file must be refused"
    );
    assert_eq!(
        fs::read(&path).expect("the regular file stays"),
        b"not an agent socket",
        "bind must never remove a non-socket file"
    );
}

#[test]
fn agent_socket_refuses_a_symlink_at_its_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("target");
    fs::write(&target, b"target contents").expect("write target");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    std::os::unix::fs::symlink(&target, &path).expect("symlink the socket path");

    let bound = AgentSocket::bind(dir.path());
    assert!(
        matches!(bound.as_ref(), Err(AgentError::Config(_))),
        "a symlink must be refused"
    );
    assert!(
        fs::symlink_metadata(&path)
            .expect("the symlink stays")
            .file_type()
            .is_symlink(),
        "bind must not replace the symlink"
    );
    assert_eq!(fs::read(&target).expect("the target stays"), b"target contents");
}

#[test]
fn socket_attach_and_detach_forward_the_surface_frames() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Accept);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let mut stream = connect(&path);
    send_request(
        &mut stream,
        json!({ "kind": "attach", "surface": SURFACE, "session": SESSION }),
    );
    let mut reader = BufReader::new(stream);
    assert_eq!(read_response(&mut reader), json!({ "kind": "ok" }));

    let frames = wait_for_frames(&home, |frames| {
        frames.iter().any(|frame| {
            matches!(
                frame,
                AgentFrame::SurfaceAttach(attach)
                    if attach.surface == surface() && attach.session == session()
            )
        })
    });
    assert!(
        frames.iter().any(|frame| matches!(frame, AgentFrame::SurfaceAttach(_))),
        "the attach frame reached the home"
    );

    send_request(
        reader.get_mut(),
        json!({ "kind": "detach", "surface": SURFACE, "session": SESSION }),
    );
    assert_eq!(read_response(&mut reader), json!({ "kind": "ok" }));
    wait_for_frames(&home, |frames| {
        frames.iter().any(|frame| {
            matches!(
                frame,
                AgentFrame::SurfaceDetach(detach)
                    if detach.surface == surface() && detach.session == session()
            )
        })
    });

    stop_run(&home, run);
}

#[test]
fn socket_tool_call_round_trips_and_matches_by_request_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Reverse);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let one_path = path.clone();
    let one = thread::spawn(move || call_tool(&one_path, "request-one", 1));
    let two_path = path.clone();
    let two = thread::spawn(move || call_tool(&two_path, "request-two", 2));

    let first = one.join().expect("first call");
    let second = two.join().expect("second call");
    for (response, request_id, value) in [(&first, "request-one", 1), (&second, "request-two", 2)] {
        assert_eq!(response["kind"], "tool_result");
        assert_eq!(response["request_id"], request_id);
        assert_eq!(response["result"]["value"], value);
        assert!(response.get("error").is_none());
    }
    assert_ne!(
        first["request_id"], second["request_id"],
        "each response must keep its own request id"
    );

    stop_run(&home, run);
}

#[test]
fn socket_tool_call_returns_home_offline_when_the_connection_drops() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::CloseOnCall);
    home.revoke_after_welcome(2);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let response = call_tool(&path, "request-offline", 7);
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["request_id"], "request-offline");
    assert_eq!(response["error"]["code"], "home_offline");
    assert_eq!(response["error"]["retryable"], true);

    let outcome = run.join().expect("run thread").expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");
}

#[test]
fn socket_tool_call_times_out_without_a_home_result() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Accept);
    let run = start_run_with_timeout(dir.path(), home.clone(), Duration::from_millis(150));
    wait_for_socket(&path);
    wait_for_connection(&home);

    let started = Instant::now();
    let response = call_tool(&path, "request-timeout", 3);
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["request_id"], "request-timeout");
    assert_eq!(response["error"]["code"], "tool_timeout");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the call must wait for the configured timeout"
    );

    stop_run(&home, run);
}

#[test]
fn socket_pending_tool_call_does_not_block_heartbeats() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Accept).with_heartbeat_secs(1);
    let run = start_run_with_timeout(dir.path(), home.clone(), Duration::from_secs(3));
    wait_for_socket(&path);
    wait_for_connection(&home);

    let call_path = path.clone();
    let call = thread::spawn(move || call_tool(&call_path, "request-slow", 9));
    // A heartbeat must arrive after the call reached the home and before its result.
    wait_for_frames(&home, |frames| {
        let Some(index) = frames
            .iter()
            .position(|frame| matches!(frame, AgentFrame::ToolCall(call) if call.request_id == "request-slow"))
        else {
            return false;
        };
        frames[index + 1..]
            .iter()
            .any(|frame| matches!(frame, AgentFrame::Heartbeat { .. }))
    });

    let response = call.join().expect("tool call thread");
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["error"]["code"], "tool_timeout");

    stop_run(&home, run);
}

#[test]
fn socket_rejects_an_unknown_kind_and_an_oversized_frame() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = AgentSocket::bind(dir.path()).expect("bind");
    let path = socket.path().to_path_buf();

    let mut stream = connect(&path);
    send_request(&mut stream, json!({ "kind": "not-a-request" }));
    let mut reader = BufReader::new(stream);
    let response = read_response(&mut reader);
    assert_eq!(response["kind"], "error");
    assert_eq!(response["code"], "unknown_kind");

    let mut stream = connect(&path);
    let mut oversized = vec![b' '; MAX_SOCKET_REQUEST_BYTES + 1];
    oversized[MAX_SOCKET_REQUEST_BYTES] = b'\n';
    let _ = stream.write_all(&oversized);
    let mut reader = BufReader::new(stream);
    let response = read_response(&mut reader);
    assert_eq!(response["kind"], "error");
    assert_eq!(response["code"], "frame_too_large");
}

#[test]
fn socket_never_logs_arguments_or_a_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Refuse);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let mut stream = connect(&path);
    send_request(
        &mut stream,
        json!({
            "kind": "tool_call",
            "request_id": "request-secret",
            "surface": SURFACE,
            "session": SESSION,
            "tool": "session.show",
            "arguments": { "value": SECRET },
        }),
    );
    let mut reader = BufReader::new(stream);
    let response = read_response(&mut reader);
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["error"]["code"], "refused");
    assert_eq!(response["error"]["message"], "surface is not attached");
    assert_eq!(response["error"]["retryable"], false);
    assert!(
        !response.to_string().contains(SECRET),
        "the local response must not echo the argument"
    );

    // The call itself reaches the home unchanged; only the local state directory is under
    // test, and it must never contain the argument or another secret-shaped value.
    wait_for_frames(&home, |frames| {
        frames.iter().any(|frame| match frame {
            AgentFrame::ToolCall(call) => {
                call.request_id == "request-secret"
                    && call.arguments.get("value").and_then(Value::as_str) == Some(SECRET)
            }
            _ => false,
        })
    });
    for path in files_under(dir.path()) {
        let bytes = fs::read(&path).expect("read state file");
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(SECRET), "{} contains the tool argument", path.display());
    }

    stop_run(&home, run);
}

/// Waits for one cached read and returns it.
fn wait_for_cached_read(dir: &Path, tool: &str, arguments: &Value) -> vowp_agent::CachedRead {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(cached)) = Cache::new(dir).lookup(tool, &surface(), arguments, Timestamp::now()) {
            return cached;
        }
        assert!(Instant::now() < deadline, "the successful read was not cached");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_cache_entries(dir: &Path, entries: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Cache::new(dir).stats().entries == entries {
            return;
        }
        assert!(Instant::now() < deadline, "the cache did not reach {entries} entries");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn mcp_tool_call_works_read_only_with_the_cache_when_the_home_is_offline() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::AnswerThenClose);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    // One successful read while connected is stored exactly as the home returned it.
    let arguments = json!({"project": "vowp"});
    let response = call_tool_named(&path, "request-cache-first", "fleet_status", arguments.clone());
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["result"], read_result());
    let cached = wait_for_cached_read(dir.path(), "fleet_status", &arguments);
    assert_eq!(cached.result, read_result(), "the exact home payload is stored");

    // The home closed the connection; the agent is either retrying or connected to a home
    // that drops the next call, and both answer from the cache.
    wait_for_status(dir.path(), |status| {
        matches!(
            status.connection,
            AgentConnectionState::Reconnecting | AgentConnectionState::Stopped
        )
    });

    let response = call_tool_named(&path, "request-cache-second", "fleet_status", arguments.clone());
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["request_id"], "request-cache-second");
    assert!(response.get("error").is_none(), "cached read failed: {response}");
    assert_eq!(response["result"]["content"][0]["text"], "{\"nodes\":[]}");
    let marker = response["result"]["content"][1]["text"]
        .as_str()
        .expect("the as-of line");
    assert_eq!(
        marker,
        format!(
            "as of {} (the home is unreachable; this is a cached read)",
            cached.stored_at.as_rfc3339()
        )
    );
    assert_eq!(response["result"]["isError"], false);

    // A different argument set is a miss and keeps the existing offline error.
    let miss = call_tool_named(
        &path,
        "request-cache-miss",
        "fleet_status",
        json!({"project": "another-project"}),
    );
    assert_eq!(miss["kind"], "tool_result");
    assert_eq!(miss["error"]["code"], "home_offline");
    assert_eq!(
        miss["error"]["message"],
        "the agent is not connected to the home; start `vowp agent run`"
    );

    home.stop();
    let outcome = run.join().expect("run thread").expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");
}

#[test]
fn cache_miss_returns_the_existing_home_offline_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = AgentSocket::bind(dir.path()).expect("bind");

    let response = call_tool_named(
        socket.path(),
        "request-cache-miss",
        "fleet_status",
        json!({"project": "vowp"}),
    );
    assert_eq!(response["kind"], "tool_result");
    assert_eq!(response["request_id"], "request-cache-miss");
    assert_eq!(response["error"]["code"], "home_offline");
    assert_eq!(
        response["error"]["message"],
        "the agent is not connected to the home; start `vowp agent run`"
    );
    assert_eq!(response["error"]["retryable"], true);
    assert_eq!(
        Cache::new(dir.path()).stats().entries,
        0,
        "a miss writes no cache entry"
    );
}

#[test]
fn cached_read_answers_while_the_agent_is_not_connected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = AgentSocket::bind(dir.path()).expect("bind");
    let arguments = json!({"project": "vowp"});
    let stored_at = Timestamp::now();
    Cache::new(dir.path())
        .store("fleet_status", &surface(), &arguments, &read_result(), stored_at)
        .expect("cache entry");

    let response = call_tool_named(socket.path(), "request-offline-cache", "fleet_status", arguments);
    assert!(response.get("error").is_none(), "cached read failed: {response}");
    assert_eq!(response["result"]["content"][0]["text"], "{\"nodes\":[]}");
    assert_eq!(
        response["result"]["content"][1]["text"],
        format!(
            "as of {} (the home is unreachable; this is a cached read)",
            stored_at.as_rfc3339()
        )
    );
    assert_eq!(response["result"]["isError"], false);
}

#[test]
fn cached_read_answers_a_tool_timeout() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Accept);
    let run = start_run_with_timeout(dir.path(), home.clone(), Duration::from_millis(150));
    wait_for_socket(&path);
    wait_for_connection(&home);

    let arguments = json!({"project": "vowp"});
    let stored_at = Timestamp::now();
    Cache::new(dir.path())
        .store("fleet_status", &surface(), &arguments, &read_result(), stored_at)
        .expect("cache entry");

    let started = Instant::now();
    let response = call_tool_named(&path, "request-timeout-cache", "fleet_status", arguments);
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the call waits for the configured tool timeout"
    );
    assert!(response.get("error").is_none(), "cached read failed: {response}");
    assert_eq!(
        response["result"]["content"][1]["text"],
        format!(
            "as of {} (the home is unreachable; this is a cached read)",
            stored_at.as_rfc3339()
        )
    );

    stop_run(&home, run);
}

#[test]
fn a_tool_error_envelope_is_not_cached() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::ErrorEnvelope);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let response = call_tool_named(
        &path,
        "request-error-envelope",
        "fleet_status",
        json!({"project": "vowp"}),
    );
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        Cache::new(dir.path()).stats().entries,
        0,
        "an MCP error envelope is not a successful read"
    );

    stop_run(&home, run);
}

#[test]
fn socket_surface_revoked_clears_the_cached_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(AGENT_SOCKET_FILE);
    let home = FakeHome::new(Mode::Answer);
    let run = start_run(dir.path(), home.clone());
    wait_for_socket(&path);
    wait_for_connection(&home);

    let arguments = json!({"project": "vowp"});
    let response = call_tool_named(&path, "request-cache-before-revoke", "fleet_status", arguments);
    assert_eq!(response["result"], read_result());
    wait_for_cache_entries(dir.path(), 1);

    home.revoke_surface(surface());
    wait_for_cache_entries(dir.path(), 0);

    stop_run(&home, run);
}
