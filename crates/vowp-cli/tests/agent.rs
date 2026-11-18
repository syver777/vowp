//! `vowp agent` CLI tests.
//!
//! Every test uses a temporary `HOME` or in-process parsing; none reads the real
//! `~/.vowp-agent`, opens a store or starts ssh.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use ed25519_dalek::{Signer, SigningKey};
use tempfile::TempDir;
use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, AgentStatus, Cache, ConnectionState, FrameRead, FrameStream,
    NodeKey, PushStatus, RunHandler, RunOptions, RunOutcome as AgentRunOutcome, RunSink, RunSinkError, RunSummary,
    ServiceError, SshTransport, SurfacePairRequest, SystemAgentClock, Transport, TransportConfig, WssTlsMode,
    pending_spool_bytes,
};
use vowp_cli::agent_exec::CoreRunHandler;
use vowp_cli::cli::{AgentCommand, Cli, Command};
use vowp_cli::cmd::agent::{enrollment_transport_config, service_error_exit_code};
use vowp_cli::node_ctx::ControlConfig;
use vowp_cli::{BufferIo, Ctx, Global};
use vowp_core::agent::gateway::{self, GatewayOptions};
use vowp_core::agent::pair::{create_pending_surface, issue_surface_token, verify_surface_token};
use vowp_core::deps::Deps;
use vowp_core::surface::orchestrator::{CommandSpool, NewOrchestrator, Orchestrator};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, AgentRole, AssignmentPayload,
    ErrorKind, Executor, JobId, NodeKind, NodeName, NodeStatus, OrchestratorApp, PathRef, Project, ProjectName,
    RemoteEvent, RemoteLine, RemoteRunState, RunId, RunOutcome, RunPurpose, RunRejectReason, SecretName, Session,
    SessionId, SessionRole, SessionState, StopReason, SurfaceId, SurfaceKind, SurfaceState, Timestamp, TurnSummary,
    VowpConfig,
};
use vowp_store::{NodeCredential, NodePath, NodePaths, Store};

mod common;

use common::TestEnv;

fn run_binary(home: &Path, args: &[&str]) -> (i32, String, String) {
    run_binary_with(home, args, &[])
}

/// Runs the real binary with an isolated home and extra environment values.
fn run_binary_with(home: &Path, args: &[&str], envs: &[(&str, String)]) -> (i32, String, String) {
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"));
    command
        .args(args)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("VOWP_AGENT_HOME")
        .env_remove("VOWP_HOME")
        .env_remove("VOWP_CONTROL_HOME")
        .env_remove("XDG_CONFIG_HOME");
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("run vowp");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn agent_status_without_enrollment_exits_three() {
    let home = TempDir::new().expect("temp home");
    let (code, stdout, stderr) = run_binary(home.path(), &["agent", "status"]);

    assert_eq!(code, 3, "no enrollment is exit 3; stderr: {stderr}");
    assert!(stdout.is_empty(), "status failure prints no document: {stdout}");
    assert!(
        stderr.contains("vowp agent enroll --target") && stderr.contains("--code-file"),
        "the error must hint at enrollment: {stderr}"
    );
}

#[test]
fn agent_status_prints_json() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let node = NodeName::parse("mac-studio").expect("node slug");
    let config = AgentConfig::new("home-1", node.clone(), state_dir.clone(), "home.example");
    config.write().expect("agent.json");

    let heartbeat = Timestamp::parse("2026-11-14T10:00:00.000Z").expect("timestamp");
    let status = AgentStatus {
        connection: ConnectionState::Connected,
        node: Some(node.clone()),
        home: Some("home-1".to_owned()),
        protocol: Some(AGENT_PROTOCOL.to_owned()),
        last_heartbeat: Some(heartbeat),
        last_error: None,
        pending_spool_bytes: 42,
        key_fingerprint: Some("sha256:0123456789abcdef".to_owned()),
        surface_token_expires_at: None,
        cache_entries: 0,
        cache_bytes: 0,
        cache_oldest_at: None,
    };
    status.write(&state_dir).expect("status.json");
    std::fs::create_dir_all(state_dir.join("spool")).expect("spool dir");
    std::fs::write(state_dir.join("spool/pending.json"), vec![b'x'; 42]).expect("spool bytes");
    let cached_at = Timestamp::parse("2026-11-14T09:00:00.000Z").expect("timestamp");
    let surface = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned());
    Cache::new(&state_dir)
        .store(
            "fleet_status",
            &surface,
            &serde_json::json!({"project": "vowp"}),
            &serde_json::json!({"content": [{"type": "text", "text": "{}"}], "isError": false}),
            cached_at,
        )
        .expect("cache entry");

    let (code, stdout, stderr) = run_binary(home.path(), &["--json", "agent", "status"]);
    assert_eq!(code, 0, "status reports even when stopped; stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("status JSON");
    assert_eq!(value["connection"], "connected");
    assert_eq!(value["node"], "mac-studio");
    assert_eq!(value["home"], "home-1");
    assert_eq!(value["protocol"], AGENT_PROTOCOL);
    assert_eq!(value["pending_spool_bytes"], 42);
    assert_eq!(value["last_heartbeat"], "2026-11-14T10:00:00.000Z");
    assert_eq!(value["key_fingerprint"], "sha256:0123456789abcdef");
    assert_eq!(value["cache_entries"], 1);
    assert!(value["cache_bytes"].as_u64().expect("cache bytes") > 0);
    assert_eq!(value["cache_oldest_at"], "2026-11-14T09:00:00.000Z");

    let (code, human, stderr) = run_binary(home.path(), &["agent", "status"]);
    assert_eq!(code, 0, "human status failed: {stderr}");
    assert!(human.contains("cache: 1 entries"), "{human}");
    assert!(human.contains("cache oldest: 2026-11-14T09:00:00.000Z"), "{human}");
}

#[test]
fn agent_enroll_help_names_code_file() {
    let mut io = BufferIo::new("");
    let code = vowp_cli::run(
        vec![
            "vowp".to_owned(),
            "agent".to_owned(),
            "enroll".to_owned(),
            "--help".to_owned(),
        ],
        &mut io,
    );
    assert_eq!(code, 0, "help exits 0; stderr: {}", io.stderr());
    let help = io.stdout();
    assert!(help.contains("--code-file"), "help must name --code-file: {help}");
    assert!(help.contains("--target"), "help must name --target: {help}");
    assert!(help.contains("--wss"), "help must name --wss: {help}");
    assert!(
        help.contains("--tls-fingerprint"),
        "help must name --tls-fingerprint: {help}"
    );
    assert!(help.contains("PATH|-"), "help must document stdin: {help}");
}

#[test]
fn agent_enroll_accepts_code_file_stdin() {
    let cli = Cli::try_parse_from([
        "vowp",
        "agent",
        "enroll",
        "--target",
        "home.example",
        "--code-file",
        "-",
    ])
    .expect("parse agent enroll");
    match cli.command {
        Command::Agent {
            command: AgentCommand::Enroll(args),
        } => {
            assert_eq!(args.target.as_deref(), Some("home.example"));
            assert_eq!(args.code_file, "-");
            assert!(args.project.is_empty());
            assert!(args.worktree_root.is_none());
            assert!(args.wss.is_none());
            assert!(args.tls_fingerprint.is_none());
        }
        other => panic!("expected agent enroll, got {other:?}"),
    }
}

/// Creates an enrolled agent config under `home` for the path-map commands.
fn enrolled_agent_config(home: &Path) -> AgentConfig {
    let state_dir = home.join(".vowp-agent");
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        state_dir,
        "home.example",
    );
    config.write().expect("agent.json");
    config
}

/// Creates a temporary checkout and the map `vowp agent paths add` should produce.
fn checkout_map(home: &Path, directory: &str, project: &str) -> (PathBuf, AgentPathMap) {
    let root = home.join(directory);
    let worktrees = root.join(".vowp/worktrees");
    std::fs::create_dir_all(&worktrees).expect("worktree root");
    std::fs::write(root.join("vowp.toml"), format!("[project]\nname = \"{project}\"\n")).expect("vowp.toml");
    let root = std::fs::canonicalize(root).expect("canonical checkout");
    let worktrees = std::fs::canonicalize(worktrees).expect("canonical worktrees");
    let map = AgentPathMap {
        project: ProjectName::parse(project).expect("project slug"),
        local_root: root.to_str().expect("utf-8").to_owned(),
        worktree_root: Some(worktrees.to_str().expect("utf-8").to_owned()),
        allow: Vec::new(),
    };
    (root, map)
}

// ---------------------------------------------------------------------------
// Gateway stdio fixtures: a real process over stdin/stdout
// ---------------------------------------------------------------------------

/// One isolated unix home whose `$HOME/.vowp` is an initialised node home.
struct GatewayHome {
    unix_home: TempDir,
}

impl GatewayHome {
    /// Creates the home with the reserved `_node` project and the named real projects.
    fn new(projects: &[&str]) -> Self {
        let unix_home = TempDir::new().expect("temp home");
        let paths = NodePaths::new(unix_home.path().join(".vowp"));
        let store = Store::open(&paths).expect("store opens");
        let now = store.clock().now();
        let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT).expect("node project");
        if store.project(&node_project).expect("node project lookup").is_none() {
            store
                .upsert_project(&Project {
                    name: node_project.clone(),
                    path: paths.root().display().to_string(),
                    remote_url: None,
                    default_branch: "main".to_owned(),
                    config: serde_json::json!({}),
                    created_at: now,
                    updated_at: now,
                })
                .expect("node project row");
        }
        Store::create_project_db(&paths, &node_project).expect("node project database");
        for name in projects {
            let project = ProjectName::parse(name).expect("project slug");
            let checkout = unix_home.path().join(format!("{name}-checkout"));
            std::fs::create_dir_all(&checkout).expect("project checkout");
            let mut config = VowpConfig::with_defaults_for_tests();
            config.project.name = project.clone();
            config.project.path = Some(checkout.display().to_string());
            std::fs::write(
                checkout.join("vowp.toml"),
                toml::to_string_pretty(&config).expect("config serialises"),
            )
            .expect("vowp.toml");
            store
                .upsert_project(&Project {
                    name: project.clone(),
                    path: checkout.display().to_string(),
                    remote_url: None,
                    default_branch: "main".to_owned(),
                    config: serde_json::to_value(&config).expect("config json"),
                    created_at: now,
                    updated_at: now,
                })
                .expect("project row");
            Store::create_project_db(&paths, &project).expect("project database");
        }
        Self { unix_home }
    }

    fn home(&self) -> &Path {
        self.unix_home.path()
    }

    /// Returns the node home root (`$HOME/.vowp`) the home CLI receives with `--home`.
    fn root(&self) -> PathBuf {
        self.paths().root().to_path_buf()
    }

    fn paths(&self) -> NodePaths {
        NodePaths::new(self.home().join(".vowp"))
    }

    fn checkout(&self, name: &str) -> PathBuf {
        self.home().join(format!("{name}-checkout"))
    }

    fn store(&self) -> Store {
        Store::open(&self.paths()).expect("store opens")
    }
}

/// Spawns the real `vowp agent gateway --stdio` with `$HOME` and a non-project cwd.
fn spawn_gateway_stdio(home: &Path, cwd: &Path) -> Child {
    ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["agent", "gateway", "--stdio"])
        .current_dir(cwd)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("VOWP_HOME")
        .env_remove("VOWP_CONTROL_HOME")
        .env_remove("VOWP_AGENT_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent gateway")
}

/// Writes one agent frame as a JSON line.
fn write_agent_frame(stdin: &mut impl Write, frame: &AgentFrame) {
    let text = serde_json::to_string(frame).expect("frame JSON");
    writeln!(stdin, "{text}").expect("write agent frame");
    stdin.flush().expect("flush agent frame");
}

/// Reads one agent frame line.
fn read_agent_frame(reader: &mut impl BufRead) -> AgentFrame {
    let mut line = String::new();
    let read = reader.read_line(&mut line).expect("read agent frame");
    assert!(read > 0, "the gateway closed before the next frame");
    serde_json::from_str(line.trim()).unwrap_or_else(|error| panic!("invalid agent frame {line:?}: {error}"))
}

/// Returns the hex Ed25519 signature over the handshake payload.
fn signature_hex(key: &SigningKey, nonce: &str, home: &str, connection: &str) -> String {
    key.sign(&vowp_core::agent::auth::handshake_payload(nonce, home, connection))
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Returns the lowercase hex public key.
fn public_hex(key: &SigningKey) -> String {
    key.verifying_key()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Names-only capabilities that map one project.
fn gateway_capabilities(project: &ProjectName) -> AgentCapabilities {
    AgentCapabilities {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        agent_version: "0.3.0".to_owned(),
        harnesses: BTreeMap::from([("opencode".to_owned(), "1.0.0".to_owned())]),
        git_version: Some("git version 2.47.0".to_owned()),
        labels: vec!["linux".to_owned()],
        secret_names: Vec::new(),
        projects: vec![project.clone()],
    }
}

/// One idle node capacity snapshot.
fn gateway_capacity() -> AgentCapacity {
    AgentCapacity {
        cpu_cores: 8,
        load1: 0.1,
        mem_total_bytes: 16 * 1024 * 1024 * 1024,
        mem_free_bytes: 8 * 1024 * 1024 * 1024,
        disk_free_bytes: 100 * 1024 * 1024 * 1024,
        busy_slots: 0,
        max_slots: 4,
        on_battery: Some(false),
        thermal_state: Some("nominal".to_owned()),
    }
}

/// Redeems one enrollment code on its own connection.
///
/// `serve` handles exactly one connection: an `Enroll` is answered with `Enrolled` and the
/// gateway exits, then the agent reconnects for the handshake. The test does the same.
fn enroll_node(gateway_home: &GatewayHome, node: &NodeName, key: &SigningKey, project: &ProjectName) {
    enroll_node_with_public_key(gateway_home, node, &public_hex(key), project, Vec::new());
}

/// Redeems one enrollment code for an already generated public key and optional path map.
fn enroll_node_with_public_key(
    gateway_home: &GatewayHome,
    node: &NodeName,
    public_key: &str,
    project: &ProjectName,
    paths: Vec<AgentPathMap>,
) {
    let store = gateway_home.store();
    let pending =
        vowp_core::agent::create_pending(&store, node, &["linux".to_owned()], Timestamp::now()).expect("pending node");
    let mut child = spawn_gateway_stdio(gateway_home.home(), gateway_home.home());
    let mut stdin = child.stdin.take().expect("gateway stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("gateway stdout"));
    write_agent_frame(
        &mut stdin,
        &AgentFrame::Enroll {
            code: pending.code,
            public_key: public_key.to_owned(),
            agent_version: "0.3.0".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            labels: vec!["linux".to_owned()],
            capabilities: gateway_capabilities(project),
            paths,
        },
    );
    match read_agent_frame(&mut stdout) {
        AgentFrame::Enrolled { node: enrolled, .. } => assert_eq!(enrolled, *node),
        other => panic!("expected Enrolled, got {other:?}"),
    }
    drop(stdin);
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "an enrollment connection exits 0");
}

/// Writes a fake `ssh` that runs the real gateway against `gateway_home`.
///
/// Every invocation appends one line to `log`, so a test can prove the agent opened exactly
/// one connection and never re-enrolled or reconnected.
fn fake_gateway_ssh(tools: &Path, gateway_home: &Path, log: &Path) -> PathBuf {
    std::fs::create_dir_all(tools).expect("fake tools dir");
    let script = tools.join("ssh");
    let binary = env!("CARGO_BIN_EXE_vowp");
    let root = gateway_home.join(".vowp");
    let text = format!(
        "#!/bin/sh\nprintf 'connect\\n' >> \"{}\"\nHOME=\"{}\" exec \"{}\" --home \"{}\" agent gateway --stdio\n",
        log.display(),
        gateway_home.display(),
        binary,
        root.display()
    );
    std::fs::write(&script, text).expect("fake ssh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod ssh");
    script
}

/// Returns whether `check` succeeds before `timeout`, polling every 20 ms.
fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Opens a new gateway connection and completes the AgentHello challenge.
fn handshake_node(
    gateway_home: &GatewayHome,
    node: &NodeName,
    key: &SigningKey,
    project: &ProjectName,
) -> (Child, std::process::ChildStdin, BufReader<std::process::ChildStdout>) {
    let mut child = spawn_gateway_stdio(gateway_home.home(), gateway_home.home());
    let mut stdin = child.stdin.take().expect("gateway stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("gateway stdout"));
    write_agent_frame(
        &mut stdin,
        &AgentFrame::AgentHello {
            node: node.clone(),
            key_id: public_hex(key),
            agent_version: "0.3.0".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            labels: vec!["linux".to_owned()],
            capabilities: gateway_capabilities(project),
        },
    );
    let AgentFrame::HomeChallenge {
        nonce,
        home_id,
        connection_id,
        ..
    } = read_agent_frame(&mut stdout)
    else {
        panic!("expected HomeChallenge");
    };
    write_agent_frame(
        &mut stdin,
        &AgentFrame::AgentAuth {
            signature: signature_hex(key, &nonce, &home_id, &connection_id),
        },
    );
    match read_agent_frame(&mut stdout) {
        AgentFrame::HomeWelcome { .. } => {}
        other => panic!("expected HomeWelcome, got {other:?}"),
    }
    (child, stdin, stdout)
}

/// Waits for the child to exit, failing after `timeout`.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll gateway") {
            return status;
        }
        assert!(Instant::now() < deadline, "the gateway did not exit in time");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reads everything the child wrote to stderr after it exited.
fn child_stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut text);
    }
    text
}

#[test]
fn agent_paths_add_update_and_remove_one_project() {
    let home = TempDir::new().expect("temp home");
    enrolled_agent_config(home.path());
    let (checkout, expected) = checkout_map(home.path(), "checkout", "vowp");
    let checkout_text = checkout.to_str().expect("utf-8 checkout");

    let (code, stdout, stderr) = run_binary(
        home.path(),
        &["agent", "paths", "add", "--project", checkout_text, "--allow", "src"],
    );
    assert_eq!(code, 0, "paths add failed: {stderr}");
    assert!(stdout.contains("vowp"), "{stdout}");
    assert!(
        stdout.contains(&expected.local_root),
        "the owner sees the root: {stdout}"
    );

    let state_dir = home.path().join(".vowp-agent");
    let config = AgentConfig::load(&state_dir).expect("stored config");
    let map = config.path_map(&expected.project).expect("added map").clone();
    assert_eq!(map.local_root, expected.local_root);
    assert_eq!(map.worktree_root, expected.worktree_root);
    assert_eq!(map.allow, vec!["src".to_owned()]);
    let mode = std::fs::metadata(AgentConfig::file_path(&state_dir))
        .expect("agent.json")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let (code, list, stderr) = run_binary(home.path(), &["--json", "agent", "paths", "ls"]);
    assert_eq!(code, 0, "paths ls failed: {stderr}");
    let list: serde_json::Value = serde_json::from_str(&list).expect("paths JSON");
    assert_eq!(list[0]["project"], "vowp");
    assert_eq!(list[0]["local_root"], expected.local_root);

    let other = home.path().join("other-worktrees");
    let other_text = other.to_str().expect("utf-8");
    let (code, _stdout, stderr) = run_binary(
        home.path(),
        &[
            "agent",
            "paths",
            "add",
            "--project",
            checkout_text,
            "--worktree-root",
            other_text,
            "--allow",
            "src",
            "--allow",
            "docs",
        ],
    );
    assert_eq!(code, 0, "paths update failed: {stderr}");
    let config = AgentConfig::load(&state_dir).expect("updated config");
    let map = config.path_map(&expected.project).expect("updated map");
    assert_eq!(
        map.worktree_root.as_deref(),
        Some(
            std::fs::canonicalize(&other)
                .expect("canonical")
                .to_str()
                .expect("utf-8")
        )
    );
    assert_eq!(map.allow, vec!["src".to_owned(), "docs".to_owned()]);

    let (code, stdout, stderr) = run_binary(home.path(), &["agent", "paths", "rm", "vowp"]);
    assert_eq!(code, 0, "paths rm failed: {stderr}");
    assert!(stdout.contains("unmapped project vowp"), "{stdout}");
    let config = AgentConfig::load(&state_dir).expect("config after rm");
    assert!(config.path_maps.is_empty());

    let (code, _stdout, stderr) = run_binary(home.path(), &["agent", "paths", "rm", "vowp"]);
    assert_eq!(code, 3, "an unknown project is not found: {stderr}");
    assert!(stderr.contains("not mapped"), "{stderr}");
}

