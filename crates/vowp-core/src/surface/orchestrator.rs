//! Orchestrator sessions and their app hosts.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.6. An orchestrator is one
//! project-less or project-bound session with role [`SessionRole::Orchestrator`], a
//! project-local directory under `~/.vowp/orchestrators/<name>/`, the app's own config
//! files (never the user's global app config) and, for the app hosts, one live surface
//! lease. The `vowp chat` app is a fallback with no background host: its join command
//! starts the chat surface directly.
//!
//! State lives in `orchestrator.json` (mode 0600). It is a superset of
//! [`OrchestratorCard`] so the fleet snapshot reads the same file: the card fields plus
//! `executor` and `created_at`. The app host keeps its own `host.json`; a running card is
//! only reported as running while that host's pid is alive.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_adapters::proc::{Signal, signal_group};
use vowp_protocol::{
    AgentFrame, CapsuleTrigger, EndpointAccess, EventType, ExecutorRef, JoinInstructions, Lease, Level, NewSession,
    NodeName, NodePresence, NodeStatus, OrchestratorApp, OrchestratorAttach, OrchestratorCard, OrchestratorCommand,
    OrchestratorEndpoint, OrchestratorError, OrchestratorLocation, OrchestratorNodeState, OrchestratorResult,
    OwnershipMode, Project, ProjectName, SessionId, SessionRole, Surface, SurfaceId, SurfaceKind, Timestamp,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{Store, StoreError, hash_lease_token};

use super::claude_remote::ClaudeRemoteHost;
use super::lease::{AcquireLease, LeaseGuard, STALE_AFTER};
use super::mirror::{self, MirrorCursor};
use super::native::{AppHandle, AppHost, HostRequest, process_alive, write_private_json};
use super::opencode_host::OpencodeHost;
use crate::CoreError;
use crate::agent::dispatch;
use crate::deps::Deps;
use crate::session::service::SessionService;

/// The instructions file written into every orchestrator directory.
///
/// The text lives in this crate so `cargo package -p vowp-core` carries it. The
/// repository copy in `framework/templates/ORCHESTRATOR.md` stays byte-identical and
/// a `vowp-cli` test checks that the two do not drift.
pub const ORCHESTRATOR_INSTRUCTIONS: &str = r#"# VOWP orchestrator

You are the owner's VOWP orchestrator. This directory belongs to one orchestrator session; the
session itself lives on the VOWP home and is shared by every surface the owner uses (the opencode
desktop app, a Claude app, or `vowp chat`). This file is your standing instruction, not a job brief.

## Start every turn with the real picture

1. Read `vowp fleet status --json` (or the home's `snapshot.json`), then `vowp fleet queue --json`
   and `vowp fleet inbox --json`. Read `vowp capacity report --json` when capacity, waiting work or
   a brake could matter.
2. Say what is running, waiting, blocked or failed, with the times from the snapshot. Do not invent
   status, counts or completion.
3. Say what you propose and why, then ask. The owner decides.
4. Apply only after an explicit yes, through the VOWP tools or the matching `vowp` command.

## How to talk to the owner

- Plain sentences, short paragraphs, times in the home's timezone. No wide tables and no colour
  requirement; the owner may be on a phone over SSH, or in the Claude mobile app for Remote Control.
- A phone join is app-specific: run the SSH `join.phone` command for opencode and `vowp chat`, and
  open the Claude mobile app for Claude Remote Control.
- Report a failure with its exact command or tool, the exit status and the useful tail of the output.
- When the owner asks to see something, use the `vowp tunnel open` command printed in `join.desktop`
  **on the control machine**. This home cannot open tunnels itself; `tunnel_open` and
  `vowp tunnel open` refuse here. Ask the owner to run the control-machine command and give you the
  local address, then close it on the control machine when the owner is done.
- When the owner changes app, device or model, continue the same session. The new surface gets the
  capsule and the last turns from `session_resume`; the old surface becomes read-only and says so.

## Hard limits

- Never open, query or edit a database, `orchestrator.json`, a capsule or a session log by hand.
  Use the commands and tools.
- Never read, print, log or commit a secret value, token, key or `.env` file. Refer to secrets by
  name only; use `vowp secret request` for a value the owner must enter.
- Push, publish, deploy or change repository visibility only after explicit owner confirmation in
  the current turn. State the repository/remote, branch/tag, files and expected effect, then wait
  for a clear yes. Do not rely on standing or previous approval. Report the result after acting.
  The supervisor still merges worker branches; workers do not push directly.
- A write tool that refuses because the session moved is final. Do not retry it with another token
  or another surface; tell the owner and continue read-only here.
- Never claim a tool, app join, tunnel or job is working without checking it.

## VOWP tools

When the app exposes the VOWP MCP server, prefer these tools over shell parsing: `fleet_status`,
`fleet_queue`, `inbox_list`, `inbox_answer`, `job_add`, `job_pause`, `job_retry`, `capacity_report`,
`capacity_set`, `session_tail`, `session_handoff`, `session_resume`, `secret_request`.
`tunnel_open` and `tunnel_close` exist only on a control-machine surface; do not promise them from
the home.

Some opencode 2.0.28 builds report the server `connected` without exposing its tools to the model.
The config and the server are still real; if you cannot see the tools, use the `vowp` CLI or the
`vowp chat` fallback and tell the owner which path you used.

## This directory

- `orchestrator.json` records the name, app, session id, bound project, executor and state.
- `opencode.json` or `.mcp.json` registers the `vowp mcp` server for this app, using the lease token
  in `lease.token`.
- `host.json` belongs to the running app host; `mirror.json` is the transcript cursor.

Do not edit these files. `vowp orchestrator stop` and `vowp orchestrator resume` manage them.
"#;

/// The persisted orchestrator state file.
const STATE_FILE: &str = "orchestrator.json";

/// The lease token file referenced by the app's MCP server config.
const LEASE_FILE: &str = "lease.token";

/// The app host's state file (written by the host implementation).
const HOST_FILE: &str = "host.json";

/// The transcript cursor written for a running orchestrator.
const MIRROR_FILE: &str = "mirror.json";

/// The pid file of the long-lived lease keeper.
pub const KEEPER_PID_FILE: &str = "keeper.pid";

/// How long the keeper waits between checks of its pid file and lease.
const KEEPER_TICK: Duration = Duration::from_secs(1);

/// Home directory holding one command frame per node and request.
const OUTBOX_DIR: &str = "agent-commands";

/// Home directory holding one result frame per node and request.
const INBOX_DIR: &str = "agent-results";

/// Poll interval while a command waits for its result file.
const COMMAND_POLL: Duration = Duration::from_millis(20);

/// Default time a home command waits for the live agent's result before treating the
/// command as durably queued.
pub const COMMAND_RESULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Counter that makes a private temporary file name unique within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How the home delivers orchestrator commands to a node's live agent connection.
///
/// The production [`CommandSpool`] writes the `AgentFrame` a live connection would carry
/// and waits briefly for the correlated result. Tests inject a fake stream that records
/// the frames and answers without a network, tmux or app.
pub trait OrchestratorTransport: Send + Sync {
    /// Delivers one command to `node`.
    ///
    /// `Ok(Some(result))` is the correlated agent result. `Ok(None)` means the command is
    /// durably queued for the node's next live connection; the caller records `starting`
    /// and applies the result later with [`Orchestrator::apply_result`].
    fn send(&self, node: &NodeName, command: &OrchestratorCommand) -> Result<Option<OrchestratorResult>, CoreError>;
}

/// The home's per-node command outbox and result inbox.
///
/// Layout below the home root:
///
/// ```text
/// agent-commands/<node>/<request_id>.json   one AgentFrame::OrchestratorCommand
/// agent-results/<node>/<request_id>.json    one AgentFrame::OrchestratorResult
/// ```
///
/// A live gateway connection drains the outbox, sends the frame to the agent and writes the
/// correlated result back. The files carry the same frame the connection carries: no port,
/// password, pairing link, absolute path or secret value. They are 0600 in a 0700 directory.
#[derive(Debug, Clone)]
pub struct CommandSpool {
    root: PathBuf,
    timeout: Duration,
}

impl CommandSpool {
    /// Builds a spool below `root` with the default result timeout.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            timeout: COMMAND_RESULT_TIMEOUT,
        }
    }

    /// Builds a spool with an explicit result timeout.
    pub fn with_timeout(root: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            root: root.into(),
            timeout,
        }
    }

    /// Returns the per-node outbox directory.
    fn outbox(&self, node: &NodeName) -> PathBuf {
        self.root.join(OUTBOX_DIR).join(node.as_str())
    }

    /// Returns the per-node result inbox directory.
    fn inbox(&self, node: &NodeName) -> PathBuf {
        self.root.join(INBOX_DIR).join(node.as_str())
    }

    /// Returns the commands waiting for a live connection, oldest request id first.
    ///
    /// A command whose result already exists is skipped: the waiting home command or an
    /// earlier delivery already completed it. A missing outbox is an empty queue.
    pub fn pending(&self, node: &NodeName) -> Result<Vec<OrchestratorCommand>, CoreError> {
        let outbox = self.outbox(node);
        let entries = match fs::read_dir(&outbox) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: outbox.display().to_string(),
                    source,
                });
            }
        };
        let inbox = self.inbox(node);
        let mut commands = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: outbox.display().to_string(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let frame: AgentFrame = serde_json::from_slice(&bytes)
                .map_err(|error| CoreError::Invalid(format!("{}: {error}", path.display())))?;
            let AgentFrame::OrchestratorCommand(command) = frame else {
                return Err(CoreError::Invalid(format!(
                    "{} does not contain an orchestrator command",
                    path.display()
                )));
            };
            if inbox.join(format!("{}.json", command.request_id())).exists() {
                continue;
            }
            commands.push(command);
        }
        commands.sort_by(|left, right| left.request_id().cmp(right.request_id()));
        Ok(commands)
    }

    /// Retires one queued command without publishing an unsolicited result.
    ///
    /// A result that no record names as its latest command is stale: the owner already
    /// moved on to a newer request id. Removing the outbox file keeps the next connection
    /// from re-sending a superseded command. A missing file is already retired.
    pub fn retire(&self, node: &NodeName, request_id: &str) -> Result<(), CoreError> {
        if !safe_request_id(request_id) {
            return Err(CoreError::Invalid(format!(
                "invalid orchestrator request id {request_id:?}"
            )));
        }
        let path = self.outbox(node).join(format!("{request_id}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CoreError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Publishes one agent result and retires its command.
    ///
    /// The result file is written before the command is removed, so an interrupted write
    /// leaves the command queued for the next connection instead of losing it.
    pub fn complete(&self, node: &NodeName, result: &OrchestratorResult) -> Result<(), CoreError> {
        if !safe_request_id(&result.request_id) {
            return Err(CoreError::Invalid(format!(
                "invalid orchestrator request id {:?}",
                result.request_id
            )));
        }
        let inbox = self.inbox(node);
        create_private_dir(&inbox)?;
        let result_path = inbox.join(format!("{}.json", result.request_id));
        write_spool_frame(&result_path, &AgentFrame::OrchestratorResult(result.clone()))?;
        let _ = fs::remove_file(self.outbox(node).join(format!("{}.json", result.request_id)));
        Ok(())
    }
}

/// Returns whether a request id is a plain file name component.
fn safe_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= 128
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl OrchestratorTransport for CommandSpool {
    fn send(&self, node: &NodeName, command: &OrchestratorCommand) -> Result<Option<OrchestratorResult>, CoreError> {
        let outbox = self.outbox(node);
        let inbox = self.inbox(node);
        create_private_dir(&outbox)?;
        create_private_dir(&inbox)?;
        let request_id = command.request_id();
        if !safe_request_id(request_id) {
            return Err(CoreError::Invalid(format!(
                "invalid orchestrator request id {request_id:?}"
            )));
        }
        let command_path = outbox.join(format!("{request_id}.json"));
        let result_path = inbox.join(format!("{request_id}.json"));
        write_spool_frame(&command_path, &AgentFrame::OrchestratorCommand(command.clone()))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            match fs::read(&result_path) {
                Ok(bytes) => {
                    let frame: AgentFrame = serde_json::from_slice(&bytes)
                        .map_err(|error| CoreError::Invalid(format!("{}: {error}", result_path.display())))?;
                    let _ = fs::remove_file(&result_path);
                    let _ = fs::remove_file(&command_path);
                    return match frame {
                        AgentFrame::OrchestratorResult(result) => {
                            if result.request_id == request_id {
                                Ok(Some(sanitize_result(result)))
                            } else {
                                Err(CoreError::Invalid(format!(
                                    "orchestrator result {} does not match request {request_id}",
                                    result.request_id
                                )))
                            }
                        }
                        AgentFrame::Error { message, retryable, .. } => Ok(Some(OrchestratorResult::failed(
                            request_id,
                            OrchestratorNodeState::Failed,
                            OrchestratorError::new("agent_error", redact_text(&message), retryable),
                        ))),
                        other => Err(CoreError::Invalid(format!(
                            "unexpected agent frame for orchestrator result: {other:?}"
                        ))),
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(CoreError::Io {
                        path: result_path.display().to_string(),
                        source,
                    });
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(COMMAND_POLL);
        }
    }
}

/// Writes one spool frame through a private temporary file and a rename.
fn write_spool_frame(path: &Path, frame: &AgentFrame) -> Result<(), CoreError> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    write_private_json(&temp, frame)?;
    fs::rename(&temp, path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// A request to create an orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub struct NewOrchestrator {
    /// Orchestrator name.
    pub name: String,
    /// Project to bind, when any.
    pub project: Option<ProjectName>,
    /// App to run.
    pub app: OrchestratorApp,
    /// Executor to use, when chosen.
    pub executor: Option<ExecutorRef>,
    /// Replace an existing orchestrator with the same name.
    pub force: bool,
}

/// One orchestrator on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct OrchestratorInfo {
    /// Snapshot card.
    pub card: OrchestratorCard,
    /// Orchestrator directory.
    pub dir: PathBuf,
    /// Session id.
    pub session: SessionId,
    /// Running host, when any.
    pub host: Option<AppHandle>,
    /// Last redacted node detail, when any.
    pub detail: Option<String>,
}

/// What one command send did to the caller's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandDelivery {
    /// The transport returned a correlated result, which was applied to the state.
    Applied,
    /// The command is durably queued for a live gateway; the state is still `starting`.
    Queued,
}

/// A held attach: the lease heartbeat plus the one-time grant.
///
/// The caller keeps this alive while the attach process runs. Dropping it, or calling
/// [`AttachGuard::release`], releases the session. There is no detached keeper.
pub struct AttachGuard {
    lease: LeaseGuard,
    grant: OrchestratorAttach,
}

impl AttachGuard {
    /// Returns the grant. It carries the lease token: never log it or put it in an error.
    pub fn grant(&self) -> &OrchestratorAttach {
        &self.grant
    }

    /// Releases the lease and stops the heartbeat.
    pub fn release(self) -> Result<(), CoreError> {
        self.lease.release()
    }
}

impl std::fmt::Debug for AttachGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachGuard").field("grant", &self.grant).finish()
    }
}

