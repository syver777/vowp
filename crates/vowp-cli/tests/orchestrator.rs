//! Orchestrator role and app-join tests.
//!
//! Every test uses a temporary `VOWP_HOME` and project. App hosts are fakes: they write the
//! same `host.json` shape as the real hosts and never open a socket, start opencode or
//! Claude, or read the real home. The `chat` app has no host and is exercised through the
//! real CLI and its project-local state.

// This file is compiled as a `vowp-cli` unit-test module through the owned
// `src/cmd/orchestrator.rs`; the workspace package root has no auto-discovered
// `tests/` directory, and the job may not edit the shared root `Cargo.toml`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tempfile::TempDir;
use vowp_protocol::{
    JoinInstructions, Lease, Node, NodeName, OrchestratorApp, Project, ProjectName, SessionRole, Surface, SurfaceId,
    SurfaceKind, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, Store};

use vowp_core::deps::Deps;
use vowp_core::surface::mirror::MirrorCursor;
use vowp_core::surface::native::{AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, TranscriptEntry};
use vowp_core::surface::orchestrator::{NewOrchestrator, Orchestrator};

use crate::{BufferIo, Ctx, Global};

/// A temporary node home and project, with no tmux and no real harness.
struct TestEnv {
    home: TempDir,
    project: TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let project = tempfile::tempdir().expect("temp project");
        let config = VowpConfig::with_defaults_for_tests();
        std::fs::write(
            project.path().join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config serialises"),
        )
        .expect("write vowp.toml");
        std::fs::create_dir_all(project.path().join(".vowp/jobs")).expect("jobs dir");

        let paths = NodePaths::new(home.path());
        let store = Store::open(&paths).expect("store opens");
        let now = store.clock().now();
        store
            .upsert_node(&Node {
                name: NodeName::parse("local").expect("local slug"),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: Some(now),
                created_at: now,
            })
            .expect("node row");
        store
            .upsert_project(&Project {
                name: config.project.name.clone(),
                path: project.path().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        Self { home, project }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn project_path(&self) -> &Path {
        self.project.path()
    }

    fn global(&self, json: bool) -> Global {
        Global {
            json,
            home: Some(self.home.path().to_path_buf()),
            project: Some(self.project.path().display().to_string()),
            ..Global::default()
        }
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut full = vec![
            "vowp".to_owned(),
            "--home".to_owned(),
            self.home.path().display().to_string(),
            "--project".to_owned(),
            self.project.path().display().to_string(),
        ];
        full.extend(args.iter().map(|value| (*value).to_owned()));
        let mut io = BufferIo::new("");
        let code = crate::run(full, &mut io);
        (code, io.stdout(), io.stderr())
    }

    fn run_json(&self, args: &[&str]) -> (i32, Value, String) {
        let mut full = vec!["--json"];
        full.extend(args.iter().copied());
        let (code, stdout, stderr) = self.run(&full);
        let value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
        (code, value, stderr)
    }

    fn store(&self) -> Store {
        Store::open(&NodePaths::new(self.home.path())).expect("store opens")
    }
}

/// A hermetic app host that records starts and stops and writes a live `host.json`.
struct FakeHost {
    kind: OrchestratorApp,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    entries: Vec<TranscriptEntry>,
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
                entries: Vec::new(),
            },
            starts,
            stops,
        )
    }

    /// Returns a fake host that publishes one transcript entry on its first mirror pass.
    fn with_entry(mut self, native_id: &str) -> Self {
        self.entries = vec![TranscriptEntry {
            native_id: native_id.to_owned(),
            kind: vowp_core::surface::native::TranscriptKind::UserMessage,
            ts: Timestamp::now(),
            text: Some("hello from the fake app".to_owned()),
            tool: None,
            input: None,
            output: None,
            path: None,
            call_id: None,
            model: None,
            native_session: None,
            ok: None,
            tokens: None,
            usd: None,
        }];
        self
    }

    fn host_file(workdir: &std::path::Path) -> std::path::PathBuf {
        workdir.join("host.json")
    }
}

