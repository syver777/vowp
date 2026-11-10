//! Safe tmux window management for VOWP job sessions.
//!
//! tmux windows are views: they run a read-only log follower, never a harness
//! TUI and never a control channel. Every session this crate touches must be
//! in the `vowp-` namespace; foreign windows are never killed. There is no
//! respawn or keystroke capability by design.
//!
//! Spec: `docs/design/crates/vowp-tmux.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod fake;
mod manager;
pub mod quote;
mod tmux;

pub use error::TmuxError;
pub use manager::{
    FIXED_WINDOWS, LayoutSpec, WindowInfo, WindowManager, WindowSpec, session_name_for_project, window_name_for_job,
};
pub use tmux::{Tmux, socket_name_for_home};
