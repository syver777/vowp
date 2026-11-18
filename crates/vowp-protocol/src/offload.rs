//! v0.4 offload wire format: placement, assignments, run states and resume records.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §7.1, §7.2, §7.5, §8.3 and
//! §11.3. This module is pure data: no store, dispatcher, policy evaluation or executor.
//!
//! [`AssignmentPayload`] is the durable, token-free payload the home stores and sends
//! with [`crate::agent::AgentFrame::AssignRun`]. The run lease token is a frame-only
//! secret: the home issues it when it sends the frame, stores only its hash in the lease
//! row, and re-issues a token on resume.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::PathRef;
use crate::config::ResultTransport;
use crate::error::ProtocolError;
use crate::executor::Executor;
use crate::ids::{JobId, NodeName, ProjectName, RunId, SessionId};
use crate::run::RunPurpose;
use crate::surface::SurfaceId;
use crate::time::Timestamp;
use crate::vault::SecretName;

/// Where a job may run.
///
/// The wire form is one short string: `home`, `any`, `node:<name>` or
/// `node:<name>+home`. Parsing rejects an unknown selector, an empty node, whitespace,
/// `/` or `:` inside the node part and an unknown fallback. The node is a
/// [`NodeName`] slug. [`Placement::Home`] is the default and there is no silent
/// fallback: `node:<name>+home` is an explicit opt-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub enum Placement {
    /// The job never leaves the home.
    #[default]
    Home,
    /// The home and every eligible node.
    Any,
    /// Only the named node; `fallback` opts in to the home when the node is ineligible.
    Node {
        /// Node the job is pinned to.
        node: NodeName,
        /// Optional explicit fallback when the node is ineligible.
        fallback: Option<NodeFallback>,
    },
}

impl Placement {
    /// Parses and validates a placement wire string.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        let invalid = || ProtocolError::InvalidId {
            kind: "placement",
            value: s.to_owned(),
        };
        match s {
            "home" => Ok(Self::Home),
            "any" => Ok(Self::Any),
            _ => {
                let rest = s.strip_prefix("node:").ok_or_else(invalid)?;
                let (node, fallback) = match rest.split_once('+') {
                    Some((node, "home")) => (node, Some(NodeFallback::Home)),
                    Some(_) => return Err(invalid()),
                    None => (rest, None),
                };
                if node.is_empty() || node.chars().any(char::is_whitespace) || node.contains(['/', ':']) {
                    return Err(invalid());
                }
                let node = NodeName::parse(node).map_err(|_| invalid())?;
                Ok(Self::Node { node, fallback })
            }
        }
    }
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Home => f.write_str("home"),
            Self::Any => f.write_str("any"),
            Self::Node {
                node,
                fallback: Some(NodeFallback::Home),
            } => write!(f, "node:{node}+home"),
            Self::Node { node, fallback: None } => write!(f, "node:{node}"),
        }
    }
}

