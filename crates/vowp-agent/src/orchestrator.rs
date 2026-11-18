//! The Mac-side orchestrator host: materialized config, one app process and its surface.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.2, §5.6, §5.7 and §8.3, and
//! `docs/design/crates/vowp-agent.md`. A home `OrchestratorCommand` names an orchestrator,
//! its app and its project. The agent writes the private local copy below
//! `<state_dir>/orchestrators/<name>/`, starts the configured app command in its own process
//! group, waits for the app's mode-0600 `service.json` when the app is opencode, and asks the
//! caller to attach the paired surface before it reports `running`. The same private directory
//! holds the app-facing `AGENTS.md`, `opencode.json` and `orchestrator.json`; an opencode app
//! loads them through its private global config directory, never from the project checkout.
//!
//! The port, service password, pair link and absolute local paths are private configuration.
//! They are read when needed and never appear in a host state file, a result detail, an error
//! or a log line.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_protocol::{
    NodeName, OrchestratorApp, OrchestratorCommand, OrchestratorError, OrchestratorNodeState, OrchestratorResult,
    ProjectName, SessionId, SurfaceId, Timestamp,
};

use crate::client::validate_surface_id;
use crate::config::{AgentConfig, ensure_private_dir};
use crate::error::AgentError;
use crate::socket::AGENT_SOCKET_FILE;

/// Directory below the agent state directory that holds every orchestrator.
pub const ORCHESTRATORS_DIR: &str = "orchestrators";
/// Materialized local configuration file name.
pub const ORCHESTRATOR_CONFIG_FILE: &str = "config.json";
/// Private host state and pid file name.
pub const ORCHESTRATOR_HOST_FILE: &str = "host.json";
/// The app-facing standing instruction file name.
pub const ORCHESTRATOR_AGENTS_FILE: &str = "AGENTS.md";
/// The app-facing opencode MCP and instruction config file name.
pub const ORCHESTRATOR_OPENCODE_FILE: &str = "opencode.json";
/// The redacted app-facing record file name.
pub const ORCHESTRATOR_RECORD_FILE: &str = "orchestrator.json";
/// Environment variable that names opencode's global configuration directory.
///
/// opencode 2.0.28 resolves its global config directory from `OPENCODE_CONFIG_DIR` when it is
/// set, and otherwise from `$XDG_CONFIG_HOME/opencode`. Pointing it at the orchestrator directory
/// makes the app read `<orchestrator>/opencode.json` and `<orchestrator>/AGENTS.md` without any
/// copy in the project checkout.
pub const ORCHESTRATOR_CONFIG_DIR_ENV: &str = "OPENCODE_CONFIG_DIR";
/// Private XDG root for one managed opencode app.
pub const ORCHESTRATOR_XDG_DIR: &str = ".opencode";
/// Default time to wait for an opencode `service.json` before failing the start.
pub const ORCHESTRATOR_START_TIMEOUT: Duration = Duration::from_secs(20);
/// Time between two `service.json` probes.
pub const ORCHESTRATOR_POLL: Duration = Duration::from_millis(25);
/// How long a partially written or malformed `service.json` is retried before it is fatal.
const SERVICE_INVALID_GRACE: Duration = Duration::from_millis(750);
/// Time between SIGTERM and SIGKILL when a host is stopped.
pub const ORCHESTRATOR_STOP_GRACE: Duration = Duration::from_secs(2);
/// How long a `chat` or `claude` app must stay alive after spawn before it is considered up.
const STARTUP_LIVENESS: Duration = Duration::from_millis(200);
/// Longest stdout line retained while waiting for an app's listening URL.
const STARTUP_LINE_MAX: usize = 4096;

/// The local-surface `AGENTS.md` template written for every orchestrator app.
///
/// It carries the rules from `docs/design/v0.4/local-orchestrator.md` §5.7: a local surface,
/// project-relative paths, a home that cannot see the Mac's local files, `session_resume` when
/// the tool is available, an explicit yes before public actions, no secret reads, and no tunnel
/// because the agent is the transport. It deliberately does not promise MCP tool calls: an
/// opencode 2.0.28 server can report `connected` without exposing its tools to the model.
pub const ORCHESTRATOR_LOCAL_INSTRUCTIONS: &str = r#"# VOWP local surface

You are on a local surface: an app on the owner's Mac, paired to one VOWP session on the home.
This file is your standing instruction, not a job brief.

## Paths

- Paths are project-relative. The project checkout is the working directory; write and report
  relative paths.
- The home cannot see the Mac's local files. Do not ask it to open a local path; send the bytes
  or a description through a VOWP tool instead.

## Working with the owner

- Call `session_resume` if the tool is available when you need the session capsule or the last
  turns. If it is not available, continue here and say which path you used.
- Ask before public actions. Push, publish, deploy or change repository visibility only after an
  explicit yes in the current turn: state the repository or remote, branch, files and expected
  effect, then wait.
- Never read a secret value, token, key or `.env` file. Refer to secrets by name only.

## Tunnels

- Do not open a tunnel. The agent is the transport between this surface and the home; tunnels
  are refused on a local surface and the home or phone cannot reach this Mac's loopback server.

## Tools

- Some opencode 2.0.28 builds report a configured MCP server as `connected` without exposing its
  tools to the model. Do not promise a VOWP tool call. If the tools are not visible, use the
  `vowp` CLI or the `vowp chat` fallback and tell the owner which path you used.

Do not edit this file; the agent writes it at every start and resume.
"#;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One app process a host may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCommand {
    /// App whose orchestrator uses this command.
    pub app: OrchestratorApp,
    /// Program name or absolute path. It is private configuration.
    pub program: String,
    /// Arguments passed unchanged to the program.
    pub args: Vec<String>,
}

/// The materialized local record of one orchestrator.
///
/// `command`, `args`, `working_dir` and `service_json` are local settings: the home command
/// supplies the app and project, and a previously written local file keeps the private
/// command, working directory and surface binding across a stop and resume.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Orchestrator name; also the directory name.
    pub name: String,
    /// App the orchestrator runs.
    pub app: OrchestratorApp,
    /// Project the orchestrator is bound to, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectName>,
    /// Paired surface the app is attached as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// Paired surface id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceId>,
    /// Local app program. Filled with the app default when the file is first materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for the local app program.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Private local working directory for the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Private path of the app's `service.json`, for opencode-style apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_json: Option<String>,
}

impl std::fmt::Debug for OrchestratorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorConfig")
            .field("name", &self.name)
            .field("app", &self.app)
            .field("project", &self.project)
            .field("session", &self.session)
            .field("surface", &self.surface)
            .field("command", &self.command.as_ref().map(|_| "<command>"))
            .field("args", &self.args.len())
            .field("working_dir", &self.working_dir.as_ref().map(|_| "<path>"))
            .field("service_json", &self.service_json.as_ref().map(|_| "<path>"))
            .finish()
    }
}

