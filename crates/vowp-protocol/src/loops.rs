//! Loops, their per-loop bounds, and L9 schedules.
//! Spec: `docs/design/types.md` §10 and `docs/design/loops.md`.
//!
//! A [`Loop`] is a supervisor loop (L1–L9); [`LoopBounds`] carries the numbers that used to be
//! hard-coded. A [`Schedule`] fires an action on a cron or interval spec.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ProjectName, ScheduleId};
use crate::job::NewJob;
use crate::time::Timestamp;

/// The nine supervisor loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum LoopKind {
    /// L1: supervise running jobs.
    L1,
    /// L2: continue or restart stalled jobs.
    L2,
    /// L3: dispatch ready jobs.
    L3,
    /// L4: plan.
    L4,
    /// L5: diagnose stalls and failures.
    L5,
    /// L6: probe limited providers.
    L6,
    /// L7: verify.
    L7,
    /// L8: merge queue.
    L8,
    /// L9: schedules.
    L9,
}

/// Whether a loop or schedule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActiveState {
    /// Running.
    Active,
    /// Paused.
    Paused,
}

/// Per-loop bounds stored in `loops.bounds_json`; unused fields are omitted.
///
/// Fix-round bounds are per job ([`crate::job::JobLoop::max_attempts`]), not loop settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoopBounds {
    /// L2: automatic continues per attempt (default 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_continues: Option<u32>,
    /// L1: minutes without progress before declaring a stall (default 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stall_minutes: Option<u32>,
    /// L1: minutes without any output before declaring a silent stall (default 8).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub silent_minutes: Option<u32>,
    /// L2: restarts of a run that hung before its first output (default 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_hang_restarts: Option<u32>,
    /// Bound on repeating the same action without progress (default 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_action_max: Option<u32>,
    /// L6: minutes between probes of a limited provider (default 15).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_interval_minutes: Option<u32>,
    /// L4/L5: minimum minutes between planner turns (default 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner_min_gap_minutes: Option<u32>,
    /// L4: minutes of idleness before planning (default 15).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_gap_minutes: Option<u32>,
    /// L1: minutes between board/report checkpoints (default 120).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_minutes: Option<u32>,
}

/// One configured loop row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Loop {
    /// Loop name, for example `l4-planner`.
    pub name: String,
    /// Which of L1–L9 this is.
    pub kind: LoopKind,
    /// Project the loop serves; `None` means every project.
    pub project: Option<ProjectName>,
    /// Tick interval in seconds.
    pub interval_secs: u64,
    /// Whether the loop is active.
    pub state: ActiveState,
    /// Per-loop bounds.
    pub bounds: LoopBounds,
    /// Last successful run.
    pub last_run_at: Option<Timestamp>,
    /// Next due time.
    pub next_run_at: Option<Timestamp>,
    /// Short status from the last run.
    pub last_status: String,
    /// Consecutive failed ticks.
    pub fail_streak: u32,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// How a schedule spec is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleSpecKind {
    /// A five-field cron expression.
    Cron,
    /// A fixed interval.
    Interval,
}

/// What to do when several scheduled times were missed while the supervisor was down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissedPolicy {
    /// Run once, then resume the normal cadence.
    RunOnce,
    /// Skip missed occurrences.
    Skip,
}

/// Default timeout for a scheduled command.
fn default_command_timeout() -> u64 {
    crate::gate::default_gate_timeout()
}

/// What a schedule does when due.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleAction {
    /// Create a job from a template; the id gets a `-<yyyymmddhhmm>` suffix.
    JobTemplate {
        /// Template for the new job.
        template: NewJob,
    },
    /// Trigger one run of a loop now.
    Loop {
        /// Loop name, for example `l4-planner`.
        loop_name: String,
    },
    /// Run a command.
    Command {
        /// Program and arguments.
        argv: Vec<String>,
        /// Working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// Timeout in seconds (default 1800).
        #[serde(default = "default_command_timeout")]
        timeout_secs: u64,
    },
}

/// One L9 schedule row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Schedule {
    /// Schedule id.
    pub id: ScheduleId,
    /// Project the schedule belongs to, when any.
    pub project: Option<ProjectName>,
    /// Unique schedule name.
    pub name: String,
    /// Cron or interval.
    pub spec_kind: ScheduleSpecKind,
    /// Cron expression or interval value.
    pub spec: String,
    /// Fixed offset or `UTC`.
    pub tz: String,
    /// What to do when due.
    pub action: ScheduleAction,
    /// Missed-occurrence policy.
    pub missed_policy: MissedPolicy,
    /// Whether the schedule is active.
    pub state: ActiveState,
    /// Last fire time.
    pub last_run_at: Option<Timestamp>,
    /// Next due time.
    pub next_run_at: Option<Timestamp>,
    /// Short status from the last fire.
    pub last_status: String,
    /// When the row was created.
    pub created_at: Timestamp,
}
