//! Surface lease and takeover tests. Hermetic: a temporary node home, a fake clock, fake
//! adapters and no harness process. Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3
//! and the surface lease tests in §15.

mod support;

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::session::remote::{IngestGrant, IngestSession};
use vowp_core::session::service::SessionService;
use vowp_core::surface::lease::{AcquireLease, HEARTBEAT_INTERVAL, LeaseGuard, STALE_AFTER, check_lease_with};
use vowp_core::{AdapterRegistry, CoreError, Deps};
use vowp_protocol::{
    Actor, AssistantMessage, AttachAction, Clock, Cost, EventBody, EventSource, EventType, Executor, FakeClock,
    HarnessKind, IngestAck, MemoryLog, NewSession, Node, NodeName, RemoteLine, Run, RunId, RunPurpose, RunRole,
    SessionId, SessionRole, SessionState, Surface, SurfaceId, SurfaceKind, SwitchReason, SwitchState, Timestamp,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePaths, Store, StoreError};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn local() -> NodeName {
    NodeName::parse("local").expect("node name")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config.capsule.executor = Some(opencode());
    config
}

fn deps_with_clock(tmp: &TempDir, registry: AdapterRegistry, clock: FakeClock) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: local(),
        clock: Arc::new(clock),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(registry),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(0)),
    }
}

fn deps(tmp: &TempDir, clock: &FakeClock) -> Deps {
    let adapter = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: serde_json::json!({
                "intent": "continue the session",
                "constraints": [],
                "decisions": [],
                "state": {"done": [], "in_progress": [], "blocked": [], "next_steps": ["keep going"]},
                "files": [],
                "gotchas": [],
                "open_questions": []
            })
            .to_string(),
        })],
    );
    deps_with_clock(
        tmp,
        AdapterRegistry::new().with("opencode", Arc::new(adapter)),
        clock.clone(),
    )
}

fn create_session(service: &SessionService, tmp: &TempDir, executor: Option<Executor>) -> SessionId {
    service
        .create_session(NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: "surface lease".to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor,
        })
        .expect("create session")
}

fn surface(kind: SurfaceKind, holder: &str, now: Timestamp) -> Surface {
    Surface {
        id: SurfaceId::new(),
        kind,
        node: local(),
        holder: holder.to_owned(),
        app: None,
        cwd: None,
        metadata: serde_json::json!({}),
        created_at: now,
        last_seen_at: now,
    }
}

fn acquire(deps: &Deps, store: &Store, session: &SessionId, surface: &Surface) -> Result<LeaseGuard, CoreError> {
    LeaseGuard::acquire(
        deps,
        store,
        &AcquireLease {
            session: session.clone(),
            surface: surface.clone(),
            takeover: false,
        },
    )
}

fn read_events(deps: &Deps, session: &SessionId) -> Vec<vowp_protocol::EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

fn append_note(deps: &Deps, session: &SessionId, text: &str) {
    let store = deps.open_store().expect("store");
    let log = SessionLog::open(&deps.paths, session, deps.clock.clone()).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(local()),
                native_session_id: None,
            },
            body: EventBody::Note(vowp_protocol::Note { text: text.to_owned() }),
            raw: None,
            ts: None,
        })
        .expect("append note");
    store.index_event(&index_row(&appended)).expect("index note");
    log.sync().expect("sync note");
}

fn token_of(store: &Store, session: &SessionId) -> String {
    store.lease(session).expect("lease").expect("held lease").token
}

fn open_run(session: &SessionId) -> Run {
    Run {
        id: RunId::new(),
        job_id: None,
        session_id: Some(session.clone()),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(opencode()),
        provider: Some("deepseek".to_owned()),
        node: local(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at: ts(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    }
}

fn frame_line(frame: &RemoteLine) -> String {
    format!("{}\n", serde_json::to_string(frame).expect("frame JSON"))
}

fn parse_acks(bytes: &[u8]) -> Vec<IngestAck> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).expect("ack JSON"))
        .collect()
}

