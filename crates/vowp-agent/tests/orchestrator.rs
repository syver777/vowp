//! Hermetic orchestrator-host tests: a fake app script, a temporary agent home and a fake
//! transport. No real opencode/claude binary, home directory, network or tmux server is used.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vowp_agent::orchestrator::ORCHESTRATOR_LOCAL_INSTRUCTIONS;
use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, FrameRead, FrameStream, NodeKey, OrchestratorConfig,
    OrchestratorHost, RunOptions, RunOutcome, SurfaceAttachment, SurfaceBinding, Transport, validate_name,
};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentFrame, AgentRole, NodeName, OrchestratorApp, OrchestratorCommand, OrchestratorError,
    OrchestratorLocation, OrchestratorNodeState, ProjectName, SessionId, SurfaceId, Timestamp,
};

const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SESSION_TWO: &str = "01ARZ3NDEKTSV4RRFFQ69G5FBV";
const SURFACE_TWO: &str = "01ARZ3NDEKTSV4RRFFQ69G5FBW";
const SESSION_THREE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FCV";
const SURFACE_THREE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FCW";
const PROJECT: &str = "vowp";

fn session(value: &str) -> SessionId {
    SessionId::parse(value).expect("session id")
}

fn surface(value: &str) -> SurfaceId {
    SurfaceId(value.to_owned())
}

fn project() -> ProjectName {
    ProjectName::parse(PROJECT).expect("project")
}

fn node() -> NodeName {
    NodeName::parse("mac-studio").expect("node slug")
}

/// Builds a service password at runtime so no secret-shaped literal is committed.
fn service_password() -> String {
    format!("vowp-test-{}", "P".repeat(24))
}

fn agent_state(root: &Path) -> PathBuf {
    let state = root.join("agent");
    fs::create_dir_all(&state).expect("state dir");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).expect("state mode");
    state
}

fn project_root(root: &Path) -> PathBuf {
    let project = root.join("project");
    fs::create_dir_all(&project).expect("project dir");
    project
}

fn write_binding(state: &Path, surface: &SurfaceId, session: &SessionId) {
    let dir = state.join("surfaces").join(&surface.0);
    fs::create_dir_all(&dir).expect("surface dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("surface mode");
    fs::write(dir.join("session"), session.as_str()).expect("session binding");
    write_surface_state(&dir, "paired");
}

fn write_surface_state(dir: &Path, state: &str) {
    let path = dir.join("state");
    fs::write(&path, state).expect("surface state");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("surface state mode");
}

fn revoke_binding(state: &Path, surface: &SurfaceId) {
    write_surface_state(&state.join("surfaces").join(&surface.0), "revoked");
}

fn write_config(state: &Path, config: &OrchestratorConfig) {
    let path = OrchestratorHost::config_path(state, &config.name).expect("config path");
    fs::create_dir_all(path.parent().expect("config parent")).expect("config dir");
    fs::write(&path, serde_json::to_vec_pretty(config).expect("config json")).expect("config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("config mode");
}

fn fake_app(root: &Path, label: &str, password: &str) -> (PathBuf, PathBuf) {
    let script = root.join(format!("{label}-app.sh"));
    let count = root.join(format!("{label}-pids"));
    let body = format!(
        "#!/bin/sh\n\
         set -eu\n\
         count=\"$1\"\n\
         password=\"{password}\"\n\
         printf '%s\\n' \"$$\" >> \"$count\"\n\
         pwd > \"$count.cwd\"\n\
         env > \"$count.env\"\n\
         if [ -n \"${{OPENCODE_CONFIG_DIR:-}}\" ]; then\n\
           cat \"$OPENCODE_CONFIG_DIR/opencode.json\" > \"$count.seen-opencode.json\"\n\
           cat \"$OPENCODE_CONFIG_DIR/AGENTS.md\" > \"$count.seen-agents.md\"\n\
         fi\n\
         state=\"${{XDG_STATE_HOME:?}}/opencode\"\n\
         mkdir -p \"$state\"\n\
         umask 077\n\
         printf '{{\"version\":\"2.0.28\",\"port\":54321,\"url\":\"http://127.0.0.1:54321\",\"password\":\"%s\",\"pid\":%s}}\\n' \"$password\" \"$$\" > \"$state/service.json\"\n\
         chmod 600 \"$state/service.json\"\n\
         while :; do sleep 0.05; done\n"
    );
    fs::write(&script, body).expect("fake app");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("fake app mode");
    (script, count)
}

fn pid_count(count: &Path) -> usize {
    fs::read_to_string(count)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

fn last_pid(count: &Path) -> u32 {
    fs::read_to_string(count)
        .expect("pid file")
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .next_back()
        .expect("a recorded pid")
}

/// Returns one value from the environment snapshot the fake app wrote.
fn env_value(env: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    env.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
}

/// Returns the argument after `flag` in an argv list.
fn arg_after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let position = args
        .iter()
        .position(|arg| arg == flag)
        .unwrap_or_else(|| panic!("{flag} missing from {args:?}"));
    args.get(position + 1)
        .unwrap_or_else(|| panic!("{flag} has no value in {args:?}"))
}

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).expect("metadata").ino()
}

fn directory_names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .expect("directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_private_file(path: &Path) {
    let mode = fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "{} is not 0600", path.display());
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition did not become true");
}

fn start_command(name: &str, app: OrchestratorApp) -> OrchestratorCommand {
    OrchestratorCommand::Start {
        request_id: format!("req-start-{name}"),
        name: name.to_owned(),
        app,
        project: Some(project()),
    }
}

fn stop_command(name: &str) -> OrchestratorCommand {
    OrchestratorCommand::Stop {
        request_id: format!("req-stop-{name}"),
        name: name.to_owned(),
    }
}

fn resume_command(name: &str) -> OrchestratorCommand {
    OrchestratorCommand::Resume {
        request_id: format!("req-resume-{name}"),
        name: name.to_owned(),
    }
}

#[derive(Default)]
struct RecordingLink {
    attached: Vec<SurfaceBinding>,
    detached: Vec<SurfaceBinding>,
    fail_attach: bool,
    remove_on_attach: Option<PathBuf>,
}

impl SurfaceAttachment for RecordingLink {
    fn attach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        if self.fail_attach {
            return Err(OrchestratorError::new(
                "attach_failed",
                "the surface attach frame could not be sent to the home",
                true,
            ));
        }
        if let Some(path) = self.remove_on_attach.as_ref() {
            let _ = fs::remove_file(path);
        }
        self.attached.push(binding.clone());
        Ok(())
    }

    fn detach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        self.detached.push(binding.clone());
        Ok(())
    }
}

fn host_with_app(state: &Path, script: &Path, count: &Path, _password: &str) -> OrchestratorHost {
    OrchestratorHost::new(state)
        .with_startup(Duration::from_secs(5))
        .with_stop_grace(Duration::from_millis(300))
        .with_app_command(
            OrchestratorApp::Opencode,
            script.display().to_string(),
            vec![count.display().to_string()],
        )
}

fn bind(state: &Path, name: &str, surface_id: &str, session_id: &str, working: &Path, script: &Path, count: &Path) {
    write_binding(state, &surface(surface_id), &session(session_id));
    write_config(
        state,
        &OrchestratorConfig {
            name: name.to_owned(),
            app: OrchestratorApp::Opencode,
            project: Some(project()),
            session: Some(session(session_id)),
            surface: Some(surface(surface_id)),
            command: Some(script.display().to_string()),
            args: vec![count.display().to_string()],
            working_dir: Some(working.display().to_string()),
            service_json: Some(
                OrchestratorHost::dir(state, name)
                    .expect("dir")
                    .join(".opencode/state/opencode/service.json")
                    .display()
                    .to_string(),
            ),
        },
    );
}

#[test]
fn orchestrator_start_materializes_0600_config_and_starts_one_host() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "one", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("one", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Running);
    assert_eq!(link.attached.len(), 1);
    assert_eq!(link.attached[0].surface, surface(SURFACE));
    assert_eq!(link.attached[0].session, session(SESSION));

    let config_path = OrchestratorHost::config_path(&state, "one").expect("config path");
    let metadata = fs::metadata(&config_path).expect("config metadata");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let config: OrchestratorConfig =
        serde_json::from_slice(&fs::read(&config_path).expect("config bytes")).expect("config");
    assert_eq!(config.app, OrchestratorApp::Opencode);
    assert_eq!(config.project, Some(project()));
    assert_eq!(config.session, Some(session(SESSION)));
    assert_eq!(config.surface, Some(surface(SURFACE)));

    let host_path = OrchestratorHost::host_path(&state, "one").expect("host path");
    assert_eq!(
        fs::metadata(&host_path).expect("host metadata").permissions().mode() & 0o777,
        0o600
    );
    let host_state = host.state("one").expect("host state").expect("state exists");
    let pid = host_state.pid.expect("pid");
    assert_eq!(host_state.state, OrchestratorNodeState::Running);
    assert!(process_alive(pid));
    assert_eq!(pid_count(&count), 1);

    let result = host.stop(&stop_command("one"), &mut link);
    assert!(result.accepted);
    assert_eq!(result.state, OrchestratorNodeState::Stopped);
    wait_until(|| !process_alive(pid));
    assert!(!host_path.exists());
    assert_eq!(link.detached.len(), 1);
}

