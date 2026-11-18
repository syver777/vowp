//! The frame client: enrollment, handshake, heartbeat, reconnect and status.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.2, §4.6, §8.2–§8.5 and §12.2. All I/O goes
//! through a [`Transport`]; tests inject an in-memory home instead of opening SSH.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vowp_protocol::agent::surface::{SurfaceAttach, SurfaceDetach, ToolCall, ToolResult};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, AgentRole, ErrorKind, NodeName,
    OrchestratorApp, OrchestratorCommand, OrchestratorError, OrchestratorNodeState, OrchestratorResult, ProjectName,
    SessionId, SurfaceId, SurfaceScope, Timestamp,
};

use crate::bridge::{BridgeState, OpencodeBridge};
use crate::cache::{Cache, debug_failure, is_cacheable_tool};
use crate::config::{AgentConfig, TransportConfig, ensure_private_dir, write_private, write_surface_binding};
use crate::error::AgentError;
use crate::executor::{RunDriver, RunHandler};
use crate::key::NodeKey;
use crate::orchestrator::{ORCHESTRATOR_POLL, OrchestratorHost, PrepareOutcome, SurfaceAttachment, SurfaceBinding};
use crate::service::ServiceRunOutcome;
use crate::socket::{AgentSocket, PairReply, SOCKET_POLL, SocketEvent, SocketResponse, TOOL_CALL_TIMEOUT};
use crate::status::{AgentStatus, ConnectionState, pending_spool_bytes, surface_token_expiry};
use crate::surface_state::{SurfaceRefusal, mark_needs_pair, mark_paired, mark_revoked, surface_statuses};
use crate::transport::{FrameRead, FrameStream, MAX_FRAME_BYTES, Transport, transport_for_config};

/// Agent crate version sent in `AgentHello` and `Enroll`.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Default heartbeat interval when the home does not name one.
pub const HEARTBEAT_SECS: u64 = 10;
/// A connection that lasted this long resets the reconnect backoff.
pub const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Longest reconnect delay.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long `pair` waits for the home's answer after the handshake.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a running agent prunes the read cache, in addition to a bounded store.
const CACHE_PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The clock and sleeper the client uses. Tests inject a fake clock.
pub trait AgentClock: Send + Sync {
    /// Returns the current instant.
    fn now(&self) -> Timestamp;
    /// Sleeps for `duration`, advancing fake time in tests.
    fn sleep(&self, duration: Duration);
}

/// The real clock backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAgentClock;

impl AgentClock for SystemAgentClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Exponential reconnect backoff: 1, 2, 4 … 60 seconds with up to 25 % jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    next: Duration,
    state: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// Creates a time-seeded backoff.
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self::seeded(seed)
    }

    /// Creates a deterministic backoff for tests.
    pub fn seeded(seed: u64) -> Self {
        Self {
            base: Duration::from_secs(1),
            max: MAX_BACKOFF,
            next: Duration::from_secs(1),
            state: seed | 1,
        }
    }

    /// Returns the next delay and doubles the base for the following attempt.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.next;
        let span = u64::try_from(base.as_millis() / 4).unwrap_or(u64::MAX);
        let jitter = if span == 0 { 0 } else { self.random() % (span + 1) };
        let delay = base.saturating_add(Duration::from_millis(jitter));
        self.next = base.saturating_mul(2).min(self.max);
        delay
    }

    /// Resets the next delay to one second after a stable connection.
    pub fn reset(&mut self) {
        self.next = self.base;
    }

    fn random(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }
}

/// What `vowp agent run` passed on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Requested role. Phase 0 accepts it and keeps the connection open for later assignment work.
    pub role: AgentRole,
    /// Keep the process in the terminal. Phase 0 always runs in the foreground.
    pub foreground: bool,
}

impl RunOptions {
    /// Creates options with the Phase 0 foreground default.
    pub fn new(role: AgentRole) -> Self {
        Self { role, foreground: true }
    }
}

/// Why the run loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The home revoked the node credential.
    Revoked {
        /// Human-readable reason.
        reason: String,
    },
    /// Another connection took over.
    Superseded {
        /// Connection that replaced this one.
        connection_id: String,
    },
}

/// The surface metadata a Mac needs to redeem a home pairing code.
///
/// The code is a secret. `Debug` redacts it; callers must never print the struct with its
/// fields expanded by hand.
#[derive(Clone, PartialEq, Eq)]
pub struct SurfacePairRequest {
    /// One-time pairing code read from the code file; never logged or echoed.
    pub code: String,
    /// Surface the owner created on the home.
    pub surface_id: SurfaceId,
    /// Session the surface will mirror.
    pub session: SessionId,
    /// Application name, for example `opencode`.
    pub app: String,
}

impl SurfacePairRequest {
    /// Builds one pairing request.
    pub fn new(code: impl Into<String>, surface_id: SurfaceId, session: SessionId, app: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            surface_id,
            session,
            app: app.into(),
        }
    }
}

impl std::fmt::Debug for SurfacePairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfacePairRequest")
            .field("code", &"[REDACTED]")
            .field("surface_id", &self.surface_id)
            .field("session", &self.session)
            .field("app", &self.app)
            .finish()
    }
}

/// A surface token stored by the agent. The token itself is never carried in this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedSurface {
    /// Surface the token belongs to.
    pub surface_id: SurfaceId,
    /// Scopes granted to the token.
    pub scopes: Vec<SurfaceScope>,
    /// When the token expires.
    pub expires_at: Timestamp,
    /// Mode-0600 file that holds the token, for display only.
    pub token_path: PathBuf,
}

/// The result of a successful enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrollment {
    /// Node name assigned by the home.
    pub node: NodeName,
    /// Home identity.
    pub home: String,
    /// Negotiated protocol.
    pub protocol: String,
    /// Heartbeat interval in seconds.
    pub heartbeat_secs: u64,
}

/// One enrolled agent connection.
pub struct AgentClient {
    state_dir: PathBuf,
    key: NodeKey,
    config: Option<AgentConfig>,
    transport: Box<dyn Transport>,
    clock: Arc<dyn AgentClock>,
    connection: Option<Connection>,
    status: AgentStatus,
    seen_nonces: HashSet<String>,
    backoff: Backoff,
    pending_path_maps: Vec<AgentPathMap>,
    sent_path_maps: Vec<AgentPathMap>,
    path_map_updates: bool,
    awaiting_path_map_reply: bool,
    socket: Option<AgentSocket>,
    pending_tool_calls: HashMap<String, PendingToolCall>,
    tool_call_timeout: Duration,
    cache: Cache,
    cache_pruned_at: Option<Timestamp>,
    bridges: Vec<BridgeLink>,
    orchestrator: OrchestratorHost,
    pending_orchestrator: Option<PendingOrchestrator>,
    pending_pairs: HashMap<SurfaceId, PendingPair>,
    enroll_transport: Option<TransportConfig>,
}

struct PendingOrchestrator {
    command: OrchestratorCommand,
    receiver: mpsc::Receiver<PrepareOutcome>,
}

struct PendingToolCall {
    reply: mpsc::Sender<SocketResponse>,
    deadline: Instant,
    cache_key: Option<CacheKey>,
}

/// One socket-originated pairing request awaiting the home's `SurfaceIssued`.
struct PendingPair {
    session: SessionId,
    reply: mpsc::Sender<PairReply>,
    deadline: Instant,
}

/// The surface, tool and arguments needed to store one successful read.
struct CacheKey {
    tool: String,
    surface: SurfaceId,
    arguments: serde_json::Value,
}

struct BridgeLink {
    bridge: OpencodeBridge,
    attached: bool,
}

struct Connection {
    stream: Box<dyn FrameStream>,
    heartbeat_interval: Duration,
    max_frame_bytes: usize,
}

enum ServeOutcome {
    Stopped(RunOutcome),
    Drained,
    Disconnected(String),
    Failed(AgentError),
}