/// The private pid and lifecycle file of one host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorHostState {
    /// Orchestrator name.
    pub name: String,
    /// App the host runs.
    pub app: OrchestratorApp,
    /// Project binding, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectName>,
    /// Surface the host is attached as, when paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceId>,
    /// Session the surface mirrors, when paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// Process-group leader pid. `None` after the app exited or was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Lifecycle state as last observed locally.
    pub state: OrchestratorNodeState,
    /// When the host last started, in RFC 3339 form.
    pub started_at: Timestamp,
    /// Redacted local explanation, never a path, port or secret value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One paired surface an app host may attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceBinding {
    /// Surface id.
    pub surface: SurfaceId,
    /// Session the surface mirrors.
    pub session: SessionId,
}

/// The subset of `agent.json` the app record needs; nothing else is read or validated.
#[derive(Deserialize)]
struct AgentNodeRecord {
    node: NodeName,
}

/// The redacted app-facing record written as `orchestrator.json`.
///
/// It never carries the app command, a working directory, the service port or password, the
/// surface token, a pair link or any path outside the agent's state directory.
#[derive(Serialize)]
struct AppRecord {
    name: String,
    app: OrchestratorApp,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<ProjectName>,
    session: SessionId,
    surface: SurfaceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<NodeName>,
}

/// The attach and detach side effects the client owns.
///
/// The host never opens a second connection: the caller sends `SurfaceAttach` and
/// `SurfaceDetach` on the one existing agent connection.
pub trait SurfaceAttachment {
    /// Attaches `binding` to the home connection.
    fn attach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError>;
    /// Detaches `binding` from the home connection.
    fn detach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError>;
}

/// A host prepared by [`OrchestratorHost::prepare_start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedHost {
    /// Request id from the home command.
    pub request_id: String,
    /// Orchestrator name.
    pub name: String,
    /// App the host runs.
    pub app: OrchestratorApp,
    /// Surface binding to attach, when the local pairing exists.
    pub binding: SurfaceBinding,
    /// Process-group leader pid.
    pub pid: u32,
    /// Local lifecycle state before the caller attaches.
    pub state: OrchestratorNodeState,
}

/// The outcome of the blocking half of a start or resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// The host process exists; the caller must attach and finish the command.
    Ready(PreparedHost),
    /// The command failed; the typed result is ready to send.
    Failed(OrchestratorResult),
}

/// A live host found during reconnect reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHost {
    /// Orchestrator name.
    pub name: String,
    /// Surface binding the caller must re-attach.
    pub binding: SurfaceBinding,
}

/// The local orchestrator host manager for one agent state directory.
#[derive(Clone)]
pub struct OrchestratorHost {
    state_dir: PathBuf,
    startup: Duration,
    stop_grace: Duration,
    command_override: Option<AppCommand>,
    /// Piped stdin handles for `chat` and `claude` hosts. Keeping the pipe open stops the
    /// interactive app from seeing immediate EOF; dropping it on stop closes the app's input.
    stdin_holds: Arc<Mutex<HashMap<String, ChildStdin>>>,
    /// Optional PATH used to resolve a default app program. Tests point it at a private
    /// directory so a fake `vowp` or `opencode` never touches the owner's real binaries.
    path_lookup: Option<PathBuf>,
}

