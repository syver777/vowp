//! Hermetic service tests: a temporary HOME/XDG_CONFIG_HOME, a fake manager and a fake
//! transport. No real launchd, systemd, `~/.vowp-agent` or network is touched.
//!
//! Spec: `docs/design/v0.4/hardening.md` §3.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::json;
use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, BRIDGE_EVENTS_FILE, CommandRecord, FrameRead, FrameStream,
    HandlerReject, InstallOptions, InstallReport, IsolationError, NodeKey, OpencodeBridge, RunHandler, RunOptions,
    SERVICE_MARKER, ServiceError, ServiceManager, ServicePlatform, ServiceRunOutcome, ServiceSpec, Transport,
    UninstallReport, ensure_not_root, install, uninstall,
};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentFrame, AgentRole, AssignmentPayload, ErrorKind, NodeName, SessionId, SurfaceId, Timestamp,
};

const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NATIVE_SESSION: &str = "ses_service_drain";
const PASSWORD: &str = "local-test-service-password";

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

/// A fake service manager that records every call and succeeds.
#[derive(Default)]
struct FakeManager {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    available: bool,
    stdout: String,
}

impl FakeManager {
    fn available() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            available: true,
            stdout: String::new(),
        }
    }

    fn with_stdout(stdout: &str) -> Self {
        Self {
            stdout: stdout.to_owned(),
            ..Self::available()
        }
    }

    fn unavailable() -> Self {
        Self {
            available: false,
            ..Self::available()
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ServiceManager for FakeManager {
    fn run(&self, argv: &[String]) -> Result<CommandRecord, ServiceError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(argv.to_vec());
        Ok(CommandRecord {
            argv: argv.to_vec(),
            status: Some(0),
            stdout: self.stdout.clone(),
            stderr: String::new(),
        })
    }

    fn available(&self) -> bool {
        self.available
    }
}

fn uid() -> u32 {
    vowp_agent::current_uid().expect("uid")
}

fn test_spec(platform: ServicePlatform, home: &Path, state: &Path, xdg: Option<PathBuf>) -> ServiceSpec {
    let mut spec = ServiceSpec::new(
        platform,
        "/usr/local/bin/vowp",
        state,
        home,
        "/usr/local/bin:/usr/bin:/bin",
        uid(),
    );
    spec.xdg_config_home = xdg;
    spec
}

fn private_home() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let state = home.path().join(".vowp-agent");
    fs::create_dir_all(&state).expect("state dir");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("state mode");
    let path = home.path().to_path_buf();
    (home, path, state)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn agent_service_install_writes_the_launchd_plist_under_a_temp_home() {
    let (_holder, home, state) = private_home();
    let spec = test_spec(ServicePlatform::Launchd, &home, &state, None);
    let manager = FakeManager::available();
    let report: InstallReport = install(&spec, &InstallOptions::default(), &manager).expect("install launchd");

    let expected = home.join("Library/LaunchAgents/dev.vowp.agent.plist");
    assert_eq!(report.unit_path, expected);
    assert!(report.installed);
    assert!(expected.is_file(), "the plist is written");
    assert_eq!(mode(&expected), 0o600, "the plist is 0600");
    let text = fs::read_to_string(&expected).expect("plist text");
    assert!(text.contains(SERVICE_MARKER));
    assert!(text.contains("<string>dev.vowp.agent</string>"));
    assert!(text.contains("<string>/usr/local/bin/vowp</string>"));
    assert!(text.contains("<string>agent</string><string>run</string>"));
    assert!(text.contains("<string>--foreground</string><string>--service</string>"));
    assert!(text.contains(&format!(
        "<key>VOWP_AGENT_HOME</key><string>{}</string>",
        state.display()
    )));
    assert!(text.contains(&format!("<key>{}/run</key><true/>", state.display())));
    assert!(text.contains(&format!("{}/logs/agent.log", state.display())));
    assert!(text.contains("<!-- generated by vowp agent service install -->"));

    let marker = state.join("run");
    assert!(marker.is_file(), "launchd install writes the restart marker");
    assert_eq!(mode(&marker), 0o600);
    assert_eq!(mode(&state), 0o700, "the state directory stays private");
    assert_eq!(mode(&state.join("logs")), 0o700);
    assert!(
        manager.calls().is_empty(),
        "install writes but does not load without --now"
    );
    assert!(report.linger_hint.is_none(), "launchd has no linger");
}

#[test]
fn agent_service_install_writes_the_systemd_user_unit_under_a_temp_xdg_config() {
    let (_holder, home, state) = private_home();
    let xdg = home.join("xdg");
    let spec = test_spec(ServicePlatform::SystemdUser, &home, &state, Some(xdg.clone()));
    let manager = FakeManager::available();
    let report = install(&spec, &InstallOptions::default(), &manager).expect("install systemd");

    let expected = xdg.join("systemd/user/vowp-agent.service");
    assert_eq!(report.unit_path, expected);
    assert!(expected.is_file());
    assert_eq!(mode(&expected), 0o600);
    let text = fs::read_to_string(&expected).expect("unit text");
    assert!(text.starts_with("# generated by vowp agent service install"));
    assert!(text.contains("ExecStart=/usr/local/bin/vowp agent run --foreground --service"));
    assert!(text.contains(&format!("Environment=VOWP_AGENT_HOME={}", state.display())));
    assert!(text.contains("Restart=on-failure"));
    assert!(text.contains("RestartPreventExitStatus=4 5"));
    assert!(text.contains("Umask=0077"));
    assert!(text.contains("WantedBy=default.target"));
    assert!(!text.contains("User="));
    assert!(!state.join("run").exists(), "the systemd unit does not use the marker");
    assert_eq!(mode(&state), 0o700);
    assert!(manager.calls().is_empty());
    let hint = report.linger_hint.expect("linger hint");
    assert!(hint.contains("loginctl enable-linger"), "{hint}");

    // With an absolute XDG_CONFIG_HOME the unit lands there; a relative one is ignored.
    let fallback = spec.clone();
    let fallback = ServiceSpec {
        xdg_config_home: Some(PathBuf::from("relative/xdg")),
        ..fallback
    };
    assert_eq!(
        fallback.unit_path().expect("fallback unit"),
        home.join(".config/systemd/user/vowp-agent.service")
    );
}

#[test]
fn agent_service_install_never_calls_the_manager_without_now() {
    let (_holder, home, state) = private_home();
    let spec = test_spec(ServicePlatform::SystemdUser, &home, &state, None);
    let manager = FakeManager::available();

    let report = install(&spec, &InstallOptions::default(), &manager).expect("install without now");
    assert!(
        manager.calls().is_empty(),
        "no manager call without --now: {:?}",
        manager.calls()
    );
    assert!(report.unit_path.is_file());
    assert!(
        report
            .commands
            .iter()
            .any(|argv| argv.contains(&"daemon-reload".to_owned())),
        "the printout names daemon-reload: {:?}",
        report.commands
    );

    let now = InstallOptions {
        now: true,
        ..InstallOptions::default()
    };
    let report = install(&spec, &now, &manager).expect("install with now");
    assert_eq!(manager.calls().len(), report.commands.len());
    assert_eq!(manager.calls()[0], vec!["systemctl", "--user", "daemon-reload"]);
    assert!(
        manager.calls()[1].contains(&"enable".to_owned()) && manager.calls()[1].contains(&"--now".to_owned()),
        "{:?}",
        manager.calls()
    );

    // `--print` writes nothing and runs nothing, even with --now.
    let (_print_holder, print_home, print_state) = private_home();
    let print_spec = test_spec(ServicePlatform::SystemdUser, &print_home, &print_state, None);
    let printed = install(
        &print_spec,
        &InstallOptions {
            print_only: true,
            now: true,
            force: false,
        },
        &FakeManager::available(),
    )
    .expect("print");
    assert!(!printed.installed);
    assert!(!printed.text.is_empty());
    assert!(!printed.commands.is_empty());
    assert!(
        !print_spec.unit_path().expect("unit").exists(),
        "print does not write the unit"
    );
}

#[test]
fn agent_service_uninstall_removes_only_its_own_unit() {
    let (_holder, home, state) = private_home();
    let spec = test_spec(ServicePlatform::SystemdUser, &home, &state, None);
    let manager = FakeManager::available();
    let unit = spec.unit_path().expect("unit path");

    // A foreign file at the unit path is refused and left in place.
    fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit parent");
    fs::write(&unit, "[Unit]\nDescription=someone else\n").expect("foreign unit");
    let error = uninstall(&spec, &manager).expect_err("a foreign unit is refused");
    assert!(matches!(error, ServiceError::ForeignUnit { .. }));
    assert_eq!(error.exit_code(), 4);
    assert!(unit.exists(), "the foreign file is not removed");

    // A VOWP unit is stopped and removed; the state directory, node key and tokens stay.
    fs::remove_file(&unit).expect("foreign unit");
    install(
        &spec,
        &InstallOptions {
            force: true,
            ..InstallOptions::default()
        },
        &manager,
    )
    .expect("install");
    let other = unit.parent().expect("parent").join("other.service");
    fs::write(&other, "someone else's unit").expect("other unit");
    fs::write(state.join("node.key"), "not-a-real-key").expect("state file");
    let tokens = state.join("surfaces");
    fs::create_dir_all(&tokens).expect("tokens");
    fs::write(tokens.join("token-file"), "vowp_surf_test").expect("token file");

    let report: UninstallReport = uninstall(&spec, &manager).expect("uninstall");
    assert!(report.removed);
    assert!(!unit.exists(), "the VOWP unit is removed");
    assert!(other.exists(), "another unit file is untouched");
    assert!(state.is_dir(), "the state directory is kept");
    assert!(state.join("node.key").exists(), "the node key is kept");
    assert!(tokens.join("token-file").exists(), "surface tokens are kept");
    assert_eq!(
        manager.calls()[0],
        vec!["systemctl", "--user", "disable", "--now", "vowp-agent.service"]
    );
    assert_eq!(manager.calls()[1], vec!["systemctl", "--user", "daemon-reload"]);

    // A missing unit is a clean no-op.
    let missing = uninstall(&spec, &FakeManager::available()).expect("missing unit");
    assert!(!missing.removed);
}

#[test]
fn agent_service_refuses_a_foreign_owner_or_loose_state_dir() {
    let (_holder, home, state) = private_home();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("loose mode");
    let spec = test_spec(ServicePlatform::SystemdUser, &home, &state, None);
    let error = install(&spec, &InstallOptions::default(), &FakeManager::available())
        .expect_err("a loose state dir is refused");
    assert!(matches!(
        error,
        ServiceError::Isolation(IsolationError::LooseMode { .. })
    ));
    assert_eq!(error.exit_code(), 5);
    assert!(!spec.unit_path().expect("unit").exists(), "nothing is written");

    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("private mode");
    let foreign = ServiceSpec {
        uid: uid().wrapping_add(1),
        ..spec
    };
    let error = install(&foreign, &InstallOptions::default(), &FakeManager::available())
        .expect_err("a foreign owner is refused");
    match error {
        ServiceError::Isolation(IsolationError::ForeignOwner { path, expected, .. }) => {
            assert_eq!(expected, uid().wrapping_add(1));
            assert_eq!(path, state);
        }
        other => panic!("expected ForeignOwner, got {other:?}"),
    }
}

#[test]
fn agent_refuses_to_run_as_root() {
    assert!(matches!(ensure_not_root(0), Err(IsolationError::Root)));
    ensure_not_root(501).expect("a standard user is allowed");
    let (_holder, home, state) = private_home();
    let root = ServiceSpec {
        uid: 0,
        ..test_spec(ServicePlatform::Launchd, &home, &state, None)
    };
    assert!(matches!(
        root.validate(),
        Err(ServiceError::Isolation(IsolationError::Root))
    ));
    assert_eq!(ServiceError::Isolation(IsolationError::Root).exit_code(), 5);
}

// --- Drain -----------------------------------------------------------------------------------

/// One fake home connection that answers the handshake, records frames and asks for a drain
/// after the first bridge frame.
#[derive(Clone)]
struct DrainHome {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Vec<AgentFrame>>>,
}

impl Transport for DrainHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        Ok(Box::new(DrainStream {
            stop: Arc::clone(&self.stop),
            state: Arc::clone(&self.state),
            outbound: VecDeque::new(),
            key: None,
            challenge: None,
        }))
    }
}

