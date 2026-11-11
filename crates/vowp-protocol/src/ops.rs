//! Control-plane DTOs shared by the CLI, MCP and core.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §5.5, §8.3, §12.3 and §13.1.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::executor::Role;
use crate::ids::{JobId, ProjectName};
use crate::job::{JobKind, JobState};
use crate::snapshot::Snapshot;
use crate::time::Timestamp;

/// Output of `vowp fleet status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FleetStatus {
    /// The snapshot document.
    pub snapshot: Snapshot,
    /// Where the answer came from: `local`, `remote` or `cache`.
    pub source: String,
    /// When the snapshot was fetched or computed.
    pub fetched_at: Timestamp,
    /// Whether the answer is older than three ticks.
    pub stale: bool,
}

/// One row of `vowp fleet queue --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FleetQueueRow {
    /// Project the job belongs to.
    pub project: ProjectName,
    /// Job id.
    pub job: JobId,
    /// Job title.
    pub title: String,
    /// Job kind.
    pub kind: JobKind,
    /// Job role.
    pub role: Role,
    /// Dispatch priority.
    pub priority: i32,
    /// Job state.
    pub state: JobState,
    /// Age in seconds.
    pub age_secs: u64,
    /// Last admission or brake reason.
    pub waiting_reason: Option<String>,
}

/// One SSH tunnel opened by a control machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tunnel {
    /// Tunnel name.
    pub name: String,
    /// Local loopback port.
    pub local_port: u16,
    /// Remote loopback port on the home.
    pub remote_port: u16,
    /// Remote host the tunnel forwards to (`127.0.0.1` by default).
    pub host: String,
    /// SSH process id.
    pub pid: u32,
    /// When the tunnel was opened.
    pub started_at: Timestamp,
}

/// One before/after row count in a migration report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MigrationCount {
    /// Row count before the migration.
    pub before: u64,
    /// Row count after the migration.
    pub after: u64,
}

/// Output of `vowp migrate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MigrationReport {
    /// Schema version before the migration.
    pub from_version: i64,
    /// Schema version after the migration.
    pub to_version: i64,
    /// Pre-migration backup path.
    pub backup: String,
    /// Projects migrated.
    pub projects: Vec<ProjectName>,
    /// Row counts by table.
    pub tables: BTreeMap<String, MigrationCount>,
    /// Whether the row-count checks passed.
    pub verified: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
}
