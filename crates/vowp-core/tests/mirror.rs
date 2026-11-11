//! Transcript mirror and resume-context tests. Hermetic: a temporary node home, a fake
//! clock, a file-backed fake host and a loopback HTTP fake for the opencode API.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5 and the transcript mirror tests in §15.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
mod support;

use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::importer::{import_file, scan_file};
use vowp_core::session::service::SessionService;
use vowp_core::surface::mirror::{
    MirrorCursor, mirror_from_host, mirror_once, read_cursor, resume_context, write_cursor,
};
use vowp_core::surface::native::{
    AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, TranscriptEntry, TranscriptKind,
};
use vowp_core::surface::new_lease_token;
use vowp_core::surface::opencode_host::{
    OpencodeHost, entries_from_api_event, entries_from_api_events, parse_message_list,
};
use vowp_core::{AdapterRegistry, Deps};
use vowp_protocol::{
    Actor, AssistantMessage, CAPSULE_VERSION, Capsule, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger,
    EventBody, EventSource, EventType, Executor, ExecutorRef, FakeClock, HarnessKind, Lease, MemoryLog, NewSession,
    NodeName, OrchestratorApp, SessionId, SessionRole, SurfaceKind, Timestamp, TurnEnded, TurnStarted, UserMessage,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, Store};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-10T12:00:00Z").expect("test timestamp")
}

fn local() -> NodeName {
    NodeName::parse("local").expect("node name")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, clock: &FakeClock) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: local(),
        clock: Arc::new(clock.clone()),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(0)),
    }
}

fn create_session(deps: &Deps, tmp: &TempDir) -> SessionId {
    SessionService::new(deps.clone())
        .create_session(NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Orchestrator,
            title: "mirror".to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: None,
        })
        .expect("create session")
}

fn append(deps: &Deps, store: &Store, session: &SessionId, body: EventBody) {
    let log = SessionLog::open(&deps.paths, session, deps.clock.clone()).expect("open log");
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(local()),
                native_session_id: None,
            },
            body,
            raw: None,
            ts: None,
        })
        .expect("append");
    store.index_event(&index_row(&appended)).expect("index");
    log.sync().expect("sync");
}

fn event_count(deps: &Deps, session: &SessionId) -> usize {
    SessionLog::open_read(&deps.paths, session)
        .expect("read log")
        .read_all()
        .expect("read all")
        .len()
}

fn events_of(deps: &Deps, session: &SessionId) -> Vec<vowp_protocol::EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("read log")
        .read_all()
        .expect("read all")
}

fn empty_cursor() -> MirrorCursor {
    MirrorCursor {
        last_native_id: None,
        updated_at: ts(),
    }
}

fn entry(native_id: &str, kind: TranscriptKind, text: Option<&str>) -> TranscriptEntry {
    TranscriptEntry {
        native_id: native_id.to_owned(),
        kind,
        ts: ts(),
        text: text.map(str::to_owned),
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: Some("fake-model".to_owned()),
        native_session: Some("ses_app".to_owned()),
        ok: None,
        tokens: None,
        usd: None,
    }
}

/// A file-free fake app host: tests hand it transcript entries.
struct FakeHost {
    kind: OrchestratorApp,
    entries: Mutex<Vec<TranscriptEntry>>,
}

impl FakeHost {
    fn new(kind: OrchestratorApp, entries: Vec<TranscriptEntry>) -> Self {
        Self {
            kind,
            entries: Mutex::new(entries),
        }
    }

    fn push(&self, entry: TranscriptEntry) {
        self.entries.lock().expect("entries").push(entry);
    }
}

impl AppHost for FakeHost {
    fn kind(&self) -> OrchestratorApp {
        self.kind
    }

    fn start(&self, req: &HostRequest) -> Result<AppHandle, vowp_core::CoreError> {
        Ok(AppHandle {
            kind: self.kind,
            pid: 1,
            port: None,
            started_at: ts(),
            workdir: req.workdir.clone(),
        })
    }

    fn stop(&self, _handle: &AppHandle) -> Result<(), vowp_core::CoreError> {
        Ok(())
    }

    fn health(&self, _handle: &AppHandle) -> Result<HostHealth, vowp_core::CoreError> {
        Ok(HostHealth {
            ok: true,
            detail: "fake".to_owned(),
        })
    }

    fn join_instructions(&self, _handle: &AppHandle) -> Result<vowp_protocol::JoinInstructions, vowp_core::CoreError> {
        Ok(vowp_protocol::JoinInstructions {
            desktop: "desktop".to_owned(),
            phone: "phone".to_owned(),
            chat: "chat".to_owned(),
        })
    }

    fn transcript_since(
        &self,
        _handle: &AppHandle,
        cursor: &MirrorCursor,
    ) -> Result<TranscriptBatch, vowp_core::CoreError> {
        let entries = self.entries.lock().expect("entries").clone();
        let entries = match cursor.last_native_id.as_deref() {
            None => entries,
            Some(last) => match entries.iter().position(|entry| entry.native_id == last) {
                Some(position) => entries.into_iter().skip(position + 1).collect(),
                None => entries,
            },
        };
        let last = entries
            .last()
            .map(|entry| entry.native_id.clone())
            .or_else(|| cursor.last_native_id.clone());
        Ok(TranscriptBatch {
            entries,
            cursor: MirrorCursor {
                last_native_id: last,
                updated_at: ts(),
            },
        })
    }
}

fn handle(kind: OrchestratorApp, workdir: &Path) -> AppHandle {
    AppHandle {
        kind,
        pid: 1,
        port: None,
        started_at: ts(),
        workdir: workdir.to_path_buf(),
    }
}