#[test]
fn orchestrator_duplicate_start_is_idempotent() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "dup", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let first = host.start(
        &start_command("dup", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(first.accepted);
    let dir = OrchestratorHost::dir(&state, "dup").expect("dir");
    let agents = fs::read_to_string(dir.join("AGENTS.md")).expect("instructions");
    let opencode = fs::read(dir.join("opencode.json")).expect("opencode config");
    let pid = host.state("dup").expect("state").expect("running").pid.expect("pid");
    let second = host.start(
        &start_command("dup", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(second.accepted, "{second:?}");
    assert_eq!(second.state, OrchestratorNodeState::Running);
    assert_eq!(pid_count(&count), 1, "a duplicate start spawned no second app");
    assert_eq!(host.state("dup").expect("state").expect("running").pid, Some(pid));
    assert_eq!(link.attached.len(), 1, "a running host is not attached twice");
    assert_eq!(
        fs::read_to_string(dir.join("AGENTS.md")).expect("instructions"),
        agents,
        "a duplicate start keeps the app files idempotent"
    );
    assert_eq!(
        fs::read(dir.join("opencode.json")).expect("opencode config"),
        opencode,
        "a duplicate start keeps the app files idempotent"
    );

    host.stop(&stop_command("dup"), &mut link);
}

#[test]
fn orchestrator_stop_kills_only_its_own_host_and_detaches() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    let password = service_password();
    let (script, count_one) = fake_app(root.path(), "stop-one", &password);
    let (script_two, count_two) = fake_app(root.path(), "stop-two", &password);
    let host = host_with_app(&state, &script, &count_one, &password);
    bind(&state, "one", SURFACE, SESSION, &working, &script, &count_one);
    bind(
        &state,
        "two",
        SURFACE_TWO,
        SESSION_TWO,
        &working,
        &script_two,
        &count_two,
    );
    let mut link = RecordingLink::default();

    let first = host.start(
        &start_command("one", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(first.accepted, "{first:?}");
    let second = host.start(
        &start_command("two", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(second.accepted, "{second:?}");
    let one_pid = last_pid(&count_one);
    let two_pid = last_pid(&count_two);
    assert!(process_alive(one_pid) && process_alive(two_pid));

    let stopped = host.stop(&stop_command("one"), &mut link);
    assert!(stopped.accepted, "{stopped:?}");
    wait_until(|| !process_alive(one_pid));
    assert!(process_alive(two_pid), "the second host keeps running");
    assert_eq!(link.detached.len(), 1);
    assert_eq!(link.detached[0].surface, surface(SURFACE));
    assert_eq!(link.detached[0].session, session(SESSION));

    host.stop(&stop_command("two"), &mut link);
    wait_until(|| !process_alive(two_pid));
}

#[test]
fn orchestrator_resume_starts_again_after_stop() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "resume", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let started = host.start(
        &start_command("resume", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted);
    let first_pid = last_pid(&count);
    let stopped = host.stop(&stop_command("resume"), &mut link);
    assert!(stopped.accepted);
    wait_until(|| !process_alive(first_pid));

    let resumed = host.start(&resume_command("resume"), None, &mut link);
    assert!(resumed.accepted, "{resumed:?}");
    assert_eq!(resumed.state, OrchestratorNodeState::Running);
    let second_pid = last_pid(&count);
    assert_ne!(first_pid, second_pid);
    assert!(process_alive(second_pid));
    assert_eq!(pid_count(&count), 2);
    assert_eq!(link.attached.len(), 2);

    host.stop(&stop_command("resume"), &mut link);
    wait_until(|| !process_alive(second_pid));
}

#[test]
fn orchestrator_missing_binary_returns_a_typed_error_and_leaves_no_state() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let missing = root.path().join("no-such-app");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_app_command(
            OrchestratorApp::Opencode,
            missing.display().to_string(),
            vec!["--fake".to_owned()],
        );
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("missing", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Failed);
    let error = result.error.expect("typed error");
    assert_eq!(error.code, "spawn_failed");
    assert!(!error.message.contains("no-such-app"), "{}", error.message);
    assert!(link.attached.is_empty());
    assert!(
        host.state("missing").expect("state").is_none(),
        "no partial host state is left"
    );
    assert!(fs::metadata(OrchestratorHost::host_path(&state, "missing").expect("path")).is_err());
}

#[test]
fn orchestrator_failed_attach_returns_a_typed_error_and_leaves_no_host() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "attach-fail", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink {
        fail_attach: true,
        ..RecordingLink::default()
    };

    let result = host.start(
        &start_command("attach-fail", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Failed);
    let error = result.error.expect("typed error");
    assert_eq!(error.code, "attach_failed");
    assert!(!error.message.contains(&state.display().to_string()));
    assert!(link.attached.is_empty());
    assert!(host.state("attach-fail").expect("state").is_none());
    let pid = last_pid(&count);
    wait_until(|| !process_alive(pid));
}

#[test]
fn orchestrator_bad_service_json_is_typed_and_leaves_no_host() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let script = root.path().join("bad-service.sh");
    fs::write(
        &script,
        "#!/bin/sh\n\
         state=\"${XDG_STATE_HOME:?}/opencode\"\n\
         mkdir -p \"$state\"\n\
         umask 077\n\
         printf '{not json}\\n' > \"$state/service.json\"\n\
         while :; do sleep 0.05; done\n",
    )
    .expect("bad script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_app_command(OrchestratorApp::Opencode, script.display().to_string(), Vec::new());
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("bad-json", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    let error = result.error.expect("typed error");
    assert_eq!(error.code, "config_invalid");
    assert!(!error.message.contains(&state.display().to_string()));
    assert!(host.state("bad-json").expect("state").is_none());
    assert!(link.attached.is_empty());
}

#[test]
fn orchestrator_macos_stdout_readiness_materializes_0600_service_json() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = format!("topsecret-macos-{}", "P".repeat(20));
    let script = root.path().join("macos-opencode.sh");
    fs::write(
        &script,
        "#!/bin/sh\n\
         printf 'server listening on http://127.0.0.1:43123\\n'\n\
         printf 'server password topsecret-macos-%s\\n' \"PPPPPPPPPPPPPPPPPPPP\"\n\
         while :; do sleep 0.05; done\n",
    )
    .expect("stdout app");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(5))
        .with_stop_grace(Duration::from_millis(300))
        .with_app_command(OrchestratorApp::Opencode, script.display().to_string(), Vec::new());
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("macos-stdout", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Running);

    let config = host
        .config("macos-stdout")
        .expect("config")
        .expect("materialized config");
    let service_path = PathBuf::from(config.service_json.expect("service.json path"));
    assert!(service_path.is_file(), "stdout readiness writes service.json");
    assert_private_file(&service_path);
    let service: serde_json::Value =
        serde_json::from_slice(&fs::read(&service_path).expect("service bytes")).expect("service json");
    assert_eq!(service["url"], "http://127.0.0.1:43123");
    assert_eq!(service["port"], 43123);
    assert_eq!(service["password"], password.as_str());

    let dir = OrchestratorHost::dir(&state, "macos-stdout").expect("dir");
    let host_state =
        fs::read_to_string(OrchestratorHost::host_path(&state, "macos-stdout").expect("path")).expect("host state");
    let record = fs::read_to_string(dir.join("orchestrator.json")).expect("record");
    for text in [
        serde_json::to_string(&result).expect("result json"),
        format!("{result:?}"),
        host_state,
        record,
    ] {
        assert!(!text.contains(&password), "{text}");
        assert!(!text.contains("43123"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
        assert!(!text.contains(&state.display().to_string()), "{text}");
    }

    host.stop(&stop_command("macos-stdout"), &mut link);
}

#[test]
fn orchestrator_app_exits_before_ready_is_typed() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let script = root.path().join("exit-before-ready.sh");
    fs::write(&script, "#!/bin/sh\nexit 7\n").expect("exit app");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_app_command(OrchestratorApp::Opencode, script.display().to_string(), Vec::new());
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("exit-early", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Failed);
    assert_eq!(result.error.expect("error").code, "app_exited");
    assert!(host.state("exit-early").expect("state").is_none());
    assert!(link.attached.is_empty());
}

#[test]
fn orchestrator_service_timeout_is_typed() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let script = root.path().join("silent-app.sh");
    fs::write(&script, "#!/bin/sh\nwhile :; do sleep 0.05; done\n").expect("silent app");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_millis(200))
        .with_app_command(OrchestratorApp::Opencode, script.display().to_string(), Vec::new());
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("timeout", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Failed);
    assert_eq!(result.error.expect("error").code, "service_timeout");
    assert!(host.state("timeout").expect("state").is_none());
}

#[test]
fn orchestrator_missing_default_binary_is_capability_missing() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let empty = root.path().join("empty-bin");
    fs::create_dir_all(&empty).expect("empty bin");
    let host = OrchestratorHost::new(&state).with_path_lookup(&empty);

    let outcome = host.prepare_start(&start_command("no-opencode", OrchestratorApp::Opencode), Some(&working));
    match outcome {
        vowp_agent::PrepareOutcome::Failed(result) => {
            assert_eq!(result.state, OrchestratorNodeState::Failed);
            let error = result.error.expect("error");
            assert_eq!(error.code, "capability_missing");
            assert!(error.message.contains("opencode"), "{}", error.message);
            assert!(
                !error.message.contains(&empty.display().to_string()),
                "{}",
                error.message
            );
        }
        other => panic!("expected capability_missing, got {other:?}"),
    }
    assert!(host.state("no-opencode").expect("state").is_none());
}

#[test]
fn orchestrator_chat_default_command_carries_the_session_and_holds_stdin() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let record = root.path().join("chat-record");
    let script = bin.join("vowp");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" > \"{}.args\"\n\
             printf '%s\\n' \"${{VOWP_AGENT_HOME:-}}\" > \"{}.home\"\n\
             while read line; do :; done\n",
            record.display(),
            record.display()
        ),
    )
    .expect("fake vowp");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_stop_grace(Duration::from_millis(300))
        .with_path_lookup(&bin);
    let mut link = RecordingLink::default();
    let command = OrchestratorCommand::Start {
        request_id: "req-start-chat".to_owned(),
        name: "chat".to_owned(),
        app: OrchestratorApp::Chat,
        project: None,
    };

    let result = host.start(&command, None, &mut link);
    assert!(result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Running);
    let config = host.config("chat").expect("config").expect("chat config");
    assert_eq!(
        config.args,
        vec!["chat".to_owned(), "--agent".to_owned(), SESSION.to_owned()]
    );
    let args = fs::read_to_string(format!("{}.args", record.display())).expect("args");
    assert_eq!(args.trim(), format!("chat --agent {SESSION}"));
    let home = fs::read_to_string(format!("{}.home", record.display())).expect("home");
    assert_eq!(
        home.trim(),
        state.display().to_string(),
        "vowp chat must use the agent state that started it"
    );
    let pid = host.state("chat").expect("state").expect("running").pid.expect("pid");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        process_alive(pid),
        "the piped stdin handle must keep `vowp chat` alive past EOF"
    );

    host.stop(&stop_command("chat"), &mut link);
    wait_until(|| !process_alive(pid));
}

