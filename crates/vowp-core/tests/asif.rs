//! ASIF export/import facade tests: determinism, redaction, strict losses,
//! staged-log checks, reindex and round-trip fidelity.
//! Hermetic: temp node, fake clock, fake adapter; no network, real HOME or tmux.
//! Spec: `docs/design/v0.4/asif-integration.md` §7.1.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::asif::{
    AsifExportOptions, AsifImportOptions, MAX_ASIF_ARCHIVE_BYTES, export_asif, import_asif,
};
use vowp_core::session::capsule::CapsuleWriter;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    Actor, AssistantMessage, CapsuleEvent, CapsuleTrigger, ErrorKind, EventBody, EventEnvelope, EventSource, EventType,
    Executor, FakeClock, HarnessKind, MemoryLog, NewSession, NodeName, SessionId, SessionRole, SessionState, Timestamp,
    ToolCall, ToolResult, UserMessage,
};
use vowp_store::blobs::BlobStore;
use vowp_store::session_files::{list_capsule_files, read_capsule_file};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, SessionFilter, Store};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T12:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, registry: AdapterRegistry) -> Deps {
    deps_at(tmp, "node", registry)
}

fn deps_at(tmp: &TempDir, root: &str, registry: AdapterRegistry) -> Deps {
    let paths = NodePaths::new(tmp.path().join(root));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: NodeName::parse("local").expect("node name"),
        clock: Arc::new(FakeClock::at(ts())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(registry),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

fn draft_json(intent: &str) -> String {
    json!({
        "intent": intent,
        "constraints": [],
        "decisions": [],
        "state": {"done": [], "in_progress": ["import"], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

fn deps_with_capsule(tmp: &TempDir, intent: &str) -> Deps {
    deps(
        tmp,
        AdapterRegistry::new().with(
            "opencode",
            Arc::new(FakeAdapter::from_bodies(
                HarnessKind::Opencode,
                ts(),
                vec![EventBody::AssistantMessage(AssistantMessage {
                    text: draft_json(intent),
                })],
            )),
        ),
    )
}

fn create_session(service: &SessionService, tmp: &TempDir, title: &str) -> SessionId {
    service
        .create_session(NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: title.to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: Some(opencode()),
        })
        .expect("create session")
}

fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::Opencode,
        model: Some("deepseek/deepseek-flash".to_owned()),
        node: Some(NodeName::parse("local").expect("node")),
        native_session_id: None,
    }
}

fn append(log: &SessionLog, actor: Actor, body: EventBody) -> EventEnvelope {
    log.append(NewEvent {
        actor,
        source: source(),
        body,
        raw: None,
        ts: None,
    })
    .expect("append")
    .envelope
}

fn append_user(log: &SessionLog, text: &str) {
    append(
        log,
        Actor::User,
        EventBody::UserMessage(UserMessage {
            text: text.to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
    );
}

fn append_assistant(log: &SessionLog, text: &str) {
    append(
        log,
        Actor::Assistant,
        EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() }),
    );
}

fn export_default(paths: &NodePaths, source: &SessionId, out: &Path) {
    export_asif(
        paths,
        source,
        out,
        &AsifExportOptions {
            strict: true,
            ..AsifExportOptions::default()
        },
    )
    .expect("export");
}

fn import_default(paths: &NodePaths, store: &Store, package: &Path, cwd: &Path) -> SessionId {
    import_asif(paths, store, package, &AsifImportOptions::new(cwd))
        .expect("import")
        .session
        .id
}

fn session_count(store: &Store) -> usize {
    store.list_sessions(&SessionFilter::default()).expect("sessions").len()
}

fn log_events(paths: &NodePaths, session: &SessionId) -> Vec<EventEnvelope> {
    SessionLog::open_read(paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

fn text_events(events: &[EventEnvelope]) -> Vec<(EventType, String)> {
    events
        .iter()
        .filter_map(|event| match event.body().ok()? {
            EventBody::UserMessage(message) => Some((EventType::UserMessage, message.text)),
            EventBody::AssistantMessage(message) => Some((EventType::AssistantMessage, message.text)),
            _ => None,
        })
        .collect()
}

fn event_type_sequence(events: &[EventEnvelope]) -> Vec<EventType> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type,
                EventType::UserMessage | EventType::AssistantMessage | EventType::ToolCall | EventType::ToolResult
            )
        })
        .map(|event| event.event_type)
        .collect()
}

fn blob_ref(events: &[EventEnvelope]) -> String {
    events
        .iter()
        .find_map(|event| match event.body().ok()? {
            EventBody::ToolResult(result) => result.output_ref.map(|blob| blob.sha256),
            _ => None,
        })
        .expect("tool result blob ref")
}

/// A minimal valid ASIF document used as an external import fixture.
fn minimal_document() -> Value {
    json!({
        "asif_version": "0.4",
        "capture": {
            "id": "asif-test-capture",
            "producer": {"name": "vowp-core-asif-test", "version": "1"},
            "consistency": "consistent",
            "boundary": "Authored synthetic fixture for the core ASIF facade."
        },
        "session": {
            "id": "asif-test-session",
            "title": "Synthetic ASIF import",
            "objective": [{"kind": "text", "text": "Exercise the core ASIF import facade."}],
            "native_ids": [],
            "lineage": []
        },
        "participants": [
            {"id": "asif-test-human", "kind": "human"},
            {"id": "asif-test-agent", "kind": "agent"}
        ],
        "events": [
            {
                "id": "asif-test-event-1",
                "sequence": 0,
                "actor_id": "asif-test-human",
                "kind": "message",
                "causes": [],
                "provenance": {
                    "mode": "synthetic",
                    "producer": "vowp-core-asif-test",
                    "method": "authored conformance fixture",
                    "inputs": []
                },
                "data": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello from asif"}]
                }
            },
            {
                "id": "asif-test-event-2",
                "sequence": 1,
                "actor_id": "asif-test-agent",
                "kind": "message",
                "causes": [{"event_id": "asif-test-event-1"}],
                "provenance": {
                    "mode": "synthetic",
                    "producer": "vowp-core-asif-test",
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
            "event_ids": ["asif-test-event-1", "asif-test-event-2"],
            "head_event_id": "asif-test-event-2"
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
            "at_event_id": "asif-test-event-2",
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

/// An external fixture whose second event is an orphan tool result.
fn orphan_tool_result_document() -> Value {
    let mut document = minimal_document();
    document["events"][1] = json!({
        "id": "asif-test-event-2",
        "sequence": 1,
        "actor_id": "asif-test-agent",
        "kind": "tool_result",
        "causes": [],
        "provenance": {
            "mode": "synthetic",
            "producer": "vowp-core-asif-test",
            "method": "authored recoverable-integrity fixture",
            "inputs": []
        },
        "data": {
            "call_id": "call-without-a-call",
            "result_index": 0,
            "terminal": true,
            "outcome": "success",
            "parts": [{"kind": "text", "text": "orphan output"}]
        }
    });
    document
}

fn write_package(dir: &Path, name: &str, document: &Value) -> PathBuf {
    let bytes = serde_json::to_vec_pretty(document).expect("document JSON");
    let package = vowp_asif::Package::new(bytes);
    let archive = vowp_asif::pack(&package).expect("pack fixture");
    let path = dir.join(name);
    fs::write(&path, archive).expect("write fixture package");
    path
}

fn unpack_document(path: &Path) -> Value {
    let package = vowp_asif::unpack(path).expect("unpack package");
    serde_json::from_slice(&package.session).expect("session.json")
}

#[test]
fn asif_export_writes_a_deterministic_package_and_summary() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "deterministic export");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, "same input produces the same package");
        append_assistant(&log, "deterministic output");
        log.sync().expect("sync");
    }
    let first = tmp.path().join("first.asif.zip");
    let second = tmp.path().join("second.asif.zip");
    let one = export_asif(&deps.paths, &session, &first, &AsifExportOptions::default()).expect("first export");
    let two = export_asif(&deps.paths, &session, &second, &AsifExportOptions::default()).expect("second export");

    assert_eq!(
        fs::read(&first).expect("first bytes"),
        fs::read(&second).expect("second bytes")
    );
    assert_eq!(one.bytes, fs::metadata(&first).expect("first metadata").len());
    assert_eq!(one.report, two.report);
    assert_eq!(one.report.source_session.as_deref(), Some(session.as_str()));
    assert_eq!(
        one.report.counts.events_in,
        log_events(&deps.paths, &session).len() as u64
    );
    assert!(!tmp.path().join("first.asif.zip.report.json").exists());
}

#[test]
fn asif_export_redacts_exact_literal_and_never_echoes_it() {
    const SECRET: &str = "test-secret-value-do-not-echo";
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "redacted export");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, &format!("the literal {SECRET} must not survive"));
        log.sync().expect("sync");
    }
    let out = tmp.path().join("redacted.asif.zip");
    let report_path = tmp.path().join("report.json");
    let exported = export_asif(
        &deps.paths,
        &session,
        &out,
        &AsifExportOptions {
            redact: vec![SECRET.to_owned()],
            report_path: Some(report_path.clone()),
            ..AsifExportOptions::default()
        },
    )
    .expect("redacted export");

    let package = vowp_asif::unpack(&out).expect("unpack");
    let session_json = String::from_utf8(package.session).expect("session JSON is UTF-8");
    assert!(
        !session_json.contains(SECRET),
        "redaction left the literal in session.json"
    );
    let report_json = fs::read_to_string(&report_path).expect("full report");
    assert!(
        !report_json.contains(SECRET),
        "redaction left the literal in the full report"
    );
    let summary = serde_json::to_string(&exported.report).expect("summary JSON");
    assert!(!summary.contains(SECRET), "redaction left the literal in the summary");
}