#[test]
fn agent_paths_add_reaches_the_home_without_a_reconnect() {
    let gateway = GatewayHome::new(&["vowp", "docs"]);
    let node = NodeName::parse("mac-studio").expect("node slug");
    let project = ProjectName::parse("vowp").expect("project");
    let agent_home = TempDir::new().expect("agent home");
    let state_dir = agent_home.path().join(".vowp-agent");
    let key = NodeKey::generate(&state_dir).expect("node key");
    enroll_node_with_public_key(&gateway, &node, &key.public_key_hex(), &project, Vec::new());

    let tools = agent_home.path().join("bin");
    let log = agent_home.path().join("ssh.log");
    let ssh = fake_gateway_ssh(&tools, gateway.home(), &log);
    let config = AgentConfig::new("home-1", node.clone(), state_dir.clone(), "home.example");
    config.write().expect("agent.json");
    let mut client = AgentClient::new(
        state_dir,
        key,
        config,
        Box::new(SshTransport::with_binary("home.example", ssh.display().to_string())),
        Arc::new(SystemAgentClock),
    );
    client.connect().expect("agent handshake");

    let (docs_root, _map) = checkout_map(agent_home.path(), "docs-checkout", "docs");
    let (code, stdout, stderr) = run_binary(
        agent_home.path(),
        &[
            "agent",
            "paths",
            "add",
            "--project",
            docs_root.to_str().expect("utf-8 checkout"),
        ],
    );
    assert_eq!(code, 0, "paths add failed: {stdout} {stderr}");
    assert_eq!(
        client.refresh_path_maps().expect("send the path map update"),
        1,
        "only the added project is sent"
    );

    let store = gateway.store();
    let persisted = wait_until(Duration::from_secs(5), || {
        store
            .list_node_paths(&node)
            .expect("node paths")
            .iter()
            .any(|path| path.project.as_str() == "docs")
    });
    assert!(persisted, "the home gateway did not persist the PathMapUpdate frame");

    let root = gateway.root();
    let (code, card, stderr) = run_binary(
        gateway.home(),
        &[
            "--json",
            "--home",
            root.to_str().expect("utf-8 home"),
            "--project",
            "vowp",
            "node",
            "show",
            node.as_str(),
        ],
    );
    assert_eq!(code, 0, "node show failed: {stderr}");
    let card: serde_json::Value = serde_json::from_str(&card).expect("node card JSON");
    let mapped = card["mapped"].as_array().expect("mapped array");
    assert!(
        mapped.iter().any(|name| name == "docs"),
        "node show must list the new project without re-enrolling: {card}"
    );
    let text = card.to_string();
    assert!(
        !text.contains(docs_root.to_str().expect("utf-8 checkout")),
        "the redacted card leaked the local root: {text}"
    );
    let connections = std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    assert_eq!(
        connections, 1,
        "the path update must use the live connection, not a reconnect: {log:?}"
    );
}

