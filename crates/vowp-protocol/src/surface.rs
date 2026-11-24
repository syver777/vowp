//! Surfaces, leases, remote execution lines and orchestrator cards.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.2–§7.6 and §13.1. The wire shapes are
//! fixed here; leases, remote ingest and app hosts build on them.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capacity::RemoteUsage;
use crate::cost::Cost;
use crate::error::{ErrorKind, ProtocolError};
use crate::event::{Actor, EventSource, EventType, LimitEvent, StopReason};
use crate::ids::{CapsuleId, EventId, NodeName, ProjectName, RunId, SessionId};
use crate::run::RunOutcome;
use crate::time::Timestamp;

/// A surface instance id (ULID, 26 characters).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(with = "String")]
pub struct SurfaceId(pub String);

impl SurfaceId {
    /// Returns a fresh surface id.
    pub fn new() -> Self {
        Self(ulid::Ulid::generate().to_string())
    }
}

impl Default for SurfaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SurfaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returns a fresh lease token: two fresh ULIDs concatenated (52 characters, 160 random bits).
///
/// The token is never logged or written to the snapshot.
pub fn new_lease_token() -> String {
    format!("{}{}", ulid::Ulid::generate(), ulid::Ulid::generate())
}

/// Which kind of client holds a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    /// The `vowp chat` fallback.
    Chat,
    /// An opencode app or server.
    Opencode,
    /// A Claude Code app.
    ClaudeCode,
    /// A `vowp mcp` server.
    Mcp,
    /// A transcript mirror.
    Mirror,
    /// A direct API client.
    Api,
    /// A v0.4 local surface attached through a `vowp agent`.
    Agent,
    /// A desktop UI that talks to an orchestrator's app engine itself.
    ///
    /// Wire spelling `desktop_ui`; the store and the CLI spell it `desktop-ui`.
    DesktopUi,
}

impl SurfaceKind {
    /// Returns the human-facing name used in saved surface labels and owner views.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chat => "VOWP Chat",
            Self::Opencode => "OpenCode",
            Self::ClaudeCode => "Claude Code",
            Self::Mcp => "MCP",
            Self::Mirror => "Mirror",
            Self::Api => "API",
            Self::Agent => "Agent",
            Self::DesktopUi => "Desktop UI",
        }
    }

    /// Returns the hyphenated store and CLI spelling (`claude-code`, `desktop-ui`).
    pub fn store_name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Opencode => "opencode",
            Self::ClaudeCode => "claude-code",
            Self::Mcp => "mcp",
            Self::Mirror => "mirror",
            Self::Api => "api",
            Self::Agent => "agent",
            Self::DesktopUi => "desktop-ui",
        }
    }
}

/// Returns the clean human label a surface gets when nobody named it.
///
/// The label is `"<name> · <node>"`: a desktop UI uses its holder name (for example
/// `StackSpirit`) when one is set, every other kind uses [`SurfaceKind::display_name`].
pub fn default_surface_label(kind: SurfaceKind, holder: &str, node: &NodeName) -> String {
    let name = match kind {
        SurfaceKind::DesktopUi if !holder.trim().is_empty() => holder.trim(),
        other => other.display_name(),
    };
    format!("{name} · {node}")
}

/// A registered surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Surface {
    /// Surface instance id.
    pub id: SurfaceId,
    /// Kind of client.
    pub kind: SurfaceKind,
    /// Node the surface runs on.
    pub node: NodeName,
    /// Human-visible holder name (`laptop`, `phone`, `mac opencode`).
    pub holder: String,
    /// Application name, when known.
    pub app: Option<String>,
    /// Working directory, when known.
    pub cwd: Option<String>,
    /// Free-form client metadata.
    pub metadata: serde_json::Value,
    /// When the surface first registered.
    pub created_at: Timestamp,
    /// Last heartbeat.
    pub last_seen_at: Timestamp,
}

/// The one live lease of a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Lease {
    /// Session the lease belongs to.
    pub session_id: SessionId,
    /// Surface instance id, when the surface row exists.
    pub surface_id: Option<SurfaceId>,
    /// Kind of client holding the lease.
    pub surface_kind: SurfaceKind,
    /// Node the holder runs on.
    pub node: NodeName,
    /// Human-visible holder name.
    pub holder: String,
    /// Secret token; never logged.
    pub token: String,
    /// When the lease was acquired.
    pub acquired_at: Timestamp,
    /// Last heartbeat.
    pub heartbeat_at: Timestamp,
}

