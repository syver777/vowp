//! Repository methods for `loops` and `schedules`.
//! Spec: `docs/design/crates/vowp-store.md` §3.4, `docs/design/types.md` §10 and
//! `docs/design/loops.md` §11.
//!
//! A [`Loop`] is one supervisor loop's definition and mutable state; a [`Schedule`] is an L9
//! trigger. The store only persists rows and selects due schedules: computing `next_run_at`,
//! missed-run policy and firing belong to `vowp-core`.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{
    ActiveState, Loop, LoopBounds, LoopKind, MissedPolicy, ProjectName, Schedule, ScheduleAction, ScheduleId,
    ScheduleSpecKind, Timestamp,
};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text, ts_from_text,
    ts_to_text, u32_from_i64, u64_from_i64,
};
use crate::db::Store;
use crate::error::StoreError;

/// Column list shared by every `loops` query.
const LOOP_COLUMNS: &str = "name, kind, project, interval_secs, state, bounds_json, last_run_at, next_run_at, \
     last_status, fail_streak, updated_at";

/// Column list shared by every `schedules` query.
const SCHEDULE_COLUMNS: &str = "id, project, name, spec_kind, spec, tz, action_kind, action_json, missed_policy, \
     state, last_run_at, next_run_at, last_status, created_at";

/// Raw values of one `loops` row before protocol conversion.
struct LoopRow {
    name: String,
    kind: String,
    project: Option<String>,
    interval_secs: i64,
    state: String,
    bounds: String,
    last_run_at: Option<String>,
    next_run_at: Option<String>,
    last_status: String,
    fail_streak: i64,
    updated_at: String,
}

/// Reads the textual columns of a `loops` row.
fn read_loop_row(row: &Row<'_>) -> rusqlite::Result<LoopRow> {
    Ok(LoopRow {
        name: row.get(0)?,
        kind: row.get(1)?,
        project: row.get(2)?,
        interval_secs: row.get(3)?,
        state: row.get(4)?,
        bounds: row.get(5)?,
        last_run_at: row.get(6)?,
        next_run_at: row.get(7)?,
        last_status: row.get(8)?,
        fail_streak: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

/// Converts a raw `loops` row into a protocol [`Loop`].
fn loop_from_row(raw: LoopRow) -> Result<Loop, StoreError> {
    Ok(Loop {
        name: raw.name,
        kind: enum_from_text::<LoopKind>(&raw.kind, "loops.kind")?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        interval_secs: u64_from_i64(raw.interval_secs, "loops.interval_secs")?,
        state: enum_from_text::<ActiveState>(&raw.state, "loops.state")?,
        bounds: json_from_text::<LoopBounds>(&raw.bounds, "loops.bounds_json")?,
        last_run_at: opt_ts_from_text(raw.last_run_at.as_deref(), "loops.last_run_at")?,
        next_run_at: opt_ts_from_text(raw.next_run_at.as_deref(), "loops.next_run_at")?,
        last_status: raw.last_status,
        fail_streak: u32_from_i64(raw.fail_streak, "loops.fail_streak")?,
        updated_at: ts_from_text(&raw.updated_at, "loops.updated_at")?,
    })
}

/// Raw values of one `schedules` row before protocol conversion.
struct ScheduleRow {
    id: String,
    project: Option<String>,
    name: String,
    spec_kind: String,
    spec: String,
    tz: String,
    action: String,
    missed_policy: String,
    state: String,
    last_run_at: Option<String>,
    next_run_at: Option<String>,
    last_status: String,
    created_at: String,
}

/// Reads the textual columns of a `schedules` row.
fn read_schedule_row(row: &Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        id: row.get(0)?,
        project: row.get(1)?,
        name: row.get(2)?,
        spec_kind: row.get(3)?,
        spec: row.get(4)?,
        tz: row.get(5)?,
        action: row.get(7)?,
        missed_policy: row.get(8)?,
        state: row.get(9)?,
        last_run_at: row.get(10)?,
        next_run_at: row.get(11)?,
        last_status: row.get(12)?,
        created_at: row.get(13)?,
    })
}

/// Returns the stored `action_kind` wire name of a schedule action.
fn action_kind(action: &ScheduleAction) -> &'static str {
    match action {
        ScheduleAction::JobTemplate { .. } => "job_template",
        ScheduleAction::Loop { .. } => "loop",
        ScheduleAction::Command { .. } => "command",
    }
}