/// The orchestrator manager.
pub struct Orchestrator {
    deps: Deps,
    hosts: Vec<Arc<dyn AppHost>>,
    transport: Arc<dyn OrchestratorTransport>,
}

impl Orchestrator {
    /// Creates a manager over `deps` with the real opencode and Claude Code hosts.
    pub fn new(deps: Deps) -> Self {
        let transport = Arc::new(CommandSpool::new(deps.paths.root()));
        Self::with_transport(
            deps,
            vec![Arc::new(OpencodeHost::default()), Arc::new(ClaudeRemoteHost::default())],
            transport,
        )
    }

    /// Creates a manager that only queues commands for a live gateway to drain.
    ///
    /// The gateway uses this for `node_reconnected`: it must not block the connection for
    /// the full [`COMMAND_RESULT_TIMEOUT`] while the agent runs the command. The result is
    /// applied later by [`Orchestrator::apply_result`].
    pub fn new_queued(deps: Deps) -> Self {
        let transport = Arc::new(CommandSpool::with_timeout(deps.paths.root(), Duration::ZERO));
        Self::with_transport(
            deps,
            vec![Arc::new(OpencodeHost::default()), Arc::new(ClaudeRemoteHost::default())],
            transport,
        )
    }

    /// Creates a manager over `deps` with explicit app hosts and the default spool.
    ///
    /// Tests pass fakes here; production uses [`Orchestrator::new`]. A missing host for
    /// an app that needs one is refused by [`Orchestrator::create`] and
    /// [`Orchestrator::resume`].
    pub fn with_hosts(deps: Deps, hosts: Vec<Arc<dyn AppHost>>) -> Self {
        let transport = Arc::new(CommandSpool::new(deps.paths.root()));
        Self::with_transport(deps, hosts, transport)
    }

    /// Creates a manager with explicit app hosts and an injected command transport.
    ///
    /// Hermetic tests use this to feed the home a fake agent frame stream.
    pub fn with_transport(deps: Deps, hosts: Vec<Arc<dyn AppHost>>, transport: Arc<dyn OrchestratorTransport>) -> Self {
        Self { deps, hosts, transport }
    }

    /// Creates and starts an orchestrator on the home.
    pub fn create(&self, req: &NewOrchestrator) -> Result<OrchestratorInfo, CoreError> {
        self.create_on_node(req, None)
    }

    /// Creates an orchestrator whose app may run on `node`.
    ///
    /// An unknown or revoked node is refused before any directory or session exists. An
    /// active node that is offline gets the home record in `node_offline` and is never
    /// started on the home. An online node receives a typed start command; the card and
    /// the persisted state carry the node, the request id and the resulting state.
    pub fn create_on_node(&self, req: &NewOrchestrator, node: Option<NodeName>) -> Result<OrchestratorInfo, CoreError> {
        validate_name(&req.name)?;
        let dir = self.dir(&req.name);
        if dir.exists() {
            if !req.force {
                return Err(CoreError::Store(StoreError::Conflict(format!(
                    "orchestrator {} already exists in {}; pass --force to replace it",
                    req.name,
                    dir.display()
                ))));
            }
            self.remove_existing(&req.name)?;
        }

        // Validate the node before creating a directory or a session: an unknown,
        // pending or revoked node leaves no half-made record behind.
        let remote_card = match node.as_ref() {
            Some(node) if !self.is_home_node(node)? => Some(self.require_active_node(node)?),
            _ => None,
        };
        let card_node = node.clone();

        let global = Store::open_global(&self.deps.paths)?;
        self.prepare_project(&global, req.project.as_ref())?;
        drop(global);

        create_private_dir(&dir)?;
        let mut scoped = self.scoped_deps(req.project.as_ref())?;
        // A remote-node orchestrator session lives on the home but records the node whose
        // app will run it, so a later pairing on that node is authorized.
        if let Some(card) = remote_card.as_ref() {
            scoped.node = card.name.clone();
        }
        let service = SessionService::new(scoped.clone());
        let cwd = if remote_card.is_some() {
            // The node resolves its own local root. The home never stores or shows the
            // absolute path in a card, frame, error or log.
            PathBuf::from(".")
        } else {
            dir.clone()
        };
        let session = service.create_session(NewSession {
            project: req.project.clone(),
            job_id: None,
            role: SessionRole::Orchestrator,
            title: format!("orchestrator {}", req.name),
            cwd,
            parent: None,
            executor: req.executor.as_ref().map(|executor| executor.executor.clone()),
        })?;

        if let Some(card) = remote_card {
            return self.create_on_remote_node(req, card, session, dir);
        }

        let surface = self.surface(&req.name, req.app, &dir);
        let surface_id = surface.id.clone();
        let token_path = dir.join(LEASE_FILE);
        self.write_app_files(&dir, req.app, &session, &surface.id, &token_path)?;

        let store = scoped.open_store()?;
        let mut token = None;
        if req.app != OrchestratorApp::Chat {
            let guard = LeaseGuard::acquire(
                &scoped,
                &store,
                &AcquireLease {
                    session: session.clone(),
                    surface,
                    takeover: false,
                },
            )?;
            let held = guard.token().to_owned();
            if let Err(error) = write_private_text(&token_path, &held) {
                drop(guard);
                return Err(error);
            }
            // The CLI process exits after `orchestrator new`; the lease row and token
            // file are what the app's `vowp mcp` server checks. This is the same one-shot
            // pattern as the hidden `vowp session lease --acquire`.
            token = Some(held);
            std::mem::forget(guard);
        }

        let host = self.host_for(req.app)?;
        let config = self.config_path(&dir, req.app);
        let handle = if let Some(host) = host.as_ref() {
            match host.start(&HostRequest {
                name: req.name.clone(),
                session: session.clone(),
                port: None,
                workdir: dir.clone(),
                config,
            }) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    if let Some(token) = token.as_deref() {
                        let _ = store.release_lease(&session, token);
                        let _ = fs::remove_file(&token_path);
                    }
                    let join = fallback_join(req.app, &session);
                    let state = StateFile {
                        name: req.name.clone(),
                        app: req.app,
                        session_id: session.clone(),
                        project: req.project.clone(),
                        executor: req.executor.clone(),
                        state: OrchestratorNodeState::Degraded,
                        created_at: self.deps.clock.now(),
                        join: Some(join),
                        surface_id: Some(surface_id.clone()),
                        node: card_node,
                        last_request_id: None,
                        detail: Some(redact_text(&error.to_string())),
                        last_error: None,
                    };
                    let _ = write_state(&dir, &state);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let join = match (host.as_ref(), handle.as_ref()) {
            (Some(host), Some(handle)) => host
                .join_instructions(handle)
                .unwrap_or_else(|_| fallback_join(req.app, &session)),
            _ => fallback_join(req.app, &session),
        };
        let state = StateFile {
            name: req.name.clone(),
            app: req.app,
            session_id: session.clone(),
            project: req.project.clone(),
            executor: req.executor.clone(),
            state: OrchestratorNodeState::Running,
            created_at: self.deps.clock.now(),
            join: Some(join),
            surface_id: Some(surface_id),
            node: card_node,
            last_request_id: None,
            detail: None,
            last_error: None,
        };
        write_state(&dir, &state)?;
        if let (Some(host), Some(handle)) = (host.as_ref(), handle.as_ref()) {
            self.mirror(&dir, &state, host.as_ref(), handle);
        }
        Ok(info_from_state(state, dir, handle))
    }

    /// Creates the home record for an orchestrator whose app runs on `card`.
    fn create_on_remote_node(
        &self,
        req: &NewOrchestrator,
        card: vowp_protocol::NodeCard,
        session: SessionId,
        dir: PathBuf,
    ) -> Result<OrchestratorInfo, CoreError> {
        let request_id = new_request_id();
        let online = card.presence == NodePresence::Online;
        let mut state = StateFile {
            name: req.name.clone(),
            app: req.app,
            session_id: session.clone(),
            project: req.project.clone(),
            executor: req.executor.clone(),
            state: if online {
                OrchestratorNodeState::Starting
            } else {
                OrchestratorNodeState::NodeOffline
            },
            created_at: self.deps.clock.now(),
            join: Some(node_join(req.app, &card.name, &session)),
            surface_id: None,
            node: Some(card.name.clone()),
            last_request_id: Some(request_id.clone()),
            detail: if online {
                None
            } else {
                Some(format!("node {} is {}", card.name, presence_name(card.presence)))
            },
            last_error: None,
        };
        write_state(&dir, &state)?;
        if !online {
            return Ok(info_from_state(state, dir, None));
        }
        let command = OrchestratorCommand::Start {
            request_id,
            name: req.name.clone(),
            app: req.app,
            project: req.project.clone(),
        };
        let result = self.send_command(&card.name, &command, &mut state);
        // A queued command already persisted `starting` and its request id before the
        // send. Writing this in-memory copy again could overwrite a `degraded` mark that
        // the gateway wrote when the connection dropped during the wait.
        if !matches!(&result, Ok(CommandDelivery::Queued)) {
            write_state(&dir, &state)?;
        }
        match result {
            Ok(CommandDelivery::Applied) if state.state == OrchestratorNodeState::Failed => {
                Err(refused_by_node(&card.name, "start", &state))
            }
            Ok(_) => Ok(info_from_state(state, dir, None)),
            Err(error) => {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!("cannot deliver start for {} to node {}: {error}", state.name, card.name),
                );
                Ok(info_from_state(state, dir, None))
            }
        }
    }

