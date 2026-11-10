//! `SessionService::run_turn` end-to-end tests, hermetic: temp node, fake clock,
//! recorded events through the real adapter contract and no harness process.
//! Spec: `docs/design/crates/vowp-core.md` §3 named tests.

mod support;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_adapters::claude_code::ClaudeParser;
use vowp_adapters::opencode::OpencodeParser;
use vowp_adapters::stream::StreamParser;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, CoreError, Deps, FixedRng};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, AttachAction, CancelToken, Cost,
    CostEvent, EventBody, EventEnvelope, EventType, Executor, ExecutorRef, FakeClock, HarnessEvent, HarnessKind,
    LimitEvent, LimitKind, MemoryLog, NodeName, ProbeResult, RunOutcome, RunPurpose, RunRole, RunSummary, ServiceError,
    SessionExecutor, SessionId, SessionOps, SessionRole, StartInput, StopReason, Timestamp, Tokens, ToolResult,
    TurnEnded, TurnRequest,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{NodePaths, StoreError};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn claude() -> Executor {
    Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude executor")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, config: vowp_protocol::VowpConfig, registry: AdapterRegistry) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config),
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

fn read_events(deps: &Deps, session: &SessionId) -> Vec<EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

fn turn(session: &SessionId, executor: Executor, prompt: &str) -> TurnRequest {
    TurnRequest {
        session_id: session.clone(),
        executor: ExecutorRef { executor, node: None },
        purpose: RunPurpose::Start,
        role: RunRole::Worker,
        job_id: None,
        prompt: prompt.to_owned(),
        profile: Some("worker".to_owned()),
        rung: 0,
        window: None,
        timeout: None,
    }
}

fn body_types(events: &[EventEnvelope]) -> Vec<EventType> {
    events.iter().map(|event| event.event_type).collect()
}

#[test]
fn run_turn_appends_events_and_finishes_run() {
    let tmp = tempdir().expect("tempdir");
    let events = vec![
        body_event(EventBody::AssistantMessage(AssistantMessage {
            text: "all done".to_owned(),
        })),
        body_event(EventBody::Cost(CostEvent {
            cost: Cost::receipt(
                0.25,
                Tokens {
                    input: 10,
                    output: 5,
                    ..Tokens::default()
                },
            ),
            run_id: None,
        })),
    ];
    let fake = FakeAdapter::new(HarnessKind::Opencode, events).with_summary(RunSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: None,
        cost: Cost::receipt(
            0.25,
            Tokens {
                input: 10,
                output: 5,
                ..Tokens::default()
            },
        ),
        limit: None,
        result_text: Some("all done".to_owned()),
        stop_reason: StopReason::EndTurn,
    });
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "run");
    let result = service
        .run_turn(turn(&session, opencode(), "do the work"), &CancelToken::default())
        .expect("run turn");

    assert_eq!(result.summary.outcome, RunOutcome::Ok);
    let events = read_events(&deps, &session);
    let types = body_types(&events);
    assert_eq!(types[0], EventType::SessionCreated);
    assert_eq!(
        &types[1..4],
        &[
            EventType::SessionExecutor,
            EventType::TurnStarted,
            EventType::UserMessage
        ]
    );
    assert_eq!(types.last(), Some(&EventType::SessionExecutor));
    assert_eq!(
        types
            .iter()
            .filter(|event_type| **event_type == EventType::TurnEnded)
            .count(),
        1
    );
    assert_eq!(
        types
            .iter()
            .filter(|event_type| **event_type == EventType::Cost)
            .count(),
        1,
        "the adapter's receipt is forwarded once, not duplicated"
    );
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::AssistantMessage(payload)) if payload.text == "all done"
    )));

    let store = deps.open_store().expect("store");
    let run = store.require_run(&result.run_id).expect("run row");
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert!(run.ended_at.is_some());
    assert_eq!(run.cost.usd, Some(0.25));
    let session_row = store.require_session(&session).expect("session row");
    assert_eq!(session_row.state, vowp_protocol::SessionState::Idle);
    assert_eq!(session_row.last_seq, events.last().expect("event").seq);
}