#[test]
fn asif_export_strict_refuses_blocking_loss_and_removes_output() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "missing capsule");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &log,
            Actor::System,
            EventBody::Capsule(CapsuleEvent {
                capsule_id: vowp_protocol::CapsuleId::new(),
                version: 1,
                trigger: CapsuleTrigger::Manual,
                at_seq: 1,
            }),
        );
        log.sync().expect("sync");
    }

    let default_out = tmp.path().join("default.asif.zip");
    let default = export_asif(
        &deps.paths,
        &session,
        &default_out,
        &AsifExportOptions {
            allow_loss: true,
            ..AsifExportOptions::default()
        },
    )
    .expect("default export allows a declared integrity loss");
    assert!(
        default.report.losses.iter().any(|loss| loss.kind == "capsule-missing"),
        "fixture must declare the missing capsule: {:?}",
        default.report.losses
    );

    let strict_out = tmp.path().join("strict.asif.zip");
    let strict_report = tmp.path().join("strict-report.json");
    fs::write(&strict_out, b"existing output must stay").expect("seed output");
    fs::write(&strict_report, b"existing report must stay").expect("seed report");
    let error = export_asif(
        &deps.paths,
        &session,
        &strict_out,
        &AsifExportOptions {
            strict: true,
            allow_loss: true,
            report_path: Some(strict_report.clone()),
            ..AsifExportOptions::default()
        },
    )
    .expect_err("strict export must refuse an integrity loss");

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.to_string().contains("STRICT_LOSS"), "{error}");
    assert_eq!(
        fs::read(&strict_out).expect("seeded output"),
        b"existing output must stay"
    );
    assert_eq!(
        fs::read(&strict_report).expect("seeded report"),
        b"existing report must stay"
    );
}