    /// Moves an orchestrator between the home and an enrolled agent node.
    ///
    /// The session id, the home session log and the project binding are preserved: the
    /// current host is stopped and its lease released, a handoff capsule is written on the
    /// existing session when it has events beyond its creation and placement notes, and the
    /// orchestrator is re-materialized on the target bound to the same session. Moving to the placement the
    /// orchestrator already occupies is idempotent: a live record is returned unchanged and
    /// a stopped or degraded record is resumed there.
    ///
    /// The target is validated before anything is stopped. An unknown, pending, revoked or
    /// offline node, or a project that the target node has not mapped, is refused with the
    /// current host left running. A capsule failure is logged and does not abort the move,
    /// because the home session log remains the authoritative context.
    ///
    /// After the old host is stopped, the session index row follows the move: `sessions.node`
    /// and `sessions.cwd` name the target, while `last_seq`, `log_bytes`, the log and every
    /// event stay exactly as they were. A no-op move writes no row.
    pub fn move_to_node(&self, name: &str, target: Option<NodeName>) -> Result<OrchestratorInfo, CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        let target_node = match target {
            Some(node) if !self.is_home_node(&node)? => Some(node),
            _ => None,
        };
        let currently_remote = self.state_is_remote(&state)?;

        // A no-op move must not stop or restart a healthy host. A stopped record on the
        // requested placement is resumed there.
        if currently_remote == target_node.is_some()
            && (target_node.is_none() || state.node.as_ref() == target_node.as_ref())
        {
            return self.resume_or_return(name, state, dir);
        }

        let target_card = match target_node.as_ref() {
            Some(node) => {
                let card = self.require_active_node(node)?;
                // The move stops the old host before the target starts. A target that cannot
                // start now would leave no live placement, so refuse it before stopping.
                if card.presence != NodePresence::Online {
                    return Err(CoreError::Refused(format!(
                        "node_offline: node {node} is {}; `vowp orchestrator move` needs the node online",
                        presence_name(card.presence)
                    )));
                }
                let global = Store::open_global(&self.deps.paths)?;
                self.prepare_project(&global, state.project.as_ref())?;
                if let Some(project) = state.project.as_ref()
                    && !global
                        .list_node_paths(node)?
                        .iter()
                        .any(|path| path.project == *project)
                {
                    return Err(CoreError::Refused(format!(
                        "project {project} is not mapped on node {node}; add the local checkout on that node first"
                    )));
                }
                drop(global);
                Some(card)
            }
            None => None,
        };

        let old_node = state.node.clone();
        self.stop(name)?;
        let mut state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        if let Some(node) = old_node.as_ref() {
            self.release_remote_lease(&state, node)?;
        }
        self.write_move_capsule(&state);

        // The session index row is what `session ls`, the snapshot and a session lookup
        // read. Follow the placement with a location-only write so the log position and
        // the log itself do not move. A row that still names the old placement must not
        // start the target.
        let (location_node, location_cwd) = match target_card.as_ref() {
            Some(card) => (card.name.clone(), ".".to_owned()),
            None => (self.deps.node.clone(), dir.display().to_string()),
        };
        let scoped = self.scoped_deps(state.project.as_ref())?;
        let store = scoped.open_store()?;
        store
            .set_session_location(&state.session_id, &location_node, &location_cwd)
            .map_err(|error| {
                CoreError::Store(StoreError::Conflict(format!(
                    "session {} location was not updated after stopping the old host: {error}",
                    state.session_id
                )))
            })?;

