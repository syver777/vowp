//! Remote executor tests: framing, home-assigned sequence numbers, idempotent replay,
//! torn tails, lease rejection, summary finalisation and remote usage.
//!
//! Hermetic: a temporary node home, a fake clock and no harness process. The transport is
//! a `Cursor`, which lets a test cut the stream exactly where a dropped SSH connection would.

mod support;

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::bundle;
use vowp_core::session::remote::{BeginTurn, IngestGrant, begin_turn, ingest_lines};
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, CoreError, Deps};
use vowp_protocol::{
    Actor, AssistantMessage, BuildMode, Cost, CostEvent, EventBody, EventEnvelope, EventId, EventSource, EventType,
    Executor, ExecutorRef, FakeClock, HarnessKind, IngestAck, LimitEvent, LimitKind, MemoryLog, NodeName, RemoteEvent,
    RemoteLine, RemoteUsage, RunId, RunOutcome, RunPurpose, SessionId, SessionRole, StopReason, Timestamp, Tokens,
    ToolResult, TurnEnded, UsageSource, UserMessage,
};
use vowp_store::NodePaths;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn laptop() -> NodeName {
    NodeName::parse("laptop").expect("node name")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: NodeName::parse("local").expect("node name"),
        clock: Arc::new(FakeClock::at(ts())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(7)),
    }
}

fn create_session(service: &SessionService, tmp: &TempDir, name: &str) -> SessionId {
    service
        .create_session(vowp_protocol::NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: name.to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: Some(opencode()),
        })
        .expect("create session")
}

fn begin(deps: &Deps, session: &SessionId, lease: &str, prompt: &str) -> vowp_protocol::TurnGrant {
    begin_on(deps, session, lease, prompt, laptop(), None)
}

fn begin_on(
    deps: &Deps,
    session: &SessionId,
    lease: &str,
    prompt: &str,
    node: NodeName,
    cwd: Option<String>,
) -> vowp_protocol::TurnGrant {
    let store = deps.open_store().expect("store");
    begin_turn(
        deps,
        &store,
        &BeginTurn {
            session: session.clone(),
            lease: lease.to_owned(),
            executor: ExecutorRef {
                executor: opencode(),
                node: Some(node.clone()),
            },
            purpose: RunPurpose::Start,
            node,
            cwd,
            prompt: prompt.to_owned(),
        },
    )
    .expect("begin turn")
}

/// Dependencies whose built-in opencode adapter can resume native sessions.
///
/// The registry resolves adapters from config without probing or spawning a harness.
fn resumable_deps(tmp: &TempDir) -> Deps {
    let mut deps = deps(tmp);
    deps.adapters = Arc::new(AdapterRegistry::from_config(deps.config.as_ref()).expect("adapter registry from config"));
    deps
}

/// Produces the capsule draft the capsule writer records during bundle export.
fn capsule_adapter(intent: &str) -> FakeAdapter {
    let draft = serde_json::json!({
        "intent": intent,
        "constraints": [],
        "decisions": [],
        "state": {"done": ["started"], "in_progress": ["import"], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    });
    FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: draft.to_string(),
        })],
    )
}

fn read_events(deps: &Deps, session: &SessionId) -> Vec<EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

fn hello(grant: &vowp_protocol::TurnGrant, lease: &str) -> RemoteLine {
    RemoteLine::Hello {
        session_id: grant.session_id.clone(),
        run_id: grant.run_id.clone(),
        executor: grant.executor.clone(),
        node: grant.node.clone(),
        spool_id: "test-spool".to_owned(),
        lease: lease.to_owned(),
    }
}

fn remote_event(id: EventId, body: EventBody) -> RemoteEvent {
    RemoteEvent {
        id,
        ts: ts(),
        event_type: body.event_type(),
        actor: Actor::Assistant,
        source: EventSource {
            harness: HarnessKind::Opencode,
            model: Some("deepseek/deepseek-flash".to_owned()),
            node: Some(laptop()),
            native_session_id: None,
        },
        payload: body.to_value(),
        raw: None,
        replayable: !matches!(body, EventBody::AssistantReasoning(_)),
    }
}

