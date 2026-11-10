//! Human inbox items: questions, approvals, decisions and notices.
//! Spec: `docs/design/types.md` §9.
//!
//! Items are created by loops or workers and answered with `vowp inbox answer`; answers are
//! never stored as free-form secrets.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{InboxId, JobId, ProjectName};
use crate::time::Timestamp;

/// Why an inbox item exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    /// A question that needs an answer.
    Question,
    /// An approval before a guarded action.
    Approval,
    /// A decision between options.
    Decision,
    /// A one-way notice.
    Notice,
}

/// Lifecycle of an inbox item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    /// Waiting for an answer.
    Open,
    /// Answered.
    Answered,
    /// Dismissed without an answer.
    Dismissed,
}

/// One row of the inbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboxItem {
    /// Item id.
    pub id: InboxId,
    /// Project the item belongs to, when any.
    pub project: Option<ProjectName>,
    /// Job that raised it, when any.
    pub job_id: Option<JobId>,
    /// Item kind.
    pub kind: InboxKind,
    /// Short title shown on the board.
    pub title: String,
    /// Full question or notice text.
    pub body: String,
    /// Suggested answers.
    pub options: Vec<String>,
    /// Current state.
    pub state: InboxState,
    /// Answer text, once answered.
    pub answer: Option<String>,
    /// Who answered.
    pub answered_by: Option<String>,
    /// When the item was created.
    pub created_at: Timestamp,
    /// When the item was answered or dismissed.
    pub answered_at: Option<Timestamp>,
}
