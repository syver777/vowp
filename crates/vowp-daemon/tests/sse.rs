//! Hermetic per-session SSE tests: a temporary home, a real loopback listener and real
//! streaming GETs.
//!
//! No test here touches the real home, the network beyond `127.0.0.1`, a tmux server or a
//! harness. Frames are decoded from the real HTTP chunked body, never from a mock server.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::{Board, CostReport, InboxPage, JobPage, NodePage};
use vowp_protocol::{
    Actor, Clock, Cost, EventBody, EventEnvelope, EventSource, Evidence, FakeClock, Gate, GateResult, HarnessKind,
    InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, LedgerEntry, LedgerKind, NewJob, Node, NodeName,
    Project, ProjectName, Role, Run, RunId, RunPurpose, RunRole, Session, SessionId, SessionRole, SessionState,
    Timestamp, Tokens, UserMessage, VowpConfig,
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
///
/// The same rows back the jobs-list and job-detail stream tests: the pending job changes
/// state, gains runs and gains gate results through real writable store calls.
struct BoardStreamFixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
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
        config.node.name = node.clone();
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
            node,
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

    /// Adds one unfinished worker run to the pending job.
    fn add_run(&self) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .insert_run(&Run {
                id: RunId::new(),
                job_id: Some(self.pending_job.clone()),
                session_id: None,
                role: RunRole::Worker,
                purpose: RunPurpose::Start,
                profile: None,
                rung: 0,
                executor: None,
                provider: None,
                node: self.node.clone(),
                native_session_id: None,
                pid: None,
                window: None,
                cwd: None,
                started_at: epoch().plus(Duration::from_secs(1)),
                ended_at: None,
                exit_code: None,
                outcome: None,
                limit_kind: None,
                cost: Cost::unknown(),
                detail: String::new(),
            })
            .expect("run row");
        drop(store);
    }

    /// Adds one passing gate result to the pending job's first attempt.
    fn add_gate_result(&self) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .insert_gate_result(&GateResult {
                id: None,
                job_id: self.pending_job.clone(),
                run_id: None,
                attempt: 0,
                gate_index: 0,
                gate: Gate::OwnsRespected(true),
                passed: true,
                evidence: Evidence::OwnsRespected {
                    changed: Vec::new(),
                    violations: Vec::new(),
                },
                output_blob: None,
                duration_ms: 1,
                ts: epoch(),
            })
            .expect("gate result row");
        drop(store);
    }
}

/// A temporary home with one cost row, one open inbox item and one local node.
///
/// The writable helpers add a second cost row, another open item and another node, answer
/// the open item, move a node's last-seen time and remove the project database. The same rows
/// back the cost, inbox and nodes stream tests.
struct LivePagesFixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    inbox_id: InboxId,
    node_name: NodeName,
}

impl LivePagesFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(epoch());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("live-proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
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

        store
            .upsert_node(&Node {
                name: node.clone(),
                ssh_target: None,
                is_local: true,
                capabilities: json!({ "vowp": "0.5.0", "probe_ok": true }),
                last_seen_at: Some(epoch()),
                created_at: epoch(),
            })
            .expect("node row");

        let inbox_id = InboxId::new();
        store
            .insert_inbox(&open_item(&project, inbox_id.clone(), "open question"))
            .expect("inbox row");
        append_cost_row(&store, &project, "job-a", 0.5, epoch());
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            inbox_id,
            node_name: node,
        }
    }

    /// Adds one receipt row for `job`.
    fn add_cost_row(&self, job: &str, usd: f64) {
        let store = Store::open(&self.paths).expect("open writable store");
        append_cost_row(&store, &self.project, job, usd, epoch().plus(Duration::from_secs(60)));
        drop(store);
    }

    /// Adds one open inbox item.
    fn add_inbox_item(&self, title: &str) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .insert_inbox(&open_item(&self.project, InboxId::new(), title))
            .expect("inbox row");
        drop(store);
    }

    /// Adds one remote node without an SSH target.
    fn add_remote_node(&self, name: &str) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .upsert_node(&Node {
                name: NodeName::parse(name).expect("node name"),
                ssh_target: None,
                is_local: false,
                capabilities: json!({ "vowp": "0.5.0", "probe_ok": true }),
                last_seen_at: Some(epoch()),
                created_at: epoch(),
            })
            .expect("node row");
        drop(store);
    }

    /// Answers the fixture's open item through the same store call the web write uses.
    fn answer_open_item(&self) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .answer_inbox(&self.inbox_id, "yes", "web")
            .expect("answer inbox item");
        drop(store);
    }

    /// Moves the fixture node's last-seen time forward.
    fn touch_node(&self) {
        let store = Store::open(&self.paths).expect("open writable store");
        store
            .touch_node_seen(&self.node_name, epoch().plus(Duration::from_secs(120)))
            .expect("touch node");
        drop(store);
    }

    /// Removes the attached project database, making the next snapshot read fail.
    fn remove_project_database(&self) {
        std::fs::remove_file(self.paths.project_db_path(&self.project)).expect("remove project database");
    }
}