#[test]
fn orchestrator_claude_holds_stdin_and_reports_an_early_exit() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let record = root.path().join("claude-record");
    let script = root.path().join("fake-claude.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             printf 'started\\n' > \"{}.started\"\n\
             while read line; do :; done\n",
            record.display()
        ),
    )
    .expect("fake claude");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_stop_grace(Duration::from_millis(300))
        .with_app_command(OrchestratorApp::ClaudeCode, script.display().to_string(), Vec::new());
    let mut link = RecordingLink::default();
    let command = OrchestratorCommand::Start {
        request_id: "req-start-claude".to_owned(),
        name: "claude".to_owned(),
        app: OrchestratorApp::ClaudeCode,
        project: None,
    };

    let result = host.start(&command, None, &mut link);
    assert!(result.accepted, "{result:?}");
    let pid = host.state("claude").expect("state").expect("running").pid.expect("pid");
    std::thread::sleep(Duration::from_millis(100));
    assert!(process_alive(pid), "claude keeps a piped stdin handle");
    host.stop(&stop_command("claude"), &mut link);
    wait_until(|| !process_alive(pid));

    let early = root.path().join("claude-exits.sh");
    fs::write(&early, "#!/bin/sh\nexit 3\n").expect("early claude");
    fs::set_permissions(&early, fs::Permissions::from_mode(0o700)).expect("script mode");
    let host = OrchestratorHost::new(&state).with_app_command(
        OrchestratorApp::ClaudeCode,
        early.display().to_string(),
        Vec::new(),
    );
    let result = host.start(
        &OrchestratorCommand::Start {
            request_id: "req-start-claude-early".to_owned(),
            name: "claude-early".to_owned(),
            app: OrchestratorApp::ClaudeCode,
            project: None,
        },
        None,
        &mut link,
    );
    assert!(!result.accepted, "{result:?}");
    assert_eq!(result.error.expect("error").code, "app_exited");
    assert!(host.state("claude-early").expect("state").is_none());
}

