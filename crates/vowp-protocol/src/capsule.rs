//! Handoff capsules: a validated, model-written summary of a session at a sequence point,
//! plus the markdown seed rendered for the next harness.
//! Spec: `docs/design/types.md` §6 and `docs/design/sessions.md` §3–4.
//!
//! [`Capsule::validate`] enforces the structural rules the writer relies on;
//! [`Capsule::render_markdown`] is the human/model-readable handoff text. Empty sections are
//! omitted so a short capsule stays short.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::event::CapsuleTrigger;
use crate::executor::ExecutorRef;
use crate::ids::{CapsuleId, SessionId};
use crate::time::Timestamp;

/// Capsule format version written by v0.1.
pub const CAPSULE_VERSION: u32 = 1;

/// A handoff capsule stored at `sessions/<id>/capsules/<at_seq:010>-<ulid>.json`.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct Capsule {
    /// Capsule format version; must equal [`CAPSULE_VERSION`].
    pub version: u32,
    /// Capsule id (`cap_` plus a ULID).
    pub capsule_id: CapsuleId,
    /// Session the capsule summarises.
    pub session_id: SessionId,
    /// When the capsule was written.
    pub created_at: Timestamp,
    /// Last session event sequence covered by the capsule; at least 1.
    pub at_seq: u64,
    /// What caused the capsule.
    pub trigger: CapsuleTrigger,
    /// Executor that was running when the capsule was written.
    pub from: ExecutorRef,
    /// One-paragraph statement of what the session is trying to achieve.
    pub intent: String,
    /// Constraints the next agent must respect.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Decisions made so far.
    #[serde(default)]
    pub decisions: Vec<CapsuleDecision>,
    /// What is done, in progress, blocked and next.
    pub state: CapsuleState,
    /// Files the session read or changed.
    #[serde(default)]
    pub files: Vec<CapsuleFile>,
    /// Traps the next agent must know about.
    #[serde(default)]
    pub gotchas: Vec<String>,
    /// Unresolved questions.
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// Session telemetry at capsule time.
    pub telemetry: CapsuleTelemetry,
}

/// One decision recorded in a capsule.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleDecision {
    /// What was decided.
    pub what: String,
    /// Why it was decided.
    pub why: String,
    /// Sequence number of the event that records the decision, when known.
    pub at_seq: Option<u64>,
}

/// The four state lists of a capsule.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Default)]
pub struct CapsuleState {
    /// Finished work.
    #[serde(default)]
    pub done: Vec<String>,
    /// Work currently in progress.
    #[serde(default)]
    pub in_progress: Vec<String>,
    /// Work that cannot proceed.
    #[serde(default)]
    pub blocked: Vec<String>,
    /// Ordered next steps.
    #[serde(default)]
    pub next_steps: Vec<String>,
}

/// How a capsule file entry relates to the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    /// Read but not changed.
    Read,
    /// Created by the session.
    Created,
    /// Modified by the session.
    Modified,
    /// Deleted by the session.
    Deleted,
    /// Mentioned but not touched.
    Mentioned,
}

/// One file mentioned in a capsule.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleFile {
    /// Path relative to the session working directory.
    pub path: String,
    /// How the file relates to the work.
    pub role: FileRole,
    /// Short note about what changed or why the file matters.
    #[serde(default)]
    pub note: String,
}

/// Session telemetry captured with a capsule.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Default)]
pub struct CapsuleTelemetry {
    /// Number of completed turns.
    pub turns: u32,
    /// Input tokens across the session.
    pub tokens_in: u64,
    /// Output tokens across the session.
    pub tokens_out: u64,
    /// Total cost in USD, when known.
    #[serde(default)]
    pub usd: Option<f64>,
}