#[test]
fn opencode_mirror_maps_messages_to_events() {
    let tmp = tempdir().expect("temp dir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let store = deps.open_store().expect("store");
    let session = create_session(&deps, &tmp);
    // The real v2.0.28 export: user text is top-level, assistant work is in `content`, and
    // cost/tokens sit at message level.
    let export = json!({
        "info": {"id": "ses_app"},
        "messages": [
            {
                "id": "m1", "type": "user", "text": "hello",
                "time": {"created": 1_700_000_000_000_i64}
            },
            {
                "id": "m2", "type": "assistant",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "tool_use", "id": "call-1", "name": "read", "input": {"path": "a.rs"}}
                ],
                "model": {"id": "fake-model"},
                "cost": 0.25,
                "tokens": {"input": 3, "output": 4, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "time": {"created": 1_700_000_001_000_i64}
            },
            {
                "id": "m3", "type": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": "done"}],
                "time": {"created": 1_700_000_002_000_i64}
            },
            {"id": "idle1", "type": "idle", "time": {"created": 1_700_000_003_000_i64}}
        ]
    });
    let entries = parse_message_list(&export);
    assert_eq!(entries.len(), 5, "{entries:?}");
    assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
    assert_eq!(entries[0].text.as_deref(), Some("hello"));
    assert_eq!(entries[1].kind, TranscriptKind::AssistantMessage);
    assert_eq!(entries[1].text.as_deref(), Some("hi"));
    assert_eq!(entries[2].kind, TranscriptKind::ToolCall);
    assert_eq!(entries[3].kind, TranscriptKind::Cost);
    assert_eq!(entries[3].usd, Some(0.25));
    let tokens = entries[3].tokens.expect("tokens");
    assert_eq!(entries[4].kind, TranscriptKind::ToolResult);
    assert_eq!((tokens.input, tokens.output), (3, 4));

    let host = FakeHost::new(OrchestratorApp::Opencode, entries);
    let handle = handle(OrchestratorApp::Opencode, tmp.path());
    let mut cursor = empty_cursor();
    let report = mirror_from_host(&deps, &store, &session, &host, &handle, &mut cursor).expect("mirror");
    assert_eq!(report.events, 5, "user, assistant text, tool call, tool result, cost");

    let events = events_of(&deps, &session);
    let bodies: Vec<EventType> = events.iter().map(|event| event.event_type).collect();
    assert!(bodies.contains(&EventType::UserMessage));
    assert!(bodies.contains(&EventType::AssistantMessage));
    assert!(bodies.contains(&EventType::ToolCall));
    assert!(bodies.contains(&EventType::ToolResult));
    assert!(bodies.contains(&EventType::Cost));
    let tool_call = events
        .iter()
        .find(|event| event.event_type == EventType::ToolCall)
        .expect("tool call event");
    let EventBody::ToolCall(call) = tool_call.body().expect("tool call body") else {
        panic!("wrong body");
    };
    assert_eq!(call.call_id, "call-1");
    assert_eq!(call.name, "read");
    assert_eq!(call.input, json!({"path": "a.rs"}));
    let tool_result = events
        .iter()
        .find(|event| event.event_type == EventType::ToolResult)
        .expect("tool result event");
    let EventBody::ToolResult(result) = tool_result.body().expect("tool result body") else {
        panic!("wrong body");
    };
    assert_eq!(result.call_id, "call-1");
    assert!(result.ok);
    assert_eq!(result.output.as_deref(), Some("done"));
    let cost = events
        .iter()
        .find(|event| event.event_type == EventType::Cost)
        .expect("cost event");
    let EventBody::Cost(cost) = cost.body().expect("cost body") else {
        panic!("wrong body");
    };
    assert_eq!(cost.cost.usd, Some(0.25));
    assert_eq!(cost.cost.tokens.input, 3);
    assert_eq!(cost.cost.tokens.output, 4);
    assert!(
        events
            .iter()
            .skip(1)
            .all(|event| event.source.harness == HarnessKind::Opencode)
    );
}

#[test]
fn opencode_export_shape_keeps_user_messages_and_cost() {
    // Trimmed copy of the recorded v2.0.28 `session-export.json`: user text is top-level,
    // assistant content is in `content`, cost/tokens are at message level, and non-content
    // messages (`idle`, `model-switched`) are skipped.
    let export = json!({
        "info": {"id": "ses_e3a86c119ffe1iZkxI5m5oSi1V"},
        "messages": [
            {
                "id": "msg_user_1", "time": {"created": 1794314420736_i64},
                "text": "turn one from the desktop side", "type": "user"
            },
            {
                "id": "msg_assistant_1",
                "time": {"created": 1794314420770_i64, "streamed": 1794314420793_i64, "completed": 1794314420796_i64},
                "type": "assistant", "agent": "build",
                "model": {"id": "fake-model", "providerID": "fake"},
                "content": [{"type": "text", "text": "hello from the fake local model"}],
                "finish": "stop", "rawFinish": "stop",
                "cost": 0,
                "tokens": {"input": 10, "output": 8, "reasoning": 0, "cache": {"read": 0, "write": 0}}
            },
            {"id": "msg_idle_1", "time": {"created": 1794314420800_i64}, "type": "idle", "outcome": "succeeded"},
            {"id": "msg_switch", "time": {"created": 1794314421495_i64}, "type": "model-switched", "model": {"id": "fake-model", "providerID": "fake"}},
            {
                "id": "msg_user_2", "time": {"created": 1794314421521_i64},
                "text": "\"turn two from the phone\"", "files": [], "type": "user"
            },
            {
                "id": "msg_assistant_2",
                "time": {"created": 1794314421546_i64, "streamed": 1794314421567_i64, "completed": 1794314421568_i64},
                "type": "assistant", "agent": "build",
                "model": {"id": "fake-model", "providerID": "fake", "variant": "default"},
                "content": [{"type": "text", "text": "hello from the fake local model"}],
                "finish": "stop", "rawFinish": "stop",
                "cost": 0,
                "tokens": {"input": 10, "output": 8, "reasoning": 0, "cache": {"read": 0, "write": 0}}
            }
        ]
    });
    let entries = parse_message_list(&export);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::UserMessage)
            .count(),
        2,
        "the real export's user messages were dropped: {entries:?}"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::AssistantMessage)
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::Cost)
            .count(),
        2
    );
    assert_eq!(entries[0].text.as_deref(), Some("turn one from the desktop side"));
    let cost = entries
        .iter()
        .find(|entry| entry.kind == TranscriptKind::Cost)
        .expect("cost");
    assert_eq!(cost.usd, Some(0.0));
    let tokens = cost.tokens.expect("tokens");
    assert_eq!((tokens.input, tokens.output), (10, 8));
}

