//! Hermetic per-session SSE tests: a temporary home, a real loopback listener and real
//! streaming GETs.
//!
//! No test here touches the real home, the network beyond `127.0.0.1`, a tmux server or a
//! harness. Frames are decoded from the real HTTP chunked body, never from a mock server.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::Board;
use vowp_protocol::{
    Actor, Clock, EventBody, EventEnvelope, EventSource, FakeClock, HarnessKind, JobId, JobKind, NewJob, NodeName,
    Project, ProjectName, Role, Session, SessionId, SessionRole, SessionState, Timestamp, UserMessage, VowpConfig,
};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, StateChange, Store};

/// A temporary home with one project and one session that has three stored events.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    session_id: SessionId,
    envelopes: Vec<EventEnvelope>,
    clock: FakeClock,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(epoch());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.project.name = project.clone();
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: tmp.path().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("project row");

        let job = store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("job");
        store
            .set_job_state(
                &job.id,
                vowp_protocol::JobState::Running,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("job running");

        let session_id = SessionId::new();
        let log = SessionLog::create(&paths, &session_id, Arc::new(clock.clone())).expect("session log");
        let mut envelopes = Vec::new();
        for text in ["first", "second", "third"] {
            envelopes.push(append(&log, text).envelope);
        }
        let log_bytes = log.byte_len();
        drop(log);

        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project.clone()),
                job_id: Some(job.id),
                role: Some(SessionRole::Worker),
                title: "stream session".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Running,
                executor: None,
                last_seq: 3,
                last_capsule_id: None,
                log_bytes,
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("session row");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            session_id,
            envelopes,
            clock,
        }
    }

    /// Appends one live event through the real single-writer log handle.
    fn append_live(&self, text: &str) -> EventEnvelope {
        let log = SessionLog::open(&self.paths, &self.session_id, Arc::new(self.clock.clone())).expect("open log");
        let envelope = append(&log, text).envelope;
        drop(log);
        envelope
    }

    /// Removes the stored log, as session compaction does.
    fn remove_log(&self) {
        std::fs::remove_file(self.paths.log_path(&self.session_id)).expect("remove log");
    }

    /// Truncates the stored log to its first event, simulating a truncated file.
    fn truncate_to_first_event(&self) {
        let path = self.paths.log_path(&self.session_id);
        let bytes = std::fs::read(&path).expect("read log");
        let end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .expect("first line");
        let replacement = path.with_extension("rewrite");
        std::fs::write(&replacement, &bytes[..end]).expect("write replacement log");
        std::fs::rename(&replacement, &path).expect("replace log");
    }
}

/// A temporary home whose board has no live runs, so only a row change moves it.
struct BoardStreamFixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    pending_job: JobId,
}

impl BoardStreamFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(epoch());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("board-proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node;
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.validate().expect("valid fixture config");
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("project row");

        let pending = store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("pending job");
        let done = store
            .insert_job(&project, &new_job("job-b", JobKind::Test, Role::Verifier), "test")
            .expect("done job");
        store
            .set_job_state(
                &done.id,
                vowp_protocol::JobState::Done,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("done job state");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            pending_job: pending.id,
        }
    }

    /// Moves the pending job to done through a writable store.
    fn complete_pending_job(&self) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .set_job_state(
                &self.pending_job,
                vowp_protocol::JobState::Done,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("complete pending job");
        drop(store);
    }
}

fn append(log: &SessionLog, text: &str) -> vowp_store::session_log::AppendedEvent {
    log.append(NewEvent {
        actor: Actor::User,
        source: EventSource {
            harness: HarnessKind::ClaudeCode,
            model: Some("test-model".to_owned()),
            node: None,
            native_session_id: None,
        },
        body: EventBody::UserMessage(UserMessage {
            text: text.to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
        raw: None,
        ts: None,
    })
    .expect("append event")
}

fn new_job(id: &str, kind: JobKind, role: Role) -> NewJob {
    NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("{id} title"),
        kind,
        role,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    }
}

fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
}