        match target_card {
            Some(card) => self.start_move_on_remote_node(&dir, state, card),
            None => {
                state.node = None;
                state.surface_id = None;
                state.last_request_id = None;
                state.join = None;
                state.state = OrchestratorNodeState::Stopped;
                state.detail = None;
                state.last_error = None;
                write_state(&dir, &state)?;
                self.resume(name)
            }
        }
    }

    /// Returns a live orchestrator unchanged, or resumes one that is stopped or degraded.
    fn resume_or_return(&self, name: &str, state: StateFile, dir: PathBuf) -> Result<OrchestratorInfo, CoreError> {
        let info = self.info_from_dir(state, dir);
        if matches!(
            info.card.state,
            OrchestratorNodeState::Running | OrchestratorNodeState::Starting
        ) {
            return Ok(info);
        }
        self.resume(name)
    }

    /// Re-materializes one moved orchestrator on an active local node.
    ///
    /// `Start` is used rather than `Resume` so a node that has never hosted this
    /// orchestrator writes its own private copy. The home record keeps the existing
    /// session id; the node's paired surface must already be bound to that session.
    fn start_move_on_remote_node(
        &self,
        dir: &Path,
        mut state: StateFile,
        card: vowp_protocol::NodeCard,
    ) -> Result<OrchestratorInfo, CoreError> {
        let request_id = new_request_id();
        let online = card.presence == NodePresence::Online;
        state.node = Some(card.name.clone());
        state.surface_id = None;
        state.last_request_id = Some(request_id.clone());
        state.state = if online {
            OrchestratorNodeState::Starting
        } else {
            OrchestratorNodeState::NodeOffline
        };
        state.detail = if online {
            None
        } else {
            Some(format!("node {} is {}", card.name, presence_name(card.presence)))
        };
        state.last_error = None;
        state.join = Some(node_join(state.app, &card.name, &state.session_id));
        write_state(dir, &state)?;
        if !online {
            return Ok(info_from_state(state, dir.to_path_buf(), None));
        }
        let command = OrchestratorCommand::Start {
            request_id,
            name: state.name.clone(),
            app: state.app,
            project: state.project.clone(),
        };
        let result = self.send_command(&card.name, &command, &mut state);
        if !matches!(&result, Ok(CommandDelivery::Queued)) {
            write_state(dir, &state)?;
        }
        match result {
            Ok(CommandDelivery::Applied) if state.state == OrchestratorNodeState::Failed => {
                Err(refused_by_node(&card.name, "start", &state))
            }
            Ok(_) => Ok(info_from_state(state, dir.to_path_buf(), None)),
            Err(error) => {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!(
                        "cannot deliver move start for {} to node {}: {error}",
                        state.name, card.name
                    ),
                );
                Ok(info_from_state(state, dir.to_path_buf(), None))
            }
        }
    }

    /// Releases a lease left on the node an orchestrator just left.
    ///
    /// A remote host detaches its surface on `Stop`, which releases the lease through the
    /// gateway. When that node is unreachable the row can remain; release it only when it
    /// still belongs to this orchestrator's app on the old node, so a takeover by another
    /// surface is preserved. A surface paired without `--app` (the command `node_join`
    /// prints) is held by the node name, so that holder is accepted too.
    fn release_remote_lease(&self, state: &StateFile, node: &NodeName) -> Result<(), CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        let Some(lease) = store.lease(&state.session_id)? else {
            return Ok(());
        };
        let own_holder =
            lease.holder == state.name || lease.holder == app_name(state.app) || lease.holder == node.as_str();
        if lease.node == *node && own_holder {
            store.release_lease_hash(&state.session_id, &lease.token)?;
        }
        Ok(())
    }

    /// Writes a handoff capsule on the existing session after a real placement change.
    ///
    /// A session with only its creation and placement notes has no context to hand off, so
    /// no capsule turn runs. The move continues when the capsule writer fails; the session
    /// log on the home is still the authoritative record the new host resumes from.
    fn write_move_capsule(&self, state: &StateFile) {
        let result = (|| -> Result<(), CoreError> {
            let scoped = self.scoped_deps(state.project.as_ref())?;
            let store = scoped.open_store()?;
            store.require_session(&state.session_id)?;
            let events = SessionLog::open_read(&self.deps.paths, &state.session_id)?.read_all()?;
            if !events
                .iter()
                .any(|event| !matches!(event.event_type, EventType::SessionCreated | EventType::Note))
            {
                return Ok(());
            }
            SessionService::new(scoped).write_capsule(&state.session_id, CapsuleTrigger::Manual)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!(
                    "cannot write handoff capsule for {} on {}: {error}",
                    state.name, state.session_id
                ),
            );
        }
    }

    /// Lists every orchestrator.
    pub fn list(&self) -> Result<Vec<OrchestratorInfo>, CoreError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: root.display().to_string(),
                source,
            })?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if let Some(state) = read_state(&dir)? {
                found.push(self.info_from_dir(state, dir));
            }
        }
        found.sort_by(|left, right| left.card.name.cmp(&right.card.name));
        Ok(found)
    }

    /// Returns one orchestrator.
    pub fn info(&self, name: &str) -> Result<OrchestratorInfo, CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        Ok(self.info_from_dir(state, dir))
    }

    /// Returns the discovery document a local UI client uses to reach one orchestrator.
    ///
    /// The base fields always come from the home record. A node-hosted orchestrator is
    /// metadata only: this method never opens SSH, a socket or the node's files. A
    /// home-hosted orchestrator fills `url`, `native_session_id` and `auth` from the live
    /// app host's private 0600 state; a stopped host, or one with no loopback endpoint,
    /// leaves them `None` rather than guessing. Every failure names the orchestrator and
    /// the failure class, never a path, port, password, token or pair link.
    ///
    /// `owner` is read from the project-scoped store at the injected clock's `now`, for a
    /// node-hosted record too. `access` is [`EndpointAccess::Owner`] only when `surface` is
    /// the live owner's surface id; no caller identity, another surface, a stale or expired
    /// owner, or no owner all read as [`EndpointAccess::ReadOnly`]. Recorded ownership is
    /// cooperative: a UI that talks to the app's HTTP API directly is not stopped by it.
    pub fn endpoint(&self, name: &str, surface: Option<&SurfaceId>) -> Result<OrchestratorEndpoint, CoreError> {
        let (mut endpoint, project) = self.endpoint_base(name, false)?;
        let now = self.deps.clock.now();
        let store = self
            .scoped_deps(project.as_ref())?
            .open_store()
            .map_err(|_| owner_unreadable(name))?;
        let record = store
            .ownership(&endpoint.session_id)
            .map_err(|_| owner_unreadable(name))?;
        if let Some(record) = record {
            let live = record.is_live_at(now);
            if live && surface.is_some() && surface == record.lease.surface_id.as_ref() {
                endpoint.access = EndpointAccess::Owner;
            }
            endpoint.owner = Some(record.to_owner(now));
        }
        Ok(endpoint)
    }

    /// Builds the endpoint document without the owner view, and the orchestrator's project.
    ///
    /// `for_attach` turns a corrupt local record or service file into the typed attach
    /// refusal that suggests `vowp orchestrator resume`; the read path keeps its existing
    /// error text and classes.
    fn endpoint_base(
        &self,
        name: &str,
        for_attach: bool,
    ) -> Result<(OrchestratorEndpoint, Option<ProjectName>), CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_endpoint_state(name, &dir)
            .map_err(|error| {
                if for_attach {
                    attach_endpoint_unavailable(name)
                } else {
                    error
                }
            })?
            .ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        let remote = self.state_is_remote(&state)?;
        let info = self.info_from_dir(state, dir);
        let project = info.card.project.clone();
        let location = if remote {
            OrchestratorLocation::Node
        } else {
            OrchestratorLocation::Home
        };
        let mut endpoint = OrchestratorEndpoint {
            name: info.card.name.clone(),
            app: info.card.app,
            session_id: info.card.session_id.clone(),
            location,
            node: info.card.node.clone(),
            state: info.card.state,
            url: None,
            native_session_id: None,
            auth: None,
            owner: None,
            access: EndpointAccess::ReadOnly,
        };
        if remote {
            return Ok((endpoint, project));
        }
        let Some(handle) = info.host.as_ref() else {
            return Ok((endpoint, project));
        };
        let Some(host) = self.host_for(info.card.app)? else {
            return Ok((endpoint, project));
        };
        let Some(parts) = host.endpoint(handle).map_err(|_| {
            if for_attach {
                attach_endpoint_unavailable(name)
            } else {
                CoreError::Invalid(format!("orchestrator {name}: the app endpoint state is not readable"))
            }
        })?
        else {
            return Ok((endpoint, project));
        };
        endpoint.url = Some(parts.url);
        endpoint.native_session_id = parts.native_session_id;
        endpoint.auth = parts.auth;
        Ok((endpoint, project))
    }

    /// Attaches a desktop UI to one home-hosted orchestrator and returns the held grant.
    ///
    /// This is the writer path. It creates a [`SurfaceKind::DesktopUi`] surface for `holder`
    /// and acquires the session lease through [`LeaseGuard::acquire`], so a live interactive
    /// owner needs `takeover` and a stale one is replaced. A job owner that is expired, or
    /// live with `takeover`, is fenced first through [`dispatch::fence_job_owner`]: the run
    /// closes through `lose`, the job stays `ready` and the owner row is released. A live job
    /// takeover then records the capsule and the one note with [`dispatch::record_takeover`];
    /// an interactive takeover already wrote its note.
    ///
    /// A node-hosted orchestrator is refused (the home cannot heartbeat a node-local engine),
    /// and so is a stopped or corrupt host whose endpoint is unavailable. The returned
    /// [`AttachGuard`] owns the heartbeat; the caller keeps it alive while the attach process
    /// runs and releases or drops it to give the session up. No keeper is started.
    pub fn attach(
        &self,
        name: &str,
        holder: &str,
        label: Option<&str>,
        takeover: bool,
    ) -> Result<AttachGuard, CoreError> {
        let (mut endpoint, project) = self.endpoint_base(name, true)?;
        if endpoint.location == OrchestratorLocation::Node {
            return Err(CoreError::Refused(format!(
                "orchestrator {name} runs on a node; the home cannot heartbeat a node-local engine, so it cannot be attached from here"
            )));
        }
        if endpoint.url.is_none() || endpoint.auth.is_none() {
            return Err(CoreError::Refused(format!(
                "orchestrator {name} has no live app endpoint to attach to; run `vowp orchestrator resume {name}` first"
            )));
        }
        let holder = holder.trim();
        if holder.is_empty() {
            return Err(CoreError::Invalid("an attach needs a non-empty holder name".to_owned()));
        }
        let session = endpoint.session_id.clone();
        let scoped = self.scoped_deps(project.as_ref())?;
        let store = scoped.open_store()?;
        let now = self.deps.clock.now();
        let surface = Surface {
            id: SurfaceId::new(),
            kind: SurfaceKind::DesktopUi,
            node: self.deps.node.clone(),
            holder: holder.to_owned(),
            app: Some(app_name(endpoint.app).to_owned()),
            cwd: None,
            metadata: serde_json::json!({"orchestrator": name}),
            created_at: now,
            last_seen_at: now,
        };
        let surface_id = surface.id.clone();

        let mut job_takeover_from = None;
        if let Some(record) = store.ownership(&session)?
            && record.mode == OwnershipMode::Job
        {
            let live = record.is_live_at(now);
            if !live || takeover {
                dispatch::fence_job_owner(&scoped, &store, &session, holder, now)?;
                if live {
                    job_takeover_from = Some(record.lease.holder);
                }
            }
        }
        let guard = LeaseGuard::acquire(
            &scoped,
            &store,
            &AcquireLease {
                session: session.clone(),
                surface,
                takeover,
            },
        )?;
        if let Some(previous) = job_takeover_from
            && let Err(error) = dispatch::record_takeover(&scoped, &store, &session, &previous, holder, now)
        {
            // The lease already moved; a failed note must not drop the grant.
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!("attach to {name}: could not record the takeover note: {error}"),
            );
        }
        if let Some(label) = label {
            // Dropping the guard releases the row, so a bad label leaves no owner behind.
            store.set_surface_label(&surface_id, Some(label))?;
        }
        let owner = store
            .session_owner(&session, now)?
            .ok_or_else(|| CoreError::Invalid(format!("orchestrator {name}: the attach did not record an owner")))?;
        endpoint.owner = Some(owner.clone());
        endpoint.access = EndpointAccess::Owner;
        let grant = OrchestratorAttach {
            surface_id,
            token: guard.token().to_owned(),
            expires_at: owner.heartbeat_at.plus(STALE_AFTER),
            owner,
            endpoint,
        };
        Ok(AttachGuard { lease: guard, grant })
    }

    /// Stops one orchestrator's host and releases its lease. The session stays.
    ///
    /// A local-node orchestrator receives a `stop` command (the agent maps it to
    /// `SurfaceDetach`), never a home host stop and never a stop of the whole agent.
    pub fn stop(&self, name: &str) -> Result<(), CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let mut state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        self.stop_keeper(&dir);
        if self.state_is_remote(&state)? {
            let node = state
                .node
                .clone()
                .ok_or_else(|| CoreError::Invalid("remote state has no node".to_owned()))?;
            let request_id = new_request_id();
            state.last_request_id = Some(request_id.clone());
            let command = OrchestratorCommand::Stop {
                request_id,
                name: state.name.clone(),
            };
            // The owner's stop is final even when the node cannot be reached now: the
            // record is stopped and a later reconnect must not resume it.
            if self.send_command(&node, &command, &mut state).is_err() {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!(
                        "{} could not deliver stop to node {}; recorded stopped",
                        state.name, node
                    ),
                );
            }
            self.release_lease(&dir, &state.name, &state.session_id)?;
            let _ = fs::remove_file(dir.join(LEASE_FILE));
            state.state = OrchestratorNodeState::Stopped;
            state.detail = None;
            return write_state(&dir, &state);
        }
        if let Some(handle) = host_handle(&dir, state.app) {
            if let Ok(host) = self.host_for(state.app)
                && let Some(host) = host.as_ref()
            {
                host.stop(&handle)?;
            }
        }
        self.release_lease(&dir, &state.name, &state.session_id)?;
        let _ = fs::remove_file(dir.join(LEASE_FILE));
        state.state = OrchestratorNodeState::Stopped;
        state.detail = None;
        write_state(&dir, &state)
    }

    /// Starts one orchestrator's host again.
    ///
    /// A local-node orchestrator receives a typed `resume` command. When its node is
    /// offline the call fails with `node_offline` and the record is marked
    /// `node_offline`; it is never started on the home.
    pub fn resume(&self, name: &str) -> Result<OrchestratorInfo, CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let mut state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        self.stop_keeper(&dir);
        if self.state_is_remote(&state)? {
            return self.resume_on_remote_node(&dir, state);
        }
        let host = self.host_for(state.app)?;
        let config = self.config_path(&dir, state.app);
        let mut token = None;
        let mut surface_id = state.surface_id.clone();
        if state.app != OrchestratorApp::Chat {
            let scoped = self.scoped_deps(state.project.as_ref())?;
            let store = scoped.open_store()?;
            let mut surface = self.surface(&state.name, state.app, &dir);
            if let Some(id) = &state.surface_id {
                surface.id = id.clone();
            }
            let current_id = surface.id.clone();
            let guard = LeaseGuard::acquire(
                &scoped,
                &store,
                &AcquireLease {
                    session: state.session_id.clone(),
                    surface,
                    takeover: true,
                },
            )?;
            let held = guard.token().to_owned();
            let token_path = dir.join(LEASE_FILE);
            if let Err(error) = write_private_text(&token_path, &held)
                .and_then(|()| self.write_app_files(&dir, state.app, &state.session_id, &current_id, &token_path))
            {
                drop(guard);
                return Err(error);
            }
            token = Some(held);
            surface_id = Some(current_id);
            std::mem::forget(guard);
        }
        state.surface_id = surface_id;
        let current = host_handle(&dir, state.app);
        let handle = if let Some(current) = current {
            current
        } else {
            let Some(host) = host.as_ref() else {
                state.state = OrchestratorNodeState::Running;
                state.detail = None;
                write_state(&dir, &state)?;
                return Ok(info_from_state(state, dir, None));
            };
            match host.start(&HostRequest {
                name: state.name.clone(),
                session: state.session_id.clone(),
                port: None,
                workdir: dir.clone(),
                config,
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    if let Some(token) = token.as_deref() {
                        self.release_lease_with(&state.session_id, token)?;
                        let _ = fs::remove_file(dir.join(LEASE_FILE));
                    }
                    state.state = OrchestratorNodeState::Degraded;
                    state.detail = Some(redact_text(&error.to_string()));
                    let _ = write_state(&dir, &state);
                    return Err(error);
                }
            }
        };
        let join = match host.as_ref() {
            Some(host) => host
                .join_instructions(&handle)
                .unwrap_or_else(|_| fallback_join(state.app, &state.session_id)),
            None => fallback_join(state.app, &state.session_id),
        };
        state.state = OrchestratorNodeState::Running;
        state.detail = None;
        state.join = Some(join);
        write_state(&dir, &state)?;
        if let Some(host) = host.as_ref() {
            self.mirror(&dir, &state, host.as_ref(), &handle);
        }
        Ok(info_from_state(state, dir, Some(handle)))
    }

    /// Resumes every orchestrator that was live before the home restarted.
    ///
    /// A local-node orchestrator is never started on the home. If its node is offline at
    /// startup the record is marked `degraded` and the home resumes it on reconnect; a
    /// `stopped` or `failed` record stays as it is. A host that cannot start is marked
    /// `degraded` and does not fail the remaining orchestrators.
    pub fn resume_all(&self) -> Result<Vec<OrchestratorInfo>, CoreError> {
        let mut resumed = Vec::new();
        for mut state in self.states()? {
            let dir = self.dir(&state.name);
            if self.state_is_remote(&state)? {
                match self.resume_remote_after_restart(&dir, &mut state) {
                    Ok(Some(info)) => resumed.push(info),
                    Ok(None) => {}
                    Err(error) => {
                        self.deps.log.log(
                            Level::Warn,
                            "orchestrator",
                            &format!("cannot resume {}: {error}", state.name),
                        );
                    }
                }
                continue;
            }
            if state.state != OrchestratorNodeState::Running {
                continue;
            }
            // A live host is already running. Reuse it only while it still owns its
            // lease: a supervisor restart must not invalidate every app config, and it
            // must not steal a session another surface took over.
            if let Some(handle) = host_handle(&dir, state.app) {
                if !self.can_reuse_live_lease(&dir, &state)? {
                    self.deps.log.log(
                        Level::Warn,
                        "orchestrator",
                        &format!(
                            "{} is running without its lease (another surface may hold it); leaving it",
                            state.name
                        ),
                    );
                    continue;
                }
                if let Ok(Some(host)) = self.host_for(state.app) {
                    self.mirror(&dir, &state, host.as_ref(), &handle);
                    state.detail = None;
                    resumed.push(info_from_state(state, dir, Some(handle)));
                    continue;
                }
            }
            match self.resume(&state.name) {
                Ok(info) => resumed.push(info),
                Err(error) => {
                    self.deps.log.log(
                        vowp_protocol::Level::Warn,
                        "orchestrator",
                        &format!("cannot resume {}: {error}", state.name),
                    );
                }
            }
        }
        Ok(resumed)
    }

    /// Resumes one local-node orchestrator at home startup, or marks it degraded.
    fn resume_remote_after_restart(
        &self,
        dir: &Path,
        state: &mut StateFile,
    ) -> Result<Option<OrchestratorInfo>, CoreError> {
        let Some(node) = state.node.clone() else {
            return Ok(None);
        };
        match state.state {
            OrchestratorNodeState::Stopped | OrchestratorNodeState::Failed => return Ok(None),
            OrchestratorNodeState::Running
            | OrchestratorNodeState::Starting
            | OrchestratorNodeState::Degraded
            | OrchestratorNodeState::NodeOffline => {}
        }
        let online = self.node_is_active_online(&node)?;
        if !online {
            if state.state != OrchestratorNodeState::NodeOffline {
                state.state = OrchestratorNodeState::Degraded;
                state.detail = Some(format!("node {node} is offline at home startup"));
                write_state(dir, state)?;
            }
            return Ok(None);
        }
        // A `starting` record may have been queued but never applied before the home
        // restarted, so re-send `Start`; a live or degraded record gets `Resume`.
        let start = matches!(
            state.state,
            OrchestratorNodeState::NodeOffline | OrchestratorNodeState::Starting
        );
        let request_id = new_request_id();
        state.last_request_id = Some(request_id.clone());
        state.state = OrchestratorNodeState::Starting;
        state.detail = None;
        write_state(dir, state)?;
        let command = if start {
            OrchestratorCommand::Start {
                request_id,
                name: state.name.clone(),
                app: state.app,
                project: state.project.clone(),
            }
        } else {
            OrchestratorCommand::Resume {
                request_id,
                name: state.name.clone(),
            }
        };
        let result = self.send_command(&node, &command, state);
        if !matches!(&result, Ok(CommandDelivery::Queued)) {
            write_state(dir, state)?;
        }
        match result {
            Ok(_) => Ok(Some(info_from_state(state.clone(), dir.to_path_buf(), None))),
            Err(error) => {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!("cannot resume {} on node {node}: {error}", state.name),
                );
                Ok(None)
            }
        }
    }

    /// Marks every live orchestrator on `node` degraded after a connection drop.
    ///
    /// The gateway calls this when an authenticated agent connection closes. It changes
    /// only `starting`, `running` and `degraded` records; `stopped`, `failed` and
    /// `node_offline` keep their owner-visible meaning.
    pub fn mark_node_disconnected(&self, node: &NodeName) -> Result<usize, CoreError> {
        let mut changed = 0;
        for mut state in self.states()? {
            if state.node.as_ref() != Some(node) {
                continue;
            }
            if !matches!(
                state.state,
                OrchestratorNodeState::Starting | OrchestratorNodeState::Running | OrchestratorNodeState::Degraded
            ) {
                continue;
            }
            state.state = OrchestratorNodeState::Degraded;
            state.detail = Some(format!("node {node} connection dropped"));
            write_state(&self.dir(&state.name), &state)?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Resumes every degraded or offline orchestrator when `node` reconnects.
    ///
    /// The gateway calls this after a new authenticated connection is welcome. A
    /// `stopped` or `failed` record is never resumed automatically.
    pub fn node_reconnected(&self, node: &NodeName) -> Result<Vec<OrchestratorInfo>, CoreError> {
        if !self.node_is_active_online(node)? {
            return Ok(Vec::new());
        }
        let mut resumed = Vec::new();
        for mut state in self.states()? {
            if state.node.as_ref() != Some(node) {
                continue;
            }
            let start = match state.state {
                OrchestratorNodeState::Degraded => false,
                OrchestratorNodeState::NodeOffline => true,
                _ => continue,
            };
            let dir = self.dir(&state.name);
            let request_id = new_request_id();
            state.last_request_id = Some(request_id.clone());
            state.state = OrchestratorNodeState::Starting;
            state.detail = None;
            write_state(&dir, &state)?;
            let command = if start {
                OrchestratorCommand::Start {
                    request_id,
                    name: state.name.clone(),
                    app: state.app,
                    project: state.project.clone(),
                }
            } else {
                OrchestratorCommand::Resume {
                    request_id,
                    name: state.name.clone(),
                }
            };
            match self.send_command(node, &command, &mut state) {
                Ok(CommandDelivery::Queued) => {
                    // `starting` and the request id are already persisted.
                    resumed.push(info_from_state(state, dir, None));
                }
                Ok(CommandDelivery::Applied) => {
                    write_state(&dir, &state)?;
                    resumed.push(info_from_state(state, dir, None));
                }
                Err(error) => {
                    write_state(&dir, &state)?;
                    self.deps.log.log(
                        Level::Warn,
                        "orchestrator",
                        &format!("cannot resume {} on reconnect: {error}", state.name),
                    );
                }
            }
        }
        Ok(resumed)
    }

    /// Returns whether `command` is still the latest command for its record on `node`.
    ///
    /// A `stop` is always current: the owner's stop is final even when a newer start or
    /// resume was queued behind it. Any other command whose request id is no longer the
    /// record's latest one has been superseded and must be retired rather than sent.
    pub fn is_command_current(&self, node: &NodeName, command: &OrchestratorCommand) -> Result<bool, CoreError> {
        if matches!(command, OrchestratorCommand::Stop { .. }) {
            return Ok(true);
        }
        let state = match read_state(&self.dir(command.name())) {
            Ok(Some(state)) => state,
            // A record that is absent has no command waiting for it. A momentarily
            // unreadable record is not proof of supersession; sending is safe because the
            // correlated result decides.
            Ok(None) => return Ok(false),
            Err(_) => return Ok(true),
        };
        Ok(state.node.as_ref() == Some(node) && state.last_request_id.as_deref() == Some(command.request_id()))
    }

    /// Applies one agent result to the record with the matching node and request id.
    ///
    /// The gateway calls this when a live connection delivers an
    /// `AgentFrame::OrchestratorResult` after the command was queued; the home never
    /// applies a result with another request id.
    pub fn apply_result(
        &self,
        node: &NodeName,
        result: &OrchestratorResult,
    ) -> Result<Option<OrchestratorInfo>, CoreError> {
        let result = sanitize_result(result.clone());
        for mut state in self.states()? {
            if state.node.as_ref() != Some(node) || state.last_request_id.as_deref() != Some(result.request_id.as_str())
            {
                continue;
            }
            apply_result_to_state(&mut state, &result);
            let dir = self.dir(&state.name);
            write_state(&dir, &state)?;
            return Ok(Some(info_from_state(state, dir, None)));
        }
        Ok(None)
    }

    /// Sends a resume command to a local-node orchestrator and updates the record.
    fn resume_on_remote_node(&self, dir: &Path, mut state: StateFile) -> Result<OrchestratorInfo, CoreError> {
        let node = state
            .node
            .clone()
            .ok_or_else(|| CoreError::Invalid("local-node orchestrator has no node".to_owned()))?;
        let card = self.require_active_node(&node)?;
        if card.presence != NodePresence::Online {
            state.state = OrchestratorNodeState::NodeOffline;
            state.detail = Some(format!("node {node} is {}", presence_name(card.presence)));
            write_state(dir, &state)?;
            return Err(CoreError::Refused(format!(
                "node_offline: node {node} is {}; `vowp orchestrator resume` waits for the node to reconnect",
                presence_name(card.presence)
            )));
        }
        let request_id = new_request_id();
        state.last_request_id = Some(request_id.clone());
        state.state = OrchestratorNodeState::Starting;
        state.detail = None;
        write_state(dir, &state)?;
        let command = OrchestratorCommand::Resume {
            request_id,
            name: state.name.clone(),
        };
        let result = self.send_command(&node, &command, &mut state);
        if !matches!(&result, Ok(CommandDelivery::Queued)) {
            write_state(dir, &state)?;
        }
        result?;
        if state.state == OrchestratorNodeState::Failed {
            return Err(refused_by_node(&node, "resume", &state));
        }
        Ok(info_from_state(state, dir.to_path_buf(), None))
    }

    /// Sends one command and folds a synchronous result into `state`.
    fn send_command(
        &self,
        node: &NodeName,
        command: &OrchestratorCommand,
        state: &mut StateFile,
    ) -> Result<CommandDelivery, CoreError> {
        match self.transport.send(node, command) {
            Ok(Some(result)) => {
                apply_result_to_state(state, &sanitize_result(result));
                Ok(CommandDelivery::Applied)
            }
            Ok(None) => {
                // The caller already wrote `starting` before sending. Leave the field as
                // it is: the gateway may have marked the record `degraded` on a disconnect
                // while this command waited, and this process must not overwrite that.
                Ok(CommandDelivery::Queued)
            }
            Err(error) => {
                state.state = if is_node_offline_error(&error) {
                    OrchestratorNodeState::NodeOffline
                } else {
                    OrchestratorNodeState::Failed
                };
                state.detail = Some(redact_text(&error.to_string()));
                Err(error)
            }
        }
    }

    /// Returns whether `state` is bound to a node other than the home.
    fn state_is_remote(&self, state: &StateFile) -> Result<bool, CoreError> {
        match state.node.as_ref() {
            Some(node) => Ok(!self.is_home_node(node)?),
            None => Ok(false),
        }
    }

    /// Returns whether `node` is this home.
    ///
    /// The home is the local identity in `deps`, or a registry row marked `is_local` (for
    /// example the `local` row a fresh home creates). A node orchestrator bound to the home
    /// keeps the home host, lease and keeper path.
    pub fn is_home_node(&self, node: &NodeName) -> Result<bool, CoreError> {
        if *node == self.deps.node {
            return Ok(true);
        }
        let store = Store::open_global(&self.deps.paths)?;
        Ok(store.node(node)?.is_some_and(|row| row.is_local))
    }

    /// Returns an active node card, refusing unknown, pending and revoked nodes.
    fn require_active_node(&self, node: &NodeName) -> Result<vowp_protocol::NodeCard, CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        let Some(card) = store.node_card(node, self.deps.clock.now())? else {
            return Err(CoreError::NotFound(format!("node {node}")));
        };
        match card.status {
            NodeStatus::Active => Ok(card),
            NodeStatus::Pending => Err(CoreError::Refused(format!(
                "node {node} is pending; approve the enrollment first"
            ))),
            NodeStatus::Revoked => Err(CoreError::Refused(format!(
                "node {node} is revoked; enroll it again before creating an orchestrator"
            ))),
        }
    }

    /// Returns whether `node` is active and online at the current clock.
    fn node_is_active_online(&self, node: &NodeName) -> Result<bool, CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        Ok(store
            .node_card(node, self.deps.clock.now())?
            .is_some_and(|card| card.status == NodeStatus::Active && card.presence == NodePresence::Online))
    }

    /// Returns whether a live host still owns the lease named by its token file.
    ///
    /// `resume_all` uses this to leave an already-running orchestrator and its keeper
    /// untouched; only a missing or replaced lease needs a takeover.
    fn can_reuse_live_lease(&self, dir: &Path, state: &StateFile) -> Result<bool, CoreError> {
        if state.app == OrchestratorApp::Chat {
            return Ok(true);
        }
        let token = match fs::read_to_string(dir.join(LEASE_FILE)) {
            Ok(token) => token.trim().to_owned(),
            Err(_) => return Ok(false),
        };
        if token.is_empty() {
            return Ok(false);
        }
        let scoped = self.scoped_deps(state.project.as_ref())?;
        let store = scoped.open_store()?;
        let token_hash = hash_lease_token(&token);
        Ok(store
            .lease(&state.session_id)?
            .is_some_and(|lease| lease.token == token_hash))
    }

    /// Keeps one orchestrator's lease alive until its pid file changes or it is revoked.
    ///
    /// `vowp orchestrator new|resume` starts this as a small detached process, because the
    /// CLI itself exits and a [`LeaseGuard`] heartbeat thread cannot outlive it. The keeper
    /// reuses the token file rather than taking over, so it never writes a switch or a
    /// takeover note. It refuses a row whose token differs (the owner moved the session),
    /// and it exits when `keeper.pid` is removed or names another process.
    pub fn keep_alive(&self, name: &str) -> Result<(), CoreError> {
        validate_name(name)?;
        let dir = self.dir(name);
        let state = read_state(&dir)?.ok_or_else(|| CoreError::NotFound(format!("orchestrator {name}")))?;
        if state.app == OrchestratorApp::Chat {
            return Err(CoreError::Invalid("the chat app has no host to keep alive".to_owned()));
        }
        if self.state_is_remote(&state)? {
            return Err(CoreError::Refused(format!(
                "orchestrator {name} runs on a local node; the home keeps no lease for it"
            )));
        }
        let scoped = self.scoped_deps(state.project.as_ref())?;
        let store = scoped.open_store()?;
        let token_path = dir.join(LEASE_FILE);
        let token = fs::read_to_string(&token_path)
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                CoreError::Refused(format!("orchestrator {name} has no lease token file; the keeper stops"))
            })?;
        match store.lease(&state.session_id)? {
            Some(lease) if lease.token != hash_lease_token(&token) => {
                return Err(CoreError::Refused(format!(
                    "orchestrator {name} moved to another surface; the keeper stops"
                )));
            }
            Some(_) => {
                store.heartbeat_lease(&state.session_id, &token, self.deps.clock.now())?;
            }
            None => {
                let mut surface = self.surface(&state.name, state.app, &dir);
                if let Some(id) = &state.surface_id {
                    surface.id = id.clone();
                }
                store.upsert_surface(&surface)?;
                store.take_lease(&Lease {
                    session_id: state.session_id.clone(),
                    surface_id: Some(surface.id.clone()),
                    surface_kind: surface_kind(state.app),
                    node: self.deps.node.clone(),
                    holder: state.name.clone(),
                    token: token.clone(),
                    acquired_at: self.deps.clock.now(),
                    heartbeat_at: self.deps.clock.now(),
                })?;
            }
        }

        let pid = std::process::id().to_string();
        let pid_path = dir.join(KEEPER_PID_FILE);
        if let Err(error) = write_private_text(&pid_path, &pid) {
            let _ = store.release_lease(&state.session_id, &token);
            return Err(error);
        }
        loop {
            thread::sleep(KEEPER_TICK);
            let still_ours = fs::read_to_string(&pid_path)
                .ok()
                .is_some_and(|text| text.trim() == pid);
            if !still_ours || !store.heartbeat_lease(&state.session_id, &token, self.deps.clock.now())? {
                break;
            }
        }
        let _ = fs::remove_file(&pid_path);
        store.release_lease(&state.session_id, &token)?;
        Ok(())
    }

    /// Stops a running lease keeper, if any.
    fn stop_keeper(&self, dir: &Path) {
        let pid_path = dir.join(KEEPER_PID_FILE);
        let pid = match fs::read_to_string(&pid_path) {
            Ok(text) => text.trim().parse::<u32>().ok(),
            Err(_) => None,
        };
        if let Some(pid) = pid
            && process_alive(pid)
        {
            let _ = signal_group(pid, Signal::Term);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if !process_alive(pid) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if process_alive(pid) {
                let _ = signal_group(pid, Signal::Kill);
            }
        }
        let _ = fs::remove_file(&pid_path);
    }

    /// Returns every persisted state, sorted by name.
    fn states(&self) -> Result<Vec<StateFile>, CoreError> {
        let root = self.root();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        let mut states = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: root.display().to_string(),
                source,
            })?;
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(state) = read_state(&entry.path())? {
                states.push(state);
            }
        }
        states.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(states)
    }

    /// Returns the orchestrator root (`~/.vowp/orchestrators`).
    fn root(&self) -> PathBuf {
        self.deps.paths.root().join("orchestrators")
    }

    /// Returns one orchestrator's directory.
    fn dir(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    /// Builds an [`OrchestratorInfo`] from persisted state and a live host, if any.
    fn info_from_dir(&self, state: StateFile, dir: PathBuf) -> OrchestratorInfo {
        let host = host_handle(&dir, state.app);
        let mut card = state.card();
        if self.state_is_remote(&state).unwrap_or(false) {
            if let Some(node) = card.node.as_ref()
                && matches!(
                    card.state,
                    OrchestratorNodeState::Starting | OrchestratorNodeState::Running
                )
                && !self.node_is_active_online(node).unwrap_or(false)
            {
                card.state = OrchestratorNodeState::Degraded;
            }
        } else if card.state == OrchestratorNodeState::Running && state.app != OrchestratorApp::Chat && host.is_none() {
            card.state = OrchestratorNodeState::Degraded;
        }
        OrchestratorInfo {
            card,
            dir,
            session: state.session_id,
            host,
            detail: state.detail,
        }
    }

    /// Returns the app host for one app, when the app has a host.
    fn host_for(&self, app: OrchestratorApp) -> Result<Option<Arc<dyn AppHost>>, CoreError> {
        if app == OrchestratorApp::Chat {
            return Ok(None);
        }
        self.hosts
            .iter()
            .find(|host| host.kind() == app)
            .cloned()
            .map(Some)
            .ok_or_else(|| CoreError::Refused(format!("no app host is configured for {}", app_name(app))))
    }

    /// Builds the surface row for the orchestrator's app.
    fn surface(&self, name: &str, app: OrchestratorApp, dir: &Path) -> Surface {
        let now = self.deps.clock.now();
        Surface {
            id: SurfaceId::new(),
            kind: surface_kind(app),
            node: self.deps.node.clone(),
            holder: name.to_owned(),
            app: Some(app_name(app).to_owned()),
            cwd: Some(dir.display().to_string()),
            metadata: serde_json::json!({"orchestrator": name}),
            created_at: now,
            last_seen_at: now,
        }
    }

    /// Writes the instructions file and the app's project-local MCP config.
    fn write_app_files(
        &self,
        dir: &Path,
        app: OrchestratorApp,
        session: &SessionId,
        surface: &SurfaceId,
        token_file: &Path,
    ) -> Result<(), CoreError> {
        write_private_text(&dir.join("AGENTS.md"), ORCHESTRATOR_INSTRUCTIONS)?;
        let token_file = token_file.display().to_string();
        let session = session.to_string();
        let surface = surface.to_string();
        match app {
            OrchestratorApp::Opencode => {
                let config = serde_json::json!({
                    "mcp": {
                        "vowp": {
                            "type": "local",
                            "command": [
                                "vowp",
                                "mcp",
                                "--session",
                                session,
                                "--surface",
                                surface,
                                "--token-file",
                                token_file,
                            ],
                            "enabled": true
                        }
                    }
                });
                write_private_json_atomic(&dir.join("opencode.json"), &config)?;
            }
            OrchestratorApp::ClaudeCode => {
                let config = serde_json::json!({
                    "mcpServers": {
                        "vowp": {
                            "command": "vowp",
                            "args": [
                                "mcp",
                                "--session",
                                session,
                                "--surface",
                                surface,
                                "--token-file",
                                token_file,
                            ]
                        }
                    }
                });
                write_private_json_atomic(&dir.join(".mcp.json"), &config)?;
            }
            OrchestratorApp::Chat => {}
        }
        Ok(())
    }

    /// Mirrors one running host's new transcript entries and writes `mirror.json`.
    ///
    /// A transcript read or cursor write failure is logged, not returned: `create` and
    /// `resume` already started the host, and the app may still be coming up. The cursor
    /// file is written even after a failed read so `mirror.json` exists for a running
    /// orchestrator and the next pass starts from the last known position.
    fn mirror(&self, dir: &Path, state: &StateFile, host: &dyn AppHost, handle: &AppHandle) {
        let path = dir.join(MIRROR_FILE);
        let mut cursor = match mirror::read_cursor(&path) {
            Ok(cursor) => cursor,
            Err(error) => {
                self.deps.log.log(
                    Level::Warn,
                    "orchestrator",
                    &format!("cannot read {}: {error}", path.display()),
                );
                MirrorCursor {
                    last_native_id: None,
                    updated_at: self.deps.clock.now(),
                }
            }
        };
        let result = (|| -> Result<(), CoreError> {
            let scoped = self.scoped_deps(state.project.as_ref())?;
            let store = scoped.open_store()?;
            mirror::mirror_from_host(&scoped, &store, &state.session_id, host, handle, &mut cursor)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!("cannot mirror {}: {error}", state.name),
            );
        }
        if let Err(error) = mirror::write_cursor(&path, &cursor) {
            self.deps.log.log(
                Level::Warn,
                "orchestrator",
                &format!("cannot write {}: {error}", path.display()),
            );
        }
    }

    /// Returns the app's project-local config path.
    fn config_path(&self, dir: &Path, app: OrchestratorApp) -> PathBuf {
        match app {
            OrchestratorApp::Opencode => dir.join("opencode.json"),
            OrchestratorApp::ClaudeCode => dir.join(".mcp.json"),
            OrchestratorApp::Chat => dir.join("AGENTS.md"),
        }
    }

    /// Ensures the registry row and project database the session needs exist.
    fn prepare_project(&self, global: &Store, project: Option<&ProjectName>) -> Result<(), CoreError> {
        match project {
            Some(project) => {
                if global.project(project)?.is_none() {
                    return Err(CoreError::NotFound(format!(
                        "project {project} is not registered; run `vowp project add <path>` first"
                    )));
                }
                Store::create_project_db(&self.deps.paths, project)?;
            }
            None => {
                if global.local_node()?.is_none() {
                    let now = self.deps.clock.now();
                    global.upsert_node(&vowp_protocol::Node {
                        name: self.deps.node.clone(),
                        ssh_target: None,
                        is_local: true,
                        capabilities: serde_json::json!({ "vowp": env!("CARGO_PKG_VERSION") }),
                        last_seen_at: Some(now),
                        created_at: now,
                    })?;
                }
                let node = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
                if global.project(&node)?.is_none() {
                    let now = self.deps.clock.now();
                    global.upsert_project(&Project {
                        name: node.clone(),
                        path: self.deps.paths.root().display().to_string(),
                        remote_url: None,
                        default_branch: "main".to_owned(),
                        config: serde_json::json!({}),
                        created_at: now,
                        updated_at: now,
                    })?;
                } else {
                    Store::create_project_db(&self.deps.paths, &node)?;
                }
            }
        }
        Ok(())
    }

    /// Returns deps bound to the orchestrator's project (`_node` when project-less).
    fn scoped_deps(&self, project: Option<&ProjectName>) -> Result<Deps, CoreError> {
        let target = match project {
            Some(project) => project.clone(),
            None => ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?,
        };
        let mut config = (*self.deps.config).clone();
        config.project.name = target;
        let mut scoped = self.deps.clone();
        scoped.config = Arc::new(config);
        Ok(scoped)
    }

    /// Releases this orchestrator's lease, never another surface's.
    ///
    /// The token file is the authority: a takeover replaces the lease row's token, so a
    /// stop after a takeover must leave the new holder's row alone. Only when the token
    /// file is gone does the holder name decide, which covers an old crashed directory.
    fn release_lease(&self, dir: &Path, holder: &str, session: &SessionId) -> Result<(), CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        let Some(lease) = store.lease(session)? else {
            return Ok(());
        };
        let token = fs::read_to_string(dir.join(LEASE_FILE)).ok();
        match token.as_deref() {
            Some(token) => {
                let token = token.trim();
                if hash_lease_token(token) == lease.token {
                    store.release_lease(session, token)?;
                }
            }
            None => {
                if lease.holder == holder {
                    store.release_lease_hash(session, &lease.token)?;
                }
            }
        }
        Ok(())
    }

    /// Releases a lease with an explicit token.
    fn release_lease_with(&self, session: &SessionId, token: &str) -> Result<(), CoreError> {
        let store = Store::open_global(&self.deps.paths)?;
        store.release_lease(session, token)?;
        Ok(())
    }

    /// Stops and removes an existing orchestrator directory (`--force`).
    fn remove_existing(&self, name: &str) -> Result<(), CoreError> {
        let dir = self.dir(name);
        if let Some(mut state) = read_state(&dir)? {
            self.stop_keeper(&dir);
            if self.state_is_remote(&state)?
                && let Some(node) = state.node.clone()
            {
                let request_id = new_request_id();
                let command = OrchestratorCommand::Stop {
                    request_id,
                    name: state.name.clone(),
                };
                let _ = self.send_command(&node, &command, &mut state);
            }
            if let Some(handle) = host_handle(&dir, state.app)
                && let Ok(Some(host)) = self.host_for(state.app)
            {
                let _ = host.stop(&handle);
            }
            self.release_lease(&dir, &state.name, &state.session_id)?;
        }
        remove_dir_all_bounded(&dir)
    }
}

