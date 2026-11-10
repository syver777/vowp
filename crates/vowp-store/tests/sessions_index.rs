//! Session index, native-session, capsule and switch repository tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vowp_protocol::{
    Actor, Capsule, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, Clock, EventId, EventIndexRow,
    EventType, Executor, ExecutorRef, FakeClock, HarnessKind, NodeName, Project, ProjectName, Session, SessionId,
    SessionRole, SessionState, Switch, SwitchId, SwitchReason, SwitchState, Timestamp,
};
use vowp_store::{NodePaths, SessionFilter, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A file-backed store with a controllable clock and one project.
struct Fixture {
    _tmp: TempDir,
    store: Store,
    clock: FakeClock,
    project: ProjectName,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project");
        let now = clock.now();
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: "/tmp/proj".to_owned(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("insert project");
        Self {
            _tmp: tmp,
            store,
            clock,
            project,
        }
    }

    fn session(&self, state: SessionState) -> Session {
        let now = self.clock.now();
        Session {
            id: SessionId::new(),
            project: Some(self.project.clone()),
            job_id: None,
            role: Some(SessionRole::Worker),
            title: "test session".to_owned(),
            parent_session_id: None,
            node: NodeName::parse("local").expect("node"),
            cwd: "/tmp/proj".to_owned(),
            state,
            executor: Some(Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor")),
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Builds one indexed event for `session`.
fn event(session: &SessionId, seq: u64, ts: Timestamp, offset: u64, len: u64) -> EventIndexRow {
    EventIndexRow {
        session_id: session.clone(),
        seq,
        id: EventId::new(),
        ts,
        event_type: EventType::UserMessage,
        actor: Actor::User,
        harness: HarnessKind::ClaudeCode,
        model: Some("claude-sonnet-5-5".to_owned()),
        byte_offset: offset,
        byte_len: len,
        summary: format!("event {seq}"),
    }
}

/// Builds a minimal valid capsule.
fn capsule(session: &Session, at_seq: u64, created_at: Timestamp) -> Capsule {
    Capsule {
        version: 1,
        capsule_id: CapsuleId::new(),
        session_id: session.id.clone(),
        created_at,
        at_seq,
        trigger: CapsuleTrigger::Manual,
        from: ExecutorRef {
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor"),
            node: Some(NodeName::parse("local").expect("node")),
        },
        intent: "hand the session off".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: CapsuleTelemetry::default(),
    }
}

/// Builds a started switch.
fn started_switch(session: &Session, started_at: Timestamp) -> Switch {
    Switch {
        id: SwitchId::new(),
        session_id: session.id.clone(),
        from_executor: session.executor.clone(),
        to_executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
        from_node: Some(NodeName::parse("local").expect("node")),
        to_node: NodeName::parse("local").expect("node"),
        capsule_id: None,
        reason: SwitchReason::User,
        state: SwitchState::Started,
        error: None,
        started_at,
        ended_at: None,
        at_seq: 3,
    }
}

#[test]
fn session_index_and_natives() {
    let fixture = Fixture::new();
    let mut session = fixture.session(SessionState::Idle);
    fixture.store.upsert_session(&session).expect("insert session");
    assert_eq!(fixture.store.require_session(&session.id).expect("session"), session);

    let first = event(&session.id, 1, fixture.clock.now(), 0, 10);
    fixture.store.index_event(&first).expect("index event 1");
    fixture.clock.advance(Duration::from_secs(1));
    let second = event(&session.id, 2, fixture.clock.now(), 10, 20);
    fixture.store.index_event(&second).expect("index event 2");
    fixture.clock.advance(Duration::from_secs(1));
    let third = event(&session.id, 3, fixture.clock.now(), 30, 5);
    fixture.store.index_event(&third).expect("index event 3");

    let indexed = fixture.store.require_session(&session.id).expect("indexed session");
    assert_eq!(indexed.last_seq, 3, "index_event bumps last_seq");
    assert_eq!(indexed.log_bytes, 35, "log_bytes is the end of the last line");
    assert_eq!(indexed.updated_at, third.ts, "updated_at follows the last event");

    let after_one = fixture.store.event_rows(&session.id, 1, 10).expect("events");
    assert_eq!(after_one, vec![second.clone(), third.clone()]);
    assert_eq!(
        fixture.store.event_rows(&session.id, 1, 1).expect("limited events"),
        vec![second.clone()]
    );
    assert_eq!(
        fixture.store.event_row_by_id(&third.id).expect("event by id"),
        Some(third.clone())
    );
    assert_eq!(
        fixture.store.event_row_by_id(&EventId::new()).expect("missing event"),
        None
    );

    // An upsert updates the row but keeps its original creation time.
    session.title = "renamed session".to_owned();
    session.state = SessionState::Blocked;
    fixture.store.upsert_session(&session).expect("upsert session");
    let renamed = fixture.store.require_session(&session.id).expect("renamed");
    assert_eq!(renamed.title, "renamed session");
    assert_eq!(renamed.state, SessionState::Blocked);
    assert_eq!(renamed.created_at, session.created_at);

    // Listing filters and orders by updated_at descending.
    let mut other = fixture.session(SessionState::Running);
    other.title = "other session".to_owned();
    fixture.clock.advance(Duration::from_secs(1));
    other.created_at = fixture.clock.now();
    other.updated_at = fixture.clock.now();
    fixture.store.upsert_session(&other).expect("insert other");
    let all = fixture.store.list_sessions(&SessionFilter::default()).expect("list");
    assert_eq!(
        all.iter().map(|item| item.id.clone()).collect::<Vec<_>>(),
        vec![other.id.clone(), session.id.clone()]
    );
    let running = fixture
        .store
        .list_sessions(&SessionFilter {
            state: Some(SessionState::Running),
            ..SessionFilter::default()
        })
        .expect("running sessions");
    assert_eq!(running, vec![other.clone()]);
    let limited = fixture
        .store
        .list_sessions(&SessionFilter {
            limit: Some(1),
            ..SessionFilter::default()
        })
        .expect("limited list");
    assert_eq!(limited, vec![other]);
    assert_eq!(
        fixture
            .store
            .list_sessions(&SessionFilter {
                project: Some(fixture.project.clone()),
                ..SessionFilter::default()
            })
            .expect("project list")
            .len(),
        2
    );

    let node = NodeName::parse("local").expect("node");
    fixture
        .store
        .upsert_native(&session.id, HarnessKind::ClaudeCode, "native-1", &node, 1)
        .expect("native 1");
    fixture
        .store
        .upsert_native(&session.id, HarnessKind::ClaudeCode, "native-1", &node, 99)
        .expect("duplicate native is a no-op");
    fixture
        .store
        .upsert_native(&session.id, HarnessKind::ClaudeCode, "native-2", &node, 5)
        .expect("native 2");

    let natives = fixture.store.natives_of(&session.id).expect("natives");
    assert_eq!(natives.len(), 2, "the duplicate pair was not inserted twice");
    assert_eq!(natives[0].native_session_id, "native-1");
    assert_eq!(natives[0].first_seq, 1, "the duplicate did not overwrite first_seq");
    assert_eq!(natives[1].native_session_id, "native-2");
    assert_eq!(natives[1].harness, HarnessKind::ClaudeCode);
    assert_eq!(
        fixture
            .store
            .native_for(&session.id, HarnessKind::ClaudeCode)
            .expect("native for"),
        Some("native-2".to_owned()),
        "most recent native id wins"
    );
    assert_eq!(
        fixture
            .store
            .native_for(&session.id, HarnessKind::Opencode)
            .expect("missing native"),
        None
    );
}

#[test]
fn finish_switch_links_capsule() {
    let fixture = Fixture::new();
    let session = fixture.session(SessionState::Idle);
    fixture.store.upsert_session(&session).expect("insert session");
    let capsule = capsule(&session, 4, fixture.clock.now());
    fixture
        .store
        .insert_capsule(&capsule, "sessions/capsule.json")
        .expect("insert capsule");

    let switch = started_switch(&session, fixture.clock.now());
    fixture.store.insert_switch(&switch).expect("insert switch");
    let started = fixture.store.switch(&switch.id).expect("started").expect("row");
    assert_eq!(started.capsule_id, None, "the plan row starts unlinked");

    fixture.clock.advance(Duration::from_secs(1));
    fixture
        .store
        .finish_switch(&switch.id, SwitchState::Completed, None, Some(&capsule.capsule_id))
        .expect("finish switch");
    let completed = fixture.store.switch(&switch.id).expect("completed").expect("row");
    assert_eq!(completed.state, SwitchState::Completed);
    assert_eq!(
        completed.capsule_id,
        Some(capsule.capsule_id.clone()),
        "the completed switch row links the capsule it used"
    );
    assert_eq!(completed.ended_at, Some(fixture.clock.now()));

    let unlinked = started_switch(&session, fixture.clock.now());
    fixture.store.insert_switch(&unlinked).expect("insert unlinked switch");
    fixture
        .store
        .finish_switch(&unlinked.id, SwitchState::Aborted, Some("probe failed"), None)
        .expect("abort unlinked switch");
    assert_eq!(
        fixture
            .store
            .switch(&unlinked.id)
            .expect("unlinked")
            .expect("row")
            .capsule_id,
        None
    );
}

#[test]
fn capsule_insert_sets_last_capsule() {
    let fixture = Fixture::new();
    let session = fixture.session(SessionState::Idle);
    fixture.store.upsert_session(&session).expect("insert session");

    let first = capsule(&session, 2, fixture.clock.now());
    fixture
        .store
        .insert_capsule(&first, "sessions/first.json")
        .expect("insert first capsule");
    let after_first = fixture.store.require_session(&session.id).expect("session");
    assert_eq!(after_first.last_capsule_id, Some(first.capsule_id.clone()));

    fixture.clock.advance(Duration::from_secs(30));
    let second = capsule(&session, 7, fixture.clock.now());
    fixture
        .store
        .insert_capsule(&second, "sessions/second.json")
        .expect("insert second capsule");

    let after_second = fixture.store.require_session(&session.id).expect("session");
    assert_eq!(after_second.last_capsule_id, Some(second.capsule_id.clone()));
    assert_eq!(
        fixture.store.capsule(&first.capsule_id).expect("capsule"),
        Some(first.clone())
    );
    assert_eq!(fixture.store.capsule(&CapsuleId::new()).expect("missing capsule"), None);
    assert_eq!(
        fixture.store.latest_capsule(&session.id).expect("latest"),
        Some(second.clone()),
        "latest_capsule picks the highest at_seq"
    );
    assert_eq!(
        fixture.store.list_capsules(&session.id).expect("list capsules"),
        vec![second, first],
        "capsules are newest first"
    );
}

#[test]
fn switch_lifecycle_and_open_switches() {
    let fixture = Fixture::new();
    let session = fixture.session(SessionState::Idle);
    fixture.store.upsert_session(&session).expect("insert session");

    let first = started_switch(&session, fixture.clock.now());
    fixture.store.insert_switch(&first).expect("insert switch");
    assert_eq!(fixture.store.switch(&first.id).expect("switch"), Some(first.clone()));
    assert_eq!(
        fixture.store.open_switches().expect("open switches"),
        vec![first.clone()]
    );
    assert_eq!(
        fixture.store.list_switches(&session.id).expect("switches"),
        vec![first.clone()]
    );

    fixture.clock.advance(Duration::from_secs(5));
    fixture
        .store
        .finish_switch(&first.id, SwitchState::Completed, None, None)
        .expect("finish switch");
    let finished = fixture.store.switch(&first.id).expect("finished switch").expect("row");
    assert_eq!(finished.state, SwitchState::Completed);
    assert_eq!(finished.ended_at, Some(fixture.clock.now()));
    assert!(fixture.store.open_switches().expect("open switches").is_empty());

    let second = started_switch(&session, fixture.clock.now());
    fixture.store.insert_switch(&second).expect("insert second switch");
    let error = fixture
        .store
        .finish_switch(&second.id, SwitchState::Started, None, None)
        .expect_err("started is not a terminal switch state");
    assert!(
        matches!(error, vowp_store::StoreError::InvalidInput(_)),
        "got {error:?}"
    );
    assert_eq!(
        fixture
            .store
            .switch(&second.id)
            .expect("still open")
            .expect("row")
            .state,
        SwitchState::Started
    );
    assert_eq!(fixture.store.open_switches().expect("open switches").len(), 1);

    fixture
        .store
        .finish_switch(&second.id, SwitchState::Aborted, Some("target probe failed"), None)
        .expect("abort switch");
    let aborted = fixture.store.switch(&second.id).expect("aborted").expect("row");
    assert_eq!(aborted.state, SwitchState::Aborted);
    assert_eq!(aborted.error, Some("target probe failed".to_owned()));
    assert!(fixture.store.open_switches().expect("open switches").is_empty());

    let missing = SwitchId::new();
    let error = fixture
        .store
        .finish_switch(&missing, SwitchState::Completed, None, None)
        .expect_err("missing switch");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}