#[test]
fn lease_acquire_is_one_holder() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");

    let first = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &first).expect("first lease");
    let lease = store.lease(&session).expect("lease").expect("held");
    assert_eq!(lease.holder, "laptop");
    assert_eq!(lease.surface_kind, SurfaceKind::Chat);
    assert_eq!(lease.surface_id.as_ref(), Some(&first.id));
    assert_eq!(lease.token.len(), 52, "two ULIDs, 160 random bits");
    assert_eq!(guard.token(), lease.token);

    let second = surface(SurfaceKind::Opencode, "phone", ts());
    let error = match acquire(&deps, &store, &session, &second) {
        Ok(_) => panic!("second surface must be refused"),
        Err(error) => error,
    };
    match error {
        CoreError::Store(StoreError::Conflict(text)) => {
            assert!(text.contains("laptop"), "conflict names the holder: {text}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    let leases = store.list_leases().expect("leases");
    assert_eq!(leases.len(), 1, "one row per session");
    assert_eq!(
        leases[0].token, lease.token,
        "a refused acquire must not replace the token"
    );

    guard.release().expect("release");
    assert!(store.lease(&session).expect("lease").is_none());
}

#[test]
fn heartbeat_keeps_lease_live() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");
    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &target).expect("lease");
    let acquired = store.lease(&session).expect("lease").expect("held").heartbeat_at;

    clock.advance(HEARTBEAT_INTERVAL);
    std::thread::sleep(HEARTBEAT_INTERVAL + Duration::from_secs(2));

    let lease = store.lease(&session).expect("lease").expect("heartbeat kept it");
    assert!(
        lease.heartbeat_at > acquired,
        "the guard thread must advance heartbeat_at beyond the acquisition time"
    );
    assert!(
        ts().plus(STALE_AFTER) > lease.heartbeat_at,
        "the lease is still inside the stale window"
    );
    guard.release().expect("release");
}

#[test]
fn stale_lease_free_after_sixty_seconds() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let old_guard = acquire(&deps, &store, &session, &old).expect("old lease");

    clock.advance(STALE_AFTER + Duration::from_secs(1));
    let new = surface(SurfaceKind::Opencode, "phone", clock.now());
    let new_guard = acquire(&deps, &store, &session, &new).expect("stale lease is free");

    let lease = store.lease(&session).expect("lease").expect("held");
    assert_eq!(lease.holder, "phone");
    assert_eq!(new_guard.token(), lease.token);
    let notes: Vec<String> = read_events(&deps, &session)
        .into_iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::Note(note)) => Some(note.text),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|note| note == "stale surface laptop replaced"),
        "the replacement is recorded: {notes:?}"
    );

    old_guard.release().expect("release old");
    new_guard.release().expect("release new");
}

#[test]
fn takeover_cuts_old_surface_within_two_seconds() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    // Pre-attach session: no executor, so the takeover is the note-only path.
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &old).expect("old lease");
    let old_token = guard.token().to_owned();

    let phone = surface(SurfaceKind::Chat, "phone", clock.now());
    let started = Instant::now();
    let report = vowp_core::takeover(&deps, &store, &session, &phone, clock.now()).expect("takeover");
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_secs(2), "takeover took {elapsed:?}");
    assert_eq!(report.previous.as_deref(), Some("laptop"));
    assert!(!report.drained);
    assert_eq!(report.capsule_id, None);
    assert!(
        check_lease_with(&store, &session, Some(&old_token)).is_err(),
        "the old surface is cut off immediately"
    );
    assert!(
        check_lease_with(&store, &session, Some(&token_of(&store, &session))).is_ok(),
        "the new surface holds the live lease"
    );
}