/// How many attempts a forced orchestrator-directory removal may make.
const REMOVE_ATTEMPTS: u32 = 25;

/// How long each failed removal waits before retrying.
const REMOVE_RETRY: Duration = Duration::from_millis(40);

/// Removes `dir`, retrying transient `--force` races while a dying app flushes its WAL.
///
/// A SQLite WAL checkpoint can keep recreating a file while `remove_dir_all` walks the
/// tree, which surfaces as `DirectoryNotEmpty`/`ENOTEMPTY`, `AlreadyExists` or
/// `PermissionDenied`. `NotFound` is already the desired state.
fn remove_dir_all_bounded(dir: &Path) -> Result<(), CoreError> {
    for attempt in 0..REMOVE_ATTEMPTS {
        match fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt + 1 < REMOVE_ATTEMPTS && transient_remove_error(&error) => {
                thread::sleep(REMOVE_RETRY);
            }
            Err(source) => {
                return Err(CoreError::Io {
                    path: dir.display().to_string(),
                    source,
                });
            }
        }
    }
    Err(CoreError::Io {
        path: dir.display().to_string(),
        source: std::io::Error::other("the directory was still busy after the bounded retries"),
    })
}

/// Returns whether a directory-removal error can clear up on its own.
fn transient_remove_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::DirectoryNotEmpty
            | std::io::ErrorKind::AlreadyExists
            | std::io::ErrorKind::PermissionDenied
    ) || matches!(error.raw_os_error(), Some(39) | Some(66))
}

