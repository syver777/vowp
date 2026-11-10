//! Service traits that decouple `vowp-core` loops from the services they call.
//! Spec: `docs/design/types.md` §13 and `docs/design/crates/vowp-core.md`.
//!
//! [`SessionOps`] is the boundary between the loops and the real session service: loops call
//! it to create sessions, run turns, switch executors and write capsules, and their tests use
//! fakes. [`CancelToken`] is the cooperative cancellation shared by supervisor and runner
//! threads.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capsule::Capsule;
use crate::event::{CapsuleTrigger, LimitEvent, SessionRole, SwitchReason};
use crate::executor::{Executor, ExecutorRef};
use crate::ids::{JobId, ProjectName, RunId, SessionId};
use crate::run::{RunPurpose, RunRole};
use crate::session::Switch;

/// The session operations the loops need.
pub trait SessionOps: Send + Sync {
    /// Creates a session: row, directory and `session.created` event.
    fn create_session(&self, req: NewSession) -> Result<SessionId, ServiceError>;

    /// Runs one turn on `req.executor`, blocking until it ends.
    ///
    /// The capsule seed is rendered as the prompt prefix when the session has no native
    /// session on this harness.
    fn run_turn(&self, req: TurnRequest, cancel: &CancelToken) -> Result<TurnResult, ServiceError>;

    /// Switches the session's executor/protocol, writing a capsule first.
    fn switch(&self, req: SwitchRequest) -> Result<Switch, ServiceError>;

    /// Writes a capsule for the session, triggered by `trigger`.
    fn write_capsule(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, ServiceError>;
}

/// Input for creating a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSession {
    /// Project the session belongs to, when any.
    pub project: Option<ProjectName>,
    /// Job the session works, when any.
    pub job_id: Option<JobId>,
    /// Who the session serves.
    pub role: SessionRole,
    /// Human-readable title.
    pub title: String,
    /// Working directory.
    pub cwd: PathBuf,
    /// Session this one is forked from, when any.
    pub parent: Option<SessionId>,
    /// Initial executor; `vowp session new --executor`.
    #[serde(default)]
    pub executor: Option<Executor>,
}

/// One turn to run on a session.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnRequest {
    /// Session to run on.
    pub session_id: SessionId,
    /// Executor and optional node to run.
    pub executor: ExecutorRef,
    /// Why the run was started.
    pub purpose: RunPurpose,
    /// Role the run serves.
    pub role: RunRole,
    /// Job being worked, when any.
    pub job_id: Option<JobId>,
    /// Prompt for this turn.
    pub prompt: String,
    /// Profile that picked the executor.
    pub profile: Option<String>,
    /// Ladder rung used.
    pub rung: u32,
    /// tmux window to mirror the run in.
    pub window: Option<String>,
    /// Wall-clock cap for the run.
    pub timeout: Option<std::time::Duration>,
}

/// The outcome of one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnResult {
    /// Run row created for the turn.
    pub run_id: RunId,
    /// Classified run summary.
    pub summary: crate::adapter::RunSummary,
}

/// Request to switch a session to another executor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SwitchRequest {
    /// Session to switch.
    pub session_id: SessionId,
    /// Executor (and node) to switch to.
    pub to: ExecutorRef,
    /// Why the switch happens.
    pub reason: SwitchReason,
    /// Seconds to let the current run drain before stopping it.
    #[serde(default = "default_drain_secs")]
    pub drain_secs: u64,
}

/// Returns the default switch drain time.
fn default_drain_secs() -> u64 {
    5
}

/// Cooperative cancellation shared between supervisor and runner threads.
///
/// Cloning a token shares the underlying flag: cancelling any clone cancels them all.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Errors raised by session service operations.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The operation failed for the stated reason.
    #[error("{0}")]
    Failed(String),
    /// The requested session or row does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A provider limit blocked the operation.
    #[error("limited: {0:?}")]
    Limited(LimitEvent),
    /// Cancellation was requested through the [`CancelToken`].
    #[error("cancelled")]
    Cancelled,
}