struct DrainStream {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Vec<AgentFrame>>>,
    outbound: VecDeque<AgentFrame>,
    key: Option<String>,
    challenge: Option<(String, String, String)>,
}

impl FrameStream for DrainStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Timeout),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(frame.clone());
        if matches!(frame, AgentFrame::SurfaceEvent(_) | AgentFrame::SurfaceTurn(_)) {
            self.stop.store(true, Ordering::SeqCst);
        }
        match frame {
            AgentFrame::AgentHello { key_id, .. } => {
                self.key = Some(key_id.clone());
                let challenge = (
                    "drain-nonce".to_owned(),
                    "home-1".to_owned(),
                    "drain-connection".to_owned(),
                );
                self.challenge = Some(challenge.clone());
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: challenge.0,
                    home_id: challenge.1,
                    connection_id: challenge.2,
                    server_time: Timestamp::now(),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { signature } => {
                let ok = self.key.as_deref().zip(self.challenge.as_ref()).is_some_and(
                    |(key, (nonce, home_id, connection_id))| {
                        verify_signature(key, nonce, home_id, connection_id, signature)
                    },
                );
                if ok {
                    self.outbound.push_back(AgentFrame::HomeWelcome {
                        protocol: AGENT_PROTOCOL.to_owned(),
                        home_id: "home-1".to_owned(),
                        node: NodeName::parse("mac-studio").expect("node slug"),
                        connection_id: "drain-connection".to_owned(),
                    });
                } else {
                    self.outbound.push_back(AgentFrame::Error {
                        kind: ErrorKind::Auth,
                        message: "signature rejected".to_owned(),
                        retryable: false,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn verify_signature(public_hex: &str, nonce: &str, home_id: &str, connection_id: &str, signature_hex: &str) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let Ok(public) = hex::decode(public_hex) else {
        return false;
    };
    let Ok(public) = <[u8; 32]>::try_from(public.as_slice()) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&public) else {
        return false;
    };
    let Ok(signature) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    key.verify(
        &vowp_agent::handshake_payload(nonce, home_id, connection_id),
        &signature,
    )
    .is_ok()
}

struct NoopHandler;

impl RunHandler for NoopHandler {
    fn accept(&self, _assignment: &AssignmentPayload) -> Result<vowp_agent::AcceptedRun, HandlerReject> {
        Err(HandlerReject::other("the drain test never assigns a run"))
    }

    fn run(&self, _accepted: vowp_agent::AcceptedRun, _sink: &mut dyn vowp_agent::RunSink) {}

    fn cancel(&self, _run: &vowp_protocol::RunId) {}

    fn cleanup(&self, _run: &vowp_protocol::RunId) {}
}

/// A minimal opencode SSE stream: one event, then close.
fn fake_sse(events: Vec<serde_json::Value>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake SSE");
    let port = listener.local_addr().expect("fake SSE address").port();
    let handle = thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => raw.extend_from_slice(&chunk[..read]),
            }
        }
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
        for event in &events {
            let line = format!("data: {event}\n\n");
            if stream.write_all(line.as_bytes()).is_err() {
                return;
            }
        }
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Write);
    });
    (port, handle)
}

