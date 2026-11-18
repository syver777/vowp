//! Hermetic tests for the opencode bridge.
//!
//! A fake SSE server binds `127.0.0.1:0`, the bridge state and the service config live in
//! a `tempfile` directory, and every fixture is recorded JSON. No real `~/.vowp-agent`,
//! opencode binary, network or home store is touched.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vowp_agent::{
    BRIDGE_EVENTS_FILE, BridgeFrame, BridgeNotice, BridgeSync, MIRROR_FILE, OpencodeBridge, PathRoot, Redaction,
};
use vowp_protocol::agent::surface::{SurfaceEvent, SurfaceTurn, SurfaceTurnState};
use vowp_protocol::{EventBody, JobId, ProjectName, SessionId, SurfaceId, UserMessage};

const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NATIVE_SESSION: &str = "ses_opencode_1";
const PASSWORD: &str = "local-test-service-password";

fn surface() -> SurfaceId {
    SurfaceId(SURFACE.to_owned())
}

fn session() -> SessionId {
    SessionId::parse(SESSION).expect("session id")
}

fn app_event(seq: u64, kind: &str, data: Value) -> Value {
    json!({
        "id": format!("evt_{seq}"),
        "created": 1_700_000_000_000_i64 + i64::try_from(seq).expect("small seq"),
        "type": kind,
        "data": data,
        "durable": { "seq": seq },
    })
}

/// A fake opencode event stream. Each accepted connection is answered with the recorded
/// events, optionally starting after the cursor named by `?since=`/`Last-Event-ID`.
struct FakeSse {
    port: u16,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    honor_since: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestRecord {
    line: String,
    headers: Vec<(String, String)>,
}

impl FakeSse {
    fn start(events: Vec<Value>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake SSE");
        listener.set_nonblocking(true).expect("nonblocking listener");
        let port = listener.local_addr().expect("fake SSE address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let honor_since = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_honor = Arc::clone(&honor_since);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve_connection(stream, &events, thread_honor.load(Ordering::SeqCst), &thread_requests);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            requests,
            honor_since,
            stop,
            handle: Some(handle),
        }
    }

    fn set_honor_since(&self, honor: bool) {
        self.honor_since.store(honor, Ordering::SeqCst);
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for FakeSse {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(mut stream: TcpStream, events: &[Value], honor_since: bool, requests: &Mutex<Vec<RequestRecord>>) {
    // macOS/BSD accept() inherits O_NONBLOCK from the nonblocking listener, and a read
    // timeout does not make a nonblocking stream block.
    stream.set_nonblocking(false).expect("blocking client stream");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return;
            }
            Err(_) => return,
        }
        if raw.len() > 64 * 1024 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = head.lines();
    let line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RequestRecord {
            line: line.clone(),
            headers: headers.clone(),
        });