/// Returns a fresh, opaque command correlation id.
fn new_request_id() -> String {
    ulid::Ulid::generate().to_string()
}

/// Returns the stable human name of a node presence.
fn presence_name(presence: NodePresence) -> &'static str {
    match presence {
        NodePresence::Online => "online",
        NodePresence::Degraded => "degraded",
        NodePresence::Offline => "offline",
        NodePresence::Revoked => "revoked",
    }
}

/// Returns whether a transport failure means the node is not reachable now.
fn is_node_offline_error(error: &CoreError) -> bool {
    match error {
        CoreError::Refused(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("node_offline") || message.contains("offline")
        }
        CoreError::NotFound(_) => true,
        _ => false,
    }
}

/// Builds the refusal a failed synchronous node result returns.
///
/// The detail is redacted again here as defence in depth: a caller may hold state from an
/// older file, and a refusal is printed on the terminal and captured in scripts.
fn refused_by_node(node: &NodeName, action: &str, state: &StateFile) -> CoreError {
    let detail = state
        .detail
        .as_deref()
        .map(redact_text)
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| "the node returned no detail".to_owned());
    CoreError::Refused(format!(
        "node {node} refused to {action} orchestrator {}: {detail}",
        state.name
    ))
}

/// Applies one agent result to a persisted record.
fn apply_result_to_state(state: &mut StateFile, result: &OrchestratorResult) {
    state.state = result.state;
    state.detail = result.detail.as_deref().map(redact_text).or_else(|| {
        result
            .error
            .as_ref()
            .map(|error| redact_text(&format!("{}: {}", error.code, error.message)))
    });
}

