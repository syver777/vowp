//! Repository methods for the `jobs` and `edges` tables.
//! Spec: `docs/design/crates/vowp-store.md` §3.4 and `docs/design/types.md` §7.
//!
//! The store owns persistence, not job-state legality: [`Store::set_job_state`] writes any
//! state the caller passes, and `vowp-graph::transition` remains the only authority on legal
//! transitions. Job ids are slugs; their `needs` list is always read back from `edges`.

use std::collections::BTreeMap;

use rusqlite::types::ToSql;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use vowp_protocol::{
    Edge, EdgeKind, Job, JobId, JobKind, JobPatch, JobState, JobState as State, NewJob, ParkedReason, ProjectName,
    Role, Timestamp, default_gates,
};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, ts_from_text, ts_to_text,
    u32_from_i64,
};
use crate::db::{JobFilter, StateChange, Store};
use crate::error::StoreError;

/// Column list shared by every `jobs` query.
const JOB_COLUMNS: &str = "id, project, title, kind, role, profile, state, priority, owns_json, reads_json, \
     brief_path, report_path, gates_json, loop_json, branch, worktree, attempts, continues, hang_restarts, \
     parked_reason, note, created_by, last_progress_at, state_changed_at, created_at, updated_at";

/// Raw TEXT/INTEGER values of one `jobs` row before protocol conversion.
struct JobRow {
    id: String,
    project: String,
    title: String,
    kind: String,
    role: String,
    profile: String,
    state: String,
    priority: i64,
    owns: String,
    reads: String,
    brief: String,
    report: String,
    gates: String,
    loop_json: String,
    branch: Option<String>,
    worktree: Option<String>,
    attempts: i64,
    continues: i64,
    hang_restarts: i64,
    parked_reason: Option<String>,
    note: String,
    created_by: String,
    last_progress_at: Option<String>,
    state_changed_at: String,
    created_at: String,
    updated_at: String,
}

/// Reads the textual columns of a `jobs` row.
fn read_job_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: row.get(0)?,
        project: row.get(1)?,
        title: row.get(2)?,
        kind: row.get(3)?,
        role: row.get(4)?,
        profile: row.get(5)?,
        state: row.get(6)?,
        priority: row.get(7)?,
        owns: row.get(8)?,
        reads: row.get(9)?,
        brief: row.get(10)?,
        report: row.get(11)?,
        gates: row.get(12)?,
        loop_json: row.get(13)?,
        branch: row.get(14)?,
        worktree: row.get(15)?,
        attempts: row.get(16)?,
        continues: row.get(17)?,
        hang_restarts: row.get(18)?,
        parked_reason: row.get(19)?,
        note: row.get(20)?,
        created_by: row.get(21)?,
        last_progress_at: row.get(22)?,
        state_changed_at: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
    })
}

/// Converts a raw `jobs` row into a protocol [`Job`]; `needs` is filled by the caller.
fn job_from_row(raw: JobRow) -> Result<Job, StoreError> {
    let parked_reason = match raw.parked_reason {
        Some(reason) => Some(enum_from_text::<ParkedReason>(&reason, "jobs.parked_reason")?),
        None => None,
    };
    Ok(Job {
        id: JobId::parse(&raw.id)?,
        project: ProjectName::parse(&raw.project)?,
        title: raw.title,
        kind: enum_from_text::<JobKind>(&raw.kind, "jobs.kind")?,
        role: enum_from_text::<Role>(&raw.role, "jobs.role")?,
        profile: raw.profile,
        state: enum_from_text::<JobState>(&raw.state, "jobs.state")?,
        priority: i32::try_from(raw.priority).map_err(|err| crate::convert::corrupt("jobs.priority", err))?,
        needs: Vec::new(),
        owns: json_from_text(&raw.owns, "jobs.owns_json")?,
        reads: json_from_text(&raw.reads, "jobs.reads_json")?,
        brief: raw.brief,
        report: raw.report,
        gates: json_from_text(&raw.gates, "jobs.gates_json")?,
        loop_: json_from_text(&raw.loop_json, "jobs.loop_json")?,
        branch: raw.branch,
        worktree: raw.worktree,
        attempts: u32_from_i64(raw.attempts, "jobs.attempts")?,
        continues: u32_from_i64(raw.continues, "jobs.continues")?,
        hang_restarts: u32_from_i64(raw.hang_restarts, "jobs.hang_restarts")?,
        parked_reason,
        note: raw.note,
        created_by: raw.created_by,
        last_progress_at: opt_ts_from_text(raw.last_progress_at.as_deref(), "jobs.last_progress_at")?,
        state_changed_at: ts_from_text(&raw.state_changed_at, "jobs.state_changed_at")?,
        created_at: ts_from_text(&raw.created_at, "jobs.created_at")?,
        updated_at: ts_from_text(&raw.updated_at, "jobs.updated_at")?,
    })
}

