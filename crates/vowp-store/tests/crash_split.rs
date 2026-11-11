//! Real child-process crash injection for the global/project write order.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.7 and §15.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use vowp_protocol::{
    Actor, Cost, EventBody, EventSource, FakeClock, HarnessKind, NodeName, Project, ProjectName, Run, RunId,
    RunPurpose, RunRole, Session, SessionId, SessionRole, SessionState, Timestamp, TurnStarted,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, RunFinish, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

fn now() -> Timestamp {
    Timestamp::parse(EPOCH).expect("epoch")
}

fn project() -> ProjectName {
    ProjectName::parse("proj").expect("project")
}

fn node_project() -> ProjectName {
    ProjectName::parse("_node").expect("node project")
}

fn open_bound(root: &Path) -> Store {
    let paths = NodePaths::new(root);
    let clock = FakeClock::at(now());
    let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock));
    if store.bound_project().is_none() {
        store
            .upsert_project(&Project {
                name: project(),
                path: root.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now(),
                updated_at: now(),
            })
            .expect("project");
    }
    Store::open_with_project(&paths, &project())
        .expect("bound")
        .with_clock(Arc::new(FakeClock::at(now())))
}

fn session(store: &Store, state: SessionState) -> SessionId {
    session_with_project(store, Some(project()), state)
}

fn session_with_project(store: &Store, project: Option<ProjectName>, state: SessionState) -> SessionId {
    let id = SessionId::new();
    store
        .upsert_session(&Session {
            id: id.clone(),
            project,
            job_id: None,
            role: Some(SessionRole::Worker),
            title: "crash".to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/tmp".to_owned(),
            state,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now(),
            updated_at: now(),
        })
        .expect("session");
    id
}

fn open_node_bound(root: &Path) -> Store {
    let paths = NodePaths::new(root);
    let clock = FakeClock::at(now());
    let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock));
    if store.project(&node_project()).expect("lookup").is_none() {
        store
            .upsert_project(&Project {
                name: node_project(),
                path: root.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now(),
                updated_at: now(),
            })
            .expect("node project");
    }
    Store::open_with_project(&paths, &node_project())
        .expect("bound node")
        .with_clock(Arc::new(FakeClock::at(now())))
}

fn assert_no_duplicate_seqs(store: &Store) {
    for session in store.list_sessions(&Default::default()).expect("sessions") {
        let events = store.event_rows(&session.id, 0, 10_000).expect("events");
        let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
        let unique: HashSet<u64> = seqs.iter().copied().collect();
        assert_eq!(seqs.len(), unique.len(), "duplicate event seq in {}", session.id);
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "event seq is not monotonic"
        );
    }
}

fn open_run(store: &Store, session: &SessionId) -> RunId {
    let id = RunId::new();
    store
        .insert_run(&Run {
            id: id.clone(),
            job_id: None,
            session_id: Some(session.clone()),
            role: RunRole::Worker,
            purpose: RunPurpose::Start,
            profile: None,
            rung: 0,
            executor: None,
            provider: None,
            node: NodeName::parse("local").expect("node"),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at: now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        })
        .expect("run");
    id
}

fn write_turn_started(root: &Path, store: &Store, session: &SessionId) {
    let paths = NodePaths::new(root);
    let log = SessionLog::create(&paths, session, Arc::new(FakeClock::at(now()))).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(NodeName::parse("local").expect("node")),
                native_session_id: None,
            },
            body: EventBody::TurnStarted(TurnStarted { turn: 1 }),
            raw: None,
            ts: Some(now()),
        })
        .expect("turn started");
    store.index_event(&index_row(&appended)).expect("index event");
    log.sync().expect("sync");
}

fn child_case() -> Option<&'static str> {
    std::env::var("VOWP_CRASH_CASE").ok().map(|value| match value.as_str() {
        "global" => "global",
        "node" => "node",
        "project" => "project",
        _ => "unknown",
    })
}

fn child_root() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("VOWP_CRASH_ROOT").expect("VOWP_CRASH_ROOT"))
}

fn run_child(test: &str, case: &str, root: &Path) {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .env("VOWP_CRASH_CASE", case)
        .env("VOWP_CRASH_ROOT", root);
    let status = command.status().expect("spawn child");
    assert!(status.success(), "crash child exited with {status}");
}

#[test]
fn crash_between_global_framing_and_project_run() {
    if child_case() == Some("global") {
        let root = child_root();
        let store = open_bound(&root);
        let session = session(&store, SessionState::Running);
        write_turn_started(&root, &store, &session);
        std::process::exit(0);
    }

    let tmp = TempDir::new().expect("tempdir");
    run_child("crash_between_global_framing_and_project_run", "global", tmp.path());
    let store = open_bound(tmp.path());
    let report = store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.sessions_repaired, 1);
    assert_eq!(
        store.list_sessions(&Default::default()).expect("sessions")[0].state,
        SessionState::Idle
    );
    assert_no_duplicate_seqs(&store);
    let second = store.reconcile_cross_db().expect("idempotent");
    assert_eq!(second, Default::default());
}

#[test]
fn crash_on_node_handle_repairs_project_less_session() {
    if child_case() == Some("node") {
        let root = child_root();
        let store = open_node_bound(&root);
        let session = session_with_project(&store, None, SessionState::Running);
        write_turn_started(&root, &store, &session);
        std::process::exit(0);
    }

    let tmp = TempDir::new().expect("tempdir");
    run_child("crash_on_node_handle_repairs_project_less_session", "node", tmp.path());
    let store = open_node_bound(tmp.path());
    let report = store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.sessions_repaired, 1);
    let sessions = store.list_sessions(&Default::default()).expect("sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].project, None);
    assert_eq!(sessions[0].state, SessionState::Idle);
    assert_no_duplicate_seqs(&store);
    let second = store.reconcile_cross_db().expect("idempotent");
    assert_eq!(second, Default::default());
}

#[test]
fn crash_between_project_finish_and_terminal_events() {
    if child_case() == Some("project") {
        let root = child_root();
        let store = open_bound(&root);
        let session = session(&store, SessionState::Idle);
        write_turn_started(&root, &store, &session);
        let run = open_run(&store, &session);
        store
            .finish_run(
                &run,
                &RunFinish {
                    outcome: vowp_protocol::RunOutcome::Ok,
                    exit_code: Some(0),
                    limit_kind: None,
                    cost: Cost::unknown(),
                    native_session_id: None,
                    detail: String::new(),
                },
            )
            .expect("finish project row");
        std::process::exit(0);
    }

    let tmp = TempDir::new().expect("tempdir");
    run_child(
        "crash_between_project_finish_and_terminal_events",
        "project",
        tmp.path(),
    );
    let store = open_bound(tmp.path());
    let report = store.reconcile_cross_db().expect("reconcile");
    assert_eq!(report.terminal_events_repaired, 1);
    assert_eq!(report.ledgers_repaired, 1);
    assert_no_duplicate_seqs(&store);
    let second = store.reconcile_cross_db().expect("idempotent");
    assert_eq!(second, Default::default());
}