/// A running server with short, injected stream intervals.
struct Server {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(fixture: &Fixture, poll: Duration, heartbeat: Duration) -> Self {
        let selection = select_project(&fixture.paths, None).expect("project selection");
        assert_eq!(selection.project, fixture.project);
        assert_eq!(selection.node, fixture.node);
        Self::start_with_selection(&fixture.paths, selection, poll, heartbeat)
    }

    fn start_for(paths: &NodePaths, poll: Duration, heartbeat: Duration) -> Self {
        let selection = select_project(paths, None).expect("project selection");
        Self::start_with_selection(paths, selection, poll, heartbeat)
    }

    fn start_with_selection(
        paths: &NodePaths,
        selection: vowp_daemon::ProjectSelection,
        poll: Duration,
        heartbeat: Duration,
    ) -> Self {
        let state = ApiState::new(paths, &selection)
            .with_stream_intervals(poll, heartbeat)
            .with_board_poll_interval(poll);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async move {
                let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("loopback bind");
                ready_tx
                    .send(listener.local_addr().expect("bound address"))
                    .expect("send address");
                serve_with_shutdown(listener, state, async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve");
            });
        });
        let address = ready_rx.recv_timeout(Duration::from_secs(10)).expect("server ready");
        Self {
            address,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One parsed SSE frame.
struct SseFrame {
    event: Option<String>,
    data: String,
}

impl SseFrame {
    fn parse(text: &str) -> Self {
        let mut event = None;
        let mut data = String::new();
        for line in text.lines() {
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event = Some(value.to_owned()),
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
                _ => {}
            }
        }
        Self { event, data }
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.data).unwrap_or_else(|error| panic!("invalid frame JSON: {error}\n{}", self.data))
    }
}

/// A real chunked HTTP/1.1 streaming client.
struct SseClient {
    stream: TcpStream,
    raw: Vec<u8>,
    body: Vec<u8>,
    chunked: bool,
    chunk: ChunkState,
    eof: bool,
    status: u16,
    content_type: Option<String>,
}

enum ChunkState {
    Size,
    Data(usize),
    Crlf,
}

impl SseClient {
    fn open(address: SocketAddr, path: &str) -> Self {
        let mut stream = TcpStream::connect(address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");

        let mut raw = Vec::new();
        let mut buffer = [0_u8; 4096];
        let head_end = loop {
            if let Some(end) = find(&raw, b"\r\n\r\n") {
                break end;
            }
            let read = stream.read(&mut buffer).expect("read headers");
            assert!(read > 0, "connection closed before response headers");
            raw.extend_from_slice(&buffer[..read]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse().ok())
            .expect("status line");
        let content_type = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_owned())
            })
            .expect("content type");
        let chunked = head.lines().any(|line| {
            line.to_ascii_lowercase().starts_with("transfer-encoding:") && line.to_ascii_lowercase().contains("chunked")
        });
        let raw = raw.split_off(head_end + 4);
        Self {
            stream,
            raw,
            body: Vec::new(),
            chunked,
            chunk: ChunkState::Size,
            eof: false,
            status,
            content_type: Some(content_type),
        }
    }

    /// Returns the next complete frame, or `None` when the connection closes or `timeout`
    /// expires.
    fn next_frame(&mut self, timeout: Duration) -> Option<SseFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            self.decode();
            if let Some(end) = find(&self.body, b"\n\n") {
                let frame = self.body.drain(..end + 2).collect::<Vec<_>>();
                return Some(SseFrame::parse(&String::from_utf8_lossy(&frame)));
            }
            if self.eof {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let wait = (deadline - now).min(Duration::from_millis(500));
            self.stream.set_read_timeout(Some(wait)).expect("read timeout");
            let mut buffer = [0_u8; 4096];
            match self.stream.read(&mut buffer) {
                Ok(0) => self.eof = true,
                Ok(read) => self.raw.extend_from_slice(&buffer[..read]),
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(error) => panic!("read SSE body: {error}"),
            }
        }
    }

    fn decode(&mut self) {
        if self.eof {
            return;
        }
        loop {
            if !self.chunked {
                self.body.extend_from_slice(&self.raw);
                self.raw.clear();
                return;
            }
            match self.chunk {
                ChunkState::Size => {
                    let Some(end) = find(&self.raw, b"\r\n") else {
                        return;
                    };
                    let size_text = String::from_utf8_lossy(&self.raw[..end]).into_owned();
                    let size_text = size_text.split(';').next().unwrap_or_default().trim();
                    let size = usize::from_str_radix(size_text, 16).expect("chunk size");
                    self.raw.drain(..end + 2);
                    if size == 0 {
                        self.eof = true;
                        return;
                    }
                    self.chunk = ChunkState::Data(size);
                }
                ChunkState::Data(remaining) => {
                    if remaining == 0 {
                        self.chunk = ChunkState::Crlf;
                        continue;
                    }
                    if self.raw.is_empty() {
                        return;
                    }
                    let take = remaining.min(self.raw.len());
                    self.body.extend_from_slice(&self.raw[..take]);
                    self.raw.drain(..take);
                    if take < remaining {
                        self.chunk = ChunkState::Data(remaining - take);
                        return;
                    }
                    self.chunk = ChunkState::Crlf;
                }
                ChunkState::Crlf => {
                    if self.raw.len() < 2 {
                        return;
                    }
                    assert_eq!(&self.raw[..2], b"\r\n", "chunk terminator");
                    self.raw.drain(..2);
                    self.chunk = ChunkState::Size;
                }
            }
        }
    }
}