impl AppHost for FakeHost {
    fn kind(&self) -> OrchestratorApp {
        self.kind
    }

    fn start(&self, req: &HostRequest) -> Result<AppHandle, vowp_core::CoreError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(&req.workdir).expect("fake host workdir");
        let port = match self.kind {
            OrchestratorApp::Opencode => Some(4096_u16),
            OrchestratorApp::ClaudeCode | OrchestratorApp::Chat => None,
        };
        let state = serde_json::json!({
            "kind": match self.kind {
                OrchestratorApp::Opencode => "opencode",
                OrchestratorApp::ClaudeCode => "claude-code",
                OrchestratorApp::Chat => "chat",
            },
            "name": req.name,
            "pid": std::process::id(),
            "port": port,
            "started_at": Timestamp::now(),
            "workdir": req.workdir.display().to_string(),
            "vowp_session": req.session.to_string(),
        });
        std::fs::write(Self::host_file(&req.workdir), state.to_string()).expect("fake host.json");
        Ok(AppHandle {
            kind: self.kind,
            pid: std::process::id(),
            port,
            started_at: Timestamp::now(),
            workdir: req.workdir.clone(),
        })
    }

    fn stop(&self, handle: &AppHandle) -> Result<(), vowp_core::CoreError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        let _ = std::fs::remove_file(Self::host_file(&handle.workdir));
        Ok(())
    }

    fn health(&self, handle: &AppHandle) -> Result<HostHealth, vowp_core::CoreError> {
        Ok(HostHealth {
            ok: Self::host_file(&handle.workdir).is_file(),
            detail: "fake host".to_owned(),
        })
    }

    fn join_instructions(&self, _handle: &AppHandle) -> Result<JoinInstructions, vowp_core::CoreError> {
        Ok(JoinInstructions {
            desktop: "FAKE HOST desktop: vowp tunnel open fake --remote-port 4096; opencode pair one-time"
                .to_owned(),
            phone: "FAKE HOST phone: ssh -t <home-ssh-target> opencode --server http://127.0.0.1:4096 --session fake-native"
                .to_owned(),
            chat: "vowp chat fake-native".to_owned(),
        })
    }

    fn transcript_since(
        &self,
        _handle: &AppHandle,
        cursor: &MirrorCursor,
    ) -> Result<TranscriptBatch, vowp_core::CoreError> {
        let entries = if cursor.last_native_id.is_some() {
            Vec::new()
        } else {
            self.entries.clone()
        };
        Ok(TranscriptBatch {
            entries,
            cursor: cursor.clone(),
        })
    }
}

/// Builds the core dependencies from the test environment.
fn deps(env: &TestEnv) -> Deps {
    let io = BufferIo::new("");
    let ctx = Ctx::build(&env.global(false), &io).expect("test context");
    ctx.deps().clone()
}

/// Builds an orchestrator manager with both fake hosts.
fn manager(
    deps: Deps,
) -> (
    Orchestrator,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (opencode, opencode_starts, opencode_stops) = FakeHost::new(OrchestratorApp::Opencode);
    let (claude, claude_starts, claude_stops) = FakeHost::new(OrchestratorApp::ClaudeCode);
    (
        Orchestrator::with_hosts(deps, vec![Arc::new(opencode), Arc::new(claude)]),
        opencode_starts,
        opencode_stops,
        claude_starts,
        claude_stops,
    )
}

fn request(name: &str, app: OrchestratorApp) -> NewOrchestrator {
    NewOrchestrator {
        name: name.to_owned(),
        project: None,
        app,
        executor: None,
        force: false,
    }
}

fn read_json(path: &std::path::Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read JSON file");
    serde_json::from_str(&text).expect("parse JSON file")
}

fn state_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("orchestrator.json")
}

