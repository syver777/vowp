//! Shared hermetic board fixture and raw HTTP helpers for daemon integration tests.
//!
//! The fixture is a temporary [`NodePaths`] home with one project, three jobs (one running
//! with a live run and session), worker slots, a limit, an open inbox item and a checkpoint.
//! The server listens on `127.0.0.1:0` inside the test process. Nothing here touches the
//! real home, the network beyond loopback, a tmux server or a harness.

// Each integration test compiles this module separately and uses only the fixture and
// helpers it needs, so the unused subset is expected rather than dead production code.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, ApiToken, ProjectSelection, select_project, serve_with_shutdown};
use vowp_protocol::{
    Cost, Executor, FakeClock, HarnessKind, InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, Limit,
    LimitRowKind, NewJob, NodeName, Project, ProjectName, Role, Run, RunId, RunPurpose, RunRole, Session, SessionId,
    SessionRole, SessionState, Timestamp, VowpConfig, Worker, WorkerKind, WorkerState,
};
use vowp_store::{NodePaths, StateChange, Store};

/// A temporary home whose board document has known values.
pub struct BoardFixture {
    _tmp: TempDir,
    /// The temporary VOWP home.
    pub paths: NodePaths,
    /// The one registered project.
    pub project: ProjectName,
    /// Node named by the project's registered config.
    pub node: NodeName,
    /// The job that is running.
    pub running_job: JobId,
    /// Executor recorded for the running job.
    pub run_executor: String,
    /// When the fixture's provider limit lifts.
    pub limit_resets_at: Timestamp,
    /// The newest checkpoint time.
    pub checkpoint: Timestamp,
}

impl BoardFixture {
    /// Builds the temporary home and its rows.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");

        let now = Timestamp::now();
        let base = now.minus(Duration::from_secs(600));
        let clock = FakeClock::at(base);
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let run_executor = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("run executor");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.node.max_workers = 3;
        config.node.max_verifiers = 1;
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.profiles.get_mut("planner").expect("planner profile").ladder =
            vec![Executor::parse("claude-code:planner-model@medium").expect("planner executor")];
        config.profiles.get_mut("keeper").expect("keeper profile").ladder =
            vec![Executor::parse("opencode:provider/keeper-model#max").expect("keeper executor")];
        config.validate().expect("valid fixture config");
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: base,
                updated_at: base,
            })
            .expect("project row");

        store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("job a");
        let running = store
            .insert_job(&project, &new_job("job-b", JobKind::Test, Role::Verifier), "test")
            .expect("job b");
        let done = store
            .insert_job(&project, &new_job("job-c", JobKind::Build, Role::Worker), "test")
            .expect("job c");
        for (job, state) in [
            (&running, vowp_protocol::JobState::Running),
            (&done, vowp_protocol::JobState::Done),
        ] {
            store
                .set_job_state(
                    &job.id,
                    state,
                    &StateChange {
                        note: None,
                        parked_reason: None,
                    },
                )
                .expect("job state");
        }

        // The session was updated 10s ago, while the job state change is 600s old, so a
        // wrong age source is visible in the board document.
        let session_id = SessionId::new();
        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project.clone()),
                job_id: Some(running.id.clone()),
                role: Some(SessionRole::Worker),
                title: "running session".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Running,
                executor: Some(run_executor.clone()),
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: base,
                updated_at: now.minus(Duration::from_secs(10)),
            })
            .expect("session row");

        let run_id = RunId::new();
        store
            .insert_run(&Run {
                id: run_id.clone(),
                job_id: Some(running.id.clone()),
                session_id: Some(session_id),
                role: RunRole::Worker,
                purpose: RunPurpose::Start,
                profile: Some("worker".to_owned()),
                rung: 0,
                executor: Some(run_executor.clone()),
                provider: Some("deepseek".to_owned()),
                node: node.clone(),
                native_session_id: None,
                pid: None,
                window: None,
                cwd: Some("/tmp/proj".to_owned()),
                started_at: now.minus(Duration::from_secs(120)),
                ended_at: None,
                exit_code: None,
                outcome: None,
                limit_kind: None,
                cost: Cost::unknown(),
                detail: String::new(),
            })
            .expect("run row");

        for (kind, slot, state) in [
            (WorkerKind::Worker, 1, WorkerState::Busy),
            (WorkerKind::Worker, 2, WorkerState::Idle),
            (WorkerKind::Verifier, 1, WorkerState::Busy),
        ] {
            let busy = state == WorkerState::Busy;
            store
                .upsert_worker(&Worker {
                    node: node.clone(),
                    slot,
                    kind,
                    state,
                    run_id: busy.then(|| run_id.clone()),
                    job_id: busy.then(|| running.id.clone()),
                    window: None,
                    updated_at: base,
                })
                .expect("worker row");
        }

        let limit_resets_at = now.plus(Duration::from_secs(3600));
        store
            .upsert_limit(&Limit {
                provider: "claude".to_owned(),
                harness: Some(HarnessKind::ClaudeCode),
                kind: LimitRowKind::Quota,
                resets_at: Some(limit_resets_at),
                detail: "weekly quota".to_owned(),
                since: base,
                last_probe_at: None,
                probe_fail_streak: 0,
                concurrency_cap: None,
                cap_until: None,
            })
            .expect("limit row");

        store
            .insert_inbox(&InboxItem {
                id: InboxId::new(),
                project: Some(project.clone()),
                job_id: None,
                kind: InboxKind::Question,
                title: "Need a decision".to_owned(),
                body: "Which way?".to_owned(),
                options: vec!["yes".to_owned(), "no".to_owned()],
                state: InboxState::Open,
                answer: None,
                answered_by: None,
                created_at: base,
                answered_at: None,
            })
            .expect("inbox row");

        let checkpoint = Timestamp::parse("2026-01-02T03:04:05.000Z").expect("checkpoint time");
        let checkpoint_dir = project_dir.join(".vowp").join("checkpoints");
        std::fs::create_dir_all(&checkpoint_dir).expect("checkpoint dir");
        std::fs::write(
            checkpoint_dir.join(format!("{}.md", checkpoint.as_rfc3339())),
            "# checkpoint\n",
        )
        .expect("checkpoint file");

        drop(store);
        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            running_job: running.id,
            run_executor: run_executor.to_string(),
            limit_resets_at,
            checkpoint,
        }
    }
}