    let start = if honor_since {
        cursor_of(&line, &headers)
            .and_then(|cursor| events.iter().position(|event| event_seq(event) == Some(cursor)))
            .map(|position| position + 1)
            .unwrap_or(0)
    } else {
        0
    };
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }
    for event in &events[start..] {
        let line = format!("data: {event}\n\n");
        if stream.write_all(line.as_bytes()).is_err() {
            return;
        }
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

fn cursor_of(line: &str, headers: &[(String, String)]) -> Option<u64> {
    if let Some(position) = line.find("since=") {
        let value = line[position + "since=".len()..]
            .split(|character: char| !character.is_ascii_digit())
            .next()
            .unwrap_or_default();
        if let Ok(cursor) = value.parse::<u64>() {
            return Some(cursor);
        }
    }
    headers
        .iter()
        .find(|(name, _)| name == "last-event-id")
        .and_then(|(_, value)| value.strip_prefix("seq:"))
        .and_then(|value| value.parse::<u64>().ok())
}

fn event_seq(event: &Value) -> Option<u64> {
    event.pointer("/durable/seq").and_then(Value::as_u64)
}

fn write_service(dir: &Path, port: u16, password: &str) -> PathBuf {
    let path = dir.join("service.json");
    let text = json!({
        "version": "2.0.28",
        "url": format!("http://127.0.0.1:{port}"),
        "password": password,
    })
    .to_string();
    fs::write(&path, text).expect("write service.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("service mode");
    path
}

fn load_bridge(state_dir: &Path, service: &Path) -> OpencodeBridge {
    OpencodeBridge::load(service, state_dir, surface(), session())
        .expect("load bridge")
        .with_native_session(NATIVE_SESSION)
}

fn events_of(sync: &BridgeSync) -> Vec<&SurfaceEvent> {
    sync.frames
        .iter()
        .filter_map(|frame| match frame {
            BridgeFrame::Event { event } => Some(event.as_ref()),
            BridgeFrame::Turn { .. } => None,
        })
        .collect()
}

fn turns_of(sync: &BridgeSync) -> Vec<&SurfaceTurn> {
    sync.frames
        .iter()
        .filter_map(|frame| match frame {
            BridgeFrame::Turn { turn } => Some(turn),
            BridgeFrame::Event { .. } => None,
        })
        .collect()
}

fn assistant_texts(sync: &BridgeSync) -> Vec<String> {
    events_of(sync)
        .into_iter()
        .filter_map(|event| match &event.event {
            EventBody::AssistantMessage(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect()
}

fn user_texts(sync: &BridgeSync) -> Vec<String> {
    events_of(sync)
        .into_iter()
        .filter_map(|event| match &event.event {
            EventBody::UserMessage(message) => Some(message.text.clone()),
            _ => None,
        })
        .collect()
}

fn spool_text(bridge: &OpencodeBridge) -> String {
    fs::read_to_string(bridge.spool_dir().join(BRIDGE_EVENTS_FILE)).expect("read bridge spool")
}

fn mirror_value(bridge: &OpencodeBridge) -> Value {
    let text = fs::read_to_string(bridge.mirror_path()).expect("read mirror.json");
    serde_json::from_str(&text).expect("mirror json")
}

fn recorded_message() -> Vec<Value> {
    vec![
        app_event(
            1,
            "session.execution.started",
            json!({ "sessionID": NATIVE_SESSION, "modelID": "model-a" }),
        ),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_1",
                "item": { "id": "item_1", "type": "user", "payload": { "text": "please inspect the project" } },
            }),
        ),
        app_event(
            3,
            "session.text.ended",
            json!({ "sessionID": NATIVE_SESSION, "assistantMessageID": "msg_1", "ordinal": 1, "text": "first part" }),
        ),
        app_event(
            4,
            "session.text.ended",
            json!({ "sessionID": NATIVE_SESSION, "assistantMessageID": "msg_1", "ordinal": 2, "text": "second part" }),
        ),
        app_event(
            5,
            "session.step.ended",
            json!({ "sessionID": NATIVE_SESSION, "assistantMessageID": "msg_1" }),
        ),
        app_event(
            6,
            "session.tool.called",
            json!({
                "sessionID": NATIVE_SESSION,
                "id": "call_1",
                "name": "read",
                "input": { "path": "src/lib.rs" },
            }),
        ),
        app_event(
            7,
            "session.tool.success",
            json!({ "sessionID": NATIVE_SESSION, "id": "call_1", "content": "read 10 lines" }),
        ),
        app_event(
            8,
            "session.usage.updated",
            json!({
                "sessionID": NATIVE_SESSION,
                "tokens": { "input": 100, "output": 20, "cache": { "read": 5, "write": 1 } },
                "cost": 0.02,
            }),
        ),
        app_event(9, "session.execution.succeeded", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            10,
            "session.text.started",
            json!({ "sessionID": NATIVE_SESSION, "assistantMessageID": "msg_2" }),
        ),
        app_event(
            11,
            "session.text.delta",
            json!({ "sessionID": NATIVE_SESSION, "assistantMessageID": "msg_2", "delta": "not sent" }),
        ),
        app_event(12, "server.connected", json!({ "sessionID": NATIVE_SESSION })),
    ]
}

#[test]
fn opencode_bridge_maps_a_completed_message_once() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = FakeSse::start(recorded_message());
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service);

    let sync = bridge.drain().expect("drain recorded stream");
    let turns = turns_of(&sync);
    assert_eq!(turns.len(), 2, "one turn start and one turn end");
    assert_eq!(turns[0].state, SurfaceTurnState::Started);
    assert_eq!(turns[0].native_id, "turn-start:seq:1");
    assert_eq!(turns[1].state, SurfaceTurnState::Ended);
    assert_eq!(turns[1].native_id, "turn-end:seq:9");

    assert_eq!(
        user_texts(&sync),
        vec!["please inspect the project".to_owned()],
        "the user item is mirrored once"
    );
    assert_eq!(
        assistant_texts(&sync),
        vec!["first part\nsecond part".to_owned()],
        "text parts are coalesced into one completed assistant message"
    );
    assert!(
        events_of(&sync).iter().all(|event| !event.native_id.contains("msg_2")),
        "UI-only text deltas are dropped"
    );

    let frames = serde_json::to_string(&sync.frames).expect("frame json");
    assert!(frames.contains("\"type\":\"assistant.message\""));
    assert!(frames.contains("\"type\":\"tool.call\""));
    assert!(frames.contains("\"type\":\"tool.result\""));
    assert!(frames.contains("\"type\":\"cost\""));
    let cost = events_of(&sync)
        .into_iter()
        .find(|event| matches!(&event.event, EventBody::Cost(_)))
        .expect("cost event");
    assert_eq!(cost.native_id, "cost:msg_1");
    assert!(!frames.contains("not sent"), "a text delta must never be mirrored");

    let mode = fs::metadata(bridge.mirror_path())
        .expect("mirror metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "mirror.json must be 0600");
    assert_eq!(bridge.cursor().last_seq, Some(12));
    assert_eq!(mirror_value(&bridge)["cursor"]["last_native_id"], "turn-end:seq:9");
    assert_eq!(spool_text(&bridge).lines().count(), sync.frames.len());
}

#[test]
fn opencode_bridge_records_native_ids_and_skips_duplicates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let events = vec![
        app_event(1, "session.execution.started", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_1",
                "item": { "id": "item_1", "type": "user", "payload": { "text": "hello" } },
            }),
        ),
        app_event(
            3,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_1",
                "item": { "id": "item_1", "type": "user", "payload": { "text": "hello" } },
            }),
        ),
        app_event(4, "session.execution.succeeded", json!({ "sessionID": NATIVE_SESSION })),
    ];
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service);

    let first = bridge.drain().expect("first drain");
    assert_eq!(
        user_texts(&first),
        vec!["hello".to_owned()],
        "a duplicate app inbox id is skipped"
    );
    assert_eq!(
        bridge.cursor().last_seq,
        Some(4),
        "the cursor records the last applied event"
    );
    assert_eq!(bridge.cursor().last_native_id.as_deref(), Some("turn-end:seq:4"));
    assert_eq!(mirror_value(&bridge)["cursor"]["last_event_id"], "seq:4");

    server.set_honor_since(false);
    let replay = bridge.drain().expect("replay from the start");
    assert!(
        replay.frames.is_empty(),
        "a replay at or before the cursor must not emit frames"
    );
    assert_eq!(user_texts(&replay).len(), 0);

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the bridge reconnects once");
    assert!(
        requests[1].line.contains("since=4"),
        "the reconnect names the cursor: {}",
        requests[1].line
    );
    assert!(
        requests[1]
            .headers
            .iter()
            .any(|(name, value)| name == "last-event-id" && value == "seq:4"),
        "the reconnect sends Last-Event-ID"
    );
    assert_eq!(
        spool_text(&bridge).lines().count(),
        first.frames.len(),
        "duplicates never reach the spool"
    );
}