fn summary(cost: Cost) -> RemoteLine {
    RemoteLine::Summary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: None,
        cost,
        limit: None,
        result_text: Some("done".to_owned()),
        stop_reason: StopReason::EndTurn,
        usage: None,
    }
}

fn lines(frames: &[RemoteLine]) -> String {
    frames
        .iter()
        .map(|frame| serde_json::to_string(frame).expect("frame JSON"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn ingest(
    deps: &Deps,
    grant: &vowp_protocol::TurnGrant,
    expected_lease: &str,
    text: &str,
) -> (
    Result<vowp_core::session::remote::IngestReport, CoreError>,
    Vec<IngestAck>,
) {
    let store = deps.open_store().expect("store");
    let input = IngestGrant {
        session: grant.session_id.clone(),
        lease: expected_lease.to_owned(),
        run: grant.run_id.clone(),
    };
    let mut reader = Cursor::new(text.as_bytes().to_vec());
    let mut ack = Vec::new();
    let result = ingest_lines(deps, &store, &input, &mut reader, &mut ack);
    let acks = String::from_utf8_lossy(&ack)
        .lines()
        .map(|line| serde_json::from_str(line).expect("ack JSON"))
        .collect();
    (result, acks)
}

#[test]
fn turn_begin_writes_framing_events_and_returns_grant() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "remote");
    let grant = begin(&deps, &session, "lease-token", "do the work");

    assert_eq!(grant.session_id, session);
    assert_eq!(grant.executor, opencode().to_string());
    assert_eq!(grant.node, laptop());
    assert_eq!(grant.prompt, "do the work");
    assert_eq!(grant.lease_token, "lease-token");
    assert_eq!(grant.resume_native, None);
    assert!(grant.native_hint.is_some());
    assert_eq!(grant.tail_seq, 4);

    let events = read_events(&deps, &session);
    let types: Vec<EventType> = events.iter().map(|event| event.event_type).collect();
    assert_eq!(
        types,
        vec![
            EventType::SessionCreated,
            EventType::SessionExecutor,
            EventType::TurnStarted,
            EventType::UserMessage,
        ]
    );
    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.node, laptop());
    assert_eq!(run.purpose, RunPurpose::Start);
    assert!(run.session_id.as_ref() == Some(&session));
    let row = store.require_session(&session).expect("session");
    assert_eq!(row.state, vowp_protocol::SessionState::Running);

    let log = std::fs::read_to_string(deps.paths.log_path(&session)).expect("log text");
    assert!(!log.contains("lease-token"), "the lease token reached the session log");
}

#[test]
fn imported_session_first_remote_turn_uses_imported_seed() {
    let tmp = tempdir().expect("tempdir");
    let base = deps(&tmp);
    let mut export_deps = base.clone();
    export_deps.adapters =
        Arc::new(AdapterRegistry::new().with("opencode", Arc::new(capsule_adapter("Continue the remote import"))));
    let service = SessionService::new(export_deps.clone());
    let origin = create_session(&service, &tmp, "origin");
    {
        let log = SessionLog::open(&export_deps.paths, &origin, export_deps.clock.clone()).expect("origin log");
        log.append(NewEvent {
            actor: Actor::User,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(export_deps.node.clone()),
                native_session_id: None,
            },
            body: EventBody::UserMessage(UserMessage {
                text: "ORIGIN-TAIL-REMOTE-7f3".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
            raw: None,
            ts: Some(ts()),
        })
        .expect("origin tail");
        log.sync().expect("sync origin");
    }

    let exported = bundle::export(&export_deps, &origin, 10).expect("export");
    let child = bundle::import(&export_deps, &exported, None, None).expect("import");
    assert!(
        export_deps.paths.session_dir(&child).join("import.json").is_file(),
        "the import sidecar was not written"
    );

    // The first remote turn must consume the sidecar even though this executor supports
    // native resume: no native id exists yet for the child on this node.
    let remote = resumable_deps(&tmp);
    let grant = begin(&remote, &child, "lease", "pick up the remote import");

    assert_eq!(grant.resume_native, None);
    assert!(
        grant.prompt.contains("Continue the remote import"),
        "the capsule markdown is missing from the seed"
    );
    assert!(
        grant.prompt.contains("ORIGIN-TAIL-REMOTE-7f3"),
        "the bundle tail is missing from the seed"
    );
    assert!(
        grant.prompt.ends_with("pick up the remote import"),
        "the first turn's input is not the seed's next message"
    );

    let events = read_events(&remote, &child);
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload))
                if payload.note.as_deref() == Some("handoff")
                    && payload.text.contains("ORIGIN-TAIL-REMOTE-7f3")
        )),
        "the log does not show the seeded prompt the executor received"
    );
    assert!(
        !remote.paths.session_dir(&child).join("import.json").exists(),
        "the import sidecar was not consumed by the first remote turn"
    );
    let store = remote.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.native_session_id, None);
}

