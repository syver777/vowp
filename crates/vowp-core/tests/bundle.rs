//! Bundle export/import tests: round trip, checksum/format rejection and blob inlining.
//! Hermetic: temp node, fake clock, fake capsule adapter; no network or real HOME.
//! Spec: `docs/design/sessions.md` §6 and `docs/design/crates/vowp-core.md` §3.

mod support;

use std::path::Path;
use std::sync::Arc;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::bundle::{self, BUNDLE_FORMAT};
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps};
use vowp_protocol::{
    Actor, AssistantMessage, CancelToken, EventBody, EventEnvelope, EventSource, EventType, Executor, ExecutorRef,
    FakeClock, HarnessKind, JobId, JobKind, MemoryLog, NewJob, NodeName, Project, ProjectName, Role, RunPurpose,
    RunRole, SessionId, SessionRole, Timestamp, ToolResult, TurnRequest, UserMessage,
};
use vowp_store::blobs::BlobStore;
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, SessionFilter};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, registry: AdapterRegistry) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
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
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(0)),
    }
}

fn draft_json(intent: &str) -> String {
    serde_json::json!({
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

fn capsule_adapter(intent: &str) -> FakeAdapter {
    FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: draft_json(intent),
        })],
    )
}

fn deps_with_capsule(tmp: &TempDir, intent: &str) -> Deps {
    deps(
        tmp,
        AdapterRegistry::new().with("opencode", Arc::new(capsule_adapter(intent))),
    )
}

fn create_session(service: &SessionService, tmp: &TempDir, name: &str) -> SessionId {
    create_session_with(service, tmp, name, None, None)
}

fn create_session_with(
    service: &SessionService,
    tmp: &TempDir,
    name: &str,
    project: Option<ProjectName>,
    job_id: Option<JobId>,
) -> SessionId {
    service
        .create_session(vowp_protocol::NewSession {
            project,
            job_id,
            role: SessionRole::Worker,
            title: name.to_owned(),
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

fn session_count(deps: &Deps) -> usize {
    deps.open_store()
        .expect("store")
        .list_sessions(&SessionFilter::default())
        .expect("sessions")
        .len()
}

#[test]
fn bundle_export_import_roundtrip_child_session() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_capsule(&tmp, "Continue the bundle work");
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "roundtrip");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, "remember the frobnicator");
        append_assistant(&log, "noted");
        log.sync().expect("sync");
    }

    let exported = bundle::export(&deps, &session, 10).expect("export");
    assert_eq!(exported.format, BUNDLE_FORMAT);
    assert_eq!(exported.protocol_version, vowp_protocol::PROTOCOL_VERSION);
    assert_eq!(exported.source_node, deps.node);
    assert_eq!(exported.session.id, session);
    assert_eq!(exported.capsule.trigger, vowp_protocol::CapsuleTrigger::Export);
    assert!(exported.tail.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::UserMessage(payload)) if payload.text == "remember the frobnicator"
    )));
    assert!(exported.checksum.starts_with("sha256:"));
    assert_eq!(exported.checksum.len(), "sha256:".len() + 64);

    let json = bundle::to_json(&exported).expect("to_json");
    let parsed = bundle::from_json(&json).expect("from_json");
    assert_eq!(parsed, exported, "canonical checksum survives a pretty round trip");

    let store = deps.open_store().expect("store");
    let origin_events = SessionLog::open_read(&deps.paths, &session)
        .expect("origin log")
        .read_all()
        .expect("origin events");

    let before_import = session_count(&deps);
    let child = bundle::import(&deps, &exported, None, None).expect("import");
    assert_ne!(child, session);
    assert_eq!(session_count(&deps), before_import + 1);

    let row = store.require_session(&child).expect("child row");
    assert_eq!(row.parent_session_id, Some(session.clone()));
    assert_eq!(row.title, "roundtrip (imported)");
    assert_eq!(row.cwd, exported.session.cwd);
    assert_eq!(row.executor, Some(opencode()));

    let child_events = SessionLog::open_read(&deps.paths, &child)
        .expect("child log")
        .read_all()
        .expect("child events");
    let types: Vec<EventType> = child_events.iter().map(|event| event.event_type).collect();
    assert_eq!(
        types,
        vec![
            EventType::SessionCreated,
            EventType::Note,
            EventType::Switch,
            EventType::Capsule
        ],
        "exactly the four import setup events"
    );
    let created = child_events[0].body().expect("created");
    assert!(matches!(
        created,
        EventBody::SessionCreated(ref payload) if payload.parent_session_id == Some(session.clone())
    ));
    let note = match child_events[1].body().expect("note") {
        EventBody::Note(note) => note.text,
        other => panic!("expected note, got {other:?}"),
    };
    assert!(note.starts_with(&format!("imported from local session {session} at seq ")));

    let switch_event = match child_events[2].body().expect("switch") {
        EventBody::Switch(payload) => payload,
        other => panic!("expected switch, got {other:?}"),
    };
    assert_eq!(switch_event.reason, vowp_protocol::SwitchReason::User);
    assert_eq!(switch_event.to.executor, opencode());
    assert_eq!(switch_event.to.node.as_ref(), Some(&deps.node));
    assert!(switch_event.capsule_id.is_some());

    let capsule = store.latest_capsule(&child).expect("capsule").expect("capsule row");
    assert_ne!(capsule.capsule_id, exported.capsule.capsule_id);
    assert_eq!(capsule.session_id, child);
    assert_eq!(capsule.intent, "Continue the bundle work");
    assert_eq!(row.last_capsule_id, Some(capsule.capsule_id.clone()));

    let lineage = store.list_switches(&child).expect("child lineage");
    assert_eq!(lineage.len(), 1, "reindex rebuilds the import switch row");
    assert_eq!(lineage[0].state, vowp_protocol::SwitchState::Completed);
    assert_eq!(lineage[0].capsule_id, Some(capsule.capsule_id.clone()));

    let seed = bundle::imported_seed(&deps, &exported, &child, "next step");
    assert!(seed.contains("Continue the bundle work"));
    assert!(seed.contains("remember the frobnicator"));
    assert!(seed.ends_with("next step"));
    assert!(
        seed.lines()
            .next()
            .is_some_and(|line| line.contains(&format!("session {child}"))),
        "the seed names the child, not the origin session"
    );

    let origin_after = SessionLog::open_read(&deps.paths, &session)
        .expect("origin log")
        .read_all()
        .expect("origin events");
    assert_eq!(origin_events, origin_after, "import never touches the origin session");

    let second_child = bundle::import(&deps, &exported, None, None).expect("second import");
    assert_ne!(second_child, child, "no dedupe: each import is an independent child");
}

