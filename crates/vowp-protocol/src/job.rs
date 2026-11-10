//! Job graph nodes: kinds, states, edges, jobs and partial updates.
//! Spec: `docs/design/types.md` §7 and `docs/design/crates/vowp-protocol.md` §4 rules 8–9.
//!
//! A [`Job`] is one node of the project graph. Its `needs` list is a convenience copy of the
//! `needs` edges; the store fills both from the same rows. State transitions are not defined
//! here: only the predicates [`JobState::is_terminal`] and [`JobState::holds_lock`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::executor::Role;
use crate::gate::Gate;
use crate::ids::{JobId, ProjectName};
use crate::time::Timestamp;

/// What a job does in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Turn an idea into a plan.
    Plan,
    /// Write or refine the specification.
    Spec,
    /// Implement code or docs.
    Build,
    /// Add or run tests.
    Test,
    /// Review another job's work.
    Verify,
    /// Repair the work of a failed verify round.
    Fix,
    /// Merge finished work into the base branch.
    Merge,
    /// Write documentation.
    Docs,
    /// Investigate a question.
    Research,
    /// Promote a build to an environment.
    Promote,
    /// Cut or publish a release.
    Release,
    /// Make a human or planner decision.
    Decision,
}

/// The lifecycle state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for its needs.
    Pending,
    /// Needs satisfied; can be dispatched.
    Ready,
    /// A worker is running it.
    Running,
    /// The running worker stopped making progress.
    Stalled,
    /// Command gates are running.
    Gating,
    /// A verifier is reviewing it.
    Reviewing,
    /// A fix round is running against the original work.
    Fixing,
    /// The merge queue is merging it.
    Merging,
    /// Finished successfully.
    Done,
    /// Finished unsuccessfully; can be retried.
    Failed,
    /// Suspended until a human or limit clears.
    Parked,
    /// The planner replaced it with a new plan.
    Replanned,
    /// A human cancelled it.
    Cancelled,
}

impl JobState {
    /// Returns true for states after which no further work happens.
    ///
    /// [`JobState::Failed`] and [`JobState::Parked`] are deliberately not terminal: both can
    /// return to the graph.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Replanned | Self::Cancelled)
    }

    /// Returns true when the job holds its ownership paths and worktree.
    pub fn holds_lock(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Stalled | Self::Gating | Self::Reviewing | Self::Fixing | Self::Merging
        )
    }

    /// Returns the canonical snake_case state string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Stalled => "stalled",
            Self::Gating => "gating",
            Self::Reviewing => "reviewing",
            Self::Fixing => "fixing",
            Self::Merging => "merging",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Parked => "parked",
            Self::Replanned => "replanned",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The kind of a directed graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The source job cannot start before the destination is done.
    Needs,
    /// The source job verifies the destination.
    Verifies,
    /// The source job fixes a failed verify round of the destination.
    Fixes,
    /// The source job escalates the destination to a human.
    Escalates,
    /// The source job blocks the destination.
    Blocks,
}

impl EdgeKind {
    /// Returns true only for [`EdgeKind::Fixes`], the one loop-forming edge.
    pub fn is_loop_edge(&self) -> bool {
        matches!(self, Self::Fixes)
    }
}

/// One directed edge, read as `src <kind> dst`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct Edge {
    /// Source job.
    pub src: JobId,
    /// Destination job.
    pub dst: JobId,
    /// Relationship from source to destination.
    pub kind: EdgeKind,
}

/// When a fix loop stops: v0.1 only supports "until the gates pass".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopUntil {
    /// Retry until every gate passes.
    #[default]
    Gates,
}

/// What happens when a job's fix rounds are exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    /// Escalate to a human through the inbox.
    #[default]
    Escalate,
    /// Park the job until a human intervenes.
    Park,
    /// Mark the job failed.
    Fail,
}

/// Default bound on fix rounds for a job.
fn default_max_attempts() -> u32 {
    3
}

/// A job's bounded fix loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct JobLoop {
    /// Stop condition; v0.1 only knows `gates`.
    #[serde(default)]
    pub until: LoopUntil,
    /// Maximum fix rounds before `on_exhausted` applies.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Action when the bound is reached.
    #[serde(default)]
    pub on_exhausted: OnExhausted,
}

impl Default for JobLoop {
    fn default() -> Self {
        Self {
            until: LoopUntil::Gates,
            max_attempts: default_max_attempts(),
            on_exhausted: OnExhausted::Escalate,
        }
    }
}