#[test]
fn orchestrator_never_logs_the_port_password_or_absolute_path() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "redact", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("redact", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    let serialized = serde_json::to_string(&result).expect("result json");
    let host_state =
        fs::read_to_string(OrchestratorHost::host_path(&state, "redact").expect("path")).expect("host state");
    let config = fs::read_to_string(OrchestratorHost::config_path(&state, "redact").expect("path")).expect("config");
    let state_text = state.display().to_string();
    for text in [&serialized, &host_state] {
        assert!(!text.contains(&password), "{text}");
        assert!(!text.contains("54321"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
        assert!(!text.contains(&state_text), "{text}");
        assert!(!text.contains(&working.display().to_string()), "{text}");
    }
    assert!(
        !config.contains(&password),
        "the service password is not copied into config"
    );

    let missing = root.path().join("absent-app");
    let other = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(2))
        .with_app_command(OrchestratorApp::Opencode, missing.display().to_string(), Vec::new());
    let failed = other.start(
        &start_command("redact-missing", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    let serialized = serde_json::to_string(&failed).expect("failed json");
    assert!(!serialized.contains("absent-app"), "{serialized}");
    assert!(!serialized.contains(&state_text), "{serialized}");
    assert!(!serialized.contains(&password), "{serialized}");

    host.stop(&stop_command("redact"), &mut link);
}

#[test]
fn orchestrator_disconnect_reports_degraded_and_reconnect_does_not_spawn_a_second_host() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "degraded", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let started = host.start(
        &start_command("degraded", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted);
    let pid = last_pid(&count);
    host.mark_degraded().expect("mark degraded");
    let state_file = host.state("degraded").expect("state").expect("degraded state");
    assert_eq!(state_file.state, OrchestratorNodeState::Degraded);
    assert_eq!(state_file.pid, Some(pid));
    assert!(process_alive(pid));

    let live = host.reconcile().expect("reconcile");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].name, "degraded");
    assert_eq!(live[0].binding.surface, surface(SURFACE));
    host.mark_running("degraded").expect("mark running");
    assert_eq!(
        host.state("degraded").expect("state").expect("running").state,
        OrchestratorNodeState::Running
    );
    assert_eq!(pid_count(&count), 1);
    assert!(process_alive(pid));

    // A duplicate Start after the reconnect still reports the existing state and never
    // spawns a second process.
    let again = host.start(
        &start_command("degraded", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(again.accepted);
    assert_eq!(pid_count(&count), 1);
    assert_eq!(last_pid(&count), pid);

    host.stop(&stop_command("degraded"), &mut link);
}

#[test]
fn orchestrator_start_materializes_the_app_files_0600() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    AgentConfig::new("home-1", node(), state.clone(), "example.invalid")
        .write()
        .expect("agent config");
    let password = service_password();
    let (script, count) = fake_app(root.path(), "app-files", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let dir = OrchestratorHost::dir(&state, "app-files").expect("dir");
    // Delete one app file during the attach: `finish_start` must rewrite it once the surface
    // is bound, not only rely on the pre-start copy.
    let mut link = RecordingLink {
        remove_on_attach: Some(dir.join("AGENTS.md")),
        ..RecordingLink::default()
    };

    let result = host.start(
        &start_command("app-files", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");

    assert!(
        dir.join("AGENTS.md").is_file(),
        "finish_start did not refresh the app files after the attach"
    );
    assert_eq!(
        fs::metadata(&dir).expect("dir metadata").permissions().mode() & 0o777,
        0o700,
        "the orchestrator directory is private"
    );
    for name in ["AGENTS.md", "opencode.json", "orchestrator.json", "config.json"] {
        assert_private_file(&dir.join(name));
    }
    assert_eq!(
        fs::read_to_string(dir.join("AGENTS.md")).expect("instructions"),
        ORCHESTRATOR_LOCAL_INSTRUCTIONS,
        "AGENTS.md is exactly the local-surface template"
    );

    // The app's own environment points at the private dirs and at the directory opencode
    // reads its global config from; both files are readable there when the app starts.
    let env = fs::read_to_string(format!("{}.env", count.display())).expect("environment record");
    assert_eq!(
        env_value(&env, "XDG_STATE_HOME"),
        Some(dir.join(".opencode/state").display().to_string())
    );
    assert_eq!(
        env_value(&env, "XDG_CONFIG_HOME"),
        Some(dir.join(".opencode/config").display().to_string())
    );
    assert_eq!(env_value(&env, "OPENCODE_CONFIG_DIR"), Some(dir.display().to_string()));
    let app_config = PathBuf::from(env_value(&env, "OPENCODE_CONFIG_DIR").expect("config dir"));
    assert!(app_config.join("opencode.json").is_file());
    assert!(app_config.join("AGENTS.md").is_file());
    // The child process could read both files from the directory opencode loads its global
    // config and AGENTS.md from.
    assert_eq!(
        fs::read(format!("{}.seen-opencode.json", count.display())).expect("app read opencode.json"),
        fs::read(dir.join("opencode.json")).expect("opencode.json")
    );
    assert_eq!(
        fs::read(format!("{}.seen-agents.md", count.display())).expect("app read AGENTS.md"),
        fs::read(dir.join("AGENTS.md")).expect("AGENTS.md")
    );
    let cwd = fs::read_to_string(format!("{}.cwd", count.display())).expect("cwd record");
    assert_eq!(
        fs::canonicalize(cwd.trim()).expect("app cwd"),
        fs::canonicalize(&working).expect("project root")
    );

    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("orchestrator.json")).expect("record")).expect("record json");
    assert_eq!(record["name"], "app-files");
    assert_eq!(record["app"], "opencode");
    assert_eq!(record["project"], PROJECT);
    assert_eq!(record["session"], SESSION);
    assert_eq!(record["surface"], SURFACE);
    assert_eq!(record["node"], "mac-studio");
    for private in [
        "token",
        "password",
        "port",
        "pair",
        "command",
        "working_dir",
        "service_json",
    ] {
        assert!(record.get(private).is_none(), "orchestrator.json has {private}");
    }

    host.stop(&stop_command("app-files"), &mut link);
}

#[test]
fn orchestrator_agents_md_carries_the_local_surface_rules() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "agents-rules", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("agents-rules", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    let dir = OrchestratorHost::dir(&state, "agents-rules").expect("dir");
    let agents = fs::read_to_string(dir.join("AGENTS.md")).expect("instructions");

    for rule in [
        "local surface",
        "project-relative",
        "The home cannot see the Mac's local files.",
        "Call `session_resume` if the tool is available",
        "Ask before public actions",
        "Never read a secret",
        "Do not open a tunnel",
        "The agent is the transport",
        "Do not promise a VOWP tool call",
    ] {
        assert!(agents.contains(rule), "missing {rule:?}:\n{agents}");
    }
    for forbidden in ["tunnel_open", "tunnel_close", "will expose", "MCP tools are available"] {
        assert!(
            !agents.contains(forbidden),
            "AGENTS.md promises {forbidden:?}:\n{agents}"
        );
    }

    host.stop(&stop_command("agents-rules"), &mut link);
}

#[test]
fn orchestrator_opencode_json_names_the_socket_and_surface_without_a_token() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "opencode-config", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("opencode-config", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    let dir = OrchestratorHost::dir(&state, "opencode-config").expect("dir");
    let text = fs::read_to_string(dir.join("opencode.json")).expect("opencode config");
    let value: serde_json::Value = serde_json::from_str(&text).expect("opencode json");
    let server = &value["mcp"]["vowp"];
    assert_eq!(server["type"], "local");
    assert_eq!(server["enabled"], true);
    let command: Vec<String> = server["command"]
        .as_array()
        .expect("mcp command")
        .iter()
        .map(|value| value.as_str().expect("command string").to_owned())
        .collect();
    assert_eq!(command[0], "vowp");
    assert_eq!(command[1], "mcp");
    let socket = state.join("agent.sock").display().to_string();
    assert_eq!(arg_after(&command, "--agent-socket"), socket.as_str());
    assert_eq!(arg_after(&command, "--surface"), SURFACE);
    let instructions = dir.join("AGENTS.md").display().to_string();
    assert_eq!(value["instructions"][0].as_str(), Some(instructions.as_str()));
    assert!(!text.contains("vowp_surf_"), "{text}");
    assert!(!text.to_ascii_lowercase().contains("token"), "{text}");
    assert!(!text.contains("--token-file"), "{text}");

    host.stop(&stop_command("opencode-config"), &mut link);
}

#[test]
fn orchestrator_app_files_never_land_in_the_project_checkout() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "checkout", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();
    let before = directory_names(&working);

    let result = host.start(
        &start_command("checkout", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");

    assert_eq!(directory_names(&working), before, "the checkout was changed");
    for name in [
        "AGENTS.md",
        "opencode.json",
        "orchestrator.json",
        ".opencode",
        ".mcp.json",
    ] {
        assert!(!working.join(name).exists(), "{name} landed in the checkout");
    }
    assert!(!root.path().join("AGENTS.md").exists());
    assert!(!root.path().join("opencode.json").exists());

    host.stop(&stop_command("checkout"), &mut link);
}

#[test]
fn orchestrator_resume_refreshes_the_app_files_atomically() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "refresh", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let started = host.start(
        &start_command("refresh", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted, "{started:?}");
    let dir = OrchestratorHost::dir(&state, "refresh").expect("dir");
    let agents_path = dir.join("AGENTS.md");
    let first_inode = inode(&agents_path);
    let first_agents = fs::read_to_string(&agents_path).expect("instructions");
    let first_record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("orchestrator.json")).expect("record")).expect("record json");
    assert_eq!(first_record["surface"], SURFACE);

    let pid = last_pid(&count);
    let stopped = host.stop(&stop_command("refresh"), &mut link);
    assert!(stopped.accepted, "{stopped:?}");
    wait_until(|| !process_alive(pid));

    // Stale app files must be replaced, a wider mode tightened and a missing file recreated.
    fs::write(&agents_path, "stale\n").expect("stale instructions");
    fs::set_permissions(&agents_path, fs::Permissions::from_mode(0o644)).expect("stale mode");
    fs::remove_file(dir.join("opencode.json")).expect("remove opencode config");
    fs::write(dir.join("orchestrator.json"), "{}").expect("stale record");
    // Keep the stale inode open across the resume. A resume rewrites this file more than
    // once through temporary files, and the kernel may hand a just-freed inode number to
    // the next temporary file; without a live reference the final inode could cycle back
    // to `first_inode` even though every write was a rename. Holding the handle keeps the
    // old inode allocated, so an in-place rewrite would still show `first_inode` below.
    let stale = fs::File::open(&agents_path).expect("hold the stale instructions");
    assert_eq!(
        inode(&agents_path),
        first_inode,
        "the stale write kept the original inode"
    );
    bind(&state, "refresh", SURFACE_TWO, SESSION_TWO, &working, &script, &count);

    let resumed = host.start(&resume_command("refresh"), None, &mut link);
    assert!(resumed.accepted, "{resumed:?}");
    assert_eq!(
        fs::read_to_string(&agents_path).expect("refreshed instructions"),
        first_agents,
        "the resume restored the local template"
    );
    assert_ne!(
        inode(&agents_path),
        first_inode,
        "the refresh replaces the file atomically"
    );
    drop(stale);
    for name in ["AGENTS.md", "opencode.json", "orchestrator.json"] {
        assert_private_file(&dir.join(name));
    }
    let opencode: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("opencode.json")).expect("opencode config")).expect("opencode json");
    let command: Vec<String> = opencode["mcp"]["vowp"]["command"]
        .as_array()
        .expect("mcp command")
        .iter()
        .map(|value| value.as_str().expect("command string").to_owned())
        .collect();
    assert_eq!(arg_after(&command, "--surface"), SURFACE_TWO);
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("orchestrator.json")).expect("record")).expect("record json");
    assert_eq!(record["surface"], SURFACE_TWO);
    assert_eq!(record["session"], SESSION_TWO);
    assert!(
        directory_names(&dir).iter().all(|name| !name.starts_with(".tmp-")),
        "a temporary file was left behind"
    );

    host.stop(&stop_command("refresh"), &mut link);
}