fn write_service(dir: &Path, port: u16) -> PathBuf {
    let path = dir.join("service.json");
    let text = json!({
        "version": "2.0.28",
        "url": format!("http://127.0.0.1:{port}"),
        "password": PASSWORD,
    })
    .to_string();
    fs::write(&path, text).expect("write service.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("service mode");
    path
}

#[test]
fn agent_service_drain_flushes_the_spool_and_releases_the_lease() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().to_path_buf();
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        state_dir.clone(),
        "home.example",
    );
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(&state_dir).expect("node key");

    let event = json!({
        "id": "evt_drain_1",
        "created": 1_700_000_000_000_i64,
        "type": "session.text.ended",
        "data": {
            "sessionID": NATIVE_SESSION,
            "assistantMessageID": "msg_drain_1",
            "ordinal": 1,
            "text": "the drain flushed this event",
        },
        "durable": { "seq": 1 },
    });
    let (port, sse) = fake_sse(vec![event]);
    let service = write_service(&state_dir, port);
    let surface = SurfaceId(SURFACE.to_owned());
    let session = SessionId::parse(SESSION).expect("session id");
    let bridge = OpencodeBridge::load(&service, &state_dir, surface.clone(), session.clone())
        .expect("load bridge")
        .with_native_session(NATIVE_SESSION);

    let stop = Arc::new(AtomicBool::new(false));
    let home = DrainHome {
        stop: Arc::clone(&stop),
        state: Arc::new(Mutex::new(Vec::new())),
    };
    let recorder = Arc::clone(&home.state);
    let mut client = AgentClient::new(
        state_dir.clone(),
        key,
        config,
        Box::new(home),
        Arc::new(FixedClock(Timestamp::parse("2026-11-15T10:00:00Z").expect("timestamp"))),
    )
    .with_bridge(bridge);

    let outcome = client
        .run_as_service(
            &RunOptions::new(AgentRole::Both),
            Arc::new(NoopHandler),
            Arc::clone(&stop),
        )
        .expect("service run drains");
    assert_eq!(outcome, ServiceRunOutcome::Drained);
    sse.join().expect("fake SSE");

    let frames = recorder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let attach = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceAttach(_)))
        .expect("the drain attaches the surface");
    let event_at = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceEvent(_)))
        .expect("the spooled event was flushed before the drain");
    let detach = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceDetach(_)))
        .expect("the drain sends SurfaceDetach");
    assert!(attach < event_at, "attach precedes the flushed event");
    assert!(event_at < detach, "the event is flushed before detach");

    let status = vowp_agent::AgentStatus::load(&state_dir)
        .expect("status")
        .expect("status written");
    assert_eq!(status.connection, vowp_agent::ConnectionState::Stopped);
    let spool = vowp_agent::OpencodeBridge::load(&service, &state_dir, surface, session)
        .expect("reload bridge")
        .spool_dir()
        .join(BRIDGE_EVENTS_FILE);
    assert!(
        fs::metadata(&spool).map(|metadata| metadata.len() > 0).unwrap_or(false),
        "the spool is kept for inspection: {}",
        spool.display()
    );
    assert!(
        state_dir.join("run").is_file(),
        "a drain keeps the launchd restart marker"
    );
}

