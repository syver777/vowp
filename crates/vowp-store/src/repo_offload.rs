//! Repository methods for the v0.4 offload state: job placement, durable agent assignments
//! and per-node provider limits.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §7.1, §11.1 and §11.2.
//!
//! Assignment payloads are stored as JSON exactly as the protocol defines them. They carry
//! environment and secret *names* only: no method here stores a lease token, a secret value or
//! an absolute local path. Placement lives in a side table so the v0.3 `jobs` row and every
//! existing `Job`/`NewJob`/`JobPatch` literal stay source-compatible.

use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use vowp_protocol::{
    AssignmentPayload, JobId, LimitKind, NodeName, Placement, ProjectName, RunId, SessionId, Timestamp,
};

use crate::convert::{
    corrupt, enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text,
    ts_from_text, ts_to_text, u64_from_i64,
};
use crate::db::Store;
use crate::error::StoreError;

/// The durable state of one `agent_assignments` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAssignmentState {
    /// The home queued the assignment but has not sent it yet.
    Queued,
    /// The assignment was sent on a live node connection.
    Sent,
    /// The agent validated the assignment and accepted it.
    Accepted,
    /// The harness is running and events are flowing.
    Running,
    /// The home lost the connection; the run may resume within the grace period.
    Disconnected,
    /// The home wrote the terminal run row.
    Finished,
    /// The grace expired or a takeover fenced the run.
    Lost,
    /// The owner or the home cancelled the run.
    Cancelled,
    /// The agent refused the assignment before starting it.
    Rejected,
}

impl AgentAssignmentState {
    /// Returns the snake_case wire name stored in `agent_assignments.state`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Disconnected => "disconnected",
            Self::Finished => "finished",
            Self::Lost => "lost",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
        }
    }

    /// Returns whether the assignment still holds a run or a dispatch slot.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Sent | Self::Accepted | Self::Running | Self::Disconnected
        )
    }

    /// Returns whether no further state change is expected for this row.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Lost | Self::Cancelled | Self::Rejected)
    }

    /// Returns whether `self` may move to `next` in the assignment lifecycle.
    ///
    /// A state may repeat itself: the agent protocol is idempotent, so a duplicate
    /// `RunAccepted` or a replayed state report is accepted. The only backwards edge is
    /// `sent -> queued`, used when the home could not issue the run lease and returns the
    /// row to the dispatch queue without losing it.
    pub fn can_transition_to(&self, next: Self) -> bool {
        if *self == next {
            return true;
        }
        match self {
            Self::Queued => matches!(next, Self::Sent | Self::Lost | Self::Cancelled),
            Self::Sent => matches!(
                next,
                Self::Queued
                    | Self::Accepted
                    | Self::Running
                    | Self::Disconnected
                    | Self::Rejected
                    | Self::Lost
                    | Self::Cancelled
            ),
            Self::Accepted => matches!(
                next,
                Self::Running | Self::Disconnected | Self::Lost | Self::Cancelled | Self::Finished
            ),
            Self::Running => matches!(next, Self::Disconnected | Self::Lost | Self::Cancelled | Self::Finished),
            Self::Disconnected => matches!(next, Self::Running | Self::Lost | Self::Cancelled | Self::Finished),
            Self::Finished | Self::Lost | Self::Cancelled | Self::Rejected => false,
        }
    }
}

/// One durable assignment: the dispatch queue row and the reconnect record.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentAssignment {
    /// Durable assignment id (also carried in [`AssignmentPayload::assignment_id`]).
    pub id: String,
    /// Run the assignment dispatches.
    pub run_id: RunId,
    /// Session the run writes to.
    pub session_id: SessionId,
    /// Project the run works in.
    pub project: ProjectName,
    /// Executor node the assignment is sent to.
    pub node: NodeName,
    /// Assignment epoch; a higher epoch supersedes an older row for the same run.
    pub epoch: u64,
    /// Current durable state.
    pub state: AgentAssignmentState,
    /// Token-free protocol payload stored as JSON.
    pub payload: AssignmentPayload,
    /// When the row was created.
    pub created_at: Timestamp,
    /// When the node claimed the assignment, when it has.
    pub claimed_at: Option<Timestamp>,
    /// Deadline after which the home fences the run.
    pub expires_at: Option<Timestamp>,
    /// Redacted human-readable detail; never a secret value.
    pub detail: String,
}