#[test]
fn native_session_ids_are_scoped_to_the_run_node() {
    let tmp = tempdir().expect("tempdir");
    let deps = resumable_deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "node scoped native");
    let store = deps.open_store().expect("store");
    store
        .upsert_native(&session, HarnessKind::Opencode, "home-native", &deps.node, 1)
        .expect("home native");

    // The home has a native id, but this turn executes on the laptop. It must not inherit it.
    let laptop_first = begin(&deps, &session, "lease", "first laptop turn");
    assert_eq!(
        laptop_first.resume_native, None,
        "a remote turn inherited the home node's native session id"
    );

    store
        .upsert_native(&session, HarnessKind::Opencode, "laptop-native", &laptop(), 2)
        .expect("laptop native");
    let laptop = begin(&deps, &session, "lease", "second laptop turn");
    assert_eq!(
        laptop.resume_native.as_deref(),
        Some("laptop-native"),
        "the laptop did not resume the native id recorded on the laptop"
    );
    assert_eq!(laptop.native_hint, None, "a resumed turn asked for a fresh native id");

    let home = begin_on(&deps, &session, "lease", "home turn", deps.node.clone(), None);
    assert_eq!(
        home.resume_native.as_deref(),
        Some("home-native"),
        "the home did not resume the native id recorded on the home"
    );
    let home_run = store.require_run(&home.run_id).expect("home run");
    assert_eq!(home_run.native_session_id.as_deref(), Some("home-native"));

    // A harness whose adapter cannot resume native must not be asked to, even when a
    // matching id exists for the node; it starts fresh instead.
    let mut non_resumable = deps.clone();
    non_resumable.adapters = Arc::new(AdapterRegistry::new());
    let fresh = begin(&non_resumable, &session, "lease", "no native resume");
    assert_eq!(
        fresh.resume_native, None,
        "a non-resumable harness was asked to resume a native session"
    );
    assert!(fresh.native_hint.is_some(), "the fresh start got no native id hint");
}

#[test]
fn begin_turn_uses_the_surface_cwd_without_probing_it_on_the_home() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "surface cwd");
    // This path exists only on the surface; the home must not reject it for being absent.
    let surface = tmp.path().join("surface-checkout");
    assert!(!surface.exists());
    let cwd = surface.display().to_string();

    let grant = begin_on(
        &deps,
        &session,
        "lease",
        "run in the surface checkout",
        laptop(),
        Some(cwd.clone()),
    );
    assert_eq!(grant.cwd, cwd);

    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.cwd.as_deref(), Some(cwd.as_str()));
    let row = store.require_session(&session).expect("session");
    assert_ne!(row.cwd, cwd, "the session metadata keeps the home's checkout");
}