impl OrchestratorHost {
    /// Builds a host manager below `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            startup: ORCHESTRATOR_START_TIMEOUT,
            stop_grace: ORCHESTRATOR_STOP_GRACE,
            command_override: None,
            stdin_holds: Arc::new(Mutex::new(HashMap::new())),
            path_lookup: None,
        }
    }

    /// Overrides how long a start waits for the app's `service.json`.
    pub fn with_startup(mut self, startup: Duration) -> Self {
        self.startup = startup;
        self
    }

    /// Overrides how long a stop waits after SIGTERM before SIGKILL.
    pub fn with_stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// Uses an explicit app command instead of the built-in default for `app`.
    ///
    /// This is the injectable seam for tests and for an owner who keeps the app outside
    /// `PATH`. The command is persisted in the private local config.
    pub fn with_app_command(mut self, app: OrchestratorApp, program: impl Into<String>, args: Vec<String>) -> Self {
        self.command_override = Some(AppCommand {
            app,
            program: program.into(),
            args,
        });
        self
    }

    /// Resolves a default app program against `dir` instead of the process `PATH`.
    ///
    /// The app is also spawned with this `PATH`, so a test can place a fake `vowp` or
    /// `opencode` script in a private directory and never start the owner's real binary.
    pub fn with_path_lookup(mut self, dir: impl Into<PathBuf>) -> Self {
        self.path_lookup = Some(dir.into());
        self
    }

    /// Returns `state_dir/orchestrators`.
    pub fn root(state_dir: &Path) -> PathBuf {
        state_dir.join(ORCHESTRATORS_DIR)
    }

    /// Returns one orchestrator directory, after validating `name`.
    pub fn dir(state_dir: &Path, name: &str) -> Result<PathBuf, OrchestratorError> {
        validate_name(name)?;
        Ok(Self::root(state_dir).join(name))
    }

    /// Returns one orchestrator's private config path.
    pub fn config_path(state_dir: &Path, name: &str) -> Result<PathBuf, OrchestratorError> {
        Ok(Self::dir(state_dir, name)?.join(ORCHESTRATOR_CONFIG_FILE))
    }

    /// Returns one orchestrator's private host/pid path.
    pub fn host_path(state_dir: &Path, name: &str) -> Result<PathBuf, OrchestratorError> {
        Ok(Self::dir(state_dir, name)?.join(ORCHESTRATOR_HOST_FILE))
    }

    /// Reads one materialized config, tightening a file left wider than 0600.
    pub fn config(&self, name: &str) -> Result<Option<OrchestratorConfig>, OrchestratorError> {
        let path = Self::config_path(&self.state_dir, name)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                let config: OrchestratorConfig = serde_json::from_slice(&bytes)
                    .map_err(|_| config_error("the local orchestrator config is not valid JSON"))?;
                if config.name != name {
                    return Err(config_error("the local orchestrator config names another orchestrator"));
                }
                Ok(Some(config))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(config_error("the local orchestrator config could not be read")),
        }
    }

    /// Reads one host state/pid file.
    pub fn state(&self, name: &str) -> Result<Option<OrchestratorHostState>, OrchestratorError> {
        read_host_state(&Self::host_path(&self.state_dir, name)?)
    }

    /// Prepares a `Start` or `Resume` without attaching the surface.
    ///
    /// The method may block while the app writes its `service.json`; the client runs it on a
    /// worker thread so heartbeats, runs and tool calls keep flowing. `project_root` is the
    /// local checkout for `Start.project`; `Resume` uses the persisted working directory.
    pub fn prepare_start(&self, command: &OrchestratorCommand, project_root: Option<&Path>) -> PrepareOutcome {
        let request_id = command.request_id().to_owned();
        let name = command.name().to_owned();
        if let Err(error) = validate_name(&name) {
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }

        let config = match command {
            OrchestratorCommand::Start { app, project, .. } => {
                match self.materialize_config(&name, *app, project.as_ref(), project_root) {
                    Ok(config) => config,
                    Err(error) => {
                        return PrepareOutcome::Failed(failed_result(
                            &request_id,
                            OrchestratorNodeState::Failed,
                            error,
                        ));
                    }
                }
            }
            OrchestratorCommand::Resume { .. } => match self.config(&name) {
                Ok(Some(config)) => config,
                Ok(None) => {
                    return PrepareOutcome::Failed(failed_result(
                        &request_id,
                        OrchestratorNodeState::Failed,
                        OrchestratorError::new("not_found", "there is no local record for this orchestrator", false),
                    ));
                }
                Err(error) => {
                    return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
                }
            },
            OrchestratorCommand::Stop { .. } => {
                return PrepareOutcome::Failed(failed_result(
                    &request_id,
                    OrchestratorNodeState::Failed,
                    OrchestratorError::new("invalid_command", "stop is not a start command", false),
                ));
            }
        };

        // A live process is the authority: a duplicate Start/Resume reports the existing
        // host instead of spawning a second one.
        match self.state(&name) {
            Ok(Some(state)) if state.pid.is_some_and(process_alive) => {
                let binding = match binding_from(config.session.clone(), config.surface.clone()) {
                    Ok(binding) => binding,
                    Err(error) => {
                        return PrepareOutcome::Failed(failed_result(
                            &request_id,
                            OrchestratorNodeState::Degraded,
                            error,
                        ));
                    }
                };
                return PrepareOutcome::Ready(PreparedHost {
                    request_id,
                    name,
                    app: config.app,
                    binding,
                    // invariant: the guard above proved `pid` is `Some`.
                    pid: state.pid.unwrap_or_default(),
                    state: state.state,
                });
            }
            Ok(Some(_)) => {
                self.release_stdin(&name);
                let _ = fs::remove_file(Self::host_path(&self.state_dir, &name).unwrap_or_default());
            }
            Ok(None) => {}
            Err(error) => {
                return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
            }
        }

        let mut config = config;
        if let Err(error) = self.check_default_capability(&config) {
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }
        let binding = match resolve_binding(&self.state_dir, &mut config) {
            Ok(binding) => binding,
            Err(error) => {
                return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
            }
        };
        // `vowp chat --agent` needs the session id it mirrors. An older local config may
        // have been written before the argument existed, so the resolved binding supplies
        // it again on every start.
        if self.uses_default_chat_command(&config) {
            config.args = vec!["chat".to_owned(), "--agent".to_owned(), binding.session.to_string()];
        }
        if let Err(error) = self.write_config(&config) {
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }
        // The surface binding is resolved before the process starts, so the app can read its
        // MCP config and standing instruction from its first request.
        if let Err(error) = self.write_app_files(&config, &binding) {
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }
        let program = config.command.clone().unwrap_or_default();
        let args = config.args.clone();
        let working_dir = PathBuf::from(config.working_dir.clone().unwrap_or_default());
        let service_json = config.service_json.as_ref().map(PathBuf::from);
        // A stopped app leaves its old service.json behind. Remove it so the start waits
        // for the new process rather than reading the previous run's file.
        if let Some(path) = service_json.as_deref() {
            let _ = fs::remove_file(path);
        }
        let startup = self.startup;
        let state_dir = self.state_dir.clone();
        let name = name.clone();
        let capture_output = config.app == OrchestratorApp::Opencode || service_json.is_some();

        self.release_stdin(&name);
        let spawned = match spawn_app(
            &program,
            &args,
            &working_dir,
            &state_dir,
            &name,
            config.app,
            capture_output,
            self.path_lookup.as_deref(),
        ) {
            Ok(spawned) => spawned,
            Err(error) => {
                return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
            }
        };
        let SpawnedApp {
            mut child,
            startup: mut capture,
        } = spawned;
        let pid = child.id();
        // `chat` and `claude` are interactive: a missing binary or an immediate exit is a
        // failed start, not a host that silently reports `Starting` and then disappears.
        if matches!(config.app, OrchestratorApp::Chat | OrchestratorApp::ClaudeCode)
            && let Err(error) = wait_for_initial_liveness(&mut child, config.app, STARTUP_LIVENESS)
        {
            stop_process(pid, Duration::from_millis(0));
            let _ = child.wait();
            let _ = fs::remove_file(Self::host_path(&state_dir, &name).unwrap_or_default());
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }
        let started_at = Timestamp::now();
        let starting = OrchestratorHostState {
            name: name.clone(),
            app: config.app,
            project: config.project.clone(),
            surface: Some(binding.surface.clone()),
            session: Some(binding.session.clone()),
            pid: Some(pid),
            state: OrchestratorNodeState::Starting,
            started_at,
            detail: None,
        };
        if let Err(error) = write_host_state(&state_dir, &starting) {
            stop_process(pid, Duration::from_millis(0));
            let _ = child.wait();
            return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
        }

        if config.app == OrchestratorApp::Opencode || service_json.is_some() {
            let Some(service_json) = service_json.as_deref() else {
                stop_process(pid, Duration::from_millis(0));
                let _ = child.wait();
                let _ = fs::remove_file(Self::host_path(&state_dir, &name).unwrap_or_default());
                return PrepareOutcome::Failed(failed_result(
                    &request_id,
                    OrchestratorNodeState::Failed,
                    config_error("the opencode host has no local service.json path"),
                ));
            };
            if let Err(error) = wait_for_service(service_json, &mut child, startup, capture.as_ref()) {
                stop_process(pid, Duration::from_millis(0));
                let _ = child.wait();
                let _ = fs::remove_file(Self::host_path(&state_dir, &name).unwrap_or_default());
                return PrepareOutcome::Failed(failed_result(&request_id, OrchestratorNodeState::Failed, error));
            }
        }
        // The app may have printed its readiness before this point; keep the pipe reader
        // draining but stop parsing now that startup is over.
        drop(capture.take());
        self.hold_stdin(&name, child.stdin.take());

        // Reap the child so a crashed app does not leave a zombie; the pid file and the
        // process-group signal are the authorities after this point.
        let _ = thread::Builder::new()
            .name(format!("vowp-orchestrator-reaper-{pid}"))
            .spawn(move || {
                let _ = child.wait();
            });

        PrepareOutcome::Ready(PreparedHost {
            request_id,
            name,
            app: config.app,
            binding,
            pid,
            state: OrchestratorNodeState::Starting,
        })
    }

    /// Attaches a prepared host and writes its `running` state.
    ///
    /// A duplicate command for an already-running host attaches only when the host was
    /// `degraded`; a fresh start attaches once. A failed attach stops a fresh process and
    /// leaves no partial host behind.
    pub fn finish_start(&self, outcome: PrepareOutcome, link: &mut dyn SurfaceAttachment) -> OrchestratorResult {
        let prepared = match outcome {
            PrepareOutcome::Failed(result) => return result,
            PrepareOutcome::Ready(prepared) => prepared,
        };
        let already_attached = prepared.state == OrchestratorNodeState::Running;
        if !already_attached && let Err(error) = link.attach(&prepared.binding) {
            return self.fail_prepared(&prepared, error);
        }
        // Refresh once the surface is bound: a resumed host whose binding changed now carries
        // the new surface id in opencode.json and orchestrator.json.
        if !already_attached && let Err(error) = self.refresh_app_files(&prepared.name, &prepared.binding) {
            return self.fail_prepared(&prepared, error);
        }
        if let Err(error) = self.set_state(&prepared.name, OrchestratorNodeState::Running, None) {
            self.abort(&prepared.name);
            return failed_result(&prepared.request_id, OrchestratorNodeState::Failed, error);
        }
        let mut result = OrchestratorResult::accepted(&prepared.request_id, OrchestratorNodeState::Running);
        result.detail = Some(if already_attached {
            format!("{} host is already running", app_name(prepared.app))
        } else {
            format!("{} host is running", app_name(prepared.app))
        });
        result
    }

    /// Starts or resumes one host and attaches it in one call.
    pub fn start(
        &self,
        command: &OrchestratorCommand,
        project_root: Option<&Path>,
        link: &mut dyn SurfaceAttachment,
    ) -> OrchestratorResult {
        let outcome = self.prepare_start(command, project_root);
        self.finish_start(outcome, link)
    }

    /// Stops only the named host and detaches its surface.
    ///
    /// The process is signalled as a group (TERM, then a bounded KILL); the agent and every
    /// other orchestrator are untouched. The config stays for a later resume.
    pub fn stop(&self, command: &OrchestratorCommand, link: &mut dyn SurfaceAttachment) -> OrchestratorResult {
        let request_id = command.request_id().to_owned();
        let name = command.name().to_owned();
        if let Err(error) = validate_name(&name) {
            return failed_result(&request_id, OrchestratorNodeState::Failed, error);
        }
        let state = match self.state(&name) {
            Ok(Some(state)) => state,
            Ok(None) => {
                let mut result = OrchestratorResult::accepted(&request_id, OrchestratorNodeState::Stopped);
                result.detail = Some("the host was not running".to_owned());
                return result;
            }
            Err(error) => return failed_result(&request_id, OrchestratorNodeState::Failed, error),
        };
        let binding = state
            .surface
            .clone()
            .zip(state.session.clone())
            .map(|(surface, session)| SurfaceBinding { surface, session });
        let mut detach_failed = false;
        if let Some(binding) = binding.as_ref()
            && link.detach(binding).is_err()
        {
            detach_failed = true;
        }
        if let Some(pid) = state.pid
            && process_alive(pid)
        {
            stop_process(pid, self.stop_grace);
            if process_alive(pid) {
                return failed_result(
                    &request_id,
                    OrchestratorNodeState::Failed,
                    OrchestratorError::new("stop_failed", "the app host did not stop after SIGKILL", true),
                );
            }
        }
        self.release_stdin(&name);
        let _ = fs::remove_file(Self::host_path(&self.state_dir, &name).unwrap_or_default());
        let mut result = OrchestratorResult::accepted(&request_id, OrchestratorNodeState::Stopped);
        result.detail = Some(if detach_failed {
            "the host stopped; the home detach frame could not be sent".to_owned()
        } else {
            "the host stopped".to_owned()
        });
        result
    }

    /// Marks every live host `degraded` after the home connection is lost.
    ///
    /// The app process keeps running; a reconnect re-attaches it without spawning another.
    pub fn mark_degraded(&self) -> Result<(), OrchestratorError> {
        for state in self.states()? {
            if !state.state.is_live() {
                continue;
            }
            let detail = match state.pid {
                Some(pid) if process_alive(pid) => {
                    "the home connection was lost; the app host keeps running".to_owned()
                }
                _ => "the app host exited while the home was disconnected".to_owned(),
            };
            let next = match state.pid {
                Some(pid) if process_alive(pid) => OrchestratorNodeState::Degraded,
                _ => OrchestratorNodeState::Failed,
            };
            self.set_state(&state.name, next, Some(detail))?;
        }
        Ok(())
    }

    /// Marks every live host `degraded` when its surface lease is lost.
    ///
    /// The app process keeps running; the host stops writing through this connection until
    /// a later `Resume` or reconnect re-attaches it.
    pub fn mark_lease_lost(&self) -> Result<(), OrchestratorError> {
        for state in self.states()? {
            if !state.state.is_live() {
                continue;
            }
            self.set_state(
                &state.name,
                OrchestratorNodeState::Degraded,
                Some("the surface lease was lost; the app host is degraded".to_owned()),
            )?;
        }
        Ok(())
    }

    /// Stops only the host attached as `surface` after that surface was revoked.
    pub fn stop_surface(&self, surface: &SurfaceId) {
        let Ok(states) = self.states() else {
            return;
        };
        for state in states {
            if state.surface.as_ref() != Some(surface) {
                continue;
            }
            if let Some(pid) = state.pid
                && process_alive(pid)
            {
                stop_process(pid, self.stop_grace);
            }
            self.release_stdin(&state.name);
            let _ = fs::remove_file(Self::host_path(&self.state_dir, &state.name).unwrap_or_default());
        }
    }

    /// Returns the live hosts a reconnect must re-attach.
    pub fn reconcile(&self) -> Result<Vec<LiveHost>, OrchestratorError> {
        let mut live = Vec::new();
        for state in self.states()? {
            let Some(pid) = state.pid else { continue };
            if !process_alive(pid) {
                let _ = self.set_state(
                    &state.name,
                    OrchestratorNodeState::Failed,
                    Some("the app host exited".to_owned()),
                );
                continue;
            }
            let (Some(surface), Some(session)) = (state.surface.clone(), state.session.clone()) else {
                continue;
            };
            live.push(LiveHost {
                name: state.name.clone(),
                binding: SurfaceBinding { surface, session },
            });
        }
        Ok(live)
    }

    /// Records that one reconciled host is attached again.
    pub fn mark_running(&self, name: &str) -> Result<(), OrchestratorError> {
        self.set_state(name, OrchestratorNodeState::Running, None)
    }

    /// Stops every host this agent started.
    ///
    /// `clean` removes the orchestrator directories (node revoke); otherwise the private
    /// config stays so a later `Resume` can start the app again (agent exit).
    pub fn stop_all(&self, clean: bool) {
        self.stdin_holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let Ok(entries) = fs::read_dir(Self::root(&self.state_dir)) else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(name) = dir.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_name(name).is_err() {
                continue;
            }
            if let Ok(Some(state)) = self.state(name)
                && let Some(pid) = state.pid
                && process_alive(pid)
            {
                stop_process(pid, self.stop_grace);
            }
            if clean {
                let _ = fs::remove_dir_all(&dir);
            } else {
                let _ = fs::remove_file(dir.join(ORCHESTRATOR_HOST_FILE));
            }
        }
    }

    /// Stops and removes every host after the node credential is revoked.
    pub fn revoke(&self) {
        self.stop_all(true);
    }

    fn materialize_config(
        &self,
        name: &str,
        app: OrchestratorApp,
        project: Option<&ProjectName>,
        project_root: Option<&Path>,
    ) -> Result<OrchestratorConfig, OrchestratorError> {
        let dir = Self::dir(&self.state_dir, name)?;
        ensure_private_dir(&dir).map_err(|_| config_error("the orchestrator directory could not be created"))?;
        let mut config = self.config(name)?.unwrap_or_else(|| OrchestratorConfig {
            name: name.to_owned(),
            app,
            project: project.cloned(),
            session: None,
            surface: None,
            command: None,
            args: Vec::new(),
            working_dir: None,
            service_json: None,
        });
        config.app = app;
        config.project = project.cloned();
        if config.command.is_none() {
            let (program, args) = self.default_command(app);
            config.command = Some(program);
            config.args = args;
        }
        if let Some(root) = project_root {
            config.working_dir = Some(root.display().to_string());
        }
        if config.working_dir.is_none() {
            if project.is_some() {
                return Err(import_error(
                    "project_not_mapped",
                    "the project has no local checkout on this node",
                ));
            }
            config.working_dir = Some(dir.display().to_string());
        }
        let working = config.working_dir.as_deref().unwrap_or_default();
        let working = Path::new(working);
        if !working.is_absolute()
            || working
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(config_error(
                "the local working directory must be absolute without traversal",
            ));
        }
        if !working.is_dir() {
            return Err(import_error(
                "project_not_mapped",
                "the project has no local checkout on this node",
            ));
        }
        if app == OrchestratorApp::Opencode && config.service_json.is_none() {
            let xdg = dir.join(ORCHESTRATOR_XDG_DIR);
            config.service_json = Some(
                xdg.join("state")
                    .join("opencode")
                    .join("service.json")
                    .display()
                    .to_string(),
            );
        }
        self.write_config(&config)?;
        Ok(config)
    }

    fn write_config(&self, config: &OrchestratorConfig) -> Result<(), OrchestratorError> {
        let path = Self::config_path(&self.state_dir, &config.name)?;
        let bytes = serde_json::to_vec_pretty(config)
            .map_err(|_| config_error("the local orchestrator config could not be encoded"))?;
        write_atomic_private(&path, &bytes)
            .map_err(|_| config_error("the local orchestrator config could not be written"))
    }

    /// Writes the app-facing files for `config` and its bound surface.
    ///
    /// The files live in the same 0600/0700 orchestrator directory as `config.json` and are
    /// written through the atomic private helper, so a start or resume is idempotent and a
    /// reader never sees a partial file. An opencode app gets `opencode.json`; every app gets
    /// `AGENTS.md` and the redacted `orchestrator.json`.
    fn write_app_files(&self, config: &OrchestratorConfig, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        let dir = Self::dir(&self.state_dir, &config.name)?;
        write_atomic_private(
            &dir.join(ORCHESTRATOR_AGENTS_FILE),
            ORCHESTRATOR_LOCAL_INSTRUCTIONS.as_bytes(),
        )
        .map_err(|_| config_error("the local app instructions could not be written"))?;
        let record = AppRecord {
            name: config.name.clone(),
            app: config.app,
            project: config.project.clone(),
            session: binding.session.clone(),
            surface: binding.surface.clone(),
            node: self.node_name(),
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|_| config_error("the local app record could not be encoded"))?;
        write_atomic_private(&dir.join(ORCHESTRATOR_RECORD_FILE), &bytes)
            .map_err(|_| config_error("the local app record could not be written"))?;
        if config.app == OrchestratorApp::Opencode {
            let socket = self.state_dir.join(AGENT_SOCKET_FILE).display().to_string();
            let instructions = dir.join(ORCHESTRATOR_AGENTS_FILE).display().to_string();
            let value = serde_json::json!({
                "mcp": {
                    "vowp": {
                        "type": "local",
                        "command": [
                            "vowp",
                            "mcp",
                            "--agent-socket",
                            socket,
                            "--surface",
                            binding.surface.0,
                        ],
                        "enabled": true,
                    }
                },
                "instructions": [instructions],
            });
            let bytes = serde_json::to_vec_pretty(&value)
                .map_err(|_| config_error("the local opencode config could not be encoded"))?;
            write_atomic_private(&dir.join(ORCHESTRATOR_OPENCODE_FILE), &bytes)
                .map_err(|_| config_error("the local opencode config could not be written"))?;
        }
        Ok(())
    }

    /// Rewrites the app-facing files after a surface attach, using the persisted config.
    fn refresh_app_files(&self, name: &str, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        let config = self
            .config(name)?
            .ok_or_else(|| config_error("the local orchestrator config is missing"))?;
        self.write_app_files(&config, binding)
    }

    /// Reads only the node name from `agent.json`, or `None` before enrollment.
    fn node_name(&self) -> Option<NodeName> {
        let bytes = fs::read(AgentConfig::file_path(&self.state_dir)).ok()?;
        let record: AgentNodeRecord = serde_json::from_slice(&bytes).ok()?;
        Some(record.node)
    }

    fn default_command(&self, app: OrchestratorApp) -> (String, Vec<String>) {
        if let Some(command) = &self.command_override
            && command.app == app
        {
            return (command.program.clone(), command.args.clone());
        }
        match app {
            OrchestratorApp::Opencode => ("opencode".to_owned(), vec!["serve".to_owned()]),
            OrchestratorApp::ClaudeCode => ("claude".to_owned(), Vec::new()),
            OrchestratorApp::Chat => ("vowp".to_owned(), vec!["chat".to_owned(), "--agent".to_owned()]),
        }
    }

    /// Returns whether `config` still names the built-in `vowp chat --agent` command.
    fn uses_default_chat_command(&self, config: &OrchestratorConfig) -> bool {
        let overridden = self
            .command_override
            .as_ref()
            .is_some_and(|command| command.app == config.app);
        !overridden && config.app == OrchestratorApp::Chat && config.command.as_deref() == Some("vowp")
    }

    /// Refuses a default app program that is not installed before a process is spawned.
    ///
    /// The check resolves a bare program name on `PATH`; it never runs `<program> --version`,
    /// so a test or a fake app script is never started just to probe it. A custom command and
    /// an absolute path keep the existing `spawn_failed` behavior.
    fn check_default_capability(&self, config: &OrchestratorConfig) -> Result<(), OrchestratorError> {
        if self
            .command_override
            .as_ref()
            .is_some_and(|command| command.app == config.app)
        {
            return Ok(());
        }
        let Some(program) = config.command.as_deref() else {
            return Ok(());
        };
        if program.contains('/') {
            return Ok(());
        }
        let (default_program, _) = self.default_command(config.app);
        if program != default_program {
            return Ok(());
        }
        if resolve_program(program, self.path_lookup.as_deref()).is_some() {
            return Ok(());
        }
        Err(OrchestratorError::new(
            "capability_missing",
            format!("the local {program} binary is not installed or not on PATH"),
            false,
        ))
    }

    /// Keeps a piped stdin handle open for one interactive app host.
    fn hold_stdin(&self, name: &str, stdin: Option<ChildStdin>) {
        let mut holds = self
            .stdin_holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        holds.remove(name);
        if let Some(stdin) = stdin {
            holds.insert(name.to_owned(), stdin);
        }
    }

    /// Drops the piped stdin handle that kept one interactive app host alive.
    fn release_stdin(&self, name: &str) {
        self.stdin_holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }

    /// Stops or degrades a prepared host after a failed attach or app-file refresh.
    fn fail_prepared(&self, prepared: &PreparedHost, error: OrchestratorError) -> OrchestratorResult {
        if prepared.state == OrchestratorNodeState::Starting {
            self.abort(&prepared.name);
        } else {
            let _ = self.set_state(
                &prepared.name,
                OrchestratorNodeState::Degraded,
                Some(error.message.clone()),
            );
        }
        let state = if prepared.state == OrchestratorNodeState::Starting {
            OrchestratorNodeState::Failed
        } else {
            OrchestratorNodeState::Degraded
        };
        failed_result(&prepared.request_id, state, error)
    }

    /// Stops a fresh process and removes its state after a failed attach.
    fn abort(&self, name: &str) {
        if let Ok(Some(state)) = self.state(name)
            && let Some(pid) = state.pid
        {
            stop_process(pid, self.stop_grace);
        }
        self.release_stdin(name);
        let _ = fs::remove_file(Self::host_path(&self.state_dir, name).unwrap_or_default());
    }

    fn set_state(
        &self,
        name: &str,
        state: OrchestratorNodeState,
        detail: Option<String>,
    ) -> Result<(), OrchestratorError> {
        let Some(mut current) = self.state(name)? else {
            return Ok(());
        };
        current.state = state;
        current.detail = detail.map(|text| redact_detail(&text));
        if state == OrchestratorNodeState::Stopped || state == OrchestratorNodeState::Failed {
            current.pid = None;
        }
        write_host_state(&self.state_dir, &current)
    }

    fn states(&self) -> Result<Vec<OrchestratorHostState>, OrchestratorError> {
        let entries = match fs::read_dir(Self::root(&self.state_dir)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(config_error("the orchestrator directory could not be read")),
        };
        let mut states = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_name(&name).is_err() {
                continue;
            }
            if let Some(state) = self.state(&name)? {
                states.push(state);
            }
        }
        Ok(states)
    }
}