/// One per-node provider limit.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeLimit {
    /// Provider key the limit applies to.
    pub provider: String,
    /// Executor node that sees the limit.
    pub node: NodeName,
    /// Limit class.
    pub kind: LimitKind,
    /// When the limit lifts; `None` means a human must resolve it.
    pub resets_at: Option<Timestamp>,
    /// Redacted human-readable detail.
    pub detail: String,
    /// When the limit was first observed on this node.
    pub since: Timestamp,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// One job's offload placement, read back from the project side table.
#[derive(Debug, Clone, PartialEq)]
pub struct JobPlacement {
    /// Job the placement belongs to.
    pub job: JobId,
    /// Parsed placement value.
    pub placement: Placement,
    /// When the placement last changed.
    pub updated_at: Timestamp,
}

/// Filter for [`Store::list_assignments`].
#[derive(Debug, Clone, Default)]
pub struct AssignmentFilter {
    /// Restrict to one node.
    pub node: Option<NodeName>,
    /// Restrict to one run.
    pub run_id: Option<RunId>,
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Restrict to these states; empty means every state.
    pub states: Vec<AgentAssignmentState>,
    /// Maximum number of rows.
    pub limit: Option<u32>,
}

/// Column list shared by every `agent_assignments` query.
const ASSIGNMENT_COLUMNS: &str = "id, run_id, session_id, project, node, epoch, state, payload_json, \
     created_at, claimed_at, expires_at, detail";

/// Raw TEXT/INTEGER values of one `agent_assignments` row before conversion.
struct AssignmentRow {
    id: String,
    run_id: String,
    session_id: String,
    project: String,
    node: String,
    epoch: i64,
    state: String,
    payload_json: String,
    created_at: String,
    claimed_at: Option<String>,
    expires_at: Option<String>,
    detail: String,
}

/// Reads the columns of an `agent_assignments` row.
fn read_assignment_row(row: &Row<'_>) -> rusqlite::Result<AssignmentRow> {
    Ok(AssignmentRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        session_id: row.get(2)?,
        project: row.get(3)?,
        node: row.get(4)?,
        epoch: row.get(5)?,
        state: row.get(6)?,
        payload_json: row.get(7)?,
        created_at: row.get(8)?,
        claimed_at: row.get(9)?,
        expires_at: row.get(10)?,
        detail: row.get(11)?,
    })
}

/// Converts a raw assignment row into an [`AgentAssignment`].
fn assignment_from_row(raw: AssignmentRow) -> Result<AgentAssignment, StoreError> {
    let payload: AssignmentPayload = json_from_text(&raw.payload_json, "agent_assignments.payload_json")?;
    let assignment = AgentAssignment {
        id: raw.id,
        run_id: RunId::parse(&raw.run_id)?,
        session_id: SessionId::parse(&raw.session_id)?,
        project: ProjectName::parse(&raw.project)?,
        node: NodeName::parse(&raw.node)?,
        epoch: u64_from_i64(raw.epoch, "agent_assignments.epoch")?,
        state: enum_from_text::<AgentAssignmentState>(&raw.state, "agent_assignments.state")?,
        payload,
        created_at: ts_from_text(&raw.created_at, "agent_assignments.created_at")?,
        claimed_at: opt_ts_from_text(raw.claimed_at.as_deref(), "agent_assignments.claimed_at")?,
        expires_at: opt_ts_from_text(raw.expires_at.as_deref(), "agent_assignments.expires_at")?,
        detail: raw.detail,
    };
    check_payload_matches(&assignment)?;
    Ok(assignment)
}

/// Verifies that the stored payload names the same assignment, run, session, project and epoch.
fn check_payload_matches(assignment: &AgentAssignment) -> Result<(), StoreError> {
    let payload = &assignment.payload;
    let field = if payload.assignment_id != assignment.id {
        Some("assignment_id")
    } else if payload.run_id != assignment.run_id {
        Some("run_id")
    } else if payload.session_id != assignment.session_id {
        Some("session_id")
    } else if payload.project != assignment.project {
        Some("project")
    } else if payload.epoch != assignment.epoch {
        Some("epoch")
    } else {
        None
    };
    match field {
        Some(field) => Err(StoreError::InvalidInput(format!(
            "assignment payload {field} does not match the assignment row"
        ))),
        None => Ok(()),
    }
}