#[test]
fn ingest_rejects_unknown_or_mismatched_runs_before_events() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "run owner");
    let grant = begin(&deps, &session, "lease", "prompt");
    let other = create_session(&service, &tmp, "other session");
    let store = deps.open_store().expect("store");

    let unknown = IngestGrant {
        session: session.clone(),
        lease: "lease".to_owned(),
        run: RunId::new(),
    };
    let mut reader = Cursor::new(Vec::<u8>::new());
    let mut ack = Vec::new();
    let error = ingest_lines(&deps, &store, &unknown, &mut reader, &mut ack)
        .expect_err("an unknown run must be refused before ingest starts");
    assert!(error.to_string().contains("run"), "unexpected error: {error}");

    let mismatched = IngestGrant {
        session: other,
        lease: "lease".to_owned(),
        run: grant.run_id.clone(),
    };
    let mut reader = Cursor::new(Vec::<u8>::new());
    let mut ack = Vec::new();
    let error = ingest_lines(&deps, &store, &mismatched, &mut reader, &mut ack)
        .expect_err("a run from another session must be refused before ingest starts");
    assert!(
        error.to_string().contains("does not belong"),
        "unexpected error: {error}"
    );
}

#[test]
fn remote_exec_streams_events_and_home_assigns_seq() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "stream");
    let grant = begin(&deps, &session, "lease", "prompt");
    let first = EventId::new();
    let second = EventId::new();
    let text = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Event {
            event: remote_event(
                first.clone(),
                EventBody::AssistantMessage(AssistantMessage { text: "one".to_owned() }),
            ),
        },
        RemoteLine::Event {
            event: remote_event(
                second.clone(),
                EventBody::AssistantMessage(AssistantMessage { text: "two".to_owned() }),
            ),
        },
        summary(Cost::unknown()),
    ]);
    let (result, acks) = ingest(&deps, &grant, "lease", &text);
    let report = result.expect("ingest");
    assert_eq!(report.events, 2);
    assert_eq!(report.duplicates, 0);
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Ack { seq: 5, .. }));
    assert!(matches!(acks[1], IngestAck::Ack { seq: 6, .. }));
    assert!(matches!(acks[2], IngestAck::Summary { done: true, .. }));

    let events = read_events(&deps, &session);
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(events[4].id, first);
    assert_eq!(events[5].id, second);
    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert!(run.ended_at.is_some());
}

#[test]
fn remote_ingest_is_idempotent_by_event_id() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "idempotent");
    let grant = begin(&deps, &session, "lease", "prompt");
    let id = EventId::new();
    let event = RemoteLine::Event {
        event: remote_event(
            id.clone(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "once".to_owned(),
            }),
        ),
    };
    let text = lines(&[hello(&grant, "lease"), event.clone(), event, summary(Cost::unknown())]);
    let (result, acks) = ingest(&deps, &grant, "lease", &text);
    let report = result.expect("ingest");
    assert_eq!(report.events, 1);
    assert_eq!(report.duplicates, 1);
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Ack { seq: 5, .. }));
    assert!(matches!(acks[1], IngestAck::Duplicate { seq: 5, .. }));
    let events = read_events(&deps, &session);
    assert_eq!(
        events.iter().filter(|event| event.id == id).count(),
        1,
        "the duplicate id was appended twice"
    );
}

#[test]
fn remote_ingest_rejects_bad_lease() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "lease");
    let grant = begin(&deps, &session, "the-right-token", "prompt");
    let event = RemoteLine::Event {
        event: remote_event(
            EventId::new(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "should not land".to_owned(),
            }),
        ),
    };
    let text = lines(&[hello(&grant, "the-wrong-token"), event, summary(Cost::unknown())]);
    let (result, _) = ingest(&deps, &grant, "the-right-token", &text);
    let error = result.expect_err("bad lease must be rejected");
    assert!(error.to_string().contains("lease"), "unexpected error: {error}");
    let events = read_events(&deps, &session);
    assert!(
        !events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::AssistantMessage(payload)) if payload.text == "should not land"
        )),
        "a rejected lease still appended an event"
    );
}

