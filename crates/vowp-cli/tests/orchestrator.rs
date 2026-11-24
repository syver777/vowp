//! Orchestrator role and app-join tests.
//!
//! Every test uses a temporary `VOWP_HOME` and project. App hosts are fakes: they write the
//! same `host.json` shape as the real hosts and never open a socket, start opencode or
//! Claude, or read the real home. The `chat` app has no host and is exercised through the
//! real CLI and its project-local state.

// This file is the `orchestrator` integration test target declared in the root
// `Cargo.toml` (the `vowp-cli` package root is the workspace root).

use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;
use tempfile::TempDir;
use vowp_protocol::{
    AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, JoinInstructions, Lease, Node, NodeName, NodeStatus,
    OrchestratorApp, OrchestratorCommand, OrchestratorError, OrchestratorNodeState, OrchestratorResult, Project,
    ProjectName, SessionId, SessionRole, Surface, SurfaceId, SurfaceKind, Timestamp, VowpConfig,
};
use vowp_store::{NodeCredential, NodePath, NodePaths, Store, hash_lease_token};

use vowp_core::CoreError;
use vowp_core::agent::auth;
use vowp_core::agent::gateway::{self, GatewayOptions};
use vowp_core::deps::Deps;
use vowp_core::surface::mirror::MirrorCursor;
use vowp_core::surface::native::{AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, TranscriptEntry};
use vowp_core::surface::orchestrator::{NewOrchestrator, Orchestrator, OrchestratorTransport};

use vowp_cli::{BufferIo, Ctx, Global};

