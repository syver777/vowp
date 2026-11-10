//! The typed error enum shared by every module of the crate.
//! Spec: `docs/design/crates/vowp-graph.md` §9.

use vowp_protocol::{ErrorKind, JobId, JobState, ProtocolError};

/// Everything `vowp-graph` can refuse.
///
/// Validation errors carry the offending id so callers can report which row or edge is
/// wrong without parsing an English message.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// Two jobs in the same graph share an id.
    #[error("duplicate job id {0}")]
    DuplicateJob(JobId),
    /// An edge or a `needs` entry references a job that is not in the graph.
    #[error("unknown job {0} referenced by an edge")]
    UnknownJob(JobId),
    /// An edge starts and ends at the same job.
    #[error("self edge on {0}")]
    SelfEdge(JobId),
    /// The precedence constraints form a cycle; the path starts and ends at the same job.
    #[error("dependency cycle: {0:?}")]
    Cycle(Vec<JobId>),
    /// A `needs` edge is not mirrored by the source job's `needs` list.
    #[error("edge/needs mismatch on {0}")]
    EdgeMismatch(JobId),
    /// The state machine has no row for this event in this state.
    #[error("invalid transition: {event} in state {state:?}")]
    InvalidTransition {
        /// State the job was in.
        state: JobState,
        /// Debug rendering of the rejected event.
        event: String,
    },
    /// An ownership glob cannot be interpreted.
    #[error("invalid owns pattern {0:?}")]
    InvalidOwns(String),
    /// A graph YAML document could not be written or read.
    #[error("yaml: {0}")]
    Yaml(String),
    /// A protocol type rejected its input while building a graph value.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

impl GraphError {
    /// Maps the error to the stable [`ErrorKind`] vocabulary used by `--json` output.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::DuplicateJob(_) | Self::Cycle(_) | Self::EdgeMismatch(_) | Self::InvalidTransition { .. } => {
                ErrorKind::Conflict
            }
            Self::UnknownJob(_) => ErrorKind::NotFound,
            Self::SelfEdge(_) | Self::InvalidOwns(_) => ErrorKind::InvalidInput,
            Self::Yaml(_) => ErrorKind::Parse,
            Self::Protocol(err) => err.kind(),
        }
    }
}