/// Redacts a result's free text before the home stores or shows it.
fn sanitize_result(mut result: OrchestratorResult) -> OrchestratorResult {
    if let Some(error) = result.error.as_mut() {
        error.message = redact_text(&error.message);
    }
    if let Some(detail) = result.detail.as_mut() {
        *detail = redact_text(detail);
    }
    result
}

/// Redacts absolute paths, URLs with credentials or a loopback port, and secret values
/// from free text.
///
/// The agent contract already forbids a path or a secret in a result; this is the home's
/// defence in depth so a mistaken detail cannot enter `orchestrator.json`, a card, a CLI
/// refusal or a log. A URL is replaced whole, so a userinfo password or a
/// `http://127.0.0.1:<port>` link never survives even when the port is the only secret.
pub fn redact_text(text: &str) -> String {
    let secret_keys = ["token", "password", "secret", "apikey", "api_key", "code"];
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in text.split_whitespace() {
        let bare = word.trim_start_matches(['(', '[', '{', '\'', '"']);
        let lower = bare.to_ascii_lowercase();
        let looks_like_secret = lower.contains('=') && secret_keys.iter().any(|key| lower.contains(key));
        let has_userinfo = bare
            .split_once("://")
            .is_some_and(|(_, rest)| rest.split('/').next().unwrap_or_default().contains('@'));
        let loopback = lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("0.0.0.0");
        let was_redacted = redact_next;
        if was_redacted {
            words.push("<redacted>".to_owned());
            redact_next = false;
        } else if has_userinfo || loopback {
            words.push("<url>".to_owned());
        } else if bare.starts_with('/') || bare.starts_with("~/") {
            words.push("<path>".to_owned());
        } else if looks_like_secret {
            words.push("<redacted>".to_owned());
        } else {
            words.push(word.to_owned());
        }
        // `token: value`, `password = value` and `Bearer value` are all assignments whose
        // value is a secret. `token=value` was already redacted as one word.
        if !was_redacted {
            let key = bare.trim_end_matches([':', '=']).to_ascii_lowercase();
            let assignment = secret_keys.contains(&key.as_str()) || key == "bearer";
            redact_next = assignment && !looks_like_secret;
        }
    }
    words.join(" ").chars().take(512).collect()
}