#[test]
fn asif_export_keeps_inherent_losses_non_fatal() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "inherent losses");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, "a normal session keeps its inherent losses");
        log.sync().expect("sync");
    }
    let out = tmp.path().join("inherent.asif.zip");
    let exported = export_asif(
        &deps.paths,
        &session,
        &out,
        &AsifExportOptions {
            strict: true,
            ..AsifExportOptions::default()
        },
    )
    .expect("inherent omission losses are not strict-fatal");

    assert!(out.is_file());
    assert!(exported.report.loss_count > 0);
    let kinds: Vec<&str> = exported.report.losses.iter().map(|loss| loss.kind.as_str()).collect();
    assert!(kinds.contains(&"cwd-omitted"), "{kinds:?}");
    assert!(kinds.contains(&"no-causal-edges"), "{kinds:?}");
    assert!(exported.report.unsupported.iter().any(|item| item == "vowp.jobs"));
    assert!(!tmp.path().join("inherent.asif.zip.report.json").exists());
}

#[test]
fn asif_import_creates_one_indexed_session_and_no_turn() {
    let tmp = tempdir().expect("tempdir");
    let source_deps = deps_with_capsule(&tmp, "Restore the capsule without starting a turn");
    let service = SessionService::new(source_deps.clone());
    let session = create_session(&service, &tmp, "capsule source");
    {
        let log = SessionLog::open(&source_deps.paths, &session, source_deps.clock.clone()).expect("log");
        append_user(&log, "remember the capsule");
        log.sync().expect("sync");
    }
    let capsule = CapsuleWriter::new(source_deps.clone())
        .write(&session, CapsuleTrigger::Manual)
        .expect("write capsule");
    let package = tmp.path().join("capsule.asif.zip");
    export_default(&source_deps.paths, &session, &package);

    let target_deps = deps_at(&tmp, "target", AdapterRegistry::new());
    let store = Store::open(&target_deps.paths).expect("store");
    let imported = import_default(&target_deps.paths, &store, &package, &tmp.path().join("project"));

    let row = store.require_session(&imported).expect("imported row");
    assert_eq!(row.state, SessionState::Idle);
    assert_ne!(imported, session);
    assert!(store.lease(&imported).expect("lease").is_none());
    let events = log_events(&target_deps.paths, &imported);
    assert!(!events.is_empty());
    assert!(matches!(
        events[0].body().expect("first body"),
        EventBody::SessionCreated(_)
    ));
    assert_eq!(
        store.event_rows(&imported, 0, 10_000).expect("indexed events").len(),
        events.len()
    );
    let imported_capsules = store.list_capsules(&imported).expect("imported capsules");
    assert_eq!(imported_capsules.len(), 1);
    assert_eq!(imported_capsules[0].capsule_id, capsule.capsule_id);
    assert_eq!(imported_capsules[0].session_id, imported);
    let files = list_capsule_files(&target_deps.paths, &imported).expect("capsule files");
    assert_eq!(files.len(), 1);
    let restored = read_capsule_file(&files[0]).expect("read restored capsule");
    assert_eq!(restored.capsule_id, capsule.capsule_id);
    assert_eq!(restored.session_id, imported);
}

