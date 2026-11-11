//! App-host tests. Hermetic: a temporary orchestrator directory, a private `XDG_*` tree,
//! fake `opencode`/`claude` scripts and a loopback HTTP fake. No real app, no real
//! `~/.vowp`, no default tmux server and no external network.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5 and the app-host tests in §15.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use vowp_adapters::proc::process_alive;
use vowp_core::CoreError;
use vowp_core::surface::claude_remote::{ClaudeRemoteHost, NO_LOGIN};
use vowp_core::surface::mirror::MirrorCursor;
use vowp_core::surface::native::{AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, TranscriptEntry};
use vowp_core::surface::opencode_host::OpencodeHost;
use vowp_protocol::{JoinInstructions, OrchestratorApp, SessionId, Timestamp};

/// One recorded `(method, path, authorization)` request.
type SeenRequest = (String, String, String);

const PASSWORD: &str = "test-password";
const EXPECTED_AUTH: &str = "Basic b3BlbmNvZGU6dGVzdC1wYXNzd29yZA==";

fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("test timestamp")
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write fake script");
    let mut permissions = std::fs::metadata(path).expect("stat fake script").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("chmod fake script");
}

/// A loopback HTTP fake with the opencode v2 shapes the host reads.
struct FakeOpencode {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    events: Arc<Mutex<Vec<Value>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeOpencode {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake opencode");
        listener.set_nonblocking(true).expect("nonblocking fake opencode");
        let addr = listener.local_addr().expect("fake opencode address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(vec![
            json!({
                "id": "msg_user_1",
                "sessionID": "ses_fake",
                "type": "user",
                "text": "hello",
                "time": {"created": 1_700_000_000_000_i64}
            }),
            json!({
                "id": "msg_assistant_1",
                "sessionID": "ses_fake",
                "type": "assistant",
                "content": [{"type": "text", "text": "hi there"}],
                "model": {"id": "fake-model"},
                "cost": 0.25,
                "tokens": {"input": 3, "output": 5},
                "time": {"created": 1_700_000_001_000_i64}
            }),
        ]));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let events = Arc::clone(&events);
            let messages = Arc::clone(&messages);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => handle(stream, &seen, &events, &messages),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            addr,
            seen,
            events,
            shutdown,
            thread: Some(thread),
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    fn push_event(&self, event: Value) {
        self.events.lock().expect("events").push(event);
    }

    fn seen_paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect()
    }

    fn auth_headers(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("seen")
            .iter()
            .map(|(_, _, auth)| auth.clone())
            .collect()
    }
}

impl Drop for FakeOpencode {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle(
    mut stream: TcpStream,
    seen: &Arc<Mutex<Vec<SeenRequest>>>,
    events: &Arc<Mutex<Vec<Value>>>,
    messages: &Arc<Mutex<Vec<Value>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
        if let Some(end) = find_header_end(&raw) {
            let head = String::from_utf8_lossy(&raw[..end]).into_owned();
            let length = content_length(&head);
            if raw.len() >= end + 4 + length {
                break;
            }
        }
    }
    let Some(end) = find_header_end(&raw) else {
        return;
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let body = String::from_utf8_lossy(&raw[end + 4..]).into_owned();
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let auth = headers.get("authorization").cloned().unwrap_or_default();
    seen.lock()
        .expect("seen")
        .push((method.clone(), path.clone(), auth.clone()));

    if auth != EXPECTED_AUTH {
        respond(&mut stream, 401, "application/json", "{\"error\":\"unauthorized\"}");
        return;
    }
    let route = path.split('?').next().unwrap_or(&path);
    match (method.as_str(), route) {
        ("POST", "/api/session") => {
            let directory = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/location/directory")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let response = json!({
                "data": {"id": "ses_fake", "location": {"directory": directory}}
            })
            .to_string();
            respond(&mut stream, 200, "application/json", &response);
        }
        ("GET", "/api/session") => {
            respond(
                &mut stream,
                200,
                "application/json",
                "{\"data\":[{\"id\":\"ses_fake\"}]}",
            );
        }
        ("GET", "/api/session/ses_fake/message") => {
            let body = json!({"data": messages.lock().expect("messages").clone()}).to_string();
            respond(&mut stream, 200, "application/json", &body);
        }
        ("GET", "/api/experimental/session/ses_fake/export") => {
            let body = json!({
                "info": {"id": "ses_fake"},
                "messages": messages.lock().expect("messages").clone(),
            })
            .to_string();
            respond(&mut stream, 200, "application/json", &body);
        }
        ("GET", "/api/event") => {
            let mut body = String::new();
            for event in events.lock().expect("events").iter() {
                body.push_str("data: ");
                body.push_str(&event.to_string());
                body.push_str("\n\n");
            }
            respond(&mut stream, 200, "text/event-stream", &body);
        }
        _ => respond(&mut stream, 404, "application/json", "{\"error\":\"not found\"}"),
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Reads one request so the server's later close is a FIN, not a reset for unread input.
fn read_request(stream: &mut TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
        if let Some(end) = find_header_end(&raw) {
            let head = String::from_utf8_lossy(&raw[..end]);
            if raw.len() >= end + 4 + content_length(&head) {
                break;
            }
        }
    }
}