#[test]
fn agent_status_redacts_absolute_paths_from_the_snapshot_cache() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let (_checkout, map) = checkout_map(home.path(), "checkout", "vowp");
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        state_dir.clone(),
        "home.example",
    )
    .with_path_maps(vec![map.clone()]);
    config.write().expect("agent.json");

    let status = AgentStatus {
        connection: ConnectionState::Connected,
        node: Some(NodeName::parse("mac-studio").expect("node slug")),
        home: Some("home-1".to_owned()),
        protocol: Some(AGENT_PROTOCOL.to_owned()),
        last_heartbeat: Some(Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")),
        last_error: None,
        pending_spool_bytes: 0,
        key_fingerprint: Some("sha256:0123456789abcdef".to_owned()),
        surface_token_expires_at: None,
        cache_entries: 0,
        cache_bytes: 0,
        cache_oldest_at: None,
    };
    status.write(&state_dir).expect("status.json");
    let cached_before = std::fs::read_to_string(AgentStatus::file_path(&state_dir)).expect("status cache");

    let (code, stdout, stderr) = run_binary(home.path(), &["--json", "agent", "status"]);
    assert_eq!(code, 0, "status failed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("status JSON");
    assert_eq!(value["paths"][0]["project"], "vowp");
    assert_eq!(
        value["paths"][0]["local_root"], map.local_root,
        "the owner-only JSON may carry the absolute root"
    );

    let cached_after = std::fs::read_to_string(AgentStatus::file_path(&state_dir)).expect("status cache");
    assert_eq!(
        cached_before, cached_after,
        "status must not rewrite its cache with a path map"
    );
    assert!(
        !cached_after.contains(&map.local_root),
        "the status snapshot cache keeps names only: {cached_after}"
    );

    let (code, human, stderr) = run_binary(home.path(), &["agent", "status"]);
    assert_eq!(code, 0, "human status failed: {stderr}");
    assert!(human.contains("paths:"), "{human}");
    assert!(
        human.contains(&map.local_root),
        "the owner sees the mapped root: {human}"
    );
}

#[test]
fn node_enroll_creates_a_pending_node_and_prints_the_code_once() {
    let env = TestEnv::new();
    let (code, value, stderr) = env.run_json(&[
        "node",
        "enroll",
        "mac-studio",
        "--label",
        "darwin-arm64",
        "--label",
        "gpu",
    ]);
    assert_eq!(code, 0, "node enroll failed: {stderr}");
    let text = value["code"].as_str().expect("code is printed once");
    assert_eq!(text.len(), 52, "the code is a 52-character token");
    assert_eq!(value["node"], "mac-studio");
    let command = value["command"].as_str().expect("enrollment command");
    assert!(command.contains("vowp agent enroll --target"), "{command}");
    assert!(command.contains("--code-file -"), "{command}");
    assert!(
        !stderr.contains(text),
        "the code must never be written to stderr: {stderr}"
    );

    let store = env.store();
    let name = NodeName::parse("mac-studio").expect("node slug");
    let card = store
        .node_card(&name, store.clock().now())
        .expect("card")
        .expect("node exists");
    assert_eq!(card.kind, NodeKind::Agent);
    assert_eq!(card.status, NodeStatus::Pending);
    assert_eq!(card.labels, vec!["darwin-arm64".to_owned(), "gpu".to_owned()]);
    let hash = vowp_core::agent::hash_code(text);
    assert_eq!(hash.len(), 64);
    assert!(
        store
            .consume_enrollment_code(&hash, store.clock().now())
            .expect("code lookup")
            .is_some(),
        "the store keeps the code hash"
    );
    let database = std::fs::read(env.home_path().join("vowp.db")).expect("database bytes");
    assert!(
        !database.windows(text.len()).any(|window| window == text.as_bytes()),
        "the plaintext code must not be stored in the database"
    );
}

#[test]
fn node_enroll_refuses_a_duplicate_node() {
    let env = TestEnv::new();
    let (code, _stdout, stderr) = env.run(&["node", "enroll", "mac-studio"]);
    assert_eq!(code, 0, "first enrollment failed: {stderr}");
    let store = env.store();
    let name = NodeName::parse("mac-studio").expect("node slug");
    store
        .set_node_status(&name, NodeStatus::Active, "", store.clock().now())
        .expect("activate node");

    let (code, _stdout, stderr) = env.run(&["node", "enroll", "mac-studio"]);
    assert_eq!(code, 4, "a duplicate node is a conflict: {stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
}

#[test]
fn node_enroll_replace_rotates_a_pending_code_and_node_rm_yes_removes_it() {
    let env = TestEnv::new();
    let name = NodeName::parse("mac-studio").expect("node slug");
    let (code, first, stderr) = env.run_json(&[
        "node",
        "enroll",
        "mac-studio",
        "--label",
        "darwin-arm64",
        "--label",
        "gpu",
    ]);
    assert_eq!(code, 0, "first enrollment failed: {stderr}");
    let first_code = first["code"].as_str().expect("first code").to_owned();

    let (code, second, stderr) = env.run_json(&[
        "node",
        "enroll",
        "mac-studio",
        "--replace",
        "--label",
        "darwin-arm64",
        "--label",
        "xcode",
    ]);
    assert_eq!(code, 0, "replace failed: {stderr}");
    let second_code = second["code"].as_str().expect("replacement code").to_owned();
    assert_ne!(first_code, second_code, "replace must rotate the one-time code");
    assert_eq!(second["node"], "mac-studio");

    let store = env.store();
    let now = store.clock().now();
    assert!(
        store
            .consume_enrollment_code(&vowp_core::agent::hash_code(&first_code), now)
            .expect("old code lookup")
            .is_none(),
        "the rotated code must not redeem"
    );
    assert!(
        store
            .consume_enrollment_code(&vowp_core::agent::hash_code(&second_code), now)
            .expect("new code lookup")
            .is_some(),
        "the replacement code must redeem"
    );
    let card = store.node_card(&name, now).expect("card lookup").expect("node exists");
    assert_eq!(card.status, NodeStatus::Pending);
    assert_eq!(card.labels, vec!["darwin-arm64".to_owned(), "xcode".to_owned()]);

    store
        .set_node_status(&name, NodeStatus::Active, "", now)
        .expect("activate node");
    let (code, _stdout, stderr) = env.run(&["node", "enroll", "mac-studio", "--replace"]);
    assert_eq!(code, 4, "an active node is never replaced: {stderr}");
    assert!(stderr.contains("active"), "{stderr}");
    assert!(stderr.contains("node rm mac-studio --yes"), "{stderr}");
    assert_eq!(
        store
            .node_card(&name, now)
            .expect("card lookup")
            .expect("node kept")
            .status,
        NodeStatus::Active,
        "a refused replace leaves the live node untouched"
    );

    let (code, removed, stderr) = env.run_json(&["node", "rm", "mac-studio", "--yes"]);
    assert_eq!(code, 0, "node rm --yes failed: {stderr}");
    assert_eq!(removed["name"], "mac-studio");
    assert!(
        store.require_node(&name).is_err(),
        "the removed node is gone from the registry"
    );
    let (code, _stdout, stderr) = env.run(&["node", "show", "mac-studio"]);
    assert_eq!(code, 3, "a removed node is not found: {stderr}");
    let (code, nodes, stderr) = env.run_json(&["node", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !nodes
            .as_array()
            .expect("node list")
            .iter()
            .any(|node| node["name"] == "mac-studio"),
        "the removed node is absent from node ls: {nodes}"
    );

    // A removed node must not leave an enrollment code that can still be redeemed.
    let (code, ghost, stderr) = env.run_json(&["node", "enroll", "ghost"]);
    assert_eq!(code, 0, "ghost enrollment failed: {stderr}");
    let ghost_code = ghost["code"].as_str().expect("ghost code").to_owned();
    let (code, _stdout, stderr) = env.run_json(&["node", "rm", "ghost", "--yes"]);
    assert_eq!(code, 0, "ghost removal failed: {stderr}");
    assert!(
        store
            .consume_enrollment_code(&vowp_core::agent::hash_code(&ghost_code), now)
            .expect("ghost code lookup")
            .is_none(),
        "node rm purges the node's stranded enrollment codes"
    );
}

#[test]
fn node_revoke_marks_the_node_and_revokes_credentials() {
    let env = TestEnv::new();
    let (code, _stdout, stderr) = env.run(&["node", "enroll", "mac-studio"]);
    assert_eq!(code, 0, "enrollment failed: {stderr}");
    let store = env.store();
    let now = store.clock().now();
    let name = NodeName::parse("mac-studio").expect("node slug");
    store
        .set_node_status(&name, NodeStatus::Active, "", now)
        .expect("activate node");
    store
        .upsert_node_credential(&NodeCredential {
            node: name.clone(),
            key_id: "key-1".to_owned(),
            public_key: "00".repeat(32),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
        })
        .expect("credential");

    let (code, value, stderr) = env.run_json(&["node", "revoke", "mac-studio", "--yes"]);
    assert_eq!(code, 0, "revoke failed: {stderr}");
    assert_eq!(value["node"], "mac-studio");
    assert_eq!(value["status"], "revoked");
    assert_eq!(value["credentials_revoked"], 1);
    let card = store.node_card(&name, now).expect("card").expect("node exists");
    assert_eq!(card.status, NodeStatus::Revoked);
    let credential = store
        .node_credential(&name, "key-1")
        .expect("credential lookup")
        .expect("credential exists");
    assert!(credential.revoked_at.is_some());

    let (code, _stdout, stderr) = env.run(&["node", "revoke", "missing", "--yes"]);
    assert_eq!(code, 3, "an unknown node is not found: {stderr}");
}

#[test]
fn node_show_returns_the_redacted_card() {
    let env = TestEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let name = NodeName::parse("mac-studio").expect("node slug");
    store
        .insert_pending_node(&name, &["darwin-arm64".to_owned()], now)
        .expect("pending node");
    store
        .set_node_status(&name, NodeStatus::Active, "", now)
        .expect("activate node");
    store
        .upsert_node_path(&NodePath {
            node: name.clone(),
            project: ProjectName::parse("vowp").expect("project"),
            local_root: "/Users/me/private/vowp".to_owned(),
            worktree_root: "/Users/me/private/vowp/.vowp/worktrees".to_owned(),
            allow: vec!["src/**".to_owned()],
            updated_at: now,
        })
        .expect("path map");
    let capabilities = serde_json::json!({
        "os": "darwin",
        "arch": "arm64",
        "agent_version": "0.3.0",
        "harnesses": {},
        "git_version": "git version 2.47.0",
        "labels": ["darwin-arm64"],
        "secret_names": ["DEPLOY_TOKEN"],
        "projects": ["vowp"],
        "capacity": {
            "cpu_cores": 8,
            "load1": 0.25,
            "mem_total_bytes": 17179869184_u64,
            "mem_free_bytes": 8589934592_u64,
            "disk_free_bytes": 107374182400_u64,
            "busy_slots": 0,
            "max_slots": 4,
            "on_battery": null,
            "thermal_state": "nominal"
        }
    });
    store
        .touch_node(&name, now, Some("0.3.0"), Some(AGENT_PROTOCOL), &capabilities)
        .expect("heartbeat");

    let (code, value, stderr) = env.run_json(&["node", "show", "mac-studio"]);
    assert_eq!(code, 0, "node show failed: {stderr}");
    assert_eq!(value["name"], "mac-studio");
    assert_eq!(value["kind"], "agent");
    assert_eq!(value["status"], "active");
    assert_eq!(value["presence"], "online");
    assert_eq!(value["labels"][0], "darwin-arm64");
    assert_eq!(value["mapped"][0], "vowp");
    assert_eq!(value["agent_version"], "0.3.0");
    assert_eq!(value["protocol"], AGENT_PROTOCOL);
    assert_eq!(value["capacity"]["cpu_cores"], 8);
    assert_eq!(value["capacity"]["max_slots"], 4);
    assert!(value["last_seen_at"].is_string());
    let text = serde_json::to_string(&value).expect("json text");
    assert!(!text.contains("/Users/me/private"), "path map leaked: {text}");
    assert!(!text.contains("DEPLOY_TOKEN"), "secret name leaked: {text}");
    assert!(!stderr.contains("/Users/me/private"), "path map leaked: {stderr}");
}

#[test]
fn node_enroll_is_home_only() {
    let home = TempDir::new().expect("temp home");
    let control = TempDir::new().expect("temp control");
    let created_at = Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp");
    let config = ControlConfig {
        role: "control".to_owned(),
        home: "home-1".to_owned(),
        ssh_target: "fake.example".to_owned(),
        remote_home: "/home/user/.vowp".to_owned(),
        created_at,
    };
    std::fs::write(
        control.path().join("control.json"),
        serde_json::to_string(&config).expect("control JSON"),
    )
    .expect("write control.json");

    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["node", "enroll", "mac-studio"])
        .env("HOME", home.path())
        .env("VOWP_CONTROL_HOME", control.path())
        .env_remove("VOWP_HOME")
        .env("NO_COLOR", "1")
        .output()
        .expect("run vowp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(5),
        "a control machine is refused, not routed: {stderr}"
    );
    assert!(stderr.contains("home"), "{stderr}");
    assert!(
        !home.path().join(".vowp/vowp.db").exists(),
        "the control machine must not create a home database"
    );
}

/// One spawned `vowp agent run --foreground` process, killed if the test panics.
struct AgentRunProcess(Child);

impl Drop for AgentRunProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns the real agent daemon with the fake transport and apps on its `PATH`.
fn spawn_agent_run(agent_home: &Path, tools: &Path) -> AgentRunProcess {
    let stderr = std::fs::File::create(agent_home.join("agent.err")).expect("agent stderr file");
    let path = format!("{}:/usr/bin:/bin", tools.display());
    let child = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["agent", "run", "--foreground"])
        .env("HOME", agent_home)
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("VOWP_AGENT_HOME")
        .env_remove("VOWP_HOME")
        .env_remove("VOWP_CONTROL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn agent run");
    AgentRunProcess(child)
}

/// Waits until `vowp agent status --json` reports a connected agent.
fn wait_for_connected_agent(agent_home: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last;
    loop {
        let (code, stdout, stderr) = run_binary(agent_home, &["--json", "agent", "status"]);
        last = format!("exit {code}: {stdout} {stderr}");
        if code == 0
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout)
            && value["connection"] == "connected"
        {
            return;
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(agent_home.join("agent.err")).unwrap_or_default();
            panic!("the agent did not connect: {last}; stderr: {log}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Writes one paired-surface session binding; the orchestrator attaches through it.
fn write_agent_surface(state_dir: &Path, surface: &SurfaceId, session: &SessionId) {
    let dir = AgentConfig::surface_dir(state_dir, surface);
    std::fs::create_dir_all(&dir).expect("surface dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("surface mode");
    let path = AgentConfig::surface_session_path(state_dir, surface);
    std::fs::write(&path, session.as_str()).expect("surface session");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("session mode");
}

/// Writes the fake `opencode` app with executable mode.
fn write_fake_opencode(tools: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(tools).expect("fake tools dir");
    let path = tools.join("opencode");
    std::fs::write(&path, body).expect("fake opencode");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod opencode");
    path
}

#[test]
fn opencode_stdout_readiness_reaches_running_and_a_failed_start_has_a_redacted_last_error() {
    let gateway = GatewayHome::new(&["vowp"]);
    let node = NodeName::parse("mac-studio").expect("node slug");
    let project = ProjectName::parse("vowp").expect("project");
    let agent_home = TempDir::new().expect("agent home");
    let state_dir = agent_home.path().join(".vowp-agent");
    let (_checkout, map) = checkout_map(agent_home.path(), "vowp-checkout", "vowp");
    let key = NodeKey::generate(&state_dir).expect("node key");
    enroll_node_with_public_key(&gateway, &node, &key.public_key_hex(), &project, vec![map.clone()]);
    let config =
        AgentConfig::new("home-1", node.clone(), state_dir.clone(), "home.example").with_path_maps(vec![map.clone()]);
    config.write().expect("agent.json");
    let surface = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAW".to_owned());
    let session = SessionId::new();
    write_agent_surface(&state_dir, &surface, &session);

    let tools = agent_home.path().join("bin");
    let _ssh = fake_gateway_ssh(&tools, gateway.home(), &agent_home.path().join("ssh.log"));
    let success_app = "#!/bin/sh\n\
        printf 'server listening on http://127.0.0.1:46321\\n'\n\
        printf 'server password test-only-orchestrator-password\\n'\n\
        sleep 5\n";
    write_fake_opencode(&tools, success_app);
    let _agent = spawn_agent_run(agent_home.path(), &tools);
    wait_for_connected_agent(agent_home.path());

    let root = gateway.root().display().to_string();
    let start = |name: &str| {
        let args = vec![
            "--json".to_owned(),
            "--home".to_owned(),
            root.clone(),
            "--project".to_owned(),
            "vowp".to_owned(),
            "orchestrator".to_owned(),
            "new".to_owned(),
            "--name".to_owned(),
            name.to_owned(),
            "--app".to_owned(),
            "opencode".to_owned(),
            "--node".to_owned(),
            "mac-studio".to_owned(),
            "--project".to_owned(),
            "vowp".to_owned(),
        ];
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_binary(gateway.home(), &refs)
    };

    let (code, stdout, stderr) = start("mac");
    assert_eq!(code, 0, "the orchestrator did not start: {stdout} {stderr}");
    let card: serde_json::Value = serde_json::from_str(&stdout).expect("orchestrator card JSON");
    assert_eq!(card["state"], "running", "{card}");
    assert_eq!(card["node"], "mac-studio");
    assert!(
        card["last_error"].is_null(),
        "a running orchestrator has no last_error: {card}"
    );

    // The app only printed readiness; the agent materialized the private service file itself.
    let service_path = state_dir.join("orchestrators/mac/.opencode/state/opencode/service.json");
    assert!(
        service_path.is_file(),
        "stdout readiness must materialize {}",
        service_path.display()
    );
    let mode = std::fs::metadata(&service_path)
        .expect("service metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the materialized service.json is private");
    let service: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&service_path).expect("service bytes")).expect("service JSON");
    assert_eq!(service["url"], "http://127.0.0.1:46321");
    assert_eq!(service["port"], 46321);

    // A second start whose app exits before readiness forces a redacted failure card.
    let secret = "FAKE-ORCHESTRATOR-SECRET";
    let failure_app = format!(
        "#!/bin/sh\n\
         printf 'server password {secret} at /srv/owner/private\\n' >&2\n\
         printf 'server listening on http://127.0.0.1:46322\\n'\n\
         sleep 1\n\
         exit 1\n"
    );
    write_fake_opencode(&tools, &failure_app);
    let (code, _stdout, stderr) = start("broken");
    assert_eq!(code, 5, "a failed local start is a refusal: {stderr}");
    let (code, cards, stderr) = run_binary(
        gateway.home(),
        &[
            "--json",
            "--home",
            root.as_str(),
            "--project",
            "vowp",
            "orchestrator",
            "ls",
        ],
    );
    assert_eq!(code, 0, "orchestrator ls failed: {stderr}");
    let cards: serde_json::Value = serde_json::from_str(&cards).expect("orchestrator list JSON");
    let cards = cards.as_array().expect("orchestrator cards");
    let broken = cards
        .iter()
        .find(|card| card["name"] == "broken")
        .expect("the failed orchestrator record exists");
    assert_eq!(broken["state"], "failed", "{broken}");
    let last_error = broken["last_error"]
        .as_str()
        .expect("a failed orchestrator carries a non-null last_error");
    assert!(!last_error.is_empty(), "last_error is empty: {broken}");
    for forbidden in [
        secret,
        "/srv/owner/private",
        map.local_root.as_str(),
        state_dir.to_str().expect("utf-8 state dir"),
        service_path.to_str().expect("utf-8 service path"),
        "46322",
        "127.0.0.1",
    ] {
        assert!(
            !last_error.contains(forbidden),
            "last_error leaked {forbidden:?}: {last_error}"
        );
        assert!(
            !broken.to_string().contains(forbidden),
            "the failed card leaked {forbidden:?}: {broken}"
        );
    }
    let mac = cards
        .iter()
        .find(|card| card["name"] == "mac")
        .expect("the running orchestrator record exists");
    assert_eq!(mac["state"], "running", "{mac}");
    assert!(mac["last_error"].is_null(), "{mac}");

    // Stop the successful host so its fake app does not outlive the test.
    let (code, _stdout, stderr) = run_binary(
        gateway.home(),
        &[
            "--home",
            root.as_str(),
            "--project",
            "vowp",
            "orchestrator",
            "stop",
            "mac",
        ],
    );
    assert_eq!(code, 0, "orchestrator stop failed: {stderr}");
}

/// Creates a temporary home with an active `mac-studio` node and one idle session.
fn seed_pairing_env() -> (TestEnv, NodeName, SessionId) {
    let env = TestEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let node = NodeName::parse("mac-studio").expect("node slug");
    store.insert_pending_node(&node, &[], now).expect("pending node");
    store
        .set_node_status(&node, NodeStatus::Active, "", now)
        .expect("active node");
    let session = SessionId::new();
    store
        .upsert_session(&Session {
            id: session.clone(),
            project: Some(env.config.project.name.clone()),
            job_id: None,
            role: Some(SessionRole::User),
            title: "surface pairing".to_owned(),
            parent_session_id: None,
            node: node.clone(),
            cwd: "/tmp/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        })
        .expect("session");
    (env, node, session)
}

fn pair_surface(env: &TestEnv, session: &SessionId) -> serde_json::Value {
    let (code, value, stderr) = env.run_json(&[
        "surface",
        "pair",
        "--session",
        session.as_ref(),
        "--node",
        "mac-studio",
        "--kind",
        "opencode",
        "--app",
        "opencode",
    ]);
    assert_eq!(code, 0, "surface pair failed: {stderr}");
    value
}

#[test]
fn agent_pair_help_names_code_surface_and_session() {
    let mut io = BufferIo::new("");
    let code = vowp_cli::run(
        vec![
            "vowp".to_owned(),
            "agent".to_owned(),
            "pair".to_owned(),
            "--help".to_owned(),
        ],
        &mut io,
    );
    assert_eq!(code, 0, "help exits 0; stderr: {}", io.stderr());
    let help = io.stdout();
    assert!(help.contains("--code-file"), "help must name --code-file: {help}");
    assert!(help.contains("--surface"), "help must name --surface: {help}");
    assert!(help.contains("--session"), "help must name --session: {help}");
}

#[test]
fn agent_pair_accepts_code_file_stdin() {
    let cli = Cli::try_parse_from([
        "vowp",
        "agent",
        "pair",
        "--code-file",
        "-",
        "--surface",
        "01ARZ3NDEKTSV4RRFFQ69G5FAW",
        "--session",
        "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "--app",
        "opencode",
    ])
    .expect("parse agent pair");
    match cli.command {
        Command::Agent {
            command: AgentCommand::Pair(args),
        } => {
            assert_eq!(args.code_file, "-");
            assert_eq!(args.surface, "01ARZ3NDEKTSV4RRFFQ69G5FAW");
            assert_eq!(args.session, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
            assert_eq!(args.app.as_deref(), Some("opencode"));
        }
        other => panic!("expected agent pair, got {other:?}"),
    }
}

#[test]
fn agent_pair_without_enrollment_exits_three_and_prints_no_code() {
    let home = TempDir::new().expect("temp home");
    let code = "A".repeat(52);
    let code_file = home.path().join("pair.code");
    std::fs::write(&code_file, &code).expect("code file");
    let code_file = code_file.display().to_string();

    let (status, stdout, stderr) = run_binary(
        home.path(),
        &[
            "agent",
            "pair",
            "--code-file",
            &code_file,
            "--surface",
            "01ARZ3NDEKTSV4RRFFQ69G5FAW",
            "--session",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "--app",
            "opencode",
        ],
    );
    assert_eq!(status, 3, "no enrollment is exit 3; stderr: {stderr}");
    assert!(stdout.is_empty(), "the failure prints no document: {stdout}");
    assert!(!stderr.contains(&code), "the code must never be printed: {stderr}");
}

#[test]
fn surface_pair_cli_prints_code_once_and_lists_pending() {
    let (env, node, session) = seed_pairing_env();
    let value = pair_surface(&env, &session);

    let code = value["code"].as_str().expect("code is printed once");
    assert_eq!(code.len(), 52, "the pairing code is 52 characters");
    assert_eq!(value["node"], "mac-studio");
    assert_eq!(value["session"], session.as_str());
    assert_eq!(value["kind"], "opencode");
    assert_eq!(value["app"], "opencode");
    let surface_id = value["surface"].as_str().expect("surface id").to_owned();
    assert_eq!(surface_id.len(), 26);
    let command = value["command"].as_str().expect("Mac command");
    assert!(command.contains("vowp agent pair --code-file -"), "{command}");
    assert!(command.contains("--surface"), "{command}");
    assert!(command.contains("--session"), "{command}");
    assert!(command.contains("--app opencode"), "{command}");
    assert!(
        !command.contains(code),
        "the code is not part of the command: {command}"
    );

    let (status, stdout, stderr) = env.run(&[
        "surface",
        "pair",
        "--session",
        session.as_ref(),
        "--node",
        "mac-studio",
        "--kind",
        "opencode",
        "--app",
        "opencode",
    ]);
    assert_eq!(status, 4, "a duplicate pending pairing is a conflict: {stderr}");
    assert!(stdout.is_empty(), "the conflict prints no document: {stdout}");
    assert!(
        !stderr.contains(code),
        "the code must never be written to stderr: {stderr}"
    );

    // A different kind is a separate surface and prints the human code block exactly once.
    let (status, stdout, stderr) = env.run(&[
        "surface",
        "pair",
        "--session",
        session.as_ref(),
        "--node",
        "mac-studio",
        "--kind",
        "chat",
    ]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(stdout.matches("code:").count(), 1, "the code is printed once: {stdout}");
    assert!(stdout.contains("vowp agent pair --code-file -"), "{stdout}");

    let store = env.store();
    let id = SurfaceId(surface_id.clone());
    let surface = store.surface_by_id(&id).expect("surface lookup").expect("surface row");
    assert_eq!(surface.node, node);
    assert_eq!(surface.state, SurfaceState::Pending);
    assert_eq!(surface.app.as_deref(), Some("opencode"));
    assert_eq!(surface.metadata["session"], session.as_str());
    let database = std::fs::read(env.home_path().join("vowp.db")).expect("database bytes");
    assert!(
        !database.windows(code.len()).any(|window| window == code.as_bytes()),
        "the plaintext code must not be stored in the database"
    );

    let (status, list, stderr) = env.run_json(&["surface", "ls"]);
    assert_eq!(status, 0, "surface ls failed: {stderr}");
    let entries = list.as_array().expect("surface ls returns an array");
    let entry = entries
        .iter()
        .find(|entry| entry["id"] == surface_id)
        .expect("the pending surface is listed");
    assert_eq!(entry["node"], "mac-studio");
    assert_eq!(entry["kind"], "opencode");
    assert_eq!(entry["state"], "pending");
    assert_eq!(entry["expires_at"], serde_json::Value::Null);
    let text = serde_json::to_string(&list).expect("json text");
    assert!(!text.contains(code), "surface ls must never print the code: {text}");
}

#[test]
fn surface_revoke_marks_the_token_revoked() {
    let (env, node, session) = seed_pairing_env();
    let value = pair_surface(&env, &session);
    let code = value["code"].as_str().expect("code");
    let surface_id = SurfaceId(value["surface"].as_str().expect("surface id").to_owned());
    let store = env.store();
    let now = store.clock().now();
    let issued = issue_surface_token(&store, &node, &surface_id, &session, "opencode", code, now).expect("issue token");
    assert!(verify_surface_token(&store, &issued.token, now).is_ok());

    let (status, _stdout, stderr) = env.run(&["surface", "revoke", &surface_id.0]);
    assert_eq!(status, 5, "revoke without --yes is refused: {stderr}");
    assert_eq!(
        store.surface_by_id(&surface_id).expect("lookup").expect("row").state,
        SurfaceState::Attached
    );

    let (status, value, stderr) = env.run_json(&["surface", "revoke", &surface_id.0, "--yes"]);
    assert_eq!(status, 0, "surface revoke failed: {stderr}");
    assert_eq!(value["surface"], surface_id.0);
    assert_eq!(value["status"], "revoked");
    assert_eq!(value["tokens_revoked"], 1);
    assert_eq!(
        store.surface_by_id(&surface_id).expect("lookup").expect("row").state,
        SurfaceState::Revoked
    );
    let tokens = store.list_surface_tokens(&surface_id).expect("tokens");
    assert_eq!(tokens.len(), 1);
    assert!(tokens[0].revoked_at.is_some(), "the token row is revoked");
    assert!(verify_surface_token(&store, &issued.token, now).is_err());

    let (status, _stdout, stderr) = env.run(&["surface", "revoke", "01ARZ3NDEKTSV4RRFFQ69G5FAZ", "--yes"]);
    assert_eq!(status, 3, "an unknown surface is not found: {stderr}");
}

#[test]
fn surface_command_requires_a_node_and_session() {
    let (env, _node, session) = seed_pairing_env();
    let session_text = session.to_string();

    let (status, _stdout, stderr) = env.run(&["surface", "pair", "--session", &session_text, "--kind", "opencode"]);
    assert_eq!(status, 2, "a missing --node is invalid input: {stderr}");
    assert!(stderr.contains("--node"), "{stderr}");

    let (status, _stdout, _stderr) = env.run(&["surface", "pair", "--node", "mac-studio", "--kind", "opencode"]);
    assert_eq!(status, 2, "a missing --session is a usage error");

    let (status, _stdout, stderr) = env.run(&[
        "surface",
        "pair",
        "--session",
        &session_text,
        "--node",
        "missing-node",
        "--kind",
        "opencode",
    ]);
    assert_eq!(status, 3, "an unknown node is not found: {stderr}");

    let (status, _stdout, stderr) = env.run(&[
        "surface",
        "pair",
        "--session",
        "01ARZ3NDEKTSV4RRFFQ69G5FAZ",
        "--node",
        "mac-studio",
        "--kind",
        "opencode",
    ]);
    assert_eq!(status, 3, "an unknown session is not found: {stderr}");

    let (status, _stdout, stderr) = env.run(&[
        "surface",
        "pair",
        "--session",
        &session_text,
        "--node",
        "mac-studio",
        "--kind",
        "unknown",
    ]);
    assert_eq!(status, 2, "an unknown kind is invalid input: {stderr}");
}

#[test]
fn surface_ls_never_prints_a_token() {
    let (env, node, session) = seed_pairing_env();
    let value = pair_surface(&env, &session);
    let code = value["code"].as_str().expect("code");
    let surface_id = SurfaceId(value["surface"].as_str().expect("surface id").to_owned());
    let store = env.store();
    let now = store.clock().now();
    let issued = issue_surface_token(&store, &node, &surface_id, &session, "opencode", code, now).expect("issue token");

    let (status, stdout, stderr) = env.run(&["surface", "ls"]);
    assert_eq!(status, 0, "surface ls failed: {stderr}");
    assert!(stdout.contains(&surface_id.0), "{stdout}");
    assert!(stdout.contains("attached"), "{stdout}");
    assert!(stdout.contains(&format!("session.read:{session}")), "{stdout}");
    assert!(
        !stdout.contains(&issued.token),
        "the token must never be printed: {stdout}"
    );
    assert!(
        !stderr.contains(&issued.token),
        "the token must never reach stderr: {stderr}"
    );

    let (status, value, stderr) = env.run_json(&["surface", "ls"]);
    assert_eq!(status, 0, "surface ls --json failed: {stderr}");
    let entry = value
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["id"] == surface_id.0)
        .expect("the attached surface is listed");
    assert_eq!(entry["state"], "attached");
    assert_eq!(entry["scopes"][0], format!("session.read:{session}"));
    assert!(entry["expires_at"].is_string());
    let text = serde_json::to_string(&value).expect("json text");
    assert!(!text.contains(&issued.token), "the token must never be in JSON: {text}");
    assert!(
        !text.contains("\"token\""),
        "the JSON document has no token field: {text}"
    );

    let (status, value, stderr) = env.run_json(&["surface", "ls", "--node", "mac-studio"]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(value.as_array().expect("array").len(), 1);
    let (status, value, stderr) = env.run_json(&["surface", "ls", "--node", "other-node"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(value.as_array().expect("array").is_empty());
}

#[test]
fn surface_commands_are_home_only() {
    let home = TempDir::new().expect("temp home");
    let control = TempDir::new().expect("temp control");
    let created_at = Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp");
    let config = ControlConfig {
        role: "control".to_owned(),
        home: "home-1".to_owned(),
        ssh_target: "fake.example".to_owned(),
        remote_home: "/home/user/.vowp".to_owned(),
        created_at,
    };
    std::fs::write(
        control.path().join("control.json"),
        serde_json::to_string(&config).expect("control JSON"),
    )
    .expect("write control.json");

    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["surface", "ls"])
        .env("HOME", home.path())
        .env("VOWP_CONTROL_HOME", control.path())
        .env_remove("VOWP_HOME")
        .env("NO_COLOR", "1")
        .output()
        .expect("run vowp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(5),
        "a control machine is refused, not routed: {stderr}"
    );
    assert!(stderr.contains("home"), "{stderr}");
    assert!(
        !home.path().join(".vowp/vowp.db").exists(),
        "the control machine must not create a home database"
    );
}

// ---------------------------------------------------------------------------
// The Mac assignment executor (`CoreRunHandler`)
// ---------------------------------------------------------------------------

/// A fake Claude Code adapter: it answers `--version` and then emits the JSONL body.
const ADAPTER_BODY: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-native","model":"fake-model"}'
printf '%s\n' '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello from the fake adapter"}]},"session_id":"fake-native"}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"finished ok","total_cost_usd":0.01,"usage":{"input_tokens":3,"output_tokens":4}}'
exit "${FAKE_EXIT:-0}"
"#;

/// Runs one git command in `cwd` and returns its stdout; a failure fails the test.
fn git(cwd: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Returns whether one git command exited successfully.
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    ProcessCommand::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

/// One hermetic checkout, bare origin, worktree root, fake adapter and agent state.
struct ExecFixture {
    _home: TempDir,
    root: PathBuf,
    checkout: PathBuf,
    origin: PathBuf,
    worktrees: PathBuf,
    state_dir: PathBuf,
    script: PathBuf,
    base: String,
    config: AgentConfig,
}

impl ExecFixture {
    /// Creates the fixture around one fake adapter body.
    fn new(body: &str) -> Self {
        let home = TempDir::new().expect("temp home");
        let root = std::fs::canonicalize(home.path()).expect("canonical home");
        let origin = root.join("origin.git");
        git(&root, &["init", "-q", "--bare", origin.to_str().expect("utf-8 origin")]);
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).expect("checkout dir");
        git(
            &root,
            &["init", "-q", "-b", "main", checkout.to_str().expect("utf-8 checkout")],
        );
        // The agent accepts a checkout that is group/world readable but not writable; set the
        // shape explicitly so the fixture is independent of the runner's umask.
        std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o755)).expect("checkout mode");
        std::fs::set_permissions(checkout.join(".git"), std::fs::Permissions::from_mode(0o755)).expect("git dir mode");
        git(&checkout, &["config", "user.email", "test@example.invalid"]);
        git(&checkout, &["config", "user.name", "VOWP Test"]);
        std::fs::write(checkout.join("a.txt"), "base\n").expect("base file");

        let script = root.join("fake-claude.sh");
        let text =
            format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '1.2.3\\n'\n  exit 0\nfi\n{body}\n");
        std::fs::write(&script, text).expect("fake adapter");
        let mut permissions = std::fs::metadata(&script).expect("fake adapter stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("fake adapter mode");

        let project_toml = format!(
            "[project]\nname = \"vowp\"\nremote = \"origin\"\n\n\
             [harnesses.claude-code]\nbinary = \"{}\"\n\n\
             [profiles.planner]\nladder = [\"claude-code:fake-model\"]\n\n\
             [profiles.keeper]\nladder = [\"claude-code:fake-model\"]\n\n\
             [profiles.worker]\nladder = [\"claude-code:fake-model\"]\n\n\
             [profiles.verifier]\nladder = [\"claude-code:fake-model\"]\n",
            script.display()
        );
        std::fs::write(checkout.join("vowp.toml"), project_toml).expect("vowp.toml");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-q", "-m", "base"]);
        let base = git(&checkout, &["rev-parse", "HEAD"]).trim().to_owned();
        git(
            &checkout,
            &["remote", "add", "origin", origin.to_str().expect("utf-8 origin")],
        );
        git(&checkout, &["push", "-q", "origin", "main"]);
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let worktrees = root.join("worktrees");
        std::fs::create_dir_all(&worktrees).expect("worktree root");
        std::fs::set_permissions(&worktrees, std::fs::Permissions::from_mode(0o700)).expect("worktree root mode");
        let state_dir = root.join(".vowp-agent");
        std::fs::create_dir_all(&state_dir).expect("agent state dir");
        let project = ProjectName::parse("vowp").expect("project");
        let map = AgentPathMap {
            project,
            local_root: checkout.to_str().expect("utf-8 checkout").to_owned(),
            worktree_root: Some(worktrees.to_str().expect("utf-8 worktrees").to_owned()),
            allow: Vec::new(),
        };
        let config = AgentConfig::new(
            "home-1",
            NodeName::parse("mac-studio").expect("node"),
            state_dir.clone(),
            "home.example",
        )
        .with_path_maps(vec![map]);
        config.write().expect("agent.json");
        Self {
            _home: home,
            root,
            checkout,
            origin,
            worktrees,
            state_dir,
            script,
            base,
            config,
        }
    }

    fn handler(&self, environment: BTreeMap<String, String>) -> CoreRunHandler {
        CoreRunHandler::new(self.config.clone())
            .with_reserve(0)
            .with_env(environment)
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([("FAKE_EXIT".to_owned(), "0".to_owned())])
    }

    fn flow_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("FAKE_EXIT".to_owned(), "0".to_owned()),
            ("VOWP_TEST_ENV".to_owned(), "environment-value-to-hide".to_owned()),
            ("VOWP_TEST_SECRET".to_owned(), "secret-value-to-hide".to_owned()),
        ])
    }

    fn assignment(&self, job: &str) -> AssignmentPayload {
        let project = ProjectName::parse("vowp").expect("project");
        let job = JobId::parse(job).expect("job");
        AssignmentPayload {
            assignment_id: format!("assignment-{job}"),
            run_id: RunId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("run id"),
            session_id: SessionId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("session"),
            project: project.clone(),
            job: job.clone(),
            executor: Executor::parse("claude-code:fake-model").expect("executor"),
            purpose: RunPurpose::Start,
            prompt: "implement the test job".to_owned(),
            cwd_ref: PathRef::Worktree { project, job },
            base_commit: self.base.clone(),
            remote: "origin".to_owned(),
            remote_url: String::new(),
            result_transport: vowp_protocol::ResultTransport::Push,
            env_names: vec!["FAKE_EXIT".to_owned()],
            secret_names: Vec::new(),
            epoch: 1,
            deadline: None,
            spool_cap_bytes: 2 * 1024 * 1024,
        }
    }

    fn worktree(&self, job: &str) -> PathBuf {
        self.worktrees.join(job)
    }

    fn head(&self, cwd: &Path) -> String {
        git(cwd, &["rev-parse", "HEAD"]).trim().to_owned()
    }
}

/// A run sink that records lines, states and the final summary in memory.
#[derive(Default)]
struct RecordingSink {
    events: Vec<RemoteEvent>,
    states: Vec<(RemoteRunState, String)>,
    summary: Option<RunSummary>,
    stopped: bool,
}

impl RunSink for RecordingSink {
    fn line(&mut self, line: RemoteLine) -> Result<(), RunSinkError> {
        if let RemoteLine::Event { event } = line {
            self.events.push(event);
        }
        Ok(())
    }

    fn state(&mut self, state: RemoteRunState, detail: &str) -> Result<(), RunSinkError> {
        self.states.push((state, detail.to_owned()));
        Ok(())
    }

    fn finish(&mut self, summary: RunSummary) -> Result<(), RunSinkError> {
        self.summary = Some(summary);
        Ok(())
    }

    fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn spool_bytes(&self) -> u64 {
        0
    }
}

/// Accepts and runs one assignment synchronously, returning the recording sink.
fn run_assignment(handler: &CoreRunHandler, assignment: &AssignmentPayload) -> RecordingSink {
    let accepted = handler.accept(assignment).expect("assignment is accepted");
    let mut sink = RecordingSink::default();
    handler.run(accepted, &mut sink);
    sink
}

/// A fixed agent clock for the fake gateway tests.
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

#[derive(Default)]
struct GatewayState {
    received: Vec<AgentFrame>,
    summary: Option<TurnSummary>,
}

/// One in-memory gateway: it accepts `AssignRun`, acknowledges every event and stops on
/// the terminal summary.
#[derive(Clone)]
struct FakeGateway {
    shared: Arc<Mutex<GatewayState>>,
    assignment: AssignmentPayload,
}

impl FakeGateway {
    fn new(assignment: AssignmentPayload) -> Self {
        Self {
            shared: Arc::new(Mutex::new(GatewayState::default())),
            assignment,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GatewayState> {
        self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Transport for FakeGateway {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            assignment: self.assignment.clone(),
            outbound: VecDeque::new(),
            seq: 1,
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<GatewayState>>,
    assignment: AssignmentPayload,
    outbound: VecDeque<AgentFrame>,
    seq: u64,
}

impl FakeStream {
    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Timeout),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { .. } => {
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: "nonce-1".to_owned(),
                    home_id: "home-1".to_owned(),
                    connection_id: "connection-1".to_owned(),
                    server_time: ts(),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                self.outbound.push_back(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node"),
                    connection_id: "connection-1".to_owned(),
                });
                self.outbound.push_back(AgentFrame::AssignRun {
                    assignment: self.assignment.clone(),
                    lease_token: "lease-token-for-the-fake-gateway".to_owned(),
                    lease_expires_at: ts().plus(Duration::from_secs(600)),
                });
            }
            AgentFrame::RemoteLine {
                run_id,
                line: RemoteLine::Event { event },
                ..
            } => {
                let seq = self.next_seq();
                self.outbound.push_back(AgentFrame::IngestAck {
                    run_id: run_id.clone(),
                    event_id: Some(event.id.clone()),
                    seq: Some(seq),
                    duplicate: false,
                    lease_revoked: None,
                    error: None,
                });
            }
            AgentFrame::TurnEnd { run_id, summary } => {
                self.shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .summary = Some(summary.clone());
                let seq = self.next_seq();
                self.outbound.push_back(AgentFrame::IngestAck {
                    run_id: run_id.clone(),
                    event_id: None,
                    seq: Some(seq),
                    duplicate: false,
                    lease_revoked: None,
                    error: None,
                });
                self.outbound.push_back(AgentFrame::Revoked {
                    reason: "test complete".to_owned(),
                });
            }
            AgentFrame::Ping => self.outbound.push_back(AgentFrame::Pong),
            _ => {}
        }
        Ok(())
    }
}

#[test]
fn agent_run_creates_a_worktree_and_streams_adapter_events() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    let handler = Arc::new(fixture.handler(fixture.base_env()));
    let assignment = fixture.assignment("agent-exec-stream");
    let gateway = FakeGateway::new(assignment.clone());

    let state_dir = fixture.state_dir.clone();
    let config = AgentConfig::load(&state_dir).expect("agent config");
    let key = NodeKey::load_or_generate(&state_dir).expect("node key");
    let mut client = AgentClient::new(
        state_dir,
        key,
        config,
        Box::new(gateway.clone()),
        Arc::new(FixedClock(ts())),
    );
    let outcome = client
        .run_with_handler(&RunOptions::new(AgentRole::Executor), handler)
        .expect("run loop");
    assert!(
        matches!(outcome, vowp_agent::RunOutcome::Revoked { .. }),
        "the fake gateway revokes after the summary: {outcome:?}"
    );

    let state = gateway.state();
    assert!(
        state
            .received
            .iter()
            .any(|frame| matches!(frame, AgentFrame::RunAccepted { .. })),
        "the gateway accepted AssignRun: {:?}",
        state.received
    );
    let summary = state.summary.as_ref().expect("terminal summary");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert_eq!(summary.exit_code, Some(0));
    assert_eq!(summary.native_session_id.as_deref(), Some("fake-native"));
    assert_eq!(summary.result_text.as_deref(), Some("finished ok"));
    assert_eq!(summary.cost.usd, Some(0.01));
    let mut event_ids = std::collections::BTreeSet::new();
    let mut event_texts = Vec::new();
    for frame in &state.received {
        if let AgentFrame::RemoteLine {
            line: RemoteLine::Event { event },
            ..
        } = frame
        {
            assert!(
                event_ids.insert(event.id.as_str().to_owned()),
                "event ids are unique: {}",
                event.id
            );
            event_texts.push(event.payload.to_string());
        }
    }
    assert!(
        event_texts
            .iter()
            .any(|text| text.contains("hello from the fake adapter")),
        "adapter events reached the gateway: {event_texts:?}"
    );

    let worktree = fixture.worktree("agent-exec-stream");
    assert!(worktree.is_dir(), "the job worktree exists");
    assert_eq!(
        git(&worktree, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "job/agent-exec-stream"
    );
    let head = fixture.head(&worktree);
    assert!(!head.is_empty(), "the worktree has a head commit");
}

#[test]
fn agent_run_pushes_only_the_job_branch_and_reports_the_head() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    let handler = fixture.handler(fixture.base_env());
    let mut assignment = fixture.assignment("agent-exec-push");
    assignment.remote_url = fixture.origin.to_str().expect("utf-8 origin").to_owned();

    let sink = run_assignment(&handler, &assignment);
    let summary = sink.summary.expect("summary");
    let head = fixture.head(&fixture.worktree("agent-exec-push"));
    assert_eq!(summary.head_commit.as_deref(), Some(head.as_str()));
    match summary.push_status {
        Some(PushStatus::Pushed {
            branch,
            remote,
            head_commit,
        }) => {
            assert_eq!(branch, "job/agent-exec-push");
            assert_eq!(remote, "origin");
            assert_eq!(head_commit, head);
        }
        other => panic!("expected a pushed job branch, got {other:?}"),
    }
    assert_eq!(
        git(&fixture.origin, &["rev-parse", "refs/heads/job/agent-exec-push"])
            .trim()
            .to_owned(),
        head
    );
    assert_eq!(
        git(&fixture.origin, &["rev-parse", "refs/heads/main"])
            .trim()
            .to_owned(),
        fixture.base,
        "the default branch is untouched"
    );
    assert!(git(&fixture.origin, &["tag"]).trim().is_empty(), "no tag is pushed");
}

#[test]
fn agent_run_refuses_a_missing_env_name_and_redacts_injected_values() {
    let fixture = ExecFixture::new(
        r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-native","model":"fake-model"}'
printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"values ${VOWP_TEST_ENV} and ${VOWP_TEST_SECRET}\"}]},\"session_id\":\"fake-native\"}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done ${VOWP_TEST_ENV} ${VOWP_TEST_SECRET}\",\"total_cost_usd\":0}"
exit 0
"#,
    );
    let handler = fixture.handler(fixture.flow_env());

    let mut missing = fixture.assignment("agent-exec-missing-env");
    missing.env_names.push("VOWP_TEST_MISSING".to_owned());
    let rejected = handler.accept(&missing).expect_err("a missing name is refused");
    assert_eq!(rejected.reason, RunRejectReason::EnvMissing);
    assert!(
        rejected.detail.contains("VOWP_TEST_MISSING"),
        "the refusal names the missing variable: {}",
        rejected.detail
    );
    assert!(
        !rejected
            .detail
            .contains(fixture.checkout.to_str().expect("utf-8 checkout")),
        "the refusal carries no absolute path: {}",
        rejected.detail
    );

    let mut assignment = fixture.assignment("agent-exec-redact");
    assignment.env_names.push("VOWP_TEST_ENV".to_owned());
    assignment
        .secret_names
        .push(SecretName::parse("VOWP_TEST_SECRET").expect("secret name"));
    let sink = run_assignment(&handler, &assignment);
    let serialized = serde_json::to_string(&sink.events).expect("events JSON");
    assert!(
        !serialized.contains("environment-value-to-hide") && !serialized.contains("secret-value-to-hide"),
        "injected values never reach the sink: {serialized}"
    );
    assert!(serialized.contains("[REDACTED:VOWP_TEST_ENV]"), "{serialized}");
    assert!(serialized.contains("[REDACTED:VOWP_TEST_SECRET]"), "{serialized}");
    let summary = sink.summary.expect("summary");
    let result = summary.result_text.clone().unwrap_or_default();
    assert!(!result.contains("environment-value-to-hide"), "{result}");
    assert!(!result.contains("secret-value-to-hide"), "{result}");
    assert!(result.contains("[REDACTED:VOWP_TEST_ENV]"), "{result}");
    assert!(result.contains("[REDACTED:VOWP_TEST_SECRET]"), "{result}");
}

#[test]
fn agent_run_keeps_the_worktree_on_push_failure() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    let handler = fixture.handler(fixture.base_env());
    let mut assignment = fixture.assignment("agent-exec-push-failure");
    assignment.remote_url = fixture
        .root
        .join("another-origin.git")
        .to_str()
        .expect("utf-8")
        .to_owned();

    let sink = run_assignment(&handler, &assignment);
    let summary = sink.summary.expect("summary");
    match summary.push_status {
        Some(PushStatus::Failed { detail }) => {
            assert!(
                detail.contains("origin") || detail.contains("push"),
                "the typed failure explains the push: {detail}"
            );
        }
        other => panic!("expected a failed push, got {other:?}"),
    }
    assert_eq!(summary.outcome, RunOutcome::Error);
    assert!(
        fixture.worktree("agent-exec-push-failure").is_dir(),
        "a failed push keeps the worktree for inspection"
    );
    assert!(
        git_ok(
            &fixture.checkout,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/job/agent-exec-push-failure"
            ]
        ),
        "the job branch is still local"
    );
}

#[test]
fn agent_run_cleans_up_after_a_terminal_summary() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    let handler = fixture.handler(fixture.base_env());
    let assignment = fixture.assignment("agent-exec-cleanup");
    let run = assignment.run_id.clone();
    let sink = run_assignment(&handler, &assignment);
    assert!(sink.summary.is_some(), "the summary exists before cleanup");
    let worktree = fixture.worktree("agent-exec-cleanup");
    assert!(worktree.is_dir());
    assert!(git_ok(
        &fixture.checkout,
        &["show-ref", "--verify", "--quiet", "refs/heads/job/agent-exec-cleanup"]
    ));

    handler.cleanup(&run);
    assert!(!worktree.exists(), "cleanup removes only the job worktree");
    assert!(
        !git_ok(
            &fixture.checkout,
            &["show-ref", "--verify", "--quiet", "refs/heads/job/agent-exec-cleanup"]
        ),
        "cleanup removes only the job branch"
    );
    assert!(fixture.checkout.is_dir(), "the main checkout stays");
}