#[test]
fn opencode_message_list_is_sorted_chronologically() {
    let list = json!({
        "data": [
            {"id": "a1", "type": "assistant", "content": [{"type": "text", "text": "later"}], "time": {"created": 2000}},
            {"id": "u1", "type": "user", "text": "earlier", "time": {"created": 1000}}
        ]
    });
    let entries = parse_message_list(&list);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
    assert_eq!(entries[1].kind, TranscriptKind::AssistantMessage);
}

#[test]
fn mirror_reads_api_event_and_backfills_export() {
    let tmp = tempdir().expect("temp dir");
    let server = MiniOpencode::start();
    let workdir = tmp.path().join("orchestrator");
    std::fs::create_dir_all(&workdir).expect("orchestrator dir");
    write_state(&workdir, &server.url());

    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let store = deps.open_store().expect("store");
    let session = create_session(&deps, &tmp);
    let host = OpencodeHost::default();
    let host_handle = AppHandle {
        kind: OrchestratorApp::Opencode,
        pid: 4242,
        port: Some(server.port()),
        started_at: ts(),
        workdir: workdir.clone(),
    };
    let mut cursor = empty_cursor();
    let first = mirror_from_host(&deps, &store, &session, &host, &host_handle, &mut cursor).expect("first mirror");
    assert_eq!(first.events, 3, "user, assistant and cost from the export: {first:?}");
    assert_eq!(cursor.last_native_id.as_deref(), Some("cost:m2"));

    // The first transcript read starts the persistent listener. Wait until it is connected
    // before adding events: `/api/event` is subscription-only and does not replay history.
    server.wait_for_subscriber();
    for event in first_turn_events() {
        server.push_event(event);
    }
    // Another opencode session shares the global stream; none of its content may enter this
    // VOWP session.
    server.push_event(json!({
        "id": "evt_foreign_user", "created": 1_700_000_001_500_i64,
        "type": "session.inbox.enqueued",
        "data": {
            "inboxID": "foreign_user", "sessionID": "ses_other",
            "item": {"type": "user", "payload": {"text": "foreign"}}
        },
        "durable": {"seq": 100}
    }));
    server.push_event(json!({
        "id": "evt_foreign_usage", "created": 1_700_000_001_600_i64,
        "type": "session.usage.updated",
        "data": {"sessionID": "ses_other", "cost": 9.0, "tokens": {"input": 900, "output": 900}}
    }));

    let live = wait_for_kind(&host, &host_handle, &cursor, TranscriptKind::TurnEnded);
    assert!(
        live.iter().any(|entry| entry.kind == TranscriptKind::TurnStarted),
        "the persistent listener missed the turn start: {live:?}"
    );

    let second = mirror_from_host(&deps, &store, &session, &host, &host_handle, &mut cursor).expect("second mirror");
    assert_eq!(second.events, 2, "only the live turn boundaries are new: {second:?}");
    assert_eq!(cursor.last_native_id, second.cursor.last_native_id);
    let events = events_of(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::UserMessage)
            .count(),
        1,
        "the overlapping user message was duplicated"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::AssistantMessage)
            .count(),
        1,
        "the overlapping assistant message was duplicated"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::Cost)
            .count(),
        1,
        "the overlapping cost was duplicated"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.body(), Ok(EventBody::UserMessage(message)) if message.text == "foreign")),
        "another opencode session leaked into this VOWP session"
    );
    assert!(
        server
            .seen()
            .iter()
            .any(|path| path == "/api/experimental/session/ses_fake/export"),
        "backfill did not read the export: {:?}",
        server.seen()
    );
    assert!(
        server.seen().iter().any(|path| path == "/api/event"),
        "the host did not subscribe to /api/event: {:?}",
        server.seen()
    );
    assert!(
        !server.seen().iter().any(|path| path.contains("/event?after=")),
        "the mirror used a route opencode 2.0.28 does not have: {:?}",
        server.seen()
    );

    // A complete turn happens between two passes while the subscription stays open. The
    // listener must spool it; a per-pass one-shot read would miss it.
    server.push_event(json!({
        "id": "evt_u2", "created": 1_700_000_002_000_i64, "type": "session.inbox.enqueued",
        "data": {
            "inboxID": "m4", "sessionID": "ses_fake",
            "item": {"type": "user", "payload": {"text": "again"}}
        },
        "durable": {"seq": 6}
    }));
    server.push_event(json!({
        "id": "evt_t2", "created": 1_700_000_002_100_i64, "type": "session.execution.started",
        "data": {"sessionID": "ses_fake"}, "durable": {"seq": 7}
    }));
    server.push_event(json!({
        "id": "evt_step2", "created": 1_700_000_002_150_i64, "type": "session.step.started",
        "data": {
            "sessionID": "ses_fake", "assistantMessageID": "m5",
            "model": {"id": "fake-model", "providerID": "fake"}
        },
        "durable": {"seq": 8}
    }));
    server.push_event(json!({
        "id": "evt_a2", "created": 1_700_000_002_200_i64, "type": "session.text.ended",
        "data": {"sessionID": "ses_fake", "assistantMessageID": "m5", "ordinal": 0, "text": "later"},
        "durable": {"seq": 9}
    }));
    server.push_event(json!({
        "id": "evt_s2", "created": 1_700_000_002_250_i64, "type": "session.step.ended",
        "data": {
            "sessionID": "ses_fake", "assistantMessageID": "m5", "cost": 0.0,
            "tokens": {"input": 1, "output": 1}
        },
        "durable": {"seq": 9}
    }));
    server.push_event(json!({
        "id": "evt_usage2", "created": 1_700_000_002_300_i64, "type": "session.usage.updated",
        "data": {"sessionID": "ses_fake", "cost": 0.25, "tokens": {"input": 4, "output": 5}}
    }));
    server.push_event(json!({
        "id": "evt_e2", "created": 1_700_000_002_400_i64, "type": "session.execution.succeeded",
        "data": {"sessionID": "ses_fake"}, "durable": {"seq": 11}
    }));

    let pending = wait_for_text(&host, &host_handle, &cursor, "later");
    assert!(
        pending.iter().any(|entry| entry.text.as_deref() == Some("later")),
        "the event that happened between passes was lost"
    );
    let third = mirror_from_host(&deps, &store, &session, &host, &host_handle, &mut cursor).expect("third mirror");
    assert!(third.events >= 4, "{third:?}");
    let events = events_of(&deps, &session);
    let later = events
        .iter()
        .find(|event| matches!(event.body(), Ok(EventBody::AssistantMessage(message)) if message.text == "later"))
        .expect("the between-pass turn was not written to the session log");
    assert_eq!(
        later.source.model.as_deref(),
        Some("fake-model"),
        "the live assistant lost the step's model"
    );
    let later_cost = events
        .iter()
        .find(|event| matches!(event.body(), Ok(EventBody::Cost(cost)) if cost.cost.tokens.input == 1))
        .expect("the live cost was not written");
    assert_eq!(
        later_cost.source.model.as_deref(),
        Some("fake-model"),
        "the live cost lost the step's model"
    );
    assert_eq!(
        server
            .seen()
            .iter()
            .filter(|path| path.as_str() == "/api/experimental/session/ses_fake/export")
            .count(),
        1,
        "a later pass backfilled again instead of using its cursor"
    );
}

