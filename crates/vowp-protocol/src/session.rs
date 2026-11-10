//! Sessions, nodes, projects, switches and the ledger.
//! Spec: `docs/design/types.md` §9–10 and `docs/design/sessions.md`.
//!
//! A [`Session`] is a universal conversation with an append-only event log; an
//! [`EventIndexRow`] indexes one log line. [`Switch`] records an executor change and
//! [`LedgerEntry`] is the append-only operational record.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::event::{Actor, EventType, SwitchReason};
use crate::executor::{Executor, HarnessKind};
use crate::ids::{CapsuleId, EventId, JobId, NodeName, ProjectName, RunId, SessionId, SwitchId};
use crate::run::RunRole;
use crate::time::Timestamp;

/// The lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// No turn is running.
    Idle,
    /// A turn is running.
    Running,
    /// Waiting on a limit or a human.
    Blocked,
    /// An executor switch is in progress.
    Switching,
    /// The session is closed.
    Closed,
}

/// Who a session serves.
///
/// Defined beside the `session.created` payload in [`crate::event`] because the event carries
/// the role; re-exported here with the other session rows.
pub use crate::event::SessionRole;

/// One universal conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// Session id.
    pub id: SessionId,
    /// Project the session belongs to, when any.
    pub project: Option<ProjectName>,
    /// Job the session works, when any.
    pub job_id: Option<JobId>,
    /// Who the session serves.
    pub role: Option<SessionRole>,
    /// Human-readable title.
    pub title: String,
    /// Session this one was forked from.
    pub parent_session_id: Option<SessionId>,
    /// Node that owns the session log.
    pub node: NodeName,
    /// Working directory.
    pub cwd: String,
    /// Current state.
    pub state: SessionState,
    /// Current executor.
    pub executor: Option<Executor>,
    /// Sequence number of the last event.
    pub last_seq: u64,
    /// Capsule written most recently.
    pub last_capsule_id: Option<CapsuleId>,
    /// Size of the canonical log in bytes.
    pub log_bytes: u64,
    /// When the session was created.
    pub created_at: Timestamp,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// One row of the `session_events` index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventIndexRow {
    /// Session the event belongs to.
    pub session_id: SessionId,
    /// Sequence number within the session.
    pub seq: u64,
    /// Event id.
    pub id: EventId,
    /// Event timestamp.
    pub ts: Timestamp,
    /// Dotted event type.
    pub event_type: EventType,
    /// Who produced the event.
    pub actor: Actor,
    /// Harness that produced the event.
    pub harness: HarnessKind,
    /// Model name, when the harness reported one.
    pub model: Option<String>,
    /// Byte offset of the line in the log.
    pub byte_offset: u64,
    /// Byte length of the line in the log.
    pub byte_len: u64,
    /// One-line summary.
    pub summary: String,
}

/// State of an executor switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwitchState {
    /// Capsule written and target starting.
    Started,
    /// Target accepted the seed.
    Completed,
    /// Target failed to start; the old executor is kept.
    Aborted,
}

/// One executor switch on a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Switch {
    /// Switch id.
    pub id: SwitchId,
    /// Session switched.
    pub session_id: SessionId,
    /// Executor before the switch.
    pub from_executor: Option<Executor>,
    /// Executor after the switch.
    pub to_executor: Executor,
    /// Node before the switch.
    pub from_node: Option<NodeName>,
    /// Node after the switch.
    pub to_node: NodeName,
    /// Capsule used as the seed.
    pub capsule_id: Option<CapsuleId>,
    /// Why the switch happened.
    pub reason: SwitchReason,
    /// Current state.
    pub state: SwitchState,
    /// Error that aborted the switch.
    pub error: Option<String>,
    /// When the switch started.
    pub started_at: Timestamp,
    /// When it completed or aborted.
    pub ended_at: Option<Timestamp>,
    /// Log sequence at the capsule.
    pub at_seq: u64,
}

/// One node of the VOWP network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    /// Node name slug.
    pub name: NodeName,
    /// ssh destination, when the node is remote.
    pub ssh_target: Option<String>,
    /// Whether this node is the local machine.
    pub is_local: bool,
    /// Advertised capabilities.
    pub capabilities: serde_json::Value,
    /// Last successful contact.
    pub last_seen_at: Option<Timestamp>,
    /// When the node was registered.
    pub created_at: Timestamp,
}

/// One project homed on a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    /// Project name slug.
    pub name: ProjectName,
    /// Absolute path of the project root.
    pub path: String,
    /// Git remote URL.
    pub remote_url: Option<String>,
    /// Branch merges target.
    pub default_branch: String,
    /// Parsed `vowp.toml`.
    pub config: serde_json::Value,
    /// When the project was added.
    pub created_at: Timestamp,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// Kind of an append-only ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A job or turn was dispatched.
    Dispatch,
    /// A gate was evaluated.
    Gate,
    /// A merge happened.
    Merge,
    /// A cost was recorded.
    Cost,
    /// An executor switch happened.
    Switch,
    /// A provider limit was recorded.
    Limit,
    /// A promotion started.
    Promote,
    /// A release was rolled back.
    Rollback,
    /// An inbox item was created or answered.
    Inbox,
    /// A schedule fired.
    Schedule,
    /// A job state changed.
    State,
    /// A free-form operator note.
    Note,
}

/// One append-only ledger row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LedgerEntry {
    /// Row id assigned by the store.
    pub id: Option<i64>,
    /// When the event happened.
    pub ts: Timestamp,
    /// Row kind.
    pub kind: LedgerKind,
    /// Project the row belongs to, when any.
    pub project: Option<ProjectName>,
    /// Job the row belongs to, when any.
    pub job_id: Option<JobId>,
    /// Run the row belongs to, when any.
    pub run_id: Option<RunId>,
    /// Session the row belongs to, when any.
    pub session_id: Option<SessionId>,
    /// Role, set on cost rows.
    pub role: Option<RunRole>,
    /// Provider key, when the row has a cost.
    pub provider: Option<String>,
    /// Harness, when known.
    pub harness: Option<HarnessKind>,
    /// Model name, when known.
    pub model: Option<String>,
    /// Cost, on cost rows.
    pub cost: Option<Cost>,
    /// One-line summary.
    pub summary: String,
    /// Structured detail.
    pub detail: serde_json::Value,
}