#[test]
fn agent_run_cancel_stops_the_adapter() {
    let fixture = ExecFixture::new(
        r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-native","model":"fake-model"}'
printf '%s\n' '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"started"}]},"session_id":"fake-native"}'
printf '%s\n' "$$" > PLACEHOLDER
/bin/sleep 300
"#,
    );
    let started = fixture.root.join("adapter-started");
    let script = std::fs::read_to_string(&fixture.script).expect("fake adapter");
    std::fs::write(
        &fixture.script,
        script.replace("PLACEHOLDER", &started.display().to_string()),
    )
    .expect("rewrite fake adapter");
    let handler = Arc::new(fixture.handler(fixture.base_env()));
    let assignment = fixture.assignment("agent-exec-cancel");
    let run = assignment.run_id.clone();
    let accepted = handler.accept(&assignment).expect("accepted");

    let (sender, receiver) = mpsc::channel();
    let runner = Arc::clone(&handler);
    std::thread::spawn(move || {
        let mut sink = RecordingSink::default();
        runner.run(accepted, &mut sink);
        let _ = sender.send(sink);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(started.exists(), "the fake adapter started");
    handler.cancel(&run);
    let sink = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the cancelled run returns promptly");
    let summary = sink.summary.expect("the cancelled run still reports a summary");
    assert_eq!(summary.result_text.as_deref(), Some("cancelled"));
    assert_eq!(summary.outcome, RunOutcome::Killed);
    assert_eq!(summary.stop_reason, StopReason::Interrupted);
    assert!(
        fixture.worktree("agent-exec-cancel").is_dir(),
        "cancel keeps the worktree"
    );
}

#[test]
fn agent_run_does_not_touch_the_main_checkout() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    std::fs::write(fixture.checkout.join("a.txt"), "dirty local change\n").expect("dirty file");
    std::fs::write(fixture.checkout.join("notes.txt"), "untracked\n").expect("untracked file");
    let before_head = fixture.head(&fixture.checkout);
    let before_status = git(&fixture.checkout, &["status", "--porcelain"]);

    let handler = fixture.handler(fixture.base_env());
    let assignment = fixture.assignment("agent-exec-main");
    let sink = run_assignment(&handler, &assignment);
    assert!(sink.summary.is_some());

    assert_eq!(fixture.head(&fixture.checkout), before_head, "HEAD is unchanged");
    assert_eq!(
        git(&fixture.checkout, &["status", "--porcelain"]),
        before_status,
        "the dirty working tree is unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.checkout.join("a.txt")).expect("dirty file"),
        "dirty local change\n"
    );
    assert!(fixture.worktree("agent-exec-main").is_dir());
    assert!(
        !fixture.worktree("agent-exec-main").starts_with(&fixture.checkout),
        "the worktree lives under the configured worktree root, not in the checkout"
    );
}

#[test]
fn agent_run_rejects_an_unmapped_project() {
    let fixture = ExecFixture::new(ADAPTER_BODY);
    let handler = fixture.handler(fixture.base_env());
    let mut assignment = fixture.assignment("agent-exec-unmapped");
    assignment.project = ProjectName::parse("another-project").expect("project");
    assignment.cwd_ref = PathRef::Project {
        project: assignment.project.clone(),
    };

    let rejected = handler.accept(&assignment).expect_err("an unmapped project is refused");
    assert_eq!(rejected.reason, RunRejectReason::ProjectUnmapped);
    assert!(
        !rejected
            .detail
            .contains(fixture.checkout.to_str().expect("utf-8 checkout")),
        "the refusal carries no absolute path: {}",
        rejected.detail
    );
    assert!(
        !fixture.worktree("agent-exec-unmapped").exists(),
        "a rejected assignment creates no worktree"
    );
}

// ---------------------------------------------------------------------------
// v0.4 CLI surface: WSS enrollment, service, gateway and surface UX
// ---------------------------------------------------------------------------

/// A fake `systemctl` that records every invocation and answers status queries.
#[cfg(target_os = "linux")]
struct FakeManager {
    _dir: TempDir,
    log: PathBuf,
}

#[cfg(target_os = "linux")]
impl FakeManager {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("fake manager dir");
        let log = dir.path().join("manager.log");
        let script = dir.path().join("systemctl");
        let body = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_MANAGER_LOG"
if [ "$1" = "--version" ]; then exit 0; fi
if [ "$2" = "show" ]; then
  case "${FAKE_MANAGER_MODE-}" in
    loaded)
      printf 'LoadState=loaded\nActiveState=active\nMainPID=1234\nExecMainStatus=0\n'
      ;;
    stopped)
      printf 'LoadState=loaded\nActiveState=inactive\nMainPID=0\nExecMainStatus=1\n'
      ;;
    *)
      exit 1
      ;;
  esac