/// Builds one open inbox item for `project`.
fn open_item(project: &ProjectName, id: InboxId, title: &str) -> InboxItem {
    InboxItem {
        id,
        project: Some(project.clone()),
        job_id: None,
        kind: InboxKind::Question,
        title: title.to_owned(),
        body: format!("{title} body"),
        options: vec!["yes".to_owned(), "no".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: epoch(),
        answered_at: None,
    }
}

/// Appends one known-USD cost ledger row for `job`.
fn append_cost_row(store: &Store, project: &ProjectName, job: &str, usd: f64, at: Timestamp) {
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: at,
            kind: LedgerKind::Cost,
            project: Some(project.clone()),
            job_id: Some(JobId::parse(job).expect("job id")),
            run_id: None,
            session_id: None,
            role: Some(RunRole::Worker),
            provider: Some("test-provider".to_owned()),
            harness: None,
            model: Some("test-model".to_owned()),
            cost: Some(Cost::receipt(
                usd,
                Tokens {
                    input: 100,
                    output: 10,
                    ..Tokens::default()
                },
            )),
            summary: "run cost".to_owned(),
            detail: json!({}),
        })
        .expect("cost ledger row");
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
    headers: Vec<(String, String)>,
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
        let headers: Vec<(String, String)> = head
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();
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
            headers,
        }
    }

    /// Returns one response header value, matched case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
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

/// Asserts that `text` contains no home path; a job snapshot legitimately carries token
/// usage counters, so the word "token" is not a secret marker in this slice.
fn assert_no_home(text: &str, home: &str) {
    assert!(!text.contains(home), "response leaked the home path: {text}");
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
fn jobs_stream_sends_initial_changed_snapshots_and_no_store() {
    let fixture = BoardStreamFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();
    let path = "/api/v1/jobs/stream?state=pending";

    let mut client = SseClient::open(server.address, path);
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        client.header("cache-control"),
        Some("no-store"),
        "the jobs stream must not be cached"
    );

    let first = client.next_frame(Duration::from_secs(5)).expect("initial jobs frame");
    assert_eq!(first.event.as_deref(), Some("jobs"));
    assert_no_home_or_token(&first.data, &home);
    let first_value = first.json();
    assert_schema("JobPage", &first_value);
    let first_page: JobPage = serde_json::from_value(first_value).expect("JobPage");
    assert_eq!(first_page.total, 1);
    assert_eq!(first_page.items[0].id, fixture.pending_job);
    let response = get(server.address, "/api/v1/jobs?state=pending");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let served: JobPage = serde_json::from_str(&response.body).expect("served JobPage");
    assert_eq!(
        first_page, served,
        "the first frame must be the exact filtered API page"
    );

    // A moving clock alone never sends a frame; the short heartbeat interval is next.
    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));
    assert_no_home_or_token(&heartbeat.data, &home);

    fixture.complete_pending_job();
    let changed = next_snapshot_frame(&mut client, "jobs");
    assert_no_home_or_token(&changed.data, &home);
    let changed_value = changed.json();
    assert_schema("JobPage", &changed_value);
    let changed_page: JobPage = serde_json::from_value(changed_value).expect("JobPage");
    assert_eq!(changed_page.total, 0, "the pending job moved out of the filtered list");
    assert!(changed_page.items.is_empty());

    let response = get(server.address, "/api/v1/jobs?state=pending");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let current: JobPage = serde_json::from_str(&response.body).expect("current JobPage");
    assert_eq!(changed_page, current);

    let bad = get(server.address, "/api/v1/jobs/stream?state=nope");
    assert_eq!(bad.status, 400, "body: {}", bad.body);
    assert_no_home_or_token(&bad.body, &home);
    let error: Value = serde_json::from_str(&bad.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "invalid_input", "{}", bad.body);

    drop(client);
}