#[test]
fn remote_summary_finishes_run_and_terminal_events() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "summary");
    let grant = begin(&deps, &session, "lease", "prompt");
    let cost = Cost::receipt(
        0.5,
        Tokens {
            input: 10,
            output: 5,
            ..Tokens::default()
        },
    );
    let limit = LimitEvent {
        provider: "deepseek".to_owned(),
        kind: LimitKind::Rate,
        message: "slow down".to_owned(),
        resets_at: None,
        http_status: Some(429),
    };
    let text = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Summary {
            exit_code: Some(0),
            outcome: RunOutcome::Limit,
            native_session_id: Some("nat-1".to_owned()),
            cost: cost.clone(),
            limit: Some(limit),
            result_text: Some("limited".to_owned()),
            stop_reason: StopReason::Limit,
            usage: None,
        },
    ]);
    let (result, acks) = ingest(&deps, &grant, "lease", &text);
    let report = result.expect("ingest");
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Summary { done: true, .. }));

    let events = read_events(&deps, &session);
    let types: Vec<EventType> = events.iter().map(|event| event.event_type).collect();
    assert!(types.contains(&EventType::Cost));
    assert!(types.contains(&EventType::Limit));
    assert_eq!(
        types
            .iter()
            .filter(|event_type| **event_type == EventType::TurnEnded)
            .count(),
        1
    );
    let detach = events
        .iter()
        .rev()
        .find_map(|event| match event.body() {
            Ok(EventBody::SessionExecutor(payload)) => Some(payload),
            _ => None,
        })
        .expect("detach");
    assert_eq!(detach.action, vowp_protocol::AttachAction::Detach);
    assert_eq!(detach.reason.as_deref(), Some("limit"));

    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(RunOutcome::Limit));
    assert_eq!(run.cost.usd, Some(0.5));
    let row = store.require_session(&session).expect("session");
    assert_eq!(row.state, vowp_protocol::SessionState::Blocked);
    assert!(
        store
            .active_limits(ts())
            .expect("limits")
            .iter()
            .any(|row| row.provider == "deepseek")
    );
    let usage = store
        .run_usage(&grant.run_id)
        .expect("read run_usage")
        .expect("a summary without measurements still writes the default row");
    assert_eq!(usage.source, UsageSource::Default);
    assert_eq!(usage.exit_code, Some(0));
}

#[test]
fn remote_streamed_cost_is_not_duplicated_by_summary() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "streamed cost");
    let grant = begin(&deps, &session, "lease", "prompt");
    let cost = Cost::receipt(
        0.25,
        Tokens {
            input: 10,
            output: 5,
            ..Tokens::default()
        },
    );
    // Both real adapters emit `CostEvent { run_id: None }`; the summary carries the same
    // receipt. The home must count it once.
    let text = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Event {
            event: remote_event(
                EventId::new(),
                EventBody::Cost(CostEvent {
                    cost: cost.clone(),
                    run_id: None,
                }),
            ),
        },
        RemoteLine::Summary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: None,
            cost: cost.clone(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
            usage: None,
        },
    ]);
    let (result, _) = ingest(&deps, &grant, "lease", &text);
    result.expect("ingest");

    let events = read_events(&deps, &session);
    let costs: Vec<_> = events
        .iter()
        .filter(|event| event.event_type == EventType::Cost)
        .collect();
    assert_eq!(
        costs.len(),
        1,
        "a streamed cost plus the summary cost appended two cost events"
    );
    let payload = match costs[0].body() {
        Ok(EventBody::Cost(payload)) => payload,
        other => panic!("cost event has the wrong body: {other:?}"),
    };
    assert_eq!(payload.cost.usd, Some(0.25));
    assert_eq!(payload.cost.tokens.input, 10);
    assert_eq!(payload.cost.tokens.output, 5);
    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.cost.usd, Some(0.25));
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
}