#[test]
fn opencode_bridge_backfills_from_the_cursor() {
    let dir = tempfile::tempdir().expect("temp dir");
    let events = recorded_message();
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);

    let state = dir.path().join("bridge").join(SURFACE);
    fs::create_dir_all(&state).expect("bridge state dir");
    let mirror = json!({
        "surface": SURFACE,
        "session": SESSION,
        "cursor": {
            "last_event_id": "seq:4",
            "last_seq": 4,
            "last_native_id": "assistant:msg_1",
        },
        "updated_at": "2026-11-14T00:00:00.000Z",
    });
    let mirror_path = state.join(MIRROR_FILE);
    fs::write(&mirror_path, mirror.to_string()).expect("write cursor");
    fs::set_permissions(&mirror_path, fs::Permissions::from_mode(0o600)).expect("mirror mode");

    let mut bridge = load_bridge(dir.path(), &service);
    let sync = bridge.drain().expect("backfill");
    assert_eq!(
        user_texts(&sync).len() + assistant_texts(&sync).len(),
        0,
        "events before the cursor are not replayed"
    );
    let events = events_of(&sync);
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.event, EventBody::ToolCall(call) if call.call_id == "call_1")),
        "the event after the cursor is mirrored"
    );
    assert!(
        events.iter().any(|event| matches!(&event.event, EventBody::Cost(_))),
        "the usage event after the cursor is mirrored"
    );
    assert!(sync.notices.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].line.contains("since=4"), "{}", requests[0].line);
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "last-event-id" && value == "seq:4")
    );

    // A gap is reported rather than silently treated as contiguous.
    let gap_dir = tempfile::tempdir().expect("gap dir");
    let gap_events = vec![app_event(
        6,
        "session.tool.called",
        json!({ "sessionID": NATIVE_SESSION, "id": "call_gap", "name": "read", "input": {} }),
    )];
    let gap_server = FakeSse::start(gap_events);
    let gap_service = write_service(gap_dir.path(), gap_server.port, PASSWORD);
    let gap_state = gap_dir.path().join("bridge").join(SURFACE);
    fs::create_dir_all(&gap_state).expect("gap state dir");
    let gap_mirror = json!({
        "surface": SURFACE,
        "session": SESSION,
        "cursor": { "last_event_id": "seq:4", "last_seq": 4, "last_native_id": "assistant:msg_1" },
        "updated_at": "2026-11-14T00:00:00.000Z",
    });
    let gap_path = gap_state.join(MIRROR_FILE);
    fs::write(&gap_path, gap_mirror.to_string()).expect("gap cursor");
    fs::set_permissions(&gap_path, fs::Permissions::from_mode(0o600)).expect("gap mode");
    let mut gap_bridge = load_bridge(gap_dir.path(), &gap_service);
    let gap = gap_bridge.drain().expect("gap drain");
    assert_eq!(
        gap.notices,
        vec![BridgeNotice::Gap { after: 4, next: 6 }],
        "a sequence hole is explicit"
    );
    assert_eq!(gap_bridge.state().last_gap, Some((4, 6)));
}

