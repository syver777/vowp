//! Hermetic tests for the session detail and single-capsule read endpoints.
//!
//! A temporary home holds a three-session parent chain, a long parent chain, a cycle and
//! capsules; a real loopback listener serves the daemon routes. Every document is checked
//! against the schema exported by `vowp_protocol::schema`. No test touches the real home,
//! the network beyond `127.0.0.1`, a tmux server or a harness.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::{ApiError, MAX_LINEAGE, SessionDetail};
use vowp_protocol::{
    Capsule, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, Clock, Executor, ExecutorRef, FakeClock,
    NodeName, Project, ProjectName, Session, SessionId, SessionRole, SessionState, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, Store};

/// A temporary home with a parent chain, a long chain, a cycle and two capsules.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    root_id: SessionId,
    parent_id: SessionId,
    child_id: SessionId,
    child_capsule: Capsule,
    parent_capsule: Capsule,
    long_leaf_id: SessionId,
    /// The long chain in creation order: `ancestors[0]` is the oldest and has no parent.
    long_ancestors: Vec<SessionId>,
    orphan_id: SessionId,
    cycle_id: SessionId,
    cycle_parent_id: SessionId,
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

        let root = session(&project, &node, "root session", None, clock.now());
        store.upsert_session(&root).expect("root session row");
        let parent = session(&project, &node, "parent session", Some(root.id.clone()), clock.now());
        store.upsert_session(&parent).expect("parent session row");
        let child = session(&project, &node, "child session", Some(parent.id.clone()), clock.now());
        store.upsert_session(&child).expect("child session row");

        let parent_capsule = capsule(&parent.id, 1, clock.now());
        store
            .insert_capsule(&parent_capsule, "sessions/parent/capsules/1.json")
            .expect("parent capsule row");
        let child_capsule = capsule(&child.id, 2, clock.now());
        store
            .insert_capsule(&child_capsule, "sessions/child/capsules/2.json")
            .expect("child capsule row");

        // A chain longer than the lineage bound. Creation order is oldest to newest.
        let mut long_ancestors = Vec::new();
        let mut next: Option<SessionId> = None;
        for index in 0..MAX_LINEAGE + 1 {
            let row = session(&project, &node, &format!("ancestor {index}"), next.take(), clock.now());
            store.upsert_session(&row).expect("ancestor row");
            next = Some(row.id.clone());
            long_ancestors.push(row.id);
        }
        let long_leaf = session(&project, &node, "long leaf", next, clock.now());
        store.upsert_session(&long_leaf).expect("long leaf row");

        // A dangling parent id stops the walk instead of failing the request.
        let orphan = session(&project, &node, "orphan session", Some(SessionId::new()), clock.now());
        store.upsert_session(&orphan).expect("orphan row");

        // A stored cycle must terminate at the same bound.
        let cycle_a = session(&project, &node, "cycle a", None, clock.now());
        store.upsert_session(&cycle_a).expect("cycle a row");
        let cycle_b = session(&project, &node, "cycle b", Some(cycle_a.id.clone()), clock.now());
        store.upsert_session(&cycle_b).expect("cycle b row");
        let mut cycle_a = cycle_a;
        cycle_a.parent_session_id = Some(cycle_b.id.clone());
        store.upsert_session(&cycle_a).expect("cycle back link");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            root_id: root.id,
            parent_id: parent.id,
            child_id: child.id,
            child_capsule,
            parent_capsule,
            long_leaf_id: long_leaf.id,
            long_ancestors,
            orphan_id: orphan.id,
            cycle_id: cycle_a.id,
            cycle_parent_id: cycle_b.id,
        }
    }
}

/// Builds a session row for the fixture.
fn session(
    project: &ProjectName,
    node: &NodeName,
    title: &str,
    parent_session_id: Option<SessionId>,
    at: Timestamp,
) -> Session {
    Session {
        id: SessionId::new(),
        project: Some(project.clone()),
        job_id: None,
        role: Some(SessionRole::Worker),
        title: title.to_owned(),
        parent_session_id,
        node: node.clone(),
        cwd: "/work/proj".to_owned(),
        state: SessionState::Idle,
        executor: None,
        last_seq: 0,
        last_capsule_id: None,
        log_bytes: 0,
        created_at: at,
        updated_at: at,
    }
}