#[test]
fn asif_import_round_trip_preserves_text_blobs_and_event_order() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "round trip");
    let large_output = "x".repeat(9_000);
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, "first user message");
        append_assistant(&log, "first assistant message");
        append(
            &log,
            Actor::Assistant,
            EventBody::ToolCall(ToolCall {
                call_id: "call-1".to_owned(),
                name: "shell".to_owned(),
                input: json!({"command": "echo large"}),
            }),
        );
        append(
            &log,
            Actor::Tool,
            EventBody::ToolResult(ToolResult {
                call_id: "call-1".to_owned(),
                ok: true,
                output: Some(large_output.clone()),
                output_ref: None,
                exit_code: Some(0),
            }),
        );
        log.sync().expect("sync");
    }
    let source_events = log_events(&deps.paths, &session);
    let source_text = text_events(&source_events);
    let source_types = event_type_sequence(&source_events);
    let sha256 = blob_ref(&source_events);
    let source_blob = BlobStore::new(deps.paths.blobs_dir(&session))
        .get(&sha256)
        .expect("source blob");

    let package = tmp.path().join("roundtrip.asif.zip");
    export_default(&deps.paths, &session, &package);
    let target_deps = deps_at(&tmp, "target", AdapterRegistry::new());
    let store = Store::open(&target_deps.paths).expect("store");
    let imported = import_default(&target_deps.paths, &store, &package, &tmp.path().join("project"));
    let imported_events = log_events(&target_deps.paths, &imported);

    assert_eq!(text_events(&imported_events), source_text);
    assert_eq!(event_type_sequence(&imported_events), source_types);
    let imported_sha = blob_ref(&imported_events);
    assert_eq!(imported_sha, sha256);
    let imported_blob = BlobStore::new(target_deps.paths.blobs_dir(&imported))
        .get(&imported_sha)
        .expect("imported blob");
    assert_eq!(imported_blob, source_blob);
}

#[test]
fn asif_import_invalid_package_leaves_no_directory_or_row() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = Store::open(&deps.paths).expect("store");
    let before = session_count(&store);
    let package = tmp.path().join("invalid.asif.zip");
    fs::write(&package, b"this is not an ASIF package").expect("invalid fixture");

    let error = import_asif(
        &deps.paths,
        &store,
        &package,
        &AsifImportOptions::new(tmp.path().join("project")),
    )
    .expect_err("invalid package must be refused");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(session_count(&store), before);
    assert_eq!(
        fs::read_dir(deps.paths.tmp_dir()).expect("tmp dir").count(),
        0,
        "temporary import directory must be removed"
    );
}