/// Validates a name used as one directory below `orchestrators/`.
pub fn validate_name(name: &str) -> Result<(), OrchestratorError> {
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if safe {
        Ok(())
    } else {
        Err(OrchestratorError::new(
            "invalid_name",
            "invalid orchestrator name; use letters, digits, '.', '_' or '-'",
            false,
        ))
    }
}

fn resolve_binding(state_dir: &Path, config: &mut OrchestratorConfig) -> Result<SurfaceBinding, OrchestratorError> {
    if let (Some(surface), Some(session)) = (config.surface.clone(), config.session.clone()) {
        validate_surface_id(&surface).map_err(|_| config_error("the configured surface id is not a ULID"))?;
        return Ok(SurfaceBinding { surface, session });
    }
    if config.surface.is_some() || config.session.is_some() {
        return Err(config_error("the local config has only half of a surface binding"));
    }
    let mut found = Vec::new();
    let dir = AgentConfig::surfaces_dir(state_dir);
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(surface) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let surface = SurfaceId(surface.to_owned());
            if validate_surface_id(&surface).is_err() {
                continue;
            }
            let session = match fs::read_to_string(path.join(crate::config::SURFACE_SESSION_FILE)) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let Ok(session) = SessionId::parse(session.trim()) else {
                continue;
            };
            found.push(SurfaceBinding { surface, session });
        }
    }
    match found.len() {
        0 => Err(import_error(
            "surface_missing",
            "pair a surface for this orchestrator before starting it",
        )),
        1 => {
            // invariant: `found` has exactly one element here.
            let binding = found.pop().unwrap_or(SurfaceBinding {
                surface: SurfaceId(String::new()),
                session: SessionId::new(),
            });
            config.surface = Some(binding.surface.clone());
            config.session = Some(binding.session.clone());
            Ok(binding)
        }
        _ => Err(OrchestratorError::new(
            "surface_ambiguous",
            "more than one paired surface could own this orchestrator; set one in the local config",
            false,
        )),
    }
}