#[test]
fn job_stream_sends_snapshots_for_state_run_and_gate_changes() {
    let fixture = BoardStreamFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();
    let path = format!("/api/v1/jobs/{}/stream", fixture.pending_job);

    let mut client = SseClient::open(server.address, &path);
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        client.header("cache-control"),
        Some("no-store"),
        "the job stream must not be cached"
    );

    let first = client.next_frame(Duration::from_secs(5)).expect("initial job frame");
    assert_eq!(first.event.as_deref(), Some("job"));
    assert_no_home(&first.data, &home);
    let first_value = first.json();
    assert_schema("Job", &first_value["job"]);
    assert_schema("RunPage", &first_value["runs"]);
    assert_schema("GateResultPage", &first_value["gates"]);
    assert_eq!(first_value["job"]["id"], fixture.pending_job.as_str());
    assert_eq!(first_value["job"]["state"], "pending");
    assert_eq!(first_value["runs"]["total"], 0);
    assert_eq!(first_value["gates"]["total"], 0);

    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));
    assert_no_home(&heartbeat.data, &home);

    fixture.add_gate_result();
    let gate_frame = next_snapshot_frame(&mut client, "job");
    assert_no_home(&gate_frame.data, &home);
    let gate_value = gate_frame.json();
    assert_schema("GateResultPage", &gate_value["gates"]);
    assert_eq!(gate_value["gates"]["total"], 1);
    assert_eq!(gate_value["runs"]["total"], 0);

    fixture.add_run();
    let run_frame = next_snapshot_frame(&mut client, "job");
    assert_no_home(&run_frame.data, &home);
    let run_value = run_frame.json();
    assert_schema("RunPage", &run_value["runs"]);
    assert_eq!(run_value["runs"]["total"], 1);
    assert_eq!(run_value["gates"]["total"], 1);

    fixture.complete_pending_job();
    let state_frame = next_snapshot_frame(&mut client, "job");
    assert_no_home(&state_frame.data, &home);
    let state_value = state_frame.json();
    assert_schema("Job", &state_value["job"]);
    assert_eq!(state_value["job"]["state"], "done");
    assert_eq!(state_value["runs"]["total"], 1);
    assert_eq!(state_value["gates"]["total"], 1);

    drop(client);
}

#[test]
fn job_stream_rejects_unknown_jobs_and_bad_ids_before_streaming() {
    let fixture = BoardStreamFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_secs(60));
    let home = fixture.paths.root().display().to_string();

    let missing = get(server.address, "/api/v1/jobs/missing-job/stream");
    assert_eq!(missing.status, 404, "body: {}", missing.body);
    assert!(
        !missing
            .content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "an unknown job must not start a stream"
    );
    assert_no_home_or_token(&missing.body, &home);
    let error: Value = serde_json::from_str(&missing.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "not_found", "{}", missing.body);

    let invalid = get(server.address, "/api/v1/jobs/Not_Valid/stream");
    assert_eq!(invalid.status, 400, "body: {}", invalid.body);
    assert_no_home_or_token(&invalid.body, &home);
    let error: Value = serde_json::from_str(&invalid.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "invalid_input", "{}", invalid.body);
}