#[test]
fn lease_revoked_is_pushed_within_2s() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &old).expect("old lease");
    let old_token = guard.token().to_owned();
    store
        .set_session_state(&session, SessionState::Running, Some(&opencode()))
        .expect("mark running");
    let run = open_run(&session);
    store.insert_run(&run).expect("run row");
    let mut ingest = IngestSession::begin(
        &deps,
        &store,
        &IngestGrant {
            session: session.clone(),
            lease: String::new(),
            run: run.id.clone(),
        },
    )
    .expect("ingest");
    let mut acks = Vec::new();
    let hello = RemoteLine::Hello {
        session_id: session.clone(),
        run_id: run.id.clone(),
        executor: opencode().to_string(),
        node: local(),
        spool_id: "test-spool".to_owned(),
        lease: old_token.clone(),
    };
    assert!(!ingest.line(&frame_line(&hello), &mut acks).expect("hello"));

    // The takeover command is a separate process in production; the shared lease row is
    // the only signal. Run it on another thread with its own store handle.
    let child_deps = deps.clone();
    let child_session = session.clone();
    let child_clock = clock.clone();
    let phone = surface(SurfaceKind::Chat, "phone", clock.now());
    let started = Instant::now();
    let takeover_thread = std::thread::spawn(move || {
        let store = child_deps.open_store().expect("child store");
        vowp_core::takeover(&child_deps, &store, &child_session, &phone, child_clock.now())
    });
    let moved = Instant::now() + Duration::from_secs(2);
    loop {
        let lease = store.lease(&session).expect("lease");
        if lease.as_ref().is_some_and(|lease| lease.holder == "phone") {
            break;
        }
        assert!(Instant::now() < moved, "the lease did not move within two seconds");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The next heartbeat is the old ingest's poll of the shared row.
    let heartbeat = RemoteLine::Heartbeat { at: clock.now() };
    assert!(
        ingest.line(&frame_line(&heartbeat), &mut acks).expect("heartbeat"),
        "a revoked ingest stops reading"
    );
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(2), "LeaseRevoked took {elapsed:?}");
    let parsed = parse_acks(&acks);
    assert_eq!(parsed.len(), 1, "exactly one lease-revoked acknowledgement");
    match &parsed[0] {
        IngestAck::LeaseRevoked {
            by,
            reason,
            at,
            last_seq,
        } => {
            assert_eq!(by, "phone");
            assert_eq!(reason, "session taken over");
            assert_eq!(*at, ts());
            assert!(*last_seq >= 1, "last_seq names the last appended event");
        }
        other => panic!("expected LeaseRevoked, got {other:?}"),
    }
    let before = read_events(&deps, &session).len();
    assert!(
        ingest
            .line(&frame_line(&RemoteLine::Heartbeat { at: clock.now() }), &mut acks)
            .expect("second heartbeat")
    );
    assert_eq!(read_events(&deps, &session).len(), before, "no event after revocation");

    // Release the ingest log writer so the switch drain can finish and write the note.
    ingest.finish().expect("finish ingest");
    let report = takeover_thread.join().expect("takeover thread").expect("takeover");
    assert!(report.drained);
}

#[test]
fn takeover_drains_and_writes_capsule() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &old).expect("old lease");
    let old_token = guard.token().to_owned();
    store
        .set_session_state(&session, SessionState::Running, Some(&opencode()))
        .expect("mark running");

    let child_deps = deps.clone();
    let child_session = session.clone();
    let child_clock = clock.clone();
    let phone = surface(SurfaceKind::Chat, "phone", clock.now());
    let takeover_thread = std::thread::spawn(move || {
        let store = child_deps.open_store().expect("child store");
        vowp_core::takeover(&child_deps, &store, &child_session, &phone, child_clock.now())
    });

    // A live takeover cuts the old token off while the switch drain is still running.
    let cutoff = Instant::now();
    loop {
        match check_lease_with(&store, &session, Some(&old_token)) {
            Err(_) => break,
            Ok(()) => {
                assert!(
                    cutoff.elapsed() < Duration::from_secs(2),
                    "the old token was still valid after {:?}",
                    cutoff.elapsed()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    assert!(
        cutoff.elapsed() < Duration::from_secs(2),
        "the cut-off took {:?}",
        cutoff.elapsed()
    );

    let report = takeover_thread.join().expect("takeover thread").expect("takeover");

    assert!(report.drained, "a running turn is drained before the handoff");
    let capsule = report.capsule_id.expect("switch writes a capsule");
    assert_eq!(
        store
            .latest_capsule(&session)
            .expect("latest capsule")
            .expect("capsule")
            .capsule_id,
        capsule
    );
    let switches = store.list_switches(&session).expect("switches");
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].reason, SwitchReason::Takeover);
    let events = read_events(&deps, &session);
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::TurnEnded(payload))
                if payload.stop_reason == vowp_protocol::StopReason::Interrupted
        )),
        "the interrupted turn is closed in the log"
    );
    assert!(
        events.iter().any(|event| event.event_type == EventType::Capsule),
        "the handoff capsule is in the log"
    );
    assert!(
        check_lease_with(&store, &session, Some(&old_token)).is_err(),
        "the old surface is read-only after the drain"
    );
}

