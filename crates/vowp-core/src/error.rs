//! The single error type returned by every module of `vowp-core`.
//! Spec: `docs/design/crates/vowp-core.md` §2 and `docs/design/conventions.md` §4.
//!
//! [`CoreError::kind`] maps the error onto the protocol's machine-readable
//! [`ErrorKind`] vocabulary so the CLI can render `--json` output and choose an exit code.
//! The `From<CoreError> for ServiceError` conversion is the boundary the loops use when a
//! session operation fails.

use vowp_protocol::{AdapterError, ErrorKind, LimitEvent, ProtocolError, ServiceError};
use vowp_tmux::TmuxError;

/// Everything `vowp-core` can refuse.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A store operation failed.
    #[error(transparent)]
    Store(#[from] vowp_store::StoreError),
    /// The job graph rejected a value or transition.
    #[error(transparent)]
    Graph(#[from] vowp_graph::GraphError),
    /// A protocol type rejected its input.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// A harness adapter failed.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// The tmux window manager failed.
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    /// A filesystem or process I/O operation failed.
    #[error("io {path}: {source}")]
    Io {
        /// Path or program the operation was working on.
        path: String,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// A `git` invocation exited unsuccessfully.
    #[error("git {args:?}: exit {code:?}: {stderr}")]
    Git {
        /// Argument vector passed to `git` (without the program name).
        args: Vec<String>,
        /// Process exit code, when the process exited normally.
        code: Option<i32>,
        /// Merged stdout/stderr tail.
        stderr: String,
    },
    /// A command failed for a reason other than an ordinary exit status.
    #[error("command failed: {0}")]
    Command(String),
    /// The capsule writer produced an invalid capsule even after one repair.
    #[error("capsule invalid after repair: {0}")]
    CapsuleInvalid(String),
    /// A session switch was aborted.
    #[error("switch aborted: {0}")]
    SwitchAborted(String),
    /// A provider limit blocked the operation.
    #[error("provider limited: {0:?}")]
    Limited(LimitEvent),
    /// An action was refused and must not be worked around (cwd allow-list, guard).
    #[error("refused: {0}")]
    Refused(String),
    /// A gate rejected the work.
    #[error("guard failed: {name}: {detail}")]
    Guard {
        /// Gate name.
        name: String,
        /// Human-readable evidence detail.
        detail: String,
    },
    /// The requested row or file does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A value violates a core rule.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The operation was cancelled through a cancellation token.
    #[error("cancelled")]
    Cancelled,
}

impl CoreError {
    /// Maps the error to its stable [`ErrorKind`] class.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Store(err) => err.kind(),
            Self::Graph(err) => err.kind(),
            Self::Protocol(err) => err.kind(),
            Self::Adapter(err) => adapter_kind(err),
            Self::Tmux(err) => tmux_kind(err),
            Self::Io { .. } | Self::Git { .. } => ErrorKind::Io,
            Self::Command(_) | Self::CapsuleInvalid(_) => ErrorKind::Internal,
            Self::SwitchAborted(_) | Self::Cancelled => ErrorKind::Conflict,
            Self::Limited(_) => ErrorKind::Limit,
            Self::Refused(_) => ErrorKind::Refused,
            Self::Guard { .. } => ErrorKind::Guard,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::Invalid(_) => ErrorKind::InvalidInput,
        }
    }
}

impl From<CoreError> for ServiceError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Limited(event) => Self::Limited(event),
            CoreError::Cancelled => Self::Cancelled,
            CoreError::NotFound(message) => Self::NotFound(message),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Maps an adapter error onto the protocol error vocabulary.
fn adapter_kind(error: &AdapterError) -> ErrorKind {
    match error {
        AdapterError::Unavailable(_) => ErrorKind::AdapterUnavailable,
        AdapterError::Spawn { .. } | AdapterError::Io(_) => ErrorKind::Io,
        AdapterError::Unsupported(_) => ErrorKind::InvalidInput,
        AdapterError::Mapping { .. } => ErrorKind::Config,
        AdapterError::Protocol(_) => ErrorKind::AdapterFailed,
    }
}

/// Maps a tmux error onto the protocol error vocabulary.
fn tmux_kind(error: &TmuxError) -> ErrorKind {
    match error {
        TmuxError::NotInstalled => ErrorKind::AdapterUnavailable,
        TmuxError::InvalidName { .. }
        | TmuxError::ForeignSession(_)
        | TmuxError::NotTagged { .. }
        | TmuxError::EmptyCommand
        | TmuxError::InvalidCwd(_) => ErrorKind::InvalidInput,
        TmuxError::Refused(_) => ErrorKind::Refused,
        TmuxError::WindowExists { .. } => ErrorKind::Conflict,
        TmuxError::WindowNotFound { .. } => ErrorKind::NotFound,
        TmuxError::Timeout(_) => ErrorKind::Timeout,
        TmuxError::Command { .. } => ErrorKind::Internal,
        TmuxError::Io(_) => ErrorKind::Io,
    }
}
