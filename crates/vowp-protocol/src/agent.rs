//! v0.4 local-node wire types: path refs, capabilities, capacity and Phase 0 frames.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.1, §6.1, §8.3, §8.5 and §11.3.
//! This module is pure data: no I/O, crypto, store or network. Absolute local paths stay on
//! the node; [`PathRef`] is the only path shape that crosses the machine boundary.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capacity::RemoteUsage;
use crate::cost::Cost;
use crate::error::{ErrorKind, ProtocolError};
use crate::event::{LimitEvent, StopReason};
use crate::ids::{EventId, JobId, NodeName, ProjectName, RunId, SessionId};
use crate::offload::{
    AssignmentPayload, RemoteRunState, RunRejectReason, RunResume, RunResumeResult, SurfaceResume, SurfaceResumeResult,
};
use crate::run::{RunOutcome, RunPurpose};
use crate::surface::{
    OrchestratorCommand, OrchestratorResult, RemoteLine as SurfaceRemoteLine, SurfaceId, SurfaceKind,
};
use crate::time::Timestamp;
use crate::vault::SecretName;

/// Protocol name exchanged in the agent handshake.
pub const AGENT_PROTOCOL: &str = "vowp-agent/1";

/// Longest accepted `PathRef` wire string, in bytes.
pub const MAX_PATH_REF_LEN: usize = 1024;

/// Largest raw payload of one [`AgentFrame::BundleChunk`], in bytes.
///
/// The data travels as lowercase hex, so one chunk's wire string is at most twice this
/// length and stays well inside the 1 MiB frame cap.
pub const MAX_BUNDLE_CHUNK_BYTES: usize = 256 * 1024;

/// Largest accepted result bundle, in bytes (256 MiB).
///
/// The cap is a wire contract: a `BundleStart` over it is refused before the home stages
/// any bytes.
pub const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;

/// Returns whether a chunk payload is lowercase hex of at most [`MAX_BUNDLE_CHUNK_BYTES`].
///
/// Uppercase, an odd length and any non-hex byte are invalid: the wire contract is one
/// spelling so a receiver can compare and hash without a decoder option.
pub fn is_bundle_chunk_data(data: &str) -> bool {
    data.len() % 2 == 0
        && data.len() <= MAX_BUNDLE_CHUNK_BYTES * 2
        && data
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A project-relative path the agent resolves locally.
///
/// The wire form is one short string: `project:vowp`, `worktree:vowp/<job>` or
/// `subdir:vowp/src`. Parsing rejects `..`, absolute paths, backslashes, NUL, an empty
/// project and an empty subdirectory, and normalises repeated `/` and `.` components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub enum PathRef {
    /// The project root.
    Project {
        /// Project name.
        project: ProjectName,
    },
    /// The job worktree under the project.
    Worktree {
        /// Project name.
        project: ProjectName,
        /// Job whose worktree is referenced.
        job: JobId,
    },
    /// A directory below the project root.
    Subdir {
        /// Project name.
        project: ProjectName,
        /// Normalised project-relative path.
        rel: String,
    },
}

impl PathRef {
    /// Parses and validates a path ref wire string.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        let invalid = || ProtocolError::InvalidId {
            kind: "path_ref",
            value: s.to_owned(),
        };
        if s.len() > MAX_PATH_REF_LEN || s.contains('\0') || s.contains('\\') {
            return Err(invalid());
        }
        let (kind, rest) = s.split_once(':').ok_or_else(invalid)?;
        match kind {
            "project" => {
                if rest.is_empty() || rest.contains('/') {
                    return Err(invalid());
                }
                Ok(Self::Project {
                    project: ProjectName::parse(rest)?,
                })
            }
            "worktree" => {
                let (project, job) = rest.split_once('/').ok_or_else(invalid)?;
                if project.is_empty() || job.is_empty() || job.contains('/') {
                    return Err(invalid());
                }
                Ok(Self::Worktree {
                    project: ProjectName::parse(project)?,
                    job: JobId::parse(job)?,
                })
            }
            "subdir" => {
                let (project, rel) = rest.split_once('/').ok_or_else(invalid)?;
                if project.is_empty() {
                    return Err(invalid());
                }
                let rel = normalise_rel(rel).ok_or_else(invalid)?;
                Ok(Self::Subdir {
                    project: ProjectName::parse(project)?,
                    rel,
                })
            }
            _ => Err(invalid()),
        }
    }
}

