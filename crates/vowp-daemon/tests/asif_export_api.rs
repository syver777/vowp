//! Hermetic ASIF download tests: a temporary home, a real loopback listener and raw GETs.
//!
//! The fixture writes a real session log, the download route exports it through the core
//! ASIF facade, and the test compares the streamed bytes with a direct export. The route
//! must return the documented attachment headers, a schema-valid report preview, a
//! path-free 409 for a corrupt stored log, a 404 for an unknown id, allow a session from
//! another project in the same home, and a 413 for a resource over the adapter limit.
//! No browser, harness, tmux server, real home or external network is involved.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::Server;
use tempfile::TempDir;
use vowp_core::session::asif::{AsifExport, AsifExportOptions, export_asif};
use vowp_protocol::api::ApiError;
use vowp_protocol::{
    Actor, AsifReport, AssistantMessage, BlobRef, EventBody, EventSource, FakeClock, HarnessKind, NodeName, Project,
    ProjectName, Session, SessionCreated, SessionId, SessionRole, SessionState, Timestamp, ToolCall, ToolResult,
    UserMessage, VowpConfig,
};
use vowp_store::blobs::BlobStore;
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, Store};

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
}

/// A temporary home with one project and one exportable session.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    node: NodeName,
    project_dir: PathBuf,
    session_id: SessionId,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path().join("home"));
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let store = Store::open(&paths).expect("open store");
        let config = config(&project, &node, &project_dir);
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

        let session_id = SessionId::new();
        let clock = Arc::new(FakeClock::at(epoch()));
        let log = SessionLog::create(&paths, &session_id, clock).expect("session log");
        let source = source(&node);
        for (actor, body) in [
            (
                Actor::System,
                EventBody::SessionCreated(SessionCreated {
                    title: "ASIF export fixture".to_owned(),
                    cwd: project_dir.display().to_string(),
                    project: Some(project.clone()),
                    job_id: None,
                    role: Some(SessionRole::Worker),
                    parent_session_id: None,
                }),
            ),
            (
                Actor::User,
                EventBody::UserMessage(UserMessage {
                    text: "export this fixture session".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
            ),
            (
                Actor::Assistant,
                EventBody::AssistantMessage(AssistantMessage {
                    text: "the fixture is exportable".to_owned(),
                }),
            ),
        ] {
            log.append(NewEvent {
                actor,
                source: source.clone(),
                body,
                raw: None,
                ts: None,
            })
            .expect("append event");
        }
        let last_seq = 3;
        let log_bytes = log.byte_len();
        log.sync().expect("sync log");
        drop(log);

        let store = Store::open(&paths).expect("store");
        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project.clone()),
                job_id: None,
                role: Some(SessionRole::Worker),
                title: "ASIF export fixture".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: project_dir.display().to_string(),
                state: SessionState::Idle,
                executor: None,
                last_seq,
                last_capsule_id: None,
                log_bytes,
                created_at: epoch(),
                updated_at: epoch(),
            })
            .expect("session row");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            node,
            project_dir,
            session_id,
        }
    }

    /// Adds a second project and one session in it, without changing the served project.
    fn add_home_session(&self) -> SessionId {
        let other_project = ProjectName::parse("other").expect("other project");
        let other_dir = self.project_dir.with_file_name("other-project");
        std::fs::create_dir_all(&other_dir).expect("other project dir");
        let store = Store::open(&self.paths).expect("store");
        store
            .upsert_project(&Project {
                name: other_project.clone(),
                path: other_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(config(&other_project, &self.node, &other_dir)).expect("config JSON"),
                created_at: epoch(),
                updated_at: epoch(),
            })
            .expect("other project row");
        let id = SessionId::new();
        let clock = Arc::new(FakeClock::at(epoch()));
        let log = SessionLog::create(&self.paths, &id, clock).expect("other log");
        log.append(NewEvent {
            actor: Actor::User,
            source: source(&self.node),
            body: EventBody::UserMessage(UserMessage {
                text: "a session outside the served project".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
            raw: None,
            ts: None,
        })
        .expect("append");
        let log_bytes = log.byte_len();
        log.sync().expect("sync");
        drop(log);
        store
            .upsert_session(&Session {
                id: id.clone(),
                project: Some(other_project),
                job_id: None,
                role: None,
                title: "home session".to_owned(),
                parent_session_id: None,
                node: self.node.clone(),
                cwd: other_dir.display().to_string(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 1,
                last_capsule_id: None,
                log_bytes,
                created_at: epoch(),
                updated_at: epoch(),
            })
            .expect("other session row");
        id
    }

    /// Adds a tool result whose blob is one byte over the adapter's single-resource limit.
    fn add_oversized_blob(&self) {
        // Invalid UTF-8 takes the adapter's binary-resource path, so the test reaches the
        // size check without spending a minute scrubbing 64 MiB of text in a debug build.
        let bytes = vec![0xFF_u8; 64 * 1024 * 1024 + 1];
        let blob: BlobRef = BlobStore::new(self.paths.blobs_dir(&self.session_id))
            .put(&bytes)
            .expect("oversized blob");
        let clock = Arc::new(FakeClock::at(epoch()));
        let log = SessionLog::open(&self.paths, &self.session_id, clock).expect("open log");
        let source = source(&self.node);
        log.append(NewEvent {
            actor: Actor::Assistant,
            source: source.clone(),
            body: EventBody::ToolCall(ToolCall {
                call_id: "call-big".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"command": "produce a large output"}),
            }),
            raw: None,
            ts: None,
        })
        .expect("tool call");
        log.append(NewEvent {
            actor: Actor::Tool,
            source,
            body: EventBody::ToolResult(ToolResult {
                call_id: "call-big".to_owned(),
                ok: true,
                output: None,
                output_ref: Some(blob),
                exit_code: Some(0),
            }),
            raw: None,
            ts: None,
        })
        .expect("tool result");
        log.sync().expect("sync");
    }

    /// Exports directly through the core facade for the expected bytes and report.
    fn direct_export(&self) -> (AsifExport, Vec<u8>) {
        let out = self._tmp.path().join("direct.asif.zip");
        let exported = export_asif(&self.paths, &self.session_id, &out, &AsifExportOptions::default()).expect("export");
        let bytes = std::fs::read(&out).expect("direct bytes");
        (exported, bytes)
    }
}