/// Builds a minimal job input.
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

/// A running test server, shut down and joined on drop.
pub struct Server {
    /// The loopback address the server is listening on.
    pub address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Starts the daemon for `fixture` on an ephemeral loopback port.
    pub fn start(fixture: &BoardFixture) -> Self {
        let selection = select_project(&fixture.paths, None).expect("project selection");
        assert_eq!(selection.project, fixture.project);
        assert_eq!(selection.node, fixture.node);
        Self::start_with(&fixture.paths, selection, None)
    }

    /// Starts the daemon for any temporary home with exactly one registered project.
    pub fn start_for(paths: &NodePaths) -> Self {
        Self::start_for_with_token(paths, None)
    }

    /// Starts the daemon with a fixed write token, or no token at all.
    pub fn start_for_with_token(paths: &NodePaths, token: Option<&str>) -> Self {
        let selection = select_project(paths, None).expect("project selection");
        Self::start_with(paths, selection, token)
    }

    fn start_with(paths: &NodePaths, selection: ProjectSelection, token: Option<&str>) -> Self {
        let state = ApiState::new(paths, &selection).with_api_token(token.map(ApiToken::new));
        Self::start_with_state(state)
    }

    /// Starts the daemon with an already-built state, which tests use to inject fakes.
    pub fn start_with_state(state: ApiState) -> Self {
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

/// One parsed HTTP response.
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// `Content-Type` header value, when the server sent one.
    pub content_type: Option<String>,
    /// Response headers, lower-cased names and trimmed values.
    pub headers: Vec<(String, String)>,
    /// Response body decoded from chunked transfer encoding.
    pub body: String,
}

impl HttpResponse {
    /// Returns one response header value, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Issues one real `GET` over loopback.
pub fn get(address: SocketAddr, path: &str) -> HttpResponse {
    request(address, "GET", path, None, None)
}

/// Issues one real JSON `POST` over loopback, optionally with a bearer token.
pub fn post(address: SocketAddr, path: &str, body: &str, authorization: Option<&str>) -> HttpResponse {
    request(address, "POST", path, Some(body), authorization)
}

/// Issues one raw HTTP request over loopback and parses the response.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
    authorization: Option<&str>,
) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
    )
    .expect("write request line");
    if let Some(body) = body {
        write!(
            stream,
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
        .expect("write content headers");
    }
    if let Some(value) = authorization {
        write!(stream, "Authorization: {value}\r\n").expect("write authorization");
    }
    write!(stream, "\r\n").expect("write header terminator");
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).expect("write body");
    }
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers");
    let head = String::from_utf8_lossy(&response[..split]);
    let body = decode_body(&head, &response[split + 4..]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("status line");
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    let content_type = headers
        .iter()
        .find(|(name, _)| name == "content-type")
        .map(|(_, value)| value.clone());
    HttpResponse {
        status,
        content_type,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// Decodes a response body, including the chunked transfer encoding.
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
    while let Some(end) = rest.windows(2).position(|window| window == b"\r\n") {
        let size_text = String::from_utf8_lossy(&rest[..end]);
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