#[test]
fn run_turn_frames_attach_then_turn_started_once_per_run_without_adapter_duplicates() {
    for (kind, model, events) in [
        (HarnessKind::ClaudeCode, "claude-sonnet-5-5", parse_claude_fixture()),
        (
            HarnessKind::Opencode,
            "deepseek/deepseek-flash",
            parse_opencode_fixture(),
        ),
    ] {
        let tmp = tempdir().expect("tempdir");
        let executor = Executor {
            harness: kind,
            model: model.to_owned(),
            effort: None,
            variant: None,
        };
        let key = match kind {
            HarnessKind::ClaudeCode => "claude-code",
            _ => "opencode",
        };
        let fake = FakeAdapter::new(kind, events);
        let deps = deps(
            &tmp,
            config_for(tmp.path()),
            AdapterRegistry::new().with(key, Arc::new(fake)),
        );
        let service = SessionService::new(deps.clone());
        let session = create_session(&service, &tmp, "frames");
        service
            .run_turn(turn(&session, executor, "frame this"), &CancelToken::default())
            .expect("run turn");

        let events = read_events(&deps, &session);
        let attaches: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                matches!(
                    event.body(),
                    Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Attach
                )
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(attaches.len(), 1, "exactly one attach per run");
        let started: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_type == EventType::TurnStarted)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(started.len(), 1, "exactly one turn.started per run");
        let users: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_type == EventType::UserMessage)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(users.len(), 1, "exactly one user.message per run");
        assert!(attaches[0] < started[0] && started[0] < users[0]);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == EventType::TurnEnded)
                .count(),
            1
        );
    }
}

#[test]
fn limit_resets_at_gets_single_two_minute_margin() {
    let tmp = tempdir().expect("tempdir");
    let limit = LimitEvent {
        kind: LimitKind::Quota,
        provider: "claude".to_owned(),
        resets_at: Some(Timestamp::from_unix_secs(1_794_164_400)),
        message: "Claude AI usage limit reached|1794164400".to_owned(),
        http_status: None,
    };
    let fake = FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new()).with_summary(RunSummary {
        exit_code: Some(1),
        outcome: RunOutcome::Limit,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: Some(limit),
        result_text: None,
        stop_reason: StopReason::Limit,
    });
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("claude-code", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "limits");
    let store = deps.open_store().expect("store");

    for _ in 0..2 {
        let result = service
            .run_turn(turn(&session, claude(), "work"), &CancelToken::default())
            .expect("limit outcome is returned, not raised");
        assert_eq!(result.summary.outcome, RunOutcome::Limit);
        let row = store.limit_for("claude").expect("limits row").expect("row");
        assert_eq!(
            row.resets_at,
            Some(Timestamp::parse("2026-11-08T19:02:00Z").expect("reset")),
            "exactly one two-minute margin, even after a second run"
        );
    }
}

#[test]
fn run_turn_resumes_native_session_same_harness() {
    let tmp = tempdir().expect("tempdir");
    let fake = Arc::new(NativeAdapter::new("ses_native_1"));
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", fake.clone()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "resume");
    let store = deps.open_store().expect("store");
    store
        .upsert_native(&session, HarnessKind::Opencode, "ses_native_1", &deps.node, 1)
        .expect("native row");

    service
        .run_turn(turn(&session, opencode(), "continue"), &CancelToken::default())
        .expect("run turn");

    let starts = fake.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].resume.as_deref(), Some("ses_native_1"));
    assert_eq!(starts[0].prompt, "continue");
    let events = read_events(&deps, &session);
    let attach = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Attach => Some(payload),
            _ => None,
        })
        .expect("attach");
    assert_eq!(attach.native_session_id.as_deref(), Some("ses_native_1"));
}

#[test]
fn fresh_session_gets_new_native_id_from_rng() {
    let tmp = tempdir().expect("tempdir");
    let fake = Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new()));
    let mut deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", fake.clone()),
    );
    deps.rng = Arc::new(FixedRng(0x0123_4567_89ab_cdef));
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "fresh native id");

    service
        .run_turn(turn(&session, opencode(), "start fresh"), &CancelToken::default())
        .expect("run turn");

    let starts = fake.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].resume, None, "a fresh session has nothing to resume");
    let native = starts[0]
        .new_native_id
        .as_deref()
        .expect("a fresh session pre-chooses a native id");
    assert_eq!(
        native, "01234567-89ab-4def-8123-456789abcdef",
        "the id is a uuid-v4 built from the injected Rng"
    );
    assert_eq!(native.as_bytes()[14], b'4', "uuid version 4");
    assert!(
        matches!(native.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
        "RFC 4122 variant"
    );
}