fn config(project: &ProjectName, node: &NodeName, dir: &Path) -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.project.path = Some(dir.display().to_string());
    config.node.name = node.clone();
    config
}

fn source(node: &NodeName) -> EventSource {
    EventSource {
        harness: HarnessKind::Opencode,
        model: Some("deepseek/deepseek-flash".to_owned()),
        node: Some(node.clone()),
        native_session_id: None,
    }
}

/// One parsed HTTP response with exact body bytes.
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

/// Issues one real GET over loopback and reads the exact response bytes.
fn get(address: SocketAddr, path: &str) -> HttpResponse {
    request(address, "GET", path, None, &[], &[])
}

/// Issues one raw request; `headers` are additional request headers.
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

/// Asserts the exported temp directories eventually disappear after the response ends.
fn assert_no_export_dirs(paths: &NodePaths) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining: Vec<String> = std::fs::read_dir(paths.tmp_dir())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with("asif-export-"))
                    .collect()
            })
            .unwrap_or_default();
        if remaining.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "ASIF temp directories remain: {remaining:?}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Asserts the response and error body never leak the home path.
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

#[test]
fn asif_download_returns_zip_attachment_headers_and_exact_bytes() {
    let fixture = Fixture::new();
    let (expected, direct_bytes) = fixture.direct_export();
    let server = Server::start_for(&fixture.paths);

    let response = get(
        server.address,
        &format!("/api/v1/sessions/{}/export/asif", fixture.session_id),
    );
    assert_eq!(response.status, 200, "body: {}", response.text());
    assert_eq!(response.content_type.as_deref(), Some("application/zip"));
    assert_eq!(
        response.header("content-disposition"),
        Some(format!("attachment; filename=\"{}.asif.zip\"", fixture.session_id).as_str())
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        response.header("content-length"),
        Some(direct_bytes.len().to_string().as_str())
    );
    assert_eq!(
        response.header("x-vowp-asif-losses"),
        Some(expected.report.loss_count.to_string().as_str())
    );
    assert_eq!(
        response.header("x-vowp-asif-unsupported"),
        Some(expected.report.unsupported.len().to_string().as_str())
    );
    assert_eq!(
        response.body, direct_bytes,
        "the route must stream the exact package bytes"
    );
    assert!(response.body.starts_with(b"PK\x03\x04"));
    assert_no_export_dirs(&fixture.paths);
}