/// How an owner holds a session: as a person's surface or as a worker job run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipMode {
    /// A person's surface; liveness is its heartbeat.
    #[default]
    Interactive,
    /// A worker-node job run; liveness is the run's `expires_at`.
    Job,
}

impl OwnershipMode {
    /// Returns the stable wire and store string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Job => "job",
        }
    }
}

impl fmt::Display for OwnershipMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Liveness of an owner, derived at read time from its mode and timestamps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipState {
    /// An interactive heartbeat younger than 60 seconds, or an unexpired job.
    #[default]
    Active,
    /// An interactive owner whose heartbeat is 60 seconds old or older.
    Stale,
    /// A job owner whose `expires_at` has passed.
    Expired,
}

impl OwnershipState {
    /// Returns the stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Expired => "expired",
        }
    }
}

impl fmt::Display for OwnershipState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the owner of a session is doing right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceActivity {
    /// No turn is running.
    #[default]
    Idle,
    /// A turn or run is in progress.
    Running,
    /// The session is blocked, waiting for an answer or a limit to clear.
    Waiting,
}

impl SurfaceActivity {
    /// Returns the stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Waiting => "waiting",
        }
    }
}

impl fmt::Display for SurfaceActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The read projection of a session's ownership record (its one `leases` row).
///
/// It never carries a lease token, a token hash, or an engine password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionOwner {
    /// Session the owner holds.
    pub session_id: SessionId,
    /// Surface instance id; `None` for a job run with no paired surface.
    pub surface_id: Option<SurfaceId>,
    /// Kind of client that owns the session.
    pub kind: SurfaceKind,
    /// Friendly saved surface name, for example `"StackSpirit · macbook"`.
    #[serde(default)]
    pub label: Option<String>,
    /// Human-visible holder name.
    pub holder: String,
    /// Node the owner runs on.
    pub node: NodeName,
    /// Interactive surface or job run.
    #[serde(default)]
    pub mode: OwnershipMode,
    /// Liveness at the time the projection was built.
    #[serde(default)]
    pub state: OwnershipState,
    /// Whether the session is idle, running or waiting.
    #[serde(default)]
    pub activity: SurfaceActivity,
    /// Monotonic counter the store bumps on every acquire or takeover.
    #[serde(default)]
    pub epoch: u64,
    /// When the owner acquired the session.
    pub acquired_at: Timestamp,
    /// Last heartbeat.
    pub heartbeat_at: Timestamp,
    /// Expiry of a job owner; `None` for an interactive owner.
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
}

/// A compact reference to the surface that holds a lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceRef {
    /// Surface instance id.
    pub surface_id: SurfaceId,
    /// Kind of client.
    pub kind: SurfaceKind,
    /// Human-visible holder name.
    pub holder: String,
    /// Node the holder runs on.
    pub node: NodeName,
    /// When the lease was acquired.
    pub acquired_at: Timestamp,
    /// Last heartbeat.
    pub heartbeat_at: Timestamp,
}

/// Output of `vowp session lease`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LeaseStatus {
    /// Whether a lease exists.
    pub held: bool,
    /// Holder name, when held.
    pub holder: Option<String>,
    /// Surface kind, when held.
    pub kind: Option<SurfaceKind>,
    /// Holder node, when held.
    pub node: Option<NodeName>,
    /// Acquisition time, when held.
    pub acquired_at: Option<Timestamp>,
    /// Last heartbeat, when held.
    pub heartbeat_at: Option<Timestamp>,
    /// The token, set only in the response to `--acquire`.
    pub token: Option<String>,
    /// Interactive surface or job run, when held.
    #[serde(default)]
    pub mode: Option<OwnershipMode>,
    /// Liveness of the owner, when held.
    #[serde(default)]
    pub state: Option<OwnershipState>,
    /// Owner epoch, when held.
    #[serde(default)]
    pub epoch: Option<u64>,
    /// Expiry of a job owner, when held in job mode.
    #[serde(default)]
    pub expires_at: Option<Timestamp>,
    /// Friendly saved surface name, when held and known.
    #[serde(default)]
    pub label: Option<String>,
    /// Whether the session is idle, running or waiting, when held.
    #[serde(default)]
    pub activity: Option<SurfaceActivity>,
}

/// Output of `vowp session takeover`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TakeoverReport {
    /// Previous holder, when there was one.
    pub previous: Option<String>,
    /// Whether the old turn drained.
    pub drained: bool,
    /// Capsule written for the handoff, when any.
    pub capsule_id: Option<CapsuleId>,
}

