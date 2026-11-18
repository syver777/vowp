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
