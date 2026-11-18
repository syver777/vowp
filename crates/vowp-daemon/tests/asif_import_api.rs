//! Hermetic ASIF import API and page tests: a temporary home, a real loopback listener and
//! raw HTTP requests. The fixtures are built in the test with `vowp_asif::pack` from a
//! minimal synthetic session document, never copied from a developer's home.
//!
//! The route must check the token before the body, accept only the three zip content
//! types, stream into a private capped upload file, import exactly one idle session and
//! answer with a schema-valid `AsifImportResponse`. Error paths must remove the upload and
//! never echo the uploaded bytes, filename or a temp path. No browser, harness, tmux
//! server, real home or external network is involved.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::Server;
use tempfile::TempDir;
use vowp_daemon::{ApiState, ApiToken, select_project};
use vowp_protocol::api::ApiError;
use vowp_protocol::{
    AsifImportResponse, EventBody, NodeName, Project, ProjectName, SessionId, SessionState, Timestamp, VowpConfig,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{NodePaths, Store};

/// The daemon write token used by the upload tests.
const TOKEN: &str = "test-asif-import-token";

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
}

/// A temporary home with one registered project.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path().join("home"));
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.node.name = node.clone();
        let store = Store::open(&paths).expect("open store");
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: epoch(),
                updated_at: epoch(),
            })
            .expect("project row");
        drop(store);
        Self {
            _tmp: tmp,
            paths,
            project_dir,
        }
    }

    fn server(&self) -> Server {
        Server::start_for_with_token(&self.paths, Some(TOKEN))
    }

    fn session_count(&self) -> usize {
        let store = Store::open(&self.paths).expect("store");
        store
            .list_sessions(&vowp_store::SessionFilter::default())
            .expect("sessions")
            .len()
    }
}

/// A minimal valid ASIF document used as an external import fixture.
///
/// The event ids are real VOWP ULIDs so the adapter preserves them; that makes a second
/// import of the same package conflict on event identity, which is the documented
/// re-import behaviour.
fn minimal_document() -> Value {
    let first_event = SessionId::new();
    let second_event = SessionId::new();
    json!({
        "asif_version": "0.4",
        "capture": {
            "id": "asif-test-capture",
            "producer": {"name": "vowp-daemon-import-test", "version": "1"},
            "consistency": "consistent",
            "boundary": "Authored synthetic fixture for the daemon ASIF import route."
        },
        "session": {
            "id": "asif-test-session",
            "title": "Synthetic ASIF import",
            "objective": [{"kind": "text", "text": "Exercise the daemon ASIF import route."}],
            "native_ids": [],
            "lineage": []
        },
        "participants": [
            {"id": "asif-test-human", "kind": "human"},
            {"id": "asif-test-agent", "kind": "agent"}
        ],
        "events": [
            {
                "id": first_event.as_str(),
                "sequence": 0,
                "actor_id": "asif-test-human",
                "kind": "message",
                "causes": [],
                "provenance": {
                    "mode": "synthetic",
                    "producer": "vowp-daemon-import-test",
                    "method": "authored conformance fixture",
                    "inputs": []
                },
                "data": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello from asif"}]
                }
            },
            {
                "id": second_event.as_str(),
                "sequence": 1,
                "actor_id": "asif-test-agent",
                "kind": "message",
                "causes": [{"event_id": first_event.as_str()}],
                "provenance": {
                    "mode": "synthetic",
                    "producer": "vowp-daemon-import-test",
                    "method": "authored conformance fixture",
                    "inputs": []
                },
                "data": {
                    "role": "assistant",
                    "parts": [{"kind": "text", "text": "hello from the synthetic assistant"}]
                },
                "execution_id": "asif-test-execution-1"
            }
        ],
        "branches": [{
            "id": "main",
            "event_ids": [first_event.as_str(), second_event.as_str()],
            "head_event_id": second_event.as_str()
        }],
        "executions": [{
            "id": "asif-test-execution-1",
            "participant_id": "asif-test-agent",
            "status": "completed",
            "context_ids": []
        }],
        "contexts": [],
        "configurations": [],
        "tools": [],
        "resources": [],
        "environments": [],
        "checkpoints": [{
            "id": "asif-test-checkpoint-1",
            "branch_id": "main",
            "at_event_id": second_event.as_str(),
            "knowledge": "observed",
            "status": "idle",
            "context_id": null,
            "configuration_id": null,
            "open_calls": [],
            "open_decisions": [],
            "tasks": [],
            "requirements": []
        }],
        "coverage": [
            {"scope": "participants", "status": "complete", "detail": "Two synthetic participants."},
            {"scope": "conversation", "status": "complete", "detail": "One synthetic exchange."},
            {"scope": "branches", "status": "complete", "detail": "One linear main branch."},
            {"scope": "executions", "status": "complete", "detail": "One completed execution."},
            {"scope": "contexts", "status": "known_empty", "detail": "No context is declared."},
            {"scope": "configuration", "status": "known_empty", "detail": "No configuration is declared."},
            {"scope": "tools", "status": "known_empty", "detail": "No tool activity."},
            {"scope": "decisions", "status": "known_empty", "detail": "No decisions."},
            {"scope": "tasks", "status": "known_empty", "detail": "No tasks."},
            {"scope": "resources", "status": "known_empty", "detail": "No resources."},
            {"scope": "environment", "status": "not_inspected", "detail": "No workspace evidence."},
            {"scope": "native", "status": "known_empty", "detail": "No native harness mapping."},
            {"scope": "usage", "status": "known_empty", "detail": "No usage counters."}
        ],
        "losses": [],
        "required_features": []
    })
}

