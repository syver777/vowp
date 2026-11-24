//! Orchestrator placement moves: home -> agent -> home keeps one session id.
//!
//! Every test uses a temporary node home, a fake app host and a recording transport. No
//! test opens the real `~/.vowp`, starts a network connection or runs a real harness.

mod support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::TempDir;
use vowp_core::surface::mirror::MirrorCursor;
use vowp_core::surface::native::{AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch};
use vowp_core::surface::orchestrator::{NewOrchestrator, Orchestrator, OrchestratorTransport};
use vowp_core::{AdapterRegistry, CoreError, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    Actor, AssistantMessage, CapsuleTrigger, EventBody, EventSource, HarnessKind, JoinInstructions, Lease, NodeName,
    OrchestratorApp, OrchestratorCommand, OrchestratorNodeState, OrchestratorResult, SessionId, SurfaceKind, Timestamp,
    VowpConfig,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{NodePath, NodePaths, Store};

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-18T18:00:00Z").expect("test timestamp")
}

fn home_node() -> NodeName {
    NodeName::parse("home-1").expect("home node")
}

fn agent_node(name: &str) -> NodeName {
    NodeName::parse(name).expect("agent node")
}

fn draft_json(intent: &str) -> String {
    serde_json::json!({
        "intent": intent,
        "constraints": [],
        "decisions": [],
        "state": {"done": ["moved"], "in_progress": [], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

fn capsule_adapter() -> FakeAdapter {
    FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: draft_json("handoff from the fake capsule"),
        })],
    )
}

fn config_for(tmp: &Path) -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, adapters: AdapterRegistry) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: home_node(),
        clock: Arc::new(vowp_protocol::FakeClock::at(ts())),
        log: Arc::new(vowp_protocol::MemoryLog::new()),
        adapters: Arc::new(adapters),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

fn store_of(deps: &Deps) -> Store {
    Store::open(&deps.paths).expect("store opens")
}

fn active_node(store: &Store, name: &str, now: Timestamp) -> NodeName {
    let node = agent_node(name);
    store
        .insert_pending_node(&node, &["darwin-arm64".to_owned()], now)
        .expect("pending node");
    store
        .set_node_status(&node, vowp_protocol::NodeStatus::Active, "", now)
        .expect("active node");
    store
        .touch_node(&node, now, Some("0.4.0"), Some("vowp-agent/1"), &serde_json::json!({}))
        .expect("node heartbeat");
    node
}

/// An active node whose last heartbeat is past the offline threshold.
fn offline_node(store: &Store, name: &str, now: Timestamp) -> NodeName {
    let node = agent_node(name);
    store
        .insert_pending_node(&node, &["darwin-arm64".to_owned()], now)
        .expect("pending node");
    // `set_node_status` also stamps `last_seen_at`; keep it older than the offline
    // threshold so the active node is present but offline.
    let last_seen = now.minus(std::time::Duration::from_secs(120));
    store
        .set_node_status(&node, vowp_protocol::NodeStatus::Active, "", last_seen)
        .expect("active node");
    node
}

/// Reads the session index row from the project store that owns it.
fn session_row(deps: &Deps, session: &SessionId) -> vowp_protocol::Session {
    deps.open_store()
        .expect("project store")
        .require_session(session)
        .expect("session row")
}

fn request(name: &str) -> NewOrchestrator {
    NewOrchestrator {
        name: name.to_owned(),
        project: None,
        app: OrchestratorApp::Opencode,
        executor: None,
        force: false,
    }
}