#[test]
fn remote_summary_replay_finishes_an_open_run_with_terminal_events() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "crash before finish");
    let grant = begin(&deps, &session, "lease", "prompt");

    // Simulate a home crash after the summary appended `turn.ended` but before
    // `finish_run`: the log has the terminal event, the run row is still open.
    let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("open log");
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: Some("deepseek/deepseek-flash".to_owned()),
                node: Some(laptop()),
                native_session_id: None,
            },
            body: EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: StopReason::EndTurn,
                duration_ms: Some(5),
            }),
            raw: None,
            ts: Some(ts()),
        })
        .expect("seed terminal event");
    let store = deps.open_store().expect("store");
    store.index_event(&index_row(&appended)).expect("index terminal");
    log.sync().expect("sync");
    drop(log);
    let open = store.require_run(&grant.run_id).expect("run");
    assert!(open.ended_at.is_none(), "the seeded run must still be open");

    let text = lines(&[hello(&grant, "lease"), summary(Cost::unknown())]);
    let (result, acks) = ingest(&deps, &grant, "lease", &text);
    let report = result.expect("replayed summary");
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Summary { done: true, .. }));

    let events = read_events(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1,
        "the replay appended a second turn.ended"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(payload))
                    if payload.action == vowp_protocol::AttachAction::Detach
                        && payload.run_id.as_ref() == Some(&grant.run_id)
            ))
            .count(),
        1,
        "the replay appended a second detach"
    );
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    let store = deps.open_store().expect("store");
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert!(run.ended_at.is_some());
    let row = store.require_session(&session).expect("session");
    assert_eq!(row.state, vowp_protocol::SessionState::Idle);

    // A second replay after the repaired finish is still a no-op.
    ingest(&deps, &grant, "lease", &text).0.expect("second replay");
    let events = read_events(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1
    );
    let ledger = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Cost),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(
        ledger
            .iter()
            .filter(|row| row.run_id.as_ref() == Some(&grant.run_id))
            .count(),
        1,
        "the summary replay wrote a second cost ledger row"
    );
}

#[test]
fn remote_summary_writes_run_usage() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "usage");
    let grant = begin(&deps, &session, "lease", "prompt");
    let usage = RemoteUsage {
        peak_rss_bytes: 123_456,
        anon_peak_bytes: Some(100_000),
        file_peak_bytes: Some(23_456),
        kernel_peak_bytes: None,
        cpu_usec: 9_000,
        mean_cpu_cores: 1.5,
        max_cpu_cores: 2.0,
        build_mode: BuildMode::Warm,
        oom_killed: false,
        source: UsageSource::ProcTree,
    };
    let text = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Summary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
            usage: Some(usage.clone()),
        },
    ]);
    ingest(&deps, &grant, "lease", &text).0.expect("ingest");

    let store = deps.open_store().expect("store");
    let rows = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Cost),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    let row = rows
        .iter()
        .find(|row| row.run_id.as_ref() == Some(&grant.run_id))
        .expect("cost ledger row");
    let recorded = &row.detail["usage"];
    assert_eq!(recorded["peak_rss_bytes"], usage.peak_rss_bytes);
    assert_eq!(recorded["cpu_usec"], usage.cpu_usec);
    assert_eq!(recorded["source"], "proc_tree");

    // The store row is the observable result: the remote summary writes a real row.
    let typed = store
        .run_usage(&grant.run_id)
        .expect("read run_usage")
        .expect("run_usage row");
    assert_eq!(typed.run_id, grant.run_id);
    assert_eq!(typed.project, deps.config.project.name);
    assert_eq!(typed.role, vowp_protocol::RunRole::Worker);
    assert_eq!(typed.harness, Some(HarnessKind::Opencode));
    assert_eq!(typed.model.as_deref(), Some("deepseek/deepseek-flash"));
    assert_eq!(typed.provider.as_deref(), Some("deepseek"));
    assert_eq!(typed.node, laptop());
    assert_eq!(typed.peak_rss_bytes, usage.peak_rss_bytes);
    assert_eq!(typed.anon_peak_bytes, usage.anon_peak_bytes);
    assert_eq!(typed.file_peak_bytes, usage.file_peak_bytes);
    assert_eq!(typed.cpu_usec, usage.cpu_usec);
    assert_eq!(typed.mean_cpu_cores, usage.mean_cpu_cores);
    assert_eq!(typed.max_cpu_cores, usage.max_cpu_cores);
    assert_eq!(typed.build_mode, usage.build_mode);
    assert_eq!(typed.source, usage.source);
    assert_eq!(typed.exit_code, Some(0));
}

