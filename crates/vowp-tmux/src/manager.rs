//! Window manager trait, layout description and naming helpers.
//!
//! Windows are views: each one runs a read-only follower command, never a
//! harness TUI and never a control channel.
//!
//! Spec: `docs/design/crates/vowp-tmux.md` §2, §4, §8.

use std::path::PathBuf;

use crate::error::TmuxError;
use crate::quote::validate_name;

/// One tmux window as reported by [`WindowManager::list_windows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// tmux window index inside its session.
    pub index: u32,
    /// Window name.
    pub name: String,
    /// Whether the window carries the `@vowp=1` tag.
    pub tagged: bool,
    /// PID of the window's active pane process, when tmux reports one.
    pub pane_pid: Option<u32>,
    /// Whether this is the active window of the session.
    pub active: bool,
}

/// What to run in a new window.
///
/// `argv` must be non-empty: an interactive shell window is never created.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowSpec {
    /// Command and arguments, passed without a shell wrapper around the argv.
    pub argv: Vec<String>,
    /// Working directory for the window process.
    pub cwd: Option<PathBuf>,
    /// Environment variables prefixed to the command.
    pub env: Vec<(String, String)>,
}

/// The operations the supervisor needs from a tmux server.
///
/// Implementations must enforce the safety rules: only `vowp-` sessions,
/// validated names, tagged windows only, and no respawn or keystroke paths.
pub trait WindowManager: Send + Sync {
    /// Create the session if it does not exist; the session survives through a
    /// tagged `scratch` window.
    fn ensure_session(&self, session: &str) -> Result<(), TmuxError>;

    /// Create a tagged window and start `spec` in it.
    ///
    /// Returns [`TmuxError::WindowExists`] if the name is already present.
    fn new_window(&self, session: &str, name: &str, spec: &WindowSpec) -> Result<(), TmuxError>;

    /// Kill a window, but only if it carries the `@vowp=1` tag.
    fn kill_window(&self, session: &str, name: &str) -> Result<(), TmuxError>;

    /// Kill a whole session, but only when every window in it is tagged.
    ///
    /// A missing session is a no-op. `project archive`/`project rm` use this to remove
    /// the project's session explicitly instead of relying on tmux dropping a session
    /// after its last window dies.
    fn kill_session(&self, session: &str) -> Result<(), TmuxError>;

    /// List windows in creation/index order; a missing session yields an empty
    /// vector.
    fn list_windows(&self, session: &str) -> Result<Vec<WindowInfo>, TmuxError>;

    /// Whether a window with exactly this name exists.
    fn window_exists(&self, session: &str, name: &str) -> Result<bool, TmuxError>;

    /// Create the fixed windows `board supervisor planner keeper inbox` in
    /// that order, idempotently, in session `vowp-<project>`.
    fn ensure_layout(&self, project: &str, spec: &LayoutSpec) -> Result<(), TmuxError>;
}

/// Commands for the fixed windows; each is an argv run in that window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutSpec {
    /// Command for the `board` window.
    pub board: Vec<String>,
    /// Command for the `supervisor` window.
    pub supervisor: Vec<String>,
    /// Command for the `planner` window.
    pub planner: Vec<String>,
    /// Command for the `keeper` window.
    pub keeper: Vec<String>,
    /// Command for the `inbox` window.
    pub inbox: Vec<String>,
    /// Working directory shared by all layout windows.
    pub cwd: Option<PathBuf>,
}

/// The fixed window names created by [`WindowManager::ensure_layout`].
pub const FIXED_WINDOWS: [&str; 5] = ["board", "supervisor", "planner", "keeper", "inbox"];

/// tmux session name for a project: `vowp-<project>`.
///
/// The result is validated by the window manager operations; invalid project
/// characters therefore become [`TmuxError::ForeignSession`] there.
pub fn session_name_for_project(project: &str) -> String {
    format!("vowp-{project}")
}

/// tmux window name for a job: `job-<id>`.
///
/// Names longer than [`validate_name`](crate::quote::validate_name) allows are
/// rejected by the manager call.
pub fn window_name_for_job(job_id: &str) -> String {
    format!("job-{job_id}")
}

/// Check that a session is inside the `vowp-` namespace and is a safe name.
///
/// Rule 2 of the spec reports anything else as [`TmuxError::ForeignSession`].
pub(crate) fn require_vowp_session(session: &str) -> Result<(), TmuxError> {
    if !session.starts_with("vowp-") || validate_name("session", session).is_err() {
        return Err(TmuxError::ForeignSession(session.to_owned()));
    }
    Ok(())
}

/// Validate a window name with the shared naming rules.
pub(crate) fn require_window_name(name: &str) -> Result<(), TmuxError> {
    validate_name("window", name)
}