#[test]
fn run_turn_seeds_capsule_on_new_harness() {
    let tmp = tempdir().expect("tempdir");
    let first = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: "opencode did the first step".to_owned(),
        })],
    );
    let claude_fake = Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new()));
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new()
            .with("opencode", Arc::new(first))
            .with("claude-code", claude_fake.clone()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "seed");
    service
        .run_turn(turn(&session, opencode(), "first"), &CancelToken::default())
        .expect("first turn");
    let store = deps.open_store().expect("store");
    let events = read_events(&deps, &session);
    let capsule = vowp_protocol::Capsule {
        version: vowp_protocol::CAPSULE_VERSION,
        capsule_id: vowp_protocol::CapsuleId::new(),
        session_id: session.clone(),
        created_at: ts(),
        at_seq: events.last().expect("event").seq,
        trigger: vowp_protocol::CapsuleTrigger::TurnEnd,
        from: ExecutorRef {
            executor: opencode(),
            node: Some(deps.node.clone()),
        },
        intent: "Finish the session service".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: vowp_protocol::CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: Default::default(),
    };
    store.insert_capsule(&capsule, "capsule.json").expect("capsule row");

    service
        .run_turn(turn(&session, claude(), "next message"), &CancelToken::default())
        .expect("second turn");

    let starts = claude_fake.starts();
    assert_eq!(starts.len(), 1);
    assert!(
        starts[0]
            .prompt
            .starts_with("[VOWP handoff] You are continuing session")
    );
    assert!(starts[0].prompt.contains("Finish the session service"));
    assert!(starts[0].prompt.contains("## Next message\nnext message"));
    assert_eq!(starts[0].resume, None);
    let events = read_events(&deps, &session);
    let handoff = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::UserMessage(payload)) if payload.note.as_deref() == Some("handoff") => Some(payload),
            _ => None,
        })
        .expect("handoff user message");
    assert_eq!(handoff.text, starts[0].prompt);
}

#[test]
fn run_turn_cwd_outside_allow_list_refused() {
    let tmp = tempdir().expect("tempdir");
    let fake = Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new()));
    let config = config_for(tmp.path());
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    let deps = deps(
        &tmp,
        config.clone(),
        AdapterRegistry::new().with("opencode", fake.clone()),
    );
    let service = SessionService::new(deps.clone());
    let session = service
        .create_session(vowp_protocol::NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: "outside".to_owned(),
            cwd: outside.clone(),
            parent: None,
            executor: Some(opencode()),
        })
        .expect("create session");

    let error = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("outside cwd must be refused");
    assert!(matches!(error, CoreError::Refused(_)));
    assert!(fake.starts().is_empty());
    let store = deps.open_store().expect("store");
    assert!(store.open_runs().expect("runs").is_empty());
}

#[test]
fn run_turn_cancel_interrupts_adapter() {
    let tmp = tempdir().expect("tempdir");
    let (started_tx, started_rx) = channel();
    let blocking = BlockingAdapter::new(started_tx);
    let interrupts = blocking.interrupts.clone();
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(blocking)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "cancel");
    let cancel = CancelToken::default();
    let canceller = cancel.clone();
    let handle = std::thread::spawn(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("adapter started");
        canceller.cancel();
    });

    let result = service
        .run_turn(turn(&session, opencode(), "hang"), &cancel)
        .expect("interrupted turn still returns a summary");
    handle.join().expect("canceller");

    assert!(interrupts.load(Ordering::SeqCst), "interrupt() was called");
    assert_eq!(result.summary.outcome, RunOutcome::Killed);
    assert_eq!(result.summary.stop_reason, StopReason::Interrupted);
    let events = read_events(&deps, &session);
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::TurnEnded(payload)) if payload.stop_reason == StopReason::Interrupted
    )));
}