/// A loopback server that runs `script` for each accepted connection.
///
/// The script gets the connection and its zero-based attempt number. It runs inline on the
/// accept thread, so a script that sleeps also keeps later connections waiting in the
/// listener backlog; that is how a first health attempt can outlive the client timeout.
struct ScriptedServer {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedServer {
    fn start(mut script: impl FnMut(TcpStream, usize) + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind scripted server");
        listener.set_nonblocking(true).expect("nonblocking scripted server");
        let addr = listener.local_addr().expect("scripted server address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let accepted = Arc::clone(&accepted);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let attempt = accepted.fetch_add(1, Ordering::SeqCst);
                            read_request(&mut stream);
                            script(stream, attempt);
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            addr,
            accepted,
            shutdown,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn write_service_state(workdir: &Path, url: &str, password: &str) {
    let state = workdir.join(".opencode").join("state").join("opencode");
    std::fs::create_dir_all(&state).expect("service state dir");
    std::fs::write(
        state.join("service.json"),
        json!({"id": "fake", "version": "2.0.28", "url": url, "pid": 4242, "password": password}).to_string(),
    )
    .expect("service.json");
    std::fs::write(
        workdir.join("host.json"),
        json!({
            "kind": "opencode",
            "name": "orch",
            "pid": 4242,
            "port": 1,
            "url": url,
            "started_at": "2026-11-10T10:00:00Z",
            "workdir": workdir.display().to_string(),
            "service_json": state.join("service.json").display().to_string(),
            "native_session_id": "ses_fake",
            "vowp_session": "ses_vowp"
        })
        .to_string(),
    )
    .expect("host.json");
}

fn handle_for(workdir: &Path) -> AppHandle {
    AppHandle {
        kind: OrchestratorApp::Opencode,
        pid: 4242,
        port: Some(1),
        started_at: ts("2026-11-10T10:00:00Z"),
        workdir: workdir.to_path_buf(),
    }
}

fn empty_cursor() -> MirrorCursor {
    MirrorCursor {
        last_native_id: None,
        updated_at: ts("2026-11-10T10:00:00Z"),
    }
}

/// Polls `transcript_since` until an entry with `text` appears, or panics after two seconds.
fn wait_for_text(host: &OpencodeHost, handle: &AppHandle, cursor: &MirrorCursor, text: &str) -> TranscriptBatch {
    for _ in 0..100 {
        let batch = retry_transient(|| host.transcript_since(handle, cursor)).expect("transcript while waiting");
        if batch.entries.iter().any(|entry| entry.text.as_deref() == Some(text)) {
            return batch;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {text:?}");
}

fn fake_opencode_script(port: u16, version: &str) -> String {
    format!(
        r#"#!/bin/sh
set -e
state="$XDG_STATE_HOME/opencode"
mkdir -p "$state"
case "$1" in
  service)
    case "$2" in
      set) exit 0 ;;
      start)
        nohup sleep 60 >/dev/null 2>&1 &
        pid=$!
        printf '%s\n' "$pid" > "$state/fake.pid"
        printf '{{"id":"fake","version":"{version}","url":"http://127.0.0.1:{port}","pid":%s,"password":"{password}"}}\n' "$pid" > "$state/service.json"
        exit 0 ;;
      stop)
        if [ -f "$state/fake.pid" ]; then
          kill "$(cat "$state/fake.pid")" 2>/dev/null || true
          rm -f "$state/fake.pid"
        fi
        exit 0 ;;
    esac ;;
esac
exit 0
"#,
        password = PASSWORD
    )
}

fn host_request(tmp: &TempDir, port: Option<u16>) -> HostRequest {
    HostRequest {
        name: "orch".to_owned(),
        session: SessionId::new(),
        port,
        workdir: tmp.path().join("orchestrator"),
        config: tmp.path().join("orchestrator").join("opencode.json"),
    }
}

/// True for I/O failures caused by a parallel test rather than the host: a signal
/// interrupts a loopback syscall, or a freshly written fake script is still busy.
fn transient_io(error: &CoreError) -> bool {
    match error {
        CoreError::Io { source, .. } => matches!(source.kind(), ErrorKind::Interrupted | ErrorKind::ExecutableFileBusy),
        _ => false,
    }
}

/// Retries an app-host operation when a parallel test interrupts it. Any other error is
/// returned immediately, so a real failure is never hidden.
fn retry_transient<T>(mut operation: impl FnMut() -> Result<T, CoreError>) -> Result<T, CoreError> {
    for _ in 0..19 {
        match operation() {
            Err(error) if transient_io(&error) => thread::sleep(Duration::from_millis(10)),
            result => return result,
        }
    }
    operation()
}

/// Retries the Claude login probe while it reports no login, so a signal that interrupts
/// the fake script does not fail the positive-login test. A genuinely logged-out home
/// still fails after the bounded retries.
fn login_present(host: &ClaudeRemoteHost) -> bool {
    for _ in 0..19 {
        if host.login_present() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    host.login_present()
}

/// Starts the logged-in Claude fake, retrying the product's own auth probe as well: a
/// signal can make that probe read a transient failure even though the fake is logged in.
fn start_logged_in(host: &ClaudeRemoteHost, request: &HostRequest) -> Result<AppHandle, CoreError> {
    for _ in 0..19 {
        match retry_transient(|| host.start(request)) {
            Err(error) if matches!(&error, CoreError::Refused(message) if message == NO_LOGIN) => {
                thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    retry_transient(|| host.start(request))
}

/// Whether a loopback port currently accepts TCP connections.
fn port_accepts_connections(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok()
}

/// Picks a loopback port that refuses connections. Another test binary can bind the
/// ephemeral port between the probe and the check, so retry with a new port instead of
/// assuming the first one stays free.
fn free_port() -> u16 {
    const ATTEMPTS: usize = 20;
    for _ in 0..ATTEMPTS {
        let probe = TcpListener::bind(("127.0.0.1", 0)).expect("probe port");
        let port = probe.local_addr().expect("probe address").port();
        drop(probe);
        thread::sleep(Duration::from_millis(10));
        if !port_accepts_connections(port) {
            return port;
        }
    }
    panic!("no free loopback port after {ATTEMPTS} attempts");
}

#[test]
fn opencode_host_starts_and_stops_with_fake_server() {
    let tmp = tempdir().expect("temp dir");
    let fake = FakeOpencode::start();
    let script = tmp.path().join("opencode");
    write_script(&script, &fake_opencode_script(fake.port(), "2.0.28"));
    let host = OpencodeHost::new(&script).with_startup(Duration::from_secs(5));
    let request = host_request(&tmp, Some(fake.port()));
    std::fs::create_dir_all(&request.workdir).expect("orchestrator dir");

    let handle = retry_transient(|| host.start(&request)).expect("start opencode host");
    assert_eq!(handle.kind, OrchestratorApp::Opencode);
    assert_eq!(handle.port, Some(fake.port()));
    assert!(process_alive(handle.pid));
    let host_json = request.workdir.join("host.json");
    assert!(host_json.is_file(), "host.json was not written");
    let mode = std::fs::metadata(&host_json)
        .expect("host.json metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "host.json must be private");

    let health = retry_transient(|| host.health(&handle)).expect("health");
    assert!(health.ok, "health detail: {}", health.detail);
    assert!(health.detail.contains("/api/session"));

    let join = host.join_instructions(&handle).expect("join instructions");
    assert!(join.desktop.contains("vowp tunnel open"), "{}", join.desktop);
    assert!(
        join.desktop.contains("opencode") && join.desktop.contains("pair"),
        "{}",
        join.desktop
    );
    assert!(join.phone.contains("--server"), "{}", join.phone);
    assert!(join.phone.contains("127.0.0.1"), "{}", join.phone);
    assert!(join.chat.contains("vowp chat"), "{}", join.chat);
    for text in [&join.desktop, &join.phone, &join.chat] {
        assert!(
            !text.contains(PASSWORD),
            "join instructions leaked the password: {text}"
        );
    }

    retry_transient(|| host.stop(&handle)).expect("stop opencode host");
    assert!(!process_alive(handle.pid), "the fake service is still running");
    assert!(!host_json.exists(), "host.json survived stop");
    assert!(
        !request.workdir.join(".opencode/state/opencode/service.json").exists(),
        "service.json survived stop"
    );
    let health = retry_transient(|| host.health(&handle)).expect("health after stop");
    assert!(!health.ok);
}

#[test]
fn opencode_v2_service_pair_basic_auth() {
    let tmp = tempdir().expect("temp dir");
    let fake = FakeOpencode::start();
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    write_service_state(&workdir, &fake.url(), PASSWORD);
    let host = OpencodeHost::default();
    let handle = handle_for(&workdir);

    let health = retry_transient(|| host.health(&handle)).expect("health");
    assert!(health.ok, "health detail: {}", health.detail);
    assert!(!fake.auth_headers().is_empty(), "the host sent no request");
    assert!(
        fake.auth_headers().iter().all(|auth| auth == EXPECTED_AUTH),
        "requests did not use Basic opencode:<token>: {:?}",
        fake.auth_headers()
    );

    let first = retry_transient(|| host.transcript_since(&handle, &empty_cursor())).expect("backfill");
    assert_eq!(first.entries.len(), 3, "{:?}", first.entries);
    assert!(
        first.entries.iter().any(|entry| entry.text.as_deref() == Some("hello")),
        "backfill missed the user message: {:?}",
        first.entries
    );
    assert!(
        first
            .entries
            .iter()
            .any(|entry| entry.text.as_deref() == Some("hi there")),
        "backfill missed the assistant message: {:?}",
        first.entries
    );
    let cost = first
        .entries
        .iter()
        .find(|entry| entry.kind == vowp_core::surface::native::TranscriptKind::Cost)
        .expect("backfill cost");
    assert_eq!(cost.usd, Some(0.25));
    assert_eq!(cost.tokens.expect("tokens").input, 3);
    assert_eq!(first.cursor.last_native_id.as_deref(), Some("cost:msg_assistant_1"));
    assert!(
        fake.seen_paths()
            .iter()
            .any(|path| path == "/api/experimental/session/ses_fake/export"),
        "backfill did not use the export endpoint: {:?}",
        fake.seen_paths()
    );

    fake.push_event(json!({
        "id": "evt_start", "type": "session.execution.started",
        "created": 1_700_000_002_000_i64,
        "data": {"sessionID": "ses_fake"}, "durable": {"seq": 1}
    }));
    fake.push_event(json!({
        "id": "evt_step_start", "type": "session.step.started",
        "created": 1_700_000_002_050_i64,
        "data": {
            "sessionID": "ses_fake", "assistantMessageID": "msg_assistant_2",
            "model": {"id": "fake-model", "providerID": "fake"}
        },
        "durable": {"seq": 2}
    }));
    fake.push_event(json!({
        "id": "evt_text", "type": "session.text.ended",
        "created": 1_700_000_002_100_i64,
        "data": {"sessionID": "ses_fake", "assistantMessageID": "msg_assistant_2", "ordinal": 0, "text": "next"},
        "durable": {"seq": 3}
    }));
    fake.push_event(json!({
        "id": "evt_step", "type": "session.step.ended",
        "created": 1_700_000_002_200_i64,
        "data": {"sessionID": "ses_fake", "assistantMessageID": "msg_assistant_2", "cost": 0.0, "tokens": {"input": 1, "output": 1}},
        "durable": {"seq": 4}
    }));
    fake.push_event(json!({
        "id": "evt_end", "type": "session.execution.succeeded",
        "created": 1_700_000_002_300_i64,
        "data": {"sessionID": "ses_fake"}, "durable": {"seq": 5}
    }));
    fake.push_event(json!({
        "id": "evt_foreign", "type": "session.inbox.enqueued",
        "created": 1_700_000_002_400_i64,
        "data": {
            "inboxID": "foreign", "sessionID": "ses_other",
            "item": {"type": "user", "payload": {"text": "foreign"}}
        },
        "durable": {"seq": 100}
    }));
    let second = wait_for_text(&host, &handle, &first.cursor, "next");
    let next = second
        .entries
        .iter()
        .find(|entry| entry.text.as_deref() == Some("next"))
        .expect("live pass missed the assistant message");
    assert_eq!(
        next.model.as_deref(),
        Some("fake-model"),
        "the live assistant lost the step's model"
    );
    assert!(
        !second
            .entries
            .iter()
            .any(|entry| entry.text.as_deref() == Some("foreign")),
        "another session leaked onto this surface: {:?}",
        second.entries
    );
    assert!(second.cursor.last_native_id != first.cursor.last_native_id);
    assert!(
        fake.seen_paths().iter().any(|path| path == "/api/event"),
        "the host did not subscribe to /api/event: {:?}",
        fake.seen_paths()
    );
    assert_eq!(
        fake.seen_paths()
            .iter()
            .filter(|path| *path == "/api/experimental/session/ses_fake/export")
            .count(),
        1,
        "the second pass backfilled again instead of using its cursor"
    );

    // A wrong token is rejected with HTTP 401; health reports it instead of pretending.
    write_service_state(&workdir, &fake.url(), "wrong-password");
    let health = retry_transient(|| host.health(&handle)).expect("health with wrong token");
    assert!(!health.ok);
    assert!(health.detail.contains("401"), "{}", health.detail);

    // A non-loopback service URL is refused before any request is sent.
    write_service_state(&workdir, "http://0.0.0.0:9", PASSWORD);
    let health = retry_transient(|| host.health(&handle)).expect("health with public URL");
    assert!(!health.ok);
    assert!(health.detail.contains("127.0.0.1"), "{}", health.detail);
}

#[test]
fn opencode_health_retries_an_empty_first_response() {
    let tmp = tempdir().expect("temp dir");
    let fake = ScriptedServer::start(|stream, attempt| {
        if attempt == 0 {
            // Stall past the client's three-second read timeout without writing a byte,
            // matching the macOS CI failure where the first health read saw no response.
            thread::sleep(Duration::from_millis(3_500));
            drop(stream);
        } else {
            let mut stream = stream;
            respond(&mut stream, 401, "application/json", "{\"error\":\"unauthorized\"}");
        }
    });
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    write_service_state(&workdir, &fake.url(), PASSWORD);
    let host = OpencodeHost::default();
    let handle = handle_for(&workdir);

    let health = retry_transient(|| host.health(&handle)).expect("health must not error on an empty response");

    assert!(!health.ok, "a 401 must be unhealthy: {}", health.detail);
    assert!(health.detail.contains("401"), "{}", health.detail);
    assert_eq!(
        fake.accepted(),
        2,
        "health did not retry the empty response exactly once"
    );
}

#[test]
fn opencode_health_retries_a_short_first_response() {
    let tmp = tempdir().expect("temp dir");
    let fake = ScriptedServer::start(|mut stream, attempt| {
        if attempt == 0 {
            // A first read that returns only part of the status line must not become a
            // permanent "answered without HTTP headers" health failure.
            let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Le");
        } else {
            respond(&mut stream, 401, "application/json", "{\"error\":\"unauthorized\"}");
        }
    });
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    write_service_state(&workdir, &fake.url(), PASSWORD);
    let host = OpencodeHost::default();
    let handle = handle_for(&workdir);

    let health = retry_transient(|| host.health(&handle)).expect("health must not error on a short response");

    assert!(!health.ok, "a 401 must be unhealthy: {}", health.detail);
    assert!(health.detail.contains("401"), "{}", health.detail);
    assert_eq!(
        fake.accepted(),
        2,
        "health did not retry the short response exactly once"
    );
}

#[test]
fn opencode_health_reports_a_headerless_server() {
    let tmp = tempdir().expect("temp dir");
    // Every connection closes without a byte of response. A genuine protocol failure must
    // stay visible after the one retry instead of becoming a silent success.
    let fake = ScriptedServer::start(|stream, _| drop(stream));
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    write_service_state(&workdir, &fake.url(), PASSWORD);
    let host = OpencodeHost::default();
    let handle = handle_for(&workdir);

    let health = retry_transient(|| host.health(&handle)).expect("health");

    assert!(!health.ok, "a headerless server must not look healthy");
    assert!(
        health.detail.contains("without HTTP headers"),
        "the protocol failure lost its detail: {}",
        health.detail
    );
    assert_eq!(fake.accepted(), 2, "a GET must be retried exactly once");
}

#[test]
fn opencode_refuses_non_v2_service() {
    let tmp = tempdir().expect("temp dir");
    let fake = FakeOpencode::start();
    let script = tmp.path().join("opencode");
    write_script(&script, &fake_opencode_script(fake.port(), "1.9.0"));
    let host = OpencodeHost::new(&script).with_startup(Duration::from_secs(5));
    let request = host_request(&tmp, Some(fake.port()));
    std::fs::create_dir_all(&request.workdir).expect("orchestrator dir");

    let error = retry_transient(|| host.start(&request)).expect_err("a v1 service must be refused");
    assert!(
        matches!(error, CoreError::Refused(ref message) if message.contains("2.x")),
        "{error:?}"
    );
    assert!(!request.workdir.join("host.json").exists());
}

#[test]
fn claude_remote_refuses_without_login() {
    let tmp = tempdir().expect("temp dir");
    let config = tmp.path().join("claude-config");
    std::fs::create_dir_all(&config).expect("claude config dir");
    let script = tmp.path().join("claude");
    write_script(
        &script,
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"loggedIn": false, "authMethod": "none"}'
  exit 0
fi
exit 1
"#,
    );
    // Claude creates .claude.json on a fresh home even when logged out; it is not a login.
    std::fs::write(config.join(".claude.json"), b"").expect("non-login config file");
    let host = ClaudeRemoteHost::new(&script).with_config_dir(&config);
    assert!(
        !host.login_present(),
        "a logged-out .claude.json must not count as a login"
    );
    let request = host_request(&tmp, None);

    let error = retry_transient(|| host.start(&request)).expect_err("start without login must refuse");
    assert!(
        matches!(error, CoreError::Refused(ref message) if message == NO_LOGIN),
        "{error:?}"
    );
    let error = host
        .join_instructions(&handle_for(&request.workdir))
        .expect_err("join must refuse");
    assert!(
        matches!(error, CoreError::Refused(ref message) if message == NO_LOGIN),
        "{error:?}"
    );
    let health = host.health(&handle_for(&request.workdir)).expect("health");
    assert!(!health.ok);
    assert_eq!(health.detail, NO_LOGIN);
}

#[test]
fn claude_remote_starts_with_fake_login() {
    let tmp = tempdir().expect("temp dir");
    let config = tmp.path().join("claude-config");
    std::fs::create_dir_all(&config).expect("claude config dir");
    let script = tmp.path().join("claude");
    write_script(
        &script,
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"loggedIn": true, "authMethod": "oauth_token"}'
  exit 0
fi
exec sleep 60
"#,
    );
    let host = ClaudeRemoteHost::new(&script)
        .with_config_dir(&config)
        .with_spawn_check(Duration::from_millis(300));
    assert!(login_present(&host));
    let request = host_request(&tmp, None);
    std::fs::create_dir_all(&request.workdir).expect("orchestrator dir");

    let handle = start_logged_in(&host, &request).expect("start remote control");
    assert_eq!(handle.kind, OrchestratorApp::ClaudeCode);
    assert!(process_alive(handle.pid));
    let health = retry_transient(|| host.health(&handle)).expect("health");
    assert!(health.ok, "{}", health.detail);
    let join = host.join_instructions(&handle).expect("join");
    assert!(join.desktop.contains("claude.ai/code"), "{}", join.desktop);
    assert!(join.phone.contains("Claude mobile"), "{}", join.phone);
    assert!(join.chat.contains("vowp chat"), "{}", join.chat);

    retry_transient(|| host.stop(&handle)).expect("stop remote control");
    assert!(!process_alive(handle.pid));
    assert!(!request.workdir.join("host.json").exists());
}

#[test]
fn claude_remote_refuses_api_key_login() {
    let tmp = tempdir().expect("temp dir");
    let config = tmp.path().join("claude-config");
    std::fs::create_dir_all(&config).expect("claude config dir");
    let script = tmp.path().join("claude");
    write_script(
        &script,
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{"loggedIn": true, "authMethod": "api_key"}'
  exit 0
fi
exit 1
"#,
    );
    let host = ClaudeRemoteHost::new(&script).with_config_dir(&config);
    assert!(!host.login_present(), "an API key is not a claude.ai login");
    let request = host_request(&tmp, None);
    let error = retry_transient(|| host.start(&request)).expect_err("API-key login must refuse");
    assert!(
        matches!(error, CoreError::Refused(ref message) if message == NO_LOGIN),
        "{error:?}"
    );
}

#[test]
fn claude_transcript_is_read_incrementally() {
    let tmp = tempdir().expect("temp dir");
    let config = tmp.path().join("claude-config");
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    let project = config
        .join("projects")
        .join(vowp_core::session::importer::mangle_cwd(&workdir));
    std::fs::create_dir_all(&project).expect("claude project dir");
    let transcript = project.join("11111111-1111-1111-1111-111111111111.jsonl");
    let user = json!({
        "type": "user",
        "uuid": "u1",
        "sessionId": "claude-session",
        "timestamp": "2026-11-10T10:00:00Z",
        "message": {"role": "user", "content": "hello"}
    });
    let assistant = json!({
        "type": "assistant",
        "uuid": "a1",
        "sessionId": "claude-session",
        "timestamp": "2026-11-10T10:00:01Z",
        "message": {
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
            ],
            "usage": {"input_tokens": 4, "output_tokens": 2}
        }
    });
    std::fs::write(&transcript, format!("{user}\n{assistant}\n")).expect("transcript");

    let host = ClaudeRemoteHost::new("/bin/true").with_config_dir(&config);
    let handle = handle_for(&workdir);
    let first = retry_transient(|| host.transcript_since(&handle, &empty_cursor())).expect("first read");
    assert_eq!(first.entries.len(), 4, "{:?}", first.entries);
    assert_eq!(
        first.entries[0].kind,
        vowp_core::surface::native::TranscriptKind::UserMessage
    );
    assert_eq!(
        first.entries[1].kind,
        vowp_core::surface::native::TranscriptKind::AssistantMessage
    );
    assert_eq!(
        first.entries[2].kind,
        vowp_core::surface::native::TranscriptKind::ToolCall
    );
    assert_eq!(first.entries[3].kind, vowp_core::surface::native::TranscriptKind::Cost);
    assert_eq!(first.entries[0].native_session.as_deref(), Some("claude-session"));

    let mut line = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("append transcript");
    writeln!(
        line,
        "{}",
        json!({
            "type": "user",
            "uuid": "u2",
            "sessionId": "claude-session",
            "timestamp": "2026-11-10T10:00:02Z",
            "message": {"role": "user", "content": "again"}
        })
    )
    .expect("append line");
    drop(line);
    let second = retry_transient(|| host.transcript_since(&handle, &first.cursor)).expect("incremental read");
    assert_eq!(second.entries.len(), 1, "{:?}", second.entries);
    assert_eq!(second.entries[0].text.as_deref(), Some("again"));
}

#[test]
fn app_host_fake_never_opens_a_socket() {
    struct FileHost {
        port: u16,
    }

    impl AppHost for FileHost {
        fn kind(&self) -> OrchestratorApp {
            OrchestratorApp::Opencode
        }

        fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError> {
            std::fs::create_dir_all(&req.workdir).map_err(|source| CoreError::Io {
                path: req.workdir.display().to_string(),
                source,
            })?;
            std::fs::write(req.workdir.join("fake-started"), b"yes").expect("fake start marker");
            Ok(AppHandle {
                kind: OrchestratorApp::Opencode,
                pid: 1,
                port: Some(self.port),
                started_at: ts("2026-11-10T10:00:00Z"),
                workdir: req.workdir.clone(),
            })
        }

        fn stop(&self, handle: &AppHandle) -> Result<(), CoreError> {
            std::fs::remove_file(handle.workdir.join("fake-started")).map_err(|source| CoreError::Io {
                path: handle.workdir.display().to_string(),
                source,
            })
        }

        fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError> {
            Ok(HostHealth {
                ok: handle.workdir.join("fake-started").is_file(),
                detail: "file-backed fake".to_owned(),
            })
        }

        fn join_instructions(&self, _handle: &AppHandle) -> Result<JoinInstructions, CoreError> {
            Ok(JoinInstructions {
                desktop: "fake desktop".to_owned(),
                phone: "fake phone".to_owned(),
                chat: "fake chat".to_owned(),
            })
        }

        fn transcript_since(&self, _handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
            let entries = vec![TranscriptEntry {
                native_id: "fake-1".to_owned(),
                kind: vowp_core::surface::native::TranscriptKind::UserMessage,
                ts: ts("2026-11-10T10:00:00Z"),
                text: Some("hello from a file".to_owned()),
                tool: None,
                input: None,
                output: None,
                path: None,
                call_id: None,
                model: None,
                native_session: None,
                ok: None,
                tokens: None,
                usd: None,
            }];
            Ok(TranscriptBatch {
                entries,
                cursor: cursor.clone(),
            })
        }
    }

    // The file-backed fake never binds its port. A parallel test binary can steal an
    // ephemeral port between the probe and the check, so retry the whole sequence with a
    // fresh port instead of failing on the first race.
    const ATTEMPTS: usize = 20;
    let mut accepted = false;
    for _ in 0..ATTEMPTS {
        let port = free_port();
        let tmp = tempdir().expect("temp dir");
        let host = FileHost { port };
        let request = host_request(&tmp, Some(port));
        let handle = host.start(&request).expect("fake start");
        assert_eq!(handle.port, Some(port), "the fake reported a different port");
        assert!(host.health(&handle).expect("fake health").ok);
        assert_eq!(host.join_instructions(&handle).expect("join").desktop, "fake desktop");
        let batch = host
            .transcript_since(&handle, &empty_cursor())
            .expect("fake transcript");
        assert_eq!(batch.entries.len(), 1);

        let stolen = port_accepts_connections(port);
        host.stop(&handle).expect("fake stop");
        assert!(!host.health(&handle).expect("fake health after stop").ok);
        if !stolen {
            accepted = true;
            break;
        }
    }
    assert!(
        accepted,
        "every loopback port was claimed after start ({ATTEMPTS} attempts); the fake may be listening"
    );
}
