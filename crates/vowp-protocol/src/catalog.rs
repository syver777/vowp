//! Lifecycle catalog types: the file classes used by `vowp gc` and `vowp backup`.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §10 and §13.1. The shapes are fixed here;
//! classification, garbage collection and backup build on them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{JobId, ProjectName, SessionId};
use crate::time::Timestamp;

/// One lifecycle class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CatalogClass {
    /// Releases, rollback snapshots and serving banks.
    Production,
    /// Databases, capsules, passing evidence and decisions.
    Truth,
    /// Open worktrees, active sessions and orchestrators.
    OpenWork,
    /// Secret files and the vault mode marker.
    Secrets,
    /// A closed session's full log and blobs.
    ClosedSession,
    /// Raw run output.
    RunOutput,
    /// Merged, failed or cancelled worktrees.
    Worktree,
    /// Host samples.
    Sample,
    /// Disposable caches.
    Cache,
}

/// One catalog row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CatalogEntry {
    /// Catalog row id (ULID).
    pub id: String,
    /// Absolute path or database row identity.
    pub path: String,
    /// Lifecycle class.
    pub class: CatalogClass,
    /// Project the file belongs to, when bound.
    pub project: Option<ProjectName>,
    /// `work`, `prod` or `none`.
    pub environment: String,
    /// Job the file belongs to, when bound.
    pub job_id: Option<JobId>,
    /// Session the file belongs to, when bound.
    pub session_id: Option<SessionId>,
    /// Size in bytes, when known.
    pub bytes: Option<u64>,
    /// When the row expires, for classes that expire.
    pub expires_at: Option<Timestamp>,
    /// Free-form note.
    pub note: String,
    /// When the row was created.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// When the file was deleted, when it was.
    pub deleted_at: Option<Timestamp>,
}

/// Totals for one class in a GC report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcClassReport {
    /// Class the totals describe.
    pub class: CatalogClass,
    /// Number of files.
    pub files: u64,
    /// Total bytes.
    pub bytes: u64,
    /// Up to five example paths.
    pub sample_paths: Vec<String>,
}

/// Output of `vowp gc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GcReport {
    /// When the report was generated.
    pub generated_at: Timestamp,
    /// Whether the run was a dry run.
    pub dry_run: bool,
    /// Totals per class.
    pub classes: Vec<GcClassReport>,
    /// Number of files deleted (zero on a dry run).
    pub deleted_files: u64,
    /// Bytes freed (zero on a dry run).
    pub freed_bytes: u64,
    /// Number of closed sessions compacted.
    pub compacted_sessions: u64,
    /// Errors encountered while applying.
    pub errors: Vec<String>,
}

/// One file included in a backup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackupFile {
    /// Path relative to the backup directory.
    pub path: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Kind of file (`global-db`, `project-db`, `session-log`, ...).
    pub kind: String,
}

/// Output of `vowp backup`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BackupReport {
    /// When the backup ran.
    pub at: Timestamp,
    /// Backup directory.
    pub directory: String,
    /// Files written.
    pub files: Vec<BackupFile>,
    /// Total bytes.
    pub bytes: u64,
    /// Offsite target, when one was used.
    pub offsite: Option<String>,
    /// Whether the backup was encrypted.
    pub encrypted: bool,
    /// Paths deliberately skipped, with the reason.
    pub skipped: Vec<String>,
}

/// Filter accepted by catalog queries.
#[derive(Debug, Clone, Default)]
pub struct CatalogFilter {
    /// Restrict to one class.
    pub class: Option<CatalogClass>,
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Include only rows expiring before this instant.
    pub expired_before: Option<Timestamp>,
    /// Maximum rows.
    pub limit: Option<u32>,
}