/// Appends one user event so the session has context beyond its creation event.
fn append_user_event(deps: &Deps, session: &SessionId) {
    let store = deps.open_store().expect("project store");
    let log = SessionLog::open(&deps.paths, session, deps.clock.clone()).expect("session log");
    let appended = log
        .append(NewEvent {
            actor: Actor::User,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(home_node()),
                native_session_id: None,
            },
            body: EventBody::UserMessage(vowp_protocol::UserMessage {
                text: "move this orchestrator".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
            raw: None,
            ts: None,
        })
        .expect("append user event");
    store.index_event(&index_row(&appended)).expect("index user event");
    log.sync().expect("sync log");
}

/// A hermetic app host that records starts and stops and writes a live `host.json`.
struct FakeHost {
    kind: OrchestratorApp,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl FakeHost {
    fn new(kind: OrchestratorApp) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        (
            Self {
                kind,
                starts: Arc::clone(&starts),
                stops: Arc::clone(&stops),
            },
            starts,
            stops,
        )
    }

    fn host_file(workdir: &Path) -> std::path::PathBuf {
        workdir.join("host.json")
    }
}

impl AppHost for FakeHost {
    fn kind(&self) -> OrchestratorApp {
        self.kind
    }

    fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(&req.workdir).expect("fake host workdir");
        let state = serde_json::json!({
            "kind": "opencode",
            "name": req.name,
            "pid": std::process::id(),
            "port": 4096,
            "started_at": Timestamp::now(),
            "workdir": req.workdir.display().to_string(),
            "vowp_session": req.session.to_string(),
        });
        std::fs::write(Self::host_file(&req.workdir), state.to_string()).expect("fake host.json");
        Ok(AppHandle {
            kind: self.kind,
            pid: std::process::id(),
            port: Some(4096),
            started_at: Timestamp::now(),
            workdir: req.workdir.clone(),
        })
    }

    fn stop(&self, handle: &AppHandle) -> Result<(), CoreError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        let _ = std::fs::remove_file(Self::host_file(&handle.workdir));
        Ok(())
    }

    fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError> {
        Ok(HostHealth {
            ok: Self::host_file(&handle.workdir).is_file(),
            detail: "fake host".to_owned(),
        })
    }

    fn join_instructions(&self, _handle: &AppHandle) -> Result<JoinInstructions, CoreError> {
        Ok(JoinInstructions {
            desktop: "fake desktop".to_owned(),
            phone: "fake phone".to_owned(),
            chat: "fake chat".to_owned(),
        })
    }

    fn transcript_since(&self, _handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
        Ok(TranscriptBatch {
            entries: Vec::new(),
            cursor: cursor.clone(),
        })
    }
}

/// A transport that records commands and answers them as a healthy agent would.
#[derive(Default)]
struct RecordingTransport {
    commands: Mutex<Vec<OrchestratorCommand>>,
}

impl RecordingTransport {
    fn commands(&self) -> Vec<OrchestratorCommand> {
        self.commands.lock().expect("command log").clone()
    }
}

impl OrchestratorTransport for RecordingTransport {
    fn send(&self, _node: &NodeName, command: &OrchestratorCommand) -> Result<Option<OrchestratorResult>, CoreError> {
        self.commands.lock().expect("command log").push(command.clone());
        let state = if matches!(command, OrchestratorCommand::Stop { .. }) {
            OrchestratorNodeState::Stopped
        } else {
            OrchestratorNodeState::Running
        };
        Ok(Some(OrchestratorResult::accepted(command.request_id(), state)))
    }
}

fn manager(
    deps: Deps,
) -> (
    Orchestrator,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<RecordingTransport>,
) {
    let (host, starts, stops) = FakeHost::new(OrchestratorApp::Opencode);
    let transport = Arc::new(RecordingTransport::default());
    (
        Orchestrator::with_transport(deps, vec![Arc::new(host)], transport.clone()),
        starts,
        stops,
        transport,
    )
}

#[test]
fn move_preserves_the_session_and_keeps_one_log_across_home_and_agent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with("opencode", Arc::new(capsule_adapter())),
    );
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, starts, stops, transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");
    assert_eq!(created.card.state, OrchestratorNodeState::Running);
    let session = created.session.clone();
    append_user_event(&deps, &session);

    let moved = orchestrator
        .move_to_node("night", Some(node.clone()))
        .expect("move to the agent");
    assert_eq!(moved.session, session, "the session id changed during the move");
    assert_eq!(moved.card.node.as_ref(), Some(&node));
    assert_eq!(moved.card.state, OrchestratorNodeState::Running);
    assert_eq!(starts.load(Ordering::SeqCst), 1, "the home host started once");
    assert_eq!(stops.load(Ordering::SeqCst), 1, "the old home host stopped");
    assert!(
        store.lease(&session).expect("lease lookup").is_none(),
        "the home lease survived the move to the node"
    );
    let capsule = store
        .latest_capsule(&session)
        .expect("capsule lookup")
        .expect("the move wrote a handoff capsule");
    assert_eq!(capsule.trigger, CapsuleTrigger::Manual);
    assert_eq!(capsule.intent, "handoff from the fake capsule");
    assert_eq!(capsule.session_id, session);
    let commands = transport.commands();
    assert!(
        matches!(commands.last(), Some(OrchestratorCommand::Start { name, .. }) if name == "night"),
        "the agent did not receive a start: {commands:?}"
    );

    let home = orchestrator.move_to_node("night", None).expect("move back to the home");
    assert_eq!(home.session, session, "the session id changed on the way home");
    assert_eq!(home.card.node, None);
    assert_eq!(home.card.state, OrchestratorNodeState::Running);
    assert_eq!(starts.load(Ordering::SeqCst), 2, "the home host restarted");
    let commands = transport.commands();
    assert!(
        matches!(commands.last(), Some(OrchestratorCommand::Stop { name, .. }) if name == "night"),
        "the agent did not receive a stop: {commands:?}"
    );
    let lease = store
        .lease(&session)
        .expect("lease lookup")
        .expect("the home holds the lease again");
    assert_eq!(lease.node, home_node());
    assert!(store.require_session(&session).expect("session row").last_seq > 1);
}

