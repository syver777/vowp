//! The CLI error type and its mapping onto exit codes.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §2 and `docs/design/cli.md` §1.1. Every command
//! handler returns [`CliError`]; only [`crate::run`] decides how to print it and which process
//! exit code to produce.

use vowp_protocol::ErrorKind;

/// Everything a command can refuse.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The caller supplied a value that violates a rule.
    #[error("{0}")]
    Invalid(String),
    /// The requested row or file does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The operation conflicts with the current state.
    #[error("{0}")]
    Conflict(String),
    /// A guard rejected the action.
    #[error("{0}")]
    Guard(String),
    /// A provider limit or authentication problem blocked the action.
    #[error("{0}")]
    Limited(String),
    /// A remote node operation failed.
    #[error("remote: {0}")]
    Remote(String),
    /// The command is not implemented in this build.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    /// A `vowp-core` operation failed.
    #[error(transparent)]
    Core(#[from] vowp_core::CoreError),
    /// A store operation failed.
    #[error(transparent)]
    Store(#[from] vowp_store::StoreError),
    /// A protocol type rejected its input.
    #[error(transparent)]
    Protocol(#[from] vowp_protocol::ProtocolError),
    /// A filesystem or process I/O operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl CliError {
    /// Maps the error to its stable machine-readable [`ErrorKind`].
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Invalid(_) => ErrorKind::InvalidInput,
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::Conflict(_) => ErrorKind::Conflict,
            Self::Guard(_) => ErrorKind::Guard,
            Self::Limited(_) => ErrorKind::Limit,
            Self::Remote(_) => ErrorKind::Remote,
            Self::NotImplemented(_) => ErrorKind::Internal,
            Self::Core(error) => error.kind(),
            Self::Store(error) => error.kind(),
            Self::Protocol(error) => error.kind(),
            Self::Io(_) => ErrorKind::Io,
        }
    }

    /// Returns the process exit code for this error (`cli.md` §1.1).
    pub fn exit_code(&self) -> i32 {
        exit_code_for_kind(self.kind())
    }

    /// Returns a short command suggestion for common `not found` failures.
    pub fn hint(&self) -> Option<String> {
        if self.kind() != ErrorKind::NotFound {
            return None;
        }
        let text = self.to_string();
        if text.contains("job ") {
            Some("vowp job ls".to_owned())
        } else if text.contains("session") {
            Some("vowp session ls".to_owned())
        } else if text.contains("node ") {
            Some("vowp node ls".to_owned())
        } else if text.contains("inbox") {
            Some("vowp inbox ls".to_owned())
        } else if text.contains("project") {
            Some("vowp init".to_owned())
        } else if text.contains("loop") {
            Some("vowp loops".to_owned())
        } else {
            None
        }
    }
}

impl From<vowp_graph::GraphError> for CliError {
    fn from(error: vowp_graph::GraphError) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// Maps an [`ErrorKind`] onto the process exit code table in `cli.md` §1.1.
pub fn exit_code_for_kind(kind: ErrorKind) -> i32 {
    match kind {
        ErrorKind::InvalidInput => 2,
        ErrorKind::NotFound => 3,
        ErrorKind::Conflict => 4,
        ErrorKind::Guard | ErrorKind::Refused => 5,
        ErrorKind::Limit | ErrorKind::Auth => 6,
        ErrorKind::Remote => 7,
        ErrorKind::Internal
        | ErrorKind::Io
        | ErrorKind::Db
        | ErrorKind::Parse
        | ErrorKind::Config
        | ErrorKind::AdapterFailed
        | ErrorKind::AdapterUnavailable
        | ErrorKind::Timeout => 1,
    }
}