#[test]
fn orchestrator_app_files_never_contain_a_secret_or_pair_link() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let token = format!("vowp_surf_{}", "T".repeat(43));
    let token_path = AgentConfig::surface_token_path(&state, &surface(SURFACE));
    fs::write(&token_path, &token).expect("surface token");
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).expect("token mode");
    let password = service_password();
    let (script, count) = fake_app(root.path(), "secrets", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("secrets", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    let dir = OrchestratorHost::dir(&state, "secrets").expect("dir");
    for name in ["AGENTS.md", "opencode.json", "orchestrator.json"] {
        let text = fs::read_to_string(dir.join(name)).expect(name);
        for secret in [password.as_str(), token.as_str(), "vowp_surf_", "54321", "127.0.0.1"] {
            assert!(!text.contains(secret), "{name} contains {secret:?}:\n{text}");
        }
        for link in ["pair=", "vowp://", "wss://"] {
            assert!(!text.contains(link), "{name} contains a pair link:\n{text}");
        }
    }
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("orchestrator.json")).expect("record")).expect("record json");
    for private in ["token", "token_file", "password", "port", "pair", "pair_link", "url"] {
        assert!(record.get(private).is_none(), "orchestrator.json has {private}");
    }
    let record_text = fs::read_to_string(dir.join("orchestrator.json")).expect("record text");
    assert!(!record_text.contains(&state.display().to_string()), "{record_text}");
    assert!(!record_text.contains(&working.display().to_string()), "{record_text}");

    // opencode.json may name the agent socket and the local AGENTS.md, but no other absolute
    // path: both live below the agent state directory.
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("opencode.json")).expect("opencode config")).expect("opencode json");
    let command = value["mcp"]["vowp"]["command"].as_array().expect("mcp command");
    for arg in command.iter().filter_map(|value| value.as_str()) {
        if arg.starts_with('/') {
            assert!(Path::new(arg).starts_with(&state), "external path {arg}");
        }
    }
    let instructions = value["instructions"][0].as_str().expect("instructions");
    assert!(
        Path::new(instructions).starts_with(&state),
        "external path {instructions}"
    );

    host.stop(&stop_command("secrets"), &mut link);
}