#[test]
fn opencode_bridge_normalizes_absolute_paths_to_path_refs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let project = ProjectName::parse("vowp").expect("project");
    let job = JobId::parse("job-1").expect("job");
    let roots = vec![
        PathRoot::project("/Users/me/src/vowp", &project).expect("project root"),
        PathRoot::worktree("/Users/me/src/vowp/.vowp/worktrees/job-1", &project, &job).expect("worktree root"),
    ];
    let events = vec![
        app_event(1, "session.execution.started", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_paths",
                "item": {
                    "id": "item_paths",
                    "type": "user",
                    "payload": {
                        "text": "read /Users/me/src/vowp/src/lib.rs and /Users/me/src/vowp/.vowp/worktrees/job-1 and /Users/me/src/vowp/.vowp/worktrees/job-1/src/deep.rs and /Users/me/Library/Logs/app.log",
                    },
                },
            }),
        ),
        app_event(
            3,
            "session.tool.called",
            json!({
                "sessionID": NATIVE_SESSION,
                "id": "call_paths",
                "name": "read",
                "input": { "path": "/Users/me/src/vowp/src/main.rs", "other": "/Users/me/tmp/x" },
            }),
        ),
        app_event(4, "session.execution.succeeded", json!({ "sessionID": NATIVE_SESSION })),
    ];
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service).with_path_roots(roots);

    let sync = bridge.drain().expect("drain");
    assert_eq!(
        user_texts(&sync),
        vec!["read subdir:vowp/src/lib.rs and worktree:vowp/job-1 and <external> and <external>".to_owned()]
    );
    let tool_input = events_of(&sync)
        .into_iter()
        .find_map(|event| match &event.event {
            EventBody::ToolCall(call) => Some(call.input.clone()),
            _ => None,
        })
        .expect("tool call");
    assert_eq!(tool_input["path"], "subdir:vowp/src/main.rs");
    assert_eq!(tool_input["other"], "<external>");
    let serialized = serde_json::to_string(&sync.frames).expect("frames json");
    assert!(!serialized.contains("/Users/"), "no absolute path reaches a frame");
    assert!(
        !spool_text(&bridge).contains("/Users/"),
        "no absolute path reaches the spool"
    );
}