/// Packs one synthetic document into exact package bytes.
fn package_bytes(document: &Value) -> Vec<u8> {
    let package = vowp_asif::Package::new(serde_json::to_vec_pretty(document).expect("document JSON"));
    vowp_asif::pack(&package).expect("pack fixture")
}

/// Adds one alternate branch that selects only the first user-message event.
fn add_alternate_branch(bytes: &[u8]) -> Vec<u8> {
    let mut package = vowp_asif::unpack_bytes(bytes).expect("unpack package");
    let mut document: Value = serde_json::from_slice(&package.session).expect("session.json");
    let events = document["events"].as_array().expect("events").clone();
    let user_event = events
        .iter()
        .find(|event| event["kind"] == "message" && event["data"]["role"] == "user")
        .expect("user message event");
    let user_id = user_event["id"].clone();
    document["branches"].as_array_mut().expect("branches").push(json!({
        "id": "alternate",
        "event_ids": [user_id.clone()],
        "head_event_id": user_id
    }));
    package.session = serde_json::to_vec_pretty(&document).expect("mutated session JSON");
    vowp_asif::pack(&package).expect("repack")
}

/// One parsed HTTP response with exact bytes.
struct HttpResponse {
    status: u16,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

/// Issues one real GET over loopback.
fn get(address: SocketAddr, path: &str) -> HttpResponse {
    request(address, "GET", path, None, &[], &[])
}

/// Issues one raw POST with optional Content-Type, token and extra headers.
fn post(
    address: SocketAddr,
    path: &str,
    body: &[u8],
    content_type: Option<&str>,
    token: Option<&str>,
    extra: &[(&str, &str)],
) -> HttpResponse {
    let mut headers: Vec<(&str, &str)> = extra.to_vec();
    if let Some(content_type) = content_type {
        headers.push(("Content-Type", content_type));
    }
    let authorization = token.map(|token| ("Authorization", format!("Bearer {token}")));
    let authorization_ref = authorization
        .as_ref()
        .map(|(name, value)| (*name, value.as_str()))
        .into_iter()
        .collect::<Vec<_>>();
    request(address, "POST", path, Some(body), &headers, &authorization_ref)
}

/// Issues one raw HTTP request and reads the exact response bytes.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
    authorization: &[(&str, &str)],
) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
    )
    .expect("write request line");
    if let Some(body) = body {
        write!(stream, "Content-Length: {}\r\n", body.len()).expect("write length");
    }
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n").expect("write header");
    }
    for (name, value) in authorization {
        write!(stream, "{name}: {value}\r\n").expect("write authorization");
    }
    write!(stream, "\r\n").expect("write header terminator");
    if let Some(body) = body {
        stream.write_all(body).expect("write body");
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
        body,
    }
}

/// Decodes a chunked response body.
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

/// Asserts no `asif-upload-*` directory remains, waiting briefly for the response drop.
fn assert_no_upload_dirs(paths: &NodePaths) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining: Vec<String> = std::fs::read_dir(paths.tmp_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("asif-upload-"))
                    .collect()
            })
            .unwrap_or_default();
        if remaining.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "ASIF upload directories remain: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Asserts the response never leaks the served home path.