/// Raw values of one `node_limits` row before conversion.
struct NodeLimitRow {
    provider: String,
    node: String,
    kind: String,
    resets_at: Option<String>,
    detail: String,
    since: String,
    updated_at: String,
}

/// Reads the columns of a `node_limits` row.
fn read_node_limit_row(row: &Row<'_>) -> rusqlite::Result<NodeLimitRow> {
    Ok(NodeLimitRow {
        provider: row.get(0)?,
        node: row.get(1)?,
        kind: row.get(2)?,
        resets_at: row.get(3)?,
        detail: row.get(4)?,
        since: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Converts a raw `node_limits` row into a [`NodeLimit`].
fn node_limit_from_row(raw: NodeLimitRow) -> Result<NodeLimit, StoreError> {
    Ok(NodeLimit {
        provider: raw.provider,
        node: NodeName::parse(&raw.node)?,
        kind: enum_from_text::<LimitKind>(&raw.kind, "node_limits.kind")?,
        resets_at: opt_ts_from_text(raw.resets_at.as_deref(), "node_limits.resets_at")?,
        detail: raw.detail,
        since: ts_from_text(&raw.since, "node_limits.since")?,
        updated_at: ts_from_text(&raw.updated_at, "node_limits.updated_at")?,
    })
}

impl Store {
    /// Inserts one durable assignment after checking that its payload describes the same row.
    ///
    /// A duplicate `(run_id, epoch)` is [`StoreError::Conflict`], as is a lower or equal epoch
    /// for the same run. A higher epoch is accepted only as a new, explicit row: this method
    /// never overwrites an existing assignment's payload or epoch.
    pub fn insert_assignment(&self, assignment: &AgentAssignment) -> Result<(), StoreError> {
        check_payload_matches(assignment)?;
        self.transaction(|tx| {
            let same_id: Option<i64> = tx
                .conn
                .query_row(
                    "SELECT epoch FROM main.agent_assignments WHERE id = ?1",
                    [assignment.id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if same_id.is_some() {
                return Err(StoreError::Conflict(format!(
                    "assignment {} already exists",
                    assignment.id
                )));
            }
            let newest: Option<i64> = tx
                .conn
                .query_row(
                    "SELECT MAX(epoch) FROM main.agent_assignments WHERE run_id = ?1",
                    [assignment.run_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if let Some(epoch) = newest {
                let epoch = u64_from_i64(epoch, "agent_assignments.epoch")?;
                if assignment.epoch <= epoch {
                    return Err(StoreError::Conflict(format!(
                        "run {} already has assignment epoch {epoch}; epoch {} does not supersede it",
                        assignment.run_id.as_str(),
                        assignment.epoch
                    )));
                }
            }
            tx.conn.execute(
                "INSERT INTO main.agent_assignments (
                    id, run_id, session_id, project, node, epoch, state, payload_json,
                    created_at, claimed_at, expires_at, detail
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    assignment.id,
                    assignment.run_id.as_str(),
                    assignment.session_id.as_str(),
                    assignment.project.as_str(),
                    assignment.node.as_str(),
                    i64::try_from(assignment.epoch)
                        .map_err(|_| StoreError::InvalidInput("assignment epoch is too large".to_owned()))?,
                    enum_to_text(&assignment.state)?,
                    json_to_text(&assignment.payload)?,
                    ts_to_text(assignment.created_at),
                    opt_ts_to_text(assignment.claimed_at),
                    opt_ts_to_text(assignment.expires_at),
                    assignment.detail,
                ],
            )?;
            Ok(())
        })
    }

    /// Returns the assignment called `id`, when it exists.
    pub fn assignment(&self, id: &str) -> Result<Option<AgentAssignment>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {ASSIGNMENT_COLUMNS} FROM main.agent_assignments WHERE id = ?1"),
                [id],
                read_assignment_row,
            )
            .optional()?;
        raw.map(assignment_from_row).transpose()
    }

    /// Lists assignments sent to `node`, oldest first.
    pub fn assignments_for_node(&self, node: &NodeName) -> Result<Vec<AgentAssignment>, StoreError> {
        self.query_assignments("WHERE node = ?1", vec![Box::new(node.as_str().to_owned())], None)
    }

    /// Lists assignments for `run`, lowest epoch first.
    pub fn assignments_for_run(&self, run: &RunId) -> Result<Vec<AgentAssignment>, StoreError> {
        self.query_assignments(
            "WHERE run_id = ?1 ORDER BY epoch, id",
            vec![Box::new(run.as_str().to_owned())],
            None,
        )
    }

    /// Lists assignments whose token-free payload names `job`, oldest first.
    ///
    /// The job id lives in `payload_json` by design: the durable queue does not duplicate
    /// job metadata in a column, so the dispatcher filters on the JSON field it stored.
    pub fn assignments_for_job(&self, job: &JobId) -> Result<Vec<AgentAssignment>, StoreError> {
        self.query_assignments(
            "WHERE json_extract(payload_json, '$.job') = ?1 ORDER BY created_at, id",
            vec![Box::new(job.as_str().to_owned())],
            None,
        )
    }

    /// Lists assignments matching `filter`.
    pub fn list_assignments(&self, filter: &AssignmentFilter) -> Result<Vec<AgentAssignment>, StoreError> {
        let mut query = String::from("WHERE 1 = 1");
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(node) = &filter.node {
            query.push_str(" AND node = ?");
            values.push(Box::new(node.as_str().to_owned()));
        }
        if let Some(run_id) = &filter.run_id {
            query.push_str(" AND run_id = ?");
            values.push(Box::new(run_id.as_str().to_owned()));
        }
        if let Some(project) = &filter.project {
            query.push_str(" AND project = ?");
            values.push(Box::new(project.as_str().to_owned()));
        }
        if !filter.states.is_empty() {
            query.push_str(" AND state IN (");
            for (index, state) in filter.states.iter().enumerate() {
                if index > 0 {
                    query.push(',');
                }
                query.push('?');
                values.push(Box::new(state.as_str().to_owned()));
            }
            query.push(')');
        }
        query.push_str(" ORDER BY created_at, id");
        self.query_assignments(&query, values, filter.limit)
    }

    /// Runs one assignment query with the shared column list and conversion.
    fn query_assignments(
        &self,
        clause: &str,
        values: Vec<Box<dyn ToSql>>,
        limit: Option<u32>,
    ) -> Result<Vec<AgentAssignment>, StoreError> {
        let mut query = format!("SELECT {ASSIGNMENT_COLUMNS} FROM main.agent_assignments {clause}");
        let mut values = values;
        if let Some(limit) = limit {
            query.push_str(" LIMIT ?");
            values.push(Box::new(i64::from(limit)));
        }
        let mut statement = self.conn.prepare(&query)?;
        let rows = statement.query_map(params_from_iter(values), read_assignment_row)?;
        let mut assignments = Vec::new();
        for row in rows {
            assignments.push(assignment_from_row(row?)?);
        }
        Ok(assignments)
    }

    /// Writes a new durable state, claim time and detail for one assignment.
    ///
    /// Returns the updated row. A missing id is [`StoreError::NotFound`]; the epoch and
    /// payload are never changed by this method. This unchecked write is kept for callers
    /// that already made the lifecycle decision; the dispatcher uses
    /// [`Store::advance_assignment`] instead.
    pub fn set_assignment_state(
        &self,
        id: &str,
        state: AgentAssignmentState,
        claimed_at: Option<Timestamp>,
        detail: &str,
    ) -> Result<AgentAssignment, StoreError> {
        self.transaction(|tx| {
            let mut assignment = tx
                .assignment(id)?
                .ok_or_else(|| StoreError::not_found("assignment", id))?;
            tx.conn.execute(
                "UPDATE main.agent_assignments SET state = ?1, claimed_at = ?2, detail = ?3 WHERE id = ?4",
                params![enum_to_text(&state)?, opt_ts_to_text(claimed_at), detail, id],
            )?;
            assignment.state = state;
            assignment.claimed_at = claimed_at;
            assignment.detail = detail.to_owned();
            Ok(assignment)
        })
    }

    /// Atomically claims up to `limit` queued assignments for `node`.
    ///
    /// Every selected row moves to `sent`, gets `claimed_at` and `expires_at`, and is
    /// returned after the update. The `state = 'queued'` predicate is repeated in the
    /// `UPDATE`, so a concurrent claim cannot send the same row twice.
    pub fn claim_assignments(
        &self,
        node: &NodeName,
        limit: u32,
        claimed_at: Timestamp,
        expires_at: Timestamp,
    ) -> Result<Vec<AgentAssignment>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.transaction(|tx| {
            let ids: Vec<String> = {
                let mut statement = tx.conn.prepare(
                    "SELECT id FROM main.agent_assignments
                     WHERE node = ?1 AND state = 'queued'
                     ORDER BY created_at, id LIMIT ?2",
                )?;
                let rows = statement.query_map(params![node.as_str(), i64::from(limit)], |row| row.get(0))?;
                let mut ids = Vec::new();
                for row in rows {
                    ids.push(row?);
                }
                ids
            };
            let mut claimed = Vec::with_capacity(ids.len());
            for id in ids {
                tx.conn.execute(
                    "UPDATE main.agent_assignments
                     SET state = ?1, claimed_at = ?2, expires_at = ?3
                     WHERE id = ?4 AND state = 'queued'",
                    params![
                        enum_to_text(&AgentAssignmentState::Sent)?,
                        ts_to_text(claimed_at),
                        ts_to_text(expires_at),
                        id,
                    ],
                )?;
                let assignment = tx
                    .assignment(&id)?
                    .ok_or_else(|| StoreError::not_found("assignment", &id))?;
                claimed.push(assignment);
            }
            Ok(claimed)
        })
    }

    /// Applies one checked state transition and returns the updated row.
    ///
    /// An illegal edge is [`StoreError::Conflict`]. `claimed_at`, when given, replaces the
    /// existing claim time; the epoch and payload never change.
    pub fn advance_assignment(
        &self,
        id: &str,
        state: AgentAssignmentState,
        claimed_at: Option<Timestamp>,
        detail: &str,
    ) -> Result<AgentAssignment, StoreError> {
        self.transaction(|tx| {
            let mut assignment = tx
                .assignment(id)?
                .ok_or_else(|| StoreError::not_found("assignment", id))?;
            if !assignment.state.can_transition_to(state) {
                return Err(StoreError::Conflict(format!(
                    "assignment {id} cannot move from {} to {}",
                    assignment.state.as_str(),
                    state.as_str()
                )));
            }
            tx.conn.execute(
                "UPDATE main.agent_assignments
                 SET state = ?1, claimed_at = COALESCE(?2, claimed_at), detail = ?3
                 WHERE id = ?4",
                params![enum_to_text(&state)?, opt_ts_to_text(claimed_at), detail, id,],
            )?;
            assignment.state = state;
            if claimed_at.is_some() {
                assignment.claimed_at = claimed_at;
            }
            assignment.detail = detail.to_owned();
            Ok(assignment)
        })
    }

    /// Renews the run lease timestamps of an active assignment without changing its state.
    ///
    /// A terminal row is [`StoreError::Conflict`]: a finished or lost run is never resumed.
    pub fn renew_assignment(
        &self,
        id: &str,
        claimed_at: Timestamp,
        expires_at: Timestamp,
        detail: &str,
    ) -> Result<AgentAssignment, StoreError> {
        self.transaction(|tx| {
            let mut assignment = tx
                .assignment(id)?
                .ok_or_else(|| StoreError::not_found("assignment", id))?;
            if !assignment.state.is_active() {
                return Err(StoreError::Conflict(format!(
                    "assignment {id} is {} and cannot be renewed",
                    assignment.state.as_str()
                )));
            }
            tx.conn.execute(
                "UPDATE main.agent_assignments SET claimed_at = ?1, expires_at = ?2, detail = ?3 WHERE id = ?4",
                params![ts_to_text(claimed_at), ts_to_text(expires_at), detail, id],
            )?;
            assignment.claimed_at = Some(claimed_at);
            assignment.expires_at = Some(expires_at);
            assignment.detail = detail.to_owned();
            Ok(assignment)
        })
    }

    /// Sets the row's expiry without changing its state.
    ///
    /// Used for a rejected assignment's retry cooldown and for tests; the dispatcher's
    /// normal lease path uses [`Store::claim_assignments`] and [`Store::renew_assignment`].
    pub fn set_assignment_expiry(
        &self,
        id: &str,
        expires_at: Timestamp,
        detail: &str,
    ) -> Result<AgentAssignment, StoreError> {
        self.transaction(|tx| {
            let mut assignment = tx
                .assignment(id)?
                .ok_or_else(|| StoreError::not_found("assignment", id))?;
            tx.conn.execute(
                "UPDATE main.agent_assignments SET expires_at = ?1, detail = ?2 WHERE id = ?3",
                params![ts_to_text(expires_at), detail, id],
            )?;
            assignment.expires_at = Some(expires_at);
            assignment.detail = detail.to_owned();
            Ok(assignment)
        })
    }

    /// Inserts or updates one per-node provider limit.
    ///
    /// Keyed by `(provider, node)`; `since` is preserved while the limit kind stays the same
    /// and reset when the kind changes.
    pub fn upsert_node_limit(&self, limit: &NodeLimit) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.node_limits (provider, node, kind, resets_at, detail, since, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(provider, node) DO UPDATE SET
               kind = excluded.kind,
               resets_at = excluded.resets_at,
               detail = excluded.detail,
               since = CASE WHEN node_limits.kind = excluded.kind THEN node_limits.since ELSE excluded.since END,
               updated_at = excluded.updated_at",
            params![
                limit.provider,
                limit.node.as_str(),
                enum_to_text(&limit.kind)?,
                opt_ts_to_text(limit.resets_at),
                limit.detail,
                ts_to_text(limit.since),
                ts_to_text(limit.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Lists every limit recorded for `node`, ordered by provider.
    pub fn node_limits(&self, node: &NodeName) -> Result<Vec<NodeLimit>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT provider, node, kind, resets_at, detail, since, updated_at
             FROM main.node_limits WHERE node = ?1 ORDER BY provider",
        )?;
        let rows = statement.query_map([node.as_str()], read_node_limit_row)?;
        let mut limits = Vec::new();
        for row in rows {
            limits.push(node_limit_from_row(row?)?);
        }
        Ok(limits)
    }

    /// Removes one provider limit from one node; returns whether a row existed.
    pub fn clear_node_limit(&self, provider: &str, node: &NodeName) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.node_limits WHERE provider = ?1 AND node = ?2",
            params![provider, node.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Writes the placement of one job in the bound project.
    ///
    /// The value is stored in its canonical wire form (`home`, `any`, `node:<name>` or
    /// `node:<name>+home`). A missing job is [`StoreError::NotFound`].
    pub fn set_job_placement(&self, job: &JobId, placement: Placement, at: Timestamp) -> Result<(), StoreError> {
        if !self.job_exists(job)? {
            return Err(StoreError::not_found("job", job.as_str()));
        }
        self.conn.execute(
            "INSERT INTO project.job_placement (job, placement, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(job) DO UPDATE SET
               placement = excluded.placement,
               updated_at = excluded.updated_at",
            params![job.as_str(), placement.to_string(), ts_to_text(at)],
        )?;
        Ok(())
    }

    /// Returns the placement of one job in the bound project, when it has one.
    pub fn job_placement(&self, job: &JobId) -> Result<Option<Placement>, StoreError> {
        self.require_bound()?;
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT placement FROM project.job_placement WHERE job = ?1",
                [job.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .map(|text| Placement::parse(&text).map_err(|err| corrupt("project.job_placement.placement", err)))
            .transpose()
    }

    /// Lists every placement in the bound project, ordered by job id.
    pub fn list_job_placements(&self) -> Result<Vec<JobPlacement>, StoreError> {
        self.require_bound()?;
        let mut statement = self
            .conn
            .prepare("SELECT job, placement, updated_at FROM project.job_placement ORDER BY job")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut placements = Vec::new();
        for row in rows {
            let (job, placement, updated_at) = row?;
            placements.push(JobPlacement {
                job: JobId::parse(&job)?,
                placement: Placement::parse(&placement)
                    .map_err(|err| corrupt("project.job_placement.placement", err))?,
                updated_at: ts_from_text(&updated_at, "project.job_placement.updated_at")?,
            });
        }
        Ok(placements)
    }

    /// Removes one job's placement from the bound project; returns whether a row existed.
    pub fn clear_job_placement(&self, job: &JobId) -> Result<bool, StoreError> {
        self.require_bound()?;
        let changed = self
            .conn
            .execute("DELETE FROM project.job_placement WHERE job = ?1", [job.as_str()])?;
        Ok(changed > 0)
    }
}