#[test]
fn remote_summary_duplicate_is_idempotent() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "summary twice");
    let grant = begin(&deps, &session, "lease", "prompt");
    let text = lines(&[hello(&grant, "lease"), summary(Cost::unknown())]);
    ingest(&deps, &grant, "lease", &text).0.expect("first ingest");
    let (second, acks) = ingest(&deps, &grant, "lease", &text);
    let report = second.expect("second ingest");
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Summary { done: true, .. }));
    let events = read_events(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1,
        "a replayed summary appended a second terminal event"
    );
}

#[test]
fn duplicate_id_content_conflict_is_not_appended() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "conflict");
    let grant = begin(&deps, &session, "lease", "prompt");
    let id = EventId::new();
    let first = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Event {
            event: remote_event(
                id.clone(),
                EventBody::AssistantMessage(AssistantMessage {
                    text: "original".to_owned(),
                }),
            ),
        },
    ]);
    ingest(&deps, &grant, "lease", &first).0.expect("first event");

    let changed = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Event {
            event: remote_event(
                id.clone(),
                EventBody::AssistantMessage(AssistantMessage {
                    text: "changed".to_owned(),
                }),
            ),
        },
    ]);
    let (result, acks) = ingest(&deps, &grant, "lease", &changed);
    let error = result.expect_err("content conflict must fail");
    assert!(error.to_string().contains("duplicate-conflict"), "{error}");
    assert!(matches!(
        acks.last(),
        Some(IngestAck::Error {
            kind: vowp_protocol::ErrorKind::Conflict,
            ..
        })
    ));
    let events = read_events(&deps, &session);
    assert_eq!(events.iter().filter(|event| event.id == id).count(), 1);
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::AssistantMessage(payload)) if payload.text == "original"
    )));
}

#[test]
fn home_ingest_replays_after_ack_loss_without_gap_or_duplicate() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "drop");
    let grant = begin(&deps, &session, "lease", "prompt");
    let first_id = EventId::new();
    let second_id = EventId::new();
    let first = RemoteLine::Event {
        event: remote_event(
            first_id.clone(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "first".to_owned(),
            }),
        ),
    };
    let second = RemoteLine::Event {
        event: remote_event(
            second_id.clone(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "second".to_owned(),
            }),
        ),
    };

    // Connection 1: the hello and the first event arrive, then ssh dies before the summary.
    let cut = lines(&[hello(&grant, "lease"), first.clone()]);
    let (first_pass, _) = ingest(&deps, &grant, "lease", &cut);
    let first_report = first_pass.expect("first pass");
    assert_eq!(first_report.events, 1);
    assert!(!first_report.done);

    // Connection 2: the client replays from its spool, including the already-written event.
    let replay = lines(&[hello(&grant, "lease"), first, second, summary(Cost::unknown())]);
    let (second_pass, acks) = ingest(&deps, &grant, "lease", &replay);
    let report = second_pass.expect("replay");
    assert_eq!(report.events, 1);
    assert_eq!(report.duplicates, 1);
    assert!(report.done);
    assert!(matches!(acks[0], IngestAck::Duplicate { seq: 5, .. }));
    assert!(matches!(acks[1], IngestAck::Ack { seq: 6, .. }));

    let events = read_events(&deps, &session);
    let ids: Vec<_> = events
        .iter()
        .map(|event| event.id.clone())
        .filter(|id| id == &first_id || id == &second_id)
        .collect();
    assert_eq!(ids, vec![first_id, second_id]);
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
}