#[test]
fn names_are_checked_before_any_path_is_built() {
    for name in ["", ".", "..", "../escape", "/tmp/escape", "a/b", "a\\b"] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
    let result = OrchestratorHost::new(std::env::temp_dir()).prepare_start(
        &OrchestratorCommand::Start {
            request_id: "req".to_owned(),
            name: "../escape".to_owned(),
            app: OrchestratorApp::Opencode,
            project: None,
        },
        None,
    );
    match result {
        vowp_agent::PrepareOutcome::Failed(result) => {
            assert_eq!(result.error.expect("error").code, "invalid_name")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

#[derive(Default)]
struct FakeState {
    connections: usize,
    received: Vec<AgentFrame>,
    /// Orchestrator files read at each `OrchestratorResult`, before the run revokes.
    snapshots: Vec<Vec<(String, Vec<u8>)>>,
}

type BeforeSend = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<FakeState>>,
    commands: Vec<OrchestratorCommand>,
    pre_send: Vec<Option<BeforeSend>>,
    snapshot_dir: Option<PathBuf>,
    clock: FixedClock,
}

impl FakeHome {
    fn new(command: OrchestratorCommand) -> Self {
        Self::with_commands(None, vec![command])
    }

    /// Queues every command and snapshots the local host files on each result.
    fn with_commands(snapshot_dir: Option<PathBuf>, commands: Vec<OrchestratorCommand>) -> Self {
        assert!(!commands.is_empty(), "a fake home needs at least one command");
        let pre_send = vec![None; commands.len()];
        Self {
            shared: Arc::new(Mutex::new(FakeState::default())),
            commands,
            pre_send,
            snapshot_dir,
            clock: FixedClock(Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")),
        }
    }

    /// Runs `action` before the command at `index` is delivered.
    fn before(mut self, index: usize, action: impl Fn() + Send + Sync + 'static) -> Self {
        self.pre_send[index] = Some(Arc::new(action));
        self
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn snapshots(&self) -> Vec<Vec<(String, Vec<u8>)>> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshots
            .clone()
    }
}

/// Reads the private orchestrator files at one result boundary.
fn snapshot_orchestrators(state: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(state.join("orchestrators")) else {
        return files;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        for file in ["config.json", "orchestrator.json", "host.json"] {
            if let Ok(bytes) = fs::read(dir.join(file)) {
                files.push((format!("{name}/{file}"), bytes));
            }
        }
    }
    files
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = state.connections;
        state.connections += 1;
        drop(state);
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            commands: self.commands.iter().cloned().collect(),
            pre_send: self.pre_send.clone(),
            expected: self.commands.len(),
            sent: 0,
            snapshot_dir: self.snapshot_dir.clone(),
            results: 0,
            clock: self.clock,
            connection,
            outbound: VecDeque::new(),
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<FakeState>>,
    commands: VecDeque<OrchestratorCommand>,
    pre_send: Vec<Option<BeforeSend>>,
    expected: usize,
    sent: usize,
    snapshot_dir: Option<PathBuf>,
    results: usize,
    clock: FixedClock,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        if let Some(frame) = self.outbound.pop_front() {
            return Ok(FrameRead::Frame(frame));
        }
        // The home sends the next command only after the result of the previous one.
        if self.sent <= self.results
            && let Some(command) = self.commands.pop_front()
        {
            if let Some(Some(action)) = self.pre_send.get(self.sent) {
                let action = Arc::clone(action);
                action();
            }
            self.sent += 1;
            self.outbound.push_back(AgentFrame::OrchestratorCommand(command));
            self.outbound.push_back(AgentFrame::Ping);
            return Ok(FrameRead::Frame(self.outbound.pop_front().expect("the queued command")));
        }
        Ok(FrameRead::Timeout)
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { .. } => self.outbound.push_back(AgentFrame::HomeChallenge {
                nonce: format!("nonce-{}", self.connection),
                home_id: "home-1".to_owned(),
                connection_id: format!("connection-{}", self.connection),
                server_time: self.clock.now(),
                heartbeat_secs: 10,
                max_frame_bytes: 1024 * 1024,
            }),
            AgentFrame::AgentAuth { .. } => self.outbound.push_back(AgentFrame::HomeWelcome {
                protocol: AGENT_PROTOCOL.to_owned(),
                home_id: "home-1".to_owned(),
                node: node(),
                connection_id: format!("connection-{}", self.connection),
            }),
            AgentFrame::OrchestratorResult(_) => {
                self.results += 1;
                if let Some(dir) = self.snapshot_dir.as_ref() {
                    let snapshot = snapshot_orchestrators(dir);
                    self.shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .snapshots
                        .push(snapshot);
                }
                if self.results >= self.expected {
                    self.outbound.push_back(AgentFrame::Revoked {
                        reason: "test complete".to_owned(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn connection_host_config(
    state: &Path,
    name: &str,
    script: &Path,
    count: &Path,
    _password: &str,
) -> OrchestratorConfig {
    let dir = OrchestratorHost::dir(state, name).expect("dir");
    OrchestratorConfig {
        name: name.to_owned(),
        app: OrchestratorApp::Opencode,
        project: Some(project()),
        session: Some(session(SESSION_THREE)),
        surface: Some(surface(SURFACE_THREE)),
        command: Some(script.display().to_string()),
        args: vec![count.display().to_string()],
        working_dir: Some(dir.display().to_string()),
        service_json: Some(dir.join(".opencode/state/opencode/service.json").display().to_string()),
    }
}

#[test]
fn orchestrator_connection_result_is_correlated_and_revoke_cleans_the_host() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE_THREE), &session(SESSION_THREE));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "connection", &password);
    write_config(
        &state,
        &connection_host_config(&state, "connection", &script, &count, &password),
    );
    let command = start_command("connection", OrchestratorApp::Opencode);
    let request_id = command.request_id().to_owned();
    let home = FakeHome::new(command);

    let key = NodeKey::generate(&state).expect("node key");
    let config = AgentConfig::new("home-1", node(), state.clone(), "example.invalid");
    let mut client = AgentClient::new(
        state.clone(),
        key,
        config,
        Box::new(home.clone()),
        Arc::new(FixedClock(Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp"))),
    );
    let outcome = client.run(&RunOptions::new(AgentRole::Surface)).expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "{outcome:?}");

    let frames = home.frames();
    let results: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::OrchestratorResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "{frames:?}");
    assert_eq!(results[0].request_id, request_id);
    assert!(results[0].accepted);
    assert_eq!(results[0].state, OrchestratorNodeState::Running);
    assert!(
        frames.iter().any(|frame| matches!(frame, AgentFrame::Pong)),
        "the ping is answered while the start is in flight: {frames:?}"
    );
    assert!(
        frames.iter().any(
            |frame| matches!(frame, AgentFrame::SurfaceAttach(attach) if attach.surface == surface(SURFACE_THREE))
        ),
        "the surface is attached: {frames:?}"
    );

    assert_eq!(pid_count(&count), 1);
    let pid = last_pid(&count);
    wait_until(|| !process_alive(pid));
    assert!(
        fs::metadata(OrchestratorHost::dir(&state, "connection").expect("dir")).is_err(),
        "revoke removes the private orchestrator directory"
    );
    let serialized = serde_json::to_string(&results[0]).expect("result json");
    assert!(!serialized.contains(&state.display().to_string()));
    assert!(!serialized.contains(&password));
    assert!(!serialized.contains("54321"));
    let _ = working;
}

/// Writes the local settings an earlier placement left behind.
fn move_host_config(
    state: &Path,
    name: &str,
    script: &Path,
    count: &Path,
    app: OrchestratorApp,
    session_id: Option<SessionId>,
    surface_id: Option<SurfaceId>,
) -> OrchestratorConfig {
    let dir = OrchestratorHost::dir(state, name).expect("dir");
    OrchestratorConfig {
        name: name.to_owned(),
        app,
        project: Some(project()),
        session: session_id,
        surface: surface_id,
        command: Some(script.display().to_string()),
        args: vec![count.display().to_string()],
        working_dir: Some(dir.display().to_string()),
        service_json: Some(dir.join(".opencode/state/opencode/service.json").display().to_string()),
    }
}

/// Runs one fake home connection to completion and returns its recording.
fn run_fake_home(state: &Path, home: FakeHome) -> FakeHome {
    let key = NodeKey::generate(state).expect("node key");
    let config = AgentConfig::new("home-1", node(), state.to_path_buf(), "example.invalid");
    let mut client = AgentClient::new(
        state.to_path_buf(),
        key,
        config,
        Box::new(home.clone()),
        Arc::new(FixedClock(Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp"))),
    );
    let outcome = client.run(&RunOptions::new(AgentRole::Surface)).expect("run outcome");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "{outcome:?}");
    home
}

fn orchestrator_results(frames: &[AgentFrame]) -> Vec<vowp_protocol::OrchestratorResult> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            AgentFrame::OrchestratorResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn snapshot_json(snapshot: &[(String, Vec<u8>)], key: &str) -> serde_json::Value {
    let (_, bytes) = snapshot
        .iter()
        .find(|(name, _)| name == key)
        .unwrap_or_else(|| panic!("{key} is missing from the snapshot"));
    serde_json::from_slice(bytes).expect("snapshot json")
}

fn snapshot_has(snapshot: &[(String, Vec<u8>)], key: &str) -> bool {
    snapshot.iter().any(|(name, _)| name == key)
}

#[test]
fn agent_orchestrator_move_preserves_the_home_session_id() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    // The home paired this node's surface for the orchestrator's exact session before
    // the move; the landed `Start` command carries the name/app/project only.
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-session", &password);
    // A previous placement left a local config naming the same surface but an old
    // session. The home-issued pairing file, not the stale config, is authoritative.
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-session",
            &script,
            &count,
            OrchestratorApp::Opencode,
            Some(session(SESSION_TWO)),
            Some(surface(SURFACE)),
        ),
    );
    let host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(5))
        .with_stop_grace(Duration::from_millis(300));
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("move-session", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    assert_eq!(result.state, OrchestratorNodeState::Running);
    assert_eq!(link.attached.len(), 1);
    assert_eq!(link.attached[0].session, session(SESSION));
    assert_eq!(link.attached[0].surface, surface(SURFACE));

    let config = host
        .config("move-session")
        .expect("config read")
        .expect("config exists");
    assert_eq!(config.session, Some(session(SESSION)));
    assert_eq!(config.surface, Some(surface(SURFACE)));
    let dir = OrchestratorHost::dir(&state, "move-session").expect("dir");
    let record = snapshot_json(
        &[(
            "orchestrator.json".to_owned(),
            fs::read(dir.join("orchestrator.json")).expect("record"),
        )],
        "orchestrator.json",
    );
    assert_eq!(record["session"], SESSION);
    assert_eq!(record["surface"], SURFACE);
    let host_state = host.state("move-session").expect("state read").expect("host state");
    assert_eq!(host_state.session, Some(session(SESSION)));
    assert_eq!(host_state.surface, Some(surface(SURFACE)));
    assert_eq!(host_state.state, OrchestratorNodeState::Running);
    assert_eq!(pid_count(&count), 1, "the move started exactly one host");

    // No locally minted id: the config, app record and host state all carry the one id.
    let serialized = serde_json::to_string(&config).expect("config json");
    assert_eq!(serialized.matches(SESSION).count(), 1, "{serialized}");
    assert!(
        !serialized.contains(SESSION_TWO),
        "the move did not invent or reuse another session: {serialized}"
    );

    host.stop(&stop_command("move-session"), &mut link);
}

#[test]
fn agent_orchestrator_move_overwrites_a_stale_local_binding() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-stale", &password);
    // A previous placement left a dead surface and a config naming it. The home paired
    // this node's surface for the orchestrator's session; that pairing is authoritative.
    write_binding(&state, &surface(SURFACE_TWO), &session(SESSION_TWO));
    revoke_binding(&state, &surface(SURFACE_TWO));
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-stale",
            &script,
            &count,
            OrchestratorApp::Opencode,
            Some(session(SESSION_TWO)),
            Some(surface(SURFACE_TWO)),
        ),
    );
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();

    let result = host.start(
        &start_command("move-stale", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(result.accepted, "{result:?}");
    assert_eq!(link.attached.len(), 1);
    assert_eq!(link.attached[0].session, session(SESSION));
    assert_eq!(link.attached[0].surface, surface(SURFACE));
    assert!(
        !link
            .attached
            .iter()
            .any(|binding| binding.session == session(SESSION_TWO)),
        "the dead pairing was never started: {:?}",
        link.attached
    );

    let config = host.config("move-stale").expect("config read").expect("config exists");
    assert_eq!(config.session, Some(session(SESSION)));
    assert_eq!(config.surface, Some(surface(SURFACE)));
    let dir = OrchestratorHost::dir(&state, "move-stale").expect("dir");
    let record = snapshot_json(
        &[(
            "orchestrator.json".to_owned(),
            fs::read(dir.join("orchestrator.json")).expect("record"),
        )],
        "orchestrator.json",
    );
    assert_eq!(record["session"], SESSION);
    assert_eq!(record["surface"], SURFACE);
    let host_state = host.state("move-stale").expect("state read").expect("host state");
    assert_eq!(host_state.session, Some(session(SESSION)));
    assert_eq!(host_state.surface, Some(surface(SURFACE)));
    assert_eq!(pid_count(&count), 1);
    // The dead surface file is kept for inspection; it is never selected again.
    let stale = fs::read_to_string(state.join("surfaces").join(SURFACE_TWO).join("session")).expect("stale binding");
    assert_eq!(stale.trim(), SESSION_TWO);

    host.stop(&stop_command("move-stale"), &mut link);
}

#[test]
fn agent_orchestrator_move_stops_the_old_host_before_starting() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-old", &password);
    // The old placement is live on this node under the previous pairing.
    write_binding(&state, &surface(SURFACE_TWO), &session(SESSION_TWO));
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-old",
            &script,
            &count,
            OrchestratorApp::Chat,
            Some(session(SESSION_TWO)),
            Some(surface(SURFACE_TWO)),
        ),
    );
    let old_host = OrchestratorHost::new(&state)
        .with_startup(Duration::from_secs(5))
        .with_stop_grace(Duration::from_millis(300));
    let mut old_link = RecordingLink::default();
    let old = old_host.start(
        &start_command("move-old", OrchestratorApp::Chat),
        Some(&working),
        &mut old_link,
    );
    assert!(old.accepted, "{old:?}");
    let old_pid = last_pid(&count);
    assert!(process_alive(old_pid));

    // The home paired the target surface for the session and the previous pairing died.
    revoke_binding(&state, &surface(SURFACE_TWO));
    write_binding(&state, &surface(SURFACE), &session(SESSION));

    let home = FakeHome::with_commands(
        Some(state.clone()),
        vec![start_command("move-old", OrchestratorApp::Chat)],
    );
    let home = run_fake_home(&state, home);
    let frames = home.frames();
    let results = orchestrator_results(&frames);
    assert_eq!(results.len(), 1, "{frames:?}");
    assert!(results[0].accepted, "{results:?}");
    assert_eq!(results[0].state, OrchestratorNodeState::Running);

    let attach_old = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceAttach(attach) if attach.session == session(SESSION_TWO)))
        .unwrap_or_else(|| panic!("the reattached old surface is missing: {frames:?}"));
    let detach_old = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceDetach(detach) if detach.session == session(SESSION_TWO)))
        .unwrap_or_else(|| panic!("the old surface was not detached: {frames:?}"));
    let attach_new = frames
        .iter()
        .position(|frame| matches!(frame, AgentFrame::SurfaceAttach(attach) if attach.session == session(SESSION)))
        .unwrap_or_else(|| panic!("the new surface was not attached: {frames:?}"));
    assert!(
        attach_old < detach_old && detach_old < attach_new,
        "the old host must detach before the new host starts: {frames:?}"
    );

    let pid = last_pid(&count);
    assert_ne!(pid, old_pid);
    assert_eq!(pid_count(&count), 2, "one old and one new process were spawned");
    wait_until(|| !process_alive(old_pid));
    let snapshot = home.snapshots().pop().expect("result snapshot");
    let host_state = snapshot_json(&snapshot, "move-old/host.json");
    assert_eq!(host_state["pid"], pid);
    assert_eq!(host_state["session"], SESSION);
    assert_eq!(host_state["state"], "running");
    let config = snapshot_json(&snapshot, "move-old/config.json");
    assert_eq!(config["session"], SESSION);
    assert_eq!(config["surface"], SURFACE);
    wait_until(|| !process_alive(pid));
}