/// Returns the first target-session turn as the real `/api/event` stream emits it.
fn first_turn_events() -> Vec<Value> {
    vec![
        json!({
            "id": "evt_u1", "created": 1_700_000_000_000_i64, "type": "session.inbox.enqueued",
            "data": {
                "inboxID": "m1", "sessionID": "ses_fake",
                "item": {"type": "user", "payload": {"text": "hello"}}
            },
            "durable": {"seq": 1}
        }),
        json!({
            "id": "evt_t1", "created": 1_700_000_000_500_i64, "type": "session.execution.started",
            "data": {"sessionID": "ses_fake"}, "durable": {"seq": 2}
        }),
        json!({
            "id": "evt_step1", "created": 1_700_000_000_700_i64, "type": "session.step.started",
            "data": {
                "sessionID": "ses_fake", "assistantMessageID": "m2",
                "model": {"id": "fake-model", "providerID": "fake"}
            },
            "durable": {"seq": 3}
        }),
        json!({
            "id": "evt_a1", "created": 1_700_000_001_000_i64, "type": "session.text.ended",
            "data": {"sessionID": "ses_fake", "assistantMessageID": "m2", "ordinal": 0, "text": "hi"},
            "durable": {"seq": 4}
        }),
        json!({
            "id": "evt_s1", "created": 1_700_000_001_100_i64, "type": "session.step.ended",
            "data": {
                "sessionID": "ses_fake", "assistantMessageID": "m2", "cost": 0.25,
                "tokens": {"input": 3, "output": 4}
            },
            "durable": {"seq": 5}
        }),
        json!({
            "id": "evt_usage1", "created": 1_700_000_001_150_i64, "type": "session.usage.updated",
            "data": {"sessionID": "ses_fake", "cost": 0.25, "tokens": {"input": 3, "output": 4}}
        }),
        json!({
            "id": "evt_e1", "created": 1_700_000_001_200_i64, "type": "session.execution.succeeded",
            "data": {"sessionID": "ses_fake"}, "durable": {"seq": 6}
        }),
    ]
}