/// Returns the value after `flag` in an MCP argv.
fn arg_after(args: &[String], flag: &str) -> String {
    let position = args
        .iter()
        .position(|value| value == flag)
        .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
    args.get(position + 1)
        .unwrap_or_else(|| panic!("{flag} has no value in {args:?}"))
        .clone()
}

#[test]
fn orchestrator_new_writes_only_project_local_config() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("work", OrchestratorApp::Opencode))
        .expect("create orchestrator");

    let dir = &info.dir;
    assert_eq!(dir, &env.home_path().join("orchestrators/work"));
    for name in [
        "orchestrator.json",
        "opencode.json",
        "AGENTS.md",
        "lease.token",
        "host.json",
        "mirror.json",
    ] {
        assert!(dir.join(name).is_file(), "{name} was not written");
    }
    let mode = std::fs::metadata(dir)
        .expect("orchestrator dir metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "orchestrator dir is not private");
    for name in ["orchestrator.json", "lease.token"] {
        let mode = std::fs::metadata(dir.join(name))
            .expect("file metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name} is not private");
    }

    let state = read_json(&state_path(dir));
    assert_eq!(state["name"], "work");
    assert_eq!(state["app"], "opencode");
    assert_eq!(state["state"], "running");
    assert_eq!(state["session_id"], info.session.as_str());

    // The owner's global app config and the project checkout are untouched; the app
    // config exists only inside the orchestrator directory.
    assert!(!env.home_path().join("opencode.json").exists());
    assert!(!env.home_path().join(".mcp.json").exists());
    assert!(!env.project_path().join("opencode.json").exists());
    assert!(!env.project_path().join(".mcp.json").exists());
    assert!(!env.project_path().join("AGENTS.md").exists());
    let instructions = std::fs::read_to_string(dir.join("AGENTS.md")).expect("instructions");
    assert!(instructions.contains("Never open, query or edit a database"));
    assert!(instructions.contains("Never read, print, log or commit a secret"));
    assert!(instructions.contains("session_resume"));
}

#[test]
fn orchestrator_registers_mcp_in_opencode_and_claude_configs() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let opencode = orchestrator
        .create(&request("oc", OrchestratorApp::Opencode))
        .expect("create opencode orchestrator");
    let config = read_json(&opencode.dir.join("opencode.json"));
    let server = &config["mcp"]["vowp"];
    assert_eq!(server["type"], "local");
    assert_eq!(server["enabled"], true);
    let command: Vec<String> = server["command"]
        .as_array()
        .expect("opencode mcp command")
        .iter()
        .map(|value| value.as_str().expect("command string").to_owned())
        .collect();
    assert_eq!(command[0], "vowp");
    assert_eq!(command[1], "mcp");
    assert!(command.contains(&opencode.session.to_string()));
    let token_file = opencode.dir.join("lease.token");
    assert!(
        command.contains(&token_file.display().to_string()),
        "opencode config does not name the lease token: {command:?}"
    );
    let surface = arg_after(&command, "--surface");
    let lease = env
        .store()
        .lease(&opencode.session)
        .expect("opencode lease lookup")
        .expect("opencode orchestrator has no lease");
    assert_eq!(
        lease.surface_id,
        Some(SurfaceId(surface)),
        "opencode MCP config names a different surface than the lease row"
    );

    let claude = orchestrator
        .create(&request("cc", OrchestratorApp::ClaudeCode))
        .expect("create Claude orchestrator");
    let config = read_json(&claude.dir.join(".mcp.json"));
    let server = &config["mcpServers"]["vowp"];
    assert_eq!(server["command"], "vowp");
    let args: Vec<String> = server["args"]
        .as_array()
        .expect("claude mcp args")
        .iter()
        .map(|value| value.as_str().expect("arg").to_owned())
        .collect();
    assert_eq!(args[0], "mcp");
    assert!(args.contains(&claude.session.to_string()));
    assert!(args.contains(&claude.dir.join("lease.token").display().to_string()));
    let surface = arg_after(&args, "--surface");
    let lease = env
        .store()
        .lease(&claude.session)
        .expect("Claude lease lookup")
        .expect("Claude orchestrator has no lease");
    assert_eq!(
        lease.surface_id,
        Some(SurfaceId(surface)),
        "Claude MCP config names a different surface than the lease row"
    );
    assert!(!claude.dir.join("opencode.json").exists());
}