#[test]
fn asif_download_report_query_returns_counts_and_losses() {
    let fixture = Fixture::new();
    let (expected, _bytes) = fixture.direct_export();
    let server = Server::start_for(&fixture.paths);

    for query in ["report=1", "report=true"] {
        let response = get(
            server.address,
            &format!("/api/v1/sessions/{}/export/asif?{query}", fixture.session_id),
        );
        assert_eq!(response.status, 200, "body: {}", response.text());
        assert_eq!(response.content_type.as_deref(), Some("application/json"));
        let value = response.json();
        assert_schema("AsifReport", &value);
        let report: AsifReport = serde_json::from_value(value).expect("AsifReport");
        assert_eq!(report.source_session.as_deref(), Some(fixture.session_id.as_str()));
        assert_eq!(report.counts.events_in, 3);
        assert_eq!(report.loss_count as usize, report.losses.len());
        assert!(report.loss_count > 0, "a normal session declares inherent losses");
        assert!(!report.unsupported.is_empty());
        assert_eq!(report, expected.report);
    }

    let bad = get(
        server.address,
        &format!("/api/v1/sessions/{}/export/asif?report=0", fixture.session_id),
    );
    assert_eq!(bad.status, 400, "body: {}", bad.text());
    let error: ApiError = serde_json::from_slice(&bad.body).expect("ApiError");
    assert_schema("ApiError", &serde_json::to_value(&error).expect("error JSON"));
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::InvalidInput);
    assert_no_home(&bad.text(), &fixture);
    assert_no_export_dirs(&fixture.paths);
}

#[test]
fn asif_download_unknown_session_is_404_and_a_home_session_is_allowed() {
    let fixture = Fixture::new();
    let server = Server::start_for(&fixture.paths);

    let unknown = get(
        server.address,
        &format!("/api/v1/sessions/{}/export/asif", SessionId::new()),
    );
    assert_eq!(unknown.status, 404, "body: {}", unknown.text());
    assert_schema(
        "ApiError",
        &serde_json::from_slice(&unknown.body).expect("unknown ApiError"),
    );
    assert_no_home(&unknown.text(), &fixture);

    let other = fixture.add_home_session();
    let response = get(server.address, &format!("/api/v1/sessions/{other}/export/asif"));
    assert_eq!(
        response.status,
        200,
        "a session outside the served project must still download: {}",
        response.text()
    );
    assert!(response.body.starts_with(b"PK\x03\x04"));
    assert_no_export_dirs(&fixture.paths);
}

#[test]
fn asif_download_corrupt_session_is_409_and_leaves_no_temp() {
    let fixture = Fixture::new();
    let marker = "corrupt-session-secret-marker";
    std::fs::write(
        fixture.paths.log_path(&fixture.session_id),
        format!("{marker} this is not a JSON event\n"),
    )
    .expect("corrupt log");
    let server = Server::start_for(&fixture.paths);

    let response = get(
        server.address,
        &format!("/api/v1/sessions/{}/export/asif", fixture.session_id),
    );
    assert_eq!(response.status, 409, "body: {}", response.text());
    let error: ApiError = serde_json::from_slice(&response.body).expect("ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::Conflict);
    assert!(!response.text().contains(marker), "the error echoed source bytes");
    assert!(
        !response.text().contains("asif-export-"),
        "the error echoed a temp path"
    );
    assert_no_home(&response.text(), &fixture);
    assert_no_export_dirs(&fixture.paths);
}

#[test]
fn asif_download_over_limit_is_413() {
    let fixture = Fixture::new();
    fixture.add_oversized_blob();
    let server = Server::start_for(&fixture.paths);

    let response = get(
        server.address,
        &format!("/api/v1/sessions/{}/export/asif", fixture.session_id),
    );
    assert_eq!(response.status, 413, "body: {}", response.text());
    let error: ApiError = serde_json::from_slice(&response.body).expect("ApiError");
    assert_eq!(error.error.kind, vowp_protocol::ErrorKind::InvalidInput);
    assert!(
        error.error.message.contains("TOO_LARGE"),
        "the error should carry the adapter limit code: {}",
        error.error.message
    );
    assert_no_home(&response.text(), &fixture);
    assert_no_export_dirs(&fixture.paths);
}