/// The serializable part of one remote event, without the home-assigned session id and seq.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteEvent {
    /// Event id assigned by the executor.
    pub id: EventId,
    /// Event timestamp.
    pub ts: Timestamp,
    /// Event type.
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Who produced the event.
    pub actor: Actor,
    /// Harness and model that produced the event.
    pub source: EventSource,
    /// Raw typed payload.
    pub payload: serde_json::Value,
    /// Original harness event, kept for display only.
    pub raw: Option<serde_json::Value>,
    /// Whether the event may be replayed to a harness.
    pub replayable: bool,
}

/// One JSON line exchanged between `__exec` and `session ingest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteLine {
    /// First line of a connection; carries the lease token and is never spooled.
    Hello {
        /// Session the run belongs to.
        session_id: SessionId,
        /// Run id.
        run_id: RunId,
        /// Executor string.
        executor: String,
        /// Executor node.
        node: NodeName,
        /// Spool identifier.
        spool_id: String,
        /// Lease token.
        lease: String,
    },
    /// One adapter event.
    Event {
        /// The event.
        event: RemoteEvent,
    },
    /// A periodic liveness line.
    Heartbeat {
        /// When the heartbeat was sent.
        at: Timestamp,
    },
    /// The final line of a run.
    Summary {
        /// Process exit code.
        exit_code: Option<i32>,
        /// Run outcome.
        outcome: RunOutcome,
        /// Harness-side session id.
        native_session_id: Option<String>,
        /// Cost of the run.
        cost: Cost,
        /// Provider limit, when one ended the run.
        limit: Option<LimitEvent>,
        /// Final assistant text, when any.
        result_text: Option<String>,
        /// Why the harness stopped.
        stop_reason: StopReason,
        /// Resource usage measured on the executor, when available.
        usage: Option<RemoteUsage>,
    },
}

/// One acknowledgement from `session ingest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IngestAck {
    /// The event was appended at `seq`.
    Ack {
        /// Event id.
        event_id: EventId,
        /// Assigned sequence number.
        seq: u64,
    },
    /// The event id was already appended at `seq`.
    Duplicate {
        /// Event id.
        event_id: EventId,
        /// Existing sequence number.
        seq: u64,
    },
    /// The run summary was accepted.
    Summary {
        /// Run id.
        run_id: RunId,
        /// Whether the home finished the run.
        done: bool,
    },
    /// The lease was revoked by a takeover; the client must stop sending.
    LeaseRevoked {
        /// Human-readable reason.
        reason: String,
        /// Who took over.
        by: String,
        /// When the takeover happened.
        at: Timestamp,
        /// Last sequence the home appended for this run.
        last_seq: u64,
    },
    /// The line was rejected.
    Error {
        /// Error class.
        kind: ErrorKind,
        /// Human-readable message.
        message: String,
    },
}

/// The document `vowp session turn-begin` returns to the surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnGrant {
    /// Run id assigned by the home.
    pub run_id: RunId,
    /// Session id.
    pub session_id: SessionId,
    /// Executor string.
    pub executor: String,
    /// Purpose of the run.
    pub purpose: crate::run::RunPurpose,
    /// Executor node.
    pub node: NodeName,
    /// Working directory on the executor.
    pub cwd: String,
    /// Prompt to send.
    pub prompt: String,
    /// Native session to resume, when any.
    pub resume_native: Option<String>,
    /// Hint for a fresh native session, when any.
    pub native_hint: Option<String>,
    /// Names of environment variables the executor should inject.
    pub env_names: Vec<String>,
    /// Lease token for the run.
    pub lease_token: String,
    /// Last sequence the surface should follow from.
    pub tail_seq: u64,
}

/// Which app an orchestrator runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorApp {
    /// The opencode desktop or server app.
    Opencode,
    /// Claude Code.
    ClaudeCode,
    /// The `vowp chat` fallback.
    Chat,
}

/// Join instructions for the three ways to reach an orchestrator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JoinInstructions {
    /// Command or steps for the desktop app.
    pub desktop: String,
    /// Command or steps for the phone.
    pub phone: String,
    /// Command for `vowp chat`.
    pub chat: String,
}

/// Lifecycle state of an orchestrator.
///
/// The first three states are the v0.2 home states. The v0.4 local-node states add
/// `node_offline` (the record exists but the node that runs the app is unreachable) and
/// `failed` (the node refused or lost the app). `starting` covers the window between the
/// home sending a start command and the node accepting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorNodeState {
    /// The home sent a start command and is waiting for the node's result.
    Starting,
    /// The app host is running.
    Running,
    /// The app host was stopped by the owner.
    Stopped,
    /// The node is reachable but the app is not healthy.
    Degraded,
    /// The node that runs the app is offline, degraded or revoked.
    NodeOffline,
    /// The node refused the command or the app could not start.
    Failed,
}