#[test]
fn agent_service_status_reads_the_manager() {
    let (_holder, home, state) = private_home();
    let systemd = test_spec(ServicePlatform::SystemdUser, &home, &state, None);
    let manager = FakeManager::with_stdout("LoadState=loaded\nActiveState=active\nMainPID=1234\nExecMainStatus=0\n");
    let status = vowp_agent::service::status(&systemd, &manager).expect("status");
    assert!(status.available);
    assert_eq!(status.loaded, Some(true));
    assert_eq!(status.pid, Some(1234));
    assert_eq!(status.last_exit, Some(0));
    assert_eq!(status.unit_path, systemd.unit_path().expect("unit path"));
    assert_eq!(
        manager.calls()[0],
        vec![
            "systemctl",
            "--user",
            "show",
            "vowp-agent.service",
            "--property=LoadState,ActiveState,MainPID,ExecMainStatus",
            "--no-pager",
        ]
    );

    let launchd = test_spec(ServicePlatform::Launchd, &home, &state, None);
    let manager = FakeManager::with_stdout("state = running\n\tpid = 4321\n\tlast exit status = 0\n");
    let status = vowp_agent::service::status(&launchd, &manager).expect("status");
    assert_eq!(status.loaded, Some(true));
    assert_eq!(status.pid, Some(4321));
    assert_eq!(status.last_exit, Some(0));

    let unavailable = FakeManager::unavailable();
    let status = vowp_agent::service::status(&systemd, &unavailable).expect("unavailable status");
    assert!(!status.available, "a missing manager reports unavailable, not an error");
    assert_eq!(status.loaded, None);
    assert!(unavailable.calls().is_empty());
}
