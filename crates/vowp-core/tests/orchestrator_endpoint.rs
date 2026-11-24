//! Orchestrator endpoint discovery: the home document, the local service file and the
//! security rule that no card or snapshot carries the app password.
//!
//! Every test uses a temporary node home, a private live `host.json`/`service.json` and a
//! process id that is this test process. No test starts an app, opens a socket, reaches a
//! node or reads the real home directory.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use support::fake_git::FakeGit;
use tempfile::TempDir;
use vowp_core::agent::dispatch;
use vowp_core::session::service::SessionService;
use vowp_core::surface::lease::{AcquireLease, LeaseGuard};
use vowp_core::surface::orchestrator::Orchestrator;
use vowp_core::{AdapterRegistry, CoreError, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    AgentFrame, Clock, EndpointAccess, FakeClock, FleetStatus, JobId, JobKind, JobState, NewJob, NewSession, Node,
    NodeName, NodeStatus, OrchestratorApp, OrchestratorLocation, OrchestratorNodeState, OwnershipMode, OwnershipState,
    Project, ProjectName, Role, SessionId, SessionRole, Surface, SurfaceId, SurfaceKind, Timestamp, VowpConfig,
};
use vowp_store::{NodePath, NodePaths, StateChange, Store, StoreError, hash_lease_token};
use vowp_tmux::fake::FakeWindows;

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NATIVE: &str = "ses_native_endpoint";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-19T12:00:00Z").expect("test timestamp")
}

fn session() -> SessionId {
    SessionId::parse(SESSION).expect("session id")
}

fn home_node() -> NodeName {
    NodeName::parse("home-1").expect("home node")
}

fn agent_node() -> NodeName {
    NodeName::parse("mac-studio").expect("agent node")
}

