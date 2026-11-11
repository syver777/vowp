//! The versioned `snapshot.json` document and the project registry view.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §5 and §13.1. The snapshot shape is fixed
//! here; the node supervisor builds and writes the snapshot every tick.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::capacity::CapacitySnapshot;
use crate::event::SessionRole;
use crate::executor::Role;
use crate::ids::{InboxId, JobId, NodeName, ProjectName, SessionId};
use crate::inbox::InboxKind;
use crate::job::{JobKind, JobState};
use crate::limit::Limit;
use crate::session::SessionState;
use crate::surface::{OrchestratorCard, SurfaceRef};
use crate::time::Timestamp;

/// Format marker written into every snapshot.
pub const SNAPSHOT_FORMAT: &str = "vowp-snapshot/1";
/// Version of the snapshot document.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Registry state of one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    /// The project is ticked.
    Active,
    /// The project gets maintenance ticks but starts no new work.
    Paused,
    /// The project is skipped entirely.
    Archived,
}

/// One row of the project registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectEntry {
    /// Project name.
    pub name: ProjectName,
    /// Absolute path of the main checkout on the home.
    pub path: String,
    /// Git remote, when known.
    pub remote_url: Option<String>,
    /// Default branch.
    pub default_branch: String,
    /// Registry state.
    pub state: ProjectState,
    /// Fairness weight, `1..=100`.
    pub weight: u8,
    /// Why the project is paused (`owner` or `broken: <error>`).
    pub paused_reason: String,
    /// When the project was archived.
    pub archived_at: Option<Timestamp>,
    /// When the project was registered.
    pub created_at: Timestamp,
    /// Last registry update.
    pub updated_at: Timestamp,
}

/// The supervisor card of the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SupervisorSnapshot {
    /// Whether a supervisor process is running.
    pub running: bool,
    /// Supervisor process id, when running.
    pub pid: Option<u32>,
    /// When the supervisor started.
    pub started_at: Option<Timestamp>,
    /// Last completed tick.
    pub last_tick: Option<Timestamp>,
    /// Tick interval in seconds.
    pub tick_secs: u64,
    /// Whether the node is paused.
    pub paused: bool,
    /// Consecutive failed L1 ticks.
    pub fail_streak: u32,
    /// Binary version, for example `0.2.0`.
    pub version: String,
}

impl Default for SupervisorSnapshot {
    fn default() -> Self {
        Self {
            running: false,
            pid: None,
            started_at: None,
            last_tick: None,
            tick_secs: 0,
            paused: false,
            fail_streak: 0,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// One running job card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningJobCard {
    /// Job id.
    pub job: JobId,
    /// Job kind.
    pub kind: JobKind,
    /// Job role.
    pub role: Role,
    /// Executor string, when known.
    pub executor: Option<String>,
    /// Node the job runs on.
    pub node: NodeName,
    /// Age of the run in seconds.
    pub age_secs: u64,
    /// Age of the last event in seconds, when known.
    pub last_event_age_secs: Option<u64>,
    /// Session the job runs in, when any.
    pub session_id: Option<SessionId>,
}

/// One blocked job card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BlockedJobCard {
    /// Job id.
    pub job: JobId,
    /// Job state.
    pub state: JobState,
    /// Parked or failure reason.
    pub reason: String,
    /// One redacted detail line, at most 200 characters.
    pub detail: String,
    /// When the job entered this state.
    pub since: Timestamp,
}

/// One queue row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QueueJobCard {
    /// Job id.
    pub job: JobId,
    /// Job title.
    pub title: String,
    /// Job kind.
    pub kind: JobKind,
    /// Job role.
    pub role: Role,
    /// Dispatch priority.
    pub priority: i32,
    /// Job state.
    pub state: JobState,
    /// Age in seconds.
    pub age_secs: u64,
    /// Last admission or brake reason.
    pub waiting_reason: Option<String>,
}

/// The newest merge recorded for a project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MergeCard {
    /// Merged job.
    pub job: JobId,
    /// When the merge happened.
    pub at: Timestamp,
    /// Commit sha.
    pub commit: String,
    /// Merged branch.
    pub branch: String,
}

/// One failed gate result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GateFailureCard {
    /// Job the gate ran for.
    pub job: JobId,
    /// Gate name.
    pub gate: String,
    /// When the gate failed.
    pub at: Timestamp,
    /// One redacted evidence line, at most 200 characters.
    pub evidence: String,
}

/// One open inbox row in the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboxSummary {
    /// Inbox item id.
    pub id: InboxId,
    /// Project the item belongs to, when bound.
    pub project: Option<ProjectName>,
    /// Job the item belongs to, when bound.
    pub job_id: Option<JobId>,
    /// Item kind.
    pub kind: InboxKind,
    /// Item title.
    pub title: String,
    /// When the item was created.
    pub created_at: Timestamp,
    /// Age in seconds.
    pub age_secs: u64,
}

/// One session card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSurfaceCard {
    /// Session id.
    pub id: SessionId,
    /// Project the session belongs to, when bound.
    pub project: Option<ProjectName>,
    /// Job the session belongs to, when bound.
    pub job_id: Option<JobId>,
    /// Session role, when set.
    pub role: Option<SessionRole>,
    /// Session title.
    pub title: String,
    /// Session state.
    pub state: SessionState,
    /// Executor string, when known.
    pub executor: Option<String>,
    /// Node the session runs on.
    pub node: NodeName,
    /// The surface that holds the lease, when one is live.
    pub active_surface: Option<SurfaceRef>,
    /// Time of the newest event.
    pub last_event_at: Timestamp,
}

/// One project card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectSnapshot {
    /// Project name.
    pub name: ProjectName,
    /// Registered checkout path.
    pub path: String,
    /// Registry state.
    pub state: ProjectState,
    /// Fairness weight.
    pub weight: u8,
    /// Why the project is paused.
    pub paused_reason: String,
    /// Job counts by state.
    pub jobs_by_state: BTreeMap<String, u32>,
    /// Running jobs.
    pub running: Vec<RunningJobCard>,
    /// Blocked jobs.
    pub blocked: Vec<BlockedJobCard>,
    /// Queued jobs, capped at 200.
    pub queue: Vec<QueueJobCard>,
    /// Newest merge, when any.
    pub last_merge: Option<MergeCard>,
    /// Recent gate failures.
    pub gate_failures: Vec<GateFailureCard>,
    /// Cost spent today in USD; `None` means unknown.
    pub cost_today_usd: Option<f64>,
}

/// The whole `snapshot.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// Format marker, always [`SNAPSHOT_FORMAT`].
    pub schema: String,
    /// Document version, always [`SNAPSHOT_VERSION`].
    pub version: u32,
    /// Write counter; increases every write.
    pub sequence: u64,
    /// When the snapshot was generated.
    pub generated_at: Timestamp,
    /// Name of the home.
    pub home: NodeName,
    /// Supervisor card.
    pub supervisor: SupervisorSnapshot,
    /// Project cards in registry order.
    pub projects: Vec<ProjectSnapshot>,
    /// Capacity card.
    pub capacity: CapacitySnapshot,
    /// Provider limits.
    pub limits: Vec<Limit>,
    /// Open inbox rows.
    pub inbox: Vec<InboxSummary>,
    /// Recent sessions.
    pub sessions: Vec<SessionSurfaceCard>,
    /// Configured orchestrators.
    pub orchestrators: Vec<OrchestratorCard>,
}