/// Normalises a relative path, rejecting traversal, absolute input and empty results.
fn normalise_rel(rel: &str) -> Option<String> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            other => parts.push(other),
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

impl fmt::Display for PathRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project { project } => write!(f, "project:{project}"),
            Self::Worktree { project, job } => write!(f, "worktree:{project}/{job}"),
            Self::Subdir { project, rel } => write!(f, "subdir:{project}/{rel}"),
        }
    }
}

impl From<PathRef> for String {
    fn from(value: PathRef) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for PathRef {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl FromStr for PathRef {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// What a local agent is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Hosts local surfaces only.
    Surface,
    /// Runs offloaded jobs only.
    Executor,
    /// Hosts surfaces and runs offloaded jobs.
    Both,
}

/// Names-only capabilities advertised by an agent; values never cross the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCapabilities {
    /// Operating system name (`darwin`, `linux`).
    pub os: String,
    /// CPU architecture (`arm64`, `x86_64`).
    pub arch: String,
    /// Agent binary version.
    pub agent_version: String,
    /// Harness name to version, for example `opencode` -> `1.2.3`.
    pub harnesses: BTreeMap<String, String>,
    /// `git --version` output, when known.
    pub git_version: Option<String>,
    /// Owner-assigned labels.
    pub labels: Vec<String>,
    /// Names of secrets the node can resolve; never values.
    pub secret_names: Vec<SecretName>,
    /// Projects mapped on the node.
    pub projects: Vec<ProjectName>,
}

/// Capacity reported in a heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCapacity {
    /// Logical CPU cores.
    pub cpu_cores: u32,
    /// One-minute load average.
    pub load1: f64,
    /// Total physical memory in bytes.
    pub mem_total_bytes: u64,
    /// Memory available to new work in bytes.
    pub mem_free_bytes: u64,
    /// Free bytes on the worktree filesystem.
    pub disk_free_bytes: u64,
    /// Worker and verifier slots in use.
    pub busy_slots: u32,
    /// Maximum worker and verifier slots.
    pub max_slots: u32,
    /// Whether the node is on battery, when known.
    pub on_battery: Option<bool>,
    /// Thermal state name (`nominal`, `fair`, `serious`, `critical`), when known.
    pub thermal_state: Option<String>,
}

/// Enrollment state of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Waiting for approval.
    Pending,
    /// Enrolled and usable.
    Active,
    /// Credentials revoked.
    Revoked,
}

/// How a node is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// The home itself.
    Home,
    /// A pre-v0.4 SSH node.
    Ssh,
    /// An enrolled `vowp agent` node.
    Agent,
}

/// Computed presence of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodePresence {
    /// A heartbeat arrived under 30 seconds ago.
    Online,
    /// The last heartbeat is 30 to 90 seconds old.
    Degraded,
    /// The last heartbeat is over 90 seconds old.
    Offline,
    /// The node is revoked regardless of its last heartbeat.
    Revoked,
}

/// One project path map entry supplied by an agent at enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentPathMap {
    /// Project the map belongs to.
    pub project: ProjectName,
    /// Absolute project root on the node; stays local to the home's `node_paths` row.
    pub local_root: String,
    /// Absolute worktree root on the node, when separate.
    pub worktree_root: Option<String>,
    /// Allowed relative paths below the root.
    pub allow: Vec<String>,
}

