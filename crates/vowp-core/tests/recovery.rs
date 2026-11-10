//! Startup recovery tests: `started` switches with and without a target attach, the
//! blocked-state recomputation and orphaned runs. Hermetic: temp node and fake clock.
//! Spec: `docs/design/sessions.md` §5 (crash recovery) and `docs/design/crates/vowp-core.md` §3.

mod support;

use std::path::Path;
use std::sync::Arc;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::recovery;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps};
use vowp_protocol::{
    Actor, AttachAction, Cost, EventBody, EventSource, EventType, Executor, FakeClock, HarnessKind, LimitEvent,
    LimitKind, MemoryLog, NodeName, Run, RunId, RunOutcome, RunPurpose, RunRole, SessionExecutor, SessionId,
    SessionRole, Switch, SwitchId, SwitchReason, SwitchState, Timestamp, TurnEnded, TurnStarted,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, Store};
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

fn node() -> NodeName {
    NodeName::parse("local").expect("node name")
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
        node: node(),
        clock: Arc::new(FakeClock::at(ts())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new().with(
            "opencode",
            Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new())),
        )),
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

fn started_switch(session: &SessionId, at_seq: u64) -> Switch {
    Switch {
        id: SwitchId::new(),
        session_id: session.clone(),
        from_executor: Some(opencode()),
        to_executor: claude(),
        from_node: Some(node()),
        to_node: node(),
        capsule_id: None,
        reason: SwitchReason::User,
        state: SwitchState::Started,
        error: None,
        started_at: ts(),
        ended_at: None,
        at_seq,
    }
}

fn append(store: &Store, log: &SessionLog, harness: HarnessKind, body: EventBody) {
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness,
                model: None,
                node: Some(node()),
                native_session_id: None,
            },
            body,
            raw: None,
            ts: None,
        })
        .expect("append");
    store.index_event(&index_row(&appended)).expect("index");
}

#[test]
fn recover_started_switch_never_attached_is_aborted() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "never attached");
    let store = deps.open_store().expect("store");
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq);
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&opencode()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 0);
    assert_eq!(report.switches_aborted, 1);
    assert_eq!(report.runs_closed, 0);

    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Aborted);
    assert_eq!(row.error.as_deref(), Some("interrupted by restart"));
    assert!(row.ended_at.is_some());

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, Some(opencode()), "the source stays attached");
    assert!(
        !SessionLog::open_read(&deps.paths, &session)
            .expect("log")
            .read_all()
            .expect("events")
            .iter()
            .any(|event| event.event_type == EventType::Switch),
        "an aborted switch has no switch event"
    );
}

#[test]
fn recover_started_switch_attached_is_completed() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "attached");
    let store = deps.open_store().expect("store");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Attach,
                executor: claude(),
                node: node(),
                native_session_id: None,
                run_id: None,
                reason: None,
            }),
        );
        log.sync().expect("sync");
    }
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq.saturating_sub(1));
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 1);
    assert_eq!(report.switches_aborted, 0);

    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Completed);

    let events = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .read_all()
        .expect("events");
    let recorded = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::Switch(payload)) => Some(payload),
            _ => None,
        })
        .expect("missing switch event was appended");
    assert_eq!(recorded.switch_id, switch.id);
    assert_eq!(recorded.to.executor, claude());

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle, "recovery unfreezes");
    assert_eq!(current.executor, Some(claude()), "the target is the live executor");
}

#[test]
fn recover_switch_with_recorded_event_unfreezes_on_target() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "recorded event");
    let store = deps.open_store().expect("store");
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq);
    store.insert_switch(&switch).expect("switch row");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &store,
            &log,
            HarnessKind::Vowp,
            EventBody::Switch(vowp_protocol::SwitchEvent {
                switch_id: switch.id.clone(),
                from: Some(vowp_protocol::ExecutorRef {
                    executor: opencode(),
                    node: Some(node()),
                }),
                to: vowp_protocol::ExecutorRef {
                    executor: claude(),
                    node: Some(node()),
                },
                capsule_id: None,
                reason: SwitchReason::User,
            }),
        );
        log.sync().expect("sync");
    }
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 1);
    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Completed);
    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, Some(claude()));
}