#[test]
fn opencode_bridge_redacts_local_injected_values_before_spooling() {
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = "test-only-injected-value-9271";
    let events = vec![
        app_event(1, "session.execution.started", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_secret",
                "item": { "id": "item_secret", "type": "user", "payload": { "text": format!("use {secret}") } },
            }),
        ),
        app_event(
            3,
            "session.tool.called",
            json!({ "sessionID": NATIVE_SESSION, "id": "call_secret", "name": "shell", "input": { "env": secret } }),
        ),
        app_event(
            4,
            "session.tool.success",
            json!({ "sessionID": NATIVE_SESSION, "id": "call_secret", "content": format!("exported {secret}") }),
        ),
        app_event(
            5,
            "session.execution.failed",
            json!({ "sessionID": NATIVE_SESSION, "error": { "message": format!("failed with {secret}") } }),
        ),
    ];
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service)
        .with_redactions(vec![Redaction::new("ANTHROPIC_API_KEY", secret).expect("redaction")]);

    let sync = bridge.drain().expect("drain");
    let serialized = serde_json::to_string(&sync.frames).expect("frames json");
    assert!(!serialized.contains(secret), "a secret value reached a frame");
    assert!(serialized.contains("[REDACTED:ANTHROPIC_API_KEY]"));
    let spool = spool_text(&bridge);
    assert!(!spool.contains(secret), "a secret value reached the spool");
    assert!(spool.contains("[REDACTED:ANTHROPIC_API_KEY]"));
    assert!(!format!("{bridge:?}").contains(secret), "Debug must not print a value");
    assert!(bridge.redactions_loaded());

    bridge.finish();
    assert!(!bridge.redactions_loaded(), "injected values are dropped after the run");
    assert!(
        spool_text(&bridge).contains("[REDACTED:ANTHROPIC_API_KEY]"),
        "the spool stays"
    );
}

#[test]
fn opencode_bridge_sends_an_attachment_with_sha256_and_a_size_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = FakeSse::start(Vec::new());
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service).with_attachment_cap(1024);
    let data = b"attachment bytes with a NUL \x00 and text";
    let source = dir.path().join("notes.txt");
    fs::write(&source, data).expect("write attachment");

    let attachment = bridge.attachment(&source).expect("read attachment");
    let expected = hex::encode(Sha256::digest(data));
    assert_eq!(attachment.sha256(), expected);
    assert_eq!(attachment.bytes(), u64::try_from(data.len()).expect("length"));
    assert_eq!(attachment.data(), data);
    assert_eq!(attachment.blob_ref().sha256, expected);
    let blob = bridge.blob_path(&expected);
    assert_eq!(fs::read(&blob).expect("read blob"), data);
    let mode = fs::metadata(&blob).expect("blob metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "attachment bytes are 0600");

    let frame = bridge
        .queue_attachment("see the note", std::slice::from_ref(&source))
        .expect("queue attachment");
    let BridgeFrame::Event { event } = &frame else {
        panic!("expected an event frame");
    };
    let EventBody::UserMessage(UserMessage { text, attachments, .. }) = &event.event else {
        panic!("expected a user message");
    };
    assert_eq!(text, "see the note");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].sha256, expected);
    assert!(
        !serde_json::to_string(&frame)
            .expect("frame json")
            .contains(&source.to_string_lossy().to_string())
    );
    assert!(!spool_text(&bridge).contains(&source.to_string_lossy().to_string()));
    assert_eq!(bridge.pending_frames().expect("pending").len(), 1);

    let big = dir.path().join("big.bin");
    fs::write(&big, vec![7u8; 32]).expect("write big file");
    let mut capped = load_bridge(dir.path(), &service).with_attachment_cap(8);
    let error = capped.attachment(&big).expect_err("oversize attachment is refused");
    assert!(matches!(error, vowp_agent::AgentError::Config(_)));
    let rejected = hex::encode(Sha256::digest(vec![7u8; 32]));
    assert!(
        !capped.blob_path(&rejected).exists(),
        "a refused attachment is not stored"
    );
}