#[test]
fn orchestrator_join_instructions_desktop_and_phone() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("join", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let join = &info.card.join;
    assert!(join.desktop.contains("FAKE HOST desktop"), "{}", join.desktop);
    assert!(join.desktop.contains("vowp tunnel open"), "{}", join.desktop);
    assert!(join.desktop.contains("--remote-port 4096"), "{}", join.desktop);
    assert!(
        join.desktop.contains("opencode") && join.desktop.contains("pair"),
        "{}",
        join.desktop
    );
    assert!(join.phone.contains("FAKE HOST phone"), "{}", join.phone);
    assert!(join.phone.contains("--server"), "{}", join.phone);
    assert!(join.phone.contains("fake-native"), "{}", join.phone);
    assert!(join.phone.contains("<home-ssh-target>"), "{}", join.phone);
    assert_eq!(join.chat, "vowp chat fake-native");
    // The persisted card carries the same join text for `ls` and the snapshot.
    let state = read_json(&state_path(&info.dir));
    assert_eq!(state["join"]["desktop"], join.desktop);
    assert_eq!(state["join"]["phone"], join.phone);
}

#[test]
fn orchestrator_role_session_is_project_less() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("fleet", OrchestratorApp::Chat))
        .expect("create project-less orchestrator");
    let session = env
        .store()
        .session(&info.session)
        .expect("session lookup")
        .expect("session row");
    assert_eq!(session.role, Some(SessionRole::Orchestrator));
    assert_eq!(session.project, None);
    assert_eq!(session.cwd, info.dir.display().to_string());
    let state = read_json(&state_path(&info.dir));
    assert!(state["project"].is_null(), "project-less state has a project");
    // The reserved `_node` project owns the session's future run rows.
    let node = env
        .store()
        .project(&vowp_protocol::ProjectName::parse("_node").expect("_node"))
        .expect("_node lookup");
    assert!(node.is_some(), "the reserved _node project was not created");
}

#[test]
fn orchestrator_new_refuses_existing_name_without_force() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let first = orchestrator
        .create(&request("dup", OrchestratorApp::Chat))
        .expect("first create");
    let error = orchestrator
        .create(&request("dup", OrchestratorApp::Chat))
        .expect_err("duplicate without force must fail");
    assert!(
        matches!(error, vowp_core::CoreError::Store(vowp_store::StoreError::Conflict(_))),
        "{error:?}"
    );
    assert_eq!(orchestrator.list().expect("list").len(), 1);

    let mut replace = request("dup", OrchestratorApp::Chat);
    replace.force = true;
    let second = orchestrator.create(&replace).expect("force replaces");
    assert_ne!(first.session, second.session);
    assert_eq!(orchestrator.list().expect("list").len(), 1);
    let old = env.store().session(&first.session).expect("old session lookup");
    assert!(old.is_some(), "force must not delete the old session history");
}