#[test]
fn run_turn_limit_outcome_returned_not_raised_and_limits_row_written() {
    let tmp = tempdir().expect("tempdir");
    let limit = LimitEvent {
        kind: LimitKind::Quota,
        provider: "claude".to_owned(),
        resets_at: Some(Timestamp::from_unix_secs(1_794_164_400)),
        message: "limit".to_owned(),
        http_status: None,
    };
    let fake = FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new()).with_summary(RunSummary {
        exit_code: Some(1),
        outcome: RunOutcome::Limit,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: Some(limit),
        result_text: None,
        stop_reason: StopReason::Limit,
    });
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("claude-code", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "limit outcome");
    let result = service
        .run_turn(turn(&session, claude(), "work"), &CancelToken::default())
        .expect("limit is an outcome, not a service error");
    assert_eq!(result.summary.outcome, RunOutcome::Limit);
    let store = deps.open_store().expect("store");
    assert!(store.limit_for("claude").expect("limit").is_some());
    assert_eq!(
        store.require_session(&session).expect("session").state,
        vowp_protocol::SessionState::Blocked
    );
    assert_eq!(
        store.require_run(&result.run_id).expect("run").outcome,
        Some(RunOutcome::Limit)
    );
}

#[test]
fn run_turn_large_tool_output_spilled_to_blob() {
    let tmp = tempdir().expect("tempdir");
    let output = "x".repeat(8193);
    let fake = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::ToolResult(ToolResult {
            call_id: "call_1".to_owned(),
            ok: true,
            output: Some(output.clone()),
            output_ref: None,
            exit_code: Some(0),
        })],
    );
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "blob");
    service
        .run_turn(turn(&session, opencode(), "tool"), &CancelToken::default())
        .expect("run turn");

    let events = read_events(&deps, &session);
    let (reference, inline) = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::ToolResult(payload)) => Some((payload.output_ref, payload.output)),
            _ => None,
        })
        .expect("tool result");
    assert!(inline.is_none(), "8193 bytes must not stay inline");
    let reference = reference.expect("blob ref");
    assert_eq!(reference.bytes, 8193);
    let reader = SessionLog::open_read(&deps.paths, &session).expect("reader");
    let bytes = reader.blobs().get(&reference.sha256).expect("blob");
    assert_eq!(bytes, output.as_bytes());
}

#[test]
fn second_writer_on_same_session_conflicts() {
    let tmp = tempdir().expect("tempdir");
    let fake = FakeAdapter::new(HarnessKind::Opencode, Vec::new());
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "lock");
    let held = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("hold lock");

    let error = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("second writer must conflict");
    match error {
        CoreError::Store(StoreError::Locked { .. }) => {}
        other => panic!("expected Locked, got {other:?}"),
    }
    drop(held);
    service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect("lock released");
}

#[test]
fn session_switch_writes_capsule_and_lineage() {
    let tmp = tempdir().expect("tempdir");
    let draft = serde_json::json!({
        "intent": "Switch cleanly",
        "state": {"done": [], "in_progress": [], "blocked": [], "next_steps": []},
        "files": [],
        "decisions": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string();
    let capsule_adapter = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage { text: draft })],
    );
    let target = FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new());
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter))
            .with("claude-code", Arc::new(target)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "switch");
    let request = vowp_protocol::SwitchRequest {
        session_id: session.clone(),
        to: ExecutorRef {
            executor: claude(),
            node: None,
        },
        reason: vowp_protocol::SwitchReason::User,
        drain_secs: 5,
    };
    let switch = service.switch(&request).expect("switch");
    assert_eq!(switch.state, vowp_protocol::SwitchState::Completed);
    assert!(switch.capsule_id.is_some());
    let store = deps.open_store().expect("store");
    assert_eq!(
        store.switch(&switch.id).expect("row").expect("switch").state,
        vowp_protocol::SwitchState::Completed
    );
    assert_eq!(
        store.require_session(&session).expect("session").executor,
        Some(claude())
    );
    let events = read_events(&deps, &session);
    assert!(events.iter().any(|event| event.event_type == EventType::Capsule));
    let lineage = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::Switch(payload)) => Some(payload),
            _ => None,
        })
        .expect("switch event");
    assert_eq!(lineage.switch_id, switch.id);
    assert_eq!(lineage.reason, vowp_protocol::SwitchReason::User);
    assert_eq!(lineage.to.executor, claude());
}