/// Returns whether the service stop flag has been set.
fn stop_requested(stop: Option<&Arc<AtomicBool>>) -> bool {
    stop.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

impl AgentClient {
    /// Builds a client for an already enrolled config.
    pub fn new(
        state_dir: PathBuf,
        key: NodeKey,
        config: AgentConfig,
        transport: Box<dyn Transport>,
        clock: Arc<dyn AgentClock>,
    ) -> Self {
        let status = base_status(&state_dir, Some(&config), &key);
        let pending_path_maps = config.path_maps.clone();
        let orchestrator = OrchestratorHost::new(state_dir.clone());
        let cache = Cache::new(&state_dir);
        Self {
            state_dir,
            key,
            config: Some(config),
            transport,
            clock,
            connection: None,
            status,
            seen_nonces: HashSet::new(),
            backoff: Backoff::new(),
            pending_path_maps,
            // A fresh process reports the saved map after its first handshake, so a project
            // added while the agent was stopped reaches the home without a re-enrollment.
            sent_path_maps: Vec::new(),
            path_map_updates: true,
            awaiting_path_map_reply: false,
            socket: None,
            pending_tool_calls: HashMap::new(),
            tool_call_timeout: TOOL_CALL_TIMEOUT,
            cache,
            cache_pruned_at: None,
            bridges: Vec::new(),
            orchestrator,
            pending_orchestrator: None,
            pending_pairs: HashMap::new(),
            enroll_transport: None,
        }
    }

    /// Builds a client with no config, for `vowp agent enroll`.
    pub fn unenrolled(
        state_dir: PathBuf,
        key: NodeKey,
        transport: Box<dyn Transport>,
        clock: Arc<dyn AgentClock>,
    ) -> Self {
        let status = base_status(&state_dir, None, &key);
        let orchestrator = OrchestratorHost::new(state_dir.clone());
        let cache = Cache::new(&state_dir);
        Self {
            state_dir,
            key,
            config: None,
            transport,
            clock,
            connection: None,
            status,
            seen_nonces: HashSet::new(),
            backoff: Backoff::new(),
            pending_path_maps: Vec::new(),
            sent_path_maps: Vec::new(),
            path_map_updates: true,
            awaiting_path_map_reply: false,
            socket: None,
            pending_tool_calls: HashMap::new(),
            tool_call_timeout: TOOL_CALL_TIMEOUT,
            cache,
            cache_pruned_at: None,
            bridges: Vec::new(),
            orchestrator,
            pending_orchestrator: None,
            pending_pairs: HashMap::new(),
            enroll_transport: None,
        }
    }

    /// Loads `agent.json` and `node.key` and builds the transport the config names.
    ///
    /// An SSH config runs the documented forced command; a WSS config connects through the
    /// owner's reverse proxy. The transport is chosen once and never falls back to the other.
    pub fn from_state_dir(state_dir: PathBuf, clock: Arc<dyn AgentClock>) -> Result<Self, AgentError> {
        let config = AgentConfig::load(&state_dir)?;
        let key = NodeKey::load(&state_dir)?;
        let transport = transport_for_config(&config.transport)?;
        Ok(Self::new(state_dir, key, config, transport, clock))
    }

    /// Sets the transport an [`AgentClient::enroll`] call writes into `agent.json`.
    ///
    /// `vowp agent enroll --wss` builds an unenrolled client whose transport is WSS and sets
    /// the same transport here; without it enrollment records an SSH target.
    pub fn with_enroll_transport(mut self, transport: TransportConfig) -> Self {
        self.enroll_transport = Some(transport);
        self
    }

    /// Replaces the reconnect backoff (tests use [`Backoff::seeded`]).
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Replaces how long a local tool call waits for the home's `ToolResult`.
    ///
    /// The default is [`TOOL_CALL_TIMEOUT`]. A result that arrives after the deadline is
    /// ignored and the requesting client receives a typed `tool_timeout` error.
    pub fn with_tool_call_timeout(mut self, timeout: Duration) -> Self {
        self.tool_call_timeout = timeout;
        self
    }

    /// Attaches one opencode bridge to this client.
    ///
    /// The bridge uses the same outbound connection as tool calls; its frames are spooled
    /// before they are sent, and a lost lease stops it read-only without deleting the
    /// spool. The caller loads and configures the bridge with [`OpencodeBridge::load`].
    /// More than one bridge may be attached; the accessors below report the first one.
    pub fn with_bridge(mut self, bridge: OpencodeBridge) -> Self {
        self.bridges.push(BridgeLink {
            bridge,
            attached: false,
        });
        self
    }

    /// Returns the first attached bridge, when this client hosts one.
    pub fn bridge(&self) -> Option<&OpencodeBridge> {
        self.bridges.first().map(|link| &link.bridge)
    }

    /// Returns the first attached bridge's observable state.
    pub fn bridge_state(&self) -> Option<BridgeState> {
        self.bridges.first().map(|link| link.bridge.state())
    }

    /// Sets the project path map sent by the next [`AgentClient::enroll`].
    ///
    /// Callers validate the maps with [`AgentConfig::validate_path_maps`] before enrolling.
    /// On an already enrolled client the maps replace the in-memory config; the file changes
    /// only when the caller writes it.
    pub fn with_path_maps(mut self, path_maps: Vec<AgentPathMap>) -> Self {
        self.pending_path_maps = path_maps.clone();
        if let Some(config) = self.config.as_mut() {
            config.path_maps = path_maps;
        }
        self
    }

    /// Returns the current project path map.
    pub fn path_maps(&self) -> &[AgentPathMap] {
        self.config
            .as_ref()
            .map(|config| config.path_maps.as_slice())
            .unwrap_or(&self.pending_path_maps)
    }

    /// Redeems a one-time code, reconnects and performs the handshake.
    ///
    /// `target` is the SSH destination. When [`AgentClient::with_enroll_transport`] named a
    /// WSS transport, `target` must be empty and the WSS transport is stored instead.
    pub fn enroll(&mut self, target: &str, code: &str) -> Result<Enrollment, AgentError> {
        validate_enrollment_code(code)?;
        if self.enroll_transport.is_none() && target.trim().is_empty() {
            return Err(AgentError::Config("ssh target must not be empty".to_owned()));
        }
        let public_key = self.key.public_key_hex();
        let path_maps = self.path_maps().to_vec();
        AgentConfig::validate_path_maps(&self.state_dir, &path_maps)?;
        let mut stream = self.transport.open()?;
        stream.send(&AgentFrame::Enroll {
            code: code.to_owned(),
            public_key,
            agent_version: AGENT_VERSION.to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            labels: Vec::new(),
            capabilities: capabilities(&path_maps),
            paths: path_maps.clone(),
        })?;
        let enrolled = match next_handshake_frame(&mut *stream)? {
            AgentFrame::Enrolled {
                home_id,
                node,
                protocol,
                server_time: _,
                heartbeat_secs,
            } => Enrollment {
                node,
                home: home_id,
                protocol,
                heartbeat_secs,
            },
            AgentFrame::Revoked { reason } => return Err(AgentError::Revoked(reason)),
            AgentFrame::Superseded { connection_id } => return Err(AgentError::Superseded(connection_id)),
            AgentFrame::Error {
                kind,
                message,
                retryable,
            } => return Err(remote_error(kind, message, retryable)),
            _ => {
                return Err(AgentError::Protocol("expected Enrolled from the home".to_owned()));
            }
        };
        drop(stream);

        let mut config = AgentConfig::new(
            enrolled.home.clone(),
            enrolled.node.clone(),
            self.state_dir.clone(),
            target,
        )
        .with_path_maps(path_maps);
        if let Some(transport) = self.enroll_transport.clone() {
            config.transport = transport;
        }
        self.config = Some(config.clone());
        if let Err(error) = self.connect() {
            self.config = None;
            return Err(error);
        }
        // A system-mode WSS handshake records the leaf fingerprint in the in-memory config;
        // persist that config rather than the pre-connection copy.
        let config = self.config.clone().unwrap_or(config);
        config.write()?;
        self.pending_path_maps = config.path_maps.clone();
        self.sent_path_maps = config.path_maps.clone();
        self.status = base_status(&self.state_dir, Some(&config), &self.key);
        self.status.connection = ConnectionState::Stopped;
        self.save_status()?;
        Ok(enrolled)
    }

    /// Redeems one surface pairing code over the agent connection.
    ///
    /// The client performs the normal handshake, sends one `SurfacePair` frame with the
    /// request's surface, session and app, and writes the returned token to
    /// `surfaces/<surface_id>/token` mode 0600. The token is never printed, logged or
    /// returned; the [`PairedSurface`] result names only its path, scopes and expiry.
    pub fn pair(&mut self, request: &SurfacePairRequest) -> Result<PairedSurface, AgentError> {
        if self.config.is_none() {
            return Err(AgentError::NotEnrolled("agent.json is missing".to_owned()));
        }
        validate_pairing_code(&request.code)?;
        validate_surface_id(&request.surface_id)?;
        if request.app.is_empty() || request.app.chars().count() > 64 || request.app.chars().any(char::is_control) {
            return Err(AgentError::Config(
                "app must be a printable name of at most 64 characters".to_owned(),
            ));
        }

        self.connect()?;
        let result = self.redeem_surface(request);
        // Pairing is a one-shot command; close the SSH child instead of leaving it idle.
        self.connection = None;
        result
    }

    fn redeem_surface(&mut self, request: &SurfacePairRequest) -> Result<PairedSurface, AgentError> {
        self.send_frame(&AgentFrame::SurfacePair {
            code: request.code.clone(),
            surface_id: request.surface_id.clone(),
            session: request.session.clone(),
            app: request.app.clone(),
        })?;
        // The gateway drains system-initiated frames before it reads the pairing frame, so a
        // queued `SurfaceIssued` from a rotation can arrive ahead of this request's answer.
        // Apply those frames and keep waiting for the matching answer instead of failing.
        let deadline = Instant::now() + PAIR_TIMEOUT;
        let (token, scopes, expires_at) = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AgentError::Transport(
                    "the home did not answer the surface pairing request".to_owned(),
                ));
            }
            match self.recv_frame(Some(remaining))? {
                FrameRead::Frame(AgentFrame::SurfaceIssued {
                    surface_id,
                    token,
                    scopes,
                    expires_at,
                }) => {
                    if surface_id == request.surface_id {
                        if token.is_empty() {
                            return Err(AgentError::Protocol(
                                "the home returned an empty surface token".to_owned(),
                            ));
                        }
                        break (token, scopes, expires_at);
                    }
                    self.apply_surface_issued(surface_id, token, expires_at)?;
                }
                FrameRead::Frame(AgentFrame::SurfaceRevoked { surface_id, reason }) => {
                    if surface_id == request.surface_id {
                        return Err(AgentError::Remote {
                            kind: ErrorKind::Refused,
                            message: format!("surface {} was revoked: {reason}", request.surface_id),
                            retryable: false,
                        });
                    }
                    self.forget_surface_token(&surface_id);
                    if let Err(error) = self.cache.clear_surface(&surface_id) {
                        debug_failure("cache clear after surface revocation failed", &error);
                    }
                    self.orchestrator.stop_surface(&surface_id);
                }
                FrameRead::Frame(AgentFrame::Revoked { reason }) => return Err(AgentError::Revoked(reason)),
                FrameRead::Frame(AgentFrame::Superseded { connection_id }) => {
                    return Err(AgentError::Superseded(connection_id));
                }
                FrameRead::Frame(AgentFrame::Error {
                    kind,
                    message,
                    retryable,
                }) => return Err(remote_error(kind, message, retryable)),
                FrameRead::Frame(AgentFrame::Ping) => {
                    self.send_frame(&AgentFrame::Pong)?;
                }
                // The gateway drains its post-handshake queues before it reads this
                // connection's pairing frame, so a command, assignment or lease notice
                // can arrive ahead of `SurfaceIssued`. None of them belongs to a one-shot
                // pairing connection; keep waiting for the matching answer.
                FrameRead::Frame(
                    AgentFrame::OrchestratorCommand(_)
                    | AgentFrame::AssignRun { .. }
                    | AgentFrame::CancelRun { .. }
                    | AgentFrame::RunLost { .. }
                    | AgentFrame::Cleanup { .. }
                    | AgentFrame::Credit { .. }
                    | AgentFrame::Pong
                    | AgentFrame::IngestAck { .. }
                    | AgentFrame::LeaseLost { .. }
                    | AgentFrame::LeaseRevoked { .. }
                    | AgentFrame::BundleAccept { .. }
                    | AgentFrame::BundleAbort { .. },
                ) => {}
                FrameRead::Frame(_) => {
                    return Err(AgentError::Protocol("expected SurfaceIssued from the home".to_owned()));
                }
                FrameRead::Timeout => {
                    return Err(AgentError::Transport(
                        "the home did not answer the surface pairing request".to_owned(),
                    ));
                }
                FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the home closed the connection during surface pairing".to_owned(),
                    ));
                }
            }
        };
        let token_path = write_surface_token(&self.state_dir, &request.surface_id, &token, expires_at)?;
        mark_paired(&self.state_dir, &request.surface_id)?;
        write_surface_binding(&self.state_dir, &request.surface_id, &request.session)?;
        // A fresh pairing replaces every read cached before it, so a re-paired surface
        // can never read data fetched under the previous token.
        if let Err(error) = self.cache.clear_surface(&request.surface_id) {
            debug_failure("cache clear after pairing failed", &error);
        }
        Ok(PairedSurface {
            surface_id: request.surface_id.clone(),
            scopes,
            expires_at,
            token_path,
        })
    }

    /// Applies a `SurfaceIssued` frame pushed by the home after a rotation.
    ///
    /// The replacement token is written 0600 with an atomic rename, an existing session
    /// binding is refreshed, the cached read data of the old token is dropped and
    /// `status.json` reports the new expiry. The token is never logged, printed or stored in
    /// a status field.
    fn apply_surface_issued(
        &mut self,
        surface_id: SurfaceId,
        token: String,
        expires_at: Timestamp,
    ) -> Result<(), AgentError> {
        validate_surface_id(&surface_id)?;
        if token.is_empty() {
            return Err(AgentError::Protocol(
                "the home pushed an empty surface token".to_owned(),
            ));
        }
        write_surface_token(&self.state_dir, &surface_id, &token, expires_at)?;
        mark_paired(&self.state_dir, &surface_id)?;
        self.refresh_surface_binding(&surface_id)?;
        if let Err(error) = self.cache.clear_surface(&surface_id) {
            debug_failure("cache clear after surface rotation failed", &error);
        }
        self.status.surface_token_expires_at = surface_token_expiry(&self.state_dir);
        self.save_status()
    }

    /// Rewrites an existing `surfaces/<id>/session` binding with private modes.
    ///
    /// A rotation carries no session, so the binding is only refreshed when one is already
    /// present. A missing or unreadable binding is left alone rather than invented.
    fn refresh_surface_binding(&self, surface: &SurfaceId) -> Result<(), AgentError> {
        let path = AgentConfig::surface_session_path(&self.state_dir, surface);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(AgentError::Io(error)),
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(());
        };
        let Ok(session) = SessionId::parse(text.trim()) else {
            return Ok(());
        };
        write_surface_binding(&self.state_dir, surface, &session)
    }

    /// Deletes the token and expiry of a revoked surface and refreshes `status.json`.
    fn forget_surface_token(&mut self, surface: &SurfaceId) {
        for path in [
            AgentConfig::surface_token_path(&self.state_dir, surface),
            AgentConfig::surface_expiry_path(&self.state_dir, surface),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => debug_failure("surface token delete failed", &AgentError::Io(error)),
            }
        }
        if let Err(error) = mark_revoked(&self.state_dir, surface) {
            debug_failure("surface state write after revocation failed", &error);
        }
        self.status.surface_token_expires_at = surface_token_expiry(&self.state_dir);
        if let Err(error) = self.save_status() {
            debug_failure("status write after surface revocation failed", &error);
        }
    }

    /// Marks one surface `needs_pair` after the home refused its token.
    ///
    /// The surface leaves the active expiry set, its cached reads are dropped, an attached app
    /// host is stopped and the dead token stays on disk for inspection. The one agent
    /// connection is unaffected: a dead surface token is not a dead node key.
    pub fn mark_surface_needs_pair(&mut self, surface: &SurfaceId, reason: &str) -> Result<SurfaceRefusal, AgentError> {
        let refusal = mark_needs_pair(&self.state_dir, surface, reason)?;
        if let Err(error) = self.cache.clear_surface(surface) {
            debug_failure("cache clear after a refused surface token failed", &error);
        }
        if self.bridges.iter().any(|link| link.bridge.surface() == surface) {
            self.bridge_read_only(surface, refusal.message.clone());
        }
        self.orchestrator.stop_surface(surface);
        self.status.surface_token_expires_at = surface_token_expiry(&self.state_dir);
        self.status.last_error = Some(refusal.message.clone());
        self.save_status()?;
        Ok(refusal)
    }

    /// Returns the surface a `Refused` surface-token error belongs to, when it is unambiguous.
    ///
    /// The wire `Error` frame carries no surface id (the protocol is unchanged), so the agent
    /// prefers the surface of the one pending tool call and otherwise the one local surface.
    fn refused_surface(&self) -> Option<SurfaceId> {
        let pending: Vec<SurfaceId> = self
            .pending_tool_calls
            .values()
            .filter_map(|call| call.cache_key.as_ref().map(|key| key.surface.clone()))
            .collect();
        if pending.len() == 1
            && let Some(surface) = pending.first()
        {
            return Some(surface.clone());
        }
        let statuses = surface_statuses(&self.state_dir);
        if statuses.len() == 1 {
            return statuses.first().map(|status| status.surface.clone());
        }
        None
    }

    /// Opens a transport and performs `AgentHello` → `HomeChallenge` → `AgentAuth` → `HomeWelcome`.
    pub fn connect(&mut self) -> Result<(), AgentError> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| AgentError::NotEnrolled("agent.json is missing".to_owned()))?;
        self.connection = None;
        let mut stream = self.transport.open()?;
        stream.send(&AgentFrame::AgentHello {
            node: config.node.clone(),
            key_id: self.key.public_key_hex(),
            agent_version: AGENT_VERSION.to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            labels: Vec::new(),
            capabilities: capabilities(&config.path_maps),
        })?;

        let challenge = match next_handshake_frame(&mut *stream)? {
            frame @ AgentFrame::HomeChallenge { .. } => frame,
            AgentFrame::Revoked { reason } => return Err(AgentError::Revoked(reason)),
            AgentFrame::Superseded { connection_id } => return Err(AgentError::Superseded(connection_id)),
            AgentFrame::Error {
                kind,
                message,
                retryable,
            } => return Err(remote_error(kind, message, retryable)),
            _ => {
                return Err(AgentError::Protocol("expected HomeChallenge from the home".to_owned()));
            }
        };
        let AgentFrame::HomeChallenge {
            nonce,
            home_id,
            connection_id,
            server_time: _,
            heartbeat_secs,
            max_frame_bytes,
        } = challenge
        else {
            return Err(AgentError::Protocol("expected HomeChallenge".to_owned()));
        };
        if nonce.is_empty() {
            return Err(AgentError::Protocol("HomeChallenge nonce is empty".to_owned()));
        }
        if !self.seen_nonces.insert(nonce.clone()) {
            return Err(AgentError::ReplayNonce);
        }
        stream.send(&AgentFrame::AgentAuth {
            signature: self.key.sign_handshake(&nonce, &home_id, &connection_id),
        })?;

        let welcome = match next_handshake_frame(&mut *stream)? {
            frame @ AgentFrame::HomeWelcome { .. } => frame,
            AgentFrame::Revoked { reason } => return Err(AgentError::Revoked(reason)),
            AgentFrame::Superseded { connection_id } => return Err(AgentError::Superseded(connection_id)),
            AgentFrame::Error {
                kind,
                message,
                retryable,
            } => return Err(remote_error(kind, message, retryable)),
            _ => {
                return Err(AgentError::Protocol("expected HomeWelcome from the home".to_owned()));
            }
        };
        let AgentFrame::HomeWelcome {
            protocol,
            home_id: welcome_home,
            node,
            connection_id: welcome_connection,
        } = welcome
        else {
            return Err(AgentError::Protocol("expected HomeWelcome".to_owned()));
        };
        if protocol != AGENT_PROTOCOL {
            return Err(AgentError::Protocol(format!(
                "unsupported home protocol {protocol}; this agent speaks {AGENT_PROTOCOL}"
            )));
        }
        if welcome_home != home_id || welcome_connection != connection_id {
            return Err(AgentError::Auth("HomeWelcome does not match the challenge".to_owned()));
        }
        if node != config.node {
            return Err(AgentError::Auth(format!(
                "home welcomed node {node}, but this agent enrolled as {}",
                config.node
            )));
        }

        let heartbeat_secs = if heartbeat_secs == 0 {
            HEARTBEAT_SECS
        } else {
            heartbeat_secs
        };
        self.connection = Some(Connection {
            stream,
            heartbeat_interval: Duration::from_secs(heartbeat_secs),
            max_frame_bytes: usize::try_from(max_frame_bytes).unwrap_or(MAX_FRAME_BYTES),
        });
        self.record_transport_fingerprint()?;
        Ok(())
    }

    /// Records the first system-mode WSS leaf fingerprint in `agent.json`.
    ///
    /// The pin mode stores no observation: its fingerprint is already the configured hard
    /// pin. A changed system-mode fingerprint is refused by the transport before this runs.
    fn record_transport_fingerprint(&mut self) -> Result<(), AgentError> {
        let Some(fingerprint) = self.transport.recorded_fingerprint() else {
            return Ok(());
        };
        let Some(config) = self.config.as_mut() else {
            return Ok(());
        };
        if let TransportConfig::Wss {
            tls: crate::config::WssTlsMode::System,
            fingerprint: stored,
            ..
        } = &mut config.transport
        {
            if stored.as_deref() == Some(fingerprint.as_str()) {
                return Ok(());
            }
            *stored = Some(fingerprint);
            config.write()?;
        }
        Ok(())
    }

    /// Reports path-map changes since the last report.
    ///
    /// `vowp agent paths add` and `rm` edit `agent.json` while `vowp agent run` is connected.
    /// Each heartbeat reloads the file and sends one [`AgentFrame::PathMapUpdate`] for every
    /// project that was added or changed. A home that predates the frame answers `Error`; the
    /// client then stops sending updates on this process instead of reconnecting in a loop.
    pub fn refresh_path_maps(&mut self) -> Result<usize, AgentError> {
        if self.connection.is_none() || !self.path_map_updates {
            return Ok(0);
        }
        let current = match AgentConfig::load(&self.state_dir) {
            Ok(config) => config.path_maps,
            Err(AgentError::NotEnrolled(_)) => return Ok(0),
            // `vowp agent paths add|rm` rewrites the file in place. A heartbeat that catches
            // the write mid-flight, or a hand edit that is not valid yet, keeps the last good
            // map and retries on the next heartbeat instead of ending the run loop.
            Err(AgentError::Json(_) | AgentError::Config(_)) => return Ok(0),
            Err(error) => return Err(error),
        };
        if let Some(config) = self.config.as_mut() {
            config.path_maps = current.clone();
        }
        let changed: Vec<AgentPathMap> = current
            .iter()
            .filter(|map| !self.sent_path_maps.iter().any(|sent| sent == *map))
            .cloned()
            .collect();
        if changed.is_empty() {
            self.sent_path_maps = current;
            return Ok(0);
        }
        for map in &changed {
            self.send_frame(&AgentFrame::PathMapUpdate { map: map.clone() })?;
        }
        self.sent_path_maps = current;
        self.awaiting_path_map_reply = true;
        Ok(changed.len())
    }

    /// Sends one heartbeat and updates `status.json`.
    pub fn heartbeat(&mut self) -> Result<Timestamp, AgentError> {
        let now = self.clock.now();
        let spool = pending_spool_bytes(&self.state_dir);
        self.send_frame(&AgentFrame::Heartbeat {
            at: now,
            capacity: collect_capacity(&self.state_dir),
            pending_spool_bytes: spool,
        })?;
        self.refresh_path_maps()?;
        self.status.connection = ConnectionState::Connected;
        self.status.last_heartbeat = Some(now);
        self.status.pending_spool_bytes = spool;
        if self
            .cache_pruned_at
            .is_none_or(|at| now.since(at) >= CACHE_PRUNE_INTERVAL)
        {
            self.cache.prune(now);
            self.cache_pruned_at = Some(now);
        }
        self.status.refresh_cache(&self.state_dir);
        if let Some(expiry) = surface_token_expiry(&self.state_dir) {
            self.status.surface_token_expires_at = Some(expiry);
        }
        self.save_status()?;
        Ok(now)
    }

    /// Runs until the home revokes the node or another connection takes over.
    ///
    /// This Phase 0 entry point has no executor behind it: an `AssignRun` frame is refused
    /// with [`RunRejectReason::Other`](vowp_protocol::RunRejectReason::Other) and never
    /// starts a handler. Use [`AgentClient::run_with_handler`] to run assignments.
    pub fn run(&mut self, options: &RunOptions) -> Result<RunOutcome, AgentError> {
        self.run_with(RunDriver::rejecting(), options)
    }

    /// Runs the connection loop with an injected [`RunHandler`].
    ///
    /// `AssignRun` frames are validated and accepted, the handler runs off the frame loop,
    /// every line is spooled before it is sent, and a reconnect replays the first
    /// unacknowledged line. The lease token stays in memory and is never handed to the
    /// handler.
    pub fn run_with_handler(
        &mut self,
        options: &RunOptions,
        handler: Arc<dyn RunHandler>,
    ) -> Result<RunOutcome, AgentError> {
        self.run_with(RunDriver::new(handler), options)
    }

    /// Runs the `--service` mode: writes the launchd restart marker, drains on `stop` and
    /// removes the marker after a terminal `Revoked` or `Superseded`.
    ///
    /// The caller installs the SIGTERM handler that sets `stop`; this library never installs
    /// one. A drain flushes the spool, sends `SurfaceDetach` for every attached surface (the
    /// home releases the surface lease on that frame), stops any app host it started, writes
    /// `connection = stopped`, and exits 0.
    pub fn run_as_service(
        &mut self,
        options: &RunOptions,
        handler: Arc<dyn RunHandler>,
        stop: Arc<AtomicBool>,
    ) -> Result<ServiceRunOutcome, AgentError> {
        crate::service::prepare_service_run(&self.state_dir)?;
        let outcome = self.run_with_stop(RunDriver::new(handler), options, Some(Arc::clone(&stop)))?;
        if matches!(
            outcome,
            Some(RunOutcome::Revoked { .. }) | Some(RunOutcome::Superseded { .. })
        ) && let Err(error) = crate::service::remove_run_marker(&self.state_dir)
        {
            debug_failure(
                "the launchd run marker could not be removed",
                &AgentError::Config(error.to_string()),
            );
        }
        Ok(match outcome {
            Some(outcome) => ServiceRunOutcome::Terminal(outcome),
            None => ServiceRunOutcome::Drained,
        })
    }

    fn run_with(&mut self, driver: RunDriver, options: &RunOptions) -> Result<RunOutcome, AgentError> {
        self.run_with_stop(driver, options, None)?
            .ok_or_else(|| AgentError::Config("the run loop stopped without a terminal outcome".to_owned()))
    }

    fn run_with_stop(
        &mut self,
        driver: RunDriver,
        options: &RunOptions,
        stop: Option<Arc<AtomicBool>>,
    ) -> Result<Option<RunOutcome>, AgentError> {
        if self.config.is_none() {
            return Err(AgentError::NotEnrolled("agent.json is missing".to_owned()));
        }
        crate::service::check_run_environment(&self.state_dir)?;
        if let Err(error) = self.start_socket() {
            self.status.connection = ConnectionState::Stopped;
            self.status.last_error = Some(error.to_string());
            self.save_status()?;
            self.orchestrator.stop_all(false);
            return Err(error);
        }
        let outcome = self.run_loop(driver, options, stop.as_ref());
        let _ = self.detach_bridge();
        // Injected and auto-wired values live only for one run. The spool and cursor stay
        // private on disk for inspection and a later backfill.
        for link in &mut self.bridges {
            link.bridge.finish();
        }
        self.settle_pending_orchestrator();
        match &outcome {
            Ok(Some(RunOutcome::Revoked { .. })) => self.orchestrator.revoke(),
            // A takeover leaves the app hosts to the new connection; they are marked
            // degraded by `handle_connection_lost` and re-attached on reconnect.
            Ok(Some(RunOutcome::Superseded { .. })) => {}
            // A normal exit, a drain or a lost transport is a node stop: no app host outlives it.
            _ => self.orchestrator.stop_all(false),
        }
        self.stop_socket();
        outcome
    }

    fn run_loop(
        &mut self,
        mut driver: RunDriver,
        _options: &RunOptions,
        stop: Option<&Arc<AtomicBool>>,
    ) -> Result<Option<RunOutcome>, AgentError> {
        let mut backoff = self.backoff.clone();
        self.status = base_status(&self.state_dir, self.config.as_ref(), &self.key);
        self.status.connection = ConnectionState::Connecting;
        self.save_status()?;

        loop {
            if stop_requested(stop) {
                self.drain()?;
                return Ok(None);
            }
            self.set_socket_connected(false);
            let connected_at = match self.connect() {
                Ok(()) => {
                    self.set_socket_connected(true);
                    self.status.connection = ConnectionState::Connected;
                    self.status.last_error = None;
                    self.save_status()?;
                    Some(self.clock.now())
                }
                Err(AgentError::Revoked(reason)) => {
                    self.handle_connection_lost();
                    self.status.connection = ConnectionState::Revoked;
                    self.status.last_error = Some(reason.clone());
                    self.save_status()?;
                    return Ok(Some(RunOutcome::Revoked { reason }));
                }
                Err(AgentError::Superseded(connection_id)) => {
                    self.handle_connection_lost();
                    self.status.connection = ConnectionState::Superseded;
                    self.status.last_error = Some(format!("superseded by {connection_id}"));
                    self.save_status()?;
                    return Ok(Some(RunOutcome::Superseded { connection_id }));
                }
                Err(error) if error.is_fatal() => {
                    self.handle_connection_lost();
                    self.status.connection = ConnectionState::Stopped;
                    self.status.last_error = Some(error.to_string());
                    self.save_status()?;
                    return Err(error);
                }
                Err(error) => {
                    self.handle_connection_lost();
                    self.status.connection = ConnectionState::Reconnecting;
                    self.status.last_error = Some(error.to_string());
                    self.save_status()?;
                    None
                }
            };

            if let Some(connected_at) = connected_at {
                let served = match driver.resume(self) {
                    Ok(()) => self.serve(&mut driver, stop),
                    Err(error) => serve_outcome_from_error(error),
                };
                if matches!(served, ServeOutcome::Drained) {
                    // The connection is still up: exactly one SurfaceDetach is sent before
                    // the socket is closed, so the home can release the surface lease.
                    self.drain()?;
                    return Ok(None);
                }
                let disconnected = matches!(served, ServeOutcome::Disconnected(_));
                if disconnected {
                    driver.note_disconnect(self);
                }
                self.handle_connection_lost();
                match served {
                    ServeOutcome::Stopped(outcome) => return Ok(Some(outcome)),
                    ServeOutcome::Failed(error) => {
                        self.status.connection = ConnectionState::Stopped;
                        self.status.last_error = Some(error.to_string());
                        self.save_status()?;
                        return Err(error);
                    }
                    ServeOutcome::Disconnected(message) => {
                        self.status.connection = ConnectionState::Reconnecting;
                        self.status.last_error = Some(message);
                        self.save_status()?;
                        if self.clock.now().since(connected_at) >= STABLE_CONNECTION {
                            backoff.reset();
                        }
                    }
                    ServeOutcome::Drained => {}
                }
            }

            let delay = backoff.next_delay();
            self.status.connection = ConnectionState::Reconnecting;
            self.save_status()?;
            self.sleep_unless_stopped(delay, stop);
        }
    }

    /// Sleeps in short slices so a SIGTERM during a long backoff still drains promptly.
    ///
    /// A run without a stop flag sleeps the whole delay at once, exactly as before; the
    /// existing backoff tests inspect one sleep per drop.
    fn sleep_unless_stopped(&self, delay: Duration, stop: Option<&Arc<AtomicBool>>) {
        if stop.is_none() {
            self.clock.sleep(delay);
            return;
        }
        let slice = Duration::from_millis(200);
        let mut remaining = delay;
        while !remaining.is_zero() {
            if stop_requested(stop) {
                return;
            }
            let step = remaining.min(slice);
            self.clock.sleep(step);
            remaining = remaining.saturating_sub(step);
        }
    }

    /// Flushes what can be flushed, detaches every attached surface and writes `stopped`.
    ///
    /// The run spool is append-only and fsynced, so a drain never discards it. Sending
    /// `SurfaceDetach` is the agent's release signal: the home releases the surface lease
    /// when it processes the frame.
    pub fn drain(&mut self) -> Result<(), AgentError> {
        let _ = self.flush_bridge();
        let _ = self.detach_bridge();
        self.orchestrator.stop_all(false);
        self.status.connection = ConnectionState::Stopped;
        self.save_status()?;
        self.stop_socket();
        Ok(())
    }

    fn start_socket(&mut self) -> Result<(), AgentError> {
        if self.socket.is_none() {
            self.socket = Some(AgentSocket::bind(&self.state_dir)?);
        }
        Ok(())
    }

    /// Attaches every bridge surface and replays anything its spool still holds.
    fn attach_bridge(&mut self) -> Result<(), AgentError> {
        let mut attached_any = false;
        let mut index = 0;
        while index < self.bridges.len() {
            let skip = {
                let link = &self.bridges[index];
                link.attached || link.bridge.state().read_only.is_some()
            };
            if !skip {
                let (surface, session) = {
                    let link = &self.bridges[index];
                    (link.bridge.surface().clone(), link.bridge.session().clone())
                };
                self.send_frame(&AgentFrame::SurfaceAttach(SurfaceAttach { surface, session }))?;
                self.bridges[index].attached = true;
                attached_any = true;
            }
            index += 1;
        }
        if !attached_any {
            return Ok(());
        }
        self.flush_bridge()?;
        let mut index = 0;
        while index < self.bridges.len() {
            let active = {
                let link = &self.bridges[index];
                link.attached && link.bridge.state().read_only.is_none()
            };
            if active
                && let Err(error) = self.bridges[index].bridge.attach()
                && error.is_fatal()
            {
                return Err(error);
            }
            index += 1;
        }
        Ok(())
    }

    /// Sends every spooled bridge frame the home has not seen, in spool order.
    fn flush_bridge(&mut self) -> Result<(), AgentError> {
        let mut index = 0;
        while index < self.bridges.len() {
            self.flush_bridge_at(index)?;
            index += 1;
        }
        Ok(())
    }

    /// Flushes the spool of one bridge without borrowing the whole client's bridge list.
    fn flush_bridge_at(&mut self, index: usize) -> Result<(), AgentError> {
        let pending = match self.bridges.get(index) {
            Some(link) => link.bridge.pending_frames()?,
            None => return Ok(()),
        };
        for frame in pending {
            if self.connection.is_none() {
                break;
            }
            if self
                .bridges
                .get(index)
                .is_some_and(|link| link.bridge.state().read_only.is_some())
            {
                break;
            }
            self.send_frame(&frame.as_agent_frame())?;
            if let Some(link) = self.bridges.get_mut(index) {
                link.bridge.mark_sent(frame.native_id())?;
            }
        }
        Ok(())
    }

    /// Reads one short local SSE window from every attached bridge and sends what it produced.
    ///
    /// A local stream failure is not a home-connection failure: the bridge reconnects
    /// from its cursor on the next loop turn. A frame write failure is a connection
    /// failure, and the frame stays in the spool for replay.
    fn pump_bridge(&mut self) -> Result<(), AgentError> {
        if self.connection.is_none() {
            return Ok(());
        }
        let mut index = 0;
        while index < self.bridges.len() {
            let sync = {
                let Some(link) = self.bridges.get_mut(index) else {
                    index += 1;
                    continue;
                };
                if !link.attached || link.bridge.state().read_only.is_some() {
                    index += 1;
                    continue;
                }
                match link.bridge.poll() {
                    Ok(sync) => sync,
                    Err(error) if error.is_fatal() => return Err(error),
                    Err(_) => {
                        index += 1;
                        continue;
                    }
                }
            };
            for frame in sync.frames {
                if self.connection.is_none() {
                    break;
                }
                self.send_frame(&frame.as_agent_frame())?;
                if let Some(link) = self.bridges.get_mut(index) {
                    link.bridge.mark_sent(frame.native_id())?;
                }
            }
            index += 1;
        }
        Ok(())
    }

    /// Flushes, sends `SurfaceDetach` for every attached bridge and stops reading the local apps.
    fn detach_bridge(&mut self) -> Result<(), AgentError> {
        let mut index = 0;
        while index < self.bridges.len() {
            let (attached, surface, session, read_only) = {
                let link = &self.bridges[index];
                (
                    link.attached,
                    link.bridge.surface().clone(),
                    link.bridge.session().clone(),
                    link.bridge.state().read_only.is_some(),
                )
            };
            if !attached {
                self.bridges[index].bridge.close_stream();
                index += 1;
                continue;
            }
            if !read_only {
                self.flush_bridge_at(index)?;
            }
            if self.connection.is_some() {
                self.send_frame(&AgentFrame::SurfaceDetach(SurfaceDetach { surface, session }))?;
            }
            let link = &mut self.bridges[index];
            link.attached = false;
            link.bridge.close_stream();
            index += 1;
        }
        Ok(())
    }

    /// Stops every bridge read-only and keeps its spool for inspection.
    fn surface_read_only(&mut self, reason: String) {
        for link in &mut self.bridges {
            // The surface frame was already sent. Keep `attached` true so the final
            // detach is sent without flushing the pending spool.
            link.bridge.note_lease_lost(reason.clone());
        }
    }

    /// Stops only the bridge for one revoked or refused surface read-only.
    fn bridge_read_only(&mut self, surface: &SurfaceId, reason: String) {
        for link in &mut self.bridges {
            if link.bridge.surface() == surface {
                link.bridge.note_lease_lost(reason.clone());
            }
        }
    }

    fn stop_socket(&mut self) {
        self.set_socket_connected(false);
        self.fail_pending_tool_calls("home_offline", "the agent stopped listening for local tool calls");
        self.fail_pending_pairs("home_offline", "the agent stopped listening for local pairing requests");
        self.socket = None;
    }

    fn set_socket_connected(&self, connected: bool) {
        if let Some(socket) = &self.socket {
            socket.set_connected(connected);
        }
    }

    /// Marks the home connection unusable and answers every queued local request offline.
    fn handle_connection_lost(&mut self) {
        self.set_socket_connected(false);
        self.connection = None;
        self.fail_pending_tool_calls("home_offline", "the home connection is not available");
        self.fail_pending_pairs("home_offline", "the home connection is not available");
        for link in &mut self.bridges {
            link.attached = false;
            link.bridge.close_stream();
        }
        // A prepared start whose worker is still running is reconciled on the next
        // connection; its process is not stopped just because the home went away.
        self.pending_orchestrator = None;
        let _ = self.orchestrator.mark_degraded();
        let _ = self.service_socket();
    }

    /// Handles one home orchestrator command without stalling heartbeats or tool calls.
    ///
    /// A start or resume runs on a worker thread; the frame loop pumps its receiver and
    /// sends the correlated result once the app process and service are ready. A stop is
    /// bounded by the host's stop grace and sends its result immediately.
    fn handle_orchestrator_command(&mut self, command: OrchestratorCommand) -> Result<(), AgentError> {
        match &command {
            OrchestratorCommand::Stop { .. } => {
                // Stop flushes and closes the app's bridge before `orchestrator.stop` sends
                // `SurfaceDetach` and signals the process, so the last spooled line is not
                // lost with the app.
                self.close_orchestrator_bridge(command.name());
                let result = self.orchestrator.stop(
                    &command,
                    &mut ConnectionSurface {
                        connection: &mut self.connection,
                    },
                );
                self.send_frame(&AgentFrame::OrchestratorResult(result))
            }
            OrchestratorCommand::Start { project, .. } => {
                let project_root = self.orchestrator_project_root(project.as_ref());
                self.begin_orchestrator_start(command, project_root)
            }
            OrchestratorCommand::Resume { .. } => self.begin_orchestrator_start(command, None),
        }
    }

    /// Flushes and closes the bridge bound to an orchestrator that is about to stop.
    ///
    /// The bridge is removed from the client's list; its spool and cursor stay on disk for
    /// inspection. `OrchestratorHost::stop` still sends the `SurfaceDetach` frame, so the
    /// home releases the lease even though this bridge no longer does.
    fn close_orchestrator_bridge(&mut self, name: &str) {
        let surface = self
            .orchestrator
            .state(name)
            .ok()
            .flatten()
            .and_then(|state| state.surface)
            .or_else(|| {
                self.orchestrator
                    .config(name)
                    .ok()
                    .flatten()
                    .and_then(|config| config.surface)
            });
        let Some(surface) = surface else {
            return;
        };
        let Some(index) = self.bridges.iter().position(|link| link.bridge.surface() == &surface) else {
            return;
        };
        let read_only = self.bridges[index].bridge.state().read_only.is_some();
        if !read_only {
            let _ = self.flush_bridge_at(index);
        }
        let mut link = self.bridges.remove(index);
        link.bridge.close_stream();
    }

    /// Returns the index of an active bridge for `surface`.
    fn bridge_index_for_surface(&self, surface: &SurfaceId) -> Option<usize> {
        self.bridges.iter().position(|link| link.bridge.surface() == surface)
    }

    /// Opens the local opencode bridge for one running orchestrator host.
    ///
    /// The home's `SurfaceAttach` was already sent by `finish_start` or
    /// `reattach_orchestrators`, so the returned link is marked attached. A missing
    /// `service.json`, a non-opencode app or an unreadable service is not an orchestrator
    /// failure: the host keeps running and a later start or reconnect may wire it.
    fn ensure_orchestrator_bridge(&mut self, name: &str, binding: &SurfaceBinding) -> Option<usize> {
        if let Some(index) = self.bridge_index_for_surface(&binding.surface) {
            if self.bridges[index].bridge.state().read_only.is_none() {
                return Some(index);
            }
            // A read-only bridge cannot recover; a new start or resume loads a fresh
            // reader over the same surface spool and cursor.
            self.bridges.remove(index);
        }
        let config = match self.orchestrator.config(name) {
            Ok(Some(config)) => config,
            Ok(None) => return None,
            Err(error) => {
                debug_failure(
                    "the local orchestrator config could not be read for bridging",
                    &AgentError::Config(error.message),
                );
                return None;
            }
        };
        if config.app != OrchestratorApp::Opencode {
            return None;
        }
        let service_json = config.service_json.as_deref()?;
        let maps = self.path_maps().to_vec();
        let bridge = OpencodeBridge::load(
            Path::new(service_json),
            &self.state_dir,
            binding.surface.clone(),
            binding.session.clone(),
        )
        .and_then(|bridge| bridge.with_path_maps(&maps));
        let bridge = match bridge {
            Ok(bridge) => bridge,
            Err(error) => {
                debug_failure("the local opencode bridge could not be loaded", &error);
                return None;
            }
        };
        self.bridges.push(BridgeLink {
            bridge,
            attached: false,
        });
        Some(self.bridges.len().saturating_sub(1))
    }

    /// Returns the local checkout for a project bound to an orchestrator command.
    fn orchestrator_project_root(&self, project: Option<&ProjectName>) -> Option<PathBuf> {
        let project = project?;
        self.config
            .as_ref()?
            .path_map(project)
            .map(|map| PathBuf::from(&map.local_root))
    }

    fn begin_orchestrator_start(
        &mut self,
        command: OrchestratorCommand,
        project_root: Option<PathBuf>,
    ) -> Result<(), AgentError> {
        if self.pending_orchestrator.is_some() {
            let result = OrchestratorResult::failed(
                command.request_id(),
                OrchestratorNodeState::Failed,
                OrchestratorError::new("busy", "another orchestrator start is still in progress", true),
            );
            return self.send_frame(&AgentFrame::OrchestratorResult(result));
        }
        let host = self.orchestrator.clone();
        let request_id = command.request_id().to_owned();
        let worker_command = command.clone();
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(format!("vowp-orchestrator-{}", command.name()))
            .spawn(move || {
                let outcome = host.prepare_start(&worker_command, project_root.as_deref());
                let _ = sender.send(outcome);
            });
        if spawned.is_err() {
            let result = OrchestratorResult::failed(
                &request_id,
                OrchestratorNodeState::Failed,
                OrchestratorError::new("spawn_failed", "the orchestrator worker could not start", true),
            );
            return self.send_frame(&AgentFrame::OrchestratorResult(result));
        }
        self.pending_orchestrator = Some(PendingOrchestrator { command, receiver });
        Ok(())
    }

    /// Completes a prepared start on the frame loop and sends its result.
    fn pump_orchestrator(&mut self) -> Result<(), AgentError> {
        let Some(pending) = self.pending_orchestrator.take() else {
            return Ok(());
        };
        let outcome = match pending.receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => {
                self.pending_orchestrator = Some(pending);
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let result = OrchestratorResult::failed(
                    pending.command.request_id(),
                    OrchestratorNodeState::Failed,
                    OrchestratorError::new("start_failed", "the orchestrator start worker stopped", true),
                );
                return self.send_frame(&AgentFrame::OrchestratorResult(result));
            }
        };
        let prepared = match &outcome {
            PrepareOutcome::Ready(prepared) => Some((prepared.name.clone(), prepared.app, prepared.binding.clone())),
            PrepareOutcome::Failed(_) => None,
        };
        let result = self.orchestrator.finish_start(
            outcome,
            &mut ConnectionSurface {
                connection: &mut self.connection,
            },
        );
        let running = result.accepted && result.state == OrchestratorNodeState::Running;
        if running
            && let Some((name, OrchestratorApp::Opencode, binding)) = prepared
            && let Some(index) = self.ensure_orchestrator_bridge(&name, &binding)
        {
            // `finish_start` already sent `SurfaceAttach` (or the host was already
            // attached by a duplicate start); only the local SSE reader is opened here.
            self.bridges[index].attached = true;
            let _ = self.bridges[index].bridge.attach();
            let _ = self.flush_bridge_at(index);
        }
        self.send_frame(&AgentFrame::OrchestratorResult(result))
    }

    /// Waits briefly for an in-flight start worker so a shutdown cannot leave its app behind.
    fn settle_pending_orchestrator(&mut self) {
        let Some(pending) = self.pending_orchestrator.take() else {
            return;
        };
        let _ = pending
            .receiver
            .recv_timeout(crate::orchestrator::ORCHESTRATOR_START_TIMEOUT + Duration::from_secs(5));
    }

    /// Re-attaches every app host that survived a lost connection.
    fn reattach_orchestrators(&mut self) -> Result<(), AgentError> {
        let live = self
            .orchestrator
            .reconcile()
            .map_err(|error| AgentError::Config(error.message))?;
        for host in live {
            self.send_frame(&AgentFrame::SurfaceAttach(SurfaceAttach {
                surface: host.binding.surface.clone(),
                session: host.binding.session.clone(),
            }))?;
            self.orchestrator
                .mark_running(&host.name)
                .map_err(|error| AgentError::Config(error.message))?;
            if let Some(index) = self.ensure_orchestrator_bridge(&host.name, &host.binding) {
                self.bridges[index].attached = true;
                let _ = self.bridges[index].bridge.attach();
            }
        }
        self.flush_bridge()?;
        Ok(())
    }

    fn service_socket(&mut self) -> Result<(), AgentError> {
        let events = match &self.socket {
            Some(socket) => socket.drain_events(),
            None => return Ok(()),
        };
        for event in events {
            self.handle_socket_event(event)?;
        }
        self.expire_pending_tool_calls();
        self.expire_pending_pairs();
        Ok(())
    }

    fn handle_socket_event(&mut self, event: SocketEvent) -> Result<(), AgentError> {
        match event {
            SocketEvent::Attach {
                surface,
                session,
                reply,
            } => self.forward_surface_frame(AgentFrame::SurfaceAttach(SurfaceAttach { surface, session }), reply),
            SocketEvent::Detach {
                surface,
                session,
                reply,
            } => self.forward_surface_frame(AgentFrame::SurfaceDetach(SurfaceDetach { surface, session }), reply),
            SocketEvent::ToolCall { call, reply } => self.forward_tool_call(call, reply),
            SocketEvent::Pair { request, reply } => self.forward_pair(request, reply),
        }
    }

    /// Forwards one pairing request while the run loop keeps heartbeating.
    fn forward_pair(&mut self, request: SurfacePairRequest, reply: mpsc::Sender<PairReply>) -> Result<(), AgentError> {
        if self.connection.is_none() {
            let _ = reply.send(pair_offline_error());
            return Ok(());
        }
        if self.pending_pairs.contains_key(&request.surface_id) {
            let _ = reply.send(PairReply::error(
                "pair_busy",
                "another pairing request is already using this surface id",
            ));
            return Ok(());
        }
        let frame = AgentFrame::SurfacePair {
            code: request.code.clone(),
            surface_id: request.surface_id.clone(),
            session: request.session.clone(),
            app: request.app.clone(),
        };
        match self.send_frame(&frame) {
            Ok(()) => {
                self.pending_pairs.insert(
                    request.surface_id,
                    PendingPair {
                        session: request.session,
                        reply,
                        deadline: Instant::now() + PAIR_TIMEOUT,
                    },
                );
                Ok(())
            }
            Err(error) => {
                let response = match &error {
                    AgentError::FrameTooLarge { .. } => {
                        PairReply::error("frame_too_large", "the request does not fit the home frame limit")
                    }
                    AgentError::Json(_) => PairReply::error("invalid_request", "the request could not be encoded"),
                    _ => pair_offline_error(),
                };
                let _ = reply.send(response);
                socket_forward_failure(error)
            }
        }
    }

    fn forward_surface_frame(
        &mut self,
        frame: AgentFrame,
        reply: mpsc::Sender<SocketResponse>,
    ) -> Result<(), AgentError> {
        if self.connection.is_none() {
            let _ = reply.send(socket_offline_error());
            return Ok(());
        }
        match self.send_frame(&frame) {
            Ok(()) => {
                let _ = reply.send(SocketResponse::Ok);
                Ok(())
            }
            Err(error) => {
                let response = match &error {
                    AgentError::FrameTooLarge { .. } => SocketResponse::Error {
                        code: "frame_too_large".to_owned(),
                        message: "the request does not fit the home frame limit".to_owned(),
                    },
                    AgentError::Json(_) => SocketResponse::Error {
                        code: "invalid_request".to_owned(),
                        message: "the request could not be encoded".to_owned(),
                    },
                    _ => socket_offline_error(),
                };
                let _ = reply.send(response);
                socket_forward_failure(error)
            }
        }
    }

    fn forward_tool_call(&mut self, call: ToolCall, reply: mpsc::Sender<SocketResponse>) -> Result<(), AgentError> {
        let request_id = call.request_id.clone();
        if self.connection.is_none() {
            let _ = reply.send(socket_offline_tool_result(&request_id));
            return Ok(());
        }
        if self.pending_tool_calls.contains_key(&request_id) {
            let _ = reply.send(SocketResponse::tool_error(
                &request_id,
                "duplicate_request_id",
                "another tool call is already using this request id",
                false,
            ));
            return Ok(());
        }
        // Only a cacheable read needs its surface, tool and arguments kept for a later
        // store; a write tool never reaches the cache.
        let cache_key = is_cacheable_tool(&call.tool).then(|| CacheKey {
            tool: call.tool.clone(),
            surface: call.surface.clone(),
            arguments: call.arguments.clone(),
        });
        match self.send_frame(&AgentFrame::ToolCall(call)) {
            Ok(()) => {
                self.pending_tool_calls.insert(
                    request_id,
                    PendingToolCall {
                        reply,
                        deadline: Instant::now() + self.tool_call_timeout,
                        cache_key,
                    },
                );
                Ok(())
            }
            Err(error) => {
                let response = match &error {
                    AgentError::FrameTooLarge { .. } => SocketResponse::tool_error(
                        &request_id,
                        "frame_too_large",
                        "the request does not fit the home frame limit",
                        false,
                    ),
                    AgentError::Json(_) => SocketResponse::tool_error(
                        &request_id,
                        "invalid_request",
                        "the request could not be encoded",
                        false,
                    ),
                    _ => socket_offline_tool_result(&request_id),
                };
                let _ = reply.send(response);
                socket_forward_failure(error)
            }
        }
    }

    fn complete_tool_call(&mut self, result: ToolResult) {
        let Some(pending) = self.pending_tool_calls.remove(&result.request_id) else {
            return;
        };
        let refused = result
            .error
            .as_ref()
            .filter(|error| is_surface_token_refusal(&error.message))
            .map(|error| error.message.clone());
        let refused_surface = pending
            .cache_key
            .as_ref()
            .map(|key| key.surface.clone())
            .or_else(|| self.refused_surface());
        let stored = match (
            result.error.is_none(),
            pending.cache_key.as_ref(),
            result.result.as_ref(),
        ) {
            (true, Some(key), Some(payload)) if is_successful_result(payload) => Some((key, payload.clone())),
            _ => None,
        };
        let response = SocketResponse::ToolResult {
            request_id: result.request_id,
            result: result.result,
            error: result.error,
        };
        let _ = pending.reply.send(response);
        if let (Some(message), Some(surface)) = (refused, refused_surface)
            && let Err(error) = self.mark_surface_needs_pair(&surface, &message)
        {
            debug_failure("surface needs_pair write after a tool refusal failed", &error);
        }
        // The reply is already on its way; a cache failure must never fail the tool call.
        if let Some((key, payload)) = stored
            && let Err(error) = self
                .cache
                .store(&key.tool, &key.surface, &key.arguments, &payload, self.clock.now())
        {
            debug_failure("cache store failed", &error);
        }
    }

    fn expire_pending_tool_calls(&mut self) {
        if self.pending_tool_calls.is_empty() {
            return;
        }
        let now = Instant::now();
        let expired: Vec<String> = self
            .pending_tool_calls
            .iter()
            .filter(|(_, pending)| pending.deadline <= now)
            .map(|(request_id, _)| request_id.clone())
            .collect();
        for request_id in expired {
            if let Some(pending) = self.pending_tool_calls.remove(&request_id) {
                let _ = pending.reply.send(SocketResponse::tool_error(
                    &request_id,
                    "tool_timeout",
                    "the home did not answer the tool call before the timeout",
                    true,
                ));
            }
        }
    }

    fn fail_pending_tool_calls(&mut self, code: &str, message: &str) {
        let pending: Vec<(String, PendingToolCall)> = self.pending_tool_calls.drain().collect();
        for (request_id, call) in pending {
            let _ = call
                .reply
                .send(SocketResponse::tool_error(&request_id, code, message, true));
        }
    }

    /// Answers every pairing request still waiting for the home with one typed failure.
    fn fail_pending_pairs(&mut self, code: &str, message: &str) {
        let pending: Vec<(SurfaceId, PendingPair)> = self.pending_pairs.drain().collect();
        for (_, pair) in pending {
            let _ = pair.reply.send(PairReply::error(code, message));
        }
    }

    /// Expires pairing requests the home never answered.
    fn expire_pending_pairs(&mut self) {
        if self.pending_pairs.is_empty() {
            return;
        }
        let now = Instant::now();
        let expired: Vec<SurfaceId> = self
            .pending_pairs
            .iter()
            .filter(|(_, pair)| pair.deadline <= now)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in expired {
            if let Some(pair) = self.pending_pairs.remove(&surface) {
                let _ = pair.reply.send(PairReply::error(
                    "pair_timeout",
                    "the home did not answer the surface pairing request",
                ));
            }
        }
    }

    /// Writes the token, expiry and session binding for one socket-originated pairing.
    fn complete_pending_pair(
        &mut self,
        surface_id: SurfaceId,
        token: String,
        scopes: Vec<SurfaceScope>,
        expires_at: Timestamp,
        pair: PendingPair,
    ) -> Result<(), AgentError> {
        validate_surface_id(&surface_id)?;
        if token.is_empty() {
            let _ = pair.reply.send(PairReply::error(
                "pair_failed",
                "the home returned an empty surface token",
            ));
            return Err(AgentError::Protocol(
                "the home returned an empty surface token".to_owned(),
            ));
        }
        let token_path = write_surface_token(&self.state_dir, &surface_id, &token, expires_at)?;
        write_surface_binding(&self.state_dir, &surface_id, &pair.session)?;
        mark_paired(&self.state_dir, &surface_id)?;
        // A fresh pairing replaces every read cached before it, so a re-paired surface
        // can never read data fetched under the previous token.
        if let Err(error) = self.cache.clear_surface(&surface_id) {
            debug_failure("cache clear after socket pairing failed", &error);
        }
        self.status.surface_token_expires_at = surface_token_expiry(&self.state_dir);
        self.save_status()?;
        let _ = pair.reply.send(PairReply::Paired {
            surface_id,
            scopes,
            expires_at,
            token_path,
        });
        Ok(())
    }

    /// Returns the current local status, refreshing spool and surface expiry.
    pub fn status(&self) -> AgentStatus {
        let mut status = self.status.clone();
        status.pending_spool_bytes = pending_spool_bytes(&self.state_dir);
        status.refresh_cache(&self.state_dir);
        if let Some(expiry) = surface_token_expiry(&self.state_dir) {
            status.surface_token_expires_at = Some(expiry);
        }
        status
    }

    /// Returns the agent state directory.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the node name this agent enrolled as.
    pub fn node(&self) -> NodeName {
        self.config
            .as_ref()
            .map(|config| config.node.clone())
            .or_else(|| self.status.node.clone())
            // invariant: "agent" is a valid node slug and an enrolled client has one.
            .unwrap_or_else(|| NodeName::parse("agent").expect("invariant: 'agent' is a valid node slug"))
    }

    /// Sleeps through the injected clock (tests advance fake time).
    pub(crate) fn sleep(&self, duration: Duration) {
        self.clock.sleep(duration);
    }

    /// Returns the injected clock's current instant.
    pub(crate) fn clock_now(&self) -> Timestamp {
        self.clock.now()
    }

    fn serve(&mut self, driver: &mut RunDriver, stop: Option<&Arc<AtomicBool>>) -> ServeOutcome {
        if let Err(error) = self.heartbeat() {
            return disconnected_or_failed(error);
        }
        if let Err(error) = self.reattach_orchestrators() {
            return disconnected_or_failed(error);
        }
        if let Err(error) = self.attach_bridge() {
            return serve_outcome_from_error(error);
        }
        loop {
            if stop_requested(stop) {
                return ServeOutcome::Drained;
            }
            if let Err(error) = driver.pump(self) {
                return serve_outcome_from_error(error);
            }
            let interval = self
                .connection
                .as_ref()
                .map(|connection| connection.heartbeat_interval)
                .unwrap_or(Duration::from_secs(HEARTBEAT_SECS));
            let last = self.status.last_heartbeat.unwrap_or_else(|| self.clock.now());
            let mut remaining = interval.saturating_sub(self.clock.now().since(last));
            if driver.has_open_runs() {
                remaining = remaining.min(crate::executor::RUN_POLL);
            }
            let read = match self.recv_frame(Some(remaining)) {
                Ok(read) => read,
                Err(error) => return disconnected_or_failed(error),
            };
            match read {
                FrameRead::Timeout => {
                    if self.clock.now().since(last) >= interval
                        && let Err(error) = self.heartbeat()
                    {
                        return disconnected_or_failed(error);
                    }
                }
                FrameRead::Closed => return ServeOutcome::Disconnected("connection closed".to_owned()),
                FrameRead::Frame(frame) => {
                    match frame {
                        AgentFrame::Ping => {
                            if let Err(error) = self.send_frame(&AgentFrame::Pong) {
                                return disconnected_or_failed(error);
                            }
                        }
                        AgentFrame::AssignRun {
                            assignment,
                            lease_token,
                            lease_expires_at,
                        } => {
                            if let Err(error) = driver.handle_assign(self, assignment, lease_token, lease_expires_at) {
                                return disconnected_or_failed(error);
                            }
                        }
                        AgentFrame::CancelRun { run_id, epoch, reason } => {
                            driver.handle_cancel(self, &run_id, epoch, &reason)
                        }
                        AgentFrame::RunLost { run_id, reason } => driver.handle_run_lost(self, &run_id, &reason),
                        AgentFrame::Cleanup { run_id, job, .. } => driver.handle_cleanup(self, &run_id, &job),
                        AgentFrame::Credit { run_id, count } => driver.handle_credit(&run_id, count),
                        AgentFrame::BundleAccept { run_id, epoch, offset } => {
                            driver.handle_bundle_accept(&run_id, epoch, offset)
                        }
                        AgentFrame::BundleAbort { run_id, epoch, reason } => {
                            driver.handle_bundle_abort(&run_id, epoch, &reason)
                        }
                        AgentFrame::IngestAck {
                            run_id,
                            event_id,
                            seq,
                            duplicate,
                            lease_revoked,
                            error,
                        } => driver.handle_ack(&run_id, event_id.as_ref(), seq, duplicate, lease_revoked, error),
                        AgentFrame::LeaseLost { reason } => {
                            let _ = self.orchestrator.mark_lease_lost();
                            self.surface_read_only(reason);
                        }
                        AgentFrame::LeaseRevoked { reason, .. } => {
                            let _ = self.orchestrator.mark_lease_lost();
                            self.surface_read_only(reason);
                        }
                        AgentFrame::SurfaceIssued {
                            surface_id,
                            token,
                            scopes,
                            expires_at,
                        } => {
                            if let Some(pair) = self.pending_pairs.remove(&surface_id) {
                                if let Err(error) =
                                    self.complete_pending_pair(surface_id, token, scopes, expires_at, pair)
                                {
                                    return serve_outcome_from_error(error);
                                }
                            } else if let Err(error) = self.apply_surface_issued(surface_id, token, expires_at) {
                                return serve_outcome_from_error(error);
                            }
                        }
                        AgentFrame::SurfaceRevoked { surface_id, reason } => {
                            if let Some(pair) = self.pending_pairs.remove(&surface_id) {
                                let _ = pair.reply.send(PairReply::error(
                                    "surface_revoked",
                                    format!("surface {surface_id} was revoked: {reason}"),
                                ));
                            }
                            self.forget_surface_token(&surface_id);
                            if let Err(error) = self.cache.clear_surface(&surface_id) {
                                debug_failure("cache clear after surface revocation failed", &error);
                            }
                            if self.bridges.iter().any(|link| link.bridge.surface() == &surface_id) {
                                self.bridge_read_only(&surface_id, reason);
                            }
                            self.orchestrator.stop_surface(&surface_id);
                        }
                        AgentFrame::Revoked { reason } => {
                            self.status.connection = ConnectionState::Revoked;
                            self.status.last_error = Some(reason.clone());
                            let _ = self.detach_bridge();
                            if let Err(error) = self.save_status() {
                                return ServeOutcome::Failed(error);
                            }
                            return ServeOutcome::Stopped(RunOutcome::Revoked { reason });
                        }
                        AgentFrame::Superseded { connection_id } => {
                            self.status.connection = ConnectionState::Superseded;
                            self.status.last_error = Some(format!("superseded by {connection_id}"));
                            let _ = self.detach_bridge();
                            if let Err(error) = self.save_status() {
                                return ServeOutcome::Failed(error);
                            }
                            return ServeOutcome::Stopped(RunOutcome::Superseded { connection_id });
                        }
                        AgentFrame::Error {
                            kind,
                            message,
                            retryable: _,
                        } => {
                            if self.pending_pairs.len() == 1
                                && let Some(surface) = self.pending_pairs.keys().next().cloned()
                                && let Some(pair) = self.pending_pairs.remove(&surface)
                            {
                                let _ = pair.reply.send(PairReply::error(
                                    "pair_failed",
                                    format!("the home refused surface pairing: {message}"),
                                ));
                            }
                            if self.awaiting_path_map_reply
                                && (kind == ErrorKind::InvalidInput || message.contains("unexpected frame"))
                            {
                                // An older home answers an unknown `PathMapUpdate` with `Error`
                                // and often closes the connection. Stop sending map updates for
                                // this run and reconnect; never exit the process for this.
                                self.awaiting_path_map_reply = false;
                                self.path_map_updates = false;
                                self.sent_path_maps = self.path_maps().to_vec();
                                return ServeOutcome::Disconnected(
                                    "the home rejected a path map update; path-map updates are disabled for this run"
                                        .to_owned(),
                                );
                            }
                            if kind == ErrorKind::Refused
                                && is_surface_token_refusal(&message)
                                && let Some(surface) = self.refused_surface()
                            {
                                // A refused surface token is not a refused node key: the
                                // connection stays up, the surface is marked `needs_pair`,
                                // and the dead token is never retried.
                                match self.mark_surface_needs_pair(&surface, &message) {
                                    Ok(refusal) => {
                                        self.fail_pending_tool_calls("surface_needs_pair", &refusal.message);
                                        continue;
                                    }
                                    Err(error) => return ServeOutcome::Failed(error),
                                }
                            }
                            // A home error after the handshake is not a terminal node event:
                            // keep the spool and state, report a redacted reason and reconnect.
                            return ServeOutcome::Disconnected(remote_disconnect_message(kind));
                        }
                        AgentFrame::OrchestratorCommand(command) => {
                            if let Err(error) = self.handle_orchestrator_command(command) {
                                return disconnected_or_failed(error);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub(crate) fn send_frame(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        send_connection_frame(&mut self.connection, frame)
    }

    pub(crate) fn recv_frame(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        let started = Instant::now();
        loop {
            self.service_socket()?;
            self.pump_bridge()?;
            self.pump_orchestrator()?;
            let remaining = timeout.map(|limit| limit.saturating_sub(started.elapsed()));
            // Wake at least every millisecond so an elapsed fake-clock deadline still makes
            // progress instead of spinning on a zero-length read. With a local socket bound
            // the frame loop also wakes often enough to answer a new client promptly.
            let wait = match (self.socket.is_some(), remaining) {
                (true, Some(limit)) => Some(limit.max(Duration::from_millis(1)).min(SOCKET_POLL)),
                (true, None) => Some(SOCKET_POLL),
                (false, Some(limit)) => Some(limit.max(Duration::from_millis(1))),
                (false, None) => None,
            };
            // A start worker may finish while the connection is quiet; pump it at the
            // orchestrator poll interval so its result is not delayed to the next heartbeat.
            let wait = if self.pending_orchestrator.is_some() {
                Some(wait.unwrap_or(ORCHESTRATOR_POLL).min(ORCHESTRATOR_POLL))
            } else {
                wait
            };
            // An attached bridge must be polled while the frame loop waits. The read is
            // bounded by the bridge's own short timeout, so a heartbeat or a pending tool
            // call is never blocked behind a quiet local app.
            let bridge_active = self
                .bridges
                .iter()
                .any(|link| link.attached && link.bridge.state().read_only.is_none());
            let wait = if bridge_active {
                Some(
                    wait.unwrap_or(crate::bridge::SSE_READ_TIMEOUT)
                        .min(crate::bridge::SSE_READ_TIMEOUT),
                )
            } else {
                wait
            };
            let read = self
                .connection
                .as_mut()
                .ok_or_else(|| AgentError::Transport("not connected".to_owned()))?
                .stream
                .recv(wait)?;
            match read {
                // A tool result is answered locally and never reaches the run driver.
                FrameRead::Frame(AgentFrame::ToolResult(result)) => self.complete_tool_call(result),
                other => return Ok(other),
            }
        }
    }

    fn save_status(&self) -> Result<(), AgentError> {
        self.status.write(&self.state_dir)
    }
}

/// Sends one frame on the existing agent connection, enforcing its frame limit.
fn send_connection_frame(connection: &mut Option<Connection>, frame: &AgentFrame) -> Result<(), AgentError> {
    let connection = connection
        .as_mut()
        .ok_or_else(|| AgentError::Transport("not connected".to_owned()))?;
    let size = serde_json::to_vec(frame)?.len() + 1;
    if size > connection.max_frame_bytes {
        return Err(AgentError::FrameTooLarge {
            size,
            max: connection.max_frame_bytes,
        });
    }
    connection.stream.send(frame)
}

/// The surface attach/detach side effects for one orchestrator host.
struct ConnectionSurface<'a> {
    connection: &'a mut Option<Connection>,
}

impl SurfaceAttachment for ConnectionSurface<'_> {
    fn attach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        let frame = AgentFrame::SurfaceAttach(SurfaceAttach {
            surface: binding.surface.clone(),
            session: binding.session.clone(),
        });
        send_connection_frame(self.connection, &frame).map_err(|_| {
            OrchestratorError::new(
                "attach_failed",
                "the surface attach frame could not be sent to the home",
                true,
            )
        })
    }

    fn detach(&mut self, binding: &SurfaceBinding) -> Result<(), OrchestratorError> {
        let frame = AgentFrame::SurfaceDetach(SurfaceDetach {
            surface: binding.surface.clone(),
            session: binding.session.clone(),
        });
        send_connection_frame(self.connection, &frame).map_err(|_| {
            OrchestratorError::new(
                "detach_failed",
                "the surface detach frame could not be sent to the home",
                true,
            )
        })
    }
}

fn socket_offline_error() -> SocketResponse {
    SocketResponse::Error {
        code: "home_offline".to_owned(),
        message: "the agent is not connected to the home; start `vowp agent run`".to_owned(),
    }
}

fn pair_offline_error() -> PairReply {
    PairReply::error(
        "home_offline",
        "the agent is not connected to the home; start `vowp agent run`",
    )
}

fn socket_offline_tool_result(request_id: &str) -> SocketResponse {
    SocketResponse::tool_error(
        request_id,
        "home_offline",
        "the agent is not connected to the home; start `vowp agent run`",
        true,
    )
}

/// A frame that cannot be encoded or does not fit is a local rejection; any other send
/// failure means the home connection is no longer usable.
fn socket_forward_failure(error: AgentError) -> Result<(), AgentError> {
    match error {
        AgentError::FrameTooLarge { .. } | AgentError::Json(_) => Ok(()),
        error => Err(error),
    }
}

/// Validates the one-time enrollment code shape without retaining or printing it.
pub fn validate_enrollment_code(code: &str) -> Result<(), AgentError> {
    validate_code_shape(code, "enrollment")
}

/// Validates the one-time surface pairing code shape without retaining or printing it.
pub fn validate_pairing_code(code: &str) -> Result<(), AgentError> {
    validate_code_shape(code, "pairing")
}

/// Validates the 26-character ULID shape of a surface id before it is used as a path segment.
pub fn validate_surface_id(surface: &SurfaceId) -> Result<(), AgentError> {
    if surface.0.len() == 26 && surface.0.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(AgentError::Config("surface id must be a 26-character ULID".to_owned()))
    }
}

fn validate_code_shape(code: &str, kind: &str) -> Result<(), AgentError> {
    if code.len() == 52 && code.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(AgentError::Config(format!("{kind} code must be 52 characters")))
    }
}

/// Writes the token and its expiry below `state_dir/surfaces/<id>` with private modes.
///
/// The files are written through a private temporary file and renamed, so a crash or a
/// concurrent reader never sees a truncated token. The token file contains a secret.
fn write_surface_token(
    state_dir: &Path,
    surface: &SurfaceId,
    token: &str,
    expires_at: Timestamp,
) -> Result<PathBuf, AgentError> {
    ensure_private_dir(state_dir)?;
    ensure_private_dir(&AgentConfig::surfaces_dir(state_dir))?;
    ensure_private_dir(&AgentConfig::surface_dir(state_dir, surface))?;
    let token_path = AgentConfig::surface_token_path(state_dir, surface);
    write_private_atomic(&token_path, format!("{token}\n").as_bytes())?;
    write_private_atomic(
        &AgentConfig::surface_expiry_path(state_dir, surface),
        expires_at.as_rfc3339().as_bytes(),
    )?;
    Ok(token_path)
}

/// Writes one private file through a temporary file and a rename.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    write_private(&temp, bytes)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(AgentError::Io(error));
    }
    Ok(())
}