#[test]
fn recover_completed_switch_after_limit_is_idle() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "completed after limit");
    let store = deps.open_store().expect("store");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: vowp_protocol::StopReason::Limit,
                duration_ms: None,
            }),
        );
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Attach,
                executor: claude(),
                node: node(),
                native_session_id: None,
                run_id: None,
                reason: None,
            }),
        );
        log.sync().expect("sync");
    }
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq);
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 1);
    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Completed);
    let current = store.require_session(&session).expect("session");
    assert_eq!(
        current.state,
        vowp_protocol::SessionState::Idle,
        "a completed switch unfreezes on the new executor even after a limit"
    );
    assert_eq!(current.executor, Some(claude()));
}

#[test]
fn recover_switch_after_target_detach_is_aborted() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "attached then detached");
    let store = deps.open_store().expect("store");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Attach,
                executor: claude(),
                node: node(),
                native_session_id: None,
                run_id: None,
                reason: None,
            }),
        );
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Detach,
                executor: claude(),
                node: node(),
                native_session_id: None,
                run_id: None,
                reason: Some("crash".to_owned()),
            }),
        );
        log.sync().expect("sync");
    }
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq);
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 0);
    assert_eq!(report.switches_aborted, 1, "the last session.executor is a detach");
    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Aborted);
    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, None, "the last executor event was a detach");
}

#[test]
fn recover_switch_after_limit_recomputes_blocked() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "blocked recovery");
    let store = deps.open_store().expect("store");
    {
        let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
        append(
            &store,
            &log,
            HarnessKind::ClaudeCode,
            EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: vowp_protocol::StopReason::Limit,
                duration_ms: None,
            }),
        );
        log.sync().expect("sync");
    }
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    let switch = started_switch(&session, last_seq);
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_aborted, 1);
    assert_eq!(
        store.require_session(&session).expect("session").state,
        vowp_protocol::SessionState::Blocked,
        "the last turn ended with a limit"
    );
}

#[test]
fn recovery_blocks_on_credits_and_auth_limit_events() {
    for (name, kind, status) in [
        ("credits", LimitKind::Credits, 402_u16),
        ("auth", LimitKind::Auth, 401_u16),
    ] {
        let tmp = tempdir().expect("tempdir");
        let deps = deps(&tmp);
        let service = SessionService::new(deps.clone());
        let session = create_session(&service, &tmp, name);
        let store = deps.open_store().expect("store");
        {
            let log = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("log");
            append(
                &store,
                &log,
                HarnessKind::ClaudeCode,
                EventBody::TurnStarted(TurnStarted { turn: 1 }),
            );
            append(
                &store,
                &log,
                HarnessKind::ClaudeCode,
                EventBody::Limit(LimitEvent {
                    kind,
                    provider: "claude".to_owned(),
                    resets_at: None,
                    message: format!("{name} exhausted"),
                    http_status: Some(status),
                }),
            );
            append(
                &store,
                &log,
                HarnessKind::ClaudeCode,
                EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason: vowp_protocol::StopReason::Error,
                    duration_ms: None,
                }),
            );
            log.sync().expect("sync");
        }
        let last_seq = SessionLog::open_read(&deps.paths, &session)
            .expect("log")
            .last_seq()
            .expect("seq");
        let switch = started_switch(&session, last_seq);
        store.insert_switch(&switch).expect("switch row");
        store
            .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&opencode()))
            .expect("switching");

        let report = service.recover(&store).expect("recover");
        assert_eq!(report.switches_aborted, 1, "{name}");
        let current = store.require_session(&session).expect("session");
        assert_eq!(
            current.state,
            vowp_protocol::SessionState::Blocked,
            "a {name} limit event blocks even though turn.ended says error"
        );
        assert_eq!(current.executor, Some(opencode()), "the source stays attached");
    }
}

#[test]
fn recover_closes_orphaned_runs() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(&tmp);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "orphan");
    let store = deps.open_store().expect("store");
    let run = Run {
        id: RunId::new(),
        job_id: None,
        session_id: Some(session.clone()),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(opencode()),
        provider: Some("deepseek".to_owned()),
        node: node(),
        native_session_id: None,
        pid: Some(42),
        window: None,
        cwd: None,
        started_at: ts(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("run row");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.runs_closed, 1);
    let finished = store.run(&run.id).expect("run").expect("row");
    assert_eq!(finished.outcome, Some(RunOutcome::Crash));
    assert!(finished.ended_at.is_some());
    assert!(finished.detail.contains("orphaned"));
}