/// Redacted node summary shown by `vowp node show` and never carrying a path or secret name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeCard {
    /// Node name.
    pub name: NodeName,
    /// How the node is reached.
    pub kind: NodeKind,
    /// Enrollment state.
    pub status: NodeStatus,
    /// Computed presence.
    pub presence: NodePresence,
    /// Owner-assigned labels.
    pub labels: Vec<String>,
    /// Latest capacity snapshot, when one arrived.
    pub capacity: Option<AgentCapacity>,
    /// Number of active runs on the node.
    pub runs: u32,
    /// Projects mapped on the node.
    pub mapped: Vec<ProjectName>,
    /// Agent binary version, when known.
    pub agent_version: Option<String>,
    /// Agent protocol, when known.
    pub protocol: Option<String>,
    /// Last connection or policy error, redacted by the caller.
    pub last_error: Option<String>,
}

/// Lifecycle state of a surface row.
///
/// The global schema stores this as `surfaces.state`. `pending` is a surface created for a
/// pairing code, `attached` is a live surface (and the default for pre-v0.4 rows), and
/// `revoked` is a surface whose tokens no longer work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceState {
    /// Created for a pairing code and not attached yet.
    Pending,
    /// Attached and usable.
    Attached,
    /// Revoked; no token of the surface is accepted.
    Revoked,
}

/// One capability granted to a surface token.
///
/// The wire form is one short string, for example `session.read:<id>`, `session.write:<id>`,
/// `project.read:<name>`, `fleet.read` or `tool:<name>`. The store keeps the list in
/// `surface_tokens.scopes_json`; unknown scopes are preserved verbatim so a newer home can
/// extend the vocabulary without breaking older readers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
#[schemars(with = "String")]
pub struct SurfaceScope(pub String);

impl SurfaceScope {
    /// Builds a scope from its wire string.
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    /// Returns the scope wire string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SurfaceScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A registered surface with its lifecycle state.
///
/// This is the redacted read model used by the surface repository: it carries no token or
/// token hash. The v0.2 [`crate::surface::Surface`] keeps the original field set, so rows
/// created before v0.4 remain source-compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceInfo {
    /// Surface instance id.
    pub id: SurfaceId,
    /// Kind of client.
    pub kind: SurfaceKind,
    /// Node the surface runs on.
    pub node: NodeName,
    /// Human-visible holder name.
    pub holder: String,
    /// Application name, when known.
    pub app: Option<String>,
    /// Working directory, when known.
    pub cwd: Option<String>,
    /// Free-form client metadata.
    pub metadata: serde_json::Value,
    /// Lifecycle state.
    pub state: SurfaceState,
    /// When the surface first registered.
    pub created_at: Timestamp,
    /// Last heartbeat.
    pub last_seen_at: Timestamp,
    /// When the surface was revoked; `None` while it is not revoked.
    pub revoked_at: Option<Timestamp>,
}

/// How a surface asks for a session lease.
///
/// `Acquire` fails with a `Conflict` error when another surface holds a live lease;
/// `Takeover` replaces it and fences the old holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    /// Take the lease only when it is free or stale.
    Acquire,
    /// Replace a live lease after the owner agreed.
    Takeover,
}