fn base_status(state_dir: &Path, config: Option<&AgentConfig>, key: &NodeKey) -> AgentStatus {
    let mut status = AgentStatus {
        connection: ConnectionState::Unknown,
        node: config.map(|config| config.node.clone()),
        home: config.map(|config| config.home.clone()),
        protocol: Some(AGENT_PROTOCOL.to_owned()),
        last_heartbeat: None,
        last_error: None,
        pending_spool_bytes: pending_spool_bytes(state_dir),
        key_fingerprint: Some(key.fingerprint()),
        surface_token_expires_at: surface_token_expiry(state_dir),
        cache_entries: 0,
        cache_bytes: 0,
        cache_oldest_at: None,
    };
    status.refresh_cache(state_dir);
    status
}

/// Returns whether a home result is a successful MCP envelope.
///
/// The protocol-level `ToolResult` success (`error == None`) is not enough: the home's
/// tool layer reports a refused read inside the envelope with `isError: true`.
fn is_successful_result(result: &serde_json::Value) -> bool {
    result.get("isError").and_then(serde_json::Value::as_bool) != Some(true)
}

fn next_handshake_frame(stream: &mut dyn FrameStream) -> Result<AgentFrame, AgentError> {
    match stream.recv(None)? {
        FrameRead::Frame(frame) => Ok(frame),
        FrameRead::Timeout => Err(AgentError::Transport(
            "the home did not answer the handshake".to_owned(),
        )),
        FrameRead::Closed => Err(AgentError::Transport(
            "the home closed the connection during the handshake".to_owned(),
        )),
    }
}