#[test]
fn cost_stream_sends_one_initial_snapshot_then_changed_only_frames() {
    let fixture = LivePagesFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();

    let mut client = SseClient::open(server.address, "/api/v1/cost/stream?by=job");
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        client.header("cache-control"),
        Some("no-store"),
        "the cost stream must not be cached"
    );

    let first = next_snapshot_frame(&mut client, "cost");
    assert_no_home(&first.data, &home);
    let first_value = first.json();
    assert_schema("CostReport", &first_value);
    let first_report: CostReport = serde_json::from_value(first_value).expect("CostReport");
    assert_eq!(first_report.total.runs, 1);
    let response = get(server.address, "/api/v1/cost?by=job");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let served: CostReport = serde_json::from_str(&response.body).expect("served CostReport");
    assert_eq!(first_report, served, "the first frame must be the exact cost report");

    // An unchanged report sends no duplicate frame; the next frame is the heartbeat.
    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(
        heartbeat.event.as_deref(),
        Some("heartbeat"),
        "a stable report must not resend a cost frame: {}",
        heartbeat.data
    );
    assert_no_home(&heartbeat.data, &home);

    fixture.add_cost_row("job-b", 0.25);
    let changed = next_snapshot_frame(&mut client, "cost");
    assert_no_home(&changed.data, &home);
    let changed_value = changed.json();
    assert_schema("CostReport", &changed_value);
    let changed_report: CostReport = serde_json::from_value(changed_value).expect("CostReport");
    assert_eq!(changed_report.total.runs, 2);
    assert_eq!(changed_report.total.usd, Some(0.75));
    let response = get(server.address, "/api/v1/cost?by=job");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let current: CostReport = serde_json::from_str(&response.body).expect("current CostReport");
    assert_eq!(changed_report, current);

    drop(client);
}

#[test]
fn inbox_stream_carries_the_state_filter_and_updates() {
    let fixture = LivePagesFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();

    let mut client = SseClient::open(server.address, "/api/v1/inbox/stream?state=open");
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        client.header("cache-control"),
        Some("no-store"),
        "the inbox stream must not be cached"
    );

    let first = next_snapshot_frame(&mut client, "inbox");
    assert_no_home(&first.data, &home);
    let first_value = first.json();
    assert_schema("InboxPage", &first_value);
    let first_page: InboxPage = serde_json::from_value(first_value).expect("InboxPage");
    assert_eq!(first_page.total, 1);
    assert_eq!(first_page.items[0].id, fixture.inbox_id);
    assert_eq!(first_page.items[0].state, InboxState::Open);
    let response = get(server.address, "/api/v1/inbox?state=open");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let served: InboxPage = serde_json::from_str(&response.body).expect("served InboxPage");
    assert_eq!(first_page, served, "the first frame must be the exact open page");

    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));

    fixture.answer_open_item();
    let changed = next_snapshot_frame(&mut client, "inbox");
    assert_no_home(&changed.data, &home);
    let changed_value = changed.json();
    assert_schema("InboxPage", &changed_value);
    let changed_page: InboxPage = serde_json::from_value(changed_value).expect("InboxPage");
    assert_eq!(changed_page.total, 0, "the answered item left the open filter");
    assert!(changed_page.items.is_empty());
    let response = get(server.address, "/api/v1/inbox?state=open");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let current: InboxPage = serde_json::from_str(&response.body).expect("current InboxPage");
    assert_eq!(changed_page, current);

    let bad = get(server.address, "/api/v1/inbox/stream?state=nope");
    assert_eq!(bad.status, 400, "body: {}", bad.body);
    assert!(
        !bad.content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "an invalid state must not start a stream"
    );
    assert_no_home(&bad.body, &home);
    let error: Value = serde_json::from_str(&bad.body).expect("ApiError JSON");
    assert_eq!(error["error"]["kind"], "invalid_input", "{}", bad.body);

    drop(client);
}

