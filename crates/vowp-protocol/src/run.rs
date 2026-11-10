//! Runs and workers: one adapter process and the slot that owns it.
//! Spec: `docs/design/types.md` §9.
//!
//! A [`Run`] is one harness invocation. `executor` is `None` for shell, gate and merge runs;
//! `cost` is always present and may be [`crate::cost::Cost::unknown`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::executor::Executor;
use crate::ids::{JobId, NodeName, RunId, SessionId};
use crate::limit::LimitKind;
use crate::time::Timestamp;

/// The role a run serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    /// Planner turn.
    Planner,
    /// Keeper turn.
    Keeper,
    /// Job worker.
    Worker,
    /// Verifier.
    Verifier,
    /// Capsule writer.
    Capsule,
    /// Limit probe.
    Probe,
    /// Merge-queue shell run.
    Merge,
    /// Gate-evaluation shell run.
    Gate,
}

/// Why a run was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunPurpose {
    /// First run of a job.
    Start,
    /// Continue a job run that made progress.
    Continue,
    /// Fix round after a failed verify.
    Fix,
    /// Verify another job.
    Verify,
    /// Plan work.
    Plan,
    /// Diagnose stalled or failed jobs.
    Keep,
    /// Write a capsule.
    Capsule,
    /// Probe a limited provider.
    Probe,
    /// Merge work.
    Merge,
    /// Evaluate gates.
    Gate,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Completed normally.
    Ok,
    /// Hit a provider quota limit.
    Limit,
    /// Ran out of credits.
    Credits,
    /// Authentication failed.
    Auth,
    /// Exhausted the context window.
    Context,
    /// Hit a request rate limit.
    Rate,
    /// The harness crashed.
    Crash,
    /// The supervisor declared it stalled.
    Stalled,
    /// A human or supervisor killed it.
    Killed,
    /// Ended with an error.
    Error,
}

/// One adapter process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    /// Run id.
    pub id: RunId,
    /// Job being worked, when the run belongs to one.
    pub job_id: Option<JobId>,
    /// Session being continued, when there is one.
    pub session_id: Option<SessionId>,
    /// Role the run serves.
    pub role: RunRole,
    /// Why it was started.
    pub purpose: RunPurpose,
    /// Profile that picked the executor.
    pub profile: Option<String>,
    /// Ladder rung used.
    pub rung: u32,
    /// Executor, or `None` for shell/gate/merge runs.
    pub executor: Option<Executor>,
    /// Provider key used for limits.
    pub provider: Option<String>,
    /// Node the run runs on.
    pub node: NodeName,
    /// Harness-side session id, when the harness has one.
    pub native_session_id: Option<String>,
    /// Process id, when a process was started.
    pub pid: Option<u32>,
    /// tmux window name.
    pub window: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// When the process started.
    pub started_at: Timestamp,
    /// When the process ended.
    pub ended_at: Option<Timestamp>,
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// Classified outcome.
    pub outcome: Option<RunOutcome>,
    /// Limit class that ended the run, when one did.
    pub limit_kind: Option<LimitKind>,
    /// Cost and token usage.
    pub cost: Cost,
    /// Redacted detail for the board and ledger.
    pub detail: String,
}

/// What a worker slot is allowed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    /// Runs job work.
    Worker,
    /// Runs review work.
    Verifier,
}

/// Current occupancy of a worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Free for dispatch.
    Idle,
    /// Running a job.
    Busy,
    /// Finishing its current run and then stopping.
    Draining,
}

/// One worker slot on a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Worker {
    /// Node that owns the slot.
    pub node: NodeName,
    /// Slot number on that node.
    pub slot: u32,
    /// What the slot may run.
    pub kind: WorkerKind,
    /// Current occupancy.
    pub state: WorkerState,
    /// Run currently occupying the slot.
    pub run_id: Option<RunId>,
    /// Job currently occupying the slot.
    pub job_id: Option<JobId>,
    /// tmux window of the current run.
    pub window: Option<String>,
    /// When the row last changed.
    pub updated_at: Timestamp,
}