fn disconnected_or_failed(error: AgentError) -> ServeOutcome {
    if error.is_fatal() {
        ServeOutcome::Failed(error)
    } else {
        ServeOutcome::Disconnected(error.to_string())
    }
}

/// Maps a run-loop error to the connection outcome; a revoke or takeover ends the loop.
fn serve_outcome_from_error(error: AgentError) -> ServeOutcome {
    match error {
        AgentError::Revoked(reason) => ServeOutcome::Stopped(RunOutcome::Revoked { reason }),
        AgentError::Superseded(connection_id) => ServeOutcome::Stopped(RunOutcome::Superseded { connection_id }),
        error => disconnected_or_failed(error),
    }
}

fn remote_error(kind: ErrorKind, message: String, retryable: bool) -> AgentError {
    if kind == ErrorKind::Auth {
        AgentError::Auth(message)
    } else {
        AgentError::Remote {
            kind,
            message,
            retryable,
        }
    }
}

/// Returns the redacted `last_error` recorded for a post-handshake home error.
fn remote_disconnect_message(kind: ErrorKind) -> String {
    format!("the home returned an error ({kind:?}); the agent is reconnecting")
}

/// Returns whether a home refusal names a surface token rather than the node key.
///
/// The `Error` frame has no structured surface field (the frame does not change), so the
/// home's refusal text is the discriminator. A refused node key is still a disconnect.
fn is_surface_token_refusal(message: &str) -> bool {
    message.trim_start().starts_with("surface token")
}