fn binding_from(session: Option<SessionId>, surface: Option<SurfaceId>) -> Result<SurfaceBinding, OrchestratorError> {
    match (surface, session) {
        (Some(surface), Some(session)) => Ok(SurfaceBinding { surface, session }),
        _ => Err(config_error("the local config has no surface binding")),
    }
}

/// Resolves a bare program name on `path`, or the process `PATH` when none is given.
fn resolve_program(program: &str, path: Option<&Path>) -> Option<PathBuf> {
    if program.contains('/') {
        let path = Path::new(program);
        return is_executable_file(path).then(|| path.to_path_buf());
    }
    let path = path
        .map(|dir| dir.as_os_str().to_owned())
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// A freshly spawned app and its optional startup-output reader.
struct SpawnedApp {
    child: Child,
    startup: Option<StartupCapture>,
}

/// One readiness event printed by an app before it accepts requests.
enum StartupEvent {
    Listening(String),
    Password(String),
}

/// The bounded, in-memory view of an app's startup output.
struct StartupCapture {
    events: mpsc::Receiver<StartupEvent>,
    active: Arc<AtomicBool>,
}

impl Drop for StartupCapture {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_app(
    program: &str,
    args: &[String],
    working_dir: &Path,
    state_dir: &Path,
    name: &str,
    app: OrchestratorApp,
    capture_output: bool,
    path_lookup: Option<&Path>,
) -> Result<SpawnedApp, OrchestratorError> {
    if program.is_empty() || program.contains('\0') || program.chars().any(char::is_control) {
        return Err(import_error("config_invalid", "the local app command is not usable"));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(import_error(
            "config_invalid",
            "a local app argument contains a NUL byte",
        ));
    }
    let dir = OrchestratorHost::dir(state_dir, name)?;
    let xdg = dir.join(ORCHESTRATOR_XDG_DIR);
    for leaf in ["config", "data", "state", "cache"] {
        ensure_private_dir(&xdg.join(leaf))
            .map_err(|_| config_error("the private app directory could not be created"))?;
    }
    let interactive = matches!(app, OrchestratorApp::Chat | OrchestratorApp::ClaudeCode);
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(working_dir)
        .env("XDG_CONFIG_HOME", xdg.join("config"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_STATE_HOME", xdg.join("state"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .env("VOWP_ORCHESTRATOR_APP", app_name(app))
        .stdin(if interactive { Stdio::piped() } else { Stdio::null() })
        .stdout(if capture_output { Stdio::piped() } else { Stdio::null() })
        .stderr(if capture_output { Stdio::piped() } else { Stdio::null() })
        .process_group(0);
    if app == OrchestratorApp::Opencode {
        // opencode reads its global `opencode.json` and `AGENTS.md` from this directory. The
        // app's working directory stays the project checkout, which is never written to.
        command.env(ORCHESTRATOR_CONFIG_DIR_ENV, &dir);
    }
    if app == OrchestratorApp::Chat {
        // `vowp chat --agent` must use the same agent state as the host that started it.
        command.env("VOWP_AGENT_HOME", state_dir);
    }
    if let Some(path) = path_lookup
        && !program.contains('/')
    {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|_| OrchestratorError::new("spawn_failed", "the app command could not be started", true))?;
    let startup = if capture_output {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (sender, events) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(true));
        let mut readers_ok = true;
        if let Some(stdout) = stdout {
            let sender = sender.clone();
            let active = Arc::clone(&active);
            let thread = thread::Builder::new()
                .name(format!("vowp-orchestrator-stdout-{name}"))
                .spawn(move || read_startup_stdout(stdout, sender, active));
            readers_ok &= thread.is_ok();
        }
        if let Some(stderr) = stderr {
            let thread = thread::Builder::new()
                .name(format!("vowp-orchestrator-stderr-{name}"))
                .spawn(move || drain_output(stderr));
            readers_ok &= thread.is_ok();
        }
        drop(sender);
        if !readers_ok {
            let _ = child.kill();
            let _ = child.wait();
            return Err(OrchestratorError::new(
                "spawn_failed",
                "the app output could not be read",
                true,
            ));
        }
        Some(StartupCapture { events, active })
    } else {
        None
    };
    Ok(SpawnedApp { child, startup })
}

/// Reads bounded stdout lines until EOF and reports only startup readiness.
fn read_startup_stdout(mut reader: impl Read, sender: mpsc::Sender<StartupEvent>, active: Arc<AtomicBool>) {
    let mut line = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return,
        };
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                report_startup_line(&line, &sender, &active);
                line.clear();
            } else if line.len() < STARTUP_LINE_MAX {
                line.push(*byte);
            }
        }
    }
    if !line.is_empty() {
        report_startup_line(&line, &sender, &active);
    }
}