/// One agent protocol frame, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentFrame {
    /// Agent -> home: identity and capabilities.
    AgentHello {
        /// Node name.
        node: NodeName,
        /// Public-key id.
        key_id: String,
        /// Agent binary version.
        agent_version: String,
        /// Operating system name.
        os: String,
        /// CPU architecture.
        arch: String,
        /// Owner-assigned labels.
        labels: Vec<String>,
        /// Names-only capabilities.
        capabilities: AgentCapabilities,
    },
    /// Home -> agent: the nonce and limits for authentication.
    HomeChallenge {
        /// One-time nonce.
        nonce: String,
        /// Home identity.
        home_id: String,
        /// Connection identity.
        connection_id: String,
        /// Home server time.
        server_time: Timestamp,
        /// Heartbeat interval in seconds.
        heartbeat_secs: u64,
        /// Maximum accepted frame size in bytes.
        max_frame_bytes: u64,
    },
    /// Agent -> home: signature over the challenge.
    AgentAuth {
        /// Lowercase hex signature.
        signature: String,
    },
    /// Home -> agent: the handshake succeeded.
    HomeWelcome {
        /// Negotiated protocol.
        protocol: String,
        /// Home identity.
        home_id: String,
        /// Node name.
        node: NodeName,
        /// Connection identity.
        connection_id: String,
    },
    /// Agent -> home: redeem a one-time enrollment code.
    Enroll {
        /// One-time enrollment code; never logged.
        code: String,
        /// Lowercase hex ed25519 public key.
        public_key: String,
        /// Agent binary version.
        agent_version: String,
        /// Operating system name.
        os: String,
        /// CPU architecture.
        arch: String,
        /// Owner-assigned labels.
        labels: Vec<String>,
        /// Names-only capabilities.
        capabilities: AgentCapabilities,
        /// Initial project path map.
        paths: Vec<AgentPathMap>,
    },
    /// Home -> agent: enrollment succeeded.
    Enrolled {
        /// Home identity.
        home_id: String,
        /// Node name.
        node: NodeName,
        /// Negotiated protocol.
        protocol: String,
        /// Home server time.
        server_time: Timestamp,
        /// Heartbeat interval in seconds.
        heartbeat_secs: u64,
    },
    /// Agent -> home: redeem a one-time pairing code for a local surface.
    SurfacePair {
        /// One-time pairing code; never logged or echoed.
        code: String,
        /// Surface the owner created on the home.
        surface_id: SurfaceId,
        /// Session the surface will mirror.
        session: SessionId,
        /// Application name, for example `opencode`.
        app: String,
    },
    /// Home -> agent: the pairing succeeded and issued a token.
    SurfaceIssued {
        /// Surface the token belongs to.
        surface_id: SurfaceId,
        /// Secret token; write it 0600, never log or echo it.
        token: String,
        /// Scopes granted to the token.
        scopes: Vec<SurfaceScope>,
        /// When the token expires.
        expires_at: Timestamp,
    },
    /// Home -> agent: the surface token was revoked; detach and delete it.
    SurfaceRevoked {
        /// Surface whose token was revoked.
        surface_id: SurfaceId,
        /// Human-readable reason.
        reason: String,
    },
    /// Agent -> home: attach a surface to a session.
    SurfaceAttach(surface::SurfaceAttach),
    /// Agent -> home: detach a surface after flushing its pending events.
    SurfaceDetach(surface::SurfaceDetach),
    /// Agent -> home: one normalized event carrying the app's native id.
    ///
    /// The home assigns the canonical `seq` and deduplicates by
    /// `(session, harness, native_id)`; the agent never sends a sequence number.
    SurfaceEvent(surface::SurfaceEvent),
    /// Agent -> home: the app started or ended a turn.
    SurfaceTurn(surface::SurfaceTurn),
    /// Agent -> home: an MCP tool call from a local client.
    ToolCall(surface::ToolCall),
    /// Home -> agent: the result of one [`Self::ToolCall`].
    ToolResult(surface::ToolResult),
    /// Agent -> home: acquire or take over the session lease.
    LeaseAcquire {
        /// Surface asking for the lease.
        surface: SurfaceId,
        /// Session the lease protects.
        session: SessionId,
        /// Scoped surface token issued by `SurfaceIssued`; never logged.
        surface_token: String,
        /// Whether a live lease may be replaced.
        mode: LeaseMode,
    },
    /// Agent -> home: keep the held lease alive.
    LeaseHeartbeat {
        /// Surface holding the lease.
        surface: SurfaceId,
        /// Session the lease protects.
        session: SessionId,
        /// Plaintext lease token returned by `LeaseGrant`; never logged.
        token: String,
    },
    /// Agent -> home: release the lease on detach.
    LeaseRelease {
        /// Surface releasing the lease.
        surface: SurfaceId,
        /// Session the lease protects.
        session: SessionId,
        /// Plaintext lease token returned by `LeaseGrant`; never logged.
        token: String,
    },
    /// Home -> agent: the lease is granted for this surface.
    LeaseGrant {
        /// Plaintext lease token; shown once in the grant, never logged.
        lease_token: String,
        /// When the lease goes stale without a heartbeat.
        expires_at: Timestamp,
        /// Session sequence the new holder may continue from.
        tail_seq: u64,
    },
    /// Home -> agent: the lease was taken over or expired; the surface is read-only.
    LeaseLost {
        /// Human-readable reason.
        reason: String,
    },
    /// Home -> agent: another surface now holds the lease; stop sending events.
    LeaseRevoked {
        /// Human-readable reason.
        reason: String,
        /// Human-visible holder of the lease now.
        holder: String,
    },
    /// Agent -> home: capacity and spool state.
    Heartbeat {
        /// When the heartbeat was sent.
        at: Timestamp,
        /// Latest capacity snapshot.
        capacity: AgentCapacity,
        /// Bytes waiting in the pending spool.
        pending_spool_bytes: u64,
    },
    /// Home -> agent: liveness probe.
    Ping,
    /// Agent -> home: liveness reply.
    Pong,
    /// Home -> agent: the node credential was revoked.
    Revoked {
        /// Human-readable reason.
        reason: String,
    },
    /// Home -> agent: another connection took over.
    Superseded {
        /// Connection that superseded this one.
        connection_id: String,
    },
    /// Home -> agent: protocol or policy error.
    Error {
        /// Error class.
        kind: ErrorKind,
        /// Human-readable message.
        message: String,
        /// Whether a retry may succeed.
        retryable: bool,
    },
    /// Agent -> home: start one local chat turn after the lease was granted.
    TurnBegin {
        /// Paired surface that owns the turn.
        surface: SurfaceId,
        /// Session the turn continues.
        session: SessionId,
        /// Plaintext lease token returned by `LeaseGrant`; never logged.
        lease_token: String,
        /// Prompt for this turn.
        prompt: String,
        /// Executor string, for example `opencode:deepseek/deepseek-flash#max`.
        executor: String,
        /// Project-relative working directory; the agent resolves it locally.
        cwd_ref: PathRef,
        /// Job the run belongs to, when the session is job-scoped.
        job: Option<JobId>,
        /// Why the run starts.
        purpose: RunPurpose,
    },
    /// Home -> agent: the turn started and the run id is assigned.
    TurnGrant {
        /// Run id assigned by the home.
        run_id: RunId,
        /// Session the run belongs to.
        session: SessionId,
        /// Lease token for the run; the same value the agent presented.
        lease_token: String,
        /// When the lease goes stale without a heartbeat.
        expires_at: Timestamp,
        /// Last session sequence the agent may continue from.
        tail_seq: u64,
    },
    /// Agent -> home: one v0.2 remote line wrapped with its run and epoch.
    RemoteLine {
        /// Run the line belongs to.
        run_id: RunId,
        /// Run epoch; a later frame with another epoch is fenced.
        epoch: u64,
        /// The v0.2 event, heartbeat or summary line.
        line: SurfaceRemoteLine,
    },
    /// Home -> agent: the result of one remote line.
    ///
    /// Exactly one of the optional fields is set for each acknowledgement kind:
    /// `event_id`/`seq` for an append or duplicate, `seq` for a summary,
    /// `lease_revoked` for a takeover and `error` for a rejection.
    IngestAck {
        /// Run the acknowledgement belongs to.
        run_id: RunId,
        /// Event id that was appended or already present.
        event_id: Option<EventId>,
        /// Assigned or existing sequence number.
        seq: Option<u64>,
        /// Whether the event was already present.
        duplicate: bool,
        /// Human-readable takeover reason; the stream stops.
        lease_revoked: Option<String>,
        /// Human-readable rejection; the stream stops.
        error: Option<String>,
    },
    /// Home -> agent: replenish the in-flight event window.
    Credit {
        /// Run the credit belongs to.
        run_id: RunId,
        /// Number of additional in-flight events allowed.
        count: u64,
    },
    /// Agent -> home: the run finished with one classified summary.
    TurnEnd {
        /// Run that finished.
        run_id: RunId,
        /// Classified terminal summary.
        summary: TurnSummary,
    },
    /// Home -> agent: the run was fenced; stop the harness and keep the spool.
    RunLost {
        /// Run that was fenced.
        run_id: RunId,
        /// Human-readable reason.
        reason: String,
    },
    /// Agent -> home: announce one `base..head` git bundle before its chunks.
    ///
    /// The bundle holds exactly `refs/heads/job/<id>` (`branch`) with `base` as its
    /// prerequisite. `bytes` is the transfer size and `sha256` its lowercase hex digest;
    /// both are re-checked at [`Self::BundleEnd`].
    BundleStart {
        /// Run the bundle belongs to.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Assigned base commit; the bundle's prerequisite.
        base: String,
        /// Committed head the bundle returns.
        head: String,
        /// The bundle's single branch, `job/<id>`.
        branch: String,
        /// Bundle size in bytes.
        bytes: u64,
        /// Lowercase hex SHA-256 of the bundle file.
        sha256: String,
    },
    /// Home -> agent: the transfer may continue from `offset`.
    ///
    /// A start with the same run, epoch, digest and size resumes at the stored length; a
    /// fresh transfer is `offset = 0`.
    BundleAccept {
        /// Run the bundle belongs to.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Bytes already stored by the home.
        offset: u64,
    },
    /// Agent -> home: one chunk of the bundle at `offset`.
    ///
    /// `data` is lowercase hex and at most [`MAX_BUNDLE_CHUNK_BYTES`] raw bytes. A chunk's
    /// `offset` must equal the bytes already stored; a gap, overlap or oversized chunk
    /// aborts the bundle.
    BundleChunk {
        /// Run the bundle belongs to.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Byte offset this chunk starts at.
        offset: u64,
        /// Lowercase hex chunk data.
        data: String,
    },
    /// Agent -> home: the last chunk was sent; verify size and digest.
    BundleEnd {
        /// Run the bundle belongs to.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Bundle size in bytes.
        bytes: u64,
        /// Lowercase hex SHA-256 of the bundle file.
        sha256: String,
    },
    /// Either direction: the transfer is refused or cannot continue.
    BundleAbort {
        /// Run the bundle belongs to.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Stable, path-free reason.
        reason: String,
    },
    /// Home -> agent: offload one run assignment.
    ///
    /// `lease_token` is a frame-only secret. The home issues it when it sends this
    /// frame, stores only its hash in the lease row and re-issues a token on resume. It
    /// is never part of [`AssignmentPayload`] and must never be logged or stored.
    AssignRun {
        /// Durable, token-free assignment payload.
        assignment: AssignmentPayload,
        /// Short-lived run lease token; frame-only, never stored in the payload.
        lease_token: String,
        /// When the run lease goes stale without a heartbeat.
        lease_expires_at: Timestamp,
    },
    /// Agent -> home: the assignment passed validation.
    RunAccepted {
        /// Assignment id from the payload.
        assignment_id: String,
        /// Run id from the assignment.
        run_id: RunId,
        /// Assignment epoch.
        epoch: u64,
    },
    /// Agent -> home: the assignment was refused before it started.
    RunRejected {
        /// Assignment id from the payload.
        assignment_id: String,
        /// Run id from the assignment.
        run_id: RunId,
        /// Assignment epoch.
        epoch: u64,
        /// Machine-readable refusal class.
        reason: RunRejectReason,
        /// Human-readable detail; never a secret value.
        detail: String,
    },
    /// Agent -> home: the current state of one remote run.
    RunState {
        /// Run being reported.
        run_id: RunId,
        /// Run epoch.
        epoch: u64,
        /// Current remote run state.
        state: RemoteRunState,
        /// Bytes waiting in the run's spool.
        spool_bytes: u64,
        /// Head commit the node pushed, when known.
        head_commit: Option<String>,
        /// Human-readable detail; never a secret value.
        detail: String,
    },
    /// Home -> agent: cancel a run.
    CancelRun {
        /// Run to cancel.
        run_id: RunId,
        /// Run epoch; a stale epoch is ignored.
        epoch: u64,
        /// Human-readable reason.
        reason: String,
    },
    /// Agent -> home: the runs and surfaces this node still holds after a reconnect.
    Resume {
        /// Runs with their last acknowledged sequence.
        runs: Vec<RunResume>,
        /// Surfaces with their last acknowledged sequence.
        surfaces: Vec<SurfaceResume>,
    },
    /// Home -> agent: per-run and per-surface resume results.
    ResumeOk {
        /// Run resume results.
        runs: Vec<RunResumeResult>,
        /// Surface resume results.
        surfaces: Vec<SurfaceResumeResult>,
    },
    /// Agent -> home: a project's local path map changed.
    PathMapUpdate {
        /// Updated path map for one project.
        map: AgentPathMap,
    },
    /// Home -> agent: remove a finished run's worktree.
    Cleanup {
        /// Run whose worktree is removed.
        run_id: RunId,
        /// Job whose worktree is removed.
        job: JobId,
        /// Worktree to remove; the agent resolves it locally.
        worktree: PathRef,
    },
    /// Home -> agent: create, stop or resume one local orchestrator.
    ///
    /// The frame carries no port, password, pairing link, absolute local path or secret
    /// value. The agent materializes the local record and answers with
    /// [`Self::OrchestratorResult`], correlated by `request_id`.
    OrchestratorCommand(OrchestratorCommand),
    /// Agent -> home: the result of one [`Self::OrchestratorCommand`].
    ///
    /// The result carries the resulting state and a typed error or a redacted detail; it
    /// never carries a port, password, absolute local path or secret value.
    OrchestratorResult(OrchestratorResult),
}

