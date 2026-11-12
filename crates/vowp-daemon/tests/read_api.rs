//! Hermetic read-API tests: a temporary home, a real loopback listener and real GETs.
//!
//! No test here touches the real home, the network beyond `127.0.0.1`, a tmux server or
//! a harness. Every returned document is validated against the schema exported by
//! `vowp_protocol::schema`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::{ApiError, EventPage, Health, JobPage, SessionPage};
use vowp_protocol::{
    Actor, Clock, EventBody, EventEnvelope, EventSource, FakeClock, HarnessKind, JobId, JobKind, NewJob, NodeName,
    Project, ProjectName, Role, Session, SessionId, SessionRole, SessionState, Timestamp, UserMessage, VowpConfig,
};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, StateChange, Store};

/// A temporary home with one project, two jobs, one session and three events.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    session_id: SessionId,
    envelopes: Vec<EventEnvelope>,
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

        let first = store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("job a");
        let second = store
            .insert_job(&project, &new_job("job-b", JobKind::Test, Role::Verifier), "test")
            .expect("job b");
        store
            .set_job_state(
                &second.id,
                vowp_protocol::JobState::Running,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("job b running");

        let session_id = SessionId::new();
        let log = SessionLog::create(&paths, &session_id, Arc::new(clock.clone())).expect("session log");
        let mut envelopes = Vec::new();
        for text in ["first", "second", "third"] {
            let appended = log
                .append(NewEvent {
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
                .expect("append event");
            envelopes.push(appended.envelope);
        }
        let log_bytes = log.byte_len();
        drop(log);
        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project.clone()),
                job_id: Some(first.id.clone()),
                role: Some(SessionRole::Worker),
                title: "test session".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 3,
                last_capsule_id: None,
                log_bytes,
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("session row");
        std::fs::write(paths.root().join("heartbeat"), format!("{}\n", clock.now().unix_secs())).expect("heartbeat");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            session_id,
            envelopes,
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

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
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
        assert_eq!(selection.node, fixture.node);
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
fn read_api_serves_schema_valid_documents() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);
    let home = fixture.paths.root().display().to_string();

    let response = get(server.address, "/api/v1/health");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "health leaked the home path");
    assert!(
        !response.body.to_ascii_lowercase().contains("token"),
        "health leaked a token"
    );
    let value = response.json();
    assert_schema("Health", &value);
    let health: Health = serde_json::from_value(value).expect("Health");
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(health.node, fixture.node);
    assert_eq!(health.project, fixture.project);
    assert_eq!(health.schema_version, vowp_store::PROJECT_SCHEMA_VERSION);
    assert_eq!(health.heartbeat, Some(epoch()));

    let response = get(server.address, "/api/v1/jobs");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "jobs leaked the home path");
    let value = response.json();
    assert_schema("JobPage", &value);
    let page: JobPage = serde_json::from_value(value).expect("JobPage");
    assert_eq!(page.total, 2);
    assert_eq!(page.next, None);
    let ids = page.items.iter().map(|job| job.id.as_str()).collect::<Vec<_>>();
    assert!(ids.contains(&"job-a") && ids.contains(&"job-b"), "{ids:?}");
    for item in &page.items {
        assert_schema("Job", &serde_json::to_value(item).expect("job JSON"));
    }

    let response = get(server.address, "/api/v1/jobs?state=pending");
    assert_eq!(response.status, 200);
    let page: JobPage = serde_json::from_value(response.json()).expect("JobPage");
    assert_eq!(
        page.items.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
        vec!["job-a"]
    );

    let response = get(server.address, "/api/v1/jobs?state=running");
    let page: JobPage = serde_json::from_value(response.json()).expect("JobPage");
    assert_eq!(
        page.items.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
        vec!["job-b"]
    );

    let response = get(server.address, "/api/v1/jobs?state=ready,done&state=running");
    let page: JobPage = serde_json::from_value(response.json()).expect("JobPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id.as_str(), "job-b");

    let response = get(server.address, "/api/v1/jobs?kind=test");
    let page: JobPage = serde_json::from_value(response.json()).expect("JobPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id.as_str(), "job-b");

    let response = get(server.address, "/api/v1/jobs?role=verifier");
    let page: JobPage = serde_json::from_value(response.json()).expect("JobPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id.as_str(), "job-b");

    let response = get(server.address, "/api/v1/sessions");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "sessions leaked the home path");
    let value = response.json();
    assert_schema("SessionPage", &value);
    let page: SessionPage = serde_json::from_value(value).expect("SessionPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.next, None);
    assert_eq!(page.items[0].id, fixture.session_id);
    assert_schema("Session", &serde_json::to_value(&page.items[0]).expect("session JSON"));

    for query in ["?project=proj", "?job=job-a", "?state=idle"] {
        let response = get(server.address, &format!("/api/v1/sessions{query}"));
        assert_eq!(response.status, 200, "{query}");
        let page: SessionPage = serde_json::from_value(response.json()).expect("SessionPage");
        assert_eq!(page.total, 1, "{query}");
    }
    for query in ["?project=other", "?job=job-b", "?state=running"] {
        let response = get(server.address, &format!("/api/v1/sessions{query}"));
        assert_eq!(response.status, 200, "{query}");
        let page: SessionPage = serde_json::from_value(response.json()).expect("SessionPage");
        assert_eq!(page.total, 0, "{query}");
    }

    let events_path = format!("/api/v1/sessions/{}/events", fixture.session_id);
    let response = get(server.address, &events_path);
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "events leaked the home path");
    let value = response.json();
    assert_schema("EventPage", &value);
    let page: EventPage = serde_json::from_value(value).expect("EventPage");
    assert_eq!(page.items, fixture.envelopes);
    assert_eq!(page.total, 3);
    assert_eq!(page.next, None);
    for item in &page.items {
        assert_schema("EventEnvelope", &serde_json::to_value(item).expect("event JSON"));
    }

    let response = get(server.address, &format!("{events_path}?after_seq=1"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert_eq!(page.items.iter().map(|event| event.seq).collect::<Vec<_>>(), vec![2, 3]);

    let response = get(server.address, &format!("{events_path}?after_seq=1&limit=1"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert_eq!(page.items.iter().map(|event| event.seq).collect::<Vec<_>>(), vec![2]);
    assert_eq!(page.next, Some(2));

    let response = get(server.address, &format!("{events_path}?limit=2"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert_eq!(page.items.iter().map(|event| event.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(page.next, Some(2));

    let response = get(server.address, &format!("{events_path}?after_seq=3"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert!(page.items.is_empty());
    assert_eq!(page.next, None);

    let response = get(server.address, &format!("{events_path}?limit=0"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert!(page.items.is_empty());
    assert_eq!(page.next, None);

    let response = get(server.address, &format!("{events_path}?limit=5000"));
    let page: EventPage = serde_json::from_value(response.json()).expect("EventPage");
    assert_eq!(page.total, 3);
}

#[test]
fn read_api_returns_api_errors() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let error = assert_api_error(
        &get(server.address, "/api/v1/jobs?state=not-a-state"),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("not-a-state"), "{}", error.error.message);

    assert_api_error(
        &get(server.address, "/api/v1/jobs?kind=not-a-kind"),
        400,
        "invalid_input",
    );
    assert_api_error(
        &get(server.address, "/api/v1/sessions?project=Not_Valid"),
        400,
        "invalid_input",
    );

    let missing = SessionId::new();
    assert_api_error(
        &get(server.address, &format!("/api/v1/sessions/{missing}/events")),
        404,
        "not_found",
    );
    assert_api_error(
        &get(server.address, "/api/v1/sessions/not-a-ulid/events"),
        400,
        "invalid_input",
    );
    assert_api_error(
        &get(
            server.address,
            &format!("/api/v1/sessions/{}/events?after_seq=nope", fixture.session_id),
        ),
        400,
        "invalid_input",
    );
    assert_api_error(
        &get(
            server.address,
            &format!("/api/v1/sessions/{}/events?limit=-1", fixture.session_id),
        ),
        400,
        "invalid_input",
    );

    assert_api_error(&get(server.address, "/api/v1/missing"), 404, "not_found");
}