/// Polls the transcript until an entry of `kind` appears, or panics after two seconds.
fn wait_for_kind(
    host: &OpencodeHost,
    handle: &AppHandle,
    cursor: &MirrorCursor,
    kind: TranscriptKind,
) -> Vec<TranscriptEntry> {
    for _ in 0..100 {
        let batch = host.transcript_since(handle, cursor).expect("transcript while waiting");
        if batch.entries.iter().any(|entry| entry.kind == kind) {
            return batch.entries;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {kind:?}");
}

/// Polls the transcript until an entry with `text` appears, or panics after two seconds.
fn wait_for_text(host: &OpencodeHost, handle: &AppHandle, cursor: &MirrorCursor, text: &str) -> Vec<TranscriptEntry> {
    for _ in 0..100 {
        let batch = host.transcript_since(handle, cursor).expect("transcript while waiting");
        if batch.entries.iter().any(|entry| entry.text.as_deref() == Some(text)) {
            return batch.entries;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {text:?}");
}

#[test]
fn opencode_api_event_mapping_covers_tools_cost_and_errors() {
    let user = json!({
        "id": "evt_user",
        "type": "session.inbox.enqueued",
        "created": 1_700_000_000_000_i64,
        "data": {
            "inboxID": "item_1", "sessionID": "ses_app", "seq": 1,
            "item": {"id": "item_1", "type": "user", "payload": {"text": "go"}}
        },
        "durable": {"seq": 1}
    });
    let entries = entries_from_api_event(&user);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
    assert_eq!(entries[0].text.as_deref(), Some("go"));
    assert_eq!(entries[0].native_id, "user:item_1");

    let tool = json!({
        "id": "evt_tool",
        "type": "session.tool.called",
        "created": 1_700_000_001_000_i64,
        "data": {"sessionID": "ses_app", "seq": 2, "id": "call_1", "name": "read", "input": {"path": "a"}}
    });
    let entries = entries_from_api_event(&tool);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, TranscriptKind::ToolCall);
    assert_eq!(entries[0].native_id, "tool-call:call_1");
    assert_eq!(entries[0].call_id.as_deref(), Some("call_1"));

    let failed = json!({
        "id": "evt_tool_failed",
        "type": "session.tool.failed",
        "created": 1_700_000_002_000_i64,
        "data": {"sessionID": "ses_app", "seq": 3, "id": "call_1", "error": {"message": "boom"}}
    });
    let entries = entries_from_api_event(&failed);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, TranscriptKind::ToolResult);
    assert_eq!(entries[0].native_id, "tool-result:call_1");
    assert_eq!(entries[0].ok, Some(false));
    assert_eq!(entries[0].output.as_deref(), Some("boom"));

    // Two text.ended parts of one assistant message coalesce; the step cost is per message.
    let turn = [
        json!({
            "id": "evt_start", "type": "session.execution.started",
            "created": 1_700_000_003_000_i64,
            "data": {"sessionID": "ses_app"}, "durable": {"seq": 3}
        }),
        json!({
            "id": "evt_step0", "type": "session.step.started",
            "created": 1_700_000_003_050_i64,
            "data": {
                "sessionID": "ses_app", "assistantMessageID": "a1",
                "model": {"id": "fake-model", "providerID": "fake"}
            },
            "durable": {"seq": 4}
        }),
        json!({
            "id": "evt_text0", "type": "session.text.ended",
            "created": 1_700_000_003_100_i64,
            "data": {"sessionID": "ses_app", "assistantMessageID": "a1", "ordinal": 0, "text": "part one"},
            "durable": {"seq": 5}
        }),
        json!({
            "id": "evt_text1", "type": "session.text.ended",
            "created": 1_700_000_003_200_i64,
            "data": {"sessionID": "ses_app", "assistantMessageID": "a1", "ordinal": 1, "text": "part two"},
            "durable": {"seq": 6}
        }),
        json!({
            "id": "evt_step", "type": "session.step.ended",
            "created": 1_700_000_003_300_i64,
            "data": {
                "sessionID": "ses_app", "assistantMessageID": "a1", "cost": 0.5,
                "tokens": {"input": 7, "output": 2}
            },
            "durable": {"seq": 7}
        }),
        json!({
            "id": "evt_usage", "type": "session.usage.updated",
            "created": 1_700_000_003_350_i64,
            "data": {
                "sessionID": "ses_app", "cost": 0.5,
                "tokens": {"input": 7, "output": 2}
            }
        }),
        json!({
            "id": "evt_done", "type": "session.execution.succeeded",
            "created": 1_700_000_003_400_i64,
            "data": {"sessionID": "ses_app"}, "durable": {"seq": 8}
        }),
    ];
    let entries = entries_from_api_events(&turn);
    let messages: Vec<&TranscriptEntry> = entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::AssistantMessage)
        .collect();
    assert_eq!(messages.len(), 1, "{entries:?}");
    assert_eq!(messages[0].native_id, "assistant:a1");
    assert_eq!(messages[0].text.as_deref(), Some("part one\npart two"));
    assert_eq!(messages[0].model.as_deref(), Some("fake-model"));
    let costs: Vec<&TranscriptEntry> = entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::Cost)
        .collect();
    assert_eq!(costs.len(), 1, "{entries:?}");
    assert_eq!(costs[0].native_id, "cost:a1");
    assert_eq!(costs[0].usd, Some(0.5));
    assert_eq!(costs[0].model.as_deref(), Some("fake-model"));
    assert_eq!(costs[0].tokens.expect("tokens").input, 7);
    assert!(entries.iter().any(|entry| entry.kind == TranscriptKind::TurnStarted));
    assert!(entries.iter().any(|entry| entry.kind == TranscriptKind::TurnEnded));

    // A usage snapshot is cumulative; two snapshots produce two delta costs when no step
    // cost exists.
    let usage = [
        json!({
            "id": "evt_usage1", "type": "session.usage.updated",
            "created": 1_700_000_004_000_i64,
            "data": {"sessionID": "ses_app", "cost": 0.1, "tokens": {"input": 10, "output": 8}}
        }),
        json!({
            "id": "evt_usage2", "type": "session.usage.updated",
            "created": 1_700_000_004_100_i64,
            "data": {"sessionID": "ses_app", "cost": 0.3, "tokens": {"input": 20, "output": 16}}
        }),
    ];
    let entries = entries_from_api_events(&usage);
    let costs: Vec<&TranscriptEntry> = entries
        .iter()
        .filter(|entry| entry.kind == TranscriptKind::Cost)
        .collect();
    assert_eq!(costs.len(), 2, "{entries:?}");
    assert_eq!(costs[1].tokens.expect("tokens").input, 10);
    assert_eq!(costs[1].tokens.expect("tokens").output, 8);
    let usd = costs[1].usd.expect("usd");
    assert!((usd - 0.2).abs() < 1e-9, "{usd}");

    let execution = json!({
        "id": "evt_failed",
        "type": "session.execution.failed",
        "created": 1_700_000_005_000_i64,
        "data": {"sessionID": "ses_app", "seq": 9, "error": {"message": "model failed"}},
        "durable": {"seq": 9}
    });
    let entries = entries_from_api_event(&execution);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].kind, TranscriptKind::Error);
    assert_eq!(entries[1].kind, TranscriptKind::TurnEnded);
    assert_eq!(entries[1].text.as_deref(), Some("failed"));

    let streaming = json!({
        "type": "session.text.delta",
        "created": 1_700_000_006_000_i64,
        "data": {"sessionID": "ses_app", "text": "partial"}
    });
    assert!(entries_from_api_event(&streaming).is_empty());

    let selected = json!({
        "type": "session.model.selected",
        "created": 1_700_000_007_000_i64,
        "data": {"sessionID": "ses_app", "seq": 10, "model": {"id": "other-model"}},
        "durable": {"seq": 10}
    });
    let entries = entries_from_api_event(&selected);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, TranscriptKind::Note);
    assert!(entries[0].text.as_deref().unwrap_or_default().contains("other-model"));
}