fn capabilities(path_maps: &[AgentPathMap]) -> AgentCapabilities {
    AgentCapabilities {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        agent_version: AGENT_VERSION.to_owned(),
        harnesses: BTreeMap::new(),
        git_version: None,
        labels: Vec::new(),
        secret_names: Vec::new(),
        projects: path_maps.iter().map(|map| map.project.clone()).collect(),
    }
}

fn collect_capacity(state_dir: &Path) -> AgentCapacity {
    let cpu_cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(u32::MAX as usize) as u32;
    let (mem_total_bytes, mem_free_bytes) = read_meminfo();
    AgentCapacity {
        cpu_cores,
        load1: read_load1(),
        mem_total_bytes,
        mem_free_bytes,
        disk_free_bytes: read_disk_free(state_dir),
        busy_slots: 0,
        max_slots: cpu_cores.max(1),
        on_battery: None,
        thermal_state: None,
    }
}

fn read_meminfo() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let total = meminfo_value(&text, "MemTotal:").unwrap_or(0);
    let free = meminfo_value(&text, "MemAvailable:")
        .or_else(|| meminfo_value(&text, "MemFree:"))
        .unwrap_or(0);
    (total, free)
}

fn meminfo_value(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != key {
            return None;
        }
        let kib = fields.next()?.parse::<u64>().ok()?;
        Some(kib.saturating_mul(1024))
    })
}