#[test]
fn asif_import_existing_id_is_a_conflict_and_changes_nothing() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let existing = create_session(&service, &tmp, "existing target");
    {
        let log = SessionLog::open(&deps.paths, &existing, deps.clock.clone()).expect("log");
        append_user(&log, "original bytes");
        log.sync().expect("sync");
    }
    let before_log = fs::read(deps.paths.log_path(&existing)).expect("log bytes");
    let package = write_package(tmp.path(), "fixture.asif.zip", &minimal_document());
    let store = Store::open(&deps.paths).expect("store");

    let error = import_asif(
        &deps.paths,
        &store,
        &package,
        &AsifImportOptions {
            session_id: Some(existing.clone()),
            ..AsifImportOptions::new(tmp.path().join("project"))
        },
    )
    .expect_err("existing target must conflict");

    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(fs::read(deps.paths.log_path(&existing)).expect("log bytes"), before_log);
    assert_eq!(session_count(&store), 1);
}

#[test]
fn asif_import_same_package_twice_conflicts_on_event_ids_and_keeps_the_first_session() {
    let tmp = tempdir().expect("tempdir");
    let source_deps = deps_at(&tmp, "source", AdapterRegistry::new());
    let service = SessionService::new(source_deps.clone());
    let source = create_session(&service, &tmp, "repeat import source");
    {
        let log = SessionLog::open(&source_deps.paths, &source, source_deps.clock.clone()).expect("log");
        append_user(&log, "import me once");
        append_assistant(&log, "the first import stays");
        log.sync().expect("sync");
    }
    let package = tmp.path().join("repeat.asif.zip");
    export_default(&source_deps.paths, &source, &package);
    let target_deps = deps_at(&tmp, "target", AdapterRegistry::new());
    let store = Store::open(&target_deps.paths).expect("store");
    let cwd = tmp.path().join("project");
    let first = import_default(&target_deps.paths, &store, &package, &cwd);
    let first_log = fs::read(target_deps.paths.log_path(&first)).expect("first log");

    let error = import_asif(&target_deps.paths, &store, &package, &AsifImportOptions::new(&cwd))
        .expect_err("second import must conflict on the preserved event ids");

    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert!(error.to_string().contains("EVENT_ID_DUPLICATE"), "{error}");
    assert_eq!(session_count(&store), 1, "the first import remains");
    assert_eq!(
        fs::read(target_deps.paths.log_path(&first)).expect("first log"),
        first_log
    );
}

#[test]
fn asif_import_duplicate_event_id_conflicts_without_a_partial_index() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = Store::open(&deps.paths).expect("store");
    let mut document = minimal_document();
    document["events"][1]["id"] = json!("asif-test-event-1");
    let package = write_package(tmp.path(), "duplicate.asif.zip", &document);

    let error = import_asif(
        &deps.paths,
        &store,
        &package,
        &AsifImportOptions::new(tmp.path().join("project")),
    )
    .expect_err("duplicate ASIF event ids must conflict");

    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(session_count(&store), 0);
    assert_eq!(fs::read_dir(deps.paths.sessions_dir()).expect("sessions").count(), 0);
    assert_eq!(fs::read_dir(deps.paths.tmp_dir()).expect("tmp").count(), 0);
}

#[test]
fn asif_import_strict_removes_staging_and_reports_the_loss() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = Store::open(&deps.paths).expect("store");
    let package = write_package(tmp.path(), "orphan.asif.zip", &orphan_tool_result_document());
    let report_path = tmp.path().join("import-report.json");

    let error = import_asif(
        &deps.paths,
        &store,
        &package,
        &AsifImportOptions {
            strict: true,
            allow_loss: true,
            report_path: Some(report_path.clone()),
            ..AsifImportOptions::new(tmp.path().join("project"))
        },
    )
    .expect_err("strict import must refuse the downgraded integrity loss");

    assert_eq!(error.kind(), ErrorKind::Refused);
    assert!(error.to_string().contains("STRICT_LOSS"), "{error}");
    assert!(
        error.to_string().contains("recoverable-integrity-downgraded"),
        "{error}"
    );
    assert!(!report_path.exists());
    assert_eq!(session_count(&store), 0);
    assert_eq!(fs::read_dir(deps.paths.sessions_dir()).expect("sessions").count(), 0);
    assert_eq!(fs::read_dir(deps.paths.tmp_dir()).expect("tmp").count(), 0);
}