/// One parsed non-streaming HTTP response.
struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    body: String,
}

fn get(address: SocketAddr, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let head_end = find(&response, b"\r\n\r\n").expect("response headers");
    let head = String::from_utf8_lossy(&response[..head_end]).into_owned();
    let body = decode_body(&head, &response[head_end + 4..]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("status line");
    let content_type = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-type")
            .then(|| value.trim().to_owned())
    });
    HttpResponse {
        status,
        content_type,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn decode_body(head: &str, body: &[u8]) -> Vec<u8> {
    let chunked = head.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    if !chunked {
        return body.to_vec();
    }
    let mut decoded = Vec::new();
    let mut rest = body;
    while let Some(end) = find(rest, b"\r\n") {
        let size_text = String::from_utf8_lossy(&rest[..end]).into_owned();
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).expect("chunk size");
        rest = &rest[end + 2..];
        if size == 0 {
            break;
        }
        decoded.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    decoded
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn stream_path(session_id: &SessionId, cursor: &str) -> String {
    format!("/api/v1/sessions/{session_id}/events/stream{cursor}")
}

fn assert_no_home_or_token(text: &str, home: &str) {
    assert!(!text.contains(home), "response leaked the home path: {text}");
    assert!(
        !text.to_ascii_lowercase().contains("token"),
        "response leaked a token: {text}"
    );
}

/// Asserts that `value` validates against the exported schema for `name`.
fn assert_schema(name: &str, value: &Value) {
    let schema =
        vowp_protocol::schema::schema_for_name(name).unwrap_or_else(|| panic!("schema {name} is not exported"));
    let validator =
        jsonschema::validator_for(&schema).unwrap_or_else(|error| panic!("schema {name} is invalid: {error}"));
    if !validator.is_valid(value) {
        let errors = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        panic!("{name} document does not validate: {errors:?}\n{value}");
    }
}

/// Asserts that a streamed board is the served document apart from its `at` time.
fn assert_board_matches(mut streamed: Board, served: &Board) {
    streamed.at = served.at;
    assert_eq!(
        &streamed, served,
        "the streamed board must match GET /api/v1/board apart from at"
    );
}

#[test]
fn board_stream_sends_initial_changed_snapshots_and_heartbeats() {
    let fixture = BoardStreamFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();

    let response = get(server.address, "/api/v1/board");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &home);
    let served: Board = serde_json::from_str(&response.body).expect("Board");

    let mut client = SseClient::open(server.address, "/api/v1/board/stream");
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");

    let first = client.next_frame(Duration::from_secs(5)).expect("initial board frame");
    assert_eq!(first.event.as_deref(), Some("board"));
    assert_no_home_or_token(&first.data, &home);
    let first_value = first.json();
    assert_schema("Board", &first_value);
    let first_board: Board = serde_json::from_value(first_value).expect("Board");
    assert_eq!(first_board.project, fixture.project);
    assert_board_matches(first_board, &served);

    // With no row change the moving wall-clock `at` alone must not produce another board
    // frame; the short heartbeat interval is the next frame.
    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(
        heartbeat.event.as_deref(),
        Some("heartbeat"),
        "a moving `at` alone must not send a board frame: {}",
        heartbeat.data
    );
    assert_no_home_or_token(&heartbeat.data, &home);
    assert!(heartbeat_time(&heartbeat) <= Timestamp::now());

    fixture.complete_pending_job();
    // A heartbeat may still be due while the writable store commits; ignore those and
    // assert the first board frame after the change carries the new snapshot.
    let changed = loop {
        let frame = client
            .next_frame(Duration::from_secs(5))
            .expect("board or heartbeat frame");
        match frame.event.as_deref() {
            Some("heartbeat") => continue,
            Some("board") => break frame,
            other => panic!("unexpected stream frame {other:?}: {}", frame.data),
        }
    };
    assert_eq!(changed.event.as_deref(), Some("board"));
    assert_no_home_or_token(&changed.data, &home);
    let changed_value = changed.json();
    assert_schema("Board", &changed_value);
    let changed_board: Board = serde_json::from_value(changed_value).expect("Board");
    assert_ne!(changed_board.jobs, served.jobs, "the row change must move the board");
    assert_eq!(changed_board.jobs.get("done"), Some(&2));
    assert_eq!(changed_board.jobs.get("pending").copied().unwrap_or(0), 0);

    let response = get(server.address, "/api/v1/board");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let current: Board = serde_json::from_str(&response.body).expect("Board");
    assert_board_matches(changed_board, &current);

    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("second heartbeat");
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));
    assert_no_home_or_token(&heartbeat.data, &home);

    drop(client);
}