/// Reads an `edges` row as its three wire columns.
fn read_edge_row(row: &Row<'_>) -> rusqlite::Result<(String, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

/// Converts the three wire columns of an `edges` row.
fn edge_from_parts(src: String, dst: String, kind: String) -> Result<Edge, StoreError> {
    Ok(Edge {
        src: JobId::parse(&src)?,
        dst: JobId::parse(&dst)?,
        kind: enum_from_text::<EdgeKind>(&kind, "edges.kind")?,
    })
}

impl Store {
    /// Inserts a job with its defaults and one `needs` edge per `new.needs`, in one transaction.
    ///
    /// Errors: [`StoreError::Conflict`] for a duplicate id, [`StoreError::NotFound`] for an
    /// unknown project or need.
    pub fn insert_job(&self, project: &ProjectName, new: &NewJob, created_by: &str) -> Result<Job, StoreError> {
        self.transaction(|tx| {
            if tx.project(project)?.is_none() {
                return Err(StoreError::not_found("project", project.as_str()));
            }
            if tx.job_exists(&new.id)? {
                return Err(StoreError::Conflict(format!("job {} already exists", new.id)));
            }
            for need in &new.needs {
                if !tx.job_exists(need)? {
                    return Err(StoreError::not_found("job (need)", need.as_str()));
                }
            }

            let now = tx.clock().now();
            let now_text = ts_to_text(now);
            let role_text = enum_to_text(&new.role)?;
            let profile = new.profile.clone().unwrap_or_else(|| role_text.clone());
            let gates = new.gates.clone().unwrap_or_else(|| default_gates(new.kind));
            let loop_json = new.loop_.clone().unwrap_or_default();
            let brief = format!(".vowp/jobs/{}/brief.md", new.id.as_str());
            let report = format!(".vowp/jobs/{}/report.md", new.id.as_str());

            tx.conn.execute(
                "INSERT INTO jobs (
                    id, project, title, kind, role, profile, state, priority, owns_json, reads_json,
                    brief_path, report_path, gates_json, loop_json, branch, worktree, attempts, continues,
                    hang_restarts, parked_reason, note, created_by, last_progress_at, state_changed_at,
                    created_at, updated_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    NULL, NULL, 0, 0, 0, NULL, '', ?14, NULL, ?15, ?15, ?15
                 )",
                params![
                    new.id.as_str(),
                    project.as_str(),
                    new.title,
                    enum_to_text(&new.kind)?,
                    role_text,
                    profile,
                    new.priority.unwrap_or(50),
                    json_to_text(&new.owns)?,
                    json_to_text(&new.reads)?,
                    brief,
                    report,
                    json_to_text(&gates)?,
                    json_to_text(&loop_json)?,
                    created_by,
                    now_text,
                ],
            )?;

            for need in &new.needs {
                tx.conn.execute(
                    "INSERT OR IGNORE INTO edges (src, dst, kind, created_at) VALUES (?1, ?2, 'needs', ?3)",
                    params![new.id.as_str(), need.as_str(), now_text],
                )?;
            }

            tx.require_job(&new.id)
        })
    }

    /// Returns the job called `id`, with `needs` filled from the edge rows.
    pub fn job(&self, id: &JobId) -> Result<Option<Job>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id.as_str()],
                read_job_row,
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => {
                let mut job = job_from_row(raw)?;
                job.needs = self.needs_of(id)?;
                Ok(Some(job))
            }
        }
    }

    /// Returns the job called `id` or [`StoreError::NotFound`].
    pub fn require_job(&self, id: &JobId) -> Result<Job, StoreError> {
        self.job(id)?.ok_or_else(|| StoreError::not_found("job", id.as_str()))
    }

    /// Lists jobs matching `filter`, ordered by priority descending, then creation time, then id.
    pub fn list_jobs(&self, filter: &JobFilter) -> Result<Vec<Job>, StoreError> {
        let mut query = format!("SELECT {JOB_COLUMNS} FROM jobs WHERE 1 = 1");
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
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
                values.push(Box::new(enum_to_text(state)?));
            }
            query.push(')');
        }
        if let Some(kind) = &filter.kind {
            query.push_str(" AND kind = ?");
            values.push(Box::new(enum_to_text(kind)?));
        }
        if let Some(role) = &filter.role {
            query.push_str(" AND role = ?");
            values.push(Box::new(enum_to_text(role)?));
        }
        if let Some(prefix) = &filter.id_prefix {
            query.push_str(" AND substr(id, 1, length(?)) = ?");
            values.push(Box::new(prefix.clone()));
            values.push(Box::new(prefix.clone()));
        }
        query.push_str(" ORDER BY priority DESC, created_at, id");
        if let Some(limit) = filter.limit {
            query.push_str(" LIMIT ?");
            values.push(Box::new(i64::from(limit)));
        }

        let mut statement = self.conn.prepare(&query)?;
        let rows = statement.query_map(params_from_iter(values), read_job_row)?;
        let mut jobs = Vec::new();
        for row in rows {
            let mut job = job_from_row(row?)?;
            job.needs = self.needs_of(&job.id)?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    /// Writes a new lifecycle state without checking whether the transition is legal.
    ///
    /// `state_changed_at` and `updated_at` become the clock's current time; `note` replaces the
    /// note when given; `parked_reason` is written only for [`JobState::Parked`] and cleared for
    /// every other state. Returns the previous state.
    pub fn set_job_state(&self, id: &JobId, state: JobState, change: &StateChange) -> Result<JobState, StoreError> {
        self.transaction(|tx| {
            let current = tx.require_job(id)?;
            let now = ts_to_text(tx.clock().now());
            let parked = if state == State::Parked {
                change.parked_reason.map(|reason| enum_to_text(&reason)).transpose()?
            } else {
                None
            };
            tx.conn.execute(
                "UPDATE jobs SET state = ?1, state_changed_at = ?2, updated_at = ?2,
                    note = COALESCE(?3, note), parked_reason = ?4
                 WHERE id = ?5",
                params![enum_to_text(&state)?, now, change.note.as_deref(), parked, id.as_str()],
            )?;
            Ok(current.state)
        })
    }

    /// Applies a partial update; lifecycle state is never changed.
    pub fn patch_job(&self, id: &JobId, patch: &JobPatch) -> Result<Job, StoreError> {
        self.transaction(|tx| {
            tx.require_job(id)?;
            let now = ts_to_text(tx.clock().now());
            if let Some(title) = &patch.title {
                tx.conn.execute(
                    "UPDATE jobs SET title = ?1, updated_at = ?2 WHERE id = ?3",
                    params![title, now, id.as_str()],
                )?;
            }
            if let Some(priority) = patch.priority {
                tx.conn.execute(
                    "UPDATE jobs SET priority = ?1, updated_at = ?2 WHERE id = ?3",
                    params![priority, now, id.as_str()],
                )?;
            }
            if let Some(profile) = &patch.profile {
                tx.conn.execute(
                    "UPDATE jobs SET profile = ?1, updated_at = ?2 WHERE id = ?3",
                    params![profile, now, id.as_str()],
                )?;
            }
            if let Some(owns) = &patch.owns {
                tx.conn.execute(
                    "UPDATE jobs SET owns_json = ?1, updated_at = ?2 WHERE id = ?3",
                    params![json_to_text(owns)?, now, id.as_str()],
                )?;
            }
            if let Some(reads) = &patch.reads {
                tx.conn.execute(
                    "UPDATE jobs SET reads_json = ?1, updated_at = ?2 WHERE id = ?3",
                    params![json_to_text(reads)?, now, id.as_str()],
                )?;
            }
            if let Some(gates) = &patch.gates {
                tx.conn.execute(
                    "UPDATE jobs SET gates_json = ?1, updated_at = ?2 WHERE id = ?3",
                    params![json_to_text(gates)?, now, id.as_str()],
                )?;
            }
            if let Some(loop_) = &patch.loop_ {
                tx.conn.execute(
                    "UPDATE jobs SET loop_json = ?1, updated_at = ?2 WHERE id = ?3",
                    params![json_to_text(loop_)?, now, id.as_str()],
                )?;
            }
            if let Some(note) = &patch.note {
                tx.conn.execute(
                    "UPDATE jobs SET note = ?1, updated_at = ?2 WHERE id = ?3",
                    params![note, now, id.as_str()],
                )?;
            }
            for need in &patch.add_needs {
                tx.add_edge(&Edge {
                    src: id.clone(),
                    dst: need.clone(),
                    kind: EdgeKind::Needs,
                })?;
            }
            for need in &patch.remove_needs {
                tx.remove_edge(&Edge {
                    src: id.clone(),
                    dst: need.clone(),
                    kind: EdgeKind::Needs,
                })?;
            }
            tx.require_job(id)
        })
    }

    /// Sets any of the attempt, continue and hang-restart counters that are given.
    pub fn set_job_counters(
        &self,
        id: &JobId,
        attempts: Option<u32>,
        continues: Option<u32>,
        hang_restarts: Option<u32>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET attempts = COALESCE(?1, attempts), continues = COALESCE(?2, continues),
                hang_restarts = COALESCE(?3, hang_restarts), updated_at = ?4
             WHERE id = ?5",
            params![
                attempts.map(i64::from),
                continues.map(i64::from),
                hang_restarts.map(i64::from),
                ts_to_text(self.clock().now()),
                id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("job", id.as_str()));
        }
        Ok(())
    }

    /// Sets the job branch and worktree, or clears them when `None`.
    pub fn set_job_workspace(
        &self,
        id: &JobId,
        branch: Option<&str>,
        worktree: Option<&str>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET branch = ?1, worktree = ?2, updated_at = ?3 WHERE id = ?4",
            params![branch, worktree, ts_to_text(self.clock().now()), id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("job", id.as_str()));
        }
        Ok(())
    }

    /// Moves `last_progress_at` forward to `at`, never backwards.
    pub fn touch_job_progress(&self, id: &JobId, at: Timestamp) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET last_progress_at =
                CASE WHEN last_progress_at IS NULL OR last_progress_at < ?1 THEN ?1 ELSE last_progress_at END
             WHERE id = ?2",
            params![ts_to_text(at), id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("job", id.as_str()));
        }
        Ok(())
    }

    /// Replaces the job's status note.
    pub fn set_job_note(&self, id: &JobId, note: &str) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE jobs SET note = ?1, updated_at = ?2 WHERE id = ?3",
            params![note, ts_to_text(self.clock().now()), id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("job", id.as_str()));
        }
        Ok(())
    }

    /// Deletes a job only while it has not started: `pending`, `cancelled` or `replanned`.
    pub fn delete_job(&self, id: &JobId) -> Result<(), StoreError> {
        let job = self.require_job(id)?;
        if !matches!(job.state, JobState::Pending | JobState::Cancelled | JobState::Replanned) {
            return Err(StoreError::Conflict(format!(
                "job {} is {} and cannot be deleted",
                id,
                job.state.as_str()
            )));
        }
        self.conn.execute("DELETE FROM jobs WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    /// Adds one edge; duplicate edges are ignored and self-edges are rejected.
    pub fn add_edge(&self, edge: &Edge) -> Result<(), StoreError> {
        if edge.src == edge.dst {
            return Err(StoreError::InvalidInput(
                "edge source and destination are the same".to_owned(),
            ));
        }
        if !self.job_exists(&edge.src)? {
            return Err(StoreError::not_found("job (edge source)", edge.src.as_str()));
        }
        if !self.job_exists(&edge.dst)? {
            return Err(StoreError::not_found("job (edge destination)", edge.dst.as_str()));
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO edges (src, dst, kind, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                edge.src.as_str(),
                edge.dst.as_str(),
                enum_to_text(&edge.kind)?,
                ts_to_text(self.clock().now()),
            ],
        )?;
        Ok(())
    }

    /// Removes one edge, returning whether a row was deleted.
    pub fn remove_edge(&self, edge: &Edge) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM edges WHERE src = ?1 AND dst = ?2 AND kind = ?3",
            params![edge.src.as_str(), edge.dst.as_str(), enum_to_text(&edge.kind)?],
        )?;
        Ok(changed > 0)
    }

    /// Lists edges, optionally restricted to those whose source is in `project`.
    pub fn list_edges(&self, project: Option<&ProjectName>) -> Result<Vec<Edge>, StoreError> {
        let rows = match project {
            Some(project) => {
                let mut statement = self.conn.prepare(
                    "SELECT src, dst, kind FROM edges
                     WHERE src IN (SELECT id FROM jobs WHERE project = ?1)
                     ORDER BY src, dst, kind",
                )?;
                let rows = statement.query_map([project.as_str()], read_edge_row)?;
                collect_edges(rows)?
            }
            None => {
                let mut statement = self
                    .conn
                    .prepare("SELECT src, dst, kind FROM edges ORDER BY src, dst, kind")?;
                let rows = statement.query_map([], read_edge_row)?;
                collect_edges(rows)?
            }
        };
        Ok(rows)
    }

    /// Lists every edge that touches `id` as source or destination.
    pub fn edges_of(&self, id: &JobId) -> Result<Vec<Edge>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT src, dst, kind FROM edges WHERE src = ?1 OR dst = ?1 ORDER BY src, dst, kind")?;
        let rows = statement.query_map([id.as_str()], read_edge_row)?;
        collect_edges(rows)
    }

    /// Returns the destinations of the `needs` edges whose source is `id`.
    pub fn needs_of(&self, id: &JobId) -> Result<Vec<JobId>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT dst FROM edges WHERE src = ?1 AND kind = 'needs' ORDER BY dst")?;
        let rows = statement.query_map([id.as_str()], |row| row.get::<_, String>(0))?;
        let mut needs = Vec::new();
        for row in rows {
            let value = row?;
            needs.push(JobId::parse(&value)?);
        }
        Ok(needs)
    }

    /// Returns the sources of the `needs` edges whose destination is `id`.
    pub fn dependents_of(&self, id: &JobId) -> Result<Vec<JobId>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT src FROM edges WHERE dst = ?1 AND kind = 'needs' ORDER BY src")?;
        let rows = statement.query_map([id.as_str()], |row| row.get::<_, String>(0))?;
        let mut dependents = Vec::new();
        for row in rows {
            let value = row?;
            dependents.push(JobId::parse(&value)?);
        }
        Ok(dependents)
    }

    /// Counts jobs by state, optionally restricted to one project.
    ///
    /// Keys are the canonical snake_case state names. `vowp_protocol::JobState` does not
    /// implement `Ord`, so a `BTreeMap<JobState, u32>` cannot be built without changing the
    /// protocol crate.
    pub fn count_jobs_by_state(&self, project: Option<&ProjectName>) -> Result<BTreeMap<String, u32>, StoreError> {
        let mut counts = BTreeMap::new();
        if let Some(project) = project {
            let mut statement = self
                .conn
                .prepare("SELECT state, COUNT(*) FROM jobs WHERE project = ?1 GROUP BY state")?;
            let rows = statement.query_map([project.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            collect_state_counts(rows, &mut counts)?;
        } else {
            let mut statement = self.conn.prepare("SELECT state, COUNT(*) FROM jobs GROUP BY state")?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
            collect_state_counts(rows, &mut counts)?;
        }
        Ok(counts)
    }

    /// Returns true when a job row with `id` exists.
    pub(crate) fn job_exists(&self, id: &JobId) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM jobs WHERE id = ?1", [id.as_str()], |_| Ok(()))
            .optional()?
            .is_some())
    }
}

/// Converts an iterator of raw edge rows.
fn collect_edges<T>(rows: T) -> Result<Vec<Edge>, StoreError>
where
    T: Iterator<Item = rusqlite::Result<(String, String, String)>>,
{
    let mut edges = Vec::new();
    for row in rows {
        let (src, dst, kind) = row?;
        edges.push(edge_from_parts(src, dst, kind)?);
    }
    Ok(edges)
}

/// Fills a state-count map from raw `(state, count)` rows.
fn collect_state_counts<T>(rows: T, counts: &mut BTreeMap<String, u32>) -> Result<(), StoreError>
where
    T: Iterator<Item = rusqlite::Result<(String, i64)>>,
{
    for row in rows {
        let (state, count) = row?;
        let state = enum_from_text::<JobState>(&state, "jobs.state")?;
        counts.insert(
            state.as_str().to_owned(),
            u32::try_from(count).map_err(|err| crate::convert::corrupt("jobs.count", err))?,
        );
    }
    Ok(())
}