/// A temporary home with one registered project and one local node row.
struct Fixture {
    _home: TempDir,
    _project: TempDir,
    deps: Deps,
    clock: FakeClock,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let project = tempfile::tempdir().expect("temp project");
        let paths = NodePaths::new(home.path());
        paths.ensure().expect("node paths");
        let clock = FakeClock::at(ts());
        let store = Store::open(&paths)
            .expect("store opens")
            .with_clock(Arc::new(clock.clone()));
        let now = clock.now();
        let local = home_node();
        store
            .upsert_node(&Node {
                name: local.clone(),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: Some(now),
                created_at: now,
            })
            .expect("local node");
        let project_name = ProjectName::parse("vowp").expect("project name");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project_name.clone();
        config.project.path = Some(project.path().display().to_string());
        config.offload.enabled = true;
        config.offload.projects.insert(
            project_name.clone(),
            vowp_protocol::config::OffloadProjectConfig::default(),
        );
        store
            .upsert_project(&Project {
                name: project_name.clone(),
                path: project.path().display().to_string(),
                remote_url: Some("https://user:test@example.invalid/acme/vowp.git".to_owned()),
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        Store::create_project_db(&paths, &project_name).expect("project db");
        let deps = Deps {
            paths,
            config: Arc::new(config),
            node: local,
            clock: Arc::new(clock.clone()),
            log: Arc::new(vowp_protocol::MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(FakeGit::new().with_head_sha("0123456789abcdef0123456789abcdef01234567")),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        Self {
            _home: home,
            _project: project,
            deps,
            clock,
        }
    }

    fn store(&self) -> Store {
        Store::open_global(&self.deps.paths).expect("global store")
    }

    /// Registers one active, online agent node.
    fn add_agent_node(&self) {
        let store = self.store();
        let now = self.deps.clock.now();
        let node = agent_node();
        store
            .insert_pending_node(&node, &["darwin-arm64".to_owned()], now)
            .expect("pending node");
        store
            .set_node_status(&node, NodeStatus::Active, "", now)
            .expect("active node");
        store
            .touch_node(&node, now, Some("0.4.0"), Some("vowp-agent/1"), &serde_json::json!({}))
            .expect("node heartbeat");
    }
}

/// Writes one private `orchestrator.json` in the home's own shape.
fn write_orchestrator(deps: &Deps, name: &str, node: Option<NodeName>, state: OrchestratorNodeState) -> PathBuf {
    write_orchestrator_for(deps, name, node, state, None, &session())
}

/// Writes one private `orchestrator.json` bound to `project` and `session`.
fn write_orchestrator_for(
    deps: &Deps,
    name: &str,
    node: Option<NodeName>,
    state: OrchestratorNodeState,
    project: Option<&str>,
    session: &SessionId,
) -> PathBuf {
    let dir = deps.paths.root().join("orchestrators").join(name);
    create_private_dir(&dir);
    let value = serde_json::json!({
        "name": name,
        "app": OrchestratorApp::Opencode,
        "session_id": session,
        "project": project,
        "executor": null,
        "state": state,
        "created_at": deps.clock.now(),
        "join": {
            "desktop": format!("vowp orchestrator resume {name}"),
            "phone": format!("vowp chat {session}"),
            "chat": format!("vowp chat --agent --session {session}"),
        },
        "surface_id": null,
        "node": node,
        "last_request_id": null,
    });
    write_private(
        &dir.join("orchestrator.json"),
        &serde_json::to_vec(&value).expect("state json"),
    );
    dir
}

/// Writes a live `host.json` shaped like the opencode host's own file.
fn write_live_host(dir: &Path, port: u16) {
    let value = serde_json::json!({
        "kind": "opencode",
        "name": "main",
        "pid": std::process::id(),
        "port": port,
        "url": format!("http://127.0.0.1:{port}"),
        "started_at": ts(),
        "workdir": dir.display().to_string(),
        "service_json": dir.join(".opencode/state/opencode/service.json").display().to_string(),
        "native_session_id": NATIVE,
        "vowp_session": SESSION,
    });
    write_private(&dir.join("host.json"), &serde_json::to_vec(&value).expect("host json"));
}

/// Writes the private opencode `service.json` below `dir`.
fn write_service(dir: &Path, url: &str, port: u16, password: &str) {
    let path = dir.join(".opencode/state/opencode/service.json");
    if let Some(parent) = path.parent() {
        create_private_dir(parent);
    }
    let value = serde_json::json!({
        "version": "2.0.28",
        "url": url,
        "port": port,
        "pid": std::process::id(),
        "password": password,
    });
    write_private(&path, &serde_json::to_vec(&value).expect("service json"));
}

fn create_private_dir(dir: &Path) {
    fs::create_dir_all(dir).expect("private dir");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("private dir mode");
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("private file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private file mode");
}

fn manager(fixture: &Fixture) -> Orchestrator {
    Orchestrator::new(fixture.deps.clone())
}

#[test]
fn orchestrator_endpoint_home_is_metadata_only_for_a_node() {
    let fixture = Fixture::new();
    fixture.add_agent_node();
    let dir = write_orchestrator(&fixture.deps, "mac", Some(agent_node()), OrchestratorNodeState::Running);
    // Bait: even a live-looking local host and service file must be ignored for a node
    // orchestrator. The home never reads a node's files.
    write_live_host(&dir, 45454);
    let password = format!("vowp-endpoint-{}", "A".repeat(24));
    write_service(&dir, "http://127.0.0.1:45454", 45454, &password);

    let endpoint = manager(&fixture).endpoint("mac", None).expect("node endpoint");
    assert_eq!(endpoint.name, "mac");
    assert_eq!(endpoint.app, OrchestratorApp::Opencode);
    assert_eq!(endpoint.session_id, session());
    assert_eq!(endpoint.location, OrchestratorLocation::Node);
    assert_eq!(endpoint.node.as_ref().map(NodeName::as_str), Some("mac-studio"));
    assert_eq!(endpoint.state, OrchestratorNodeState::Running);
    assert!(endpoint.url.is_none(), "a node document has no url: {endpoint:?}");
    assert!(
        endpoint.native_session_id.is_none(),
        "a node document has no native id: {endpoint:?}"
    );
    assert!(endpoint.auth.is_none(), "a node document has no auth: {endpoint:?}");
}

#[test]
fn orchestrator_endpoint_home_reads_the_live_local_service_file() {
    let fixture = Fixture::new();
    let dir = write_orchestrator(&fixture.deps, "main", None, OrchestratorNodeState::Running);
    write_live_host(&dir, 43111);
    let password = format!("vowp-endpoint-{}", "B".repeat(24));
    write_service(&dir, "http://localhost:43111", 43111, &password);

    let orchestrator = manager(&fixture);
    let endpoint = orchestrator.endpoint("main", None).expect("home endpoint");
    assert_eq!(endpoint.location, OrchestratorLocation::Home);
    assert_eq!(endpoint.node, None);
    assert_eq!(endpoint.session_id, session());
    assert_eq!(endpoint.state, OrchestratorNodeState::Running);
    assert_eq!(endpoint.url.as_deref(), Some("http://127.0.0.1:43111"));
    assert_eq!(endpoint.native_session_id.as_deref(), Some(NATIVE));
    let auth = endpoint.auth.as_ref().expect("live local auth");
    assert_eq!(auth.header, "Authorization");
    assert_eq!(auth.scheme, "Basic");
    assert_eq!(auth.username, "opencode");
    assert_eq!(auth.password, password);

    // The explicit endpoint document is the only place the password appears.
    assert!(
        serde_json::to_string(&endpoint)
            .expect("endpoint JSON")
            .contains(&password),
        "the explicit endpoint document carries the password"
    );
    let cards = orchestrator.list().expect("orchestrator list");
    let cards_json =
        serde_json::to_string(&cards.iter().map(|info| &info.card).collect::<Vec<_>>()).expect("cards JSON");
    assert!(
        !cards_json.contains(&password),
        "orchestrator ls never carries the password"
    );
    assert!(
        !format!("{endpoint:?}").contains(&password),
        "Debug redacts the password"
    );
}

#[test]
fn orchestrator_endpoint_stopped_or_unknown_has_no_credentials() {
    let fixture = Fixture::new();
    let dir = write_orchestrator(&fixture.deps, "stopped", None, OrchestratorNodeState::Stopped);
    // A stale service file from an earlier run must not be treated as a live endpoint.
    let stale = format!("vowp-endpoint-{}", "C".repeat(24));
    write_service(&dir, "http://127.0.0.1:45555", 45555, &stale);
    let orchestrator = manager(&fixture);
    let endpoint = orchestrator.endpoint("stopped", None).expect("stopped endpoint");
    assert_eq!(endpoint.location, OrchestratorLocation::Home);
    assert_eq!(endpoint.state, OrchestratorNodeState::Stopped);
    assert!(endpoint.url.is_none(), "a stopped host has no url");
    assert!(endpoint.native_session_id.is_none(), "a stopped host has no native id");
    assert!(endpoint.auth.is_none(), "a stopped host has no auth");
    assert!(!serde_json::to_string(&endpoint).expect("stopped JSON").contains(&stale));

    let unknown = orchestrator
        .endpoint("missing", None)
        .expect_err("unknown is not found");
    let text = unknown.to_string();
    assert!(
        text.contains("orchestrator missing"),
        "the error names the orchestrator: {text}"
    );

    // A corrupt local record is a typed, path-free failure naming the orchestrator.
    let broken = write_orchestrator(&fixture.deps, "broken", None, OrchestratorNodeState::Running);
    write_private(&broken.join("orchestrator.json"), b"{\"name\":");
    let error = orchestrator
        .endpoint("broken", None)
        .expect_err("a corrupt record is typed");
    let text = error.to_string();
    assert!(
        text.contains("orchestrator broken"),
        "the error names the orchestrator: {text}"
    );
    for forbidden in [broken.display().to_string(), "orchestrator.json".to_owned()] {
        assert!(!text.contains(&forbidden), "the error leaked {forbidden:?}: {text}");
    }

    // A corrupt service file is a typed, path-free failure.
    let corrupt = write_orchestrator(&fixture.deps, "corrupt", None, OrchestratorNodeState::Running);
    write_live_host(&corrupt, 45666);
    let path = corrupt.join(".opencode/state/opencode/service.json");
    if let Some(parent) = path.parent() {
        create_private_dir(parent);
    }
    write_private(&path, b"{\"url\":\"http://127.0.0.1:45666\",\"password\":");
    let error = orchestrator
        .endpoint("corrupt", None)
        .expect_err("a corrupt service file is typed");
    let text = error.to_string();
    assert!(
        text.contains("orchestrator corrupt"),
        "the error names the orchestrator and failure class: {text}"
    );
    for forbidden in [
        corrupt.display().to_string(),
        path.display().to_string(),
        "45666".to_owned(),
        "service.json".to_owned(),
    ] {
        assert!(!text.contains(&forbidden), "the error leaked {forbidden:?}: {text}");
    }

    // A service file wider than 0600 is not trusted as local auth material.
    let wide = serde_json::json!({
        "version": "2.0.28",
        "url": "http://127.0.0.1:45666",
        "port": 45666,
        "pid": std::process::id(),
        "password": format!("vowp-endpoint-{}", "Z".repeat(24)),
    });
    fs::write(&path, serde_json::to_vec(&wide).expect("wide service json")).expect("wide service");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("wide service mode");
    let error = orchestrator
        .endpoint("corrupt", None)
        .expect_err("a wide service file is typed");
    let text = error.to_string();
    assert!(
        text.contains("orchestrator corrupt"),
        "the error names the orchestrator: {text}"
    );
    assert!(!text.contains("45666"), "the error carries no port: {text}");
    assert!(
        !text.contains(&corrupt.display().to_string()),
        "the error carries no path: {text}"
    );

    // A non-loopback URL is refused rather than returned as an endpoint.
    let public = write_orchestrator(&fixture.deps, "public", None, OrchestratorNodeState::Running);
    write_live_host(&public, 45777);
    write_service(&public, "http://0.0.0.0:45777", 45777, &stale);
    let error = orchestrator
        .endpoint("public", None)
        .expect_err("non-loopback URL is refused");
    let text = error.to_string();
    assert!(!text.contains("0.0.0.0"), "the error carries no address: {text}");
    assert!(!text.contains(&stale), "the error carries no password");
}

#[test]
fn orchestrator_card_snapshot_and_ls_never_contain_the_endpoint_password() {
    let fixture = Fixture::new();
    let dir = write_orchestrator(&fixture.deps, "main", None, OrchestratorNodeState::Running);
    write_live_host(&dir, 43222);
    let password = format!("vowp-endpoint-{}", "D".repeat(24));
    write_service(&dir, "http://127.0.0.1:43222", 43222, &password);

    let orchestrator = manager(&fixture);
    let endpoint = orchestrator.endpoint("main", None).expect("live endpoint");
    assert_eq!(endpoint.auth.as_ref().expect("auth").password, password);

    let cards = orchestrator.list().expect("orchestrator list");
    assert_eq!(cards.len(), 1, "one orchestrator card");
    let cards_json =
        serde_json::to_string(&cards.iter().map(|info| &info.card).collect::<Vec<_>>()).expect("cards JSON");
    assert!(
        !cards_json.contains(&password),
        "orchestrator ls never carries the password"
    );

    let store = fixture.store();
    let snapshot = vowp_core::fleet::build_snapshot(&fixture.deps, &store, fixture.deps.clock.now()).expect("snapshot");
    assert_eq!(snapshot.orchestrators.len(), 1, "the snapshot has the card");
    let snapshot_json = serde_json::to_string(&snapshot).expect("snapshot JSON");
    assert!(
        !snapshot_json.contains(&password),
        "snapshot.json never carries the password"
    );
    let status = FleetStatus {
        snapshot,
        source: "local".to_owned(),
        fetched_at: fixture.deps.clock.now(),
        stale: false,
    };
    let status_json = serde_json::to_string(&status).expect("fleet status JSON");
    assert!(
        !status_json.contains(&password),
        "fleet status never carries the password"
    );
}

// ---------------------------------------------------------------------------
// Owner/access read model and the attach write path (v0.5.1 Slice 2a)
// ---------------------------------------------------------------------------

const PROJECT: &str = "vowp";
const ENGINE_PASSWORD: &str = "vowp-endpoint-ATTACHSECRET0123456789";

/// A live home-hosted opencode orchestrator bound to a real orchestrator session.
fn live_orchestrator(fixture: &Fixture, name: &str) -> SessionId {
    let session = create_session(fixture);
    let dir = write_orchestrator_for(
        &fixture.deps,
        name,
        None,
        OrchestratorNodeState::Running,
        Some(PROJECT),
        &session,
    );
    write_live_host(&dir, 43555);
    write_service(&dir, "http://127.0.0.1:43555", 43555, ENGINE_PASSWORD);
    session
}

fn create_session(fixture: &Fixture) -> SessionId {
    SessionService::new(fixture.deps.clone())
        .create_session(NewSession {
            project: Some(ProjectName::parse(PROJECT).expect("project")),
            job_id: None,
            role: SessionRole::Orchestrator,
            title: "orchestrator main".to_owned(),
            cwd: PathBuf::from("."),
            parent: None,
            executor: None,
        })
        .expect("session")
}

fn project_store(fixture: &Fixture) -> Store {
    fixture.deps.open_store().expect("project store")
}

fn conflict_text(error: CoreError) -> String {
    match error {
        CoreError::Store(StoreError::Conflict(text)) => text,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn endpoint_reports_owner_and_read_only_access_without_a_surface() {
    let fixture = Fixture::new();
    let session = live_orchestrator(&fixture, "main");
    let orchestrator = manager(&fixture);

    let none = orchestrator.endpoint("main", None).expect("endpoint without an owner");
    assert!(none.owner.is_none(), "no lease, no owner");
    assert_eq!(none.access, EndpointAccess::ReadOnly);

    let guard = orchestrator
        .attach("main", "ui-one", Some("StackSpirit"), false)
        .expect("attach");
    let stranger = SurfaceId::new();
    for caller in [None, Some(&stranger)] {
        let endpoint = orchestrator.endpoint("main", caller).expect("endpoint with an owner");
        assert_eq!(
            endpoint.access,
            EndpointAccess::ReadOnly,
            "caller {caller:?} may not write"
        );
        let owner = endpoint.owner.expect("the owner is reported");
        assert_eq!(owner.session_id, session);
        assert_eq!(owner.holder, "ui-one");
        assert_eq!(owner.kind, SurfaceKind::DesktopUi);
        assert_eq!(owner.label.as_deref(), Some("StackSpirit"));
        assert_eq!(owner.mode, OwnershipMode::Interactive);
        assert_eq!(owner.state, OwnershipState::Active);
        // The live engine details stay available to the read path.
        assert!(endpoint.url.is_some() && endpoint.auth.is_some());
    }
    drop(guard);
}

#[test]
fn endpoint_tells_the_owner_surface_it_may_write() {
    let fixture = Fixture::new();
    live_orchestrator(&fixture, "main");
    let orchestrator = manager(&fixture);
    let guard = orchestrator.attach("main", "ui-one", None, false).expect("attach");
    let surface = guard.grant().surface_id.clone();

    let endpoint = orchestrator.endpoint("main", Some(&surface)).expect("owner endpoint");
    assert_eq!(endpoint.access, EndpointAccess::Owner);
    assert_eq!(
        endpoint.owner.as_ref().map(|owner| owner.surface_id.clone()),
        Some(Some(surface.clone()))
    );

    // A stale owner (no heartbeat for 60 s) may not write any more.
    fixture.clock.advance(std::time::Duration::from_secs(61));
    let stale = orchestrator.endpoint("main", Some(&surface)).expect("stale endpoint");
    assert_eq!(stale.access, EndpointAccess::ReadOnly);
    assert_eq!(stale.owner.expect("owner still shown").state, OwnershipState::Stale);
    drop(guard);
}

#[test]
fn endpoint_reports_the_owner_for_a_node_hosted_record_without_credentials() {
    let fixture = Fixture::new();
    fixture.add_agent_node();
    let session = create_session(&fixture);
    let dir = write_orchestrator_for(
        &fixture.deps,
        "mac",
        Some(agent_node()),
        OrchestratorNodeState::Running,
        Some(PROJECT),
        &session,
    );
    write_live_host(&dir, 45454);
    write_service(&dir, "http://127.0.0.1:45454", 45454, ENGINE_PASSWORD);
    // The paired node surface owns the session on the home.
    let store = project_store(&fixture);
    let now = fixture.clock.now();
    let surface = Surface {
        id: SurfaceId::new(),
        kind: SurfaceKind::Opencode,
        node: agent_node(),
        holder: "opencode".to_owned(),
        app: Some("opencode".to_owned()),
        cwd: None,
        metadata: serde_json::json!({}),
        created_at: now,
        last_seen_at: now,
    };
    let node_guard = LeaseGuard::acquire(
        &fixture.deps,
        &store,
        &AcquireLease {
            session,
            surface: surface.clone(),
            takeover: false,
        },
    )
    .expect("node lease");

    let orchestrator = manager(&fixture);
    let endpoint = orchestrator.endpoint("mac", Some(&surface.id)).expect("node endpoint");
    assert_eq!(endpoint.location, OrchestratorLocation::Node);
    let owner = endpoint.owner.as_ref().expect("the home reports the node owner");
    assert_eq!(owner.holder, "opencode");
    assert_eq!(owner.node, agent_node());
    assert_eq!(
        endpoint.access,
        EndpointAccess::Owner,
        "the live owner surface may write"
    );
    assert!(endpoint.url.is_none() && endpoint.native_session_id.is_none() && endpoint.auth.is_none());
    assert!(
        !serde_json::to_string(&endpoint)
            .expect("json")
            .contains(ENGINE_PASSWORD)
    );
    drop(node_guard);
}

#[test]
fn attach_creates_a_desktop_ui_surface_and_returns_a_one_time_token() {
    let fixture = Fixture::new();
    let session = live_orchestrator(&fixture, "main");
    let orchestrator = manager(&fixture);
    let guard = orchestrator
        .attach("main", "opencode-desktop", Some("StackSpirit · macbook"), false)
        .expect("attach");
    let grant = guard.grant().clone();

    assert_eq!(grant.token.len(), 52, "the existing two-ULID lease token");
    assert_eq!(grant.owner.kind, SurfaceKind::DesktopUi);
    assert_eq!(grant.owner.holder, "opencode-desktop");
    assert_eq!(grant.owner.session_id, session);
    assert_eq!(grant.owner.surface_id.as_ref(), Some(&grant.surface_id));
    assert_eq!(grant.owner.label.as_deref(), Some("StackSpirit · macbook"));
    assert_eq!(grant.endpoint.access, EndpointAccess::Owner);
    assert_eq!(grant.endpoint.owner.as_ref(), Some(&grant.owner));
    assert_eq!(grant.endpoint.url.as_deref(), Some("http://127.0.0.1:43555"));
    assert_eq!(
        grant.expires_at,
        grant.owner.heartbeat_at.plus(std::time::Duration::from_secs(60))
    );

    // The store holds only the token hash, and the token is the lease token of this row.
    let store = project_store(&fixture);
    let record = store.ownership(&session).expect("ownership").expect("owner row");
    assert_eq!(record.lease.token, hash_lease_token(&grant.token));
    assert_ne!(record.lease.token, grant.token, "plaintext is never stored");
    assert_eq!(record.lease.surface_kind, SurfaceKind::DesktopUi);
    let surface = store
        .surface(&grant.surface_id)
        .expect("surface read")
        .expect("surface row");
    assert_eq!(surface.kind, SurfaceKind::DesktopUi);
    assert_eq!(surface.holder, "opencode-desktop");

    // The token and the engine password never reach Debug or the project log.
    let debug = format!("{grant:?} {guard:?}");
    assert!(!debug.contains(&grant.token), "Debug redacts the token: {debug}");
    assert!(!debug.contains(ENGINE_PASSWORD), "Debug redacts the password");
    drop(guard);
}

#[test]
fn attach_conflicts_with_a_live_owner_and_takeover_replaces_it() {
    let fixture = Fixture::new();
    let session = live_orchestrator(&fixture, "main");
    let orchestrator = manager(&fixture);
    let first = orchestrator
        .attach("main", "ui-one", None, false)
        .expect("first attach");
    let first_token = first.grant().token.clone();

    let error = orchestrator
        .attach("main", "ui-two", None, false)
        .expect_err("a live owner refuses a second attach");
    let text = conflict_text(error);
    assert!(text.contains("ui-one"), "the conflict names the holder: {text}");
    assert!(!text.contains(&first_token), "the conflict never carries a token");
    let store = project_store(&fixture);
    let before = store.ownership(&session).expect("ownership").expect("owner");
    assert_eq!(before.lease.holder, "ui-one", "the refused attach changed nothing");

    let second = orchestrator
        .attach("main", "ui-two", None, true)
        .expect("takeover attach");
    let after = store.ownership(&session).expect("ownership").expect("owner");
    assert_eq!(after.lease.holder, "ui-two");
    assert!(after.epoch > before.epoch, "the takeover bumps the epoch");
    assert_eq!(after.lease.token, hash_lease_token(&second.grant().token));
    vowp_core::surface::check_lease_with(&store, &session, Some(&first_token))
        .expect_err("the replaced owner is read-only");
    vowp_core::surface::check_lease_with(&store, &session, Some(&second.grant().token)).expect("the new owner writes");
    drop(first); // must not release the new owner's row
    assert!(store.ownership(&session).expect("ownership").is_some());
    drop(second);
}

#[test]
fn attach_replaces_a_stale_interactive_owner_without_a_flag() {
    let fixture = Fixture::new();
    let session = live_orchestrator(&fixture, "main");
    let orchestrator = manager(&fixture);
    let first = orchestrator
        .attach("main", "ui-one", None, false)
        .expect("first attach");
    fixture.clock.advance(std::time::Duration::from_secs(61));
    let second = orchestrator
        .attach("main", "ui-two", None, false)
        .expect("stale owner is replaced");
    let store = project_store(&fixture);
    assert_eq!(
        store
            .ownership(&session)
            .expect("ownership")
            .expect("owner")
            .lease
            .holder,
        "ui-two"
    );
    drop(first);
    drop(second);
}

/// Enrolls the agent node, queues one job, and returns the claimed job-owned session.
fn claim_job_session(fixture: &Fixture) -> (SessionId, JobId) {
    fixture.add_agent_node();
    let store = Store::open(&fixture.deps.paths)
        .expect("store")
        .with_clock(Arc::new(fixture.clock.clone()));
    let node = agent_node();
    let project = ProjectName::parse(PROJECT).expect("project");
    store
        .upsert_node_path(&NodePath {
            node: node.clone(),
            project: project.clone(),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: "/srv/vowp-wt".to_owned(),
            allow: Vec::new(),
            updated_at: ts(),
        })
        .expect("path map");
    store
        .touch_node(
            &node,
            fixture.clock.now(),
            Some("0.3.0"),
            Some(vowp_protocol::AGENT_PROTOCOL),
            &serde_json::json!({
                "os": "linux", "arch": "x86_64", "agent_version": "0.3.0",
                "harnesses": {"opencode": "1.0.0"}, "git_version": "git version 2.47.0",
                "labels": ["linux"], "secret_names": [], "projects": [PROJECT],
                "capacity": {"cpu_cores": 8, "load1": 0.1, "mem_total_bytes": 17179869184u64,
                    "mem_free_bytes": 8589934592u64, "disk_free_bytes": 107374182400u64,
                    "busy_slots": 0, "max_slots": 4, "on_battery": false, "thermal_state": "nominal"},
                "pending_spool_bytes": 0
            }),
        )
        .expect("touch node");
    let job = store
        .insert_job(
            &project,
            &NewJob {
                id: JobId::parse("job-a").expect("job id"),
                title: "job a".to_owned(),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: Some("worker".to_owned()),
                needs: Vec::new(),
                owns: vec!["src/**".to_owned()],
                reads: Vec::new(),
                gates: None,
                priority: None,
                loop_: None,
                brief_text: None,
            },
            "test",
        )
        .expect("insert job");
    store
        .set_job_placement(&job.id, vowp_protocol::Placement::Any, ts())
        .expect("job placement");
    store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready job");
    dispatch::enqueue_ready_jobs(&fixture.deps, &store, fixture.clock.now()).expect("enqueue");
    let claimed = dispatch::claim_for_node(&fixture.deps, &store, &node, fixture.clock.now()).expect("claim");
    let AgentFrame::AssignRun { assignment, .. } = &claimed[0].frame else {
        panic!("expected AssignRun");
    };
    (assignment.session_id.clone(), job.id)
}

fn bind_orchestrator_to(fixture: &Fixture, name: &str, session: &SessionId) {
    let dir = write_orchestrator_for(
        &fixture.deps,
        name,
        None,
        OrchestratorNodeState::Running,
        Some(PROJECT),
        session,
    );
    write_live_host(&dir, 43666);
    write_service(&dir, "http://127.0.0.1:43666", 43666, ENGINE_PASSWORD);
}

#[test]
fn attach_fences_a_live_job_owner_and_leaves_the_job_ready() {
    let fixture = Fixture::new();
    let (session, job_id) = claim_job_session(&fixture);
    bind_orchestrator_to(&fixture, "main", &session);
    let store = project_store(&fixture);
    let run = store
        .ownership(&session)
        .expect("ownership")
        .expect("the job owns the session");
    assert_eq!(run.mode, OwnershipMode::Job);
    assert!(run.is_live_at(fixture.clock.now()));

    let orchestrator = manager(&fixture);
    let error = orchestrator
        .attach("main", "ui-one", None, false)
        .expect_err("a live job owner needs an explicit takeover");
    assert!(conflict_text(error).contains("session"), "the existing Conflict text");
    assert_eq!(
        store.ownership(&session).expect("ownership").expect("owner").mode,
        OwnershipMode::Job,
        "the refused attach left the job running"
    );

    let guard = orchestrator
        .attach("main", "ui-one", None, true)
        .expect("takeover attach");
    let owner = store.ownership(&session).expect("ownership").expect("owner");
    assert_eq!(owner.mode, OwnershipMode::Interactive);
    assert_eq!(owner.lease.surface_kind, SurfaceKind::DesktopUi);
    assert_eq!(owner.lease.token, hash_lease_token(&guard.grant().token));
    let job = store.require_job(&job_id).expect("job");
    assert_eq!(job.state, JobState::Ready, "the fenced job stays ready for a new run");
    let rows = store.assignments_for_job(&job_id).expect("assignments");
    assert!(
        rows.iter()
            .all(|row| row.state == vowp_store::repo_offload::AgentAssignmentState::Lost),
        "the run is fenced through lose: {rows:?}"
    );
    let events = vowp_store::session_log::SessionLog::open_read(&fixture.deps.paths, &session)
        .expect("log")
        .read_all()
        .expect("events");
    let notes = serde_json::to_string(&events).expect("events json");
    // `lose` records its own fence reason ("session .. was taken over by ui-one"); the
    // takeover note itself ("taken over by ui-one from <job holder>") is written once.
    assert_eq!(
        notes.matches("taken over by ui-one from").count(),
        1,
        "the takeover note is written once: {notes}"
    );
    drop(guard);
}

#[test]
fn attach_replaces_an_expired_job_owner_without_a_takeover_flag() {
    let fixture = Fixture::new();
    let (session, job_id) = claim_job_session(&fixture);
    bind_orchestrator_to(&fixture, "main", &session);
    fixture
        .clock
        .advance(dispatch::RUN_LEASE_TTL + std::time::Duration::from_secs(1));
    let guard = manager(&fixture)
        .attach("main", "ui-one", None, false)
        .expect("an expired job owner is fenced and replaced");
    let store = project_store(&fixture);
    assert_eq!(
        store.ownership(&session).expect("ownership").expect("owner").mode,
        OwnershipMode::Interactive
    );
    assert_eq!(store.require_job(&job_id).expect("job").state, JobState::Ready);
    drop(guard);
}

#[test]
fn attach_refuses_a_node_hosted_orchestrator() {
    let fixture = Fixture::new();
    fixture.add_agent_node();
    let session = create_session(&fixture);
    write_orchestrator_for(
        &fixture.deps,
        "mac",
        Some(agent_node()),
        OrchestratorNodeState::Running,
        Some(PROJECT),
        &session,
    );
    let error = manager(&fixture)
        .attach("mac", "ui-one", None, false)
        .expect_err("a node-hosted orchestrator cannot be attached from the home");
    let CoreError::Refused(text) = error else {
        panic!("expected Refused, got {error:?}");
    };
    assert!(text.contains("mac") && text.contains("node"), "typed message: {text}");
    let store = project_store(&fixture);
    assert!(
        store.ownership(&session).expect("ownership").is_none(),
        "no owner was created"
    );
}

#[test]
fn attach_refuses_a_corrupt_service_file_with_resume_guidance() {
    let fixture = Fixture::new();
    let session = create_session(&fixture);
    let dir = write_orchestrator_for(
        &fixture.deps,
        "corrupt",
        None,
        OrchestratorNodeState::Running,
        Some(PROJECT),
        &session,
    );
    write_live_host(&dir, 43888);
    let path = dir.join(".opencode/state/opencode/service.json");
    if let Some(parent) = path.parent() {
        create_private_dir(parent);
    }
    write_private(&path, b"{\"url\":\"http://127.0.0.1:43888\",\"password\":");
    let error = manager(&fixture)
        .attach("corrupt", "ui-one", None, false)
        .expect_err("a corrupt service file has no usable endpoint");
    let CoreError::Refused(text) = error else {
        panic!("expected Refused, got {error:?}");
    };
    assert!(text.contains("orchestrator corrupt"), "names the orchestrator: {text}");
    assert!(
        text.contains("vowp orchestrator resume corrupt"),
        "suggests resume: {text}"
    );
    for forbidden in [dir.display().to_string(), "service.json".to_owned(), "43888".to_owned()] {
        assert!(!text.contains(&forbidden), "the refusal leaked {forbidden:?}: {text}");
    }
    let store = project_store(&fixture);
    assert!(
        store.ownership(&session).expect("ownership").is_none(),
        "a corrupt endpoint creates no owner"
    );
}

#[test]
fn attach_refuses_a_stopped_host_and_a_bad_label_leaves_no_owner() {
    let fixture = Fixture::new();
    let session = create_session(&fixture);
    write_orchestrator_for(
        &fixture.deps,
        "stopped",
        None,
        OrchestratorNodeState::Stopped,
        Some(PROJECT),
        &session,
    );
    let orchestrator = manager(&fixture);
    let error = orchestrator
        .attach("stopped", "ui-one", None, false)
        .expect_err("a stopped host has no endpoint");
    let CoreError::Refused(text) = error else {
        panic!("expected Refused, got {error:?}");
    };
    assert!(text.contains("orchestrator stopped"), "names the orchestrator: {text}");
    assert!(
        text.contains("vowp orchestrator resume stopped"),
        "suggests resume: {text}"
    );

    let live = live_orchestrator(&fixture, "main");
    let error = orchestrator
        .attach("main", "ui-one", Some(&"x".repeat(200)), false)
        .expect_err("an over-long label is invalid");
    assert!(!error.to_string().contains(ENGINE_PASSWORD));
    let store = project_store(&fixture);
    assert!(
        store.ownership(&live).expect("ownership").is_none(),
        "a failed attach releases the lease"
    );
}

#[test]
fn attach_holds_the_lease_until_released() {
    let fixture = Fixture::new();
    let session = live_orchestrator(&fixture, "main");
    let guard = manager(&fixture).attach("main", "ui-one", None, false).expect("attach");
    let store = project_store(&fixture);
    assert!(
        store.ownership(&session).expect("ownership").is_some(),
        "the guard holds the row"
    );
    guard.release().expect("release");
    assert!(
        store.ownership(&session).expect("ownership").is_none(),
        "release deletes the row"
    );
}