/// A temporary node home and project, with no tmux and no real harness.
struct TestEnv {
    home: TempDir,
    project: TempDir,
    project_name: ProjectName,
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
        Self {
            home,
            project,
            project_name: config.project.name,
        }
    }

    fn project_name(&self) -> &ProjectName {
        &self.project_name
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
        let code = vowp_cli::run(full, &mut io);
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

    /// Registers an active agent node that is online now.
    fn agent_node(&self, name: &str) -> NodeName {
        let node = NodeName::parse(name).expect("node name");
        let store = self.store();
        let now = store.clock().now();
        store
            .insert_pending_node(&node, &["darwin-arm64".to_owned()], now)
            .expect("pending node");
        store
            .set_node_status(&node, NodeStatus::Active, "", now)
            .expect("active node");
        store
            .touch_node(&node, now, Some("0.4.0"), Some("vowp-agent/1"), &serde_json::json!({}))
            .expect("node heartbeat");
        node
    }

    /// Ages a node's last heartbeat past the offline threshold.
    fn age_node(&self, node: &NodeName) {
        let store = self.store();
        let old = store.clock().now().minus(Duration::from_secs(120));
        store
            .set_node_status(node, NodeStatus::Active, "", old)
            .expect("offline node");
    }

    /// Revokes a node's enrollment.
    fn revoke_node(&self, node: &NodeName) {
        let store = self.store();
        store
            .set_node_status(node, NodeStatus::Revoked, "", store.clock().now())
            .expect("revoked node");
    }

    /// Reports the test project's checkout on `node`, as a live agent would.
    fn map_project(&self, node: &NodeName) {
        let store = self.store();
        let root = self.project_path().display().to_string();
        store
            .upsert_node_path(&NodePath {
                node: node.clone(),
                project: self.project_name().clone(),
                local_root: root.clone(),
                worktree_root: root,
                allow: Vec::new(),
                updated_at: store.clock().now(),
            })
            .expect("node path map");
    }

    /// Returns one orchestrator's directory below the temporary home.
    fn orchestrator_dir(&self, name: &str) -> PathBuf {
        self.home_path().join("orchestrators").join(name)
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

/// A refused result the fake agent returns instead of accepting a start or resume.
struct FakeRefusal {
    state: OrchestratorNodeState,
    code: String,
    message: String,
    detail: Option<String>,
}

/// A fake agent frame stream backed by the home's command spool.
///
/// It reads `agent-commands/<node>/*.json`, records each `OrchestratorCommand` and writes
/// the matching `OrchestratorResult` to `agent-results/<node>/`. It never opens a socket,
/// reaches the network or starts a real app.
struct FakeAgent {
    commands: Arc<Mutex<Vec<OrchestratorCommand>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeAgent {
    fn start(home: &Path, node: &NodeName) -> Self {
        Self::start_with(home, node, None)
    }

    /// Starts a fake agent that refuses every `Start` and `Resume` with `refusal`.
    fn start_refusing(home: &Path, node: &NodeName, refusal: FakeRefusal) -> Self {
        Self::start_with(home, node, Some(refusal))
    }

    fn start_with(home: &Path, node: &NodeName, refusal: Option<FakeRefusal>) -> Self {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let command_dir = home.join("agent-commands").join(node.as_str());
        let result_dir = home.join("agent-results").join(node.as_str());
        let handle = {
            let commands = Arc::clone(&commands);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(entries) = std::fs::read_dir(&command_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                                continue;
                            }
                            let Ok(bytes) = std::fs::read(&path) else {
                                continue;
                            };
                            let Ok(AgentFrame::OrchestratorCommand(command)) =
                                serde_json::from_slice::<AgentFrame>(&bytes)
                            else {
                                continue;
                            };
                            let request_id = command.request_id().to_owned();
                            let frame = match (&refusal, &command) {
                                (
                                    Some(refusal),
                                    OrchestratorCommand::Start { .. } | OrchestratorCommand::Resume { .. },
                                ) => {
                                    let mut result = OrchestratorResult::failed(
                                        request_id.clone(),
                                        refusal.state,
                                        OrchestratorError::new(refusal.code.clone(), refusal.message.clone(), false),
                                    );
                                    result.detail = refusal.detail.clone();
                                    AgentFrame::OrchestratorResult(result)
                                }
                                _ => {
                                    let state = match command {
                                        OrchestratorCommand::Stop { .. } => OrchestratorNodeState::Stopped,
                                        OrchestratorCommand::Start { .. } | OrchestratorCommand::Resume { .. } => {
                                            OrchestratorNodeState::Running
                                        }
                                    };
                                    AgentFrame::OrchestratorResult(OrchestratorResult::accepted(
                                        request_id.clone(),
                                        state,
                                    ))
                                }
                            };
                            std::fs::create_dir_all(&result_dir).expect("fake result dir");
                            let result_path = result_dir.join(format!("{request_id}.json"));
                            let temp = result_dir.join(format!("{request_id}.tmp"));
                            std::fs::write(&temp, serde_json::to_vec(&frame).expect("result JSON"))
                                .expect("fake result write");
                            std::fs::rename(&temp, &result_path).expect("fake result rename");
                            let _ = std::fs::remove_file(&path);
                            commands.lock().expect("fake command log").push(command);
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        Self {
            commands,
            stop,
            handle: Some(handle),
        }
    }

    fn commands(&self) -> Vec<OrchestratorCommand> {
        self.commands.lock().expect("fake command log").clone()
    }
}

impl Drop for FakeAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Returns the lowercase hex public key of `key`.
fn public_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Signs the gateway challenge payload and returns lowercase hex.
fn signature_hex(key: &SigningKey, nonce: &str, home: &str, connection: &str) -> String {
    key.sign(&auth::handshake_payload(nonce, home, connection))
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The agent capabilities sent in `AgentHello`.
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.4.0".to_owned(),
        harnesses: std::collections::BTreeMap::new(),
        git_version: None,
        labels: vec!["test".to_owned()],
        secret_names: Vec::new(),
        projects: Vec::new(),
    }
}

/// One real gateway connection over a pair of local sockets.
///
/// The test plays the agent: it writes frames to the gateway's stdin and reads the
/// gateway's stdout. No network, real agent binary or tmux is involved.
struct LiveGateway {
    to_gateway: UnixStream,
    from_gateway: std::io::BufReader<UnixStream>,
    handle: Option<thread::JoinHandle<Result<gateway::GatewayOutcome, CoreError>>>,
}

impl LiveGateway {
    fn start(env: &TestEnv) -> Self {
        let deps = deps(env);
        let store = deps.open_store().expect("gateway store");
        let (to_gateway, gateway_input) = UnixStream::pair().expect("gateway input pair");
        let (gateway_output, from_gateway) = UnixStream::pair().expect("gateway output pair");
        from_gateway
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("gateway read timeout");
        let options = GatewayOptions {
            home_id: deps.node.to_string(),
            heartbeat_secs: 10,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(2),
        };
        let handle = thread::spawn(move || {
            gateway::serve_with_options(
                &store,
                &deps,
                std::io::BufReader::new(gateway_input),
                gateway_output,
                options,
            )
        });
        Self {
            to_gateway,
            from_gateway: std::io::BufReader::new(from_gateway),
            handle: Some(handle),
        }
    }

    fn send(&mut self, frame: &AgentFrame) {
        let mut bytes = serde_json::to_vec(frame).expect("gateway frame JSON");
        bytes.push(b'\n');
        self.to_gateway.write_all(&bytes).expect("write gateway frame");
        self.to_gateway.flush().expect("flush gateway frame");
    }

    fn expect(&mut self, predicate: impl Fn(&AgentFrame) -> bool) -> AgentFrame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut line = String::new();
            match self.from_gateway.read_line(&mut line) {
                Ok(0) => panic!("gateway closed while waiting for a frame"),
                Ok(_) => {
                    if let Ok(frame) = serde_json::from_str::<AgentFrame>(&line)
                        && predicate(&frame)
                    {
                        return frame;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("gateway read failed: {error}"),
            }
            assert!(Instant::now() < deadline, "timed out waiting for a gateway frame");
        }
    }

    fn finish(mut self) -> gateway::GatewayOutcome {
        let _ = self.to_gateway.shutdown(std::net::Shutdown::Write);
        self.handle
            .take()
            .expect("gateway thread")
            .join()
            .expect("gateway thread panicked")
            .expect("gateway connection")
    }
}

/// Registers a credential for an already-active test node.
fn bind_key(env: &TestEnv, node: &NodeName, key: &SigningKey) {
    let store = env.store();
    let now = store.clock().now();
    let key_id = public_hex(key);
    store
        .upsert_node_credential(&NodeCredential {
            node: node.clone(),
            key_id: key_id.clone(),
            public_key: key_id,
            created_at: now,
            last_used_at: None,
            revoked_at: None,
        })
        .expect("node credential");
}

/// Runs the real agent handshake over a [`LiveGateway`].
fn authenticate(gateway: &mut LiveGateway, node: &NodeName, key: &SigningKey) {
    gateway.send(&AgentFrame::AgentHello {
        node: node.clone(),
        key_id: public_hex(key),
        agent_version: "0.4.0".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        labels: vec!["test".to_owned()],
        capabilities: agent_capabilities(),
    });
    let AgentFrame::HomeChallenge {
        nonce,
        home_id,
        connection_id,
        ..
    } = gateway.expect(|frame| matches!(frame, AgentFrame::HomeChallenge { .. }))
    else {
        panic!("expected HomeChallenge");
    };
    let signature = signature_hex(key, &nonce, &home_id, &connection_id);
    gateway.send(&AgentFrame::AgentAuth { signature });
    gateway.expect(|frame| matches!(frame, AgentFrame::HomeWelcome { .. }));
}

/// One heartbeat frame with a fixed capacity snapshot.
fn heartbeat() -> AgentFrame {
    AgentFrame::Heartbeat {
        at: Timestamp::now(),
        capacity: AgentCapacity {
            cpu_cores: 8,
            load1: 0.1,
            mem_total_bytes: 16 * 1024 * 1024 * 1024,
            mem_free_bytes: 8 * 1024 * 1024 * 1024,
            disk_free_bytes: 64 * 1024 * 1024 * 1024,
            busy_slots: 0,
            max_slots: 2,
            on_battery: None,
            thermal_state: None,
        },
        pending_spool_bytes: 0,
    }
}

/// An in-memory transport that records the frame and answers synchronously.
struct RecordingTransport {
    sent: Arc<Mutex<Vec<OrchestratorCommand>>>,
}

impl OrchestratorTransport for RecordingTransport {
    fn send(&self, _node: &NodeName, command: &OrchestratorCommand) -> Result<Option<OrchestratorResult>, CoreError> {
        self.sent.lock().expect("recording transport log").push(command.clone());
        let state = match command {
            OrchestratorCommand::Stop { .. } => OrchestratorNodeState::Stopped,
            OrchestratorCommand::Start { .. } | OrchestratorCommand::Resume { .. } => OrchestratorNodeState::Running,
        };
        Ok(Some(OrchestratorResult::accepted(command.request_id(), state)))
    }
}

/// A transport that queues every command and records it, like a live connection that is
/// momentarily slow: the home must record `starting` and apply the late result later.
struct QueuedTransport {
    sent: Arc<Mutex<Vec<OrchestratorCommand>>>,
}

impl OrchestratorTransport for QueuedTransport {
    fn send(&self, _node: &NodeName, command: &OrchestratorCommand) -> Result<Option<OrchestratorResult>, CoreError> {
        self.sent.lock().expect("queued transport log").push(command.clone());
        Ok(None)
    }
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
    assert_eq!(state.card.state, OrchestratorNodeState::Stopped);
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
    assert_eq!(lease.token, hash_lease_token(&other_token));
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
    assert_eq!(before.token, hash_lease_token(token.trim()));

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
    assert_eq!(
        lease.token,
        hash_lease_token(&other_token),
        "startup stole the taken-over lease"
    );
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

#[test]
fn orchestrator_resume_on_the_home_node_keeps_a_keeper() {
    let env = TestEnv::new();
    let home = NodeName::parse("local").expect("home node");
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create_on_node(&request("home-keeper", OrchestratorApp::Opencode), Some(home.clone()))
        .expect("create a home-node orchestrator");
    assert_eq!(info.card.node.as_ref(), Some(&home));

    // The fake host already wrote host.json, so the CLI resume reuses it instead of
    // starting a real opencode service. The card still names the home node.
    let output = std::process::Command::new(vowp_binary())
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["orchestrator", "resume", "home-keeper"])
        .output()
        .expect("run vowp orchestrator resume");
    assert!(
        output.status.success(),
        "resume failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let pid: u32 = std::fs::read_to_string(info.dir.join("keeper.pid"))
        .expect("home-node orchestrator has no keeper pid after resume")
        .trim()
        .parse()
        .expect("keeper pid");
    assert!(vowp_adapters::proc::process_alive(pid), "keeper {pid} is not alive");
    orchestrator.stop("home-keeper").expect("stop orchestrator");
    let deadline = Instant::now() + Duration::from_secs(5);
    while vowp_adapters::proc::process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !vowp_adapters::proc::process_alive(pid),
        "stop left the home-node keeper {pid} running"
    );
}

#[test]
fn orchestrator_live_gateway_drains_commands_and_reconnects() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let key = SigningKey::from_bytes(&[7_u8; 32]);
    bind_key(&env, &node, &key);

    let mut gateway = LiveGateway::start(&env);
    authenticate(&mut gateway, &node, &key);

    // A real home command writes the spool and blocks for the correlated result; this
    // thread plays `vowp orchestrator new`. The gateway must send the frame on the live
    // connection and publish the agent's result back.
    let node_for_create = node.clone();
    let create_deps = deps(&env);
    let create = thread::spawn(move || {
        Orchestrator::new(create_deps)
            .create_on_node(&request("live", OrchestratorApp::Opencode), Some(node_for_create))
    });
    let AgentFrame::OrchestratorCommand(command) =
        gateway.expect(|frame| matches!(frame, AgentFrame::OrchestratorCommand(_)))
    else {
        panic!("expected an OrchestratorCommand frame");
    };
    assert_eq!(command.name(), "live");
    assert!(matches!(command, OrchestratorCommand::Start { .. }), "{command:?}");
    let request_id = command.request_id().to_owned();
    gateway.send(&AgentFrame::OrchestratorResult(OrchestratorResult::accepted(
        request_id.clone(),
        OrchestratorNodeState::Running,
    )));
    let info = create
        .join()
        .expect("create thread")
        .expect("create through the live gateway");
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert_eq!(info.card.node.as_ref(), Some(&node));
    let state = read_json(&state_path(&info.dir));
    assert_eq!(state["state"], "running");
    assert_eq!(state["last_request_id"], request_id);
    assert!(
        !env.home_path()
            .join("agent-commands")
            .join(node.as_str())
            .join(format!("{request_id}.json"))
            .exists(),
        "the gateway left the delivered command in the outbox"
    );

    // Closing the authenticated connection marks the record degraded.
    gateway.finish();
    let orchestrator = Orchestrator::new(deps(&env));
    let info = orchestrator.info("live").expect("info after disconnect");
    assert_eq!(info.card.state, OrchestratorNodeState::Degraded);
    assert_eq!(
        info.card.last_error.as_deref(),
        Some("node mac-studio connection dropped"),
        "a degraded card exposes the redacted disconnection detail"
    );

    // A reconnect heartbeat queues a Resume command; the new connection must send it
    // and apply the next result to the same record.
    let mut gateway = LiveGateway::start(&env);
    authenticate(&mut gateway, &node, &key);
    gateway.send(&heartbeat());
    let AgentFrame::OrchestratorCommand(command) =
        gateway.expect(|frame| matches!(frame, AgentFrame::OrchestratorCommand(_)))
    else {
        panic!("expected a Resume frame after reconnect");
    };
    assert!(matches!(command, OrchestratorCommand::Resume { .. }), "{command:?}");
    let request_id = command.request_id().to_owned();
    gateway.send(&AgentFrame::OrchestratorResult(OrchestratorResult::accepted(
        request_id,
        OrchestratorNodeState::Running,
    )));
    let deadline = Instant::now() + Duration::from_secs(5);
    let info = loop {
        let info = orchestrator.info("live").expect("info after reconnect");
        if info.card.state == OrchestratorNodeState::Running {
            break info;
        }
        assert!(
            Instant::now() < deadline,
            "reconnect did not resume the record: {info:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert!(
        info.card.last_error.is_none(),
        "a clean reconnect clears last_error: {info:?}"
    );
    gateway.finish();
}

#[test]
fn node_show_json_reflects_a_live_path_map_update() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let key = SigningKey::from_bytes(&[9_u8; 32]);
    bind_key(&env, &node, &key);

    let mut gateway = LiveGateway::start(&env);
    authenticate(&mut gateway, &node, &key);
    gateway.send(&AgentFrame::PathMapUpdate {
        map: AgentPathMap {
            project: env.project_name().clone(),
            local_root: "/srv/owner/vowp".to_owned(),
            worktree_root: Some("/srv/owner/vowp/.vowp/worktrees".to_owned()),
            allow: vec!["src".to_owned()],
        },
    });
    gateway.expect(|frame| matches!(frame, AgentFrame::Pong));

    let (code, card, stderr) = env.run_json(&["node", "show", node.as_str()]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        card["mapped"],
        serde_json::json!([env.project_name().as_str()]),
        "node show sees the map without a reconnect: {card}"
    );
    assert!(
        !card.to_string().contains("/srv/owner"),
        "the card must not carry the reported local root: {card}"
    );
    gateway.finish();
}

#[test]
fn orchestrator_new_with_a_node_creates_a_card_and_sends_start() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let agent = FakeAgent::start(env.home_path(), &node);

    let (code, card, stderr) = env.run_json(&[
        "orchestrator",
        "new",
        "--name",
        "mac",
        "--app",
        "opencode",
        "--node",
        node.as_str(),
        "--project",
        env.project_name().as_str(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(card["name"], "mac");
    assert_eq!(card["node"], "mac-studio");
    assert_eq!(card["state"], "running");
    assert_eq!(card["app"], "opencode");
    assert_eq!(card["project"], env.project_name().as_str());

    let commands = agent.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    match &commands[0] {
        OrchestratorCommand::Start {
            name,
            app,
            project,
            request_id,
        } => {
            assert_eq!(name, "mac");
            assert_eq!(*app, OrchestratorApp::Opencode);
            assert_eq!(
                project.as_ref().map(ProjectName::as_str),
                Some(env.project_name().as_str())
            );
            assert!(!request_id.is_empty(), "start has no correlation id");
        }
        other => panic!("expected a start command, got {other:?}"),
    }

    let dir = env.orchestrator_dir("mac");
    let state = read_json(&state_path(&dir));
    assert_eq!(state["node"], "mac-studio");
    assert_eq!(state["state"], "running");
    assert!(
        state["last_request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "no request id: {state}"
    );
    for absent in ["host.json", "opencode.json", "lease.token", "mirror.json"] {
        assert!(
            !dir.join(absent).exists(),
            "{absent} was written for a local-node orchestrator"
        );
    }
    let session = SessionId::parse(state["session_id"].as_str().expect("session id")).expect("session id");
    assert!(
        env.store().lease(&session).expect("lease lookup").is_none(),
        "a local-node orchestrator took a home lease"
    );

    let (code, cards, stderr) = env.run_json(&["orchestrator", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let cards = cards.as_array().expect("orchestrator list");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["node"], "mac-studio");
    assert_eq!(cards[0]["state"], "running");
}

#[test]
fn orchestrator_new_reports_a_refused_node_detail_without_leaking_it() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let _agent = FakeAgent::start_refusing(
        env.home_path(),
        &node,
        FakeRefusal {
            state: OrchestratorNodeState::Failed,
            code: "spawn_failed".to_owned(),
            message: "cannot start /srv/owner/app token=FAKE-NOT-A-SECRET \
                      at http://owner:FAKE-NOT-A-SECRET@127.0.0.1:4096/link"
                .to_owned(),
            detail: None,
        },
    );
    let (code, _, stderr) = env.run(&[
        "orchestrator",
        "new",
        "--name",
        "mac",
        "--app",
        "opencode",
        "--node",
        node.as_str(),
    ]);
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("refused to start orchestrator mac"), "{stderr}");
    assert!(stderr.contains("spawn_failed"), "{stderr}");
    for forbidden in ["/srv/", "FAKE-NOT-A-SECRET", "127.0.0.1", "4096"] {
        assert!(
            !stderr.contains(forbidden),
            "the refusal leaked {forbidden:?}: {stderr}"
        );
    }

    let state = read_json(&state_path(&env.orchestrator_dir("mac")));
    assert_eq!(state["state"], "failed");
    let last_error = state["last_error"].as_str().expect("persisted last_error");
    assert!(last_error.starts_with("spawn_failed:"), "{last_error}");
    for forbidden in ["/srv/", "FAKE-NOT-A-SECRET", "127.0.0.1", "4096"] {
        assert!(
            !last_error.contains(forbidden),
            "the state file leaked {forbidden:?}: {last_error}"
        );
    }

    let (code, cards, stderr) = env.run_json(&["orchestrator", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        cards[0]["last_error"], last_error,
        "orchestrator ls --json carries the redacted detail"
    );
}

#[test]
fn orchestrator_resume_reports_refusal_detail_and_running_clears_last_error() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let accepting = FakeAgent::start(env.home_path(), &node);
    let (code, card, stderr) = env.run_json(&[
        "orchestrator",
        "new",
        "--name",
        "mac",
        "--app",
        "opencode",
        "--node",
        node.as_str(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(card["state"], "running");
    assert!(card["last_error"].is_null(), "a healthy card has no last_error: {card}");
    drop(accepting);

    let refusing = FakeAgent::start_refusing(
        env.home_path(),
        &node,
        FakeRefusal {
            state: OrchestratorNodeState::Failed,
            code: "service_timeout".to_owned(),
            message: "the app did not answer password=FAKE-NOT-A-SECRET at http://localhost:4096/join".to_owned(),
            detail: None,
        },
    );
    let (code, _, stderr) = env.run(&["orchestrator", "resume", "mac"]);
    assert_eq!(code, 5, "{stderr}");
    assert!(
        stderr.contains("node mac-studio refused to resume orchestrator mac: service_timeout:"),
        "{stderr}"
    );
    for forbidden in ["FAKE-NOT-A-SECRET", "localhost", "4096"] {
        assert!(
            !stderr.contains(forbidden),
            "the refusal leaked {forbidden:?}: {stderr}"
        );
    }
    let state = read_json(&state_path(&env.orchestrator_dir("mac")));
    assert_eq!(state["state"], "failed");
    let last_error = state["last_error"].as_str().expect("persisted last_error");
    assert!(last_error.contains("service_timeout"), "{last_error}");
    for forbidden in ["FAKE-NOT-A-SECRET", "localhost", "4096"] {
        assert!(
            !last_error.contains(forbidden),
            "the state file leaked {forbidden:?}: {last_error}"
        );
    }
    drop(refusing);

    let accepting = FakeAgent::start(env.home_path(), &node);
    let (code, card, stderr) = env.run_json(&["orchestrator", "resume", "mac"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(card["state"], "running");
    assert!(card["last_error"].is_null(), "a clean resume clears last_error: {card}");
    let state = read_json(&state_path(&env.orchestrator_dir("mac")));
    assert_eq!(state["state"], "running");
    assert!(
        state.get("last_error").is_none(),
        "a clean record omits last_error from disk: {state}"
    );
    drop(accepting);
}

#[test]
fn orchestrator_new_refuses_an_unknown_or_revoked_node() {
    let env = TestEnv::new();

    let (code, _, stderr) = env.run(&[
        "orchestrator",
        "new",
        "--name",
        "ghost",
        "--app",
        "chat",
        "--node",
        "ghost-node",
    ]);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("node ghost-node"), "{stderr}");
    assert!(!env.orchestrator_dir("ghost").exists(), "unknown node left a record");

    let node = env.agent_node("revoked-node");
    env.revoke_node(&node);
    let (code, _, stderr) = env.run(&[
        "orchestrator",
        "new",
        "--name",
        "revoked",
        "--app",
        "chat",
        "--node",
        node.as_str(),
    ]);
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("revoked"), "{stderr}");
    assert!(!env.orchestrator_dir("revoked").exists(), "revoked node left a record");
}

#[test]
fn orchestrator_resume_on_an_offline_node_reports_node_offline() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let agent = FakeAgent::start(env.home_path(), &node);
    let (code, _, stderr) = env.run(&[
        "orchestrator",
        "new",
        "--name",
        "mac",
        "--app",
        "opencode",
        "--node",
        node.as_str(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(agent.commands().len(), 1);

    env.age_node(&node);
    let (code, _, stderr) = env.run(&["orchestrator", "resume", "mac"]);
    assert_eq!(code, 7, "{stderr}");
    assert!(stderr.contains("node_offline"), "{stderr}");
    assert!(stderr.contains("mac-studio"), "{stderr}");
    assert_eq!(agent.commands().len(), 1, "resume reached a node that is offline");
    let state = read_json(&state_path(&env.orchestrator_dir("mac")));
    assert_eq!(state["state"], "node_offline");
}

#[test]
fn orchestrator_stop_sends_detach_and_releases_the_lease() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let agent = FakeAgent::start(env.home_path(), &node);
    let (code, card, stderr) = env.run_json(&[
        "orchestrator",
        "new",
        "--name",
        "mac",
        "--app",
        "opencode",
        "--node",
        node.as_str(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let session = SessionId::parse(card["session_id"].as_str().expect("session id")).expect("session id");
    let store = env.store();
    let now = Timestamp::now();
    store
        .take_lease(&Lease {
            session_id: session.clone(),
            surface_id: None,
            surface_kind: SurfaceKind::Opencode,
            node: node.clone(),
            holder: "mac".to_owned(),
            token: vowp_core::surface::new_lease_token(),
            acquired_at: now,
            heartbeat_at: now,
        })
        .expect("simulate the node surface lease");
    assert!(store.lease(&session).expect("lease lookup").is_some());

    let (code, stdout, stderr) = env.run(&["orchestrator", "stop", "mac"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let commands = agent.commands();
    assert_eq!(commands.len(), 2, "{commands:?}");
    match &commands[1] {
        OrchestratorCommand::Stop { name, request_id } => {
            assert_eq!(name, "mac");
            assert!(!request_id.is_empty());
        }
        other => panic!("expected a stop command, got {other:?}"),
    }
    assert!(
        store.lease(&session).expect("lease lookup").is_none(),
        "stop left the lease in place"
    );
    let dir = env.orchestrator_dir("mac");
    assert!(!dir.join("lease.token").exists(), "stop left the token file");
    assert!(!dir.join("host.json").exists(), "stop started a home host");
    assert_eq!(read_json(&state_path(&dir))["state"], "stopped");
}

#[test]
fn orchestrator_disconnect_marks_it_degraded_and_reconnect_resumes() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let orchestrator = Orchestrator::with_transport(
        deps(&env),
        Vec::new(),
        Arc::new(RecordingTransport {
            sent: Arc::clone(&sent),
        }),
    );
    let info = orchestrator
        .create_on_node(&request("mac", OrchestratorApp::Opencode), Some(node.clone()))
        .expect("create on node");
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert_eq!(info.card.node.as_ref(), Some(&node));

    assert_eq!(orchestrator.mark_node_disconnected(&node).expect("disconnect"), 1);
    let info = orchestrator.info("mac").expect("info after disconnect");
    assert_eq!(info.card.state, OrchestratorNodeState::Degraded);
    assert!(
        info.detail.as_deref().is_some_and(|text| text.contains("connection")),
        "{info:?}"
    );

    let resumed = orchestrator.node_reconnected(&node).expect("reconnect");
    assert_eq!(resumed.len(), 1, "{resumed:?}");
    assert_eq!(resumed[0].card.state, OrchestratorNodeState::Running);
    let sent = sent.lock().expect("recording transport log");
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(matches!(sent[0], OrchestratorCommand::Start { .. }));
    assert!(matches!(sent[1], OrchestratorCommand::Resume { .. }));
}

#[test]
fn orchestrator_new_on_the_home_node_keeps_the_local_path() {
    let env = TestEnv::new();
    let home = NodeName::parse("local").expect("home node");
    let (orchestrator, _, _, _, _) = manager(deps(&env));
    let info = orchestrator
        .create_on_node(&request("home-node", OrchestratorApp::Chat), Some(home.clone()))
        .expect("create on the home node");
    assert_eq!(info.card.node.as_ref(), Some(&home));
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    assert!(
        info.dir.join("AGENTS.md").is_file(),
        "the home path stopped writing its local files"
    );
    let state = read_json(&state_path(&info.dir));
    assert_eq!(state["node"], "local");
}

#[test]
fn orchestrator_resume_all_marks_an_offline_node_degraded() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let orchestrator = Orchestrator::with_transport(
        deps(&env),
        Vec::new(),
        Arc::new(RecordingTransport {
            sent: Arc::clone(&sent),
        }),
    );
    let info = orchestrator
        .create_on_node(&request("mac", OrchestratorApp::Opencode), Some(node.clone()))
        .expect("create on node");
    assert_eq!(info.card.state, OrchestratorNodeState::Running);
    let sent_before = sent.lock().expect("recording transport log").len();

    env.age_node(&node);
    let resumed = orchestrator.resume_all().expect("resume all");
    assert!(resumed.is_empty(), "an offline node was resumed: {resumed:?}");
    let info = orchestrator.info("mac").expect("info after resume all");
    assert_eq!(info.card.state, OrchestratorNodeState::Degraded);
    assert!(info.host.is_none(), "resume_all started a home host");
    assert_eq!(
        sent.lock().expect("recording transport log").len(),
        sent_before,
        "resume_all sent a command to an offline node"
    );
    let state = read_json(&state_path(&info.dir));
    assert_eq!(state["state"], "degraded");
}

#[test]
fn orchestrator_apply_result_correlates_the_late_node_result() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let orchestrator = Orchestrator::with_transport(
        deps(&env),
        Vec::new(),
        Arc::new(QueuedTransport {
            sent: Arc::clone(&sent),
        }),
    );
    let info = orchestrator
        .create_on_node(&request("mac", OrchestratorApp::Opencode), Some(node.clone()))
        .expect("create on node");
    assert_eq!(info.card.state, OrchestratorNodeState::Starting);
    let command = sent
        .lock()
        .expect("queued transport log")
        .first()
        .cloned()
        .expect("queued start");

    let unknown = OrchestratorResult::accepted("not-this-request", OrchestratorNodeState::Running);
    assert!(
        orchestrator
            .apply_result(&node, &unknown)
            .expect("unknown result")
            .is_none(),
        "an uncorrelated result changed a record"
    );

    let mut result = OrchestratorResult::accepted(command.request_id(), OrchestratorNodeState::Running);
    result.detail = Some("started /srv/vowp/app token=FAKE-NOT-A-SECRET".to_owned());
    let applied = orchestrator
        .apply_result(&node, &result)
        .expect("apply result")
        .expect("correlated result");
    assert_eq!(applied.card.state, OrchestratorNodeState::Running);
    let detail = applied.detail.expect("redacted detail");
    assert!(!detail.contains("/srv/"), "{detail}");
    assert!(!detail.contains("FAKE-NOT-A-SECRET"), "{detail}");
}

#[test]
fn orchestrator_card_contains_no_path_or_secret() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let sent = Arc::new(Mutex::new(Vec::new()));
    let orchestrator = Orchestrator::with_transport(deps(&env), Vec::new(), Arc::new(RecordingTransport { sent }));
    let project = env.project_name().clone();
    let mut req = request("mac", OrchestratorApp::Opencode);
    req.project = Some(project);
    let info = orchestrator.create_on_node(&req, Some(node)).expect("create on node");

    let card = serde_json::to_string(&info.card).expect("card JSON");
    let state = std::fs::read_to_string(state_path(&info.dir)).expect("state file");
    let home = env.home_path().display().to_string();
    let project_path = env.project_path().display().to_string();
    for text in [&card, &state] {
        for forbidden in [
            home.as_str(),
            project_path.as_str(),
            "/home/",
            "/Users/",
            "token",
            "secret",
            "password",
            "127.0.0.1",
            "4096",
            "lease",
        ] {
            assert!(!text.contains(forbidden), "found {forbidden:?} in {text}");
        }
    }
    assert_eq!(info.card.node.as_ref().map(NodeName::as_str), Some("mac-studio"));
    assert_eq!(info.card.project.as_ref(), Some(env.project_name()));
}

/// Returns the built `vowp` binary, resolving from `CARGO_BIN_EXE_vowp` or the workspace
/// target directory so the lookup works regardless of Cargo's test-binary layout.
fn vowp_binary() -> std::path::PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_vowp").map(std::path::PathBuf::from)
        && path.is_file()
    {
        return path;
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = std::path::PathBuf::from(dir);
            if dir.is_absolute() { dir } else { manifest.join(dir) }
        }
        None => manifest.join("target"),
    };
    let name = format!("vowp{}", std::env::consts::EXE_SUFFIX);
    let mut candidates: Vec<std::path::PathBuf> = ["debug", "release"]
        .iter()
        .map(|profile| target.join(profile).join(&name))
        .collect();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent().and_then(std::path::Path::parent)
    {
        candidates.push(dir.join(&name));
    }
    if let Some(found) = candidates.iter().find(|path| path.is_file()) {
        return found.clone();
    }
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let _ = std::process::Command::new(cargo)
            .args(["build", "--quiet", "--bin", "vowp"])
            .current_dir(&manifest)
            .status();
    });
    if let Some(found) = candidates.iter().find(|path| path.is_file()) {
        return found.clone();
    }
    panic!("build the vowp binary first; missing {}", candidates[0].display());
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
fn orchestrator_move_requires_exactly_one_target_and_never_ssh_redirects() {
    let env = TestEnv::new();
    let (code, stdout, stderr) = env.run(&["orchestrator", "move", "missing"]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("exactly one of --node <NAME> or --home"), "{stderr}");

    let (code, stdout, stderr) = env.run(&["orchestrator", "move", "missing", "--home", "--node", "mac-studio"]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("not both"), "{stderr}");

    // The global `--node` is the move target, not an SSH redirect: an unknown orchestrator
    // is a local NotFound (exit 3), not a remote SSH failure (exit 7).
    let (code, stdout, stderr) = env.run(&["orchestrator", "move", "missing", "--node", "mac-studio"]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("orchestrator missing"), "{stderr}");
    assert!(
        !stderr.starts_with("remote:"),
        "the command was SSH-redirected: {stderr}"
    );
}

#[test]
fn orchestrator_move_preserves_the_session_between_home_and_an_agent() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    env.map_project(&node);
    let agent = FakeAgent::start(env.home_path(), &node);

    let (code, stdout, stderr) = env.run(&["orchestrator", "new", "--name", "mover", "--app", "chat"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let dir = env.orchestrator_dir("mover");
    let session = read_json(&state_path(&dir))["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let (code, card, stderr) = env.run_json(&["orchestrator", "move", "mover", "--node", node.as_str()]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(card["name"], "mover");
    assert_eq!(card["session_id"], session);
    assert_eq!(card["node"], "mac-studio");
    assert_eq!(card["state"], "running");
    let state = read_json(&state_path(&dir));
    assert_eq!(state["session_id"], session);
    assert_eq!(state["node"], "mac-studio");
    assert_eq!(state["state"], "running");
    assert!(matches!(
        agent.commands().last(),
        Some(OrchestratorCommand::Start { name, .. }) if name == "mover"
    ));

    let (code, card, stderr) = env.run_json(&["orchestrator", "move", "mover", "--home"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(card["name"], "mover");
    assert_eq!(card["session_id"], session);
    assert!(card["node"].is_null(), "the moved card still names a node: {card}");
    assert_eq!(card["state"], "running");
    let state = read_json(&state_path(&dir));
    assert_eq!(state["session_id"], session);
    assert!(state["node"].is_null(), "the state still names a node: {state}");
    assert_eq!(state["state"], "running");
    let commands = agent.commands();
    assert!(matches!(
        commands.first(),
        Some(OrchestratorCommand::Start { name, .. }) if name == "mover"
    ));
    assert!(matches!(
        commands.last(),
        Some(OrchestratorCommand::Stop { name, .. }) if name == "mover"
    ));
}

#[test]
fn orchestrator_move_to_a_revoked_node_keeps_the_home_host_running() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let (code, stdout, stderr) = env.run(&["orchestrator", "new", "--name", "mover", "--app", "chat"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let dir = env.orchestrator_dir("mover");
    let session = read_json(&state_path(&dir))["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    env.revoke_node(&node);

    let (code, stdout, stderr) = env.run(&["orchestrator", "move", "mover", "--node", node.as_str()]);
    assert_eq!(code, 5, "stdout: {stdout}\nstderr: {stderr}");
    let state = read_json(&state_path(&dir));
    assert_eq!(state["session_id"], session);
    assert!(state["node"].is_null(), "a refused move changed the placement: {state}");
    assert_eq!(state["state"], "running");
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

/// A runtime-assembled marker password; no secret-shaped literal is committed.
fn endpoint_password(label: &str) -> String {
    format!("vowp-test-{label}-{}", "E".repeat(20))
}

/// Writes one private `orchestrator.json` for an endpoint test.
fn write_endpoint_state(env: &TestEnv, name: &str, node: Option<NodeName>) -> (PathBuf, SessionId) {
    let dir = env.orchestrator_dir(name);
    std::fs::create_dir_all(&dir).expect("orchestrator dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("orchestrator mode");
    let session = SessionId::new();
    let value = serde_json::json!({
        "name": name,
        "app": "opencode",
        "session_id": session,
        "project": null,
        "executor": null,
        "state": "running",
        "created_at": Timestamp::now(),
        "join": {
            "desktop": format!("vowp orchestrator resume {name}"),
            "phone": format!("vowp chat {session}"),
            "chat": format!("vowp chat --agent --session {session}"),
        },
        "surface_id": null,
        "node": node,
        "last_request_id": null,
    });
    std::fs::write(state_path(&dir), serde_json::to_vec(&value).expect("state JSON")).expect("orchestrator.json");
    std::fs::set_permissions(state_path(&dir), std::fs::Permissions::from_mode(0o600)).expect("state mode");
    (dir, session)
}

/// Writes one live `host.json` shaped like the opencode host's own file.
fn write_endpoint_host(dir: &Path, port: u16) {
    let value = serde_json::json!({
        "kind": "opencode",
        "name": "endpoint",
        "pid": std::process::id(),
        "port": port,
        "url": format!("http://127.0.0.1:{port}"),
        "started_at": Timestamp::now(),
        "workdir": dir.display().to_string(),
        "service_json": dir.join(".opencode/state/opencode/service.json").display().to_string(),
        "native_session_id": "ses_cli_endpoint",
        "vowp_session": SessionId::new(),
    });
    std::fs::write(dir.join("host.json"), serde_json::to_vec(&value).expect("host JSON")).expect("host.json");
    std::fs::set_permissions(dir.join("host.json"), std::fs::Permissions::from_mode(0o600)).expect("host mode");
}

/// Writes the private opencode `service.json` below `dir`.
fn write_endpoint_service(dir: &Path, url: &str, port: u16, password: &str) {
    let path = dir.join(".opencode/state/opencode/service.json");
    let parent = path.parent().expect("service parent");
    std::fs::create_dir_all(parent).expect("service dir");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).expect("service dir mode");
    let value = serde_json::json!({
        "version": "2.0.28",
        "url": url,
        "port": port,
        "pid": std::process::id(),
        "password": password,
    });
    std::fs::write(&path, serde_json::to_vec(&value).expect("service JSON")).expect("service.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("service mode");
}

#[test]
fn orchestrator_endpoint_json_reports_home_and_node_locations() {
    let env = TestEnv::new();
    let (home_dir, home_session) = write_endpoint_state(&env, "home-main", None);
    let password = endpoint_password("home");
    write_endpoint_host(&home_dir, 45111);
    write_endpoint_service(&home_dir, "http://127.0.0.1:45111", 45111, &password);
    let node = env.agent_node("mac-studio");
    write_endpoint_state(&env, "mac-main", Some(node));

    let (code, value, stderr) = env.run_json(&["orchestrator", "endpoint", "home-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["name"], "home-main");
    assert_eq!(value["location"], "home");
    assert!(value["node"].is_null(), "{value}");
    assert_eq!(value["session_id"], home_session.as_str());
    assert_eq!(value["state"], "running");
    assert_eq!(value["url"], "http://127.0.0.1:45111");
    assert_eq!(value["native_session_id"], "ses_cli_endpoint");
    assert_eq!(value["auth"]["header"], "Authorization");
    assert_eq!(value["auth"]["scheme"], "Basic");
    assert_eq!(value["auth"]["username"], "opencode");
    assert_eq!(value["auth"]["password"], password);

    let (code, node_value, stderr) = env.run_json(&["orchestrator", "endpoint", "mac-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(node_value["name"], "mac-main");
    assert_eq!(node_value["location"], "node");
    assert_eq!(node_value["node"], "mac-studio");
    assert_eq!(node_value["state"], "running");
    assert!(node_value["url"].is_null(), "{node_value}");
    assert!(node_value["native_session_id"].is_null(), "{node_value}");
    assert!(node_value["auth"].is_null(), "{node_value}");

    // The global `--node` is not an SSH redirect for this inspection command: it is
    // consumed locally and the home record is still read from this home.
    let (code, value, stderr) = env.run_json(&["--node", "mac-studio", "orchestrator", "endpoint", "home-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["location"], "home");
    assert_eq!(value["url"], "http://127.0.0.1:45111");

    let (code, stdout, stderr) = env.run(&["orchestrator", "endpoint", "mac-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("vowp agent orchestrator endpoint mac-main --json"),
        "the human output points at the agent machine: {stdout}"
    );
    assert!(
        !stdout.contains(&password),
        "the human output carries no password: {stdout}"
    );
}

#[test]
fn orchestrator_endpoint_remote_never_returns_auth() {
    let env = TestEnv::new();
    let node = env.agent_node("mac-studio");
    let (dir, _session) = write_endpoint_state(&env, "mac-main", Some(node));
    // Bait: a node record's home directory has no local host, and even a stray service
    // file must not become credentials in the metadata-only document.
    let password = endpoint_password("remote");
    write_endpoint_host(&dir, 45112);
    write_endpoint_service(&dir, "http://127.0.0.1:45112", 45112, &password);

    let (code, value, stderr) = env.run_json(&["orchestrator", "endpoint", "mac-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["location"], "node");
    assert!(value["auth"].is_null(), "a remote endpoint has no auth: {value}");
    assert!(value["url"].is_null(), "a remote endpoint has no url: {value}");
    assert!(value["native_session_id"].is_null(), "{value}");
    let text = value.to_string();
    assert!(!text.contains(&password), "the remote document carries no password");
    assert!(!text.contains("45112"), "the remote document carries no port");

    let (code, text, stderr) = env.run(&["orchestrator", "endpoint", "mac-main"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        text.contains("vowp agent orchestrator endpoint mac-main --json"),
        "the owner is told where to run the local command: {text}"
    );
    assert!(!text.contains(&password), "the human output carries no password");

    let (code, _stdout, stderr) = env.run(&["orchestrator", "endpoint", "missing"]);
    assert_eq!(code, 3, "an unknown orchestrator is not found: {stderr}");
}

#[test]
fn orchestrator_endpoint_redacts_a_corrupt_private_file_error() {
    let env = TestEnv::new();
    let (dir, _session) = write_endpoint_state(&env, "corrupt-main", None);
    write_endpoint_host(&dir, 45113);
    let password = endpoint_password("corrupt");
    let service = dir.join(".opencode/state/opencode/service.json");
    let parent = service.parent().expect("service parent");
    std::fs::create_dir_all(parent).expect("service dir");
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).expect("service dir mode");
    std::fs::write(
        &service,
        format!("{{\"url\":\"http://127.0.0.1:45113\",\"password\":\"{password}\","),
    )
    .expect("corrupt service");
    std::fs::set_permissions(&service, std::fs::Permissions::from_mode(0o600)).expect("service mode");

    let (code, stdout, stderr) = env.run(&["--json", "orchestrator", "endpoint", "corrupt-main"]);
    assert_ne!(code, 0, "a corrupt service file is an error; stdout: {stdout}");
    let text = format!("{stdout}\n{stderr}");
    assert!(
        text.contains("corrupt-main"),
        "the error names the orchestrator: {text}"
    );
    for forbidden in [
        password.as_str(),
        service.to_str().expect("service path"),
        dir.to_str().expect("orchestrator dir"),
        "45113",
        "127.0.0.1",
        "service.json",
    ] {
        assert!(!text.contains(forbidden), "the error leaked a forbidden value: {text}");
    }

    // A corrupt home record names the orchestrator, never its path.
    let (broken, _session) = write_endpoint_state(&env, "broken", None);
    std::fs::write(state_path(&broken), b"{\"name\":").expect("corrupt record");
    std::fs::set_permissions(state_path(&broken), std::fs::Permissions::from_mode(0o600)).expect("record mode");
    let (code, stdout, stderr) = env.run(&["orchestrator", "endpoint", "broken"]);
    let text = format!("{stdout}\n{stderr}");
    assert_eq!(code, 2, "a corrupt home record is invalid input: {text}");
    assert!(text.contains("broken"), "the error names the orchestrator: {text}");
    for forbidden in [
        broken.to_str().expect("broken dir"),
        state_path(&broken).to_str().expect("record path"),
        "orchestrator.json",
    ] {
        assert!(!text.contains(forbidden), "the error leaked a forbidden value: {text}");
    }
}

// ---------------------------------------------------------------------------
// `vowp orchestrator attach` and the owner/access view of `endpoint`
// ---------------------------------------------------------------------------

/// Writes a live home-hosted orchestrator bound to a real session in the test project.
fn live_attach_target(env: &TestEnv, name: &str, password: &str) -> SessionId {
    let session = vowp_core::session::service::SessionService::new(deps(env))
        .create_session(vowp_protocol::NewSession {
            project: Some(env.project_name().clone()),
            job_id: None,
            role: SessionRole::Orchestrator,
            title: format!("orchestrator {name}"),
            cwd: PathBuf::from("."),
            parent: None,
            executor: None,
        })
        .expect("session");
    let (dir, _unused) = write_endpoint_state(env, name, None);
    let mut state = read_json(&state_path(&dir));
    state["session_id"] = serde_json::json!(session);
    state["project"] = serde_json::json!(env.project_name());
    std::fs::write(state_path(&dir), serde_json::to_vec(&state).expect("state JSON")).expect("orchestrator.json");
    std::fs::set_permissions(state_path(&dir), std::fs::Permissions::from_mode(0o600)).expect("state mode");
    write_endpoint_host(&dir, 45901);
    write_endpoint_service(&dir, "http://127.0.0.1:45901", 45901, password);
    session
}

fn lease_row(env: &TestEnv, session: &SessionId) -> Option<vowp_store::repo_surfaces::OwnerRecord> {
    env.store().ownership(session).expect("ownership")
}

#[test]
fn orchestrator_attach_json_prints_a_clean_grant_and_the_notice_goes_to_stderr() {
    let env = TestEnv::new();
    let password = endpoint_password("attach-json");
    let session = live_attach_target(&env, "main", &password);

    // The in-process stdin is empty, so the hold ends at once and releases the lease.
    let (code, stdout, stderr) = env.run(&[
        "--json",
        "orchestrator",
        "attach",
        "main",
        "--holder",
        "opencode-desktop",
        "--label",
        "StackSpirit · macbook",
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let value: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("stdout is not JSON: {error}: {stdout}"));
    let token = value["token"].as_str().expect("token");
    assert_eq!(token.len(), 52);
    assert_eq!(value["owner"]["kind"], "desktop_ui");
    assert_eq!(value["owner"]["holder"], "opencode-desktop");
    assert_eq!(value["owner"]["label"], "StackSpirit · macbook");
    assert_eq!(value["endpoint"]["access"], "owner");
    assert_eq!(value["endpoint"]["url"], "http://127.0.0.1:45901");
    assert_eq!(
        value["endpoint"]["auth"]["password"], password,
        "the JSON path keeps the password"
    );
    assert!(value["expires_at"].is_string() && value["surface_id"].is_string());
    assert!(
        stderr.contains("attach: holding ownership; close stdin to release"),
        "the hold notice is on stderr: {stderr}"
    );
    assert!(
        stderr.contains("closing stdin releases"),
        "the warning is on stderr: {stderr}"
    );
    assert!(
        !stdout.contains("holding ownership"),
        "stdout stays JSON only: {stdout}"
    );
    assert!(
        !stderr.contains(token) && !stderr.contains(&password),
        "stderr never carries a secret"
    );
    assert!(lease_row(&env, &session).is_none(), "EOF on stdin released the lease");
}

#[test]
fn orchestrator_attach_human_block_has_the_grant_fields_and_no_password() {
    let env = TestEnv::new();
    let password = endpoint_password("attach-human");
    live_attach_target(&env, "main", &password);
    let (code, stdout, stderr) = env.run(&["orchestrator", "attach", "main", "--holder", "ui"]);
    assert_eq!(code, 0, "{stderr}");
    for key in [
        "surface: ",
        "holder: ui",
        "token: ",
        "access: owner",
        "url: http://127.0.0.1:45901",
        "expires: ",
    ] {
        assert!(stdout.contains(key), "missing {key:?}: {stdout}");
    }
    assert!(
        !stdout.contains(&password),
        "the human block never prints the password: {stdout}"
    );
    assert!(!stderr.contains(&password));
}

#[test]
fn orchestrator_attach_refuses_node_hosted_stopped_and_live_owner() {
    let env = TestEnv::new();
    let password = endpoint_password("attach-refuse");
    let node = env.agent_node("mac-studio");
    write_endpoint_state(&env, "mac-main", Some(node));
    let (code, stdout, stderr) = env.run(&["orchestrator", "attach", "mac-main", "--holder", "ui"]);
    assert_ne!(code, 0, "a node-hosted orchestrator is refused: {stdout}");
    assert!(stderr.contains("mac-main") && stderr.contains("node"), "{stderr}");

    let (dir, _session) = write_endpoint_state(&env, "stopped", None);
    let mut state = read_json(&state_path(&dir));
    state["state"] = serde_json::json!("stopped");
    std::fs::write(state_path(&dir), serde_json::to_vec(&state).expect("state JSON")).expect("orchestrator.json");
    let (code, _stdout, stderr) = env.run(&["orchestrator", "attach", "stopped", "--holder", "ui"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("vowp orchestrator resume stopped"), "{stderr}");

    let session = live_attach_target(&env, "main", &password);
    let core = Orchestrator::new(deps(&env));
    let held = core.attach("main", "first-ui", None, false).expect("first attach");
    let (code, _stdout, stderr) = env.run(&["orchestrator", "attach", "main", "--holder", "second-ui"]);
    assert_ne!(code, 0, "a live owner refuses an attach without --takeover");
    assert!(stderr.contains("first-ui"), "{stderr}");
    assert!(!stderr.contains(&password) && !stderr.contains(&held.grant().token));
    let (code, _stdout, stderr) = env.run(&["orchestrator", "attach", "main", "--holder", "second-ui", "--takeover"]);
    assert_eq!(code, 0, "{stderr}");
    drop(held);
    let _ = session;
}

#[test]
fn orchestrator_endpoint_reports_owner_and_access_for_a_surface() {
    let env = TestEnv::new();
    let password = endpoint_password("endpoint-owner");
    let session = live_attach_target(&env, "main", &password);
    let core = Orchestrator::new(deps(&env));
    let guard = core
        .attach("main", "opencode-desktop", Some("StackSpirit"), false)
        .expect("attach");
    let surface = guard.grant().surface_id.to_string();

    let (code, value, stderr) = env.run_json(&["orchestrator", "endpoint", "main"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["access"], "read_only", "no caller identity is read-only");
    assert_eq!(value["owner"]["holder"], "opencode-desktop");
    assert_eq!(value["owner"]["session_id"], session.as_str());
    let (code, value, stderr) = env.run_json(&["orchestrator", "endpoint", "main", "--surface", &surface]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["access"], "owner");

    let (code, text, stderr) = env.run(&["orchestrator", "endpoint", "main", "--surface", &surface]);
    assert_eq!(code, 0, "{stderr}");
    assert!(text.contains("access: owner"), "{text}");
    assert!(
        text.contains("owner: StackSpirit (desktop-ui) holder opencode-desktop"),
        "{text}"
    );
    assert!(text.contains("mode interactive state active"), "{text}");
    assert!(
        !text.contains(&password),
        "the human output carries no password: {text}"
    );

    let (code, text, _stderr) = env.run(&["orchestrator", "endpoint", "main", "--surface", "not-an-id"]);
    assert_eq!(code, 2, "a malformed surface id is invalid input: {text}");
    drop(guard);
}

#[test]
fn attach_holds_the_lease_until_stdin_closes_and_then_releases_it() {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let env = TestEnv::new();
    let password = endpoint_password("attach-hold");
    let session = live_attach_target(&env, "main", &password);
    let mut child = Command::new(vowp_binary())
        .args([
            "--home",
            &env.home_path().display().to_string(),
            "--project",
            &env.project_path().display().to_string(),
            "--json",
            "orchestrator",
            "attach",
            "main",
            "--holder",
            "ui-child",
        ])
        .env("NO_COLOR", "1")
        .env_remove("VOWP_HOME")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn vowp orchestrator attach");
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    // The grant is one pretty-printed JSON document; the process keeps stdout open, so parse
    // exactly one value instead of reading to end-of-file.
    let grant: Value = serde_json::Deserializer::from_reader(&mut stdout)
        .into_iter::<Value>()
        .next()
        .expect("a grant document")
        .expect("the grant is JSON");
    let token = grant["token"].as_str().expect("token").to_owned();

    // While stdin is open the lease is held and belongs to this grant.
    let held = lease_row(&env, &session).expect("the attach process holds the lease");
    assert_eq!(held.lease.token, hash_lease_token(&token));
    assert_eq!(held.lease.holder, "ui-child");
    thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the process keeps running while stdin is open"
    );
    assert!(lease_row(&env, &session).is_some());

    drop(child.stdin.take()); // EOF
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the attach process did not exit after stdin closed"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "attach exits 0 after release: {status:?}");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("rest of stdout");
    assert!(rest.trim().is_empty(), "stdout carries only the grant: {rest:?}");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("stderr");
    assert!(
        stderr.contains("attach: holding ownership; close stdin to release"),
        "{stderr}"
    );
    assert!(!stderr.contains(&token) && !stderr.contains(&password));
    assert!(
        lease_row(&env, &session).is_none(),
        "closing stdin released the lease row"
    );
}