#[test]
fn takeover_before_first_executor_writes_note_only() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let _guard = acquire(&deps, &store, &session, &old).expect("old lease");

    let phone = surface(SurfaceKind::Chat, "phone", clock.now());
    let report = vowp_core::takeover(&deps, &store, &session, &phone, clock.now()).expect("takeover");

    assert_eq!(report.previous.as_deref(), Some("laptop"));
    assert!(!report.drained);
    assert_eq!(report.capsule_id, None);
    assert!(
        store.list_switches(&session).expect("switches").is_empty(),
        "a pre-attach session has no valid switch target"
    );
    assert!(store.latest_capsule(&session).expect("capsule").is_none());
    let notes: Vec<String> = read_events(&deps, &session)
        .into_iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::Note(note)) => Some(note.text),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|note| note.starts_with("taken over by phone at ")),
        "the note records the takeover: {notes:?}"
    );
}

#[test]
fn follow_mode_prints_new_events_and_refuses_writes() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &old).expect("old lease");
    let old_token = guard.token().to_owned();

    let phone = surface(SurfaceKind::Chat, "phone", clock.now());
    vowp_core::takeover(&deps, &store, &session, &phone, clock.now()).expect("takeover");
    let after = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("last seq");
    append_note(&deps, &session, "new event for the follower");

    let fresh = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .read_from(after, None)
        .expect("read new events");
    assert_eq!(fresh.len(), 1);
    assert!(matches!(
        fresh[0].body(),
        Ok(EventBody::Note(note)) if note.text == "new event for the follower"
    ));
    assert!(
        check_lease_with(&store, &session, Some(&old_token)).is_err(),
        "follow mode cannot write"
    );
    assert!(check_lease_with(&store, &session, Some(&token_of(&store, &session))).is_ok());
}

#[test]
fn lease_check_rejects_wrong_token() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");

    assert!(
        check_lease_with(&store, &session, Some("no-lease-yet")).is_ok(),
        "a job session without a lease passes"
    );
    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &target).expect("lease");
    assert!(check_lease_with(&store, &session, Some(guard.token())).is_ok());
    assert!(
        check_lease_with(&store, &session, None).is_ok(),
        "None means no lease required"
    );

    let error = check_lease_with(&store, &session, Some("wrong-token")).expect_err("wrong token");
    match error {
        CoreError::Refused(text) => {
            assert!(text.contains("read-only"), "refusal explains the state: {text}");
            assert!(text.contains("laptop"), "refusal names the holder: {text}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    guard.release().expect("release");
    assert!(
        check_lease_with(&store, &session, Some("wrong-token")).is_ok(),
        "without a lease the session is a job session again"
    );
}

#[test]
fn lease_sweep_drops_dead_surface() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let _guard = acquire(&deps, &store, &session, &target).expect("lease");

    clock.advance(STALE_AFTER + Duration::from_secs(1));
    let dropped = vowp_core::surface::sweep_leases(&deps, &store, clock.now()).expect("sweep");
    assert_eq!(dropped, 1);
    assert!(store.lease(&session).expect("lease").is_none());
    assert_eq!(
        vowp_core::surface::sweep_leases(&deps, &store, clock.now()).expect("second sweep"),
        0
    );
}

#[test]
fn lease_sweep_does_not_drop_a_heartbeated_lease() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let _guard = acquire(&deps, &store, &session, &target).expect("lease");

    clock.advance(STALE_AFTER + Duration::from_secs(1));
    let before = clock.now().minus(STALE_AFTER);
    let stale = store.stale_leases(before).expect("stale snapshot");
    assert_eq!(stale.len(), 1, "the row is stale at the snapshot");
    let token = stale[0].token.clone();

    // A heartbeat between the SELECT and the DELETE must keep the lease.
    assert!(store.heartbeat_lease(&session, &token, clock.now()).expect("heartbeat"));
    assert!(
        !store
            .release_stale_lease(&session, &token, before)
            .expect("conditional release"),
        "the conditional delete must not remove a refreshed lease"
    );
    assert!(store.lease(&session).expect("lease").is_some());
    assert_eq!(
        vowp_core::surface::sweep_leases(&deps, &store, clock.now()).expect("sweep"),
        0,
        "a fresh heartbeat is not stale"
    );
}

