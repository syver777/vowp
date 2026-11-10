//! The adapter contract shared by `vowp-core` and the harness implementations.
//! Spec: `docs/design/types.md` §12 and `docs/design/adapters.md`.
//!
//! Adapters use blocking I/O and plain threads: [`Adapter::start`] spawns one harness process
//! for one turn and never blocks for the run itself, while [`AdapterRun`] exposes the
//! normalised event stream and the process controls. The trait lives here so core depends on
//! the contract, not on a concrete adapter module.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cost::Cost;
use crate::event::{HarnessEvent, LimitEvent, StopReason};
use crate::executor::{Executor, HarnessKind};
use crate::ids::SessionId;
use crate::run::RunOutcome;

/// What an adapter implementation can do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdapterCapabilities {
    /// `start(resume=Some(id))` continues the harness's own session.
    pub can_resume_native: bool,
    /// Assistant history can be injected natively; false for every v0.1 adapter.
    pub can_inject_assistant_history: bool,
    /// The model can change inside a live native session.
    pub can_switch_model_in_session: bool,
    /// [`AdapterRun::send`] works while a run is active; false in v0.1.
    pub live_input: bool,
    /// The harness emits reasoning events.
    pub emits_reasoning: bool,
    /// The harness reports cost receipts.
    pub emits_cost_receipts: bool,
    /// The harness reports when a limit resets.
    pub reports_resets_at: bool,
}

/// The result of a cheap adapter probe (`--version`, never a model call).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeResult {
    /// Whether the harness binary was found and is runnable.
    pub ok: bool,
    /// Reported version, when one was printed.
    pub version: Option<String>,
    /// Resolved binary path, when found.
    pub path: Option<String>,
    /// Human-readable detail for `vowp doctor`.
    pub detail: String,
}

/// Everything one adapter run needs to start.
#[derive(Debug, Clone)]
pub struct StartInput {
    /// Session the run belongs to.
    pub session_id: SessionId,
    /// Executor to run.
    pub executor: Executor,
    /// Working directory for the harness process.
    pub cwd: PathBuf,
    /// The user/continue/seed message for this turn.
    pub prompt: String,
    /// Native session id to resume; only used when the adapter can resume natively.
    pub resume: Option<String>,
    /// Pre-chosen native session id for harnesses that accept one.
    pub new_native_id: Option<String>,
    /// Extra environment names and values, resolved by the caller.
    pub env: Vec<(String, String)>,
    /// Permission mode passed to the harness.
    pub permission_mode: Option<String>,
    /// Extra system note appended where the harness supports it.
    pub system_note: Option<String>,
    /// Where the supervisor mirrors raw output; adapters may tee raw lines here.
    pub event_log_hint: Option<PathBuf>,
    /// Overall wall-clock cap; `None` means no cap (stalls are handled by L1).
    pub timeout: Option<Duration>,
}

/// The classified result of one finished run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    /// Process exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Classified outcome.
    pub outcome: RunOutcome,
    /// Harness-side session id, when the harness reported one.
    pub native_session_id: Option<String>,
    /// Cost and token usage for the run.
    pub cost: Cost,
    /// Limit that ended or affected the run, when one was classified.
    pub limit: Option<LimitEvent>,
    /// Final assistant text, when the harness reported one.
    pub result_text: Option<String>,
    /// Why the turn stopped.
    pub stop_reason: StopReason,
}

/// One harness implementation.
pub trait Adapter: Send + Sync {
    /// Returns which harness this adapter drives.
    fn kind(&self) -> HarnessKind;

    /// Probes the harness binary cheaply, without calling a model.
    fn probe(&self) -> ProbeResult;

    /// Returns what this adapter can do.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Spawns the harness in headless JSON mode for one run and returns immediately.
    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError>;
}

/// One running harness process.
pub trait AdapterRun: Send {
    /// Returns the process id, when a process was started.
    fn pid(&self) -> Option<u32>;

    /// Returns the ordered normalised event stream.
    ///
    /// The channel closes after the terminal event. Adapters emit a terminal
    /// [`crate::event::EventBody::TurnEnded`] on every exit path.
    fn events(&self) -> &Receiver<HarnessEvent>;

    /// Sends live input; returns [`AdapterError::Unsupported`] unless the adapter has
    /// [`AdapterCapabilities::live_input`].
    fn send(&mut self, text: &str) -> Result<(), AdapterError>;

    /// Interrupts the run with SIGINT to the process group, then stops it after 5 seconds.
    fn interrupt(&mut self) -> Result<(), AdapterError>;

    /// Stops the run: SIGTERM, 3 seconds, then SIGKILL of the process group. Idempotent.
    fn stop(&mut self) -> Result<(), AdapterError>;

    /// Blocks until the process exits and the reader threads drain, then returns the summary.
    /// Idempotent after the first call.
    fn wait(&mut self) -> Result<RunSummary, AdapterError>;
}

/// Errors raised by adapter implementations.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The harness binary was not found or is not runnable.
    #[error("harness not found or not runnable: {0}")]
    Unavailable(String),
    /// The harness process could not be spawned.
    #[error("failed to spawn {binary}: {source}")]
    Spawn {
        /// Binary that was attempted.
        binary: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The adapter does not support the requested operation.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// The generic `cli` adapter's mapping file is invalid.
    #[error("invalid mapping file {path}: {message}")]
    Mapping {
        /// Mapping file path.
        path: String,
        /// What is wrong with it.
        message: String,
    },
    /// An I/O operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The harness violated the adapter protocol.
    #[error("protocol violation from harness: {0}")]
    Protocol(String),
}
