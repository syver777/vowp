//! Hermetic session-page tests: a temporary home, a real loopback listener and real GETs.
//!
//! The session page is static HTML that fetches `/api/v1/sessions` and, on the detail
//! route, opens the per-session SSE stream after an initial
//! `/api/v1/sessions/{id}/events?after_seq=` fetch. These tests pin the list and timeline
//! markup, the SSE resume and heartbeat handling, the visible reconnect state, the
//! five-second polling fallback, the `<noscript>` fallback and the absence of paths or
//! secrets, then fetch the same fixture through the API to prove the page's data source.
//! No browser, Node runtime, harness or real home is involved.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use vowp_daemon::{ApiState, select_project, serve_with_shutdown};
use vowp_protocol::api::{EventPage, SessionDetail, SessionPage};
use vowp_protocol::capsule::CAPSULE_VERSION;
use vowp_protocol::{
    Actor, AssistantMessage, Capsule, CapsuleDecision, CapsuleFile, CapsuleId, CapsuleState, CapsuleTelemetry,
    CapsuleTrigger, Clock, EventBody, EventEnvelope, EventSource, EventType, Executor, ExecutorRef, FakeClock,
    FileRole, HarnessKind, JobId, JobKind, NewJob, NodeName, Project, ProjectName, Role, Session, SessionId,
    SessionRole, SessionState, Timestamp, ToolCall, UserMessage, VowpConfig,
};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, Store};