#[test]
fn orchestrator_stop_releases_lease() {
    let env = TestEnv::new();
    let (orchestrator, _, stops, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("lease", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let store = env.store();
    assert!(
        store.lease(&info.session).expect("lease lookup").is_some(),
        "create did not acquire a lease"
    );
    assert!(info.dir.join("lease.token").is_file());

    orchestrator.stop("lease").expect("stop orchestrator");
    assert_eq!(stops.load(Ordering::SeqCst), 1, "the app host was not stopped");
    assert!(
        store.lease(&info.session).expect("lease lookup").is_none(),
        "stop left the lease in place"
    );
    assert!(!info.dir.join("lease.token").exists(), "stop left the token file");
    assert!(!info.dir.join("host.json").exists(), "stop left the host state");
    let state = orchestrator.info("lease").expect("info after stop");
    assert_eq!(state.card.state, "stopped");
    assert!(state.host.is_none());
    assert!(state.card.join.chat.contains("vowp chat"), "join lost after stop");
}

#[test]
fn orchestrator_stop_leaves_a_taken_over_lease_alone() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("moved", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let store = env.store();
    let now = Timestamp::now();
    let surface = Surface {
        id: SurfaceId::new(),
        kind: SurfaceKind::Opencode,
        node: vowp_protocol::NodeName::parse("local").expect("node"),
        holder: "phone".to_owned(),
        app: Some("opencode".to_owned()),
        cwd: None,
        metadata: serde_json::json!({}),
        created_at: now,
        last_seen_at: now,
    };
    store.upsert_surface(&surface).expect("other surface row");
    let other_token = vowp_core::surface::new_lease_token();
    store
        .take_lease(&Lease {
            session_id: info.session.clone(),
            surface_id: Some(surface.id.clone()),
            surface_kind: SurfaceKind::Opencode,
            node: surface.node.clone(),
            holder: "phone".to_owned(),
            token: other_token.clone(),
            acquired_at: now,
            heartbeat_at: now,
        })
        .expect("simulate takeover");

    orchestrator.stop("moved").expect("stop the old orchestrator");
    let lease = store
        .lease(&info.session)
        .expect("lease lookup")
        .expect("stop deleted another surface's lease");
    assert_eq!(lease.holder, "phone");
    assert_eq!(lease.token, other_token);
}

#[test]
fn orchestrator_state_survives_supervisor_restart() {
    let env = TestEnv::new();
    let (orchestrator, starts, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("restart", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    assert_eq!(starts.load(Ordering::SeqCst), 1);

    let config = read_json(&info.dir.join("opencode.json"));
    let config_args: Vec<String> = config["mcp"]["vowp"]["command"]
        .as_array()
        .expect("opencode command")
        .iter()
        .map(|value| value.as_str().expect("arg").to_owned())
        .collect();
    let surface_before = arg_after(&config_args, "--surface");

    // A crash loses host.json while orchestrator.json still says `running`.
    std::fs::remove_file(info.dir.join("host.json")).expect("simulate host crash");
    let (restarted, restarted_starts, _, _, _) = manager(deps(&env));
    let resumed = restarted.resume_all().expect("resume all");
    assert_eq!(resumed.len(), 1, "the running orchestrator was not resumed");
    assert_eq!(resumed[0].card.name, "restart");
    assert_eq!(restarted_starts.load(Ordering::SeqCst), 1);
    let state = read_json(&state_path(&info.dir));
    assert_eq!(state["state"], "running");
    assert!(info.dir.join("host.json").is_file());
    let config = read_json(&info.dir.join("opencode.json"));
    let config_args: Vec<String> = config["mcp"]["vowp"]["command"]
        .as_array()
        .expect("opencode command")
        .iter()
        .map(|value| value.as_str().expect("arg").to_owned())
        .collect();
    assert_eq!(
        arg_after(&config_args, "--surface"),
        surface_before,
        "resume rewrote the MCP config with a new surface id"
    );

    // A stopped orchestrator stays stopped across a restart.
    restarted.stop("restart").expect("stop");
    let (again, again_starts, _, _, _) = manager(deps(&env));
    assert!(again.resume_all().expect("resume all after stop").is_empty());
    assert_eq!(again_starts.load(Ordering::SeqCst), 0);
}

#[test]
fn orchestrator_writes_mirror_cursor_for_a_running_host() {
    let env = TestEnv::new();
    let (host, _, _) = FakeHost::new(OrchestratorApp::Opencode);
    let host = host.with_entry("fake-entry-1");
    let orchestrator = Orchestrator::with_hosts(deps(&env), vec![Arc::new(host)]);
    let info = orchestrator
        .create(&request("mirror", OrchestratorApp::Opencode))
        .expect("create orchestrator");

    let mirror = read_json(&info.dir.join("mirror.json"));
    assert_eq!(mirror["last_native_id"], "fake-entry-1");
    assert!(
        mirror["updated_at"].is_string(),
        "mirror.json has no cursor timestamp: {mirror}"
    );

    let store = env.store();
    let user_events = |store: &Store| {
        store
            .event_rows(&info.session, 0, 50)
            .expect("event rows")
            .into_iter()
            .filter(|row| row.event_type == vowp_protocol::EventType::UserMessage)
            .count()
    };
    assert_eq!(user_events(&store), 1, "the first pass did not mirror the entry");

    // Resume reuses the same host and mirrors once more; the native-id map skips it.
    let resumed = orchestrator.resume("mirror").expect("resume orchestrator");
    assert_eq!(resumed.card.name, "mirror");
    assert_eq!(user_events(&store), 1, "resume mirrored the same entry twice");
    let mirror = read_json(&info.dir.join("mirror.json"));
    assert_eq!(mirror["last_native_id"], "fake-entry-1");
}

#[test]
fn node_supervisor_startup_resumes_running_orchestrators() {
    let env = TestEnv::new();
    let (orchestrator, starts, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("boot", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    assert_eq!(starts.load(Ordering::SeqCst), 1);

    // A crash loses host.json while orchestrator.json still says `running`.
    std::fs::remove_file(info.dir.join("host.json")).expect("simulate host crash");
    let (restarted, restarted_starts, _, _, _) = manager(deps(&env));
    let mut node =
        vowp_core::loops::NodeSupervisor::with_orchestrator(deps(&env), restarted).expect("build the node supervisor");
    let report = node.startup().expect("node startup");

    assert_eq!(report.orchestrators_resumed, 1, "{report:?}");
    assert_eq!(
        restarted_starts.load(Ordering::SeqCst),
        1,
        "startup did not resume the host"
    );
    assert_eq!(read_json(&state_path(&info.dir))["state"], "running");
    assert!(info.dir.join("host.json").is_file(), "startup left no live host state");
}

#[test]
fn node_supervisor_startup_leaves_a_live_orchestrator_alone() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("live", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let before = env
        .store()
        .lease(&info.session)
        .expect("lease lookup")
        .expect("create left no lease");
    let token_path = info.dir.join("lease.token");
    let token = std::fs::read_to_string(&token_path).expect("lease token file");
    assert_eq!(before.token, token.trim());

    let (restarted, restarted_starts, _, _, _) = manager(deps(&env));
    let mut node =
        vowp_core::loops::NodeSupervisor::with_orchestrator(deps(&env), restarted).expect("build the node supervisor");
    let report = node.startup().expect("node startup");

    assert_eq!(report.orchestrators_resumed, 1, "{report:?}");
    assert_eq!(
        restarted_starts.load(Ordering::SeqCst),
        0,
        "startup restarted a host that was already running"
    );
    let after = env
        .store()
        .lease(&info.session)
        .expect("lease lookup")
        .expect("startup dropped a live lease");
    assert_eq!(after.token, before.token, "startup replaced a live lease token");
    assert_eq!(
        std::fs::read_to_string(&token_path).expect("lease token file"),
        token,
        "startup rewrote the live lease token file"
    );
    assert!(
        info.dir.join("mirror.json").is_file(),
        "startup did not refresh the mirror"
    );
}

#[test]
fn node_supervisor_startup_does_not_steal_a_taken_over_lease() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("moved-live", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let store = env.store();
    let now = Timestamp::now();
    let surface = Surface {
        id: SurfaceId::new(),
        kind: SurfaceKind::Opencode,
        node: NodeName::parse("local").expect("node"),
        holder: "phone".to_owned(),
        app: Some("opencode".to_owned()),
        cwd: None,
        metadata: serde_json::json!({}),
        created_at: now,
        last_seen_at: now,
    };
    store.upsert_surface(&surface).expect("other surface row");
    let other_token = vowp_core::surface::new_lease_token();
    store
        .take_lease(&Lease {
            session_id: info.session.clone(),
            surface_id: Some(surface.id.clone()),
            surface_kind: SurfaceKind::Opencode,
            node: surface.node.clone(),
            holder: "phone".to_owned(),
            token: other_token.clone(),
            acquired_at: now,
            heartbeat_at: now,
        })
        .expect("simulate takeover");

    let (restarted, restarted_starts, _, _, _) = manager(deps(&env));
    let mut node =
        vowp_core::loops::NodeSupervisor::with_orchestrator(deps(&env), restarted).expect("build the node supervisor");
    let report = node.startup().expect("node startup");

    assert_eq!(report.orchestrators_resumed, 0, "{report:?}");
    assert_eq!(
        restarted_starts.load(Ordering::SeqCst),
        0,
        "startup restarted the old host after a takeover"
    );
    let lease = store
        .lease(&info.session)
        .expect("lease lookup")
        .expect("startup dropped the new holder's lease");
    assert_eq!(lease.holder, "phone");
    assert_eq!(lease.token, other_token, "startup stole the taken-over lease");
}

#[test]
fn orchestrator_keeper_keeps_the_lease_after_the_cli_exits() {
    let env = TestEnv::new();
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create(&request("keeper", OrchestratorApp::Opencode))
        .expect("create orchestrator");
    let exe = vowp_binary();
    // A real CLI process starts the keeper and exits. Its fake host is still recorded as
    // live, so resume reuses it instead of starting a real opencode service.
    let output = std::process::Command::new(&exe)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["orchestrator", "resume", "keeper"])
        .output()
        .expect("run vowp orchestrator resume");
    assert!(
        output.status.success(),
        "resume failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let pid: u32 = std::fs::read_to_string(info.dir.join("keeper.pid"))
        .expect("keeper pid file after the CLI exited")
        .trim()
        .parse()
        .expect("keeper pid");
    assert!(vowp_adapters::proc::process_alive(pid), "keeper {pid} is not alive");
    let store = env.store();
    let first = store
        .lease(&info.session)
        .expect("lease lookup")
        .expect("keeper left no lease")
        .heartbeat_at;
    std::thread::sleep(std::time::Duration::from_secs(6));
    let second = store
        .lease(&info.session)
        .expect("lease lookup")
        .expect("keeper dropped the lease")
        .heartbeat_at;
    assert!(
        second > first,
        "the keeper did not heartbeat the lease: {first} -> {second}"
    );

    // Use the fake-host manager for stop; the real CLI's opencode host would try to stop
    // the fake host's pid (this test process).
    orchestrator.stop("keeper").expect("stop orchestrator");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while vowp_adapters::proc::process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !vowp_adapters::proc::process_alive(pid),
        "stop left keeper {pid} running"
    );
    assert!(!info.dir.join("keeper.pid").exists(), "stop left the keeper pid file");
    assert!(
        store.lease(&info.session).expect("lease lookup").is_none(),
        "stop left the keeper lease in place"
    );
}

/// Returns the built `vowp` binary next to the test harness.
fn vowp_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    let candidate = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target directory")
        .join("vowp");
    assert!(
        candidate.is_file(),
        "build the vowp binary first; missing {}",
        candidate.display()
    );
    candidate
}

#[test]
fn orchestrator_chat_cli_round_trip() {
    let env = TestEnv::new();
    let (code, stdout, stderr) = env.run(&["orchestrator", "new", "--name", "cli", "--app", "chat"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("orchestrator cli (chat, running)"), "{stdout}");
    assert!(stdout.contains("desktop: vowp chat"), "{stdout}");

    let dir = env.home_path().join("orchestrators/cli");
    assert!(dir.join("AGENTS.md").is_file());
    assert!(!dir.join("opencode.json").exists());
    assert!(!dir.join(".mcp.json").exists());
    let state = read_json(&state_path(&dir));
    let session = state["session_id"].as_str().expect("session id").to_owned();
    assert_eq!(state["state"], "running");
    assert_eq!(state["join"]["chat"], format!("vowp chat {session}"));

    let (code, value, stderr) = env.run_json(&["orchestrator", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let cards = value.as_array().expect("orchestrator list is an array");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["name"], "cli");
    assert_eq!(cards[0]["app"], "chat");
    assert_eq!(cards[0]["join"]["chat"], format!("vowp chat {session}"));

    let (code, stdout, stderr) = env.run(&["orchestrator", "stop", "cli"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("(chat, stopped)"), "{stdout}");
    assert_eq!(read_json(&state_path(&dir))["state"], "stopped");

    let (code, stdout, stderr) = env.run(&["orchestrator", "resume", "cli"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("(chat, running)"), "{stdout}");
    assert_eq!(read_json(&state_path(&dir))["state"], "running");
}

#[test]
fn orchestrator_project_path_names_a_registered_project() {
    let env = TestEnv::new();
    let checkout = tempfile::tempdir().expect("temp checkout");
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = ProjectName::parse("checkout").expect("checkout name");
    std::fs::write(
        checkout.path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config serialises"),
    )
    .expect("write checkout vowp.toml");
    let path = checkout.path().display().to_string();
    let args = [
        "orchestrator",
        "new",
        "--app",
        "chat",
        "--name",
        "from-path",
        "--project",
        path.as_str(),
    ];

    // A checkout path is resolved to its `[project].name`; binding still requires that
    // name in the registry, and the refusal says how to register it.
    let (code, _, stderr) = env.run(&args);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("project checkout"), "{stderr}");
    assert!(stderr.contains("not registered"), "{stderr}");

    let store = env.store();
    let now = Timestamp::now();
    store
        .upsert_project(&Project {
            name: config.project.name.clone(),
            path: path.clone(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("register the checkout");
    let (code, stdout, stderr) = env.run(&args);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let state = read_json(&state_path(&env.home_path().join("orchestrators/from-path")));
    assert_eq!(state["project"], "checkout");
}

#[test]
fn orchestrator_cli_rejects_bad_input_and_duplicates() {
    let env = TestEnv::new();
    let (code, _, stderr) = env.run(&["orchestrator", "new", "--app", "vim"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("unknown app"), "{stderr}");

    let (code, _, stderr) = env.run(&["orchestrator", "new", "--app", "chat", "--name", "one"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run(&["orchestrator", "new", "--app", "chat", "--name", "one"]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    let (code, _, stderr) = env.run(&[
        "orchestrator",
        "new",
        "--app",
        "chat",
        "--name",
        "missing",
        "--project",
        "nope",
    ]);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("project nope"), "{stderr}");
}

#[test]
fn orchestrator_session_is_visible_to_fleet_snapshot() {
    let env = TestEnv::new();
    let (code, stdout, stderr) = env.run(&["orchestrator", "new", "--app", "chat", "--name", "snap"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let store = Store::open(&vowp_store::NodePaths::new(env.home_path())).expect("open store");
    let snapshot = vowp_core::fleet::build_snapshot(&deps(&env), &store, Timestamp::now()).expect("snapshot");
    assert_eq!(snapshot.orchestrators.len(), 1, "{:?}", snapshot.orchestrators);
    assert_eq!(snapshot.orchestrators[0].name, "snap");
    assert_eq!(snapshot.orchestrators[0].app, OrchestratorApp::Chat);
}
