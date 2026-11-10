//! Cross-crate error vocabulary: [`ErrorKind`] for machine-readable classes and
//! [`ProtocolError`] for protocol parsing and validation failures.
//! Spec: `docs/design/types.md` §2 and `docs/design/crates/vowp-protocol.md` §5.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable machine-readable error classes, used in `error` events, `--json` output and
/// CLI exit-code mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The caller supplied a value that violates a rule.
    InvalidInput,
    /// The requested item does not exist.
    NotFound,
    /// The operation conflicts with current state.
    Conflict,
    /// A filesystem or process I/O operation failed.
    Io,
    /// A database operation failed.
    Db,
    /// Input could not be parsed.
    Parse,
    /// Configuration is invalid.
    Config,
    /// A harness binary or service is not available.
    AdapterUnavailable,
    /// A harness run failed.
    AdapterFailed,
    /// A provider limit blocked the run.
    Limit,
    /// Authentication with a provider failed.
    Auth,
    /// An operation exceeded its deadline.
    Timeout,
    /// A gate rejected the work.
    Guard,
    /// The action was refused (for example a public action without approval).
    Refused,
    /// A remote node operation failed.
    Remote,
    /// An unexpected internal failure.
    Internal,
}

/// Errors raised by the protocol crate while parsing or validating values.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// An id failed validation.
    #[error("invalid id {kind}: {value:?}")]
    InvalidId {
        /// Which id grammar was expected (`ulid`, `slug` or `capsule`).
        kind: &'static str,
        /// The rejected input.
        value: String,
    },
    /// A timestamp failed RFC 3339 parsing.
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),
    /// An executor string failed to parse or validate.
    #[error("invalid executor {0:?}: {1}")]
    InvalidExecutor(String, String),
    /// An event payload does not match its declared event type.
    #[error("invalid event payload for {event_type}: {source}")]
    InvalidPayload {
        /// The dotted event type being decoded.
        event_type: String,
        /// The underlying JSON error.
        source: serde_json::Error,
    },
    /// Report or brief front-matter is invalid.
    #[error("invalid front-matter: {0}")]
    FrontMatter(String),
    /// A gate definition is invalid.
    #[error("invalid gate: {0}")]
    InvalidGate(String),
    /// Configuration is invalid.
    #[error("invalid config: {0}")]
    Config(String),
    /// A JSON value could not be serialised or deserialised.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl ProtocolError {
    /// Maps the error to its stable [`ErrorKind`] class.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidId { .. }
            | Self::InvalidTimestamp(_)
            | Self::InvalidExecutor(..)
            | Self::InvalidGate(_)
            | Self::FrontMatter(_) => ErrorKind::InvalidInput,
            Self::InvalidPayload { .. } | Self::Json(_) => ErrorKind::Parse,
            Self::Config(_) => ErrorKind::Config,
        }
    }
}