/// A temporary home with one project, one job, a parent/child session chain, a capsule and
/// three events.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    session_id: SessionId,
    parent_id: SessionId,
    capsule: Capsule,
    envelopes: Vec<EventEnvelope>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let clock = FakeClock::at(epoch());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
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

        let job = store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("job a");

        // The parent session gives the detail page a one-entry lineage and has no capsule.
        let parent_id = SessionId::new();
        store
            .upsert_session(&Session {
                id: parent_id.clone(),
                project: Some(project.clone()),
                job_id: None,
                role: Some(SessionRole::Worker),
                title: "parent fixture session".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("parent session row");

        let session_id = SessionId::new();
        let executor = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor");
        let source = EventSource {
            harness: HarnessKind::ClaudeCode,
            model: Some("test-model".to_owned()),
            node: Some(node.clone()),
            native_session_id: None,
        };
        let log = SessionLog::create(&paths, &session_id, Arc::new(clock.clone())).expect("session log");
        let bodies = [
            (
                Actor::User,
                EventBody::UserMessage(UserMessage {
                    text: "first fixture message".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
            ),
            (
                Actor::Assistant,
                EventBody::ToolCall(ToolCall {
                    call_id: "call-1".to_owned(),
                    name: "read".to_owned(),
                    input: serde_json::json!({ "path": "src/lib.rs" }),
                }),
            ),
            (
                Actor::Assistant,
                EventBody::AssistantMessage(AssistantMessage {
                    text: "third fixture message".to_owned(),
                }),
            ),
        ];
        let mut envelopes = Vec::new();
        for (actor, body) in bodies {
            let appended = log
                .append(NewEvent {
                    actor,
                    source: source.clone(),
                    body,
                    raw: None,
                    ts: None,
                })
                .expect("append event");
            envelopes.push(appended.envelope);
        }
        let log_bytes = log.byte_len();
        drop(log);

        let capsule = capsule(&session_id, clock.now());
        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project.clone()),
                job_id: Some(job.id),
                role: Some(SessionRole::Worker),
                title: "fixture session".to_owned(),
                parent_session_id: Some(parent_id.clone()),
                node: node.clone(),
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Running,
                executor: Some(executor),
                last_seq: envelopes.len() as u64,
                last_capsule_id: Some(capsule.capsule_id.clone()),
                log_bytes,
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("session row");
        store
            .insert_capsule(&capsule, "sessions/fixture/capsules/3.json")
            .expect("capsule row");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            session_id,
            parent_id,
            capsule,
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

/// Builds a capsule with every panel section populated.
fn capsule(session: &SessionId, at: Timestamp) -> Capsule {
    Capsule {
        version: CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: at,
        at_seq: 3,
        trigger: CapsuleTrigger::TurnEnd,
        from: ExecutorRef {
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("capsule executor"),
            node: Some(NodeName::parse("test-node").expect("capsule node")),
        },
        intent: "hand the fixture session off".to_owned(),
        constraints: vec!["keep the page read-only".to_owned()],
        decisions: vec![CapsuleDecision {
            what: "render the capsule panel".to_owned(),
            why: "the session-detail API returns it".to_owned(),
            at_seq: Some(3),
        }],
        state: CapsuleState {
            done: vec!["fixture session".to_owned()],
            in_progress: vec!["capsule panel".to_owned()],
            blocked: vec!["nothing".to_owned()],
            next_steps: vec!["render the lineage".to_owned()],
        },
        files: vec![CapsuleFile {
            path: "crates/vowp-daemon/web/sessions.html".to_owned(),
            role: FileRole::Modified,
            note: "capsule panel".to_owned(),
        }],
        gotchas: vec!["the page stays read-only".to_owned()],
        open_questions: vec!["none".to_owned()],
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
    content_type: Option<String>,
    body: String,
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

/// Asserts the shared shell is the static session page with no path or secret.
fn assert_shell(page: &HttpResponse, fixture: &Fixture) {
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "session page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"sessions\"",
        "id=\"list-view\"",
        "id=\"detail-view\"",
        "fetch(\"/api/v1/sessions\")",
        "\"/api/v1/sessions/\"",
        "\"/events?after_seq=\"",
        "EVENT_LIMIT = 200",
        "REFRESH_MS = 5000",
        "setInterval(refreshList, REFRESH_MS)",
        "setInterval(refreshDetail, REFRESH_MS)",
        "Last updated",
        "href=\"/\"",
        "<noscript>The session view requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "page is missing {needle:?}");
    }

    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "page leaked the home path");
    let lowered = page.body.to_ascii_lowercase();
    // The switch form legitimately carries the token unlock and Authorization code; only
    // actual secret values and unrelated secret formats are forbidden.
    for forbidden in ["secret", "api_key", "apikey"] {
        assert!(
            !lowered.contains(forbidden),
            "session page must not mention {forbidden:?}"
        );
    }
}

#[test]
fn sessions_page_lists_the_fixture_sessions() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, "/sessions");
    assert_shell(&page, &fixture);
    for needle in [
        "Session list",
        "<dt>ID</dt>",
        "<dt>Title</dt>",
        "<dt>Project</dt>",
        "<dt>Job</dt>",
        "<dt>Role</dt>",
        "<dt>State</dt>",
        "<dt>Executor</dt>",
        "<dt>Last seq</dt>",
        "<dt>Updated</dt>",
        "link.href = \"/sessions/\"",
        "No sessions.",
    ] {
        assert!(page.body.contains(needle), "list page is missing {needle:?}");
    }

    // The page's data source is the session list API; the same fixture must return the
    // session and the fields the list renders.
    let response = get(server.address, "/api/v1/sessions");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let page: SessionPage = serde_json::from_str(&response.body).expect("SessionPage");
    assert_eq!(page.total, 2);
    assert_eq!(page.next, None);
    let session = page
        .items
        .iter()
        .find(|session| session.id == fixture.session_id)
        .expect("fixture session in the list");
    let parent = page
        .items
        .iter()
        .find(|session| session.id == fixture.parent_id)
        .expect("parent session in the list");
    assert_eq!(parent.title, "parent fixture session");
    assert_eq!(parent.parent_session_id, None);
    assert_eq!(session.title, "fixture session");
    assert_eq!(session.project.as_ref(), Some(&fixture.project));
    assert_eq!(session.job_id.as_ref().map(JobId::as_str), Some("job-a"));
    assert_eq!(session.role, Some(SessionRole::Worker));
    assert_eq!(session.state, SessionState::Running);
    assert_eq!(session.node, fixture.node);
    assert_eq!(
        session.executor.as_ref().map(ToString::to_string).as_deref(),
        Some("opencode:deepseek/deepseek-flash#max")
    );
    assert_eq!(session.last_seq, 3);
    assert_eq!(session.updated_at, epoch());
}

#[test]
fn session_page_shows_the_fixture_events() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, &format!("/sessions/{}", fixture.session_id));
    assert_shell(&page, &fixture);
    for needle in [
        ">Session</h2>",
        "Event timeline",
        "<dt>Seq</dt>",
        "<dt>Time</dt>",
        "<dt>Type</dt>",
        "<dt>Actor</dt>",
        "<dt>Summary</dt>",
        "not found",
    ] {
        assert!(page.body.contains(needle), "detail page is missing {needle:?}");
    }

    // `after_seq` starts at zero, and the page asks for at most 200 events per page.
    let events_path = format!("/api/v1/sessions/{}/events?after_seq=0&limit=200", fixture.session_id);
    let response = get(server.address, &events_path);
    assert_eq!(response.status, 200, "body: {}", response.body);
    let page: EventPage = serde_json::from_str(&response.body).expect("EventPage");
    assert_eq!(page.total, 3);
    assert_eq!(page.next, None);
    assert_eq!(
        page.items.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(page.items, fixture.envelopes);
    assert_eq!(page.items[0].event_type, EventType::UserMessage);
    assert_eq!(page.items[0].actor, Actor::User);
    assert_eq!(page.items[0].payload["text"], "first fixture message");
    assert_eq!(page.items[1].event_type, EventType::ToolCall);
    assert_eq!(page.items[1].payload["name"], "read");
    assert_eq!(page.items[2].event_type, EventType::AssistantMessage);
    assert_eq!(page.items[2].payload["text"], "third fixture message");

    // Polling resumes after the last sequence it rendered and appends only newer events.
    let response = get(
        server.address,
        &format!("/api/v1/sessions/{}/events?after_seq=1&limit=200", fixture.session_id),
    );
    assert_eq!(response.status, 200, "body: {}", response.body);
    let page: EventPage = serde_json::from_str(&response.body).expect("EventPage");
    assert_eq!(page.items.iter().map(|event| event.seq).collect::<Vec<_>>(), vec![2, 3]);

    // An unknown id still serves the shell; the API behind it answers 404, which the page
    // turns into its visible not-found state.
    let missing = SessionId::new();
    let missing_page = get(server.address, &format!("/sessions/{missing}"));
    assert_eq!(missing_page.status, 200, "body: {}", missing_page.body);
    assert!(missing_page.body.contains("not found"));
    let missing_events = get(
        server.address,
        &format!("/api/v1/sessions/{missing}/events?after_seq=0"),
    );
    assert_eq!(missing_events.status, 404, "body: {}", missing_events.body);

    // Unknown non-page paths still use the API's JSON 404 fallback.
    let unknown = get(server.address, &format!("/sessions/{}/extra", fixture.session_id));
    assert_eq!(unknown.status, 404, "body: {}", unknown.body);
}