#[test]
fn orchestrator_move_updates_the_session_location_to_the_target_node() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, _starts, _stops, transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");
    let session = created.session.clone();

    let before = session_row(&deps, &session);
    assert_eq!(before.node, home_node(), "the fresh session must start on the home");
    assert_eq!(before.cwd, created.dir.display().to_string());
    let log_path = deps.paths.log_path(&session);
    let log_before = std::fs::read(&log_path).expect("session log");

    let moved = orchestrator
        .move_to_node("night", Some(node.clone()))
        .expect("move to the agent node");
    assert_eq!(moved.session, session, "the move minted a new session");

    let after = session_row(&deps, &session);
    assert_eq!(after.node, node, "the session row still names the home");
    assert_eq!(after.cwd, ".", "a remote node resolves its own checkout");
    assert_eq!(after.last_seq, before.last_seq, "the move changed last_seq");
    assert_eq!(
        std::fs::read(&log_path).expect("session log"),
        log_before,
        "the move rewrote the session log"
    );
    let commands = transport.commands();
    assert!(
        matches!(commands.last(), Some(OrchestratorCommand::Start { name, .. }) if name == "night"),
        "the target did not receive a start: {commands:?}"
    );
}

#[test]
fn orchestrator_move_home_restores_the_session_location() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, _starts, _stops, _transport) = manager(deps.clone());
    let created = orchestrator
        .create_on_node(&request("night"), Some(node.clone()))
        .expect("create orchestrator on the node");
    let session = created.session.clone();

    let before = session_row(&deps, &session);
    assert_eq!(before.node, node, "the fresh remote session must name the node");
    assert_eq!(before.cwd, ".", "the remote create stores the relative marker");

    let home = orchestrator.move_to_node("night", None).expect("move back to the home");
    assert_eq!(home.session, session, "the move home minted a new session");

    let after = session_row(&deps, &session);
    assert_eq!(after.node, home_node(), "the session row still names the node");
    assert_eq!(
        after.cwd,
        created.dir.display().to_string(),
        "a home placement uses the orchestrator directory"
    );
}

#[test]
fn orchestrator_move_to_an_unmapped_or_offline_node_leaves_the_session_row_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let unmapped = active_node(&store, "unmapped-host", deps.clock.now());
    let offline = offline_node(&store, "offline-host", deps.clock.now());
    let now = deps.clock.now();
    let project = vowp_protocol::ProjectName::parse("checkout").expect("project");
    store
        .upsert_project(&vowp_protocol::Project {
            name: project.clone(),
            path: tmp.path().join("project").display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        })
        .expect("project row");
    // The offline target is mapped, so only its presence can refuse the move.
    store
        .upsert_node_path(&NodePath {
            node: offline.clone(),
            project: project.clone(),
            local_root: tmp.path().join("node-project").display().to_string(),
            worktree_root: tmp.path().join("node-worktrees").display().to_string(),
            allow: Vec::new(),
            updated_at: now,
        })
        .expect("offline node path");

    let (orchestrator, starts, stops, _transport) = manager(deps.clone());
    let mut req = request("night");
    req.project = Some(project);
    let created = orchestrator.create(&req).expect("create project orchestrator");
    let before = session_row(&deps, &created.session);

    let error = orchestrator
        .move_to_node("night", Some(unmapped))
        .expect_err("an unmapped target is refused");
    assert!(matches!(error, CoreError::Refused(_)), "{error}");
    assert_eq!(
        session_row(&deps, &created.session),
        before,
        "an unmapped target changed the session row"
    );

    let error = orchestrator
        .move_to_node("night", Some(offline))
        .expect_err("an offline target is refused");
    assert!(matches!(error, CoreError::Refused(_)), "{error}");
    assert_eq!(
        session_row(&deps, &created.session),
        before,
        "an offline target changed the session row"
    );

    let info = orchestrator.info("night").expect("still exists");
    assert_eq!(info.card.node, None, "a refused move changed the placement");
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0, "a refused move stopped the host");
}