#[test]
fn board_stream_closes_when_a_snapshot_cannot_be_collected() {
    let fixture = BoardStreamFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_secs(60));
    let mut client = SseClient::open(server.address, "/api/v1/board/stream");
    assert_eq!(client.status, 200);
    let first = client.next_frame(Duration::from_secs(5)).expect("initial board frame");
    assert_eq!(first.event.as_deref(), Some("board"));

    let store = Store::open(&fixture.paths).expect("open writable store");
    let mut project = store.require_project(&fixture.project).expect("project row");
    project.config = serde_json::json!({ "project": { "name": "board-proj" } });
    store.upsert_project(&project).expect("invalid project row");
    drop(store);

    assert!(
        client.next_frame(Duration::from_secs(5)).is_none(),
        "the board stream must close cleanly after a collect error"
    );
}

#[test]
fn stream_sends_exact_frames_after_seq_and_live_events() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture, Duration::from_millis(10), Duration::from_secs(60));
    let home = fixture.paths.root().display().to_string();
    let mut client = SseClient::open(server.address, &stream_path(&fixture.session_id, "?after_seq=1"));

    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");

    for expected in &fixture.envelopes[1..] {
        let frame = client.next_frame(Duration::from_secs(5)).expect("session frame");
        assert_eq!(frame.event.as_deref(), Some("session"));
        assert_no_home_or_token(&frame.data, &home);
        assert_eq!(
            frame.data,
            serde_json::to_string(expected).expect("expected envelope JSON"),
            "the data line must be the exact stored envelope"
        );
        let parsed: EventEnvelope = serde_json::from_str(&frame.data).expect("envelope");
        assert_eq!(&parsed, expected);
    }

    let live = fixture.append_live("live");
    assert_eq!(live.seq, 4);
    let frame = client.next_frame(Duration::from_secs(5)).expect("live session frame");
    assert_eq!(frame.event.as_deref(), Some("session"));
    assert_eq!(frame.data, serde_json::to_string(&live).expect("live envelope JSON"));
    let parsed: EventEnvelope = serde_json::from_str(&frame.data).expect("live envelope");
    assert_eq!(parsed.seq, 4);

    drop(client);
}