#[test]
fn session_page_pins_the_token_gated_switch_form() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, &format!("/sessions/{}", fixture.session_id));
    assert_shell(&page, &fixture);
    for needle in [
        "id=\"switch-panel\"",
        "id=\"switch-form\"",
        "id=\"switch-unlock-form\"",
        "TOKEN_KEY = \"vowp-api-token\"",
        "function storedToken()",
        "id=\"switch-token\"",
        "Authorization: \"Bearer \" + token",
        "fetch(\"/api/v1/sessions/\" + encodeURIComponent(sessionId) + \"/switch\"",
        "method: \"POST\"",
        "force: switchForce.checked",
        "id=\"switch-force\"",
        "drain for up to five seconds",
        "id=\"switch-status\"",
        "function describeSwitch(result)",
        "result.error",
        "await refreshDetail()",
        "Current executor:",
    ] {
        assert!(page.body.contains(needle), "switch form is missing {needle:?}");
    }

    assert!(
        !page.body.contains("token="),
        "session page must not put the token in a query string"
    );
    assert!(
        !page.body.contains("searchParams.set(\"token\""),
        "session page must not put the token in the URL"
    );
    assert!(
        !page.body.contains("test-write-token"),
        "session page must not embed a token value"
    );
}

#[test]
fn session_page_renders_the_capsule_and_lineage_from_the_detail_api() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, &format!("/sessions/{}", fixture.session_id));
    assert_shell(&page, &fixture);
    for needle in [
        // The detail page fetches the session-detail document in addition to the timeline.
        "fetch(\"/api/v1/sessions/\" + encodeURIComponent(sessionId))",
        "detail.latest_capsule",
        "detail.lineage",
        "function renderCapsule(capsule)",
        "function renderLineage(lineage, sessions)",
        // The capsule panel's field labels.
        "<dt>Capsule ID</dt>",
        "<dt>At seq</dt>",
        "<dt>Trigger</dt>",
        "<dt>From</dt>",
        ">Intent</h3>",
        ">Constraints</h3>",
        ">Decisions</h3>",
        ">State</h3>",
        ">Done</h4>",
        ">In progress</h4>",
        ">Blocked</h4>",
        ">Next steps</h4>",
        ">Files</h3>",
        ">Gotchas</h3>",
        ">Open questions</h3>",
        // A decision carries its sequence and a file carries its action.
        "decision.at_seq",
        "file.role",
        // The nullable-capsule branch and the lineage label.
        "No capsule yet.",
        "capsuleBody.hidden = true;",
        "capsuleBody.hidden = false;",
        "capsuleEmpty.hidden = true;",
        "capsuleEmpty.hidden = false;",
        // The panel links to the individual capsule page.
        "View capsule",
        "id=\"capsule-link\"",
        "capsuleLink.href =",
        "\"/capsules/\"",
        ">Parent lineage</h2>",
        "No parent sessions.",
        "renderLineage(detail.lineage, sessions)",
    ] {
        assert!(page.body.contains(needle), "capsule page is missing {needle:?}");
    }

    // The panel's data source is the session-detail API: the same fixture must return the
    // capsule and the parent lineage the page renders.
    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.session_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    let home = fixture.paths.root().display().to_string();
    assert!(!response.body.contains(&home), "session detail leaked the home path");
    let detail: SessionDetail = serde_json::from_str(&response.body).expect("SessionDetail");
    assert_eq!(detail.session.id, fixture.session_id);
    assert_eq!(detail.session.title, "fixture session");
    assert_eq!(detail.session.parent_session_id.as_ref(), Some(&fixture.parent_id));
    assert_eq!(detail.latest_capsule.as_ref(), Some(&fixture.capsule));
    assert_eq!(detail.lineage, vec![fixture.parent_id.clone()]);

    let capsule = detail.latest_capsule.expect("fixture capsule");
    assert_eq!(capsule.intent, "hand the fixture session off");
    assert_eq!(capsule.trigger, CapsuleTrigger::TurnEnd);
    assert_eq!(capsule.state.done, vec!["fixture session".to_owned()]);
    assert_eq!(capsule.decisions[0].at_seq, Some(3));
    assert_eq!(capsule.files[0].role, FileRole::Modified);

    // The parent session has no capsule and no parent; that is the page's no-capsule and
    // empty-lineage branch.
    let response = get(server.address, &format!("/api/v1/sessions/{}", fixture.parent_id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    let parent: SessionDetail = serde_json::from_str(&response.body).expect("parent SessionDetail");
    assert_eq!(parent.session.title, "parent fixture session");
    assert!(parent.latest_capsule.is_none());
    assert!(parent.lineage.is_empty());

    // The session list still supplies the titles the lineage links prefer over bare ids.
    let response = get(server.address, "/api/v1/sessions");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let sessions: SessionPage = serde_json::from_str(&response.body).expect("SessionPage");
    assert_eq!(
        sessions
            .items
            .iter()
            .find(|session| session.id == fixture.parent_id)
            .map(|session| session.title.as_str()),
        Some("parent fixture session")
    );
}

#[test]
fn session_page_streams_events_and_falls_back_to_polling() {
    let fixture = Fixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, &format!("/sessions/{}", fixture.session_id));
    assert_shell(&page, &fixture);

    for needle in [
        // The page opens the per-session SSE endpoint after the initial fetch and
        // resumes from the last sequence it rendered.
        "function streamUrl(after)",
        "\"/events/stream?after_seq=\"",
        "new EventSource(streamUrl(lastSeq))",
        // `session` frames append events; `heartbeat` frames only refresh the live line.
        "addEventListener(\"session\"",
        "addEventListener(\"heartbeat\"",
        "addEventListener(\"resync\"",
        "if (!renderEvent(event)) return;",
        "markHeartbeat(at);",
        "function renderEvent(event)",
        "renderedSeqs",
        "function markHeartbeat(",
        // An error or close shows the reconnect state, starts the existing poll, retries
        // the stream with bounded backoff and stops the poll once the stream opens.
        "Reconnecting",
        "function startFallbackPolling()",
        "function stopFallbackPolling()",
        "setInterval(refreshDetail, REFRESH_MS)",
        "\"/events?after_seq=\"",
        "function scheduleRetry()",
        "RETRY_MIN_MS",
        "RETRY_MAX_MS",
        "clearTimeout",
        // A browser without EventSource says so and polls only.
        "Live updates unavailable in this browser",
        "typeof EventSource !== \"function\"",
    ] {
        assert!(page.body.contains(needle), "session page is missing {needle:?}");
    }

    // The fallback uses the same cursor as the stream and only appends newer events, so a
    // reconnect cannot duplicate a timeline row.
    assert!(
        page.body.contains("let after = lastSeq;"),
        "the polling fallback must resume at the last rendered sequence"
    );
}