impl From<Placement> for String {
    fn from(value: Placement) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for Placement {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl FromStr for Placement {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// What a pinned placement may fall back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeFallback {
    /// Run the job on the home when the pinned node is ineligible.
    Home,
}

/// Why a ready job is waiting instead of running.
///
/// The values are the §7.5 wire vocabulary, in the documented order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    /// The pinned or chosen node is offline.
    NodeOffline,
    /// The node's credentials were revoked.
    NodeRevoked,
    /// The node has no free worker or verifier slot.
    NodeBusy,
    /// The node is on battery and `allow_battery` is false.
    NodeBattery,
    /// Free space on the node is below the reserve.
    NodeDiskLow,
    /// A provider limit blocks the run's executor on that node.
    NodeLimit,
    /// The project has no valid path map on the node.
    PathUnmapped,
    /// A mapped local root or worktree root is missing.
    PathMissing,
    /// The node does not have a required secret name.
    MissingSecret,
    /// The job needs a secret that exists only on the home.
    HomeOnlySecret,
    /// The job kind is not allowed to leave the home.
    KindNotAllowed,
    /// The project is marked `home_only`.
    ProjectHomeOnly,
    /// The node's `origin` does not match the project remote.
    RemoteMismatch,
    /// No node satisfies the placement and eligibility checks.
    NoEligibleNode,
}

/// The durable, token-free payload the home stores and sends with `AssignRun`.
///
/// It carries environment and secret **names** only, a project-relative [`PathRef`],
/// the base commit, the remote name, the project's expected remote URL, the epoch, an
/// optional deadline and the per-run spool cap. It **never** contains a lease token, a
/// secret value or an absolute local path. The lease token travels only in
/// [`crate::agent::AgentFrame::AssignRun`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AssignmentPayload {
    /// Durable assignment id.
    pub assignment_id: String,
    /// Run id assigned by the home.
    pub run_id: RunId,
    /// Session the run writes to.
    pub session_id: SessionId,
    /// Project the run works in.
    pub project: ProjectName,
    /// Job the run belongs to.
    pub job: JobId,
    /// Executor to start.
    pub executor: Executor,
    /// Why the run starts.
    pub purpose: RunPurpose,
    /// Home-seeded prompt.
    pub prompt: String,
    /// Project-relative working directory; the agent resolves it locally.
    pub cwd_ref: PathRef,
    /// Base commit the run starts from.
    pub base_commit: String,
    /// Git remote name on the node.
    pub remote: String,
    /// The home project's remote URL, compared with credentials stripped.
    ///
    /// The agent compares its push URL (not the fetch URL) with this value and refuses a
    /// mismatch as `remote_mismatch`. The home strips URL userinfo before storing it, and the
    /// agent never logs the raw value. A bundle assignment carries an empty value: it needs
    /// no git credential and must not push.
    pub remote_url: String,
    /// How the run returns its result: `push` or `bundle`.
    ///
    /// The assignment's value wins for the whole run; the agent and the home never fall
    /// back to the other transport. An older literal without this field parses as `push`.
    #[serde(default)]
    pub result_transport: ResultTransport,
    /// Environment variable names to resolve on the node; never values.
    pub env_names: Vec<String>,
    /// Secret names to resolve on the node; never values.
    pub secret_names: Vec<SecretName>,
    /// Assignment epoch; a higher epoch supersedes a lower one.
    pub epoch: u64,
    /// Deadline after which the home fences the run, when any.
    pub deadline: Option<Timestamp>,
    /// Per-run spool cap in bytes.
    pub spool_cap_bytes: u64,
}

/// Why an agent refused an assignment before starting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunRejectReason {
    /// The node is not active (revoked, pending or offline).
    NodeInactive,
    /// The project has no path map on the node.
    ProjectUnmapped,
    /// A mapped local root or worktree root is missing.
    PathMissing,
    /// A mapped local root or worktree root is not writable.
    PathNotWritable,
    /// The working directory is outside the project's allowlist.
    CwdOutsideAllow,
    /// The harness binary is not installed on the node.
    HarnessMissing,
    /// A required environment name is absent on the node.
    EnvMissing,
    /// Free disk space is below the reserve.
    DiskLow,
    /// Another run already holds the job's worktree.
    WorktreeConflict,
    /// The local `origin` does not match the project remote.
    OriginMismatch,
    /// Another refusal; [`crate::agent::AgentFrame::RunRejected`] carries the detail.
    Other,
}

/// The state an agent reports for one remote run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRunState {
    /// The harness is running and events are flowing.
    Running,
    /// The home connection dropped; the run may resume within the grace period.
    Disconnected,
    /// The summary or result push is in progress.
    Finishing,
}

/// One run offered back to the home after a reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResume {
    /// Run the node still holds.
    pub run_id: RunId,
    /// Run epoch.
    pub epoch: u64,
    /// Last sequence number the node saw acknowledged by the home.
    pub last_acked: u64,
}

/// One surface offered back to the home after a reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceResume {
    /// Surface the node still holds.
    pub surface: SurfaceId,
    /// Last sequence number the node saw acknowledged by the home.
    pub last_acked: u64,
}

/// The home's answer for one resumed run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunResumeResult {
    /// Run the result belongs to.
    pub run_id: RunId,
    /// Run epoch.
    pub epoch: u64,
    /// Whether the node may continue the run.
    pub accepted: bool,
    /// Human-readable refusal reason when `accepted` is false.
    pub reason: Option<String>,
    /// Last session sequence the node may continue from.
    pub tail_seq: u64,
}

/// The home's answer for one resumed surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SurfaceResumeResult {
    /// Surface the result belongs to.
    pub surface: SurfaceId,
    /// Whether the node may keep the surface attached.
    pub accepted: bool,
    /// Human-readable refusal reason when `accepted` is false.
    pub reason: Option<String>,
    /// Last session sequence the surface may continue from.
    pub tail_seq: u64,
}