/// The terminal summary an agent sends in [`AgentFrame::TurnEnd`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TurnSummary {
    /// Process exit code, when the harness produced one.
    pub exit_code: Option<i32>,
    /// Classified run outcome.
    pub outcome: RunOutcome,
    /// Harness-side session id, when one was reported.
    pub native_session_id: Option<String>,
    /// Cost and token usage reported by the harness.
    pub cost: Cost,
    /// Provider limit that ended the run, when one was classified.
    pub limit: Option<LimitEvent>,
    /// Final assistant text, when any.
    pub result_text: Option<String>,
    /// Why the run stopped.
    pub stop_reason: StopReason,
    /// Resource usage measured on the agent, when available.
    pub usage: Option<RemoteUsage>,
}

/// The Phase 3 local app-surface frame payloads carried inside [`AgentFrame`].
///
/// Spec: `docs/design/v0.4/local-orchestrator.md` §5.3, §5.4 and §8.3. The home owns the
/// canonical session sequence: an attached surface sends only its native ids, and the home
/// assigns `seq` and skips a duplicate native id. A native id is stable across retries and
/// backfill.
///
/// The types live in a nested module because [`crate::event`] already defines the
/// `tool.call`/`tool.result` payload structs named `ToolCall` and `ToolResult`; re-exporting
/// both at the crate root would make `vowp_protocol::ToolCall` ambiguous.
pub mod surface {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use crate::event::{EventBody, EventType};
    use crate::ids::SessionId;
    use crate::surface::SurfaceId;
    use crate::time::Timestamp;