fi
exit 0
"#;
        std::fs::write(&script, body).expect("fake systemctl");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { _dir: dir, log }
    }

    fn path(&self) -> &Path {
        self._dir.path()
    }

    fn envs(&self, mode: &str, xdg: &Path) -> Vec<(&'static str, String)> {
        vec![
            ("PATH", self.path().display().to_string()),
            ("FAKE_MANAGER_LOG", self.log.display().to_string()),
            ("FAKE_MANAGER_MODE", mode.to_owned()),
            ("XDG_CONFIG_HOME", xdg.display().to_string()),
        ]
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// One service unit path under the temporary `$XDG_CONFIG_HOME`.
#[cfg(target_os = "linux")]
fn service_unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user/vowp-agent.service")
}

/// Creates a private agent state directory for the service tests.
#[cfg(target_os = "linux")]
fn private_state_dir(home: &Path) -> PathBuf {
    let state = home.join(".vowp-agent");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).expect("state mode");
    state
}

#[test]
fn agent_enroll_accepts_wss_and_carries_the_url_and_fingerprint() {
    let cli = Cli::try_parse_from([
        "vowp",
        "agent",
        "enroll",
        "--wss",
        "wss://home.example/agent/v1",
        "--code-file",
        "-",
    ])
    .expect("parse agent enroll --wss");
    match cli.command {
        Command::Agent {
            command: AgentCommand::Enroll(args),
        } => {
            assert!(args.target.is_none(), "the target is optional with --wss");
            assert_eq!(args.wss.as_deref(), Some("wss://home.example/agent/v1"));
            assert!(args.tls_fingerprint.is_none());
        }
        other => panic!("expected agent enroll, got {other:?}"),
    }

    let fingerprint = format!("sha256:{}", "a".repeat(64));
    let cli = Cli::try_parse_from([
        "vowp",
        "agent",
        "enroll",
        "--wss",
        "wss://home.example/agent/v1",
        "--tls-fingerprint",
        &fingerprint,
        "--code-file",
        "-",
    ])
    .expect("parse agent enroll --tls-fingerprint");
    match cli.command {
        Command::Agent {
            command: AgentCommand::Enroll(args),
        } => assert_eq!(args.tls_fingerprint.as_deref(), Some(fingerprint.as_str())),
        other => panic!("expected agent enroll, got {other:?}"),
    }

    assert_eq!(enrollment_transport_config(None, None).expect("ssh default"), None);
    assert_eq!(
        enrollment_transport_config(Some("wss://home.example/agent/v1"), None).expect("system mode"),
        Some(TransportConfig::Wss {
            url: "wss://home.example/agent/v1".to_owned(),
            tls: WssTlsMode::System,
            fingerprint: None,
        })
    );
    assert_eq!(
        enrollment_transport_config(Some("wss://home.example/agent/v1"), Some(&fingerprint)).expect("pin mode"),
        Some(TransportConfig::Wss {
            url: "wss://home.example/agent/v1".to_owned(),
            tls: WssTlsMode::Pin,
            fingerprint: Some(fingerprint),
        })
    );
    assert!(enrollment_transport_config(Some("http://home.example/agent/v1"), None).is_err());
    assert!(enrollment_transport_config(Some("wss://home.example/agent/v1"), Some("sha256:short")).is_err());
}

#[test]
fn agent_enroll_without_target_or_wss_exits_two() {
    let home = TempDir::new().expect("temp home");
    let (code, _stdout, stderr) = run_binary(home.path(), &["agent", "enroll", "--code-file", "-"]);
    assert_eq!(code, 2, "a missing transport is invalid input: {stderr}");
    assert!(stderr.contains("--target") || stderr.contains("--wss"), "{stderr}");
}

/// Returns a syntactically valid `sha256:<64 lowercase hex>` fingerprint.
fn wss_fingerprint(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}

/// Writes `agent.json` with a WSS transport under a private temporary state directory.
fn write_wss_agent_config(state_dir: &Path, tls: WssTlsMode, fingerprint: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(state_dir).expect("state dir");
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).expect("state mode");
    let config = AgentConfig {
        home: "home-1".to_owned(),
        node: NodeName::parse("mac-studio").expect("node slug"),
        state_dir: state_dir.to_path_buf(),
        transport: TransportConfig::Wss {
            url: "wss://home.example/agent/v1".to_owned(),
            tls,
            fingerprint: fingerprint.map(str::to_owned),
        },
        path_maps: Vec::new(),
    };
    config.write().expect("agent.json");
    AgentConfig::file_path(state_dir)
}

/// Runs one agent command against a private `VOWP_AGENT_HOME`, never the real one.
fn run_agent_command(home: &Path, state_dir: &Path, args: &[&str]) -> (i32, String, String) {
    run_binary_with(home, args, &[("VOWP_AGENT_HOME", state_dir.display().to_string())])
}

/// Writes fake key and surface material that no `agent trust` output may print.
fn write_fake_agent_secrets(state_dir: &Path) -> (String, String) {
    let key = "FAKE-NODE-KEY-MATERIAL-DO-NOT-PRINT".to_owned();
    let token = "vowp_surf_FAKE_TOKEN_DO_NOT_PRINT".to_owned();
    std::fs::write(state_dir.join("node.key"), &key).expect("fake node key");
    let surface_dir = state_dir.join("surfaces/01ARZ3NDEKTSV4RRFFQ69G5FAW");
    std::fs::create_dir_all(&surface_dir).expect("fake surface dir");
    std::fs::write(surface_dir.join("token"), &token).expect("fake surface token");
    (key, token)
}

#[test]
fn agent_trust_updates_a_changed_system_fingerprint() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let old = wss_fingerprint('a');
    let new = wss_fingerprint('b');
    let path = write_wss_agent_config(&state_dir, WssTlsMode::System, Some(&old));
    let (key, token) = write_fake_agent_secrets(&state_dir);

    let (code, stdout, stderr) = run_agent_command(home.path(), &state_dir, &["agent", "trust", "--fingerprint", &new]);
    assert_eq!(code, 0, "trust failed: {stdout} {stderr}");
    assert!(stdout.contains(&old), "the old fingerprint is named: {stdout}");
    assert!(stdout.contains(&new), "the new fingerprint is named: {stdout}");
    assert!(stdout.contains("recorded"), "the write is confirmed: {stdout}");
    assert!(
        !stdout.contains(&key) && !stderr.contains(&key),
        "the node key must never be printed"
    );
    assert!(
        !stdout.contains(&token) && !stderr.contains(&token),
        "the surface token must never be printed"
    );
    assert!(
        !stdout.contains(state_dir.to_str().expect("utf-8 state dir")),
        "the command prints no state path: {stdout}"
    );

    let config = AgentConfig::load(&state_dir).expect("reload agent.json");
    match &config.transport {
        TransportConfig::Wss { tls, fingerprint, .. } => {
            assert_eq!(*tls, WssTlsMode::System);
            assert_eq!(fingerprint.as_deref(), Some(new.as_str()));
        }
        other => panic!("expected a wss transport, got {other:?}"),
    }
    let mode = std::fs::metadata(&path).expect("agent.json").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "agent.json stays private");
}

#[test]
fn agent_trust_refuses_a_pin_transport() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let old = wss_fingerprint('a');
    let new = wss_fingerprint('b');
    let path = write_wss_agent_config(&state_dir, WssTlsMode::Pin, Some(&old));
    let before = std::fs::read(&path).expect("agent.json");

    let (code, stdout, stderr) = run_agent_command(home.path(), &state_dir, &["agent", "trust", "--fingerprint", &new]);
    assert_eq!(code, 5, "a pin is a hard pin: {stdout} {stderr}");
    assert!(stdout.is_empty(), "a refusal prints no document: {stdout}");
    assert!(
        stderr.contains("re-enroll"),
        "the refusal names re-enrollment: {stderr}"
    );
    assert!(
        stderr.contains("--tls-fingerprint"),
        "the refusal names the enrollment flag: {stderr}"
    );
    assert_eq!(std::fs::read(&path).expect("agent.json"), before, "nothing is written");
}

#[test]
fn agent_trust_refuses_a_malformed_fingerprint() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let old = wss_fingerprint('a');
    let path = write_wss_agent_config(&state_dir, WssTlsMode::System, Some(&old));
    let before = std::fs::read(&path).expect("agent.json");

    for malformed in [
        format!("sha256:{}", "A".repeat(64)),
        "sha256:abc".to_owned(),
        "a".repeat(64),
    ] {
        let (code, stdout, stderr) = run_agent_command(
            home.path(),
            &state_dir,
            &["agent", "trust", "--fingerprint", &malformed],
        );
        assert_eq!(code, 5, "{malformed} must be refused: {stdout} {stderr}");
        assert!(stdout.is_empty(), "a refusal prints no document: {stdout}");
        assert!(
            stderr.contains("sha256:<64 lowercase hex>"),
            "the refusal names the accepted shape: {stderr}"
        );
        assert_eq!(
            std::fs::read(&path).expect("agent.json"),
            before,
            "{malformed} must not write"
        );
    }
}

#[test]
fn agent_trust_is_idempotent_for_the_same_fingerprint() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let fingerprint = wss_fingerprint('a');
    let path = write_wss_agent_config(&state_dir, WssTlsMode::System, Some(&fingerprint));
    let before = std::fs::read(&path).expect("agent.json");

    let (code, stdout, stderr) = run_agent_command(
        home.path(),
        &state_dir,
        &["agent", "trust", "--fingerprint", &fingerprint],
    );
    assert_eq!(code, 0, "the same value succeeds: {stdout} {stderr}");
    assert!(
        stdout.lines().any(|line| line.trim() == "fingerprint unchanged"),
        "the unchanged value is named: {stdout}"
    );
    assert_eq!(
        std::fs::read(&path).expect("agent.json"),
        before,
        "the file is not rewritten"
    );
}

#[test]
fn agent_trust_records_a_missing_system_fingerprint_explicitly() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let fingerprint = wss_fingerprint('c');
    let path = write_wss_agent_config(&state_dir, WssTlsMode::System, None);

    let (code, stdout, stderr) = run_agent_command(
        home.path(),
        &state_dir,
        &["agent", "trust", "--fingerprint", &fingerprint],
    );
    assert_eq!(code, 0, "a first record succeeds: {stdout} {stderr}");
    assert!(stdout.contains("old fingerprint: none"), "{stdout}");
    assert!(stdout.contains(&fingerprint), "{stdout}");
    assert!(
        stdout.contains("first record"),
        "the explicit first record is named: {stdout}"
    );
    assert!(
        stdout.contains("not a silent trust reset"),
        "the write is not a silent reset: {stdout}"
    );

    let config = AgentConfig::load(&state_dir).expect("reload agent.json");
    match &config.transport {
        TransportConfig::Wss {
            tls,
            fingerprint: stored,
            ..
        } => {
            assert_eq!(*tls, WssTlsMode::System);
            assert_eq!(stored.as_deref(), Some(fingerprint.as_str()));
        }
        other => panic!("expected a wss transport, got {other:?}"),
    }
    let mode = std::fs::metadata(&path).expect("agent.json").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "agent.json stays private");
}

#[test]
fn agent_trust_json_document_is_path_free_and_secret_free() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let old = wss_fingerprint('a');
    let new = wss_fingerprint('b');
    write_wss_agent_config(&state_dir, WssTlsMode::System, Some(&old));
    let (key, token) = write_fake_agent_secrets(&state_dir);

    let (code, stdout, stderr) = run_agent_command(
        home.path(),
        &state_dir,
        &["--json", "agent", "trust", "--fingerprint", &new],
    );
    assert_eq!(code, 0, "json trust failed: {stdout} {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("trust JSON");
    let object = value.as_object().expect("trust JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["changed", "new", "old", "tls", "trusted"],
        "the document has exactly the documented fields"
    );
    assert_eq!(value["trusted"], true);
    assert_eq!(value["changed"], true);
    assert_eq!(value["old"], old);
    assert_eq!(value["new"], new);
    assert_eq!(value["tls"], "system");
    assert!(!stdout.contains(&key), "the node key is absent: {stdout}");
    assert!(!stdout.contains(&token), "the surface token is absent: {stdout}");
    assert!(
        !stdout.contains(state_dir.to_str().expect("utf-8 state dir")),
        "the state path is absent: {stdout}"
    );
}

#[test]
fn agent_trust_refuses_an_ssh_transport() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    let node = NodeName::parse("mac-studio").expect("node slug");
    let config = AgentConfig::new("home-1", node, state_dir.clone(), "home.example");
    config.write().expect("agent.json");
    let path = AgentConfig::file_path(&state_dir);
    let before = std::fs::read(&path).expect("agent.json");

    let (code, _stdout, stderr) = run_agent_command(
        home.path(),
        &state_dir,
        &["agent", "trust", "--fingerprint", &wss_fingerprint('a')],
    );
    assert_eq!(code, 5, "ssh is refused: {stderr}");
    assert!(
        stderr.contains("agent transport is ssh; trust applies to wss"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&path).expect("agent.json"), before, "nothing is written");
}

#[test]
fn agent_trust_without_enrollment_does_not_create_a_file() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");

    let (code, _stdout, stderr) = run_agent_command(
        home.path(),
        &state_dir,
        &["agent", "trust", "--fingerprint", &wss_fingerprint('a')],
    );
    assert_eq!(code, 5, "a missing enrollment is refused: {stderr}");
    assert!(stderr.contains("not enrolled"), "{stderr}");
    assert!(
        !AgentConfig::file_path(&state_dir).exists(),
        "the command must not create agent.json"
    );
}

#[test]
fn agent_gateway_refuses_a_non_loopback_bind() {
    let home = TempDir::new().expect("temp home");
    let (code, _stdout, stderr) = run_binary(home.path(), &["agent", "gateway", "--listen", "0.0.0.0:0"]);
    assert_eq!(code, 5, "a public bind is refused: {stderr}");
    assert!(stderr.contains("loopback"), "{stderr}");
}

#[test]
fn agent_gateway_stdio_refuses_an_uninitialised_home() {
    // A home that has never run `vowp init` has no store and no `~/.vowp` state.
    let home = TempDir::new().expect("temp home");
    let (code, stdout, stderr) = run_binary(home.path(), &["agent", "gateway", "--stdio"]);

    assert_eq!(code, 3, "an uninitialised home is not found: {stderr}");
    assert!(stdout.is_empty(), "a refusal writes no agent frame: {stdout}");
    assert!(
        stderr.contains("vowp home is not initialised"),
        "the refusal must say the home is not initialised: {stderr}"
    );
    assert!(
        stderr.contains("vowp init"),
        "the refusal must name the command that fixes it: {stderr}"
    );
    assert!(
        !stderr.contains("no project selected"),
        "the refusal must not look like a project-selection failure: {stderr}"
    );
}

#[test]
fn agent_gateway_stdio_enrolls_and_handshakes_on_a_multi_project_home() {
    let home = GatewayHome::new(&["alpha", "beta"]);
    let node = NodeName::parse("mac-studio").expect("node slug");
    let key = SigningKey::from_bytes(&[21u8; 32]);
    let alpha = ProjectName::parse("alpha").expect("alpha project");

    // The SSH forced command runs from `$HOME` with no `--project`: no checkout is the cwd.
    enroll_node(&home, &node, &key, &alpha);
    let (mut child, mut stdin, mut stdout) = handshake_node(&home, &node, &key, &alpha);
    write_agent_frame(
        &mut stdin,
        &AgentFrame::Heartbeat {
            at: Timestamp::now(),
            capacity: gateway_capacity(),
            pending_spool_bytes: 0,
        },
    );
    write_agent_frame(&mut stdin, &AgentFrame::Ping);
    match read_agent_frame(&mut stdout) {
        AgentFrame::Pong => {}
        other => panic!("expected Pong, got {other:?}"),
    }

    let checkout = home.checkout("alpha");
    let (code, stdout_text, stderr) = run_binary(
        home.home(),
        &[
            "--json",
            "--project",
            checkout.to_str().expect("utf-8 checkout"),
            "node",
            "show",
            "mac-studio",
        ],
    );
    assert_eq!(code, 0, "node show failed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout_text).expect("node show JSON");
    assert_eq!(value["status"], "active", "{value}");
    assert_eq!(value["presence"], "online", "{value}");

    drop(stdin);
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "EOF exits 0");
    let stderr = child_stderr(&mut child);
    assert!(!stderr.contains("no project selected"), "{stderr}");
    assert!(stderr.is_empty(), "a clean EOF writes no stderr: {stderr}");
}