/// Builds a minimal valid capsule for `session`.
fn capsule(session: &SessionId, at_seq: u64, at: Timestamp) -> Capsule {
    Capsule {
        version: vowp_protocol::capsule::CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: at,
        at_seq,
        trigger: CapsuleTrigger::Manual,
        from: ExecutorRef {
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor"),
            node: None,
        },
        intent: format!("hand session {session} off"),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry::default(),
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
fn session_detail_returns_metadata_latest_capsule_and_lineage() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);
    let home = fixture.paths.root().display().to_string();

    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.child_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "session detail leaked the home path");
    let value = response.json();
    assert_schema("SessionDetail", &value);
    let detail: SessionDetail = serde_json::from_value(value).expect("SessionDetail");
    assert_eq!(detail.session.id, fixture.child_id);
    assert_eq!(detail.session.title, "child session");
    assert_eq!(detail.session.parent_session_id.as_ref(), Some(&fixture.parent_id));
    assert_eq!(detail.latest_capsule.as_ref(), Some(&fixture.child_capsule));
    assert_eq!(detail.lineage, vec![fixture.parent_id.clone(), fixture.root_id.clone()]);

    // A dangling parent id ends the chain instead of failing the request.
    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.orphan_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    let detail: SessionDetail = serde_json::from_value(response.json()).expect("orphan SessionDetail");
    assert!(detail.lineage.is_empty(), "{:?}", detail.lineage);

    // The long chain keeps only the nearest MAX_LINEAGE parents, nearest first.
    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.long_leaf_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_schema("SessionDetail", &response.json());
    let detail: SessionDetail = serde_json::from_value(response.json()).expect("long SessionDetail");
    let expected = fixture
        .long_ancestors
        .iter()
        .rev()
        .take(MAX_LINEAGE)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(detail.lineage.len(), MAX_LINEAGE);
    assert_eq!(detail.lineage, expected);
    assert!(
        !detail.lineage.contains(&fixture.long_ancestors[0]),
        "the oldest ancestor is beyond the bound"
    );

    // A stored cycle stops when a parent repeats instead of looping forever.
    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.cycle_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    let detail: SessionDetail = serde_json::from_value(response.json()).expect("cycle SessionDetail");
    assert_eq!(
        detail.lineage,
        vec![fixture.cycle_parent_id.clone(), fixture.cycle_id.clone()],
        "the walk stops before repeating a session"
    );

    assert_api_error(
        &get(server.address, &format!("/api/v1/sessions/{}", SessionId::new())),
        404,
        "not_found",
    );
    assert_api_error(
        &get(server.address, "/api/v1/sessions/not-a-ulid"),
        400,
        "invalid_input",
    );
}

#[test]
fn capsule_read_returns_the_stored_capsule_and_hides_foreign_ids() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);
    let home = fixture.paths.root().display().to_string();

    let path = format!(
        "/api/v1/sessions/{}/capsules/{}",
        fixture.child_id, fixture.child_capsule.capsule_id
    );
    let response = get(server.address, &path);
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "capsule response leaked the home path");
    let value = response.json();
    assert_schema("Capsule", &value);
    let capsule: Capsule = serde_json::from_value(value).expect("Capsule");
    assert_eq!(capsule, fixture.child_capsule);

    // Unknown session, unknown capsule and a capsule of another session are all 404.
    assert_api_error(
        &get(
            server.address,
            &format!(
                "/api/v1/sessions/{}/capsules/{}",
                SessionId::new(),
                fixture.child_capsule.capsule_id
            ),
        ),
        404,
        "not_found",
    );
    assert_api_error(
        &get(
            server.address,
            &format!("/api/v1/sessions/{}/capsules/{}", fixture.child_id, CapsuleId::new()),
        ),
        404,
        "not_found",
    );
    assert_api_error(
        &get(
            server.address,
            &format!(
                "/api/v1/sessions/{}/capsules/{}",
                fixture.child_id, fixture.parent_capsule.capsule_id
            ),
        ),
        404,
        "not_found",
    );
    assert_api_error(
        &get(
            server.address,
            &format!(
                "/api/v1/sessions/not-a-ulid/capsules/{}",
                fixture.child_capsule.capsule_id
            ),
        ),
        400,
        "invalid_input",
    );
    assert_api_error(
        &get(
            server.address,
            &format!("/api/v1/sessions/{}/capsules/not-a-capsule", fixture.child_id),
        ),
        400,
        "invalid_input",
    );
}