fn report_startup_line(line: &[u8], sender: &mpsc::Sender<StartupEvent>, active: &AtomicBool) {
    if !active.load(Ordering::Relaxed) {
        return;
    }
    let text = String::from_utf8_lossy(line);
    if let Some(url) = parse_listening_line(&text) {
        let _ = sender.send(StartupEvent::Listening(url));
    } else if let Some(password) = parse_password_line(&text) {
        let _ = sender.send(StartupEvent::Password(password));
    }
}

/// Drains a pipe so the child never blocks on a full OS buffer.
fn drain_output(mut reader: impl Read) {
    let mut chunk = [0u8; 4096];
    while reader.read(&mut chunk).is_ok_and(|read| read > 0) {}
}

fn parse_listening_line(line: &str) -> Option<String> {
    let line = line.trim();
    let rest = line
        .strip_prefix("server listening on ")
        .or_else(|| line.strip_prefix("listening on "))?;
    let url = rest.split_whitespace().next()?;
    is_loopback_url(url).then(|| url.to_owned())
}

fn parse_password_line(line: &str) -> Option<String> {
    let secret = line.trim().strip_prefix("server password ")?.trim();
    (!secret.is_empty() && secret.len() <= STARTUP_LINE_MAX).then(|| secret.to_owned())
}