#[test]
fn agent_gateway_stdio_exits_zero_on_eof_for_zero_single_and_multi_project_homes() {
    for projects in [&[][..], &["alpha"][..], &["alpha", "beta"][..]] {
        let home = GatewayHome::new(projects);
        let mut child = spawn_gateway_stdio(home.home(), home.home());
        drop(child.stdin.take());
        let status = wait_for_exit(&mut child, Duration::from_secs(10));
        assert_eq!(status.code(), Some(0), "projects {projects:?} must serve EOF");
        let stderr = child_stderr(&mut child);
        assert!(stderr.is_empty(), "projects {projects:?}: {stderr}");
    }
}

#[test]
fn agent_gateway_stdio_refuses_an_unknown_node_with_a_protocol_frame() {
    let home = GatewayHome::new(&["alpha"]);
    let node = NodeName::parse("ghost").expect("node slug");
    let key = SigningKey::from_bytes(&[22u8; 32]);
    let alpha = ProjectName::parse("alpha").expect("alpha project");
    let mut child = spawn_gateway_stdio(home.home(), home.home());
    let mut stdin = child.stdin.take().expect("gateway stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("gateway stdout"));

    write_agent_frame(
        &mut stdin,
        &AgentFrame::AgentHello {
            node: node.clone(),
            key_id: public_hex(&key),
            agent_version: "0.3.0".to_owned(),
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            labels: vec!["linux".to_owned()],
            capabilities: gateway_capabilities(&alpha),
        },
    );
    let AgentFrame::Error { kind, message, .. } = read_agent_frame(&mut stdout) else {
        panic!("expected an Error frame");
    };
    assert_eq!(kind, ErrorKind::NotFound);
    assert!(message.contains("unknown node"), "{message}");
    assert!(!message.contains("no project selected"), "{message}");

    drop(stdin);
    let status = wait_for_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "the refused connection closes cleanly");
}

#[test]
fn agent_enroll_ssh_argv_is_unchanged() {
    let argv = SshTransport::new("home.example").argv();
    assert_eq!(
        argv.last().map(String::as_str),
        Some("vowp agent gateway --stdio"),
        "the client keeps its hard-coded SSH command: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg == "--project"),
        "the SSH command never needs a project: {argv:?}"
    );
}

/// One spawned `agent gateway --listen` process, killed if the test panics.
struct GatewayProcess(Child);

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Sends one HTTP upgrade request and returns the response head and body.
fn http_upgrade(port: u16, path: &str, protocol: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the gateway");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {protocol}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

#[test]
fn agent_gateway_listen_refuses_other_routes_and_subprotocols() {
    let env = TestEnv::new();
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .arg("--home")
        .arg(env.home_path())
        .args(["agent", "gateway", "--listen", "127.0.0.1:0"])
        .env("HOME", env.home_path())
        .env("NO_COLOR", "1")
        .env_remove("VOWP_AGENT_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent gateway");
    let stdout = child.stdout.take().expect("gateway stdout");
    let mut guard = GatewayProcess(child);
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read gateway line");
    assert!(
        line.contains("ws://127.0.0.1:") && line.contains("/agent/v1"),
        "the gateway prints its bound address: {line:?}"
    );
    let address = line
        .split("ws://")
        .nth(1)
        .expect("bound address")
        .split('/')
        .next()
        .expect("host:port");
    let port: u16 = address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .expect("gateway port");

    let wrong_path = http_upgrade(port, "/other", "vowp-agent/1");
    assert!(
        wrong_path.starts_with("HTTP/1.1 404"),
        "only /agent/v1 is served: {wrong_path:?}"
    );
    let wrong_protocol = http_upgrade(port, "/agent/v1", "other/1");
    assert!(
        wrong_protocol.starts_with("HTTP/1.1 400"),
        "the vowp-agent/1 subprotocol is required: {wrong_protocol:?}"
    );

    let _ = guard.0.kill();
    let _ = guard.0.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn agent_service_install_print_writes_nothing_and_names_the_manager() {
    let home = TempDir::new().expect("temp home");
    private_state_dir(home.path());
    let manager = FakeManager::new();
    let xdg = home.path().join(".config");
    let envs = manager.envs("loaded", &xdg);
    let (code, stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install", "--print"], &envs);
    assert_eq!(code, 0, "service install --print failed: {stderr}");
    assert!(stdout.contains("[Service]"), "{stdout}");
    assert!(stdout.contains("ExecStart="), "{stdout}");
    assert!(stdout.contains("systemctl --user daemon-reload"), "{stdout}");
    assert!(
        stdout.contains("systemctl --user enable --now vowp-agent.service"),
        "{stdout}"
    );
    assert!(!service_unit_path(home.path()).exists(), "--print writes nothing");
    assert!(
        manager.log().is_empty(),
        "--print invokes no manager: {}",
        manager.log()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn agent_service_install_writes_one_unit_and_only_now_runs_the_manager() {
    let home = TempDir::new().expect("temp home");
    private_state_dir(home.path());
    let manager = FakeManager::new();
    let xdg = home.path().join(".config");
    let envs = manager.envs("loaded", &xdg);

    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install"], &envs);
    assert_eq!(code, 0, "service install failed: {stderr}");
    let unit = service_unit_path(home.path());
    assert!(unit.is_file(), "the unit is written at {}", unit.display());
    let mode = std::fs::metadata(&unit).expect("unit metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the unit is private");
    let text = std::fs::read_to_string(&unit).expect("unit text");
    assert!(text.contains("generated by vowp agent service install"), "{text}");
    assert!(manager.log().is_empty(), "install without --now invokes no manager");

    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install", "--now"], &envs);
    assert_eq!(code, 0, "service install --now failed: {stderr}");
    let log = manager.log();
    assert!(log.contains("--user daemon-reload"), "{log}");
    assert!(log.contains("--user enable --now vowp-agent.service"), "{log}");

    std::fs::write(&unit, "[Service]\nDescription=someone else\n").expect("foreign unit");
    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install"], &envs);
    assert_eq!(code, 4, "a foreign unit is refused without --force: {stderr}");
    assert_eq!(
        std::fs::read_to_string(&unit).expect("foreign unit text"),
        "[Service]\nDescription=someone else\n",
        "the foreign file is not replaced"
    );
    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install", "--force"], &envs);
    assert_eq!(code, 0, "install --force replaces the foreign unit: {stderr}");
    assert!(
        std::fs::read_to_string(&unit)
            .expect("forced unit text")
            .contains("generated by vowp agent service install"),
        "the forced unit is VOWP's own text"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn agent_service_uninstall_removes_only_a_generated_unit_and_keeps_the_state() {
    let home = TempDir::new().expect("temp home");
    let state = private_state_dir(home.path());
    let token_dir = state.join("surfaces/01ARZ3NDEKTSV4RRFFQ69G5FAW");
    std::fs::create_dir_all(&token_dir).expect("surface dir");
    std::fs::write(token_dir.join("token"), "kept-token-placeholder").expect("token");
    let manager = FakeManager::new();
    let xdg = home.path().join(".config");
    let envs = manager.envs("loaded", &xdg);
    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install"], &envs);
    assert_eq!(code, 0, "install failed: {stderr}");
    let unit = service_unit_path(home.path());
    assert!(unit.is_file());

    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "uninstall"], &envs);
    assert_eq!(code, 5, "uninstall without --yes is refused: {stderr}");
    assert!(unit.is_file(), "the refused uninstall keeps the unit");

    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "uninstall", "--yes"], &envs);
    assert_eq!(code, 0, "uninstall failed: {stderr}");
    assert!(!unit.exists(), "the generated unit is removed");
    let log = manager.log();
    assert!(log.contains("--user disable --now vowp-agent.service"), "{log}");
    assert!(log.contains("--user daemon-reload"), "{log}");
    assert_eq!(
        std::fs::read_to_string(token_dir.join("token")).expect("token kept"),
        "kept-token-placeholder",
        "uninstall keeps every token"
    );

    std::fs::write(&unit, "[Unit]\nDescription=someone else\n").expect("foreign unit");
    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "uninstall", "--yes"], &envs);
    assert_eq!(code, 4, "a foreign unit is not removed: {stderr}");
    assert!(unit.is_file(), "the foreign file stays");
}

#[cfg(target_os = "linux")]
#[test]
fn agent_service_status_reads_the_manager_and_reports_unavailable_json() {
    let home = TempDir::new().expect("temp home");
    private_state_dir(home.path());
    let manager = FakeManager::new();
    let xdg = home.path().join(".config");
    let loaded = manager.envs("loaded", &xdg);
    let (code, _stdout, stderr) = run_binary_with(home.path(), &["agent", "service", "install"], &loaded);
    assert_eq!(code, 0, "install failed: {stderr}");

    let (code, value, stderr) = run_binary_with(home.path(), &["--json", "agent", "service", "status"], &loaded);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&value).expect("status JSON");
    assert_eq!(value["available"], true);
    assert_eq!(value["unit"], "vowp-agent.service");
    assert_eq!(value["state"], "loaded");
    assert_eq!(value["loaded"], true);
    assert_eq!(value["pid"], 1234);
    assert_eq!(value["last_exit"], 0);
    assert!(value.get("unit_path").is_none(), "JSON stays path-free: {value}");

    let stopped = manager.envs("stopped", &xdg);
    let (code, human, stderr) = run_binary_with(home.path(), &["agent", "service", "status"], &stopped);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("unit:"), "{human}");
    assert!(
        human.contains("loaded: yes"),
        "LoadState=loaded is the manager's loaded state: {human}"
    );
    assert!(human.contains("pid: -"), "{human}");
    assert!(human.contains("last exit: 1"), "{human}");

    let empty = tempfile::tempdir().expect("empty PATH");
    let unavailable = vec![
        ("PATH", empty.path().display().to_string()),
        ("XDG_CONFIG_HOME", xdg.display().to_string()),
    ];
    let (code, value, stderr) = run_binary_with(home.path(), &["--json", "agent", "service", "status"], &unavailable);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&value).expect("unavailable JSON");
    assert_eq!(value["available"], false);
    assert_eq!(value["state"], "unavailable");
}

#[test]
fn agent_service_unsupported_platform_is_exit_five() {
    let code = service_error_exit_code(&ServiceError::UnsupportedPlatform);
    assert_eq!(code, 5);
    assert!(
        ServiceError::UnsupportedPlatform
            .to_string()
            .contains("Linux and macOS")
    );
}

#[test]
fn agent_run_service_without_home_exits_two() {
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["agent", "run", "--service"])
        .env_remove("HOME")
        .env_remove("VOWP_AGENT_HOME")
        .env("NO_COLOR", "1")
        .output()
        .expect("run vowp agent run --service");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn agent_run_service_refuses_a_loose_state_dir_with_exit_five() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    enrolled_agent_config(home.path());
    NodeKey::load_or_generate(&state_dir).expect("node key");
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).expect("loose mode");

    let (code, _stdout, stderr) = run_binary(home.path(), &["agent", "run", "--service"]);
    assert_eq!(code, 5, "a loose state directory is a refusal: {stderr}");
    assert!(stderr.contains("mode 755"), "{stderr}");
    assert!(stderr.contains("must be 700"), "{stderr}");
    assert!(
        !stderr.contains("config:"),
        "an isolation refusal is not a config error: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn agent_run_service_writes_the_run_marker_and_drains_on_sigterm() {
    let home = TempDir::new().expect("temp home");
    let state_dir = home.path().join(".vowp-agent");
    enrolled_agent_config(home.path());
    NodeKey::load_or_generate(&state_dir).expect("node key");

    let tools = tempfile::tempdir().expect("fake tools");
    let ssh = tools.path().join("ssh");
    let log = tools.path().join("ssh.log");
    std::fs::write(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_SSH_LOG\"\nexit 1\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod ssh");

    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["agent", "run", "--service", "--foreground"])
        .env("HOME", home.path())
        .env("PATH", tools.path())
        .env("FAKE_SSH_LOG", &log)
        .env("NO_COLOR", "1")
        .env_remove("VOWP_AGENT_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agent run --service");
    let marker = state_dir.join("run");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.is_file(), "the service run writes the run marker");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(log.is_file(), "the service run opens the configured transport");

    let status = ProcessCommand::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        if let Some(status) = child.try_wait().expect("poll agent") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the service run did not drain within 15 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(exit.code(), Some(0), "a drained service run exits 0");
    assert!(marker.is_file(), "a drain keeps the marker for the next start");
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(log.contains("vowp agent gateway --stdio"), "{log}");
}

