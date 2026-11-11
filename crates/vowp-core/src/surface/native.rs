//! The app-host trait and its shared types. The opencode and Claude Code hosts implement
//! it; tests use a fake.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5. One [`AppHost`] runs one app
//! (opencode or Claude Code) for an orchestrator and exposes its transcript as
//! [`TranscriptEntry`] values. The hosts never expose a public port; the desktop and the
//! phone join over SSH.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vowp_protocol::{OrchestratorApp, SessionId, Timestamp};

use crate::CoreError;

use super::mirror::MirrorCursor;

/// One request to start an app host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRequest {
    /// Orchestrator name.
    pub name: String,
    /// Session the app serves.
    pub session: SessionId,
    /// Port to bind, when the caller picked one.
    pub port: Option<u16>,
    /// Working directory for the app.
    pub workdir: PathBuf,
    /// Project-local app config file.
    pub config: PathBuf,
}

/// A running app host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppHandle {
    /// App kind.
    pub kind: OrchestratorApp,
    /// Process id.
    pub pid: u32,
    /// Port the app listens on.
    pub port: Option<u16>,
    /// When the host started.
    pub started_at: Timestamp,
    /// Working directory.
    pub workdir: PathBuf,
}

/// Health of one app host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHealth {
    /// Whether the app answered.
    pub ok: bool,
    /// Human-readable detail.
    pub detail: String,
}

/// Kind of one transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    /// A user message.
    UserMessage,
    /// An assistant message.
    AssistantMessage,
    /// Assistant reasoning, kept for display only.
    Reasoning,
    /// A tool call.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A turn started.
    TurnStarted,
    /// A turn ended; [`TranscriptEntry::text`] carries the app's outcome word.
    TurnEnded,
    /// A cost report.
    Cost,
    /// An error reported by the app.
    Error,
    /// A free-form note.
    Note,
}

/// One transcript entry read from an app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    /// App-side id used for idempotency.
    pub native_id: String,
    /// Kind of entry.
    pub kind: TranscriptKind,
    /// Timestamp.
    pub ts: Timestamp,
    /// Message text, when any.
    #[serde(default)]
    pub text: Option<String>,
    /// Tool name, when any.
    #[serde(default)]
    pub tool: Option<String>,
    /// Tool input, when any.
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// Tool output, when any.
    #[serde(default)]
    pub output: Option<String>,
    /// File path, when the entry names one.
    #[serde(default)]
    pub path: Option<String>,
    /// App-side tool call id, when the entry belongs to a tool call or result.
    #[serde(default)]
    pub call_id: Option<String>,
    /// Model the app reported for this entry, when any.
    #[serde(default)]
    pub model: Option<String>,
    /// App-side session id, when the entry carries one.
    #[serde(default)]
    pub native_session: Option<String>,
    /// Whether a tool result succeeded, when the app said so.
    #[serde(default)]
    pub ok: Option<bool>,
    /// Token usage, when the entry reports a cost.
    #[serde(default)]
    pub tokens: Option<vowp_protocol::Tokens>,
    /// USD amount, when the app reported a receipt.
    #[serde(default)]
    pub usd: Option<f64>,
}

/// One batch of transcript entries plus the cursor after the batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptBatch {
    /// Entries read since the cursor.
    pub entries: Vec<TranscriptEntry>,
    /// Cursor after the batch.
    pub cursor: MirrorCursor,
}

/// One host implementation (opencode, Claude or a fake).
pub trait AppHost: Send + Sync {
    /// Returns the app kind.
    fn kind(&self) -> OrchestratorApp;
    /// Starts the app.
    fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError>;
    /// Stops the app.
    fn stop(&self, handle: &AppHandle) -> Result<(), CoreError>;
    /// Checks whether the app is healthy.
    fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError>;
    /// Returns the join instructions for the three surfaces.
    fn join_instructions(&self, handle: &AppHandle) -> Result<vowp_protocol::JoinInstructions, CoreError>;
    /// Reads the transcript entries the mirror should consider after `cursor`.
    ///
    /// A host may replay a spool and rely on the mirror's `native_events` map for
    /// idempotency; the returned batch cursor is the last entry it read.
    fn transcript_since(&self, handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError>;
}

/// Returns true when `id` is safe to place in one URL path segment.
pub(crate) fn safe_segment(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Writes JSON with mode 0600 under an existing private directory.
pub(crate) fn write_private_json(path: &std::path::Path, value: &impl Serialize) -> Result<(), CoreError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let bytes = serde_json::to_vec(value).map_err(|error| CoreError::Invalid(error.to_string()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
    file.write_all(&bytes).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.sync_all().map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    // `mode` only applies when the file is created; narrow an older file too.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Removes a private state file, ignoring a missing file.
pub(crate) fn remove_private(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Returns whether `pid` names a live process.
pub(crate) fn process_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Stops one pid: SIGTERM, a three-second grace, then SIGKILL.
pub(crate) fn stop_pid(pid: u32) {
    use std::time::{Duration, Instant};

    if !process_alive(pid) {
        return;
    }
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Quotes one shell argument string with single quotes.
pub(crate) fn shell_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