/// Returns whether `url` is a loopback HTTP URL with a usable non-zero port.
fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = rest.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "localhost") && port.trim_end_matches('/').parse::<u16>().is_ok_and(|port| port != 0)
}

/// Waits for an app-written `service.json` or an app-printed loopback URL.
fn wait_for_service(
    path: &Path,
    child: &mut Child,
    timeout: Duration,
    capture: Option<&StartupCapture>,
) -> Result<(), OrchestratorError> {
    let pid = child.id();
    let deadline = Instant::now() + timeout;
    let mut invalid_since: Option<Instant> = None;
    let mut url: Option<String> = None;
    let mut password: Option<String> = None;
    let mut materialized: Option<(String, Option<String>)> = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return Err(OrchestratorError::new(
                    "app_exited",
                    "the app exited before it became healthy",
                    true,
                ));
            }
            Ok(None) => {}
            Err(_) => {
                return Err(OrchestratorError::new(
                    "app_exited",
                    "the app exit status could not be read",
                    true,
                ));
            }
        }
        match read_service(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.code == "service_missing" => invalid_since = None,
            Err(error) if error.code == "config_invalid" && materialized.is_none() => {
                // The app may be writing the file while it is read. Retry a short window
                // before treating genuinely malformed content as a failed start.
                let started = *invalid_since.get_or_insert_with(Instant::now);
                if started.elapsed() >= SERVICE_INVALID_GRACE {
                    return Err(error);
                }
            }
            // A file materialized from stdout is rewritten once the password line arrives;
            // its temporary missing password is not an app failure.
            Err(_) if materialized.is_some() => {}
            Err(error) => return Err(error),
        }
        if let Some(capture) = capture {
            while let Ok(event) = capture.events.try_recv() {
                match event {
                    StartupEvent::Listening(found) => url = Some(found),
                    StartupEvent::Password(secret) => password = Some(secret),
                }
            }
            if let Some(found) = url.as_deref() {
                let key = (found.to_owned(), password.clone());
                if materialized.as_ref() != Some(&key) {
                    materialize_service(path, found, password.as_deref(), pid)?;
                    materialized = Some(key);
                }
                if read_service(path).is_ok() {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(OrchestratorError::new(
                "service_timeout",
                "the app did not become healthy before the start timeout",
                true,
            ));
        }
        thread::sleep(ORCHESTRATOR_POLL);
    }
}

/// Writes the private `service.json` an app only printed on stdout.
fn materialize_service(path: &Path, url: &str, password: Option<&str>, pid: u32) -> Result<(), OrchestratorError> {
    let port = url_port(url).ok_or_else(|| config_error("the app printed no usable loopback port"))?;
    let mut value = serde_json::Map::new();
    value.insert("version".to_owned(), serde_json::Value::String("stdout".to_owned()));
    value.insert("url".to_owned(), serde_json::Value::String(url.to_owned()));
    value.insert("port".to_owned(), serde_json::Value::from(port));
    value.insert("pid".to_owned(), serde_json::Value::from(pid));
    if let Some(password) = password {
        value.insert("password".to_owned(), serde_json::Value::String(password.to_owned()));
    }
    let bytes = serde_json::to_vec_pretty(&serde_json::Value::Object(value))
        .map_err(|_| config_error("the app service state could not be encoded"))?;
    write_atomic_private(path, &bytes).map_err(|_| config_error("the app service state could not be written"))
}