#[test]
fn opencode_bridge_stops_read_only_on_lease_revocation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let events = vec![
        app_event(1, "session.execution.started", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_before",
                "item": { "id": "item_before", "type": "user", "payload": { "text": "before" } },
            }),
        ),
        app_event(3, "session.execution.succeeded", json!({ "sessionID": NATIVE_SESSION })),
    ];
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service);
    let first = bridge.drain().expect("first drain");
    assert_eq!(first.frames.len(), 3);
    assert_eq!(bridge.pending_frames().expect("pending").len(), 3);

    bridge.note_lease_lost("the phone took the lease");
    let state = bridge.state();
    assert_eq!(state.read_only.as_deref(), Some("the phone took the lease"));
    assert!(!state.attached);
    assert_eq!(state.pending, 3, "a lost lease keeps the spool");

    let after = bridge.drain().expect("read-only drain");
    assert!(after.frames.is_empty(), "a read-only bridge reads nothing");
    assert_eq!(bridge.pending_frames().expect("pending").len(), 3);
    assert!(bridge.spool_dir().join(BRIDGE_EVENTS_FILE).exists());
}

#[test]
fn opencode_bridge_service_config_is_never_logged() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = FakeSse::start(Vec::new());
    let service = write_service(dir.path(), server.port, PASSWORD);
    let bridge = load_bridge(dir.path(), &service);
    let debug = format!("{bridge:?}");

    assert!(!debug.contains(PASSWORD), "Debug printed the service password");
    assert!(
        !debug.contains(&server.port.to_string()),
        "Debug printed the service port"
    );
    assert!(!debug.contains("pair"), "Debug must not mention a pair link");

    let wider = dir.path().join("wider.json");
    fs::copy(&service, &wider).expect("copy service");
    fs::set_permissions(&wider, fs::Permissions::from_mode(0o644)).expect("widen");
    let error = OpencodeBridge::load(&wider, dir.path(), surface(), session()).expect_err("wide config");
    let text = error.to_string();
    assert!(!text.contains(PASSWORD), "an error printed the password: {text}");
    assert!(
        !text.contains(&server.port.to_string()),
        "an error printed the port: {text}"
    );

    let unauthorized = unauthorized_server();
    let unauthorized_service = write_service(dir.path(), unauthorized, PASSWORD);
    let mut bridge = OpencodeBridge::load(&unauthorized_service, dir.path(), surface(), session())
        .expect("load unauthorized bridge");
    let error = bridge.attach().expect_err("401 is a typed error");
    let text = error.to_string();
    assert!(!text.contains(PASSWORD), "an error printed the password: {text}");
    assert!(
        !text.contains(&unauthorized.to_string()),
        "an error printed the port: {text}"
    );
}

fn unauthorized_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind 401 server");
    let port = listener.local_addr().expect("401 address").port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut raw = [0u8; 2048];
            let _ = stream.read(&mut raw);
            let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        }
    });
    port
}

// --- Client integration: one outbound connection, attach, detach, read-only ---

use std::collections::VecDeque;
use std::sync::Condvar;
use std::sync::mpsc;

use vowp_agent::{
    AgentClient, AgentConfig, AgentError, FrameRead, FrameStream, NodeKey, RunOptions, RunOutcome, SystemAgentClock,
    Transport,
};
use vowp_protocol::{AGENT_PROTOCOL, AgentFrame, AgentRole, NodeName, Timestamp};

struct LiveSse {
    port: u16,
    commands: mpsc::Sender<SseCommand>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

enum SseCommand {
    Events(Vec<Value>),
    Close,
}

impl LiveSse {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind live SSE");
        listener.set_nonblocking(true).expect("live listener");
        let port = listener.local_addr().expect("live address").port();
        let (commands, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_live(stream, &receiver),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            commands,
            stop,
            handle: Some(handle),
        }
    }

    fn send(&self, events: Vec<Value>) {
        let _ = self.commands.send(SseCommand::Events(events));
    }
}

impl Drop for LiveSse {
    fn drop(&mut self) {
        let _ = self.commands.send(SseCommand::Close);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_live(mut stream: TcpStream, commands: &mpsc::Receiver<SseCommand>) {
    // macOS/BSD accept() inherits O_NONBLOCK from the nonblocking listener.
    stream.set_nonblocking(false).expect("blocking live client stream");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(_) => return,
        }
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }
    loop {
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(SseCommand::Events(events)) => {
                for event in events {
                    let line = format!("data: {event}\n\n");
                    if stream.write_all(line.as_bytes()).is_err() {
                        return;
                    }
                }
                let _ = stream.flush();
            }
            Ok(SseCommand::Close) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Default)]
struct HomeState {
    received: Vec<AgentFrame>,
    current: Option<Arc<HomeConnection>>,
    connections: usize,
}

struct HomeConnection {
    frames: Mutex<VecDeque<AgentFrame>>,
    changed: Condvar,
    closed: AtomicBool,
}