#[test]
fn surface_store_crud_and_lease_token_checks() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");
    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &target).expect("lease");
    let token = guard.token().to_owned();

    let stored = store.surface(&target.id).expect("surface").expect("row");
    assert_eq!(stored.holder, "laptop");
    assert_eq!(stored.kind, SurfaceKind::Chat);
    assert_eq!(stored.node, local());

    assert!(
        !store
            .heartbeat_lease(&session, "wrong-token", ts())
            .expect("wrong heartbeat")
    );
    assert!(!store.release_lease(&session, "wrong-token").expect("wrong release"));
    assert!(
        store.lease(&session).expect("lease").is_some(),
        "a wrong token changes nothing"
    );
    assert!(
        store
            .heartbeat_lease(&session, &token, clock.now())
            .expect("right heartbeat")
    );

    assert!(store.delete_surface(&target.id).expect("delete surface"));
    assert!(store.surface(&target.id).expect("surface").is_none());
    assert!(
        store
            .lease(&session)
            .expect("lease")
            .expect("held")
            .surface_id
            .is_none(),
        "deleting the surface clears the lease reference"
    );
    assert!(!store.delete_surface(&target.id).expect("delete again"));
}

#[test]
fn takeover_switches_to_the_target_node() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, Some(opencode()));
    let store = deps.open_store().expect("store");

    // A second node that is local to this test host: the switch probes it without SSH.
    let vps = NodeName::parse("vps").expect("vps node");
    store
        .upsert_node(&Node {
            name: vps.clone(),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({}),
            last_seen_at: Some(clock.now()),
            created_at: clock.now(),
        })
        .expect("vps node row");

    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let old_guard = acquire(&deps, &store, &session, &old).expect("old lease");

    let mut phone = surface(SurfaceKind::Chat, "phone", clock.now());
    phone.node = vps.clone();
    let report = vowp_core::takeover(&deps, &store, &session, &phone, clock.now()).expect("takeover");

    assert_eq!(report.previous.as_deref(), Some("laptop"));
    let switches = store.list_switches(&session).expect("switches");
    assert_eq!(switches.len(), 1, "the executor switch is recorded");
    assert_eq!(
        switches[0].to_node, vps,
        "the switch target is the takeover surface's node, not the session row's node"
    );
    assert_eq!(switches[0].state, SwitchState::Completed);
    assert_eq!(
        store.require_session(&session).expect("session").node,
        local(),
        "the session log still belongs to its home node"
    );

    let events = read_events(&deps, &session);
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::SessionExecutor(payload))
                if payload.action == AttachAction::Attach && payload.node == vps
        )),
        "the attach event names the target node"
    );

    old_guard.release().expect("release old");
}

#[test]
fn stale_acquire_boundary_is_exactly_sixty_seconds() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");

    let old = surface(SurfaceKind::Chat, "laptop", ts());
    let old_guard = acquire(&deps, &store, &session, &old).expect("old lease");
    let old_token = old_guard.token().to_owned();

    clock.advance(STALE_AFTER - Duration::from_millis(1));
    let not_yet = surface(SurfaceKind::Chat, "tablet", clock.now());
    match acquire(&deps, &store, &session, &not_yet) {
        Ok(_) => panic!("a lease 59.999 s old is still live"),
        Err(CoreError::Store(StoreError::Conflict(_))) => {}
        Err(other) => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(token_of(&store, &session), old_token);

    clock.advance(Duration::from_millis(1));
    let exactly = surface(SurfaceKind::Chat, "phone", clock.now());
    let new_guard = acquire(&deps, &store, &session, &exactly).expect("exactly 60 s is stale");

    assert_eq!(store.lease(&session).expect("lease").expect("held").holder, "phone");
    assert_eq!(new_guard.token(), token_of(&store, &session));
    let notes: Vec<String> = read_events(&deps, &session)
        .into_iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::Note(note)) => Some(note.text),
            _ => None,
        })
        .collect();
    assert!(
        notes.iter().any(|note| note == "stale surface laptop replaced"),
        "the exact boundary records the stale replacement: {notes:?}"
    );

    old_guard.release().expect("release old");
    new_guard.release().expect("release new");
}

