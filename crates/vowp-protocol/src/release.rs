//! Releases and their guards.
//! Spec: `docs/design/types.md` §10 and `docs/design/crates/vowp-core.md` `promote`.
//!
//! A [`Release`] records a guarded promotion into one environment, from the guarded build to
//! the probe and optional rollback. It is refused, never silently skipped, when a guard fails.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ProjectName, ReleaseId};
use crate::time::Timestamp;

/// Lifecycle of a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// Created but guards have not run.
    Planned,
    /// Guards are running or passed.
    Guarded,
    /// The previous live state was banked for rollback.
    Banked,
    /// A human approved the deployment.
    Approved,
    /// The deployment command is running.
    Deploying,
    /// The probe is running.
    Probing,
    /// The release is live.
    Live,
    /// The release failed.
    Failed,
    /// The release was rolled back.
    RolledBack,
    /// The release was refused; nothing changed.
    Refused,
}

/// One guard evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GuardResult {
    /// Guard name, for example `clean_tree`.
    pub name: String,
    /// Whether it passed.
    pub passed: bool,
    /// Redacted detail for the operator.
    pub detail: String,
}

/// One promotion into an environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Release {
    /// Release id.
    pub id: ReleaseId,
    /// Project promoted.
    pub project: ProjectName,
    /// Target environment, for example `prod`.
    pub env: String,
    /// Human-readable change reference (commit or tag).
    pub change_ref: String,
    /// Commit deployed before this release.
    pub from_commit: Option<String>,
    /// Commit deployed by this release.
    pub to_commit: Option<String>,
    /// Current state.
    pub state: ReleaseState,
    /// Guard results.
    pub guards: Vec<GuardResult>,
    /// Path of the banked previous state.
    pub snapshot_path: Option<String>,
    /// Command that rolls this release back.
    pub rollback_cmd: Option<String>,
    /// Probe result as JSON.
    pub probe: serde_json::Value,
    /// Who approved the release.
    pub approved_by: Option<String>,
    /// Release that rolled this one back, when any.
    pub rolled_back_by: Option<ReleaseId>,
    /// Operator note.
    pub note: String,
    /// When the release was created.
    pub created_at: Timestamp,
    /// When it reached a terminal state.
    pub finished_at: Option<Timestamp>,
}