#[test]
fn orchestrator_move_does_not_clobber_last_seq_or_log_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, _starts, _stops, _transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");
    let session = created.session.clone();

    let before = session_row(&deps, &session);
    assert!(before.last_seq > 0, "the fixture must exercise a non-zero last_seq");
    assert!(before.log_bytes > 0, "the fixture must exercise non-zero log_bytes");
    let log_path = deps.paths.log_path(&session);
    let log_before = std::fs::read(&log_path).expect("session log");
    let events_before = deps
        .open_store()
        .expect("project store")
        .event_rows(&session, 0, 100)
        .expect("indexed events");

    orchestrator
        .move_to_node("night", Some(node))
        .expect("move to the agent node");

    let after = session_row(&deps, &session);
    assert_eq!(after.last_seq, before.last_seq, "the move changed last_seq");
    assert_eq!(after.log_bytes, before.log_bytes, "the move changed log_bytes");
    assert_eq!(
        std::fs::read(&log_path).expect("session log"),
        log_before,
        "the move rewrote the session log"
    );
    assert_eq!(
        deps.open_store()
            .expect("project store")
            .event_rows(&session, 0, 100)
            .expect("indexed events"),
        events_before,
        "the move changed the indexed events"
    );
}

#[test]
fn moving_between_agent_nodes_releases_the_old_node_lease() {
    // `vowp surface pair` without `--app` writes the node name as the holder; that is the
    // pairing command `node_join` prints. With `--app` it writes the app name.
    for holder in ["mac-studio", "opencode"] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let deps = deps(&tmp, AdapterRegistry::new());
        let store = store_of(&deps);
        let first = active_node(&store, "mac-studio", deps.clock.now());
        let second = active_node(&store, "studio-mini", deps.clock.now());
        let (orchestrator, _starts, _stops, transport) = manager(deps.clone());
        let created = orchestrator
            .create_on_node(&request("night"), Some(first.clone()))
            .expect("create on the first node");
        let session = created.session.clone();
        let now = deps.clock.now();
        store
            .take_lease(&Lease {
                session_id: session.clone(),
                surface_id: None,
                surface_kind: SurfaceKind::Opencode,
                node: first.clone(),
                holder: holder.to_owned(),
                token: vowp_protocol::surface::new_lease_token(),
                acquired_at: now,
                heartbeat_at: now,
            })
            .expect("old remote lease");

        let moved = orchestrator
            .move_to_node("night", Some(second.clone()))
            .expect("move between nodes");
        assert_eq!(moved.session, session, "holder {holder}");
        assert_eq!(moved.card.node.as_ref(), Some(&second), "holder {holder}");
        assert!(
            store.lease(&session).expect("lease lookup").is_none(),
            "the old node's lease (holder {holder}) is still a stale writer"
        );
        let commands = transport.commands();
        assert!(
            matches!(commands.last(), Some(OrchestratorCommand::Start { name, .. }) if name == "night"),
            "the second node did not receive a start: {commands:?}"
        );
        assert!(
            matches!(
                commands.get(commands.len().saturating_sub(2)),
                Some(OrchestratorCommand::Stop { name, .. }) if name == "night"
            ),
            "the first node did not receive a stop: {commands:?}"
        );
    }
}