#[test]
fn sweep_boundary_is_exactly_sixty_seconds() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");

    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &target).expect("lease");

    clock.advance(STALE_AFTER - Duration::from_millis(1));
    assert_eq!(
        vowp_core::surface::sweep_leases(&deps, &store, clock.now()).expect("sweep"),
        0,
        "a lease 59.999 s old is still live"
    );
    assert!(store.lease(&session).expect("lease").is_some());

    clock.advance(Duration::from_millis(1));
    assert_eq!(
        vowp_core::surface::sweep_leases(&deps, &store, clock.now()).expect("sweep"),
        1,
        "exactly 60 s is stale"
    );
    assert!(store.lease(&session).expect("lease").is_none());

    guard.release().expect("release");
}

#[test]
fn concurrent_acquire_never_writes_a_losing_token() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");

    let contenders = 4;
    let rounds = 8;
    for round in 0..rounds {
        assert!(
            store.lease(&session).expect("lease").is_none(),
            "round {round} starts free"
        );
        let barrier = Arc::new(Barrier::new(contenders + 1));
        let done = Arc::new(AtomicUsize::new(0));
        let now = clock.now();
        let mut threads = Vec::new();
        for index in 0..contenders {
            let deps = deps.clone();
            let session = session.clone();
            let barrier = barrier.clone();
            let done = done.clone();
            threads.push(std::thread::spawn(move || {
                let store = deps.open_store().expect("contender store");
                let mut contender = surface(SurfaceKind::Chat, "contender", now);
                contender.holder = format!("surface-{round}-{index}");
                barrier.wait();
                let result = acquire(&deps, &store, &session, &contender);
                done.fetch_add(1, Ordering::SeqCst);
                result
            }));
        }
        barrier.wait();

        let reader = deps.open_store().expect("reader store");
        let mut seen = Vec::new();
        while done.load(Ordering::SeqCst) < contenders {
            if let Some(lease) = reader.lease(&session).expect("sample lease") {
                seen.push(lease.token);
            }
            std::thread::yield_now();
        }

        let mut winners = Vec::new();
        let mut conflicts = 0;
        for thread in threads {
            match thread.join().expect("contender thread") {
                Ok(guard) => winners.push(guard),
                Err(CoreError::Store(StoreError::Conflict(_))) => conflicts += 1,
                Err(other) => panic!("round {round}: unexpected acquire error: {other:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: exactly one surface wins");
        assert_eq!(conflicts, contenders - 1, "round {round}: every loser is refused");

        let winner = winners.pop().expect("winner");
        assert!(
            seen.iter().all(|token| token == winner.token()),
            "round {round}: a losing acquire exposed its token: {seen:?}"
        );
        assert_eq!(
            token_of(&store, &session),
            winner.token(),
            "round {round}: the winner keeps the row"
        );
        winner.release().expect("release winner");
    }
}

#[test]
fn detach_hands_over_the_token_without_releasing() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(&tmp, &clock);
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, None);
    let store = deps.open_store().expect("store");

    let target = surface(SurfaceKind::Chat, "laptop", ts());
    let guard = acquire(&deps, &store, &session, &target).expect("lease");
    let token = guard.token().to_owned();
    let detached = guard.detach();

    assert_eq!(detached, token);
    let lease = store.lease(&session).expect("lease").expect("detach keeps the row");
    assert_eq!(lease.token, detached);
    assert!(
        store.release_lease(&session, &detached).expect("release detached"),
        "the caller can release the token it owns"
    );
    assert!(store.lease(&session).expect("lease").is_none());
}

#[test]
fn takeover_notice_is_the_exact_spec_text() {
    let at = Timestamp::parse("2026-11-08T14:05:00Z").expect("timestamp");
    assert_eq!(
        vowp_core::surface::takeover::notice("phone", at, 0),
        "Taken over by phone at 14:05. This surface is now read-only (follow mode)."
    );
}