/// What the capsule-writing model must return; VOWP fills ids, timestamps and telemetry.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct CapsuleDraft {
    /// One-paragraph statement of what the session is trying to achieve.
    pub intent: String,
    /// Constraints the next agent must respect.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Decisions made so far.
    #[serde(default)]
    pub decisions: Vec<CapsuleDecision>,
    /// What is done, in progress, blocked and next.
    pub state: CapsuleState,
    /// Files the session read or changed.
    #[serde(default)]
    pub files: Vec<CapsuleFile>,
    /// Traps the next agent must know about.
    #[serde(default)]
    pub gotchas: Vec<String>,
    /// Unresolved questions.
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl Capsule {
    /// Checks the structural rules: non-empty intent, version 1 and `at_seq >= 1`.
    ///
    /// Every constraint, decision (`what` and `why`), state entry, file path, gotcha and
    /// open question must also be non-empty; the error names the offending field.
    ///
    /// The protocol error vocabulary has no dedicated capsule variant, so failures are
    /// reported as [`ProtocolError::Config`] with the offending field named.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_non_empty("intent", &self.intent)?;
        if self.version != CAPSULE_VERSION {
            return Err(ProtocolError::Config(format!(
                "capsule version must be {CAPSULE_VERSION}, got {}",
                self.version
            )));
        }
        if self.at_seq == 0 {
            return Err(ProtocolError::Config("capsule at_seq must be at least 1".to_owned()));
        }
        require_items("constraints", &self.constraints)?;
        for (index, decision) in self.decisions.iter().enumerate() {
            require_non_empty(&format!("decisions[{index}].what"), &decision.what)?;
            require_non_empty(&format!("decisions[{index}].why"), &decision.why)?;
        }
        require_items("state.done", &self.state.done)?;
        require_items("state.in_progress", &self.state.in_progress)?;
        require_items("state.blocked", &self.state.blocked)?;
        require_items("state.next_steps", &self.state.next_steps)?;
        for (index, file) in self.files.iter().enumerate() {
            require_non_empty(&format!("files[{index}].path"), &file.path)?;
        }
        require_items("gotchas", &self.gotchas)?;
        require_items("open_questions", &self.open_questions)?;
        Ok(())
    }

    /// Renders the handoff markdown used by `seed_prompt`.
    ///
    /// Sections are `## Intent`, `## Constraints`, `## Decisions`, `## State` (with
    /// `### Done`, `### In progress`, `### Blocked`, `### Next`), `## Files`, `## Gotchas`
    /// and `## Open questions`; empty sections and state subsections are omitted.
    pub fn render_markdown(&self) -> String {
        let mut sections: Vec<String> = Vec::new();

        let intent = self.intent.trim();
        if !intent.is_empty() {
            sections.push(format!("## Intent\n\n{intent}"));
        }

        push_list_section(&mut sections, "Constraints", &self.constraints);

        if !self.decisions.is_empty() {
            let body = self
                .decisions
                .iter()
                .map(|decision| {
                    let mut line = format!("- {} — {}", decision.what.trim(), decision.why.trim());
                    if let Some(seq) = decision.at_seq {
                        line.push_str(&format!(" (at seq {seq})"));
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("## Decisions\n\n{body}"));
        }

        if let Some(state) = self.render_state() {
            sections.push(state);
        }

        if !self.files.is_empty() {
            let body = self
                .files
                .iter()
                .map(|file| {
                    let role = file_role_name(file.role);
                    if file.note.trim().is_empty() {
                        format!("- `{}` ({role})", file.path.trim())
                    } else {
                        format!("- `{}` ({role}): {}", file.path.trim(), file.note.trim())
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("## Files\n\n{body}"));
        }

        push_list_section(&mut sections, "Gotchas", &self.gotchas);
        push_list_section(&mut sections, "Open questions", &self.open_questions);

        let mut out = sections.join("\n\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// Renders `## State` with only the non-empty subsections.
    fn render_state(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        let groups = [
            ("Done", &self.state.done),
            ("In progress", &self.state.in_progress),
            ("Blocked", &self.state.blocked),
            ("Next", &self.state.next_steps),
        ];
        for (title, items) in groups {
            if items.is_empty() {
                continue;
            }
            let body = items
                .iter()
                .map(|item| format!("- {}", item.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!("### {title}\n\n{body}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("## State\n\n{}", parts.join("\n\n")))
        }
    }
}

/// Rejects a trimmed-empty string field.
fn require_non_empty(field: &str, value: &str) -> Result<(), ProtocolError> {
    if value.trim().is_empty() {
        Err(ProtocolError::Config(format!("capsule {field} must not be empty")))
    } else {
        Ok(())
    }
}

/// Rejects a list that contains a trimmed-empty item.
fn require_items(field: &str, items: &[String]) -> Result<(), ProtocolError> {
    for (index, item) in items.iter().enumerate() {
        if item.trim().is_empty() {
            return Err(ProtocolError::Config(format!(
                "capsule {field}[{index}] must not be empty"
            )));
        }
    }
    Ok(())
}

/// Appends `## {title}` with one bullet per item, unless the list is empty.
fn push_list_section(sections: &mut Vec<String>, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let body = items
        .iter()
        .map(|item| format!("- {}", item.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    sections.push(format!("## {title}\n\n{body}"));
}

/// Returns the wire name of a [`FileRole`] for markdown rendering.
fn file_role_name(role: FileRole) -> &'static str {
    match role {
        FileRole::Read => "read",
        FileRole::Created => "created",
        FileRole::Modified => "modified",
        FileRole::Deleted => "deleted",
        FileRole::Mentioned => "mentioned",
    }
}
