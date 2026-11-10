//! The single error type for this crate.
//!
//! Spec: `docs/design/crates/vowp-tmux.md` §5. `vowp-core` maps these
//! variants onto protocol error kinds; this crate has no protocol dependency.

use std::time::Duration;

/// Errors returned by the safe tmux window manager.
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    /// The tmux binary could not be spawned.
    #[error("tmux binary not found or not runnable")]
    NotInstalled,

    /// A session or window name is not a safe tmux name.
    #[error("invalid {kind} name {value:?} (allowed: [A-Za-z0-9_.-], not starting with '-')")]
    InvalidName {
        /// What kind of name was rejected (for example `window`).
        kind: &'static str,
        /// The rejected value.
        value: String,
    },

    /// A session outside the `vowp-` namespace was addressed.
    #[error("session {0:?} is not a vowp- session")]
    ForeignSession(String),

    /// A window exists but does not carry the `@vowp=1` tag.
    #[error("window {session}:{name} is not tagged @vowp=1; refusing")]
    NotTagged {
        /// Session containing the window.
        session: String,
        /// Window that was not tagged.
        name: String,
    },

    /// A window with the requested name already exists in the session.
    #[error("window {session}:{name} already exists")]
    WindowExists {
        /// Session containing the window.
        session: String,
        /// Existing window name.
        name: String,
    },

    /// The requested window does not exist.
    #[error("window {session}:{name} not found")]
    WindowNotFound {
        /// Session that was searched.
        session: String,
        /// Window that was requested.
        name: String,
    },

    /// A window command with no argv was requested.
    #[error("empty command")]
    EmptyCommand,

    /// The requested working directory does not exist.
    #[error("working directory does not exist: {0}")]
    InvalidCwd(String),

    /// A deliberately refused destructive operation.
    #[error("refused: {0}")]
    Refused(&'static str),

    /// A tmux command did not finish within the watchdog timeout.
    #[error("tmux timed out after {0:?}")]
    Timeout(Duration),

    /// A tmux command exited unsuccessfully.
    #[error("tmux {args:?} failed with {status:?}: {stderr}")]
    Command {
        /// The argument vector, with environment values omitted.
        args: Vec<String>,
        /// Process exit code, if the process exited normally.
        status: Option<i32>,
        /// Captured standard error.
        stderr: String,
    },

    /// An operating-system error while spawning or reading a child process.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