/// Default priority for a new job.
fn default_priority() -> i32 {
    50
}

/// One node of the project job graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    /// Job id slug.
    pub id: JobId,
    /// Project that owns the job.
    pub project: ProjectName,
    /// Human-readable title.
    pub title: String,
    /// What kind of work the job does.
    pub kind: JobKind,
    /// Role that runs it.
    pub role: Role,
    /// Profile name used to pick an executor.
    pub profile: String,
    /// Current lifecycle state.
    pub state: JobState,
    /// Dispatch priority; higher runs first.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// Convenience copy of the `needs` edges.
    #[serde(default)]
    pub needs: Vec<JobId>,
    /// Glob patterns the job may change, repo-relative with `/` separators.
    #[serde(default)]
    pub owns: Vec<String>,
    /// Glob patterns the job may read.
    #[serde(default)]
    pub reads: Vec<String>,
    /// Repo-relative path of the generated brief.
    pub brief: String,
    /// Repo-relative path of the required report.
    pub report: String,
    /// Gates that must pass before review.
    #[serde(default)]
    pub gates: Vec<Gate>,
    /// Bounded fix loop for a failed verify round.
    #[serde(rename = "loop")]
    pub loop_: JobLoop,
    /// Branch the job works on, once created.
    #[serde(default)]
    pub branch: Option<String>,
    /// Worktree path, once created.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Completed fix rounds.
    #[serde(default)]
    pub attempts: u32,
    /// Automatic continues within the current attempt.
    #[serde(default)]
    pub continues: u32,
    /// Restarts after a run hung before producing output.
    #[serde(default)]
    pub hang_restarts: u32,
    /// Why the job is parked, when it is.
    #[serde(default)]
    pub parked_reason: Option<ParkedReason>,
    /// Free-form note from the planner or keeper.
    #[serde(default)]
    pub note: String,
    /// Who created the job (`planner`, a human name, ...).
    pub created_by: String,
    /// Last time the job made observable progress.
    #[serde(default)]
    pub last_progress_at: Option<Timestamp>,
    /// When the state last changed.
    pub state_changed_at: Timestamp,
    /// When the job was created.
    pub created_at: Timestamp,
    /// When the row was last updated.
    pub updated_at: Timestamp,
}

/// Why a job was parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParkedReason {
    /// The account ran out of credits.
    Credits,
    /// Authentication failed or expired.
    Auth,
    /// A question is waiting in the inbox.
    Question,
    /// The provider blocked further runs.
    ProviderBlocked,
    /// A human parked it manually.
    Manual,
    /// The fix-round bound was reached.
    Exhausted,
}

/// Input for `vowp job add` and `Store::insert_job`.
///
/// Everything except `id`, `title`, `kind` and `role` has a default; the caller fills the
/// profile from the role, the brief/report paths from the id and the gates from the kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NewJob {
    /// Job id slug.
    pub id: JobId,
    /// Human-readable title.
    pub title: String,
    /// What kind of work the job does.
    pub kind: JobKind,
    /// Role that runs it.
    pub role: Role,
    /// Profile name; defaults to the role name.
    #[serde(default)]
    pub profile: Option<String>,
    /// Jobs that must be done first.
    #[serde(default)]
    pub needs: Vec<JobId>,
    /// Glob patterns the job may change.
    #[serde(default)]
    pub owns: Vec<String>,
    /// Glob patterns the job may read.
    #[serde(default)]
    pub reads: Vec<String>,
    /// Gates; `None` means the default set for `kind`.
    #[serde(default)]
    pub gates: Option<Vec<Gate>>,
    /// Dispatch priority; `None` means 50.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Bounded fix loop; `None` means the default loop.
    #[serde(default, rename = "loop")]
    pub loop_: Option<JobLoop>,
    /// Brief text the caller writes to `brief_path`.
    #[serde(default)]
    pub brief_text: Option<String>,
}

/// Partial update for `vowp job update`. `None` leaves a field unchanged.
///
/// State is not updatable here: state changes go through the graph state machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobPatch {
    /// New title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// New profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Replacement ownership globs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owns: Option<Vec<String>>,
    /// Replacement read globs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<Vec<String>>,
    /// Replacement gates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gates: Option<Vec<Gate>>,
    /// Replacement fix loop, serialised as `loop`.
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_: Option<JobLoop>,
    /// Edges to add.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_needs: Vec<JobId>,
    /// Edges to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_needs: Vec<JobId>,
    /// Replacement note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