/// Writes one local surface directory with a token, expiry and state.
fn write_local_surface(state_dir: &Path, id: &str, token: &str, expiry: Timestamp, state: &str) {
    let dir = state_dir.join("surfaces").join(id);
    std::fs::create_dir_all(&dir).expect("surface dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("surface mode");
    if !token.is_empty() {
        let path = dir.join("token");
        std::fs::write(&path, token).expect("token");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("token mode");
    }
    let path = dir.join("expires_at");
    std::fs::write(&path, expiry.as_rfc3339()).expect("expiry");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("expiry mode");
    let path = dir.join("state");
    std::fs::write(&path, state).expect("state");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("state mode");
}

#[test]
fn agent_status_adds_the_surfaces_array_and_warns_within_seven_days() {
    let home = TempDir::new().expect("temp home");
    enrolled_agent_config(home.path());
    let state_dir = home.path().join(".vowp-agent");
    let now = Timestamp::now();
    let early = now.plus(Duration::from_secs(3 * 24 * 60 * 60));
    let late = now.plus(Duration::from_secs(20 * 24 * 60 * 60));
    let early_token = "AAAAAAAAAAAA-local-fixture".to_owned();
    let late_token = "BBBBBBBBBBBB-local-fixture".to_owned();
    write_local_surface(&state_dir, "01ARZ3NDEKTSV4RRFFQ69G5FAW", &early_token, early, "paired");
    write_local_surface(&state_dir, "01ARZ3NDEKTSV4RRFFQ69G5FAX", &late_token, late, "paired");
    let dead_dir = state_dir.join("surfaces/01ARZ3NDEKTSV4RRFFQ69G5FAY");
    std::fs::create_dir_all(&dead_dir).expect("needs-pair dir");
    std::fs::set_permissions(&dead_dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    std::fs::write(dead_dir.join("state"), "needs_pair").expect("state");

    let (code, stdout, stderr) = run_binary(home.path(), &["--json", "agent", "status"]);
    assert_eq!(code, 0, "status failed: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("status JSON");
    let surfaces = value["surfaces"].as_array().expect("surfaces array");
    assert_eq!(surfaces.len(), 3, "{value}");
    let first = surfaces
        .iter()
        .find(|row| row["surface"] == "01ARZ3NDEKTSV4RRFFQ69G5FAW")
        .expect("early surface");
    assert_eq!(first["prefix"], "AAAAAAAAAAAA");
    assert_eq!(first["state"], "paired");
    assert_eq!(first["expires_at"], early.as_rfc3339());
    let needs_pair = surfaces
        .iter()
        .find(|row| row["surface"] == "01ARZ3NDEKTSV4RRFFQ69G5FAY")
        .expect("needs-pair surface");
    assert_eq!(needs_pair["state"], "needs_pair");
    assert_eq!(
        value["surface_token_expires_at"],
        early.as_rfc3339(),
        "the earliest active expiry stays the compatibility field"
    );
    assert_eq!(stderr.matches("warning:").count(), 1, "{stderr}");
    assert!(stderr.contains("7 days"), "{stderr}");
    for (label, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(!text.contains(&early_token), "{label} carries no token: {text}");
        assert!(!text.contains(&late_token), "{label} carries no token: {text}");
    }

    let (code, human, stderr) = run_binary(home.path(), &["agent", "status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("surfaces:"), "{human}");
    assert!(human.contains("01ARZ3NDEKTSV4RRFFQ69G5FAW"), "{human}");
}

/// Creates one attached surface and returns its node, id and stored token.
fn attached_surface(env: &TestEnv, session: &SessionId) -> (NodeName, SurfaceId, String) {
    let value = pair_surface(env, session);
    let code = value["code"].as_str().expect("pairing code");
    let id = SurfaceId(value["surface"].as_str().expect("surface id").to_owned());
    let node = NodeName::parse("mac-studio").expect("node slug");
    let store = env.store();
    let now = store.clock().now();
    let issued = issue_surface_token(&store, &node, &id, session, "opencode", code, now).expect("issue token");
    (node, id, issued.token)
}

#[test]
fn surface_rotate_wait_returns_when_the_frame_is_drained() {
    let (env, node, session) = seed_pairing_env();
    let (_node, surface, _token) = attached_surface(&env, &session);
    let home = env.home_path().to_path_buf();
    let request = surface.0.clone();
    let drainer = std::thread::spawn(move || {
        let spool = vowp_core::agent::AgentFrameSpool::new(&home);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if spool.read(&node, &request).expect("read queued frame").is_some() {
                spool.complete(&node, &request).expect("drain queued frame");
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    });
    let (code, value, stderr) = env.run_json(&["surface", "rotate", &surface.0, "--wait", "3s"]);
    assert!(
        drainer.join().expect("drainer thread"),
        "the drainer saw the queued frame"
    );
    assert_eq!(code, 0, "surface rotate --wait failed: {stderr}");
    assert_eq!(value["pending"], false, "{value}");
    assert!(value["prefix"].is_string(), "{value}");
    assert!(
        !stderr.contains("rotation_pending"),
        "a drained rotation is not pending: {stderr}"
    );
}

#[test]
fn surface_rotate_wait_reports_rotation_pending_with_exit_seven() {
    let (env, _node, session) = seed_pairing_env();
    let (_node, surface, _token) = attached_surface(&env, &session);
    let (code, stdout, stderr) = env.run(&["surface", "rotate", &surface.0, "--wait", "0s"]);
    assert_eq!(code, 7, "an undrained rotation is exit 7: {stdout} {stderr}");
    assert!(stderr.contains("rotation_pending"), "{stderr}");
    let frame = env
        .home_path()
        .join("agent-frames/mac-studio")
        .join(format!("{}.json", surface.0));
    assert!(frame.is_file(), "the queued frame stays for the next connection");
}

#[test]
fn surface_rotate_grace_is_capped_at_twenty_four_hours() {
    let (env, _node, session) = seed_pairing_env();
    let (_node, surface, old_token) = attached_surface(&env, &session);
    let (code, _stdout, stderr) = env.run(&["surface", "rotate", &surface.0, "--grace", "48h"]);
    assert_eq!(code, 0, "surface rotate --grace failed: {stderr}");
    let store = env.store();
    let now = Timestamp::now();
    let old = store
        .list_surface_tokens(&surface)
        .expect("token rows")
        .into_iter()
        .find(|token| token.token_hash == vowp_core::agent::hash_code(&old_token))
        .expect("old token row");
    let expiry = old.expires_at.expect("old token keeps an expiry");
    assert!(
        expiry.since(now) <= Duration::from_secs(24 * 60 * 60) + Duration::from_secs(2),
        "the grace is capped at 24 hours: {}",
        expiry.as_rfc3339()
    );
    assert!(
        expiry.since(now) >= Duration::from_secs(23 * 60 * 60),
        "a 48h request still grants almost the 24h cap: {}",
        expiry.as_rfc3339()
    );

    // A second rotation with a short grace shortens the replacement token too.
    let (code, value, stderr) = env.run_json(&["surface", "rotate", &surface.0, "--grace", "30m"]);
    assert_eq!(code, 0, "{stderr}");
    let old_expires: Timestamp = serde_json::from_value(value["old_expires_at"].clone()).expect("old expiry");
    let remaining = old_expires.since(Timestamp::now());
    assert!(
        remaining <= Duration::from_secs(30 * 60) + Duration::from_secs(2) && remaining >= Duration::from_secs(29 * 60),
        "the short grace is applied: {}",
        old_expires.as_rfc3339()
    );
}

#[test]
fn surface_rotate_grace_refuses_pending_and_revoked_surfaces() {
    let (env, node, session) = seed_pairing_env();
    let store = env.store();
    let now = store.clock().now();
    let pending = vowp_core::agent::pair::create_pending_surface(
        &store,
        &node,
        &session,
        vowp_protocol::SurfaceKind::Opencode,
        Some("opencode"),
        now,
    )
    .expect("pending surface");
    let (code, _stdout, stderr) = env.run(&["surface", "rotate", &pending.surface.id.0, "--grace", "1h"]);
    assert_eq!(code, 4, "a pending surface is a conflict: {stderr}");

    let (_node, surface, _token) = attached_surface(&env, &session);
    let deps = env.global(false);
    let input = BufferIo::new("");
    let ctx = vowp_cli::Ctx::build(&deps, &input).expect("context");
    vowp_core::agent::pair::revoke_surface(&store, ctx.deps(), &surface, now).expect("revoke");
    let (code, _stdout, stderr) = env.run(&["surface", "rotate", &surface.0, "--grace", "1h"]);
    assert_eq!(code, 5, "a revoked surface is refused: {stderr}");
}

#[test]
fn surface_pair_replace_revokes_the_old_surface_and_clears_the_stale_frame() {
    let (env, _node, session) = seed_pairing_env();
    let (_node, old, old_token) = attached_surface(&env, &session);
    let (code, _stdout, stderr) = env.run(&["surface", "rotate", &old.0]);
    assert_eq!(code, 0, "rotation failed: {stderr}");
    let frame = env
        .home_path()
        .join("agent-frames/mac-studio")
        .join(format!("{}.json", old.0));
    assert!(frame.is_file(), "the rotation is queued before the replace");

    let (code, value, stderr) = env.run_json(&[
        "surface",
        "pair",
        "--session",
        session.as_ref(),
        "--node",
        "mac-studio",
        "--kind",
        "opencode",
        "--app",
        "opencode",
        "--replace",
        &old.0,
    ]);
    assert_eq!(code, 0, "surface pair --replace failed: {stderr}");
    assert_eq!(value["replaced"], old.0);
    let new_id = SurfaceId(value["surface"].as_str().expect("new surface").to_owned());
    assert_ne!(new_id, old);
    assert_eq!(value["code"].as_str().expect("new code").len(), 52);
    assert!(!frame.exists(), "the old surface's queued frame is removed");

    let store = env.store();
    assert_eq!(
        store.surface_by_id(&old).expect("old lookup").expect("old row").state,
        SurfaceState::Revoked
    );
    assert_eq!(
        store
            .surface_by_id(&new_id)
            .expect("new lookup")
            .expect("new row")
            .state,
        SurfaceState::Pending
    );
    assert!(
        store
            .list_surface_tokens(&old)
            .expect("old tokens")
            .iter()
            .all(|token| token.revoked_at.is_some()),
        "every old token is revoked"
    );
    assert!(
        verify_surface_token(&store, &old_token, Timestamp::now()).is_err(),
        "the old token is dead immediately"
    );
}

#[test]
fn surface_ls_shows_prefix_overlap_pending_and_expired_without_a_token() {
    let (env, _node, session) = seed_pairing_env();
    let (_node, surface, _token) = attached_surface(&env, &session);
    let (code, rotated, stderr) = env.run_json(&["surface", "rotate", &surface.0]);
    assert_eq!(code, 0, "rotation failed: {stderr}");
    let new_expiry: Timestamp = serde_json::from_value(rotated["expires_at"].clone()).expect("new expiry");

    let store = env.store();
    let tokens = store.list_surface_tokens(&surface).expect("tokens");
    let newest = tokens
        .iter()
        .filter(|token| token.revoked_at.is_none())
        .max_by_key(|token| token.created_at)
        .expect("newest token");
    let old_expiry = tokens
        .iter()
        .filter(|token| token.revoked_at.is_none())
        .filter_map(|token| token.expires_at)
        .filter(|expiry| *expiry < new_expiry)
        .max()
        .expect("overlap expiry");

    let (code, value, stderr) = env.run_json(&["surface", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let entry = value
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["id"] == surface.0)
        .expect("surface row");
    assert_eq!(entry["prefix"], newest.prefix);
    assert_eq!(entry["expires_at"], new_expiry.as_rfc3339());
    assert_eq!(entry["overlap_until"], old_expiry.as_rfc3339());
    assert_eq!(entry["pending"], true);
    assert_eq!(entry["delivery_deadline"], new_expiry.as_rfc3339());
    assert_eq!(entry["expired"], false);
    let text = serde_json::to_string(&value).expect("json text");
    assert!(!text.contains("vowp_surf_"), "no token in JSON: {text}");

    let (code, human, stderr) = env.run(&["surface", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("PREFIX"), "{human}");
    assert!(human.contains("OVERLAP"), "{human}");
    assert!(human.contains("PENDING"), "{human}");
    assert!(!human.contains("vowp_surf_"), "no token in human output: {human}");

    let now = Timestamp::now();
    let past = now.minus(Duration::from_secs(60));
    for token in store.list_surface_tokens(&surface).expect("tokens") {
        if token.revoked_at.is_none() {
            store
                .shorten_surface_token_expiry(&token.id, past)
                .expect("expire token");
        }
    }
    let (code, human, stderr) = env.run(&["surface", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("expired"), "an expired token is marked: {human}");
    assert!(human.contains("attached"), "the surface state stays attached: {human}");
    let (code, value, stderr) = env.run_json(&["surface", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let entry = value
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["id"] == surface.0)
        .expect("surface row");
    assert_eq!(entry["state"], "attached");
    assert_eq!(entry["expired"], true);
    assert!(entry["prefix"].is_null(), "no active prefix: {entry}");
}

#[test]
fn agent_service_install_without_home_exits_two() {
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args(["agent", "service", "install", "--print"])
        .env_remove("HOME")
        .env_remove("VOWP_AGENT_HOME")
        .env("NO_COLOR", "1")
        .output()
        .expect("run vowp agent service install");
    assert_eq!(output.status.code(), Some(2));
}

// ---------------------------------------------------------------------------
// Real gateway E2E over in-process pipes: AgentClient + socket MCP + bridge
// ---------------------------------------------------------------------------

/// Builds `Deps` for a temporary [`GatewayHome`] without spawning a process.
fn in_process_deps(home: &GatewayHome, project: &str) -> Deps {
    let io = BufferIo::new("");
    let global = Global {
        json: true,
        home: Some(home.root()),
        project: Some(home.checkout(project).display().to_string()),
        ..Global::default()
    };
    Ctx::build(&global, &io).expect("test context").deps().clone()
}

/// One real `gateway::serve` connection per [`Transport::open`] over Unix pipes.
struct InProcessGateway {
    paths: NodePaths,
    deps: Deps,
    home_id: String,
}

impl InProcessGateway {
    fn new(home: &GatewayHome) -> Self {
        let deps = in_process_deps(home, "vowp");
        Self {
            paths: NodePaths::new(home.root()),
            home_id: deps.node.to_string(),
            deps,
        }
    }
}

impl Transport for InProcessGateway {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let (agent_side, gateway_side) = UnixStream::pair().map_err(AgentError::Io)?;
        let read = gateway_side.try_clone().map_err(AgentError::Io)?;
        let store = Store::open_global(&self.paths)
            .map_err(|error| AgentError::Config(error.to_string()))?
            .with_clock(self.deps.clock.clone());
        let deps = self.deps.clone();
        let options = GatewayOptions {
            home_id: self.home_id.clone(),
            heartbeat_secs: 1,
            max_frame_bytes: 1024 * 1024,
            handshake_timeout: Duration::from_secs(5),
        };
        thread::Builder::new()
            .name("e2e-home-gateway".to_owned())
            .spawn(move || {
                let _ = gateway::serve_with_options(&store, &deps, BufReader::new(read), gateway_side, options);
            })
            .map_err(AgentError::Io)?;
        let writer = agent_side.try_clone().map_err(AgentError::Io)?;
        let (sender, frames) = mpsc::channel();
        thread::Builder::new()
            .name("e2e-agent-reader".to_owned())
            .spawn(move || {
                let mut reader = BufReader::new(agent_side);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if sender.send(line).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(AgentError::Io)?;
        Ok(Box::new(PipeFrameStream { writer, frames }))
    }
}

/// The agent end of one in-process gateway connection.
struct PipeFrameStream {
    writer: UnixStream,
    frames: mpsc::Receiver<String>,
}

impl FrameStream for PipeFrameStream {
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        let line = match timeout {
            Some(limit) => match self.frames.recv_timeout(limit) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(FrameRead::Timeout),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(FrameRead::Closed),
            },
            None => match self.frames.recv() {
                Ok(line) => line,
                Err(_) => return Ok(FrameRead::Closed),
            },
        };
        let frame = serde_json::from_str(&line).map_err(AgentError::Json)?;
        Ok(FrameRead::Frame(frame))
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        let mut bytes = serde_json::to_vec(frame)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).map_err(AgentError::Io)?;
        self.writer.flush().map_err(AgentError::Io)
    }
}

/// One loopback SSE endpoint that the fake opencode service URL points at.
struct FakeSseServer {
    port: u16,
    events: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
}

impl FakeSseServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake opencode");
        let port = listener.local_addr().expect("fake opencode address").port();
        listener.set_nonblocking(true).expect("nonblocking fake opencode");
        let events = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let events = Arc::clone(&events);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // macOS/BSD inherit O_NONBLOCK across accept(); the reader
                            // below needs blocking I/O with timeouts.
                            stream.set_nonblocking(false).expect("accept restores blocking mode");
                            let events = Arc::clone(&events);
                            let stop = Arc::clone(&stop);
                            thread::spawn(move || serve_sse(stream, events, stop));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            port,
            events,
            stop,
            accept: Some(accept),
        }
    }

    fn push(&self, event: serde_json::Value) {
        let text = format!("data: {}\n\n", serde_json::to_string(&event).expect("event JSON"));
        self.events.lock().expect("fake SSE events").push(text);
    }
}

impl Drop for FakeSseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// Serves the event stream, replaying every enqueued event until the test stops it.
fn serve_sse(mut stream: TcpStream, events: Arc<Mutex<Vec<String>>>, stop: Arc<AtomicBool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut reader = match stream.try_clone() {
        Ok(stream) => BufReader::new(stream),
        Err(_) => return,
    };
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
        }
    }
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if stream.write_all(headers.as_bytes()).is_err() {
        return;
    }
    let mut sent = 0usize;
    while !stop.load(Ordering::SeqCst) {
        let batch = events.lock().expect("fake SSE events").clone();
        for text in batch.iter().skip(sent) {
            if stream.write_all(text.as_bytes()).is_err() || stream.flush().is_err() {
                return;
            }
        }
        sent = batch.len();
        thread::sleep(Duration::from_millis(10));
    }
}

/// Writes the fake `opencode` that prints the SSE listener's readiness and password.
fn install_fake_opencode(tools: &Path, port: u16) -> PathBuf {
    std::fs::create_dir_all(tools).expect("fake tools dir");
    let path = tools.join("opencode");
    let body = format!(
        "#!/bin/sh\nprintf 'server listening on http://127.0.0.1:{port}\\n'\nprintf 'server password test-only-e2e-password\\n'\nsleep 60\n"
    );
    std::fs::write(&path, body).expect("fake opencode");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod opencode");
    path
}

/// Writes one private orchestrator config before the home command arrives.
#[allow(clippy::too_many_arguments)]
fn prewrite_orchestrator_config(
    state_dir: &Path,
    name: &str,
    app: OrchestratorApp,
    project: Option<&ProjectName>,
    working_dir: &Path,
    command: &Path,
) {
    let dir = state_dir.join("orchestrators").join(name);
    let service_dir = dir.join(".opencode/state/opencode");
    std::fs::create_dir_all(&service_dir).expect("orchestrator dirs");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("orchestrator dir mode");
    let value = serde_json::json!({
        "name": name,
        "app": app,
        "project": project.map(ToString::to_string),
        "command": command.display().to_string(),
        "args": [],
        "working_dir": working_dir.display().to_string(),
        "service_json": service_dir.join("service.json").display().to_string(),
    });
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("config JSON")).expect("orchestrator config");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("config mode");
}

/// Seeds one active node and its credential for an in-process agent.
fn prepare_agent(home: &GatewayHome, node_name: &str) -> (TempDir, PathBuf, NodeName, NodeKey, AgentConfig) {
    let agent_home = tempfile::tempdir().expect("agent home");
    let state_dir = agent_home.path().join(".vowp-agent");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).expect("state dir mode");
    let key = NodeKey::generate(&state_dir).expect("node key");
    let node = NodeName::parse(node_name).expect("node slug");
    let store = home.store();
    let now = store.clock().now();
    store
        .insert_pending_node(&node, &["linux".to_owned()], now)
        .expect("pending node");
    store
        .set_node_status(&node, NodeStatus::Active, "", now)
        .expect("active node");
    let key_id = key.public_key_hex();
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
    let deps = in_process_deps(home, "vowp");
    let config = AgentConfig::new(deps.node.to_string(), node.clone(), state_dir.clone(), "in-process");
    config.write().expect("agent.json");
    (agent_home, state_dir, node, key, config)
}

/// Starts one real `AgentClient` run loop over the in-process gateway.
fn start_agent(
    home: &GatewayHome,
    state_dir: PathBuf,
    key: NodeKey,
    config: AgentConfig,
) -> thread::JoinHandle<Result<AgentRunOutcome, AgentError>> {
    let transport = InProcessGateway::new(home);
    thread::spawn(move || {
        let mut client = AgentClient::new(state_dir, key, config, Box::new(transport), Arc::new(SystemAgentClock));
        client.run(&RunOptions::new(AgentRole::Both))
    })
}

/// Waits until the agent has bound its socket and completed the handshake.
fn wait_for_in_process_agent_connected(state_dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let connected = AgentStatus::load(state_dir)
            .ok()
            .flatten()
            .is_some_and(|status| status.connection == ConnectionState::Connected);
        let socket_bound = std::fs::symlink_metadata(state_dir.join("agent.sock"))
            .is_ok_and(|metadata| metadata.file_type().is_socket());
        if connected && socket_bound {
            return;
        }
        if Instant::now() >= deadline {
            let status = std::fs::read_to_string(AgentStatus::file_path(state_dir)).unwrap_or_default();
            panic!("the in-process agent did not connect; status: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Revokes the node so the run loop stops cleanly, then joins it.
fn stop_agent(home: &GatewayHome, node: &NodeName, handle: thread::JoinHandle<Result<AgentRunOutcome, AgentError>>) {
    let store = home.store();
    let now = store.clock().now();
    store
        .set_node_status(node, NodeStatus::Revoked, "", now)
        .expect("revoke node");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the in-process agent did not stop after revoke"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let outcome = handle.join().expect("agent thread").expect("agent run");
    assert!(
        matches!(outcome, AgentRunOutcome::Revoked { .. }),
        "the in-process agent must stop on revoke, got {outcome:?}"
    );
}

/// Ages a node past the offline threshold without changing its active status.
fn set_node_offline(home: &GatewayHome, node: &NodeName) {
    let store = home.store();
    let old = store.clock().now().minus(Duration::from_secs(600));
    store
        .set_node_status(node, NodeStatus::Active, "", old)
        .expect("offline node");
}

/// Marks a previously created node online now.
fn set_node_online(home: &GatewayHome, node: &NodeName) {
    let store = home.store();
    let now = store.clock().now();
    store
        .touch_node(node, now, Some("0.4.0"), Some(AGENT_PROTOCOL), &serde_json::json!({}))
        .expect("online node");
}

/// An orchestrator manager that only queues commands for the live gateway.
fn remote_manager(deps: &Deps) -> Orchestrator {
    Orchestrator::with_transport(
        deps.clone(),
        Vec::new(),
        Arc::new(CommandSpool::with_timeout(deps.paths.root(), Duration::ZERO)),
    )
}

/// One home-side remote orchestrator request.
fn remote_request(name: &str, app: OrchestratorApp, project: Option<&ProjectName>) -> NewOrchestrator {
    NewOrchestrator {
        name: name.to_owned(),
        project: project.cloned(),
        app,
        executor: None,
        force: false,
    }
}

/// Waits for a queued start/resume result to reach the home record.
fn wait_for_orchestrator_state(manager: &Orchestrator, name: &str, expected: vowp_protocol::OrchestratorNodeState) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let info = manager.info(name).expect("orchestrator info");
        let state = info.card.state;
        if state == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "orchestrator {name} stayed {state:?}, expected {expected:?}; detail: {:?}; last_error: {:?}",
                info.detail, info.card.last_error
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Creates one pending surface and redeems it with the real `vowp agent pair` CLI.
///
/// With a live agent the CLI must route through `agent.sock`; the fallback direct
/// SSH/WSS connection would use the fake target and fail the test.
fn pair_via_agent_cli(
    home: &GatewayHome,
    agent_home: &Path,
    state_dir: &Path,
    node: &NodeName,
    session: &SessionId,
) -> SurfaceId {
    let home_store = home.store();
    let now = home_store.clock().now();
    let pending = create_pending_surface(&home_store, node, session, SurfaceKind::Opencode, Some("opencode"), now)
        .expect("pending surface");
    let code = pending.code.clone();
    let surface = pending.surface.id.clone();
    let code_file = agent_home.join("pair.code");
    std::fs::write(&code_file, &code).expect("pair code file");
    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_vowp"))
        .args([
            "agent",
            "pair",
            "--code-file",
            &code_file.display().to_string(),
            "--surface",
            surface.0.as_str(),
            "--session",
            session.as_str(),
            "--app",
            "opencode",
        ])
        .env("HOME", agent_home)
        .env("VOWP_AGENT_HOME", state_dir)
        .env("NO_COLOR", "1")
        .env_remove("VOWP_HOME")
        .env_remove("VOWP_CONTROL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run vowp agent pair");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "agent pair failed: {stdout} {stderr}");
    assert!(
        !stdout.contains(&code) && !stderr.contains(&code),
        "the code must not be printed"
    );
    let token_path = AgentConfig::surface_token_path(state_dir, &surface);
    assert!(
        token_path.is_file(),
        "the live agent did not write {}",
        token_path.display()
    );
    let token = std::fs::read_to_string(&token_path).expect("surface token");
    assert!(
        !stdout.contains(token.trim()) && !stderr.contains(token.trim()),
        "the surface token must not be printed"
    );
    assert!(
        AgentConfig::surface_session_path(state_dir, &surface).is_file(),
        "the live agent did not write the session binding"
    );
    surface
}

/// One MCP `tools/call` through the real `vowp mcp --agent-socket` proxy.
fn mcp_tool_call(
    home: &GatewayHome,
    state_dir: &Path,
    surface: &SurfaceId,
    tool: &str,
    arguments: serde_json::Value,
) -> (Duration, serde_json::Value) {
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    });
    let input = format!("{}\n", serde_json::to_string(&request).expect("MCP request"));
    let args = vec![
        "vowp".to_owned(),
        "--home".to_owned(),
        home.root().display().to_string(),
        "--project".to_owned(),
        home.checkout("vowp").display().to_string(),
        "mcp".to_owned(),
        "--agent-socket".to_owned(),
        state_dir.join("agent.sock").display().to_string(),
        "--surface".to_owned(),
        surface.0.as_str().to_owned(),
    ];
    let mut io = BufferIo::new(input.clone());
    let started = Instant::now();
    let code = vowp_cli::run(args, &mut io);
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "{tool} failed: {}", io.stderr());
    let response = io
        .stdout()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["id"] == 1)
        .unwrap_or_else(|| panic!("{tool} returned no JSON-RPC response: {}", io.stdout()));
    (elapsed, response)
}