    /// Lifecycle state of an attached turn.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum SurfaceTurnState {
        /// The app started a turn.
        Started,
        /// The app finished a turn.
        Ended,
    }

    /// Agent -> home: attach a surface to a session.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct SurfaceAttach {
        /// Surface being attached.
        pub surface: SurfaceId,
        /// Session the surface will mirror.
        pub session: SessionId,
    }

    /// Agent -> home: detach a surface after its spool is flushed.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct SurfaceDetach {
        /// Surface being detached.
        pub surface: SurfaceId,
        /// Session the surface was mirroring.
        pub session: SessionId,
    }

    /// One normalized event mirrored from a local app.
    ///
    /// The wire form of `event` is the typed [`EventBody`] as an object with a dotted `type`
    /// plus its payload fields, for example `{"type":"assistant.message","text":"…"}`.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct SurfaceEvent {
        /// Surface that observed the event.
        pub surface: SurfaceId,
        /// Session the event belongs to.
        pub session: SessionId,
        /// App-side message or part id; stable across retries and backfill.
        pub native_id: String,
        /// Normalized event body; the home assigns its canonical sequence.
        #[serde(with = "event_body_wire")]
        #[schemars(with = "EventBodySchema")]
        pub event: EventBody,
    }

    /// One turn boundary of an attached surface.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct SurfaceTurn {
        /// Surface whose turn started or ended.
        pub surface: SurfaceId,
        /// Session the turn belongs to.
        pub session: SessionId,
        /// App-side turn id; stable across retries and backfill.
        pub native_id: String,
        /// Whether the turn started or ended.
        pub state: SurfaceTurnState,
        /// When the app observed the boundary.
        pub at: Timestamp,
    }

    /// Agent -> home: an MCP tool call proxied from a local client.
    ///
    /// `arguments` is the normal MCP JSON object. By contract it never carries a local path
    /// or a secret value; the home checks the surface token scope and the live lease before
    /// executing the same core path as its own MCP server.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct ToolCall {
        /// Client-chosen request id, echoed by the matching [`ToolResult`].
        pub request_id: String,
        /// Surface that made the call.
        pub surface: SurfaceId,
        /// Session the call is scoped to.
        pub session: SessionId,
        /// MCP tool name.
        pub tool: String,
        /// MCP arguments as normal JSON; never a local path or secret value.
        pub arguments: serde_json::Value,
    }

    /// Home -> agent: the result of one [`ToolCall`].
    ///
    /// Exactly one of `result` and `error` is set. Neither an error nor a result carries a
    /// home secret value or an absolute local path.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    pub struct ToolResult {
        /// Request id from the matching [`ToolCall`].
        pub request_id: String,
        /// Tool result as normal MCP JSON, when the call succeeded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub result: Option<serde_json::Value>,
        /// Typed failure, when the call failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<ToolError>,
    }

    /// A typed tool-call failure.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct ToolError {
        /// Machine-readable code, for example `not_found`, `refused` or `lease_revoked`.
        pub code: String,
        /// Human-readable message; never a secret value or an absolute local path.
        pub message: String,
        /// Whether retrying the same request may succeed.
        pub retryable: bool,
    }

    /// Serialises an [`EventBody`] as the dotted `type` plus its payload fields.
    mod event_body_wire {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use serde_json::Value;

        use crate::event::{EventBody, EventType};

        /// Serialises one event body as a tagged JSON object.
        pub fn serialize<S>(body: &EventBody, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut value = body.to_value();
            // invariant: every EventBody payload serialises as a JSON object.
            let object = value
                .as_object_mut()
                .ok_or_else(|| serde::ser::Error::custom("event body is not a JSON object"))?;
            object.insert("type".to_owned(), Value::String(body.event_type().as_str().to_owned()));
            value.serialize(serializer)
        }

        /// Deserialises one event body from its tagged JSON object.
        pub fn deserialize<'de, D>(deserializer: D) -> Result<EventBody, D::Error>
        where
            D: Deserializer<'de>,
        {
            let mut value = Value::deserialize(deserializer)?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| serde::de::Error::custom("event body is not a JSON object"))?;
            let tag = object
                .remove("type")
                .ok_or_else(|| serde::de::Error::custom("event body is missing the type tag"))?;
            let event_type: EventType = serde_json::from_value(tag).map_err(serde::de::Error::custom)?;
            EventBody::from_parts(event_type, value).map_err(serde::de::Error::custom)
        }
    }

    /// Schema-only shape of an [`EventBody`]: the dotted event type plus its payload fields.
    #[derive(JsonSchema)]
    #[allow(dead_code)] // schema-only: this type is never constructed
    struct EventBodySchema {
        /// Dotted event type, for example `assistant.message`.
        #[schemars(rename = "type")]
        event_type: EventType,
    }
}