impl OrchestratorNodeState {
    /// Every state, in the order shown by the design note.
    pub const ALL: [Self; 6] = [
        Self::Starting,
        Self::Running,
        Self::Stopped,
        Self::Degraded,
        Self::NodeOffline,
        Self::Failed,
    ];

    /// Returns the stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Degraded => "degraded",
            Self::NodeOffline => "node_offline",
            Self::Failed => "failed",
        }
    }

    /// Returns whether the owner considers the orchestrator live.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Degraded)
    }
}

impl fmt::Display for OrchestratorNodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OrchestratorNodeState {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            "degraded" => Ok(Self::Degraded),
            "node_offline" => Ok(Self::NodeOffline),
            "failed" => Ok(Self::Failed),
            other => Err(ProtocolError::InvalidId {
                kind: "orchestrator_node_state",
                value: other.to_owned(),
            }),
        }
    }
}

/// One orchestrator as shown by `vowp orchestrator ls` and the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorCard {
    /// Orchestrator name.
    pub name: String,
    /// Session the orchestrator uses.
    pub session_id: SessionId,
    /// Project the orchestrator is bound to, when any.
    pub project: Option<ProjectName>,
    /// App the orchestrator runs in.
    pub app: OrchestratorApp,
    /// Lifecycle state.
    pub state: OrchestratorNodeState,
    /// Node that runs the app; `None` for a v0.2 orchestrator hosted on the home.
    #[serde(default)]
    pub node: Option<NodeName>,
    /// Last redacted failure or state detail, when the record is failed, degraded or
    /// offline. `None` for a healthy or cleanly stopped orchestrator; never a path, port,
    /// password, pairing link or secret value.
    #[serde(default)]
    pub last_error: Option<String>,
    /// How to join from the three surfaces.
    pub join: JoinInstructions,
}

/// Where an orchestrator's app host runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorLocation {
    /// The app host runs on this home.
    Home,
    /// The app host runs on an enrolled agent node.
    Node,
}

/// The local auth material a UI client needs to call an app's loopback endpoint.
///
/// The password is a secret. Only the explicit local endpoint command returns it; the
/// custom [`fmt::Debug`] redacts it, and no card, snapshot, log or error carries it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorAuth {
    /// HTTP header carrying the credential, for example `Authorization`.
    pub header: String,
    /// Authentication scheme, for example `Basic`.
    pub scheme: String,
    /// Authentication username, for example `opencode`.
    pub username: String,
    /// Secret password; only the explicit local endpoint document carries it.
    pub password: String,
}

impl fmt::Debug for OrchestratorAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestratorAuth")
            .field("header", &self.header)
            .field("scheme", &self.scheme)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Discovery document for a UI client connecting to one orchestrator.
///
/// The base fields always come from the orchestrator record. `url`, `native_session_id` and
/// `auth` are filled only when the app host runs locally and its private loopback state is
/// readable; a node-hosted or stopped orchestrator carries `null` for all three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorEndpoint {
    /// Orchestrator name.
    pub name: String,
    /// App the orchestrator runs.
    pub app: OrchestratorApp,
    /// Home session id the orchestrator mirrors.
    pub session_id: SessionId,
    /// Where the app host runs.
    pub location: OrchestratorLocation,
    /// Node that runs the app, when known.
    #[serde(default)]
    pub node: Option<NodeName>,
    /// Lifecycle state at the time of the request.
    pub state: OrchestratorNodeState,
    /// Loopback URL (`http://127.0.0.1:<port>`), only for a live local endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// App-side session id, when the local app host knows one.
    #[serde(default)]
    pub native_session_id: Option<String>,
    /// Local auth material, only for a live local endpoint.
    #[serde(default)]
    pub auth: Option<OrchestratorAuth>,
    /// Who owns the orchestrator's session at the time of the request, when anyone does.
    ///
    /// Read from the home, so it is reported even for a node-hosted record. Old readers
    /// ignore it; a document without it reads as `None`.
    #[serde(default)]
    pub owner: Option<SessionOwner>,
    /// Whether the caller may write: [`EndpointAccess::Owner`] only for the live owner surface.
    #[serde(default)]
    pub access: EndpointAccess,
}