fn assert_no_home(body: &str, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!body.contains(&home), "response leaked the home path");
}

/// Asserts that `value` validates against the exported schema for `name`.
fn assert_schema(name: &str, value: &Value) {
    let schema =
        vowp_protocol::schema::schema_for_name(name).unwrap_or_else(|| panic!("schema {name} is not exported"));
    let validator =
        jsonschema::validator_for(&schema).unwrap_or_else(|error| panic!("schema {name} is invalid: {error}"));
    if !validator.is_valid(value) {
        let errors: Vec<String> = validator.iter_errors(value).map(|error| error.to_string()).collect();
        panic!("{name} did not validate: {errors:?}\nvalue: {value}");
    }
}

/// Starts a server whose upload cap is small enough to exercise the streaming limit.
fn small_limit_server(fixture: &Fixture) -> Server {
    let selection = select_project(&fixture.paths, None).expect("project selection");
    let state = ApiState::new(&fixture.paths, &selection)
        .with_api_token(Some(ApiToken::new(TOKEN)))
        .with_upload_limit(1024);
    Server::start_with_state(state)
}

#[test]
fn asif_import_without_token_is_503_and_with_wrong_token_is_401() {
    let fixture = Fixture::new();
    let package = package_bytes(&minimal_document());
    let address = "/api/v1/sessions/import/asif";

    let without_token = Server::start_for(&fixture.paths);
    let response = post(
        without_token.address,
        address,
        &package,
        Some("application/zip"),
        None,
        &[],
    );
    assert_eq!(response.status, 503, "body: {}", response.text());
    assert_schema(
        "ApiError",
        &serde_json::from_slice(&response.body).expect("disabled ApiError"),
    );
    assert_no_home(&response.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);
    assert_eq!(fixture.session_count(), 0);

    let with_token = fixture.server();
    let wrong = post(
        with_token.address,
        address,
        &package,
        Some("application/zip"),
        Some("wrong-token"),
        &[],
    );
    assert_eq!(wrong.status, 401, "body: {}", wrong.text());
    assert_eq!(wrong.header("www-authenticate"), Some("Bearer"));
    let error: ApiError = serde_json::from_slice(&wrong.body).expect("unauthorized ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::Auth);
    assert_no_home(&wrong.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);
    assert_eq!(fixture.session_count(), 0);
}

#[test]
fn asif_import_rejects_other_content_types_with_415() {
    let fixture = Fixture::new();
    let package = package_bytes(&minimal_document());
    let server = fixture.server();
    let address = "/api/v1/sessions/import/asif";

    for content_type in [None, Some("application/json"), Some("text/plain")] {
        let response = post(server.address, address, &package, content_type, Some(TOKEN), &[]);
        assert_eq!(
            response.status,
            415,
            "content type {content_type:?} body: {}",
            response.text()
        );
        assert_schema(
            "ApiError",
            &serde_json::from_slice(&response.body).expect("unsupported ApiError"),
        );
        assert_no_upload_dirs(&fixture.paths);
    }
    assert_eq!(fixture.session_count(), 0);
}

#[test]
fn asif_import_over_limit_is_413_and_removes_the_upload() {
    let fixture = Fixture::new();
    let server = small_limit_server(&fixture);
    let oversized = vec![b'x'; 4096];

    let response = post(
        server.address,
        "/api/v1/sessions/import/asif",
        &oversized,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(response.status, 413, "body: {}", response.text());
    let error: ApiError = serde_json::from_slice(&response.body).expect("too-large ApiError");
    assert!(
        error.error.message.contains("1024"),
        "the error must name the limit: {}",
        error.error.message
    );
    assert!(
        !response.text().contains("asif-upload-"),
        "the error echoed a temp path"
    );
    assert_no_home(&response.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);
    assert_eq!(fixture.session_count(), 0);
}

#[test]
fn asif_import_invalid_package_is_400_and_creates_no_session() {
    let fixture = Fixture::new();
    let marker = "not-a-real-asif-package-marker";
    let body = format!("{marker} is not a ZIP archive").into_bytes();
    let server = fixture.server();

    let response = post(
        server.address,
        "/api/v1/sessions/import/asif",
        &body,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(response.status, 400, "body: {}", response.text());
    let error: ApiError = serde_json::from_slice(&response.body).expect("invalid ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::InvalidInput);
    assert!(!response.text().contains(marker), "the error echoed uploaded bytes");
    assert_no_home(&response.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);
    assert_eq!(fixture.session_count(), 0);
}

#[test]
fn asif_import_duplicate_event_ids_is_409_and_keeps_the_existing_session() {
    let fixture = Fixture::new();
    let package = package_bytes(&minimal_document());
    let server = fixture.server();
    let address = "/api/v1/sessions/import/asif";

    let first = post(
        server.address,
        address,
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(first.status, 201, "body: {}", first.text());
    let imported: AsifImportResponse = serde_json::from_slice(&first.body).expect("first response");
    let first_log = std::fs::read(fixture.paths.log_path(&imported.session.id)).expect("first log");

    let second = post(
        server.address,
        address,
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(second.status, 409, "body: {}", second.text());
    let error: ApiError = serde_json::from_slice(&second.body).expect("conflict ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::Conflict);
    assert!(
        error.error.message.contains("EVENT_ID_DUPLICATE"),
        "the error must carry the stable code: {}",
        error.error.message
    );
    assert_eq!(fixture.session_count(), 1, "the first import remains");
    assert_eq!(
        std::fs::read(fixture.paths.log_path(&imported.session.id)).expect("first log"),
        first_log,
        "the first session must not change"
    );
    assert_no_upload_dirs(&fixture.paths);
}

#[test]
fn asif_import_creates_one_idle_session_with_201_and_location() {
    let fixture = Fixture::new();
    let package = package_bytes(&minimal_document());
    let server = fixture.server();

    let response = post(
        server.address,
        "/api/v1/sessions/import/asif",
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(response.status, 201, "body: {}", response.text());
    assert_eq!(response.content_type.as_deref(), Some("application/json"));
    let value = response.json();
    assert_schema("AsifImportResponse", &value);
    let body: AsifImportResponse = serde_json::from_value(value).expect("AsifImportResponse");
    assert_eq!(
        response.header("location"),
        Some(format!("/api/v1/sessions/{}", body.session.id).as_str())
    );
    assert_eq!(
        response.header("x-vowp-asif-losses"),
        Some(body.report.loss_count.to_string().as_str())
    );
    assert_eq!(body.session.state, SessionState::Idle);
    assert_eq!(body.session.title, "Synthetic ASIF import");
    assert_eq!(body.session.cwd, fixture.project_dir.display().to_string());
    assert!(
        body.session.project.is_none(),
        "an ASIF source without a local project leaves the session project null"
    );
    assert!(body.report.counts.events_out >= 2);
    assert_no_home(&response.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);

    let store = Store::open(&fixture.paths).expect("store");
    assert_eq!(
        store
            .list_sessions(&vowp_store::SessionFilter::default())
            .expect("sessions")
            .len(),
        1
    );
    assert!(
        store.lease(&body.session.id).expect("lease").is_none(),
        "import must not create a lease"
    );
    assert_eq!(
        store.event_rows(&body.session.id, 0, 100).expect("event rows").len(),
        body.report.counts.events_out as usize,
        "every written event is indexed"
    );
    drop(store);

    let events = SessionLog::open_read(&fixture.paths, &body.session.id)
        .expect("open imported log")
        .read_all()
        .expect("read imported log");
    assert!(!events.is_empty());
    assert!(
        matches!(events[0].body().expect("first body"), EventBody::SessionCreated(_)),
        "the imported log must start with session.created"
    );
}

#[test]
fn asif_import_never_echoes_uploaded_bytes_or_a_filename() {
    let fixture = Fixture::new();
    let marker = "uploaded-secret-marker-do-not-echo";
    let server = fixture.server();
    let filename = format!("{marker}.asif.zip");

    let invalid = format!("{marker} is not a package").into_bytes();
    let response = post(
        server.address,
        "/api/v1/sessions/import/asif",
        &invalid,
        Some("application/zip"),
        Some(TOKEN),
        &[("Content-Disposition", &format!("attachment; filename=\"{filename}\""))],
    );
    assert_eq!(response.status, 400, "body: {}", response.text());
    assert!(
        !response.text().contains(marker),
        "the error echoed uploaded bytes or filename"
    );
    assert!(!response.text().contains(&filename), "the error echoed the filename");
    assert!(
        !response.text().contains("asif-upload-"),
        "the error echoed a temp path"
    );
    assert_no_home(&response.text(), &fixture);

    let mut document = minimal_document();
    document["capture"]["boundary"] = json!(marker);
    let package = package_bytes(&document);
    let imported = post(
        server.address,
        "/api/v1/sessions/import/asif",
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[("Content-Disposition", &format!("attachment; filename=\"{filename}\""))],
    );
    assert_eq!(imported.status, 201, "body: {}", imported.text());
    assert!(
        !imported.text().contains(marker),
        "the response echoed unmapped uploaded bytes"
    );
    assert!(!imported.text().contains(&filename), "the response echoed the filename");
    assert!(
        !imported.text().contains("asif-upload-"),
        "the response echoed a temp path"
    );
    assert_no_home(&imported.text(), &fixture);
    assert_no_upload_dirs(&fixture.paths);
}

#[test]
fn asif_import_branch_is_required_for_a_multi_branch_package() {
    let fixture = Fixture::new();
    let package = add_alternate_branch(&package_bytes(&minimal_document()));
    let server = fixture.server();
    let address = "/api/v1/sessions/import/asif";

    let missing = post(
        server.address,
        address,
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(missing.status, 400, "body: {}", missing.text());
    let error: ApiError = serde_json::from_slice(&missing.body).expect("branch ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::InvalidInput);
    assert!(
        error.error.message.to_ascii_lowercase().contains("branch"),
        "the error should name the missing branch: {}",
        error.error.message
    );
    assert_eq!(fixture.session_count(), 0);

    let selected = post(
        server.address,
        &format!("{address}?branch=alternate"),
        &package,
        Some("application/zip"),
        Some(TOKEN),
        &[],
    );
    assert_eq!(selected.status, 201, "body: {}", selected.text());
    let body: AsifImportResponse = serde_json::from_slice(&selected.body).expect("selected response");
    let events = SessionLog::open_read(&fixture.paths, &body.session.id)
        .expect("open imported log")
        .read_all()
        .expect("read imported log");
    let texts: Vec<String> = events
        .iter()
        .filter_map(|event| match event.body().ok()? {
            EventBody::UserMessage(message) => Some(message.text),
            EventBody::AssistantMessage(message) => Some(message.text),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|text| text == "hello from asif"), "{texts:?}");
    assert!(
        texts.iter().all(|text| text != "hello from the synthetic assistant"),
        "the unselected branch must not be mapped: {texts:?}"
    );
    assert_no_upload_dirs(&fixture.paths);
}

#[test]
fn import_page_posts_raw_zip_with_bearer_and_clears_on_401() {
    let fixture = Fixture::new();
    let server = fixture.server();

    let response = get(server.address, "/import");
    assert_eq!(response.status, 200, "body: {}", response.text());
    let content_type = response.content_type.as_deref().expect("content type");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "import page must be text/html, got {content_type:?}"
    );
    let body = response.text();
    for needle in [
        "id=\"import\"",
        "<link rel=\"manifest\" href=\"/manifest.webmanifest\">",
        "<meta name=\"theme-color\" content=\"#16855b\">",
        "if ('serviceWorker' in navigator)",
        "navigator.serviceWorker.register('/sw.js')",
        "accept=\".asif.zip,application/zip\"",
        "const TOKEN_KEY = \"vowp-api-token\";",
        "sessionStorage",
        "const MAX_ARCHIVE_BYTES = 272 * 1024 * 1024;",
        "const LIMIT_TEXT = \"272 MiB\";",
        "/api/v1/sessions/import/asif",
        "?branch=",
        "Authorization: \"Bearer \" + token",
        "\"Content-Type\": \"application/zip\"",
        "body: file",
        "response.status === 201",
        "response.status === 401",
        "setStoredToken(\"\")",
        "not continued automatically",
        "<noscript>The import page requires JavaScript.</noscript>",
    ] {
        assert!(body.contains(needle), "import page is missing {needle:?}");
    }
    assert!(!body.contains("FormData"), "the page must not use multipart form data");
    assert!(!body.contains("file.name"), "the page must not send a filename");
    assert!(
        !body.contains("<script src="),
        "the page must not load an external script"
    );
    let home = fixture.paths.root().display().to_string();
    assert!(!body.contains(&home), "page leaked the home path");
    assert!(!body.contains(TOKEN), "page embedded the daemon write token");
}