#[test]
fn run_turn_cancel_interrupts_streaming_adapter() {
    let tmp = tempdir().expect("tempdir");
    let (started_tx, started_rx) = channel();
    let streaming = StreamingAdapter::new(started_tx);
    let interrupts = streaming.interrupts.clone();
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(streaming)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "streaming cancel");
    let cancel = CancelToken::default();
    let canceller = cancel.clone();
    let handle = std::thread::spawn(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("adapter started");
        std::thread::sleep(Duration::from_millis(30));
        canceller.cancel();
    });

    let (done_tx, done_rx) = channel();
    let worker = {
        let service = service.clone();
        let session = session.clone();
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let result = service.run_turn(turn(&session, opencode(), "stream"), &cancel);
            let _ = done_tx.send(result);
        })
    };
    let result = done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("a streaming run must return after cancellation")
        .expect("interrupted turn returns a summary");
    worker.join().expect("worker");
    handle.join().expect("canceller");

    assert!(interrupts.load(Ordering::SeqCst), "interrupt() was called");
    assert_eq!(result.summary.outcome, RunOutcome::Killed);
    let events = read_events(&deps, &session);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.body(), Ok(EventBody::AssistantMessage(_)))),
        "the adapter was producing events when cancellation arrived"
    );
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::TurnEnded(payload)) if payload.stop_reason == StopReason::Interrupted
    )));
}

#[test]
fn run_turn_rejects_switching_and_closed_sessions() {
    let tmp = tempdir().expect("tempdir");
    let fake = FakeAdapter::new(HarnessKind::Opencode, Vec::new());
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "states");
    let store = deps.open_store().expect("store");

    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&opencode()))
        .expect("switching");
    let error = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("input is rejected while switching");
    assert!(matches!(error, CoreError::Store(StoreError::Conflict(_))));

    store
        .set_session_state(&session, vowp_protocol::SessionState::Closed, None)
        .expect("closed");
    let error = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("a closed session takes no turns");
    assert!(matches!(error, CoreError::Invalid(_)));
}

#[test]
fn missing_adapter_reports_harness_not_installed() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp, config_for(tmp.path()), AdapterRegistry::new());
    let service = SessionService::new(deps);
    let session = create_session(&service, &tmp, "missing adapter");
    let error = SessionOps::run_turn(&service, turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("no adapter");
    match error {
        ServiceError::Failed(message) => assert_eq!(message, "harness not installed"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn run_turn_redacts_error_text_in_run_detail_and_event() {
    let tmp = tempdir().expect("tempdir");
    let secret = format!("{}{}", "sk-", "abcdefghijklmnopqrstuvwxyz");
    let fake = FakeAdapter::new(HarnessKind::Opencode, Vec::new()).with_summary(RunSummary {
        exit_code: Some(1),
        outcome: RunOutcome::Error,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some(format!("provider rejected {secret}")),
        stop_reason: StopReason::Error,
    });
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "redaction");
    let result = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect("error outcome is returned");
    let store = deps.open_store().expect("store");
    let run = store.require_run(&result.run_id).expect("run");
    assert!(!run.detail.contains(&secret), "run.detail is redacted");
    assert!(run.detail.contains("<redacted>"));
    let events = read_events(&deps, &session);
    let error = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::Error(payload)) => Some(payload),
            _ => None,
        })
        .expect("error event");
    assert!(!error.message.contains(&secret));
    assert!(error.message.contains("<redacted>"));
}

#[test]
fn run_detail_is_redacted_on_adapter_error() {
    let tmp = tempdir().expect("tempdir");
    let secret = format!("{}{}", "sk-", "abcdefghijklmnopqrstuvwxyz");
    let fake = Arc::new(WaitErrorAdapter::new(format!("wait failed with {secret}")));
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", fake),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "adapter wait error");

    let error = service
        .run_turn(turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("a wait error surfaces as an adapter error");
    assert!(matches!(error, CoreError::Adapter(_)));

    let events = read_events(&deps, &session);
    let run_id = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Attach => payload.run_id.clone(),
            _ => None,
        })
        .expect("attach carries the run id");
    let store = deps.open_store().expect("store");
    let run = store.require_run(&run_id).expect("run row");
    assert_eq!(run.outcome, Some(RunOutcome::Error));
    assert!(!run.detail.contains(&secret), "run.detail is redacted: {}", run.detail);
    assert!(run.detail.contains("<redacted>"));
}