fn read_load1() -> f64 {
    let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
        return 0.0;
    };
    text.split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn read_disk_free(state_dir: &Path) -> u64 {
    let Ok(output) = std::process::Command::new("df").arg("-Pk").arg(state_dir).output() else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().nth(1) else {
        return 0;
    };
    line.split_whitespace()
        .nth(3)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    use super::*;
    use crate::key::handshake_payload;

    #[derive(Clone)]
    struct TestClock {
        inner: Arc<Mutex<TestClockState>>,
    }

    struct TestClockState {
        now: Timestamp,
        sleeps: Vec<Duration>,
    }

    impl TestClock {
        fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(TestClockState {
                    now: Timestamp::from_unix_secs(1_700_000_000),
                    sleeps: Vec::new(),
                })),
            }
        }

        fn advance(&self, duration: Duration) {
            let mut state = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.now = state.now.plus(duration);
        }

        fn sleeps(&self) -> Vec<Duration> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sleeps
                .clone()
        }
    }

    impl AgentClock for TestClock {
        fn now(&self) -> Timestamp {
            self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).now
        }

        fn sleep(&self, duration: Duration) {
            let mut state = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.sleeps.push(duration);
            state.now = state.now.plus(duration);
        }
    }

    #[derive(Clone)]
    struct FakeConfig {
        node: NodeName,
        home_id: String,
        protocol: String,
        heartbeat_secs: u64,
        max_frame_bytes: u64,
        drop_connections: usize,
        reject_signature: bool,
        replay_nonce: bool,
        revoke_after_heartbeats: Option<usize>,
        supersede_after_welcome: bool,
    }

    impl Default for FakeConfig {
        fn default() -> Self {
            Self {
                node: NodeName::parse("mac-studio").expect("node slug"),
                home_id: "home-1".to_owned(),
                protocol: AGENT_PROTOCOL.to_owned(),
                heartbeat_secs: HEARTBEAT_SECS,
                max_frame_bytes: MAX_FRAME_BYTES as u64,
                drop_connections: 0,
                reject_signature: false,
                replay_nonce: false,
                revoke_after_heartbeats: None,
                supersede_after_welcome: false,
            }
        }
    }

    #[derive(Default)]
    struct FakeState {
        connections: usize,
        received: Vec<AgentFrame>,
        auth_ok: usize,
        auth_bad: usize,
        heartbeats: usize,
    }

    #[derive(Clone)]
    struct FakeHome {
        shared: Arc<Mutex<FakeState>>,
        config: FakeConfig,
        clock: TestClock,
    }

    impl FakeHome {
        fn new(config: FakeConfig, clock: TestClock) -> Self {
            Self {
                shared: Arc::new(Mutex::new(FakeState::default())),
                config,
                clock,
            }
        }

        fn frames(&self) -> Vec<AgentFrame> {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .received
                .clone()
        }

        fn connections(&self) -> usize {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connections
        }

        fn auth_ok(&self) -> usize {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .auth_ok
        }

        fn auth_bad(&self) -> usize {
            self.shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .auth_bad
        }
    }

    impl Transport for FakeHome {
        fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
            let connection = {
                let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.connections += 1;
                state.connections
            };
            Ok(Box::new(FakeStream {
                shared: Arc::clone(&self.shared),
                config: self.config.clone(),
                clock: self.clock.clone(),
                connection,
                outbound: VecDeque::new(),
                key_id: None,
                challenge: None,
                close_after_welcome: false,
                heartbeats: 0,
            }))
        }
    }

    struct FakeStream {
        shared: Arc<Mutex<FakeState>>,
        config: FakeConfig,
        clock: TestClock,
        connection: usize,
        outbound: VecDeque<AgentFrame>,
        key_id: Option<String>,
        challenge: Option<AgentFrame>,
        close_after_welcome: bool,
        heartbeats: usize,
    }

    impl FakeStream {
        fn verify_auth(&self, signature: &str) -> bool {
            let Some(key_id) = &self.key_id else {
                return false;
            };
            let Some(AgentFrame::HomeChallenge {
                nonce,
                home_id,
                connection_id,
                ..
            }) = &self.challenge
            else {
                return false;
            };
            let Ok(signature) = hex::decode(signature) else {
                return false;
            };
            let Ok(signature) = Signature::from_slice(&signature) else {
                return false;
            };
            let Ok(public) = hex::decode(key_id) else {
                return false;
            };
            let Ok(public) = <[u8; 32]>::try_from(public.as_slice()) else {
                return false;
            };
            let Ok(key) = VerifyingKey::from_bytes(&public) else {
                return false;
            };
            key.verify(&handshake_payload(nonce, home_id, connection_id), &signature)
                .is_ok()
        }
    }

    impl FrameStream for FakeStream {
        fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
            if let Some(frame) = self.outbound.pop_front() {
                return Ok(FrameRead::Frame(frame));
            }
            if self.close_after_welcome {
                return Ok(FrameRead::Closed);
            }
            match timeout {
                Some(duration) => {
                    self.clock.advance(duration);
                    Ok(FrameRead::Timeout)
                }
                None => Ok(FrameRead::Closed),
            }
        }

        fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
            {
                let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.received.push(frame.clone());
            }
            match frame {
                AgentFrame::Enroll { public_key, .. } => {
                    if public_key.len() == 64 && public_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        self.outbound.push_back(AgentFrame::Enrolled {
                            home_id: self.config.home_id.clone(),
                            node: self.config.node.clone(),
                            protocol: self.config.protocol.clone(),
                            server_time: Timestamp::from_unix_secs(1_700_000_000),
                            heartbeat_secs: self.config.heartbeat_secs,
                        });
                    } else {
                        self.outbound.push_back(AgentFrame::Error {
                            kind: ErrorKind::InvalidInput,
                            message: "public key must be 32 bytes in lowercase hex".to_owned(),
                            retryable: false,
                        });
                    }
                }
                AgentFrame::AgentHello { key_id, .. } => {
                    self.key_id = Some(key_id.clone());
                    let nonce = if self.config.replay_nonce {
                        "replayed-nonce".to_owned()
                    } else {
                        format!("nonce-{}", self.connection)
                    };
                    let challenge = AgentFrame::HomeChallenge {
                        nonce,
                        home_id: self.config.home_id.clone(),
                        connection_id: format!("connection-{}", self.connection),
                        server_time: Timestamp::from_unix_secs(1_700_000_000),
                        heartbeat_secs: self.config.heartbeat_secs,
                        max_frame_bytes: self.config.max_frame_bytes,
                    };
                    self.challenge = Some(challenge.clone());
                    self.outbound.push_back(challenge);
                }
                AgentFrame::AgentAuth { signature } => {
                    let valid = self.verify_auth(signature);
                    if valid && !self.config.reject_signature {
                        self.shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .auth_ok += 1;
                        self.outbound.push_back(AgentFrame::HomeWelcome {
                            protocol: self.config.protocol.clone(),
                            home_id: self.config.home_id.clone(),
                            node: self.config.node.clone(),
                            connection_id: format!("connection-{}", self.connection),
                        });
                        if self.connection <= self.config.drop_connections {
                            self.close_after_welcome = true;
                        }
                        if self.config.supersede_after_welcome {
                            self.outbound.push_back(AgentFrame::Superseded {
                                connection_id: "newer-connection".to_owned(),
                            });
                        }
                    } else {
                        self.shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .auth_bad += 1;
                        self.outbound.push_back(AgentFrame::Error {
                            kind: ErrorKind::Auth,
                            message: "bad signature".to_owned(),
                            retryable: false,
                        });
                    }
                }
                AgentFrame::Heartbeat { .. } => {
                    self.heartbeats += 1;
                    self.shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .heartbeats += 1;
                    if self.connection > self.config.drop_connections
                        && self.config.revoke_after_heartbeats == Some(self.heartbeats)
                    {
                        self.outbound.push_back(AgentFrame::Revoked {
                            reason: "revoked by owner".to_owned(),
                        });
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    fn test_config(dir: &Path) -> AgentConfig {
        AgentConfig::new(
            "home-1",
            NodeName::parse("mac-studio").expect("node slug"),
            dir.to_path_buf(),
            "home.example",
        )
    }

    fn enrolled_client(dir: &Path, home: &FakeHome, clock: &TestClock) -> AgentClient {
        let key = NodeKey::load_or_generate(dir).expect("node key");
        AgentClient::new(
            dir.to_path_buf(),
            key,
            test_config(dir),
            Box::new(home.clone()),
            Arc::new(clock.clone()),
        )
    }

    fn unenrolled_client(dir: &Path, home: &FakeHome, clock: &TestClock) -> AgentClient {
        let key = NodeKey::load_or_generate(dir).expect("node key");
        AgentClient::unenrolled(dir.to_path_buf(), key, Box::new(home.clone()), Arc::new(clock.clone()))
    }

    #[test]
    fn agent_enroll_sends_a_public_key_and_stores_the_node() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let home = FakeHome::new(FakeConfig::default(), clock.clone());
        let key = NodeKey::load_or_generate(dir.path()).expect("node key");
        let public_key = key.public_key_hex();
        let mut client = unenrolled_client(dir.path(), &home, &clock);
        let code = "A".repeat(52);

        let enrollment = client.enroll("home.example", &code).expect("enroll");
        assert_eq!(enrollment.node.as_ref(), "mac-studio");
        assert_eq!(enrollment.home, "home-1");
        assert_eq!(enrollment.protocol, AGENT_PROTOCOL);

        let config = AgentConfig::load(dir.path()).expect("stored config");
        assert_eq!(config.node, enrollment.node);
        assert_eq!(config.home, enrollment.home);
        assert_eq!(home.connections(), 2, "enroll then reconnect and handshake");

        let enroll = home
            .frames()
            .into_iter()
            .find_map(|frame| match frame {
                AgentFrame::Enroll { public_key, code, .. } => Some((public_key, code)),
                _ => None,
            })
            .expect("Enroll frame");
        assert_eq!(enroll.0, public_key);
        assert_eq!(enroll.1, code);

        let mode = std::fs::metadata(AgentConfig::file_path(dir.path()))
            .expect("agent.json")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "agent.json must be private");
        assert!(AgentStatus::load(dir.path()).expect("status").is_some());
    }

    #[test]
    fn agent_handshake_rejects_a_bad_signature() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            reject_signature: true,
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock);

        let error = client.connect().expect_err("bad signature is rejected");
        assert!(matches!(error, AgentError::Auth(_)), "got {error:?}");
        assert_eq!(home.auth_bad(), 1);
        assert_eq!(home.auth_ok(), 0);
    }

    #[test]
    fn agent_handshake_rejects_a_replayed_nonce() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            replay_nonce: true,
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock);

        client.connect().expect("first handshake");
        let error = client.connect().expect_err("replayed nonce is refused");
        assert!(matches!(error, AgentError::ReplayNonce), "got {error:?}");
        let auth_frames = home
            .frames()
            .into_iter()
            .filter(|frame| matches!(frame, AgentFrame::AgentAuth { .. }))
            .count();
        assert_eq!(auth_frames, 1, "the replay must not be signed");
    }

    #[test]
    fn agent_reconnects_with_backoff_after_a_drop() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            drop_connections: 2,
            revoke_after_heartbeats: Some(1),
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock).with_backoff(Backoff::seeded(7));

        let outcome = client
            .run(&RunOptions::new(AgentRole::Both))
            .expect("run stops on revoke");
        assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");
        assert_eq!(home.connections(), 3, "two drops, then the revoking connection");

        let sleeps = clock.sleeps();
        assert_eq!(sleeps.len(), 2, "one delay per drop");
        assert!(
            sleeps[0] >= Duration::from_secs(1) && sleeps[0] <= Duration::from_millis(1250),
            "first delay {sleeps:?}"
        );
        assert!(
            sleeps[1] >= Duration::from_secs(2) && sleeps[1] <= Duration::from_millis(2500),
            "second delay {sleeps:?}"
        );
        assert!(sleeps[1] > sleeps[0], "backoff must grow: {sleeps:?}");
    }

    #[test]
    fn agent_heartbeats_on_the_interval() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            revoke_after_heartbeats: Some(3),
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock);

        let outcome = client
            .run(&RunOptions::new(AgentRole::Executor))
            .expect("run stops on revoke");
        assert!(matches!(outcome, RunOutcome::Revoked { .. }), "got {outcome:?}");

        let heartbeats: Vec<Timestamp> = home
            .frames()
            .into_iter()
            .filter_map(|frame| match frame {
                AgentFrame::Heartbeat { at, .. } => Some(at),
                _ => None,
            })
            .collect();
        assert_eq!(heartbeats.len(), 3, "one immediate and two interval heartbeats");
        assert_eq!(heartbeats[1].since(heartbeats[0]), Duration::from_secs(10));
        assert_eq!(heartbeats[2].since(heartbeats[1]), Duration::from_secs(10));
        assert!(clock.sleeps().is_empty(), "a healthy connection does not back off");

        let status = AgentStatus::load(dir.path()).expect("status").expect("status written");
        assert_eq!(status.connection, ConnectionState::Revoked);
        assert_eq!(status.last_heartbeat, Some(heartbeats[2]));
        assert_eq!(status.node, Some(NodeName::parse("mac-studio").expect("node slug")));
    }

    #[test]
    fn agent_stops_on_revoked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            revoke_after_heartbeats: Some(1),
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock);

        let outcome = client.run(&RunOptions::new(AgentRole::Both)).expect("run");
        match outcome {
            RunOutcome::Revoked { reason } => assert_eq!(reason, "revoked by owner"),
            other => panic!("expected Revoked, got {other:?}"),
        }
        let status = AgentStatus::load(dir.path()).expect("status").expect("status written");
        assert_eq!(status.connection, ConnectionState::Revoked);
        assert!(clock.sleeps().is_empty());
    }

    #[test]
    fn agent_stops_on_superseded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let clock = TestClock::new();
        let config = FakeConfig {
            supersede_after_welcome: true,
            ..FakeConfig::default()
        };
        let home = FakeHome::new(config, clock.clone());
        let mut client = enrolled_client(dir.path(), &home, &clock);

        let outcome = client.run(&RunOptions::new(AgentRole::Both)).expect("run");
        match outcome {
            RunOutcome::Superseded { connection_id } => assert_eq!(connection_id, "newer-connection"),
            other => panic!("expected Superseded, got {other:?}"),
        }
        let status = AgentStatus::load(dir.path()).expect("status").expect("status written");
        assert_eq!(status.connection, ConnectionState::Superseded);
        assert!(clock.sleeps().is_empty());
    }
}