#[test]
fn mirror_is_incremental_and_idempotent() {
    let tmp = tempdir().expect("temp dir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let store = deps.open_store().expect("store");
    let session = create_session(&deps, &tmp);
    let host = FakeHost::new(
        OrchestratorApp::Opencode,
        vec![
            entry("u1", TranscriptKind::UserMessage, Some("hello")),
            entry("a1", TranscriptKind::AssistantMessage, Some("hi")),
        ],
    );
    let handle = handle(OrchestratorApp::Opencode, tmp.path());
    let mut cursor = empty_cursor();
    let first = mirror_from_host(&deps, &store, &session, &host, &handle, &mut cursor).expect("first");
    assert_eq!(first.events, 2);
    assert_eq!(first.duplicates, 0);
    assert_eq!(cursor.last_native_id.as_deref(), Some("a1"));
    let after_first = event_count(&deps, &session);
    assert_eq!(after_first, 3, "session.created plus two entries");

    // A caller that lost its cursor re-reads the same entries; the native map skips them.
    // The caller's cursor is still advanced so the next save is not stale.
    let mut fresh = empty_cursor();
    let second = mirror_from_host(&deps, &store, &session, &host, &handle, &mut fresh).expect("second");
    assert_eq!(second.events, 0);
    assert_eq!(second.duplicates, 2);
    assert_eq!(fresh.last_native_id.as_deref(), Some("a1"));
    assert_eq!(event_count(&deps, &session), after_first);
    assert_eq!(
        store
            .native_event(&session, HarnessKind::Opencode, "a1")
            .expect("native event"),
        Some(events_of(&deps, &session)[2].id.clone())
    );

    host.push(entry("a2", TranscriptKind::AssistantMessage, Some("again")));
    let mut third_cursor = first.cursor.clone();
    let third = mirror_from_host(&deps, &store, &session, &host, &handle, &mut third_cursor).expect("third");
    assert_eq!(third.events, 1);
    assert_eq!(event_count(&deps, &session), after_first + 1);
    assert_eq!(third.cursor.last_native_id.as_deref(), Some("a2"));
    assert_eq!(third_cursor.last_native_id.as_deref(), Some("a2"));

    // A chat surface has no app transcript, but the spec's `mirror_once` still updates the
    // caller's cursor so a caller can save it unconditionally.
    let chat_handle = AppHandle {
        kind: OrchestratorApp::Chat,
        pid: 1,
        port: None,
        started_at: ts(),
        workdir: tmp.path().to_path_buf(),
    };
    let mut chat_cursor = empty_cursor();
    let report = mirror_once(&deps, &store, &session, &chat_handle, &mut chat_cursor).expect("chat mirror");
    assert_eq!(chat_cursor.last_native_id, report.cursor.last_native_id);
}

#[test]
fn mirror_cursor_roundtrips_through_mirror_json() {
    let tmp = tempdir().expect("temp dir");
    let path = tmp.path().join("orchestrator").join("mirror.json");
    let missing = read_cursor(&path).expect("missing cursor");
    assert!(missing.last_native_id.is_none());
    let cursor = MirrorCursor {
        last_native_id: Some("assistant:m1".to_owned()),
        updated_at: ts(),
    };
    write_cursor(&path, &cursor).expect("write cursor");
    assert_eq!(read_cursor(&path).expect("read cursor"), cursor);
    let mode = std::fs::metadata(&path).expect("cursor metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn claude_importer_maps_transcript() {
    let tmp = tempdir().expect("temp dir");
    let transcript = tmp.path().join("session.jsonl");
    let lines = [
        json!({
            "type": "user", "uuid": "u1", "sessionId": "claude-session",
            "timestamp": "2026-11-10T10:00:00Z",
            "message": {"role": "user", "content": "hello"}
        }),
        json!({
            "type": "assistant", "uuid": "a1", "sessionId": "claude-session",
            "timestamp": "2026-11-10T10:00:01Z",
            "message": {
                "role": "assistant", "model": "claude-sonnet-5",
                "content": [
                    {"type": "thinking", "thinking": "considering"},
                    {"type": "text", "text": "hi"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}
                ],
                "usage": {"input_tokens": 4, "output_tokens": 2}
            }
        }),
        json!({
            "type": "user", "uuid": "u2", "sessionId": "claude-session",
            "timestamp": "2026-11-10T10:00:02Z",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
            ]}
        }),
        json!({"type": "summary", "summary": "not session content"}),
        json!("not an object"),
    ];
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&transcript, &body).expect("write transcript");

    let entries = scan_file(&transcript).expect("scan");
    assert_eq!(entries.len(), 6, "{entries:?}");
    assert_eq!(entries[0].kind, TranscriptKind::UserMessage);
    assert_eq!(entries[0].text.as_deref(), Some("hello"));
    assert_eq!(entries[1].kind, TranscriptKind::Reasoning);
    assert_eq!(entries[2].kind, TranscriptKind::AssistantMessage);
    assert_eq!(entries[3].kind, TranscriptKind::ToolCall);
    assert_eq!(entries[3].call_id.as_deref(), Some("toolu_1"));
    assert_eq!(entries[3].tool.as_deref(), Some("Bash"));
    assert_eq!(entries[4].kind, TranscriptKind::Cost);
    assert_eq!(entries[4].tokens.expect("tokens").input, 4);
    assert_eq!(entries[5].kind, TranscriptKind::ToolResult);
    assert_eq!(entries[5].call_id.as_deref(), Some("toolu_1"));
    assert_eq!(entries[5].output.as_deref(), Some("ok"));
    assert!(
        entries
            .iter()
            .all(|entry| entry.native_session.as_deref() == Some("claude-session"))
    );

    // The offset reader sees only complete new lines and is read-only.
    let mut cursor = vowp_core::session::importer::ImportCursor::default();
    let first = import_file(&transcript, &mut cursor).expect("first import");
    assert_eq!(first.entries.len(), 6);
    let second = import_file(&transcript, &mut cursor).expect("second import");
    assert!(second.entries.is_empty());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript")
        .write_all(format!("{}\n", lines[0]).as_bytes())
        .expect("append transcript");
    let third = import_file(&transcript, &mut cursor).expect("third import");
    assert_eq!(third.entries.len(), 1);
    assert_eq!(third.entries[0].text.as_deref(), Some("hello"));
}