/// Converts a raw `schedules` row into a protocol [`Schedule`].
fn schedule_from_row(raw: ScheduleRow) -> Result<Schedule, StoreError> {
    Ok(Schedule {
        id: ScheduleId::parse(&raw.id)?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        name: raw.name,
        spec_kind: enum_from_text::<ScheduleSpecKind>(&raw.spec_kind, "schedules.spec_kind")?,
        spec: raw.spec,
        tz: raw.tz,
        action: json_from_text::<ScheduleAction>(&raw.action, "schedules.action_json")?,
        missed_policy: enum_from_text::<MissedPolicy>(&raw.missed_policy, "schedules.missed_policy")?,
        state: enum_from_text::<ActiveState>(&raw.state, "schedules.state")?,
        last_run_at: opt_ts_from_text(raw.last_run_at.as_deref(), "schedules.last_run_at")?,
        next_run_at: opt_ts_from_text(raw.next_run_at.as_deref(), "schedules.next_run_at")?,
        last_status: raw.last_status,
        created_at: ts_from_text(&raw.created_at, "schedules.created_at")?,
    })
}

impl Store {
    /// Inserts or updates one loop row, keyed by name.
    pub fn upsert_loop(&self, loop_: &Loop) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO loops (
                name, kind, project, interval_secs, state, bounds_json, last_run_at, next_run_at,
                last_status, fail_streak, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(name) DO UPDATE SET
               kind = excluded.kind,
               project = excluded.project,
               interval_secs = excluded.interval_secs,
               state = excluded.state,
               bounds_json = excluded.bounds_json,
               last_run_at = excluded.last_run_at,
               next_run_at = excluded.next_run_at,
               last_status = excluded.last_status,
               fail_streak = excluded.fail_streak,
               updated_at = excluded.updated_at",
            params![
                loop_.name,
                enum_to_text(&loop_.kind)?,
                loop_.project.as_ref().map(ProjectName::as_str),
                i64::try_from(loop_.interval_secs)
                    .map_err(|err| crate::convert::corrupt("loops.interval_secs", err))?,
                enum_to_text(&loop_.state)?,
                json_to_text(&loop_.bounds)?,
                opt_ts_to_text(loop_.last_run_at),
                opt_ts_to_text(loop_.next_run_at),
                loop_.last_status,
                i64::from(loop_.fail_streak),
                ts_to_text(loop_.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the loop called `name`, when it exists.
    pub fn loop_(&self, name: &str) -> Result<Option<Loop>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {LOOP_COLUMNS} FROM loops WHERE name = ?1"),
                [name],
                read_loop_row,
            )
            .optional()?;
        raw.map(loop_from_row).transpose()
    }

    /// Lists every loop, ordered by name.
    pub fn list_loops(&self) -> Result<Vec<Loop>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {LOOP_COLUMNS} FROM loops ORDER BY name"))?;
        let rows = statement.query_map([], read_loop_row)?;
        let mut loops = Vec::new();
        for row in rows {
            loops.push(loop_from_row(row?)?);
        }
        Ok(loops)
    }