#[test]
fn imported_child_first_turn_uses_imported_seed() {
    let tmp = tempdir().expect("tempdir");
    let adapter = Arc::new(capsule_adapter("Continue the imported work"));
    let deps = deps(&tmp, AdapterRegistry::new().with("opencode", adapter.clone()));
    let service = SessionService::new(deps.clone());
    let origin = create_session(&service, &tmp, "origin");
    {
        let log = SessionLog::open(&deps.paths, &origin, deps.clock.clone()).expect("origin log");
        append_user(&log, "ORIGIN-TAIL-UNIQUE-7f3");
        append_assistant(&log, "the origin result");
        log.sync().expect("sync");
    }

    let exported = bundle::export(&deps, &origin, 10).expect("export");
    let child = bundle::import(&deps, &exported, None, None).expect("import");
    assert_ne!(child, origin);

    service
        .run_turn(
            TurnRequest {
                session_id: child.clone(),
                executor: ExecutorRef {
                    executor: opencode(),
                    node: None,
                },
                purpose: RunPurpose::Start,
                role: RunRole::Worker,
                job_id: None,
                prompt: "pick up the import".to_owned(),
                profile: Some("worker".to_owned()),
                rung: 0,
                window: None,
                timeout: None,
            },
            &CancelToken::default(),
        )
        .expect("first child turn");

    let starts = adapter.starts();
    let seeded = starts.last().expect("child start");
    let first_line = seeded.prompt.lines().next().unwrap_or_default();
    assert!(
        first_line.contains(&format!("You are continuing session {child}")),
        "the seed names the child session: {first_line}"
    );
    assert!(
        !first_line.contains(origin.as_str()),
        "the origin session id is not the seed's session: {first_line}"
    );
    assert!(
        seeded.prompt.contains("ORIGIN-TAIL-UNIQUE-7f3"),
        "the bundle tail is rendered as the last turns"
    );
    assert!(
        seeded.prompt.contains("Continue the imported work"),
        "the capsule markdown is part of the seed"
    );
    assert!(
        seeded.prompt.ends_with("pick up the import"),
        "the first turn's input is the seed's next message"
    );

    let events = SessionLog::open_read(&deps.paths, &child)
        .expect("child log")
        .read_all()
        .expect("child events");
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload))
                if payload.note.as_deref() == Some("handoff")
                    && payload.text.contains("ORIGIN-TAIL-UNIQUE-7f3")
        )),
        "the log shows exactly what the child executor saw"
    );
    assert!(
        !deps.paths.session_dir(&child).join("import.json").exists(),
        "the import sidecar is consumed by the first turn"
    );
}

#[test]
fn bundle_checksum_mismatch_rejected() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_capsule(&tmp, "checksum handoff");
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "checksum");
    let exported = bundle::export(&deps, &session, 10).expect("export");

    let mut value = serde_json::to_value(&exported).expect("json value");
    value["session"]["title"] = serde_json::Value::String("tampered".to_owned());
    let tampered = serde_json::to_string(&value).expect("json");
    let error = bundle::from_json(&tampered).expect_err("checksum must fail");
    assert!(error.to_string().contains("checksum mismatch"), "got {error}");

    let mut poisoned = exported.clone();
    poisoned.checksum = format!("sha256:{}", "0".repeat(64));
    let before = session_count(&deps);
    let error = bundle::import(&deps, &poisoned, None, None).expect_err("checksum must fail");
    assert!(error.to_string().contains("checksum mismatch"), "got {error}");
    assert_eq!(session_count(&deps), before, "nothing is created on rejection");
}