#[test]
fn resume_context_builds_capsule_and_tail() {
    let tmp = tempdir().expect("temp dir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let store = deps.open_store().expect("store");
    let session = create_session(&deps, &tmp);
    append(&deps, &store, &session, EventBody::TurnStarted(TurnStarted { turn: 1 }));
    append(
        &deps,
        &store,
        &session,
        EventBody::UserMessage(UserMessage {
            text: "one".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    );
    append(
        &deps,
        &store,
        &session,
        EventBody::AssistantMessage(AssistantMessage {
            text: "one answer".to_owned(),
        }),
    );
    append(
        &deps,
        &store,
        &session,
        EventBody::TurnEnded(TurnEnded {
            turn: 1,
            stop_reason: vowp_protocol::StopReason::EndTurn,
            duration_ms: None,
        }),
    );
    append(&deps, &store, &session, EventBody::TurnStarted(TurnStarted { turn: 2 }));
    append(
        &deps,
        &store,
        &session,
        EventBody::UserMessage(UserMessage {
            text: "two".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    );
    append(
        &deps,
        &store,
        &session,
        EventBody::AssistantMessage(AssistantMessage {
            text: "two answer".to_owned(),
        }),
    );
    append(
        &deps,
        &store,
        &session,
        EventBody::TurnEnded(TurnEnded {
            turn: 2,
            stop_reason: vowp_protocol::StopReason::EndTurn,
            duration_ms: None,
        }),
    );

    let capsule = Capsule {
        version: CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: ts(),
        at_seq: 5,
        trigger: CapsuleTrigger::Manual,
        from: ExecutorRef {
            executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
            node: None,
        },
        intent: "continue the orchestrator work".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry::default(),
    };
    store.insert_capsule(&capsule, "capsules/test.json").expect("capsule");
    let token = new_lease_token();
    store
        .take_lease(&Lease {
            session_id: session.clone(),
            surface_id: None,
            surface_kind: SurfaceKind::Mirror,
            node: local(),
            holder: "test".to_owned(),
            token: token.clone(),
            acquired_at: ts(),
            heartbeat_at: ts(),
        })
        .expect("lease");

    let context = resume_context(&deps, &store, &session, 1).expect("resume context");
    assert_eq!(context.session.id, session);
    assert_eq!(
        context.capsule.as_ref().map(|capsule| capsule.capsule_id.clone()),
        Some(capsule.capsule_id)
    );
    assert_eq!(context.lease.as_ref().map(|lease| lease.token.clone()), Some(token));
    assert_eq!(context.tail.len(), 4, "{:?}", context.tail);
    assert_eq!(context.tail[0].event_type, EventType::TurnStarted);
    let EventBody::TurnStarted(start) = context.tail[0].body().expect("turn body") else {
        panic!("wrong first event");
    };
    assert_eq!(start.turn, 2);
    assert!(
        context.tail.iter().any(
            |event| matches!(event.body(), Ok(EventBody::AssistantMessage(message)) if message.text == "two answer")
        )
    );
    assert!(
        !context.tail.iter().any(
            |event| matches!(event.body(), Ok(EventBody::AssistantMessage(message)) if message.text == "one answer")
        )
    );

    let none = resume_context(&deps, &store, &session, 0).expect("zero turns");
    assert!(none.tail.is_empty());
    let all = resume_context(&deps, &store, &session, 10).expect("all turns");
    assert!(all.tail.len() >= 8, "{:?}", all.tail);

    // A lease whose heartbeat is older than the stale window is not handed to the next app.
    let stale = Timestamp::parse("2026-11-10T11:50:00Z").expect("stale timestamp");
    store
        .take_lease(&Lease {
            session_id: session.clone(),
            surface_id: None,
            surface_kind: SurfaceKind::Mirror,
            node: local(),
            holder: "stale".to_owned(),
            token: new_lease_token(),
            acquired_at: stale,
            heartbeat_at: stale,
        })
        .expect("stale lease");
    let context = resume_context(&deps, &store, &session, 1).expect("stale resume context");
    assert!(context.lease.is_none(), "a stale lease was handed to the next app");
}

/// A loopback opencode fake for the mirror tests.
///
/// `/api/event` is a real subscription: a connection receives only events added after it
/// opened, and stays open. That is the v2.0.28 contract; the host must hold the connection
/// between mirror passes to avoid losing an event.
struct MiniOpencode {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<Value>>>,
    subscribers: Arc<Mutex<usize>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MiniOpencode {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mini opencode");
        listener.set_nonblocking(true).expect("nonblocking mini opencode");
        let addr = listener.local_addr().expect("mini address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let messages = Arc::new(Mutex::new(vec![
            json!({
                "id": "m1", "type": "user", "text": "hello",
                "time": {"created": 1_700_000_000_000_i64}
            }),
            json!({
                "id": "m2", "type": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "model": {"id": "fake-model"},
                "cost": 0.25,
                "tokens": {"input": 3, "output": 4},
                "time": {"created": 1_700_000_001_000_i64}
            }),
        ]));
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscribers = Arc::new(Mutex::new(0usize));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let messages = Arc::clone(&messages);
            let events = Arc::clone(&events);
            let subscribers = Arc::clone(&subscribers);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let seen = Arc::clone(&seen);
                            let messages = Arc::clone(&messages);
                            let events = Arc::clone(&events);
                            let subscribers = Arc::clone(&subscribers);
                            let shutdown = Arc::clone(&shutdown);
                            thread::spawn(move || {
                                mini_handle(stream, &seen, &messages, &events, &subscribers, &shutdown);
                            });
                        }
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
            subscribers,
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

    /// Adds one event to the live hub; connected subscribers receive it.
    fn push_event(&self, event: Value) {
        self.events.lock().expect("events").push(event);
    }

    /// Waits until the host listener has subscribed to `/api/event`.
    fn wait_for_subscriber(&self) {
        for _ in 0..200 {
            if *self.subscribers.lock().expect("subscribers") > 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no /api/event subscriber connected");
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen").clone()
    }
}

impl Drop for MiniOpencode {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn mini_handle(
    mut stream: TcpStream,
    seen: &Arc<Mutex<Vec<String>>>,
    messages: &Arc<Mutex<Vec<Value>>>,
    events: &Arc<Mutex<Vec<Value>>>,
    subscribers: &Arc<Mutex<usize>>,
    shutdown: &Arc<AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 2048];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    seen.lock().expect("seen").push(path.clone());
    let route = path.split('?').next().unwrap_or(&path);
    if route == "/api/event" {
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
        if stream.write_all(headers.as_bytes()).is_err() {
            return;
        }
        let mut cursor = events.lock().expect("events").len();
        *subscribers.lock().expect("subscribers") += 1;
        while !shutdown.load(Ordering::Relaxed) {
            let pending: Vec<Value> = {
                let events = events.lock().expect("events");
                let pending = events[cursor..].to_vec();
                cursor = events.len();
                pending
            };
            for event in pending {
                let frame = format!("data: {event}\n\n");
                if stream.write_all(frame.as_bytes()).is_err() {
                    *subscribers.lock().expect("subscribers") -= 1;
                    return;
                }
            }
            if stream.flush().is_err() {
                *subscribers.lock().expect("subscribers") -= 1;
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        *subscribers.lock().expect("subscribers") -= 1;
        return;
    }
    let (status, content_type, body) = match route {
        "/api/session" => (
            200,
            "application/json",
            json!({"data": [{"id": "ses_fake"}]}).to_string(),
        ),
        "/api/session/ses_fake/message" => (
            200,
            "application/json",
            json!({"data": messages.lock().expect("messages").clone()}).to_string(),
        ),
        "/api/experimental/session/ses_fake/export" => (
            200,
            "application/json",
            json!({
                "info": {"id": "ses_fake"},
                "messages": messages.lock().expect("messages").clone()
            })
            .to_string(),
        ),
        _ => (404, "application/json", "{\"error\":\"not found\"}".to_owned()),
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn write_state(workdir: &Path, url: &str) {
    let state = workdir.join(".opencode").join("state").join("opencode");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("service.json"),
        json!({"id": "fake", "version": "2.0.28", "url": url, "pid": 4242, "password": "password"}).to_string(),
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
            "started_at": "2026-11-10T12:00:00Z",
            "workdir": workdir.display().to_string(),
            "service_json": state.join("service.json").display().to_string(),
            "native_session_id": "ses_fake",
            "vowp_session": "ses_vowp"
        })
        .to_string(),
    )
    .expect("host.json");
}
