//! Hermetic `/api/v1/board` tests: a temporary home, a real loopback listener and real GETs.
//!
//! No test here touches the real home, the network beyond `127.0.0.1`, a tmux server or a
//! harness. The response is checked against the exact `vowp board --json` shape and against
//! the schema exported by `vowp_protocol::schema`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::{ApiError, Board};
use vowp_protocol::{
    Cost, Executor, FakeClock, HarnessKind, InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, LedgerEntry,
    LedgerKind, Limit, LimitRowKind, NewJob, NodeName, Project, ProjectName, Role, Run, RunId, RunPurpose, RunRole,
    Session, SessionId, SessionRole, SessionState, Timestamp, Tokens, VowpConfig, Worker, WorkerKind, WorkerState,
};
use vowp_store::{NodePaths, StateChange, Store};

/// A temporary home with three jobs, one running run, a limit, an inbox item and a checkpoint.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    running_job: JobId,
    limit_resets_at: Timestamp,
    checkpoint: Timestamp,
}

impl Fixture {
    fn new() -> Self {
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

        // The job state change is 600s old; the live run started 120s ago and the session
        // was updated 10s ago, so a wrong age source fails the assertions below.
        let run_started_at = now.minus(Duration::from_secs(120));
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
                started_at: run_started_at,
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

        for (state, answered_at) in [(InboxState::Open, None), (InboxState::Answered, Some(base))] {
            store
                .insert_inbox(&InboxItem {
                    id: InboxId::new(),
                    project: Some(project.clone()),
                    job_id: None,
                    kind: InboxKind::Question,
                    title: "Need a decision".to_owned(),
                    body: "Which way?".to_owned(),
                    options: vec!["yes".to_owned(), "no".to_owned()],
                    state,
                    answer: answered_at.map(|_| "yes".to_owned()),
                    answered_by: answered_at.map(|_| "owner".to_owned()),
                    created_at: base,
                    answered_at,
                })
                .expect("inbox row");
        }

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
            running_job: running.id,
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
struct Server {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(fixture: &Fixture) -> Self {
        let selection = select_project(&fixture.paths, None).expect("project selection");
        assert_eq!(selection.project, fixture.project);
        let state = ApiState::new(&fixture.paths, &selection);
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
struct HttpResponse {
    status: u16,
    body: String,
}

impl HttpResponse {
    /// Parses the body as JSON.
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| panic!("invalid JSON: {error}\n{}", self.body))
    }
}

/// Issues one real `GET` over loopback.
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
    HttpResponse {
        status,
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

/// Asserts a response is the JSON error document with `status` and `kind`.
fn assert_api_error(response: &HttpResponse, status: u16, kind: &str) -> ApiError {
    assert_eq!(response.status, status, "body: {}", response.body);
    let value = response.json();
    assert_schema("ApiError", &value);
    let error: ApiError = serde_json::from_value(value).expect("ApiError");
    assert_eq!(
        serde_json::to_value(error.error.kind).expect("kind"),
        Value::String(kind.to_owned())
    );
    error
}

#[test]
fn board_api_serves_the_cli_json_document() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);
    let home = fixture.paths.root().display().to_string();

    let response = get(server.address, "/api/v1/board");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "board leaked the home path");
    assert!(
        !response.body.to_ascii_lowercase().contains("token"),
        "board leaked a token"
    );

    let value = response.json();
    let keys = value
        .as_object()
        .expect("board object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            "at",
            "cost_today_usd",
            "inbox_open",
            "jobs",
            "keeper",
            "last_checkpoint",
            "limits",
            "planner",
            "project",
            "running",
            "slots_busy",
            "slots_total",
        ],
        "board must have exactly the `vowp board --json` keys: {value}"
    );
    assert_schema("Board", &value);

    let board: Board = serde_json::from_value(value.clone()).expect("Board");
    assert!(
        Timestamp::now().since(board.at).as_secs() <= 300,
        "board timestamp is not recent: {}",
        board.at
    );
    assert_eq!(board.project, fixture.project);
    assert_eq!(
        board.jobs,
        BTreeMap::from([
            ("done".to_owned(), 1),
            ("pending".to_owned(), 1),
            ("running".to_owned(), 1),
        ])
    );
    assert_eq!(board.running.len(), 1);
    let run = &board.running[0];
    assert_eq!(run.job, fixture.running_job);
    assert_eq!(run.executor.as_deref(), Some("opencode:deepseek/deepseek-flash#max"));
    assert!(
        (110..=300).contains(&run.age_secs),
        "run age must come from the live run, not the 600s-old state change: {}",
        run.age_secs
    );
    let last_event_age = run.last_event_age_secs.expect("session last event age");
    assert!(
        (0..=90).contains(&last_event_age),
        "last event age must come from the session update: {last_event_age}"
    );
    assert_eq!(board.slots_busy, 2);
    assert_eq!(board.slots_total, 4);
    assert_eq!(board.planner.as_deref(), Some("claude-code:planner-model@medium"));
    assert_eq!(board.keeper.as_deref(), Some("opencode:provider/keeper-model#max"));
    assert_eq!(board.limits.len(), 1);
    let limit = &board.limits[0];
    assert_eq!(limit.provider, "claude");
    assert_eq!(limit.kind, LimitRowKind::Quota);
    assert_eq!(limit.resets_at, Some(fixture.limit_resets_at));
    assert_eq!(board.cost_today_usd, None, "no cost row means an unknown total");
    assert_eq!(board.inbox_open, 1, "only the open inbox item counts");
    assert_eq!(board.last_checkpoint, Some(fixture.checkpoint));

    assert_eq!(value["limits"][0]["kind"], json!("quota"));
    assert_eq!(
        value["last_checkpoint"],
        json!(fixture.checkpoint.as_rfc3339()),
        "timestamps serialise as the CLI's RFC 3339 strings"
    );

    let store = Store::open(&fixture.paths).expect("open store");
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: Timestamp::now(),
            kind: LedgerKind::Cost,
            project: Some(fixture.project.clone()),
            job_id: None,
            run_id: None,
            session_id: None,
            role: Some(RunRole::Worker),
            provider: Some("claude".to_owned()),
            harness: Some(HarnessKind::ClaudeCode),
            model: Some("planner-model".to_owned()),
            cost: Some(Cost::receipt(1.25, Tokens::default())),
            summary: "test cost".to_owned(),
            detail: json!({}),
        })
        .expect("cost ledger row");
    drop(store);

    let response = get(server.address, "/api/v1/board");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let board: Board = serde_json::from_value(response.json()).expect("Board");
    assert_eq!(board.cost_today_usd, Some(1.25));
}

#[test]
fn board_api_returns_500_for_an_invalid_registered_config() {
    let fixture = Fixture::new();
    let store = Store::open(&fixture.paths).expect("open store");
    let mut project = store.require_project(&fixture.project).expect("project row");
    project.config = json!({ "project": { "name": "proj" } });
    store.upsert_project(&project).expect("invalid project row");
    drop(store);

    let server = Server::start(&fixture);
    let response = get(server.address, "/api/v1/board");
    let error = assert_api_error(&response, 500, "config");
    assert!(error.error.message.contains("configuration"), "{}", error.error.message);
    assert!(
        !response.body.contains(&fixture.paths.root().display().to_string()),
        "error leaked the home path"
    );
    assert!(
        !response.body.to_ascii_lowercase().contains("token"),
        "error leaked a token"
    );
}