#[test]
fn nodes_stream_sends_initial_nodes_and_updates() {
    let fixture = LivePagesFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));
    let home = fixture.paths.root().display().to_string();

    let mut client = SseClient::open(server.address, "/api/v1/nodes/stream");
    assert_eq!(client.status, 200);
    let content_type = client.content_type.as_deref().unwrap_or_default();
    assert!(content_type.starts_with("text/event-stream"), "{content_type}");
    assert_eq!(
        client.header("cache-control"),
        Some("no-store"),
        "the nodes stream must not be cached"
    );

    let first = next_snapshot_frame(&mut client, "nodes");
    assert_no_home(&first.data, &home);
    let first_value = first.json();
    assert_schema("NodePage", &first_value);
    let first_page: NodePage = serde_json::from_value(first_value).expect("NodePage");
    assert_eq!(first_page.total, 1);
    assert_eq!(first_page.items[0].name, fixture.node_name);
    assert_eq!(first_page.items[0].last_seen_at, Some(epoch()));

    let heartbeat = client.next_frame(Duration::from_secs(2)).expect("first heartbeat");
    assert_eq!(
        heartbeat.event.as_deref(),
        Some("heartbeat"),
        "an unchanged node page must not resend a nodes frame: {}",
        heartbeat.data
    );

    fixture.touch_node();
    let changed = next_snapshot_frame(&mut client, "nodes");
    assert_no_home(&changed.data, &home);
    let changed_value = changed.json();
    assert_schema("NodePage", &changed_value);
    let changed_page: NodePage = serde_json::from_value(changed_value).expect("NodePage");
    assert_eq!(changed_page.total, 1);
    assert_eq!(
        changed_page.items[0].last_seen_at,
        Some(epoch().plus(Duration::from_secs(120)))
    );
    let response = get(server.address, "/api/v1/nodes");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let current: NodePage = serde_json::from_str(&response.body).expect("current NodePage");
    assert_eq!(changed_page, current);

    drop(client);
}

#[test]
fn live_streams_never_emit_a_per_row_frame() {
    let fixture = LivePagesFixture::new();
    fixture.add_cost_row("job-b", 0.25);
    fixture.add_inbox_item("second question");
    fixture.add_remote_node("second-node");
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_millis(50));

    let mut cost = SseClient::open(server.address, "/api/v1/cost/stream?by=job");
    let cost_frame = next_snapshot_frame(&mut cost, "cost");
    let cost_value = cost_frame.json();
    let rows = cost_value["rows"]
        .as_array()
        .expect("a cost frame is one CostReport with a rows array");
    assert_eq!(rows.len(), 2, "one frame must carry every grouped row");
    assert_eq!(cost_value["total"]["runs"].as_u64(), Some(2));
    assert_schema("CostReport", &cost_value);

    let mut inbox = SseClient::open(server.address, "/api/v1/inbox/stream?state=open");
    let inbox_frame = next_snapshot_frame(&mut inbox, "inbox");
    let inbox_value = inbox_frame.json();
    let items = inbox_value["items"]
        .as_array()
        .expect("an inbox frame is one InboxPage with an items array");
    assert_eq!(items.len(), 2, "one frame must carry every open item");
    assert_eq!(inbox_value["total"].as_u64(), Some(2));
    assert_schema("InboxPage", &inbox_value);

    let mut nodes = SseClient::open(server.address, "/api/v1/nodes/stream");
    let nodes_frame = next_snapshot_frame(&mut nodes, "nodes");
    let nodes_value = nodes_frame.json();
    let items = nodes_value["items"]
        .as_array()
        .expect("a nodes frame is one NodePage with an items array");
    assert_eq!(items.len(), 2, "one frame must carry every node row");
    assert_eq!(nodes_value["total"].as_u64(), Some(2));
    assert_schema("NodePage", &nodes_value);

    drop(cost);
    drop(inbox);
    drop(nodes);
}

#[test]
fn streams_end_cleanly_when_the_store_is_unavailable() {
    let fixture = LivePagesFixture::new();
    let server = Server::start_for(&fixture.paths, Duration::from_millis(10), Duration::from_secs(60));
    let mut client = SseClient::open(server.address, "/api/v1/nodes/stream");
    assert_eq!(client.status, 200);
    let first = next_snapshot_frame(&mut client, "nodes");
    assert_eq!(first.event.as_deref(), Some("nodes"));

    fixture.remove_project_database();
    assert!(
        client.next_frame(Duration::from_secs(5)).is_none(),
        "the nodes stream must close cleanly after a store read error"
    );

    drop(client);
}

/// Returns the next snapshot frame of `event`, ignoring any interleaved heartbeats.
fn next_snapshot_frame(client: &mut SseClient, event: &str) -> SseFrame {
    loop {
        let frame = client.next_frame(Duration::from_secs(5)).expect("snapshot frame");
        match frame.event.as_deref() {
            Some("heartbeat") => continue,
            Some(kind) if kind == event => return frame,
            other => panic!("unexpected stream frame {other:?}: {}", frame.data),
        }
    }
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