/// Join instructions for an orchestrator whose app runs on a local node.
///
/// They carry only commands and the node name: no port, pairing link, absolute path or
/// secret value.
fn node_join(app: OrchestratorApp, node: &NodeName, session: &SessionId) -> JoinInstructions {
    match app {
        OrchestratorApp::Opencode => JoinInstructions {
            desktop: format!(
                "On the home run `vowp surface pair --session {session} --node {node} --kind opencode`; \
                 then on {node} run `vowp agent pair --code-file -` and open the opencode app."
            ),
            phone: format!("Open the home app or run `vowp chat {session}`."),
            chat: format!("vowp chat --agent --session {session}"),
        },
        OrchestratorApp::ClaudeCode => JoinInstructions {
            desktop: format!("Open https://claude.ai/code and choose this Remote Control session; it runs on {node}."),
            phone: format!("Open the home app or run `vowp chat {session}`."),
            chat: format!("vowp chat --agent --session {session}"),
        },
        OrchestratorApp::Chat => JoinInstructions {
            desktop: format!("vowp chat --agent --session {session}"),
            phone: format!("vowp chat {session}"),
            chat: format!("vowp chat --agent --session {session}"),
        },
    }
}

/// The persisted `orchestrator.json` shape.
///
/// It carries every [`OrchestratorCard`] field, because the fleet snapshot reads this
/// same file, plus the state the manager needs to resume a host.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    name: String,
    app: OrchestratorApp,
    session_id: SessionId,
    #[serde(default)]
    project: Option<ProjectName>,
    #[serde(default)]
    executor: Option<ExecutorRef>,
    state: OrchestratorNodeState,
    created_at: Timestamp,
    #[serde(default)]
    join: Option<JoinInstructions>,
    /// Surface id used by the app's MCP config; kept so resume can reuse it.
    #[serde(default)]
    surface_id: Option<SurfaceId>,
    /// Node that runs the app; absent on a v0.2 home orchestrator.
    #[serde(default)]
    node: Option<NodeName>,
    /// Request id of the most recent command sent to a local node.
    #[serde(default)]
    last_request_id: Option<String>,
    /// Last redacted failure or state detail for the owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Derived card field, persisted so the fleet snapshot sees the same last error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl StateFile {
    /// Returns the public snapshot card.
    fn card(&self) -> OrchestratorCard {
        OrchestratorCard {
            name: self.name.clone(),
            session_id: self.session_id.clone(),
            project: self.project.clone(),
            app: self.app,
            state: self.state,
            node: self.node.clone(),
            last_error: self.last_error(),
            join: self
                .join
                .clone()
                .unwrap_or_else(|| fallback_join(self.app, &self.session_id)),
        }
    }

    /// Returns the redacted failure detail only for a failed, degraded or offline record.
    ///
    /// A cleanly running or stopped record has no `last_error`, even when an old detail is
    /// still on disk; [`Orchestrator::resume`] and [`Orchestrator::stop`] clear it as well.
    fn last_error(&self) -> Option<String> {
        match self.state {
            OrchestratorNodeState::Failed | OrchestratorNodeState::Degraded | OrchestratorNodeState::NodeOffline => {
                self.detail.as_deref().map(redact_text)
            }
            OrchestratorNodeState::Starting | OrchestratorNodeState::Running | OrchestratorNodeState::Stopped => None,
        }
    }
}

/// The subset of `host.json` the manager needs to recognise a live host.
#[derive(Debug, Deserialize)]
struct HostFile {
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    started_at: Option<Timestamp>,
    #[serde(default)]
    workdir: Option<String>,
}

/// Builds the final info value.
fn info_from_state(state: StateFile, dir: PathBuf, host: Option<AppHandle>) -> OrchestratorInfo {
    OrchestratorInfo {
        card: state.card(),
        dir,
        session: state.session_id,
        host,
        detail: state.detail,
    }
}

/// Reads an orchestrator state file, if it exists.
fn read_state(dir: &Path) -> Result<Option<StateFile>, CoreError> {
    let path = dir.join(STATE_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CoreError::Invalid(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The path-free failure for an owner view that cannot be read.
fn owner_unreadable(name: &str) -> CoreError {
    CoreError::Invalid(format!("orchestrator {name}: the session owner could not be read"))
}

/// The typed refusal for an attach whose local endpoint record or service file is unusable.
///
/// It names the orchestrator, leaks no path or password, and tells the owner how to rebuild
/// the local endpoint.
fn attach_endpoint_unavailable(name: &str) -> CoreError {
    CoreError::Refused(format!(
        "orchestrator {name} has no usable app endpoint to attach to; run `vowp orchestrator resume {name}` first"
    ))
}

/// Reads one endpoint's state file, naming the orchestrator and never a path.
fn read_endpoint_state(name: &str, dir: &Path) -> Result<Option<StateFile>, CoreError> {
    read_state(dir).map_err(|error| match error {
        CoreError::Invalid(_) => CoreError::Invalid(format!(
            "orchestrator {name}: the local orchestrator record is not valid JSON"
        )),
        CoreError::Io { .. } => CoreError::Invalid(format!(
            "orchestrator {name}: the local orchestrator record could not be read"
        )),
        other => other,
    })
}

/// Writes an orchestrator state file with mode 0600.
///
/// The persisted document also carries the derived [`OrchestratorCard::last_error`] so the
/// fleet snapshot, which reads this same file, shows the same redacted failure detail.
fn write_state(dir: &Path, state: &StateFile) -> Result<(), CoreError> {
    let mut stored = state.clone();
    stored.last_error = state.last_error();
    write_private_json_atomic(&dir.join(STATE_FILE), &stored)
}

/// Returns the live app handle recorded by a host, when its pid is still alive.
fn host_handle(dir: &Path, app: OrchestratorApp) -> Option<AppHandle> {
    let path = dir.join(HOST_FILE);
    let bytes = fs::read(path).ok()?;
    let host: HostFile = serde_json::from_slice(&bytes).ok()?;
    if host.pid <= 1 || !process_alive(host.pid) {
        return None;
    }
    Some(AppHandle {
        kind: app,
        pid: host.pid,
        port: host.port,
        started_at: host.started_at.unwrap_or_else(Timestamp::now),
        workdir: host.workdir.map(PathBuf::from).unwrap_or_else(|| dir.to_path_buf()),
    })
}

/// Writes one private text file (mode 0600) atomically, creating its parent.
fn write_private_text(path: &Path, text: &str) -> Result<(), CoreError> {
    write_private_bytes(path, text.as_bytes())
}

/// Writes one private JSON document (mode 0600) atomically, creating its parent.
fn write_private_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), CoreError> {
    let bytes = serde_json::to_vec(value).map_err(|error| CoreError::Invalid(error.to_string()))?;
    write_private_bytes(path, &bytes)
}

/// Writes one private file (mode 0600) through a unique temporary file and a rename.
///
/// A reader either sees the old document or the new one, never a truncated file: the
/// bytes go to a `create_new` temporary file in the same directory, `sync_all` flushes
/// them, and `rename` replaces the target in one step. The temporary file is removed on
/// every error path so a failed write leaves no `.tmp-*` litter.
fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| CoreError::Invalid(format!("the private file {} has no parent", path.display())))?;
    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
        path: parent.display().to_string(),
        source,
    })?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".tmp-{}-{counter}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temp);
        return Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        });
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Creates the orchestrator directory with mode 0700.
fn create_private_dir(dir: &Path) -> Result<(), CoreError> {
    fs::create_dir_all(dir).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })
}

/// Validates a name used as one directory below `orchestrators/`.
fn validate_name(name: &str) -> Result<(), CoreError> {
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
        Err(CoreError::Invalid(format!(
            "invalid orchestrator name {name:?}; use letters, digits, '.', '_' or '-'"
        )))
    }
}

/// Maps an app to its surface kind.
fn surface_kind(app: OrchestratorApp) -> SurfaceKind {
    match app {
        OrchestratorApp::Opencode => SurfaceKind::Opencode,
        OrchestratorApp::ClaudeCode => SurfaceKind::ClaudeCode,
        OrchestratorApp::Chat => SurfaceKind::Chat,
    }
}

/// Returns the CLI name of an app.
pub(crate) fn app_name(app: OrchestratorApp) -> &'static str {
    match app {
        OrchestratorApp::Opencode => "opencode",
        OrchestratorApp::ClaudeCode => "claude-code",
        OrchestratorApp::Chat => "chat",
    }
}

/// Join instructions used when an app host is not available.
///
/// They carry only generic placeholders (`<home-ssh-target>`, `<opencode-session>`); the
/// control machine substitutes the owner's real target in `vowp tunnel` and setup.
fn fallback_join(app: OrchestratorApp, session: &SessionId) -> JoinInstructions {
    let chat = format!("vowp chat {session}");
    match app {
        OrchestratorApp::Opencode => JoinInstructions {
            desktop: "On the control machine run `vowp tunnel open <name> --remote-port <port>`, then \
                      print a one-time pair link on the home with `opencode pair` and open it in the \
                      opencode desktop app."
                .to_owned(),
            phone: "ssh -t <home-ssh-target> opencode --server http://127.0.0.1:<port> --session <opencode-session>"
                .to_owned(),
            chat,
        },
        OrchestratorApp::ClaudeCode => JoinInstructions {
            desktop: "Open https://claude.ai/code and choose this Remote Control session; it runs on the VOWP home."
                .to_owned(),
            phone: "Open the Claude mobile app and choose the same Remote Control session.".to_owned(),
            chat,
        },
        OrchestratorApp::Chat => JoinInstructions {
            desktop: chat.clone(),
            phone: format!("ssh -t <home-ssh-target> {chat}"),
            chat,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{redact_text, validate_name, write_private_text};

    #[test]
    fn a_failed_atomic_write_removes_its_temp_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("state.json");
        fs::create_dir(&target).expect("block the target with a directory");
        fs::write(target.join("occupied"), b"keep").expect("occupant");

        let error = write_private_text(&target, "{}").expect_err("a rename over a non-empty directory fails");
        assert!(matches!(error, crate::CoreError::Io { .. }), "{error}");
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
    }

    #[test]
    fn names_cannot_escape_the_orchestrator_root() {
        for name in ["..", ".", "", "a/b", "a b", "a\\b"] {
            assert!(validate_name(name).is_err(), "{name:?} was accepted");
        }
        for name in ["vowp", "release-2", "team_a", "a.b"] {
            assert!(validate_name(name).is_ok(), "{name:?} was refused");
        }
    }

    #[test]
    fn redacts_paths_and_secret_assignments_from_node_text() {
        let detail = "cannot start /srv/vowp/app token=FAKE-NOT-A-SECRET-0000";
        let redacted = redact_text(detail);
        assert!(!redacted.contains("/srv/"), "{redacted}");
        assert!(!redacted.contains("FAKE-NOT-A-SECRET-0000"), "{redacted}");
        assert!(redacted.contains("<path>"), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
        assert_eq!(redact_text("the app did not start"), "the app did not start");
        assert!(redact_text(&"x".repeat(2048)).chars().count() <= 512);
    }
}
