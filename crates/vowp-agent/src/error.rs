//! Typed errors returned by the agent client.

use vowp_protocol::ErrorKind;

/// Everything the agent can refuse or fail to do.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A local filesystem operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A config value or file is invalid.
    #[error("config: {0}")]
    Config(String),
    /// The peer sent a frame or handshake value that violates the protocol.
    #[error("protocol: {0}")]
    Protocol(String),
    /// The transport could not be opened, read or written.
    #[error("transport: {0}")]
    Transport(String),
    /// The home rejected the node key or its signature.
    #[error("authentication rejected: {0}")]
    Auth(String),
    /// The home repeated a nonce this client already signed.
    #[error("replayed handshake nonce")]
    ReplayNonce,
    /// A frame exceeded the negotiated maximum size.
    #[error("frame is {size} bytes; the maximum is {max}")]
    FrameTooLarge {
        /// Size of the rejected frame.
        size: usize,
        /// Negotiated maximum size.
        max: usize,
    },
    /// This machine has no usable enrollment.
    #[error("not enrolled: {0}")]
    NotEnrolled(String),
    /// The node key file is missing or corrupt.
    #[error("node key: {0}")]
    Key(String),
    /// The home returned an [`ErrorKind::Auth`] or retryable protocol error.
    #[error("home error ({kind:?}): {message}")]
    Remote {
        /// Machine-readable error class.
        kind: ErrorKind,
        /// Human-readable detail from the home.
        message: String,
        /// Whether reconnecting may succeed.
        retryable: bool,
    },
    /// The node credential was revoked.
    #[error("revoked: {0}")]
    Revoked(String),
    /// Another connection took over.
    #[error("superseded by {0}")]
    Superseded(String),
    /// A JSON value could not be encoded or decoded.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl AgentError {
    /// Returns whether retrying the connection can plausibly help.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Auth(_)
            | Self::ReplayNonce
            | Self::NotEnrolled(_)
            | Self::Config(_)
            | Self::Key(_)
            | Self::Protocol(_)
            | Self::FrameTooLarge { .. }
            | Self::Json(_) => true,
            Self::Remote { retryable, .. } => !*retryable,
            Self::Io(_) | Self::Transport(_) => false,
            Self::Revoked(_) | Self::Superseded(_) => true,
        }
    }
}