#[test]
fn bundle_unknown_format_rejected() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_capsule(&tmp, "format handoff");
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "format");
    let exported = bundle::export(&deps, &session, 10).expect("export");

    let mut value = serde_json::to_value(&exported).expect("json value");
    value["format"] = serde_json::Value::String("vowp-bundle-v2".to_owned());
    let unknown = serde_json::to_string(&value).expect("json");
    let error = bundle::from_json(&unknown).expect_err("format must fail");
    assert!(error.to_string().contains("unknown bundle format"), "got {error}");

    let mut too_new = exported.clone();
    too_new.format = "vowp-bundle-v2".to_owned();
    let before = session_count(&deps);
    let error = bundle::import(&deps, &too_new, None, None).expect_err("format must fail");
    assert!(error.to_string().contains("unknown bundle format"), "got {error}");
    assert_eq!(session_count(&deps), before, "nothing is created on rejection");
}

#[test]
fn bundle_inlines_small_blobs_only() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps_with_capsule(&tmp, "blob handoff");
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "blobs");
    let small = "a".repeat(60_000);
    let large = "b".repeat(70_000);
    let (small_ref, large_ref) = {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append_user(&log, "run the two tools");
        let small_event = append(
            &log,
            Actor::Assistant,
            EventBody::ToolResult(ToolResult {
                call_id: "small".to_owned(),
                ok: true,
                output: Some(small.clone()),
                output_ref: None,
                exit_code: Some(0),
            }),
        );
        let large_event = append(
            &log,
            Actor::Tool,
            EventBody::ToolResult(ToolResult {
                call_id: "large".to_owned(),
                ok: true,
                output: Some(large.clone()),
                output_ref: None,
                exit_code: Some(0),
            }),
        );
        log.sync().expect("sync");
        let small_ref = match small_event.body().expect("small body") {
            EventBody::ToolResult(payload) => payload.output_ref.expect("small spilled"),
            other => panic!("expected tool result, got {other:?}"),
        };
        let large_ref = match large_event.body().expect("large body") {
            EventBody::ToolResult(payload) => payload.output_ref.expect("large spilled"),
            other => panic!("expected tool result, got {other:?}"),
        };
        assert!(small_ref.bytes <= 65_536);
        assert!(large_ref.bytes > 65_536);
        (small_ref, large_ref)
    };

    let exported = bundle::export(&deps, &session, 20).expect("export");
    assert!(exported.blobs_inline.contains_key(&small_ref.sha256));
    assert!(
        !exported.blobs_inline.contains_key(&large_ref.sha256),
        "blobs above 64 KiB stay references"
    );

    let child = bundle::import(&deps, &exported, None, None).expect("import");
    let blobs = BlobStore::new(deps.paths.blobs_dir(&child));
    assert_eq!(blobs.get(&small_ref.sha256).expect("small blob"), small.as_bytes());
    assert!(!blobs.exists(&large_ref.sha256));
}

#[test]
fn bundle_import_drops_missing_project_and_job_rows() {
    let source_tmp = tempdir().expect("source tempdir");
    let source_deps = deps_with_capsule(&source_tmp, "cross-node handoff");
    let source_service = SessionService::new(source_deps.clone());
    let source_store = source_deps.open_store().expect("source store");
    let project = ProjectName::parse("probe-project").expect("project name");
    let job = JobId::parse("n07c-import").expect("job id");
    source_store
        .upsert_project(&Project {
            name: project.clone(),
            path: source_tmp.path().join("project").to_string_lossy().into_owned(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: ts(),
            updated_at: ts(),
        })
        .expect("project row");
    source_store
        .insert_job(
            &project,
            &NewJob {
                id: job.clone(),
                title: "import me".to_owned(),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: None,
                needs: Vec::new(),
                owns: Vec::new(),
                reads: Vec::new(),
                gates: None,
                priority: None,
                loop_: None,
                brief_text: None,
            },
            "test",
        )
        .expect("job row");
    let session = create_session_with(
        &source_service,
        &source_tmp,
        "cross node",
        Some(project.clone()),
        Some(job.clone()),
    );

    let exported = bundle::export(&source_deps, &session, 10).expect("export");
    assert_eq!(exported.session.project, Some(project));
    assert_eq!(exported.session.job_id, Some(job));

    let dest_tmp = tempdir().expect("dest tempdir");
    let dest_deps = deps(&dest_tmp, AdapterRegistry::new());
    let dest_store = dest_deps.open_store().expect("dest store");
    assert!(
        dest_store
            .project(&exported.session.project.clone().unwrap())
            .expect("project")
            .is_none()
    );
    assert!(
        dest_store
            .job(&exported.session.job_id.clone().unwrap())
            .expect("job")
            .is_none()
    );

    let child = bundle::import(&dest_deps, &exported, None, None).expect("cross-node import");
    let row = dest_store.require_session(&child).expect("child row");
    assert_eq!(row.project, None, "a missing project row becomes NULL");
    assert_eq!(row.job_id, None, "a missing job row becomes NULL");
    assert_eq!(row.parent_session_id, Some(session));
    assert_eq!(row.title, "cross node (imported)");
}