/// What the caller of an orchestrator endpoint may do with the session.
///
/// Recorded ownership is cooperative: a UI that talks to the app's HTTP API directly is not
/// stopped by it. This value tells a well-behaved client whether to write or stay read-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAccess {
    /// The caller is the live owner surface and may write.
    Owner,
    /// Any other caller, including one that named no surface.
    #[default]
    ReadOnly,
}

impl EndpointAccess {
    /// Returns the stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::ReadOnly => "read_only",
        }
    }
}

impl fmt::Display for EndpointAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The grant `vowp orchestrator attach` returns after it creates a desktop-UI surface.
///
/// `token` is the surface's lease token, shown once. The custom [`fmt::Debug`] redacts it.
/// An attach grant never goes into a card, snapshot, log or error. `expires_at` is when the
/// lease goes stale if the attach process stops heartbeating (`heartbeat_at` plus 60 s).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorAttach {
    /// The desktop-UI surface the attach created.
    pub surface_id: SurfaceId,
    /// One-time lease token; a secret.
    pub token: String,
    /// The owner the attach produced.
    pub owner: SessionOwner,
    /// The endpoint document, with `access` set to the owner.
    pub endpoint: OrchestratorEndpoint,
    /// When the lease is stale if the attach process stops.
    pub expires_at: Timestamp,
}

impl fmt::Debug for OrchestratorAttach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestratorAttach")
            .field("surface_id", &self.surface_id)
            .field("token", &"[REDACTED]")
            .field("owner", &self.owner)
            .field("endpoint", &self.endpoint)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// One home -> agent orchestrator command.
///
/// The home correlates the agent's [`OrchestratorResult`] by `request_id`; the agent
/// materializes the local copy and starts, stops or resumes the app. The payload never
/// carries a port, a password, a pairing link, an absolute local path or a secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum OrchestratorCommand {
    /// Create the local record and start the app host.
    Start {
        /// Home-assigned correlation id.
        request_id: String,
        /// Orchestrator name.
        name: String,
        /// App to run.
        app: OrchestratorApp,
        /// Project to bind, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectName>,
    },
    /// Stop the app host and release its surface.
    Stop {
        /// Home-assigned correlation id.
        request_id: String,
        /// Orchestrator name.
        name: String,
    },
    /// Start the app host again for an existing record.
    Resume {
        /// Home-assigned correlation id.
        request_id: String,
        /// Orchestrator name.
        name: String,
    },
}

impl OrchestratorCommand {
    /// Returns the correlation id.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Start { request_id, .. } | Self::Stop { request_id, .. } | Self::Resume { request_id, .. } => {
                request_id
            }
        }
    }

    /// Returns the orchestrator name.
    pub fn name(&self) -> &str {
        match self {
            Self::Start { name, .. } | Self::Stop { name, .. } | Self::Resume { name, .. } => name,
        }
    }

    /// Returns the stable action string (`start`, `stop` or `resume`).
    pub fn action(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Stop { .. } => "stop",
            Self::Resume { .. } => "resume",
        }
    }
}

/// A typed orchestrator failure.
///
/// `message` and `detail` are redacted by the sender: neither carries a local path, a port,
/// a password, a pairing link or a secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorError {
    /// Machine-readable code, for example `node_offline`, `refused` or `spawn_failed`.
    pub code: String,
    /// Human-readable message; never a secret value or an absolute local path.
    pub message: String,
    /// Whether retrying the same command may succeed.
    pub retryable: bool,
}

impl OrchestratorError {
    /// Builds one typed error.
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

/// One agent -> home orchestrator command result.
///
/// Exactly one of `error` and a successful `accepted` result describes the outcome; `state`
/// is the node's resulting record state. The agent never sends a port, a password, an
/// absolute local path or a secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorResult {
    /// Request id from the matching [`OrchestratorCommand`].
    pub request_id: String,
    /// Whether the node accepted and applied the command.
    pub accepted: bool,
    /// Resulting lifecycle state of the orchestrator on the node.
    pub state: OrchestratorNodeState,
    /// Typed failure, when the command was refused or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OrchestratorError>,
    /// Redacted explanation for the owner; never a path or a secret value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl OrchestratorResult {
    /// Builds an accepted result.
    pub fn accepted(request_id: impl Into<String>, state: OrchestratorNodeState) -> Self {
        Self {
            request_id: request_id.into(),
            accepted: true,
            state,
            error: None,
            detail: None,
        }
    }

    /// Builds a failed result with a typed error.
    pub fn failed(request_id: impl Into<String>, state: OrchestratorNodeState, error: OrchestratorError) -> Self {
        Self {
            request_id: request_id.into(),
            accepted: false,
            state,
            error: Some(error),
            detail: None,
        }
    }
}