#[test]
fn agent_orchestrator_move_is_idempotent_and_refuses_a_different_session() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-idem", &password);
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    write_config(
        &state,
        &move_host_config(&state, "move-idem", &script, &count, OrchestratorApp::Chat, None, None),
    );
    let hook_state = state.clone();
    let home = FakeHome::with_commands(
        Some(state.clone()),
        vec![
            start_command("move-idem", OrchestratorApp::Chat),
            start_command("move-idem", OrchestratorApp::Chat),
            resume_command("move-idem"),
        ],
    )
    .before(2, move || {
        // The owner re-paired the node for another session while the old host was live.
        revoke_binding(&hook_state, &surface(SURFACE));
        write_binding(&hook_state, &surface(SURFACE_TWO), &session(SESSION_TWO));
    });
    let home = run_fake_home(&state, home);
    let frames = home.frames();
    let results = orchestrator_results(&frames);
    assert_eq!(results.len(), 3, "{frames:?}");
    assert!(results[0].accepted, "{results:?}");
    assert_eq!(results[0].state, OrchestratorNodeState::Running);
    assert!(results[1].accepted, "{results:?}");
    assert_eq!(results[1].state, OrchestratorNodeState::Running);
    assert!(
        results[1]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already running")),
        "a duplicate reports the live host: {results:?}"
    );
    assert!(!results[2].accepted, "{results:?}");
    assert_eq!(results[2].state, OrchestratorNodeState::Running);
    assert_eq!(
        results[2].error.as_ref().map(|error| error.code.as_str()),
        Some("session_conflict")
    );
    assert_eq!(pid_count(&count), 1, "the duplicate and the refusal spawned nothing");
    let attaches = frames
        .iter()
        .filter(|frame| matches!(frame, AgentFrame::SurfaceAttach(_)))
        .count();
    assert_eq!(attaches, 1, "the live host was attached exactly once: {frames:?}");
    assert!(
        !frames.iter().any(|frame| matches!(frame, AgentFrame::SurfaceDetach(_))),
        "the refused resume left the live host attached: {frames:?}"
    );

    let snapshots = home.snapshots();
    let refused = snapshots.last().expect("refusal snapshot");
    let host_state = snapshot_json(refused, "move-idem/host.json");
    assert_eq!(
        host_state["state"], "running",
        "the refused resume kept the host running"
    );
    assert_eq!(host_state["session"], SESSION);
    let config = snapshot_json(refused, "move-idem/config.json");
    assert_eq!(
        config["session"], SESSION,
        "the refused resume did not rebind the config"
    );

    let pid = last_pid(&count);
    wait_until(|| !process_alive(pid));
}

#[test]
fn agent_orchestrator_move_never_returns_a_secret_path_or_port() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-redact", &password);
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-redact",
            &script,
            &count,
            OrchestratorApp::Chat,
            None,
            None,
        ),
    );
    let home = FakeHome::with_commands(
        Some(state.clone()),
        vec![start_command("move-redact", OrchestratorApp::Chat)],
    );
    let home = run_fake_home(&state, home);
    let frames = home.frames();
    let results = orchestrator_results(&frames);
    assert_eq!(results.len(), 1, "{frames:?}");
    assert!(results[0].accepted, "{results:?}");

    let state_text = state.display().to_string();
    let serialized = serde_json::to_string(&results[0]).expect("result json");
    for text in [&serialized, &format!("{results:?}")] {
        assert!(!text.contains(&password), "{text}");
        assert!(!text.contains("54321"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
        assert!(!text.contains(&state_text), "{text}");
    }
    let snapshot = home.snapshots().pop().expect("result snapshot");
    let host_state = snapshot
        .iter()
        .find(|(name, _)| name == "move-redact/host.json")
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .expect("host state");
    assert!(!host_state.contains(&password), "{host_state}");
    assert!(!host_state.contains("54321"), "{host_state}");
    assert!(!host_state.contains("127.0.0.1"), "{host_state}");
    assert!(!host_state.contains(&state_text), "{host_state}");
    let pid = last_pid(&count);
    wait_until(|| !process_alive(pid));
}

#[test]
fn agent_orchestrator_move_requires_a_paired_surface() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let password = service_password();
    let (script, count) = fake_app(root.path(), "move-missing", &password);
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-missing",
            &script,
            &count,
            OrchestratorApp::Chat,
            None,
            None,
        ),
    );
    let home = FakeHome::with_commands(
        Some(state.clone()),
        vec![start_command("move-missing", OrchestratorApp::Chat)],
    );
    let home = run_fake_home(&state, home);
    let results = orchestrator_results(&home.frames());
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].accepted, "{results:?}");
    assert_eq!(
        results[0].error.as_ref().map(|error| error.code.as_str()),
        Some("surface_missing")
    );
    assert_eq!(pid_count(&count), 0, "no host was started without a pairing");
    assert!(
        !snapshot_has(&home.snapshots()[0], "move-missing/host.json"),
        "a refused start leaves no host state"
    );

    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let (script, count) = fake_app(root.path(), "move-ambiguous", &password);
    write_config(
        &state,
        &move_host_config(
            &state,
            "move-ambiguous",
            &script,
            &count,
            OrchestratorApp::Chat,
            None,
            None,
        ),
    );
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    write_binding(&state, &surface(SURFACE_TWO), &session(SESSION_TWO));
    let home = FakeHome::with_commands(
        Some(state.clone()),
        vec![start_command("move-ambiguous", OrchestratorApp::Chat)],
    );
    let home = run_fake_home(&state, home);
    let results = orchestrator_results(&home.frames());
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].accepted, "{results:?}");
    assert_eq!(
        results[0].error.as_ref().map(|error| error.code.as_str()),
        Some("surface_ambiguous")
    );
    assert_eq!(pid_count(&count), 0, "no host was started with an ambiguous pairing");
    assert!(
        !snapshot_has(&home.snapshots()[0], "move-ambiguous/host.json"),
        "a refused start leaves no host state"
    );
}

/// Writes one 0600 app service file at `path`.
fn write_service_file(path: &Path, url: &str, port: u16, password: &str, native_session_id: Option<&str>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("service dir");
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).expect("service dir mode");
    }
    let mut value = serde_json::json!({
        "version": "2.0.28",
        "url": url,
        "port": port,
        "pid": std::process::id(),
        "password": password,
    });
    if let Some(native_session_id) = native_session_id {
        value["native_session_id"] = serde_json::Value::String(native_session_id.to_owned());
    }
    fs::write(path, serde_json::to_vec(&value).expect("service json")).expect("service file");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("service mode");
}

/// Writes the private hint the bridge leaves beside `host.json`.
fn write_native_session_hint(state: &Path, name: &str, native_session_id: &str) -> PathBuf {
    let path = OrchestratorHost::native_session_path(state, name).expect("native session path");
    let value = serde_json::json!({ "native_session_id": native_session_id });
    fs::write(&path, serde_json::to_vec(&value).expect("native session json")).expect("native session hint");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("native session mode");
    path
}

#[test]
fn agent_orchestrator_endpoint_reads_a_live_0600_service_file() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "endpoint-live", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();
    let started = host.start(
        &start_command("endpoint-live", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted, "{started:?}");

    let endpoint = host.endpoint("endpoint-live").expect("live endpoint");
    assert_eq!(endpoint.name, "endpoint-live");
    assert_eq!(endpoint.app, OrchestratorApp::Opencode);
    assert_eq!(endpoint.session_id, session(SESSION));
    assert_eq!(endpoint.location, OrchestratorLocation::Node);
    assert!(
        endpoint.node.is_none(),
        "an unenrolled agent records no node name: {endpoint:?}"
    );
    assert_eq!(endpoint.state, OrchestratorNodeState::Running);
    assert_eq!(endpoint.url.as_deref(), Some("http://127.0.0.1:54321"));
    assert!(
        endpoint.owner.is_none(),
        "the agent cannot see the home owner: {endpoint:?}"
    );
    assert_eq!(endpoint.access, vowp_protocol::EndpointAccess::ReadOnly);
    assert!(
        endpoint.native_session_id.is_none(),
        "the agent does not learn the desktop app's native session id"
    );
    let auth = endpoint.auth.as_ref().expect("live local auth");
    assert_eq!(auth.header, "Authorization");
    assert_eq!(auth.scheme, "Basic");
    assert_eq!(auth.username, "opencode");
    assert_eq!(auth.password, password);
    assert!(
        serde_json::to_string(&endpoint)
            .expect("endpoint JSON")
            .contains(&password),
        "the explicit endpoint document carries the password"
    );
    assert!(
        !format!("{endpoint:?}").contains(&password),
        "Debug redacts the password"
    );

    let stopped = host.stop(&stop_command("endpoint-live"), &mut link);
    assert!(stopped.accepted, "{stopped:?}");
    let endpoint = host.endpoint("endpoint-live").expect("stopped endpoint");
    assert_eq!(endpoint.state, OrchestratorNodeState::Stopped);
    assert!(endpoint.url.is_none(), "a stopped host has no url");
    assert!(endpoint.auth.is_none(), "a stopped host has no auth");
}