impl HomeConnection {
    fn push(&self, frame: AgentFrame) {
        self.frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(frame);
        self.changed.notify_all();
    }

    fn recv(&self, timeout: Option<Duration>) -> FrameRead {
        let deadline = timeout.map(|duration| std::time::Instant::now() + duration);
        let mut frames = self.frames.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(frame) = frames.pop_front() {
                return FrameRead::Frame(frame);
            }
            if self.closed.load(Ordering::SeqCst) {
                return FrameRead::Closed;
            }
            match deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return FrameRead::Timeout;
                    }
                    let (guard, _) = self
                        .changed
                        .wait_timeout(frames, deadline - now)
                        .unwrap_or_else(|poison| poison.into_inner());
                    frames = guard;
                }
                None => {
                    frames = self.changed.wait(frames).unwrap_or_else(|poison| poison.into_inner());
                }
            }
        }
    }
}

#[derive(Clone, Default)]
struct FakeHome {
    shared: Arc<Mutex<HomeState>>,
}

impl FakeHome {
    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn push(&self, frame: AgentFrame) {
        let current = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .clone();
        if let Some(connection) = current {
            connection.push(frame);
        }
    }

    fn lease_revoked(&self, reason: &str) {
        self.push(AgentFrame::LeaseRevoked {
            reason: reason.to_owned(),
            holder: "phone".to_owned(),
        });
    }

    fn revoke(&self) {
        self.push(AgentFrame::Revoked {
            reason: "the test revoked the node".to_owned(),
        });
    }
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let (number, connection) = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connections += 1;
            let connection = Arc::new(HomeConnection {
                frames: Mutex::new(VecDeque::new()),
                changed: Condvar::new(),
                closed: AtomicBool::new(false),
            });
            state.current = Some(Arc::clone(&connection));
            (state.connections, connection)
        };
        Ok(Box::new(HomeStream {
            shared: Arc::clone(&self.shared),
            connection,
            number,
        }))
    }
}

struct HomeStream {
    shared: Arc<Mutex<HomeState>>,
    connection: Arc<HomeConnection>,
    number: usize,
}