#[test]
fn session_ops_trait_reports_conflict_as_service_failure() {
    let tmp = tempdir().expect("tempdir");
    let fake = FakeAdapter::new(HarnessKind::Opencode, Vec::new());
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "trait");
    let held = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("hold lock");
    let error = SessionOps::run_turn(&service, turn(&session, opencode(), "work"), &CancelToken::default())
        .expect_err("conflict");
    match error {
        ServiceError::Failed(message) => assert!(message.contains("locked"), "{message}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    drop(held);
}

/// Builds one harness event at the test timestamp.
fn body_event(body: EventBody) -> HarnessEvent {
    HarnessEvent {
        ts: Some(ts()),
        actor: Actor::Assistant,
        body,
        raw: None,
    }
}

/// Runs the recorded claude lines through the real parser.
fn parse_claude_fixture() -> Vec<HarnessEvent> {
    let executor = claude();
    let mut parser: Box<dyn StreamParser> = Box::new(ClaudeParser::new(ts(), 0, executor));
    let lines = [
        r#"{"type":"system","subtype":"init","session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99","model":"claude-sonnet-5-5","cwd":"/home/user/project"}"#,
        r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[{"type":"text","text":"ok"}]},"session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99"}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","stop_reason":"end_turn","terminal_reason":"completed","num_turns":1,"duration_ms":1000,"total_cost_usd":0.1,"usage":{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens_details":{"thinking_tokens":0}},"session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99"}"#,
    ];
    let mut events = Vec::new();
    for line in lines {
        events.extend(parser.feed_line(line));
    }
    let (terminal, _summary) = parser.finish(Some(0), false, "");
    events.extend(terminal);
    events
}

/// Runs a recorded opencode run through the real parser.
fn parse_opencode_fixture() -> Vec<HarnessEvent> {
    let executor = opencode();
    let mut parser: Box<dyn StreamParser> = Box::new(OpencodeParser::new(ts(), 0, executor));
    let lines = [
        r#"{"type":"step_start","timestamp":1794168421621,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_1","type":"step-start"}}"#,
        r#"{"type":"text","timestamp":1794168436849,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_2","type":"text","text":"ok"}}"#,
        r#"{"type":"step_finish","timestamp":1794168436874,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_3","type":"step-finish","reason":"stop","cost":0.01,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}"#,
    ];
    let mut events = Vec::new();
    for line in lines {
        events.extend(parser.feed_line(line));
    }
    let (terminal, _summary) = parser.finish(Some(0), false, "");
    events.extend(terminal);
    events
}

/// An adapter that can resume a native session and returns a canned native id.
struct NativeAdapter {
    native: String,
    starts: Arc<Mutex<Vec<StartInput>>>,
}

impl NativeAdapter {
    fn new(native: &str) -> Self {
        Self {
            native: native.to_owned(),
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn starts(&self) -> Vec<StartInput> {
        self.starts.lock().expect("starts").clone()
    }
}

impl Adapter for NativeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("fake".to_owned()),
            path: None,
            detail: String::new(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: true,
            ..AdapterCapabilities::default()
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.starts.lock().expect("starts").push(input);
        let (sender, receiver) = channel();
        drop(sender);
        Ok(Box::new(NativeRun {
            receiver,
            native: self.native.clone(),
        }))
    }
}

struct NativeRun {
    receiver: Receiver<HarnessEvent>,
    native: String,
}

impl AdapterRun for NativeRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(RunSummary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: Some(self.native.clone()),
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("ok".to_owned()),
            stop_reason: StopReason::EndTurn,
        })
    }
}

/// An adapter whose run starts but fails when the caller waits for its summary.
struct WaitErrorAdapter {
    message: String,
}

impl WaitErrorAdapter {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl Adapter for WaitErrorAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("fake".to_owned()),
            path: None,
            detail: String::new(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let (sender, receiver) = channel();
        drop(sender);
        Ok(Box::new(WaitErrorRun {
            receiver,
            message: self.message.clone(),
        }))
    }
}

struct WaitErrorRun {
    receiver: Receiver<HarnessEvent>,
    message: String,
}

impl AdapterRun for WaitErrorRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Err(AdapterError::Protocol(self.message.clone()))
    }
}

/// An adapter that keeps emitting assistant messages until interrupted.
struct StreamingAdapter {
    interrupts: Arc<AtomicBool>,
    started: Arc<Mutex<Option<Sender<()>>>>,
}

impl StreamingAdapter {
    fn new(started: Sender<()>) -> Self {
        Self {
            interrupts: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Mutex::new(Some(started))),
        }
    }
}