/// Fails a `chat` or `claude` start when the process exits during its first moments.
fn wait_for_initial_liveness(
    child: &mut Child,
    app: OrchestratorApp,
    window: Duration,
) -> Result<(), OrchestratorError> {
    let deadline = Instant::now() + window;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return Err(OrchestratorError::new(
                    "app_exited",
                    format!("the {} app exited during startup", app_name(app)),
                    false,
                ));
            }
            Ok(None) => {}
            Err(_) => {
                return Err(OrchestratorError::new(
                    "app_exited",
                    format!("the {} app exited during startup", app_name(app)),
                    false,
                ));
            }
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// One app service state. Deliberately not `Debug`: `password` is a secret.
#[derive(Deserialize)]
struct ServiceState {
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    password: String,
}

fn read_service(path: &Path) -> Result<(), OrchestratorError> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OrchestratorError::new("service_missing", "the app has not written service.json yet", true)
        } else {
            config_error("the app service.json could not be read")
        }
    })?;
    if !metadata.is_file() {
        return Err(config_error("the app service.json is not a regular file"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(config_error("the app service.json is wider than 0600"));
    }
    let bytes = fs::read(path).map_err(|_| config_error("the app service.json could not be read"))?;
    let state: ServiceState =
        serde_json::from_slice(&bytes).map_err(|_| config_error("the app service.json is not valid JSON"))?;
    if state.password.is_empty() {
        return Err(config_error("the app service.json has no service password"));
    }
    let port = state
        .port
        .filter(|port| *port != 0)
        .or_else(|| state.url.as_deref().and_then(url_port))
        .ok_or_else(|| config_error("the app service.json has no usable loopback port"))?;
    if let Some(url) = state.url.as_deref()
        && !(url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:"))
    {
        return Err(config_error("the app service.json does not name a loopback URL"));
    }
    let _ = port;
    Ok(())
}

fn url_port(url: &str) -> Option<u16> {
    let (_, port) = url.rsplit_once(':')?;
    let port = port.trim_end_matches('/');
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

fn read_host_state(path: &Path) -> Result<Option<OrchestratorHostState>, OrchestratorError> {
    match fs::read(path) {
        Ok(bytes) => {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| config_error("the local host state is not valid JSON"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(config_error("the local host state could not be read")),
    }
}

fn write_host_state(state_dir: &Path, state: &OrchestratorHostState) -> Result<(), OrchestratorError> {
    let path = OrchestratorHost::host_path(state_dir, &state.name)?;
    let mut state = state.clone();
    state.detail = state.detail.as_deref().map(redact_detail);
    let bytes =
        serde_json::to_vec_pretty(&state).map_err(|_| config_error("the local host state could not be encoded"))?;
    write_atomic_private(&path, &bytes).map_err(|_| config_error("the local host state could not be written"))
}

fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    let parent = path
        .parent()
        .ok_or_else(|| AgentError::Config("a private file has no parent directory".to_owned()))?;
    ensure_private_dir(parent)?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".tmp-{}-{counter}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn failed_result(request_id: &str, state: OrchestratorNodeState, error: OrchestratorError) -> OrchestratorResult {
    OrchestratorResult::failed(request_id, state, error)
}

fn config_error(message: &str) -> OrchestratorError {
    OrchestratorError::new("config_invalid", message, false)
}

fn import_error(code: &str, message: &str) -> OrchestratorError {
    OrchestratorError::new(code, message, false)
}

/// Returns the stable CLI-ish name of an app.
pub fn app_name(app: OrchestratorApp) -> &'static str {
    match app {
        OrchestratorApp::Opencode => "opencode",
        OrchestratorApp::ClaudeCode => "claude-code",
        OrchestratorApp::Chat => "chat",
    }
}

/// Redacts absolute paths and obvious secret assignments from free text.
pub fn redact_detail(text: &str) -> String {
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        let bare = word.trim_start_matches(['(', '[', '{', '\'', '"']);
        let lower = bare.to_ascii_lowercase();
        let looks_like_secret = lower.contains('=')
            && ["token", "password", "secret", "apikey", "api_key", "code"]
                .iter()
                .any(|key| lower.contains(key));
        if bare.starts_with('/') || bare.starts_with("~/") {
            words.push("<path>".to_owned());
        } else if looks_like_secret {
            words.push("<redacted>".to_owned());
        } else {
            words.push(word.to_owned());
        }
    }
    words.join(" ").chars().take(512).collect()
}

/// Returns whether `pid` names a live process (not this process and not pid 1).
fn process_alive(pid: u32) -> bool {
    if !valid_pid(pid) || pid == std::process::id() {
        return false;
    }
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid != std::process::id()
}

/// Stops one app process group: TERM, a bounded wait, then KILL and a short reap wait.
fn stop_process(pid: u32, grace: Duration) {
    if !process_alive(pid) {
        return;
    }
    let _ = signal_group(pid, "-TERM");
    if wait_for_exit(pid, grace) {
        return;
    }
    let _ = signal_group(pid, "-KILL");
    let _ = wait_for_exit(pid, stop_kill_wait());
}

fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn stop_kill_wait() -> Duration {
    Duration::from_secs(1)
}

fn signal_group(pgid: u32, signal: &str) -> Result<(), AgentError> {
    if !valid_pid(pgid) {
        return Err(AgentError::Config(
            "refusing to signal an invalid process group".to_owned(),
        ));
    }
    if own_process_group() == Some(pgid) {
        return Err(AgentError::Config(
            "refusing to signal this process's own group".to_owned(),
        ));
    }
    let output = Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pgid}"))
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such process") {
        return Ok(());
    }
    Err(AgentError::Config(
        "the app process group could not be signalled".to_owned(),
    ))
}

fn own_process_group() -> Option<u32> {
    #[cfg(target_os = "linux")]
    if let Some(pgid) = own_process_group_from_proc() {
        return Some(pgid);
    }
    own_process_group_from_ps()
}

#[cfg(target_os = "linux")]
fn own_process_group_from_proc() -> Option<u32> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 1..)?;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    fields.next()?.parse().ok()
}

fn own_process_group_from_ps() -> Option<u32> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps").args(["-o", "pgid=", "-p", &pid]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_refuse_traversal_and_absolute_paths() {
        for name in ["", ".", "..", "../x", "a/b", "/tmp/x", "a\\b", "a\0b", "é"] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        for name in ["mac", "mac-studio", "a.b_c-1"] {
            assert!(validate_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn redaction_hides_paths_and_assignments() {
        let text = redact_detail("open /srv/owner/x password=abc token=def");
        assert!(!text.contains("/srv"), "{text}");
        assert!(!text.contains("abc"), "{text}");
        assert!(!text.contains("def"), "{text}");
    }
}