#[test]
fn asif_import_allow_loss_accepts_a_recoverable_integrity_fixture() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = Store::open(&deps.paths).expect("store");
    let package = write_package(tmp.path(), "orphan.asif.zip", &orphan_tool_result_document());

    let imported = import_asif(
        &deps.paths,
        &store,
        &package,
        &AsifImportOptions {
            allow_loss: true,
            ..AsifImportOptions::new(tmp.path().join("project"))
        },
    )
    .expect("allow-loss import accepts the recoverable fixture");

    assert_eq!(imported.session.state, SessionState::Idle);
    assert!(
        imported
            .report
            .losses
            .iter()
            .any(|loss| loss.kind == "recoverable-integrity-downgraded"),
        "report must declare the downgraded integrity loss: {:?}",
        imported.report.losses
    );
    assert_eq!(session_count(&store), 1);
}

#[test]
fn asif_import_multiple_branches_requires_a_selection() {
    let tmp = tempdir().expect("tempdir");
    let source_deps = deps_at(&tmp, "source", AdapterRegistry::new());
    let service = SessionService::new(source_deps.clone());
    let source = create_session(&service, &tmp, "branch source");
    {
        let log = SessionLog::open(&source_deps.paths, &source, source_deps.clock.clone()).expect("log");
        append_user(&log, "branch user message");
        append_assistant(&log, "branch assistant message");
        log.sync().expect("sync");
    }
    let package = tmp.path().join("branches.asif.zip");
    export_default(&source_deps.paths, &source, &package);
    add_alternate_branch(&package);
    let target_deps = deps_at(&tmp, "target", AdapterRegistry::new());
    let store = Store::open(&target_deps.paths).expect("store");

    let error = import_asif(
        &target_deps.paths,
        &store,
        &package,
        &AsifImportOptions::new(tmp.path().join("project")),
    )
    .expect_err("multiple branches need a selection");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(error.to_string().contains("BRANCH_AMBIGUOUS"), "{error}");
    assert_eq!(
        fs::read_dir(target_deps.paths.sessions_dir())
            .expect("sessions")
            .count(),
        0,
        "the target home stays empty"
    );
}

#[test]
fn asif_import_selected_branch_maps_only_that_branch() {
    let tmp = tempdir().expect("tempdir");
    let source_deps = deps_at(&tmp, "source", AdapterRegistry::new());
    let service = SessionService::new(source_deps.clone());
    let source = create_session(&service, &tmp, "branch source");
    {
        let log = SessionLog::open(&source_deps.paths, &source, source_deps.clock.clone()).expect("log");
        append_user(&log, "alternate branch user message");
        append_assistant(&log, "main branch assistant message");
        log.sync().expect("sync");
    }
    let package = tmp.path().join("branches.asif.zip");
    export_default(&source_deps.paths, &source, &package);
    add_alternate_branch(&package);
    let target_deps = deps_at(&tmp, "target", AdapterRegistry::new());
    let store = Store::open(&target_deps.paths).expect("store");

    let imported = import_asif(
        &target_deps.paths,
        &store,
        &package,
        &AsifImportOptions {
            branch: Some("alternate".to_owned()),
            ..AsifImportOptions::new(tmp.path().join("project"))
        },
    )
    .expect("selected branch imports");

    let texts = text_events(&log_events(&target_deps.paths, &imported.session.id));
    assert!(
        texts.iter().any(|(_, text)| text == "alternate branch user message"),
        "{texts:?}"
    );
    assert!(
        texts.iter().all(|(_, text)| text != "main branch assistant message"),
        "the unselected branch must not be mapped: {texts:?}"
    );
}

/// Adds one alternate branch that selects only the first user-message event.
fn add_alternate_branch(path: &Path) {
    let mut package = vowp_asif::unpack(path).expect("unpack");
    let mut document = unpack_document(path);
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
    fs::write(path, vowp_asif::pack(&package).expect("repack")).expect("write mutated package");
}

#[test]
fn asif_adapter_version_and_pin_match_the_documented_contract() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "contract");
    let out = tmp.path().join("contract.asif.zip");
    let exported = export_asif(&deps.paths, &session, &out, &AsifExportOptions::default()).expect("export");

    assert_eq!(vowp_asif::schema::ASIF_VERSION, "0.4");
    assert_eq!(vowp_asif::schema::ASIF_PIN, "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f");
    assert_eq!(vowp_asif::report::AdapterInfo::default().version, "0.1.1");
    assert_eq!(exported.report.adapter_version, "0.1.1");
    assert_eq!(exported.report.asif_version, "0.4");
    assert_eq!(exported.report.asif_pin, "652d9e23ddbe5656b9ceea311df7e09e93c1cd2f");
    assert_eq!(MAX_ASIF_ARCHIVE_BYTES, 272 * 1024 * 1024);
}