impl Adapter for StreamingAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("fake".to_owned()),
            path: None,
            detail: String::new(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        if let Some(started) = self.started.lock().expect("started sender").take() {
            let _ = started.send(());
        }
        let (sender, receiver) = channel();
        let running = Arc::new(AtomicBool::new(true));
        let producer_sender = sender.clone();
        let producer_running = Arc::clone(&running);
        std::thread::spawn(move || {
            while producer_running.load(Ordering::SeqCst) {
                if producer_sender
                    .send(HarnessEvent {
                        ts: None,
                        actor: Actor::Assistant,
                        body: EventBody::AssistantMessage(AssistantMessage {
                            text: "streaming tick".to_owned(),
                        }),
                        raw: None,
                    })
                    .is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Ok(Box::new(StreamingRun {
            receiver,
            sender: Mutex::new(Some(sender)),
            running,
            interrupts: Arc::clone(&self.interrupts),
        }))
    }
}

struct StreamingRun {
    receiver: Receiver<HarnessEvent>,
    sender: Mutex<Option<Sender<HarnessEvent>>>,
    running: Arc<AtomicBool>,
    interrupts: Arc<AtomicBool>,
}

impl AdapterRun for StreamingRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        self.interrupts.store(true, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        if let Some(sender) = self.sender.lock().expect("sender").take() {
            let _ = sender.send(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Interrupted,
                    duration_ms: None,
                }),
                raw: None,
            });
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        self.interrupt()
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(RunSummary {
            exit_code: None,
            outcome: RunOutcome::Killed,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: None,
            stop_reason: StopReason::Interrupted,
        })
    }
}

/// An adapter whose run blocks until `interrupt()` is called.
struct BlockingAdapter {
    interrupts: Arc<AtomicBool>,
    started: Arc<Mutex<Option<Sender<()>>>>,
}

impl BlockingAdapter {
    fn new(started: Sender<()>) -> Self {
        Self {
            interrupts: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Mutex::new(Some(started))),
        }
    }
}

impl Adapter for BlockingAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("fake".to_owned()),
            path: None,
            detail: String::new(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        if let Some(started) = self.started.lock().expect("started sender").take() {
            let _ = started.send(());
        }
        let (sender, receiver) = channel();
        Ok(Box::new(BlockingRun {
            receiver,
            sender: Mutex::new(Some(sender)),
            interrupts: Arc::clone(&self.interrupts),
        }))
    }
}

struct BlockingRun {
    receiver: Receiver<HarnessEvent>,
    sender: Mutex<Option<Sender<HarnessEvent>>>,
    interrupts: Arc<AtomicBool>,
}

impl AdapterRun for BlockingRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        self.interrupts.store(true, Ordering::SeqCst);
        if let Some(sender) = self.sender.lock().expect("sender").take() {
            let _ = sender.send(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Interrupted,
                    duration_ms: None,
                }),
                raw: None,
            });
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        self.interrupt()
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(RunSummary {
            exit_code: None,
            outcome: RunOutcome::Killed,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: None,
            stop_reason: StopReason::Interrupted,
        })
    }
}

#[test]
fn session_executor_framing_is_visible_on_every_run() {
    let tmp = tempdir().expect("tempdir");
    let fake = FakeAdapter::new(HarnessKind::Opencode, Vec::new());
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", Arc::new(fake)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "twice");
    for _ in 0..2 {
        service
            .run_turn(turn(&session, opencode(), "again"), &CancelToken::default())
            .expect("run");
    }
    let events = read_events(&deps, &session);
    let attaches = events
        .iter()
        .filter(|event| {
            matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Attach
            )
        })
        .count();
    assert_eq!(attaches, 2, "attach is written on every run, not only the first");
    let detaches = events
        .iter()
        .filter(|event| {
            matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(SessionExecutor {
                    action: AttachAction::Detach,
                    ..
                }))
            )
        })
        .count();
    assert_eq!(detaches, 2);
}

#[test]
fn run_turn_does_not_start_when_cancel_is_already_set() {
    let tmp = tempdir().expect("tempdir");
    let fake = Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new()));
    let deps = deps(
        &tmp,
        config_for(tmp.path()),
        AdapterRegistry::new().with("opencode", fake.clone()),
    );
    let service = SessionService::new(deps);
    let session = create_session(&service, &tmp, "pre-cancelled");
    let cancel = CancelToken::default();
    cancel.cancel();
    let error = service
        .run_turn(turn(&session, opencode(), "work"), &cancel)
        .expect_err("pre-cancelled");
    assert!(matches!(error, CoreError::Cancelled));
    assert!(fake.starts().is_empty());
}