#[test]
fn stream_sends_heartbeats_while_idle() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();
    let mut client = SseClient::open(server.address, &stream_path(&fixture.session_id, "?after_seq=3"));

    let first = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(first.event.as_deref(), Some("heartbeat"));
    assert_no_home_or_token(&first.data, &home);
    let first_at = heartbeat_time(&first);
    assert!(first_at <= Timestamp::now());

    let second = client.next_frame(Duration::from_secs(2)).expect("second heartbeat");
    assert_eq!(second.event.as_deref(), Some("heartbeat"));
    let second_at = heartbeat_time(&second);
    assert!(second_at >= first_at, "{second_at} < {first_at}");

    drop(client);
}

fn heartbeat_time(frame: &SseFrame) -> Timestamp {
    let value = frame.json();
    let at = value["at"]
        .as_str()
        .unwrap_or_else(|| panic!("heartbeat has no at: {}", frame.data));
    Timestamp::parse(at).unwrap_or_else(|error| panic!("heartbeat at is not RFC 3339: {error}\n{}", frame.data))
}

#[test]
fn stream_rejects_unknown_sessions_and_bad_cursors_before_streaming() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture, Duration::from_millis(10), Duration::from_secs(60));
    let home = fixture.paths.root().display().to_string();

    let missing = SessionId::new();
    let response = get(server.address, &stream_path(&missing, ""));
    assert_eq!(response.status, 404, "body: {}", response.body);
    assert!(
        !response
            .content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "an unknown session must not start a stream"
    );
    assert_no_home_or_token(&response.body, &home);
    let error: Value = serde_json::from_str(&response.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "not_found", "{}", response.body);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(missing.as_str()),
        "{}",
        response.body
    );

    let response = get(server.address, &stream_path(&fixture.session_id, "?after_seq=nope"));
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &home);
    let error: Value = serde_json::from_str(&response.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "invalid_input", "{}", response.body);

    let response = get(
        server.address,
        &format!("/api/v1/sessions/{}/events/stream?after_seq=-1", fixture.session_id),
    );
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &home);

    let response = get(server.address, "/api/v1/sessions/not-a-ulid/events/stream");
    assert_eq!(response.status, 400, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &home);
}

#[test]
fn stream_resyncs_and_closes_when_the_log_is_truncated_behind_the_cursor() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture, Duration::from_millis(10), Duration::from_secs(60));
    let home = fixture.paths.root().display().to_string();
    let mut client = SseClient::open(server.address, &stream_path(&fixture.session_id, "?after_seq=1"));

    for expected in &fixture.envelopes[1..] {
        let frame = client.next_frame(Duration::from_secs(5)).expect("session frame");
        assert_eq!(frame.event.as_deref(), Some("session"));
        assert_eq!(frame.data, serde_json::to_string(expected).expect("envelope JSON"));
    }

    fixture.truncate_to_first_event();
    let frame = client.next_frame(Duration::from_secs(5)).expect("resync frame");
    assert_eq!(frame.event.as_deref(), Some("resync"));
    assert_no_home_or_token(&frame.data, &home);
    let value = frame.json();
    assert_eq!(value["after_seq"], 1, "{}", frame.data);

    assert!(
        client.next_frame(Duration::from_secs(5)).is_none(),
        "the stream must close after one resync frame"
    );

    drop(client);
}

#[test]
fn stream_resyncs_when_a_compacted_session_has_no_log() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture, Duration::from_millis(10), Duration::from_secs(60));
    let home = fixture.paths.root().display().to_string();
    fixture.remove_log();

    let mut client = SseClient::open(server.address, &stream_path(&fixture.session_id, "?after_seq=3"));
    assert_eq!(client.status, 200);
    assert!(
        client
            .content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream")
    );

    let frame = client.next_frame(Duration::from_secs(5)).expect("resync frame");
    assert_eq!(frame.event.as_deref(), Some("resync"));
    assert_no_home_or_token(&frame.data, &home);
    assert_eq!(frame.json()["after_seq"], 3, "{}", frame.data);
    assert!(
        client.next_frame(Duration::from_secs(5)).is_none(),
        "the stream must close after one resync frame"
    );

    drop(client);
}
