//! Surfaces, leases, remote execution lines and orchestrator cards.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.2–§7.6 and §13.1. The wire shapes are
//! fixed here; leases, remote ingest and app hosts build on them.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capacity::RemoteUsage;
use crate::cost::Cost;
use crate::error::ErrorKind;
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
    /// `running`, `stopped` or `degraded`.
    pub state: String,
    /// How to join from the three surfaces.
    pub join: JoinInstructions,
}