    /// Sets one loop's active state, stamping `updated_at` from the clock.
    pub fn set_loop_state(&self, name: &str, state: ActiveState) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE loops SET state = ?1, updated_at = ?2 WHERE name = ?3",
            params![enum_to_text(&state)?, ts_to_text(self.clock().now()), name],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("loop", name));
        }
        Ok(())
    }

    /// Records one loop tick: timestamps, status and the consecutive-failure streak.
    pub fn record_loop_run(
        &self,
        name: &str,
        last_run: Timestamp,
        next_run: Option<Timestamp>,
        status: &str,
        ok: bool,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE loops SET last_run_at = ?1, next_run_at = ?2, last_status = ?3,
                fail_streak = CASE WHEN ?4 THEN 0 ELSE fail_streak + 1 END, updated_at = ?5
             WHERE name = ?6",
            params![
                ts_to_text(last_run),
                opt_ts_to_text(next_run),
                status,
                i64::from(ok),
                ts_to_text(self.clock().now()),
                name,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("loop", name));
        }
        Ok(())
    }

    /// Inserts one schedule row; a duplicate id or name is a [`StoreError::Conflict`].
    pub fn insert_schedule(&self, schedule: &Schedule) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO schedules (
                id, project, name, spec_kind, spec, tz, action_kind, action_json, missed_policy,
                state, last_run_at, next_run_at, last_status, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                schedule.id.as_str(),
                schedule.project.as_ref().map(ProjectName::as_str),
                schedule.name,
                enum_to_text(&schedule.spec_kind)?,
                schedule.spec,
                schedule.tz,
                action_kind(&schedule.action),
                json_to_text(&schedule.action)?,
                enum_to_text(&schedule.missed_policy)?,
                enum_to_text(&schedule.state)?,
                opt_ts_to_text(schedule.last_run_at),
                opt_ts_to_text(schedule.next_run_at),
                schedule.last_status,
                ts_to_text(schedule.created_at),
            ],
        )?;
        Ok(())
    }

    /// Returns a schedule by id or name, when it exists.
    pub fn schedule(&self, id_or_name: &str) -> Result<Option<Schedule>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!(
                    "SELECT {SCHEDULE_COLUMNS} FROM schedules WHERE id = ?1 OR name = ?1
                     ORDER BY CASE WHEN id = ?1 THEN 0 ELSE 1 END LIMIT 1"
                ),
                [id_or_name],
                read_schedule_row,
            )
            .optional()?;
        raw.map(schedule_from_row).transpose()
    }

    /// Lists every schedule, ordered by name.
    pub fn list_schedules(&self) -> Result<Vec<Schedule>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {SCHEDULE_COLUMNS} FROM schedules ORDER BY name"))?;
        let rows = statement.query_map([], read_schedule_row)?;
        let mut schedules = Vec::new();
        for row in rows {
            schedules.push(schedule_from_row(row?)?);
        }
        Ok(schedules)
    }

    /// Lists active schedules whose `next_run_at` has passed, oldest first.
    pub fn due_schedules(&self, now: Timestamp) -> Result<Vec<Schedule>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {SCHEDULE_COLUMNS} FROM schedules
             WHERE state = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ?1
             ORDER BY next_run_at, id"
        ))?;
        let rows = statement.query_map([ts_to_text(now)], read_schedule_row)?;
        let mut schedules = Vec::new();
        for row in rows {
            schedules.push(schedule_from_row(row?)?);
        }
        Ok(schedules)
    }

    /// Records one schedule fire: last/next run times and the last status.
    pub fn record_schedule_run(
        &self,
        id: &ScheduleId,
        last_run: Timestamp,
        next_run: Option<Timestamp>,
        status: &str,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE schedules SET last_run_at = ?1, next_run_at = ?2, last_status = ?3 WHERE id = ?4",
            params![ts_to_text(last_run), opt_ts_to_text(next_run), status, id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("schedule", id.as_str()));
        }
        Ok(())
    }

    /// Sets one schedule's active state.
    pub fn set_schedule_state(&self, id: &ScheduleId, state: ActiveState) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE schedules SET state = ?1 WHERE id = ?2",
            params![enum_to_text(&state)?, id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("schedule", id.as_str()));
        }
        Ok(())
    }

    /// Removes a schedule by id or name, returning whether a row was deleted.
    pub fn remove_schedule(&self, id_or_name: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM schedules WHERE id = ?1 OR name = ?1", [id_or_name])?;
        Ok(changed > 0)
    }
}