#[test]
fn move_refuses_unknown_pending_and_revoked_nodes_before_stopping_the_host() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let (orchestrator, starts, stops, _transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");
    let session = created.session.clone();

    let unknown = agent_node("missing");
    let error = orchestrator
        .move_to_node("night", Some(unknown))
        .expect_err("an unknown node is refused");
    assert!(matches!(error, CoreError::NotFound(_)), "{error}");

    let pending = agent_node("pending");
    let now = deps.clock.now();
    store
        .insert_pending_node(&pending, &["darwin-arm64".to_owned()], now)
        .expect("pending node");
    let error = orchestrator
        .move_to_node("night", Some(pending))
        .expect_err("a pending node is refused");
    assert!(matches!(error, CoreError::Refused(_)), "{error}");

    let revoked = active_node(&store, "revoked", now);
    store
        .set_node_status(&revoked, vowp_protocol::NodeStatus::Revoked, "", now)
        .expect("revoked node");
    let error = orchestrator
        .move_to_node("night", Some(revoked))
        .expect_err("a revoked node is refused");
    assert!(matches!(error, CoreError::Refused(_)), "{error}");

    let info = orchestrator.info("night").expect("still exists");
    assert_eq!(info.session, session);
    assert_eq!(info.card.node, None, "a refused move changed the placement");
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0, "a refused move stopped the host");
    assert!(
        store.lease(&session).expect("lease lookup").is_some(),
        "a refused move released the lease"
    );
}

#[test]
fn move_to_the_current_placement_is_idempotent_and_resumes_a_stopped_record() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let (orchestrator, starts, stops, _transport) = manager(deps);
    let created = orchestrator.create(&request("night")).expect("create orchestrator");

    let same = orchestrator
        .move_to_node("night", None)
        .expect("moving home to home is a no-op");
    assert_eq!(same.session, created.session);
    assert_eq!(same.card.state, OrchestratorNodeState::Running);
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "an idempotent move restarted the host"
    );

    orchestrator.stop("night").expect("stop");
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    let resumed = orchestrator
        .move_to_node("night", None)
        .expect("a stopped record resumes on the same placement");
    assert_eq!(resumed.session, created.session);
    assert_eq!(resumed.card.state, OrchestratorNodeState::Running);
    assert_eq!(starts.load(Ordering::SeqCst), 2, "the stopped record did not resume");
}

#[test]
fn move_refuses_a_project_that_the_target_node_has_not_mapped() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(&tmp, AdapterRegistry::new());
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let now = deps.clock.now();
    let project = vowp_protocol::ProjectName::parse("checkout").expect("project");
    store
        .upsert_project(&vowp_protocol::Project {
            name: project.clone(),
            path: tmp.path().join("project").display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        })
        .expect("project row");
    let (orchestrator, starts, stops, _transport) = manager(deps);
    let mut req = request("night");
    req.project = Some(project);
    let created = orchestrator.create(&req).expect("create project orchestrator");

    let error = orchestrator
        .move_to_node("night", Some(node))
        .expect_err("an unmapped project is refused");
    assert!(matches!(error, CoreError::Refused(_)), "{error}");
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0, "a refused move stopped the host");
    let info = orchestrator.info("night").expect("still exists");
    assert_eq!(info.session, created.session);
    assert_eq!(info.card.node, None);
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
}

#[test]
fn move_writes_no_capsule_for_a_session_with_only_its_creation_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with("opencode", Arc::new(capsule_adapter())),
    );
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, _starts, _stops, _transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");

    orchestrator
        .move_to_node("night", Some(node))
        .expect("move to the agent");
    assert!(
        store
            .latest_capsule(&created.session)
            .expect("capsule lookup")
            .is_none(),
        "an unused orchestrator paid for a capsule turn"
    );
}

#[test]
fn move_continues_when_the_handoff_capsule_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let failing = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: "not a capsule draft".to_owned(),
        })],
    );
    let deps = deps(&tmp, AdapterRegistry::new().with("opencode", Arc::new(failing)));
    let store = store_of(&deps);
    let node = active_node(&store, "mac-studio", deps.clock.now());
    let (orchestrator, _starts, _stops, _transport) = manager(deps.clone());
    let created = orchestrator.create(&request("night")).expect("create orchestrator");
    append_user_event(&deps, &created.session);

    let moved = orchestrator
        .move_to_node("night", Some(node))
        .expect("a failed capsule does not block the move");
    assert_eq!(moved.session, created.session);
    assert!(moved.card.node.is_some());
}