#[test]
fn agent_orchestrator_endpoint_reports_the_persisted_native_session_id() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "endpoint-native", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();
    let started = host.start(
        &start_command("endpoint-native", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted, "{started:?}");

    let endpoint = host.endpoint("endpoint-native").expect("live endpoint");
    assert!(
        endpoint.native_session_id.is_none(),
        "the app service file names no session and no hint exists yet"
    );

    let hint = write_native_session_hint(&state, "endpoint-native", "ses_bridge_first");
    assert_private_file(&hint);
    let endpoint = host.endpoint("endpoint-native").expect("endpoint with a hint");
    assert_eq!(endpoint.native_session_id.as_deref(), Some("ses_bridge_first"));
    assert_eq!(endpoint.url.as_deref(), Some("http://127.0.0.1:54321"));

    // A re-created app session replaces the hint; the endpoint reads the latest state.
    write_native_session_hint(&state, "endpoint-native", "ses_bridge_second");
    let endpoint = host
        .endpoint("endpoint-native")
        .expect("endpoint after a re-created session");
    assert_eq!(endpoint.native_session_id.as_deref(), Some("ses_bridge_second"));
    let hint_text = fs::read_to_string(&hint).expect("hint text");
    assert!(
        !hint_text.contains(&password),
        "the hint carries no service password: {hint_text}"
    );
    assert!(
        !hint_text.contains("http"),
        "the hint carries no service URL: {hint_text}"
    );
    assert!(
        !hint_text.contains("127.0.0.1"),
        "the hint carries no service port: {hint_text}"
    );

    // The app's own service file wins over the bridge hint.
    let config = host
        .config("endpoint-native")
        .expect("local config")
        .expect("materialized config");
    let service_json = PathBuf::from(config.service_json.expect("service path"));
    write_service_file(
        &service_json,
        "http://127.0.0.1:54321",
        54321,
        &password,
        Some("ses_service_file"),
    );
    let endpoint = host.endpoint("endpoint-native").expect("service-file endpoint");
    assert_eq!(endpoint.native_session_id.as_deref(), Some("ses_service_file"));

    let stopped = host.stop(&stop_command("endpoint-native"), &mut link);
    assert!(stopped.accepted, "{stopped:?}");
    assert!(
        !hint.exists(),
        "stopping the orchestrator clears the native session hint"
    );
    let endpoint = host.endpoint("endpoint-native").expect("stopped endpoint");
    assert_eq!(endpoint.state, OrchestratorNodeState::Stopped);
    assert!(endpoint.url.is_none());
    assert!(endpoint.native_session_id.is_none());
    assert!(endpoint.auth.is_none());
}

#[test]
fn agent_orchestrator_endpoint_stopped_and_unknown_are_metadata_only() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let dir = OrchestratorHost::dir(&state, "stopped").expect("dir");
    let service_json = dir.join(".opencode/state/opencode/service.json");
    write_config(
        &state,
        &OrchestratorConfig {
            name: "stopped".to_owned(),
            app: OrchestratorApp::Opencode,
            project: None,
            session: Some(session(SESSION)),
            surface: Some(surface(SURFACE)),
            command: None,
            args: Vec::new(),
            working_dir: Some(dir.display().to_string()),
            service_json: Some(service_json.display().to_string()),
        },
    );
    // A stale service file from an earlier run must not become an endpoint.
    let stale = service_password();
    write_service_file(&service_json, "http://127.0.0.1:47777", 47777, &stale, None);
    let host = OrchestratorHost::new(&state);

    let endpoint = host.endpoint("stopped").expect("stopped endpoint");
    assert_eq!(endpoint.session_id, session(SESSION));
    assert_eq!(endpoint.location, OrchestratorLocation::Node);
    assert_eq!(endpoint.state, OrchestratorNodeState::Stopped);
    assert!(endpoint.url.is_none(), "a stopped host has no url");
    assert!(endpoint.native_session_id.is_none());
    assert!(endpoint.auth.is_none(), "a stopped host has no auth");
    assert!(!serde_json::to_string(&endpoint).expect("stopped JSON").contains(&stale));

    let unknown = host.endpoint("ghost").expect_err("an unknown name is typed");
    assert_eq!(unknown.code, "not_found");
    assert!(!unknown.message.contains(&state.display().to_string()));
}

#[test]
fn agent_orchestrator_endpoint_without_a_session_names_the_orchestrator() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let name = "nosession";
    write_config(
        &state,
        &OrchestratorConfig {
            name: name.to_owned(),
            app: OrchestratorApp::Opencode,
            project: None,
            session: None,
            surface: None,
            command: None,
            args: Vec::new(),
            working_dir: None,
            service_json: None,
        },
    );
    let host = OrchestratorHost::new(&state);

    let error = host
        .endpoint(name)
        .expect_err("a config without a paired session is typed");
    assert_eq!(error.code, "config_invalid");
    let text = format!("{}: {}", error.code, error.message);
    assert!(text.contains(name), "the error names the orchestrator: {text}");
    assert!(
        text.contains("no paired session"),
        "the error names the failure class: {text}"
    );
    for forbidden in [state.display().to_string(), "config.json".to_owned()] {
        assert!(!text.contains(&forbidden), "the error leaked a forbidden value: {text}");
    }
}

#[test]
fn agent_orchestrator_endpoint_errors_never_echo_a_path_or_password() {
    let root = tempfile::tempdir().expect("temp dir");
    let state = agent_state(root.path());
    let working = project_root(root.path());
    write_binding(&state, &surface(SURFACE), &session(SESSION));
    let password = service_password();
    let (script, count) = fake_app(root.path(), "endpoint-broken", &password);
    let host = host_with_app(&state, &script, &count, &password);
    let mut link = RecordingLink::default();
    let started = host.start(
        &start_command("endpoint-broken", OrchestratorApp::Opencode),
        Some(&working),
        &mut link,
    );
    assert!(started.accepted, "{started:?}");
    let service_json = OrchestratorHost::dir(&state, "endpoint-broken")
        .expect("dir")
        .join(".opencode/state/opencode/service.json");
    let marker = service_password();
    fs::write(
        &service_json,
        format!("{{\"url\":\"http://127.0.0.1:48888\",\"password\":\"{marker}\","),
    )
    .expect("corrupt service");
    fs::set_permissions(&service_json, fs::Permissions::from_mode(0o600)).expect("service mode");

    let error = host
        .endpoint("endpoint-broken")
        .expect_err("a corrupt service file is typed");
    assert_eq!(error.code, "config_invalid");
    let text = format!("{}: {}", error.code, error.message);
    assert!(
        text.contains("endpoint-broken"),
        "the error names the orchestrator: {text}"
    );
    for forbidden in [
        state.display().to_string(),
        service_json.display().to_string(),
        password,
        marker,
        "127.0.0.1".to_owned(),
        "48888".to_owned(),
    ] {
        assert!(!text.contains(&forbidden), "the error leaked a forbidden value: {text}");
    }

    // A service file wider than 0600 is refused for the same reason and just as quietly.
    write_service_file(
        &service_json,
        "http://127.0.0.1:48888",
        48888,
        &service_password(),
        None,
    );
    fs::set_permissions(&service_json, fs::Permissions::from_mode(0o644)).expect("wide service mode");
    let error = host
        .endpoint("endpoint-broken")
        .expect_err("a wide service file is typed");
    let text = format!("{}: {}", error.code, error.message);
    assert!(
        text.contains("endpoint-broken"),
        "the error names the orchestrator: {text}"
    );
    assert!(text.contains("0600"), "the error names the permission class: {text}");
    assert!(!text.contains(&service_json.display().to_string()), "{text}");

    let stopped = host.stop(&stop_command("endpoint-broken"), &mut link);
    assert!(stopped.accepted, "{stopped:?}");
}