/// Returns the text payload of a successful MCP tool result.
fn mcp_text(response: &serde_json::Value) -> serde_json::Value {
    assert_eq!(response["result"]["isError"], false, "{response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("MCP text result");
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
}

/// Reads `vowp session tail <id> --json` as one parsed JSON value per event line.
fn session_tail(home: &GatewayHome, session: &SessionId) -> Vec<serde_json::Value> {
    let args = vec![
        "vowp".to_owned(),
        "--home".to_owned(),
        home.root().display().to_string(),
        "--project".to_owned(),
        home.checkout("vowp").display().to_string(),
        "--json".to_owned(),
        "session".to_owned(),
        "tail".to_owned(),
        session.as_str().to_owned(),
    ];
    let mut io = BufferIo::new("");
    let code = vowp_cli::run(args, &mut io);
    assert_eq!(code, 0, "session tail failed: {}", io.stderr());
    io.stdout()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("session event JSON"))
        .collect()
}

/// Waits until every event kind appears in the session log.
fn wait_for_tail_kinds(home: &GatewayHome, session: &SessionId, expected: &[&str]) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let tail = session_tail(home, session);
        let kinds: Vec<&str> = tail.iter().filter_map(|event| event["type"].as_str()).collect();
        if expected.iter().all(|kind| kinds.contains(kind)) {
            return tail;
        }
        assert!(
            Instant::now() < deadline,
            "session tail never reached {expected:?}; saw {kinds:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Sends one raw socket request and reads one response.
fn socket_request(path: &Path, request: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(path).expect("connect agent socket");
    let mut bytes = serde_json::to_vec(&request).expect("socket request JSON");
    bytes.push(b'\n');
    stream.write_all(&bytes).expect("write socket request");
    stream.flush().expect("flush socket request");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read socket response");
    serde_json::from_str(&line).expect("socket response JSON")
}

/// Pushes the full simulated OpenCode turn used by both real-gateway tests.
fn push_turn_events(server: &FakeSseServer, prompt: &str, answer: &str, base: i64) {
    server.push(serde_json::json!({
        "type": "session.execution.started",
        "created": base,
        "data": {"sessionID": "native-e2e", "model": "deepseek/deepseek-flash"},
    }));
    server.push(serde_json::json!({
        "type": "session.inbox.enqueued",
        "created": base + 1,
        "data": {
            "sessionID": "native-e2e",
            "inboxID": "inbox-e2e",
            "item": {"type": "user", "payload": {"text": prompt}},
        },
    }));
    server.push(serde_json::json!({
        "type": "session.tool.called",
        "created": base + 2,
        "data": {
            "sessionID": "native-e2e",
            "id": "tool-e2e",
            "name": "bash",
            "input": {"command": "printf hello"},
        },
    }));
    server.push(serde_json::json!({
        "type": "session.tool.success",
        "created": base + 3,
        "data": {
            "sessionID": "native-e2e",
            "id": "tool-e2e",
            "content": [{"type": "text", "text": "hello"}],
        },
    }));
    server.push(serde_json::json!({
        "type": "session.text.ended",
        "created": base + 4,
        "data": {
            "sessionID": "native-e2e",
            "assistantMessageID": "assistant-e2e",
            "ordinal": 0,
            "text": answer,
            "model": "deepseek/deepseek-flash",
        },
    }));
    server.push(serde_json::json!({
        "type": "session.usage.updated",
        "created": base + 5,
        "data": {
            "sessionID": "native-e2e",
            "tokens": {"input": 120, "output": 40},
            "cost": 0.012,
        },
    }));
    server.push(serde_json::json!({
        "type": "session.execution.succeeded",
        "created": base + 6,
        "data": {"sessionID": "native-e2e"},
    }));
}

/// Asserts the unbound session's finished run row is in the reserved `_node` project.
fn assert_node_run_finished(home: &GatewayHome, tail: &[serde_json::Value]) {
    let run_id = tail
        .iter()
        .find(|event| event["type"] == "cost")
        .and_then(|event| event["payload"]["run_id"].as_str())
        .expect("the cost event carries the run id");
    let node_project = ProjectName::parse("_node").expect("node project");
    let store = Store::open_with_project(&home.paths(), &node_project).expect("_node store");
    let run_id = RunId::parse(run_id).expect("run id");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let run = store.run(&run_id).expect("run lookup").expect("_node run row");
        if run.ended_at.is_some() {
            assert_eq!(run.outcome, Some(RunOutcome::Ok), "run outcome: {run:?}");
            return;
        }
        assert!(Instant::now() < deadline, "the _node run did not finish: {run:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Returns the JSON command files in one `agent-commands/<node>` outbox.
fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

/// Queues a `Start` and then a `Stop` with no live connection, leaving the start superseded.
fn queue_superseded_start(
    home: &GatewayHome,
    manager: &Orchestrator,
    node: &NodeName,
    name: &str,
) -> (SessionId, PathBuf) {
    let info = manager
        .create_on_node(
            &remote_request(name, OrchestratorApp::Opencode, None),
            Some(node.clone()),
        )
        .expect("queue orchestrator start");
    let outbox = home.root().join("agent-commands").join(node.as_str());
    let start = json_files(&outbox)
        .into_iter()
        .next()
        .expect("the start command is queued");
    manager.stop(name).expect("queue orchestrator stop");
    (info.session, start)
}

#[test]
fn real_gateway_unbound_orchestrator_pairs_over_socket_and_mirrors_sse() {
    let home = GatewayHome::new(&["vowp", "stackspirit"]);
    let (agent_home, state_dir, node, key, config) = prepare_agent(&home, "mac-studio");
    let deps = in_process_deps(&home, "vowp");
    let manager = remote_manager(&deps);

    // Create while the node is offline: the home keeps the session and queues no start,
    // so the surface can be paired before the agent has anything to attach.
    set_node_offline(&home, &node);
    let info = manager
        .create_on_node(
            &remote_request("global", OrchestratorApp::Opencode, None),
            Some(node.clone()),
        )
        .expect("create unbound orchestrator");
    let session = info.session.clone();
    assert_eq!(info.card.project, None);

    let sse = FakeSseServer::start();
    let tools = agent_home.path().join("bin");
    let fake = install_fake_opencode(&tools, sse.port);
    prewrite_orchestrator_config(
        &state_dir,
        "global",
        OrchestratorApp::Opencode,
        None,
        &state_dir.join("orchestrators/global"),
        &fake,
    );

    let handle = start_agent(&home, state_dir.clone(), key, config);
    wait_for_in_process_agent_connected(&state_dir);

    // Pair through the live agent's socket. A direct second connection would supersede it.
    let surface = pair_via_agent_cli(&home, agent_home.path(), &state_dir, &node, &session);
    assert!(
        !handle.is_finished(),
        "pairing through agent.sock must not supersede the live AgentClient connection"
    );
    let status = AgentStatus::load(&state_dir)
        .expect("agent status")
        .expect("status written");
    assert_eq!(status.connection, ConnectionState::Connected);

    set_node_online(&home, &node);
    manager.resume("global").expect("resume the unbound orchestrator");
    wait_for_orchestrator_state(&manager, "global", vowp_protocol::OrchestratorNodeState::Running);
    let service_path = state_dir.join("orchestrators/global/.opencode/state/opencode/service.json");
    assert!(service_path.is_file(), "the fake opencode service was not materialized");

    // MCP reads and writes over the real proxy, all against the real gateway.
    let (elapsed, response) = mcp_tool_call(&home, &state_dir, &surface, "fleet_status", serde_json::json!({}));
    assert!(elapsed < Duration::from_secs(1), "fleet_status took {elapsed:?}");
    let fleet = mcp_text(&response);
    let names: Vec<&str> = fleet["snapshot"]["projects"]
        .as_array()
        .expect("fleet projects")
        .iter()
        .filter_map(|project| project["name"].as_str())
        .collect();
    assert!(
        names.contains(&"vowp") && names.contains(&"stackspirit"),
        "fleet_status must see the multi-project home: {names:?}"
    );

    let (elapsed, response) = mcp_tool_call(&home, &state_dir, &surface, "session_resume", serde_json::json!({}));
    assert!(elapsed < Duration::from_secs(1), "session_resume took {elapsed:?}");
    let resumed = mcp_text(&response);
    assert_eq!(resumed["session"]["id"], session.as_str(), "{resumed}");

    let (elapsed, response) = mcp_tool_call(
        &home,
        &state_dir,
        &surface,
        "capacity_set",
        serde_json::json!({"target": 1, "mode": "auto"}),
    );
    assert!(elapsed < Duration::from_secs(1), "capacity_set took {elapsed:?}");
    assert_eq!(response["result"]["isError"], false, "{response}");

    let (elapsed, response) = mcp_tool_call(
        &home,
        &state_dir,
        &surface,
        "job_add",
        serde_json::json!({
            "project": "vowp",
            "id": "vowp-e2e-1",
            "title": "Real-gateway unbound E2E",
            "kind": "build",
            "role": "worker",
            "owns": ["src/**"],
        }),
    );
    assert!(elapsed < Duration::from_secs(1), "job_add took {elapsed:?}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let vowp = ProjectName::parse("vowp").expect("project");
    let project_store = Store::open_with_project(&home.paths(), &vowp).expect("project store");
    assert!(
        project_store
            .job(&JobId::parse("vowp-e2e-1").expect("job id"))
            .expect("job lookup")
            .is_some(),
        "the MCP job_add did not reach the vowp project"
    );

    // Simulated OpenCode SSE -> AgentClient's auto-wired bridge -> real gateway.
    let base = Timestamp::now().unix_millis();
    push_turn_events(&sse, "please run the E2E turn", "the E2E answer", base);
    let expected = [
        "turn.started",
        "user.message",
        "tool.call",
        "tool.result",
        "assistant.message",
        "cost",
        "turn.ended",
    ];
    let tail = wait_for_tail_kinds(&home, &session, &expected);
    assert!(
        tail.iter().filter_map(|event| event["seq"].as_u64()).any(|seq| seq > 1),
        "the mirrored turn must advance the session log: {tail:?}"
    );
    assert_node_run_finished(&home, &tail);
    assert_eq!(
        pending_spool_bytes(&state_dir),
        0,
        "no bridge frame may remain unsent after the turn ended"
    );

    stop_agent(&home, &node, handle);
    drop(sse);
}

#[test]
fn real_gateway_project_orchestrator_defaults_job_project_and_enforces_one_lease() {
    let home = GatewayHome::new(&["vowp", "stackspirit"]);
    let (agent_home, state_dir, node, key, config) = prepare_agent(&home, "mac-studio");
    let deps = in_process_deps(&home, "vowp");
    let manager = remote_manager(&deps);
    let project = ProjectName::parse("vowp").expect("project");

    set_node_offline(&home, &node);
    let info = manager
        .create_on_node(
            &remote_request("bound", OrchestratorApp::Opencode, Some(&project)),
            Some(node.clone()),
        )
        .expect("create project-bound orchestrator");
    let session = info.session.clone();
    assert_eq!(info.card.project.as_ref(), Some(&project));

    let sse = FakeSseServer::start();
    let tools = agent_home.path().join("bin");
    let fake = install_fake_opencode(&tools, sse.port);
    prewrite_orchestrator_config(
        &state_dir,
        "bound",
        OrchestratorApp::Opencode,
        Some(&project),
        &home.checkout("vowp"),
        &fake,
    );

    let handle = start_agent(&home, state_dir.clone(), key, config);
    wait_for_in_process_agent_connected(&state_dir);
    let surface = pair_via_agent_cli(&home, agent_home.path(), &state_dir, &node, &session);
    assert!(!handle.is_finished(), "pairing must not supersede the live agent");

    set_node_online(&home, &node);
    manager.resume("bound").expect("resume the project-bound orchestrator");
    wait_for_orchestrator_state(&manager, "bound", vowp_protocol::OrchestratorNodeState::Running);

    let (elapsed, response) = mcp_tool_call(&home, &state_dir, &surface, "fleet_status", serde_json::json!({}));
    assert!(elapsed < Duration::from_secs(1), "fleet_status took {elapsed:?}");
    assert_eq!(response["result"]["isError"], false, "{response}");

    let (elapsed, response) = mcp_tool_call(&home, &state_dir, &surface, "session_resume", serde_json::json!({}));
    assert!(elapsed < Duration::from_secs(1), "session_resume took {elapsed:?}");
    assert_eq!(response["result"]["isError"], false, "{response}");

    // No project argument: the bound session's project must be used.
    let (elapsed, response) = mcp_tool_call(
        &home,
        &state_dir,
        &surface,
        "job_add",
        serde_json::json!({
            "id": "vowp-bound-1",
            "title": "Project-bound E2E",
            "kind": "build",
            "role": "worker",
        }),
    );
    assert!(elapsed < Duration::from_secs(1), "job_add took {elapsed:?}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let project_store = Store::open_with_project(&home.paths(), &project).expect("project store");
    assert!(
        project_store
            .job(&JobId::parse("vowp-bound-1").expect("job id"))
            .expect("job lookup")
            .is_some(),
        "job_add must default to the session's bound project on a multi-project home"
    );

    let base = Timestamp::now().unix_millis();
    push_turn_events(&sse, "bound orchestrator prompt", "bound orchestrator answer", base);
    let expected = [
        "turn.started",
        "user.message",
        "tool.call",
        "tool.result",
        "assistant.message",
        "turn.ended",
    ];
    let _tail = wait_for_tail_kinds(&home, &session, &expected);
    assert_eq!(
        pending_spool_bytes(&state_dir),
        0,
        "no bridge frame may remain unsent after the turn ended"
    );

    // A second surface for the same session must not steal the live lease.
    let lease_before = home
        .store()
        .lease(&session)
        .expect("lease lookup")
        .expect("orchestrator holds a lease")
        .surface_id
        .clone();
    let second = pair_via_agent_cli(&home, agent_home.path(), &state_dir, &node, &session);
    assert_ne!(second, surface);
    let response = socket_request(
        &state_dir.join("agent.sock"),
        serde_json::json!({
            "kind": "attach",
            "surface": second.0.as_str(),
            "session": session.as_str(),
        }),
    );
    assert_eq!(response["kind"], "ok", "{response}");
    thread::sleep(Duration::from_millis(500));
    let lease_after = home
        .store()
        .lease(&session)
        .expect("lease lookup")
        .expect("the orchestrator still holds its lease")
        .surface_id
        .clone();
    assert_eq!(
        lease_after, lease_before,
        "the first surface must keep the one live lease"
    );

    stop_agent(&home, &node, handle);
    drop(sse);
}

#[test]
fn real_gateway_agent_pair_ignores_a_superseded_orchestrator_command() {
    // Direct transport: the home drains and retires the superseded Start before the
    // pairing answer; the client must keep reading instead of failing.
    let home = GatewayHome::new(&["vowp"]);
    let deps = in_process_deps(&home, "vowp");
    let direct_manager = remote_manager(&deps);
    let (direct_home, direct_state, direct_node, direct_key, direct_config) = prepare_agent(&home, "mac-direct");
    set_node_online(&home, &direct_node);
    let (direct_session, direct_start) =
        queue_superseded_start(&home, &direct_manager, &direct_node, "direct-superseded");

    let pending = create_pending_surface(
        &home.store(),
        &direct_node,
        &direct_session,
        SurfaceKind::Opencode,
        Some("opencode"),
        Timestamp::now(),
    )
    .expect("pending surface");
    let surface = pending.surface.id.clone();
    let request = SurfacePairRequest::new(pending.code, surface.clone(), direct_session.clone(), "opencode");
    let mut direct = AgentClient::new(
        direct_state.clone(),
        direct_key,
        direct_config,
        Box::new(InProcessGateway::new(&home)),
        Arc::new(SystemAgentClock),
    );
    let paired = direct
        .pair(&request)
        .expect("direct pairing must ignore the superseded OrchestratorCommand");
    assert_eq!(paired.surface_id, surface);
    assert!(paired.token_path.is_file());
    assert!(
        !direct_start.exists(),
        "the superseded start command in agent-commands/ must be retired"
    );
    drop(direct_home);

    // Socket transport: the live agent remains connected and the same pairing succeeds
    // through agent.sock without opening a superseding second connection.
    let (live_home, live_state, live_node, live_key, live_config) = prepare_agent(&home, "mac-live");
    let live_manager = remote_manager(&deps);
    set_node_online(&home, &live_node);
    let (live_session, live_start) = queue_superseded_start(&home, &live_manager, &live_node, "live-superseded");

    let handle = start_agent(&home, live_state.clone(), live_key, live_config);
    wait_for_in_process_agent_connected(&live_state);
    let surface = pair_via_agent_cli(&home, live_home.path(), &live_state, &live_node, &live_session);
    assert!(!handle.is_finished(), "socket pairing superseded the live agent");
    assert_eq!(
        AgentStatus::load(&live_state)
            .expect("agent status")
            .expect("status written")
            .connection,
        ConnectionState::Connected
    );
    assert!(
        !live_start.exists(),
        "the live gateway must retire the superseded command before answering SurfacePair"
    );
    assert!(
        AgentConfig::surface_token_path(&live_state, &surface).is_file(),
        "the socket-routed pairing did not write the token"
    );

    stop_agent(&home, &live_node, handle);
}