impl FrameStream for HomeStream {
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        Ok(self.connection.recv(timeout))
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { .. } => {
                self.connection.push(AgentFrame::HomeChallenge {
                    nonce: format!("nonce-{}", self.number),
                    home_id: "home-1".to_owned(),
                    connection_id: format!("connection-{}", self.number),
                    server_time: Timestamp::from_unix_secs(1_700_000_000),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                self.connection.push(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node"),
                    connection_id: format!("connection-{}", self.number),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

fn wait_for<F>(home: &FakeHome, predicate: F)
where
    F: Fn(&[AgentFrame]) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let frames = home.frames();
        if predicate(&frames) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for a home frame"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn user_text(frame: &AgentFrame) -> Option<String> {
    let AgentFrame::SurfaceEvent(event) = frame else {
        return None;
    };
    match &event.event {
        EventBody::UserMessage(message) => Some(message.text.clone()),
        _ => None,
    }
}

fn attachment_refs(frame: &AgentFrame) -> Option<Vec<vowp_protocol::BlobRef>> {
    let AgentFrame::SurfaceEvent(event) = frame else {
        return None;
    };
    match &event.event {
        EventBody::UserMessage(message) if !message.attachments.is_empty() => Some(message.attachments.clone()),
        _ => None,
    }
}

#[test]
fn opencode_bridge_attaches_and_detaches_on_the_one_connection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let live = LiveSse::start();
    let service = write_service(dir.path(), live.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service);
    let attachment_source = dir.path().join("note.txt");
    fs::write(&attachment_source, b"attached bytes before connect").expect("attachment");
    let attachment_sha = hex::encode(Sha256::digest(b"attached bytes before connect"));
    bridge
        .queue_attachment("attachment before connect", std::slice::from_ref(&attachment_source))
        .expect("queue attachment");
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node"),
        dir.path().to_path_buf(),
        "home.example",
    );
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir.path()).expect("node key");
    let home = FakeHome::default();
    let mut client = AgentClient::new(
        dir.path().to_path_buf(),
        key,
        config,
        Box::new(home.clone()),
        Arc::new(SystemAgentClock),
    )
    .with_bridge(bridge);
    let run = thread::spawn(move || client.run(&RunOptions::new(AgentRole::Both)));

    wait_for(&home, |frames| {
        frames.iter().any(|frame| matches!(frame, AgentFrame::SurfaceAttach(_)))
    });
    wait_for(&home, |frames| {
        frames.iter().any(|frame| attachment_refs(frame).is_some())
    });
    let frames = home.frames();
    let refs = frames
        .iter()
        .find_map(attachment_refs)
        .expect("the queued attachment was sent after attach");
    assert_eq!(refs[0].sha256, attachment_sha);
    assert!(
        frames.iter().all(|frame| !serde_json::to_string(frame)
            .expect("frame json")
            .contains(&attachment_source.to_string_lossy().to_string())),
        "no local attachment path is sent"
    );
    thread::sleep(Duration::from_millis(50));
    live.send(vec![
        app_event(1, "session.execution.started", json!({ "sessionID": NATIVE_SESSION })),
        app_event(
            2,
            "session.inbox.enqueued",
            json!({
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_before",
                "item": { "id": "item_before", "type": "user", "payload": { "text": "before the lease" } },
            }),
        ),
        app_event(3, "session.execution.succeeded", json!({ "sessionID": NATIVE_SESSION })),
    ]);
    wait_for(&home, |frames| {
        frames
            .iter()
            .any(|frame| user_text(frame).as_deref() == Some("before the lease"))
    });

    home.lease_revoked("the phone took the lease");
    thread::sleep(Duration::from_millis(200));
    live.send(vec![app_event(
        4,
        "session.inbox.enqueued",
        json!({
            "sessionID": NATIVE_SESSION,
            "inboxID": "inbox_after",
            "item": { "id": "item_after", "type": "user", "payload": { "text": "after the lease" } },
        }),
    )]);
    thread::sleep(Duration::from_millis(300));
    let frames = home.frames();
    assert!(
        frames
            .iter()
            .all(|frame| user_text(frame).as_deref() != Some("after the lease")),
        "a read-only bridge must not append after the lease is lost"
    );
    let spool = dir
        .path()
        .join("bridge")
        .join(SURFACE)
        .join("spool")
        .join(BRIDGE_EVENTS_FILE);
    let spool_text = fs::read_to_string(&spool).expect("the bridge spool is kept");
    assert!(spool_text.contains("before the lease"));

    home.revoke();
    let outcome = run.join().expect("run thread").expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");

    let frames = home.frames();
    let attach = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceAttach(_)))
        .expect("attach");
    let before = frames
        .iter()
        .position(|frame| user_text(frame).as_deref() == Some("before the lease"))
        .expect("event");
    let detach = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceDetach(_)))
        .expect("detach");
    assert!(attach < before, "attach precedes the mirrored event");
    assert!(before < detach, "detach follows the flushed spool");
}

#[test]
fn opencode_bridge_replays_without_sequence_numbers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let events = vec![
        json!({ "id": "evt_start", "created": 1_700_000_000_000_i64, "type": "session.execution.started", "data": { "sessionID": NATIVE_SESSION } }),
        json!({
            "id": "evt_user",
            "created": 1_700_000_000_100_i64,
            "type": "session.inbox.enqueued",
            "data": {
                "sessionID": NATIVE_SESSION,
                "inboxID": "inbox_no_seq",
                "item": { "id": "item_no_seq", "type": "user", "payload": { "text": "no sequence" } },
            },
        }),
        json!({ "id": "evt_end", "created": 1_700_000_000_200_i64, "type": "session.execution.succeeded", "data": { "sessionID": NATIVE_SESSION } }),
    ];
    let server = FakeSse::start(events);
    let service = write_service(dir.path(), server.port, PASSWORD);
    let mut bridge = load_bridge(dir.path(), &service);
    let first = bridge.drain().expect("first drain");
    assert_eq!(first.frames.len(), 3);
    assert_eq!(bridge.cursor().last_seq, None);
    assert_eq!(bridge.cursor().last_event_id.as_deref(), Some("id:evt_end"));
    let mirror = mirror_value(&bridge);
    assert!(mirror["recent_native"].as_array().is_some_and(|ids| !ids.is_empty()));

    let mut fresh = load_bridge(dir.path(), &service);
    let replay = fresh.drain().expect("replay");
    assert!(
        replay.frames.is_empty(),
        "the persisted native ids suppress a no-sequence replay"
    );
}