#[test]
fn home_ingest_mid_line_drop_has_no_gap() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "mid line");
    let grant = begin(&deps, &session, "lease", "prompt");
    let first = RemoteLine::Event {
        event: remote_event(
            EventId::new(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "first".to_owned(),
            }),
        ),
    };
    let second = RemoteLine::Event {
        event: remote_event(
            EventId::new(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "second".to_owned(),
            }),
        ),
    };
    let cut = format!("{}{{", lines(&[hello(&grant, "lease"), first.clone()]));
    let (first_pass, _) = ingest(&deps, &grant, "lease", &cut);
    let report = first_pass.expect("partial line is ignored");
    assert_eq!(report.events, 1);
    assert!(!report.done);

    let replay = lines(&[hello(&grant, "lease"), first, second, summary(Cost::unknown())]);
    let (second_pass, _) = ingest(&deps, &grant, "lease", &replay);
    let report = second_pass.expect("replay");
    assert_eq!(report.events, 1);
    assert_eq!(report.duplicates, 1);
    assert!(report.done);
    let events = read_events(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::AssistantMessage)
            .count(),
        2
    );
}

#[test]
fn remote_stream_truncates_torn_tail() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "torn tail");
    let grant = begin(&deps, &session, "lease", "prompt");
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(deps.paths.log_path(&session))
            .expect("log");
        file.write_all(b"{\"torn\":").expect("torn tail");
    }
    let text = lines(&[
        hello(&grant, "lease"),
        RemoteLine::Event {
            event: remote_event(
                EventId::new(),
                EventBody::AssistantMessage(AssistantMessage {
                    text: "after repair".to_owned(),
                }),
            ),
        },
        summary(Cost::unknown()),
    ]);
    ingest(&deps, &grant, "lease", &text).0.expect("ingest after torn tail");
    let events = read_events(&deps, &session);
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::AssistantMessage(payload)) if payload.text == "after repair"
    )));
}

#[test]
fn remote_stream_large_events() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "large");
    let grant = begin(&deps, &session, "lease", "prompt");
    let output = "x".repeat(20_000);
    let large = RemoteLine::Event {
        event: remote_event(
            EventId::new(),
            EventBody::ToolResult(ToolResult {
                call_id: "call-1".to_owned(),
                ok: true,
                output: Some(output),
                output_ref: None,
                exit_code: Some(0),
            }),
        ),
    };
    let text = lines(&[hello(&grant, "lease"), large.clone(), large, summary(Cost::unknown())]);
    let (result, _) = ingest(&deps, &grant, "lease", &text);
    let report = result.expect("large event ingest");
    assert_eq!(report.events, 1);
    assert_eq!(report.duplicates, 1, "a spilled large event must dedupe by id");
    let events = read_events(&deps, &session);
    let spilled = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::ToolResult(payload)) => Some(payload),
            _ => None,
        })
        .expect("tool result");
    assert!(spilled.output.is_none());
    assert!(spilled.output_ref.is_some(), "the large output was not spilled");
}

#[test]
fn remote_stream_requires_hello_before_events() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "no hello");
    let grant = begin(&deps, &session, "lease", "prompt");
    let text = lines(&[RemoteLine::Event {
        event: remote_event(
            EventId::new(),
            EventBody::AssistantMessage(AssistantMessage {
                text: "must not land".to_owned(),
            }),
        ),
    }]);
    let (result, _) = ingest(&deps, &grant, "lease", &text);
    let error = result.expect_err("an event without hello must be rejected");
    assert!(error.to_string().contains("hello"), "unexpected error: {error}");
    let events = read_events(&deps, &session);
    assert!(
        !events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::AssistantMessage(payload)) if payload.text == "must not land"
        )),
        "an event without hello reached the log"
    );
}
