//! Repository methods for `sessions`, `session_events`, `session_natives`, `capsules` and
//! `switches`.
//! Spec: `docs/design/crates/vowp-store.md` §3.4, `docs/design/types.md` §9 and
//! `docs/design/sessions.md` §2–5.
//!
//! The session tables are an index over the canonical `log.jsonl`; content is never stored
//! here. [`Store::index_event`] keeps the session row's `last_seq`, `log_bytes` and
//! `updated_at` in step with the indexed event inside one transaction.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{
    Actor, Capsule, CapsuleId, EventId, EventIndexRow, EventType, Executor, HarnessKind, JobId, NodeName, ProjectName,
    Session, SessionId, SessionRole, SessionState, Switch, SwitchId, SwitchReason, SwitchState, Timestamp,
};

use crate::convert::{
    enum_from_text, enum_to_text, id_from_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text,
    ts_from_text, ts_to_text, u64_from_i64,
};
use crate::db::{SessionFilter, Store};
use crate::error::StoreError;

/// One `session_natives` row: a harness-side session id seen for a VOWP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNative {
    /// VOWP session the native id belongs to.
    pub session_id: SessionId,
    /// Harness that issued the native id.
    pub harness: HarnessKind,
    /// Harness-side session id used for `--resume`.
    pub native_session_id: String,
    /// Node that homes the native session.
    pub node: NodeName,
    /// VOWP sequence number of the attach event that first reported it.
    pub first_seq: u64,
    /// When the mapping was recorded.
    pub created_at: Timestamp,
}

/// Column list shared by every `sessions` query.
const SESSION_COLUMNS: &str = "id, project, job_id, role, title, parent_session_id, node, cwd, state, executor, \
     last_seq, last_capsule_id, log_bytes, created_at, updated_at";

/// Column list shared by every `session_events` query.
const EVENT_COLUMNS: &str = "session_id, seq, id, ts, type, actor, harness, model, byte_offset, byte_len, summary";

/// Column list shared by every `session_natives` query.
const NATIVE_COLUMNS: &str = "session_id, harness, native_session_id, node, first_seq, created_at";

/// Column list shared by every `switches` query.
const SWITCH_COLUMNS: &str = "id, session_id, from_executor, to_executor, from_node, to_node, capsule_id, reason, state, error, started_at, \
     ended_at, at_seq";

/// Raw values of one `sessions` row before protocol conversion.
struct SessionRow {
    id: String,
    project: Option<String>,
    job_id: Option<String>,
    role: Option<String>,
    title: String,
    parent_session_id: Option<String>,
    node: String,
    cwd: String,
    state: String,
    executor: Option<String>,
    last_seq: i64,
    last_capsule_id: Option<String>,
    log_bytes: i64,
    created_at: String,
    updated_at: String,
}

/// Reads the textual columns of a `sessions` row.
fn read_session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        project: row.get(1)?,
        job_id: row.get(2)?,
        role: row.get(3)?,
        title: row.get(4)?,
        parent_session_id: row.get(5)?,
        node: row.get(6)?,
        cwd: row.get(7)?,
        state: row.get(8)?,
        executor: row.get(9)?,
        last_seq: row.get(10)?,
        last_capsule_id: row.get(11)?,
        log_bytes: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

/// Converts a raw `sessions` row into a protocol [`Session`].
fn session_from_row(raw: SessionRow) -> Result<Session, StoreError> {
    Ok(Session {
        id: SessionId::parse(&raw.id)?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        job_id: raw.job_id.as_deref().map(JobId::parse).transpose()?,
        role: raw
            .role
            .as_deref()
            .map(|role| enum_from_text::<SessionRole>(role, "sessions.role"))
            .transpose()?,
        title: raw.title,
        parent_session_id: raw.parent_session_id.as_deref().map(SessionId::parse).transpose()?,
        node: NodeName::parse(&raw.node)?,
        cwd: raw.cwd,
        state: enum_from_text::<SessionState>(&raw.state, "sessions.state")?,
        executor: raw.executor.as_deref().map(Executor::parse).transpose()?,
        last_seq: u64_from_i64(raw.last_seq, "sessions.last_seq")?,
        last_capsule_id: raw.last_capsule_id.as_deref().map(CapsuleId::parse).transpose()?,
        log_bytes: u64_from_i64(raw.log_bytes, "sessions.log_bytes")?,
        created_at: ts_from_text(&raw.created_at, "sessions.created_at")?,
        updated_at: ts_from_text(&raw.updated_at, "sessions.updated_at")?,
    })
}

/// Raw values of one `session_events` row before protocol conversion.
struct EventRow {
    session_id: String,
    seq: i64,
    id: String,
    ts: String,
    event_type: String,
    actor: String,
    harness: String,
    model: Option<String>,
    byte_offset: i64,
    byte_len: i64,
    summary: String,
}

/// Reads the textual columns of a `session_events` row.
fn read_event_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        session_id: row.get(0)?,
        seq: row.get(1)?,
        id: row.get(2)?,
        ts: row.get(3)?,
        event_type: row.get(4)?,
        actor: row.get(5)?,
        harness: row.get(6)?,
        model: row.get(7)?,
        byte_offset: row.get(8)?,
        byte_len: row.get(9)?,
        summary: row.get(10)?,
    })
}

/// Converts a raw `session_events` row into an [`EventIndexRow`].
fn event_from_row(raw: EventRow) -> Result<EventIndexRow, StoreError> {
    Ok(EventIndexRow {
        session_id: SessionId::parse(&raw.session_id)?,
        seq: u64_from_i64(raw.seq, "session_events.seq")?,
        id: EventId::parse(&raw.id)?,
        ts: ts_from_text(&raw.ts, "session_events.ts")?,
        event_type: enum_from_text::<EventType>(&raw.event_type, "session_events.type")?,
        actor: enum_from_text::<Actor>(&raw.actor, "session_events.actor")?,
        harness: enum_from_text::<HarnessKind>(&raw.harness, "session_events.harness")?,
        model: raw.model,
        byte_offset: u64_from_i64(raw.byte_offset, "session_events.byte_offset")?,
        byte_len: u64_from_i64(raw.byte_len, "session_events.byte_len")?,
        summary: raw.summary,
    })
}

/// Raw values of one `capsules` row before protocol conversion.
struct CapsuleRow {
    body_json: String,
}

/// Reads the cached body of a `capsules` row.
fn read_capsule_row(row: &Row<'_>) -> rusqlite::Result<CapsuleRow> {
    Ok(CapsuleRow { body_json: row.get(0)? })
}

/// Raw values of one `switches` row before protocol conversion.
struct SwitchRow {
    id: String,
    session_id: String,
    from_executor: Option<String>,
    to_executor: String,
    from_node: Option<String>,
    to_node: String,
    capsule_id: Option<String>,
    reason: String,
    state: String,
    error: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    at_seq: i64,
}

/// Reads the textual columns of a `switches` row.
fn read_switch_row(row: &Row<'_>) -> rusqlite::Result<SwitchRow> {
    Ok(SwitchRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        from_executor: row.get(2)?,
        to_executor: row.get(3)?,
        from_node: row.get(4)?,
        to_node: row.get(5)?,
        capsule_id: row.get(6)?,
        reason: row.get(7)?,
        state: row.get(8)?,
        error: row.get(9)?,
        started_at: row.get(10)?,
        ended_at: row.get(11)?,
        at_seq: row.get(12)?,
    })
}

/// Converts a raw `switches` row into a protocol [`Switch`].
fn switch_from_row(raw: SwitchRow) -> Result<Switch, StoreError> {
    Ok(Switch {
        id: SwitchId::parse(&raw.id)?,
        session_id: SessionId::parse(&raw.session_id)?,
        from_executor: raw.from_executor.as_deref().map(Executor::parse).transpose()?,
        to_executor: Executor::parse(&raw.to_executor)?,
        from_node: raw.from_node.as_deref().map(NodeName::parse).transpose()?,
        to_node: NodeName::parse(&raw.to_node)?,
        capsule_id: raw.capsule_id.as_deref().map(CapsuleId::parse).transpose()?,
        reason: enum_from_text::<SwitchReason>(&raw.reason, "switches.reason")?,
        state: enum_from_text::<SwitchState>(&raw.state, "switches.state")?,
        error: raw.error,
        started_at: ts_from_text(&raw.started_at, "switches.started_at")?,
        ended_at: opt_ts_from_text(raw.ended_at.as_deref(), "switches.ended_at")?,
        at_seq: u64_from_i64(raw.at_seq, "switches.at_seq")?,
    })
}

impl Store {
    /// Inserts or updates one session index row, keyed by id.
    ///
    /// The creation timestamp of an existing row is kept; every other column is replaced.
    pub fn upsert_session(&self, session: &Session) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO sessions (
                id, project, job_id, role, title, parent_session_id, node, cwd, state, executor,
                last_seq, last_capsule_id, log_bytes, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(id) DO UPDATE SET
               project = excluded.project,
               job_id = excluded.job_id,
               role = excluded.role,
               title = excluded.title,
               parent_session_id = excluded.parent_session_id,
               node = excluded.node,
               cwd = excluded.cwd,
               state = excluded.state,
               executor = excluded.executor,
               last_seq = excluded.last_seq,
               last_capsule_id = excluded.last_capsule_id,
               log_bytes = excluded.log_bytes,
               updated_at = excluded.updated_at",
            params![
                session.id.as_str(),
                session.project.as_ref().map(ProjectName::as_str),
                session.job_id.as_ref().map(JobId::as_str),
                session.role.as_ref().map(enum_to_text).transpose()?,
                session.title,
                session.parent_session_id.as_ref().map(SessionId::as_str),
                session.node.as_str(),
                session.cwd,
                enum_to_text(&session.state)?,
                session.executor.as_ref().map(ToString::to_string),
                i64::try_from(session.last_seq).map_err(|err| crate::convert::corrupt("sessions.last_seq", err))?,
                session.last_capsule_id.as_ref().map(CapsuleId::as_str),
                i64::try_from(session.log_bytes).map_err(|err| crate::convert::corrupt("sessions.log_bytes", err))?,
                ts_to_text(session.created_at),
                ts_to_text(session.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the session called `id`, when it exists.
    pub fn session(&self, id: &SessionId) -> Result<Option<Session>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [id.as_str()],
                read_session_row,
            )
            .optional()?;
        raw.map(session_from_row).transpose()
    }

    /// Returns the session called `id` or [`StoreError::NotFound`].
    pub fn require_session(&self, id: &SessionId) -> Result<Session, StoreError> {
        self.session(id)?
            .ok_or_else(|| StoreError::not_found("session", id.as_str()))
    }

    /// Lists sessions matching `filter`, most recently updated first.
    pub fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<Session>, StoreError> {
        let mut query = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE 1 = 1");
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(project) = &filter.project {
            query.push_str(" AND project = ?");
            values.push(Box::new(project.as_str().to_owned()));
        }
        if let Some(job_id) = &filter.job_id {
            query.push_str(" AND job_id = ?");
            values.push(Box::new(job_id.as_str().to_owned()));
        }
        if let Some(state) = &filter.state {
            query.push_str(" AND state = ?");
            values.push(Box::new(enum_to_text(state)?));
        }
        query.push_str(" ORDER BY updated_at DESC, id DESC");
        if let Some(limit) = filter.limit {
            query.push_str(" LIMIT ?");
            values.push(Box::new(i64::from(limit)));
        }

        let mut statement = self.conn.prepare(&query)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), read_session_row)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(session_from_row(row?)?);
        }
        Ok(sessions)
    }

    /// Sets the session state and current executor, stamping `updated_at` from the clock.
    ///
    /// Passing `None` clears the executor, as the detach step of a switch requires.
    pub fn set_session_state(
        &self,
        id: &SessionId,
        state: SessionState,
        executor: Option<&Executor>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE sessions SET state = ?1, executor = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                enum_to_text(&state)?,
                executor.map(ToString::to_string),
                ts_to_text(self.clock().now()),
                id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("session", id.as_str()));
        }
        Ok(())
    }

    /// Inserts one indexed event and advances the session's `last_seq`, `log_bytes` and
    /// `updated_at` in the same transaction.
    pub fn index_event(&self, row: &EventIndexRow) -> Result<(), StoreError> {
        self.transaction(|tx| {
            if tx.session(&row.session_id)?.is_none() {
                return Err(StoreError::not_found("session", row.session_id.as_str()));
            }
            tx.conn.execute(
                "INSERT INTO session_events (
                    session_id, seq, id, ts, type, actor, harness, model, byte_offset, byte_len, summary
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.session_id.as_str(),
                    i64::try_from(row.seq).map_err(|err| crate::convert::corrupt("session_events.seq", err))?,
                    row.id.as_str(),
                    ts_to_text(row.ts),
                    enum_to_text(&row.event_type)?,
                    enum_to_text(&row.actor)?,
                    enum_to_text(&row.harness)?,
                    row.model,
                    i64::try_from(row.byte_offset)
                        .map_err(|err| crate::convert::corrupt("session_events.byte_offset", err))?,
                    i64::try_from(row.byte_len)
                        .map_err(|err| crate::convert::corrupt("session_events.byte_len", err))?,
                    row.summary,
                ],
            )?;
            let end = row.byte_offset.saturating_add(row.byte_len);
            let ts = ts_to_text(row.ts);
            tx.conn.execute(
                "UPDATE sessions SET
                    last_seq = MAX(last_seq, ?1),
                    log_bytes = MAX(log_bytes, ?2),
                    updated_at = CASE WHEN updated_at < ?3 THEN ?3 ELSE updated_at END
                 WHERE id = ?4",
                params![
                    i64::try_from(row.seq).map_err(|err| crate::convert::corrupt("session_events.seq", err))?,
                    i64::try_from(end).map_err(|err| crate::convert::corrupt("session_events.byte_len", err))?,
                    ts,
                    row.session_id.as_str(),
                ],
            )?;
            Ok(())
        })
    }

    /// Lists indexed events after `after_seq`, oldest first, at most `limit` rows.
    pub fn event_rows(&self, id: &SessionId, after_seq: u64, limit: u32) -> Result<Vec<EventIndexRow>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM session_events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![
                id.as_str(),
                i64::try_from(after_seq).map_err(|err| crate::convert::corrupt("session_events.seq", err))?,
                i64::from(limit),
            ],
            read_event_row,
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(event_from_row(row?)?);
        }
        Ok(events)
    }

    /// Returns the indexed event with `id`, when it exists.
    pub fn event_row_by_id(&self, id: &EventId) -> Result<Option<EventIndexRow>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM session_events WHERE id = ?1"),
                [id.as_str()],
                read_event_row,
            )
            .optional()?;
        raw.map(event_from_row).transpose()
    }

    /// Records one harness-side session id; a duplicate `(session, harness, native)` pair is
    /// a no-op.
    pub fn upsert_native(
        &self,
        session: &SessionId,
        harness: HarnessKind,
        native: &str,
        node: &NodeName,
        first_seq: u64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO session_natives (session_id, harness, native_session_id, node, first_seq, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, harness, native_session_id) DO NOTHING",
            params![
                session.as_str(),
                enum_to_text(&harness)?,
                native,
                node.as_str(),
                i64::try_from(first_seq).map_err(|err| crate::convert::corrupt("session_natives.first_seq", err))?,
                ts_to_text(self.clock().now()),
            ],
        )?;
        Ok(())
    }

    /// Lists every native session id recorded for `session`, oldest first.
    pub fn natives_of(&self, session: &SessionId) -> Result<Vec<SessionNative>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {NATIVE_COLUMNS} FROM session_natives WHERE session_id = ?1
             ORDER BY created_at, first_seq, native_session_id"
        ))?;
        let rows = statement.query_map([session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut natives = Vec::new();
        for row in rows {
            let (session_id, harness, native_session_id, node, first_seq, created_at) = row?;
            natives.push(SessionNative {
                session_id: id_from_text(&session_id, "session_natives.session_id")?,
                harness: enum_from_text(&harness, "session_natives.harness")?,
                native_session_id,
                node: NodeName::parse(&node)?,
                first_seq: u64_from_i64(first_seq, "session_natives.first_seq")?,
                created_at: ts_from_text(&created_at, "session_natives.created_at")?,
            });
        }
        Ok(natives)
    }

    /// Returns the most recent native session id for one harness, when recorded.
    pub fn native_for(&self, session: &SessionId, harness: HarnessKind) -> Result<Option<String>, StoreError> {
        let native = self
            .conn
            .query_row(
                "SELECT native_session_id FROM session_natives
                 WHERE session_id = ?1 AND harness = ?2
                 ORDER BY created_at DESC, first_seq DESC, native_session_id DESC LIMIT 1",
                params![session.as_str(), enum_to_text(&harness)?],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(native)
    }

    /// Inserts a capsule and points `sessions.last_capsule_id` at it in one transaction.
    pub fn insert_capsule(&self, capsule: &Capsule, path: &str) -> Result<(), StoreError> {
        self.transaction(|tx| {
            if tx.session(&capsule.session_id)?.is_none() {
                return Err(StoreError::not_found("session", capsule.session_id.as_str()));
            }
            tx.conn.execute(
                "INSERT INTO capsules (id, session_id, at_seq, version, trigger, author, path, body_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    capsule.capsule_id.as_str(),
                    capsule.session_id.as_str(),
                    i64::try_from(capsule.at_seq).map_err(|err| crate::convert::corrupt("capsules.at_seq", err))?,
                    i64::from(capsule.version),
                    enum_to_text(&capsule.trigger)?,
                    capsule.from.executor.to_string(),
                    path,
                    json_to_text(capsule)?,
                    ts_to_text(capsule.created_at),
                ],
            )?;
            let changed = tx.conn.execute(
                "UPDATE sessions SET last_capsule_id = ?1 WHERE id = ?2",
                params![capsule.capsule_id.as_str(), capsule.session_id.as_str()],
            )?;
            if changed == 0 {
                return Err(StoreError::not_found("session", capsule.session_id.as_str()));
            }
            Ok(())
        })
    }

    /// Returns the capsule called `id`, when it exists.
    pub fn capsule(&self, id: &CapsuleId) -> Result<Option<Capsule>, StoreError> {
        let raw = self
            .conn
            .query_row(
                "SELECT body_json FROM capsules WHERE id = ?1",
                [id.as_str()],
                read_capsule_row,
            )
            .optional()?;
        match raw {
            Some(raw) => Ok(Some(json_from_text(&raw.body_json, "capsules.body_json")?)),
            None => Ok(None),
        }
    }

    /// Returns the session's latest capsule by `at_seq`, then creation time.
    pub fn latest_capsule(&self, session: &SessionId) -> Result<Option<Capsule>, StoreError> {
        let raw = self
            .conn
            .query_row(
                "SELECT body_json FROM capsules WHERE session_id = ?1
                 ORDER BY at_seq DESC, created_at DESC LIMIT 1",
                [session.as_str()],
                read_capsule_row,
            )
            .optional()?;
        match raw {
            Some(raw) => Ok(Some(json_from_text(&raw.body_json, "capsules.body_json")?)),
            None => Ok(None),
        }
    }

    /// Lists a session's capsules, newest first.
    pub fn list_capsules(&self, session: &SessionId) -> Result<Vec<Capsule>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT body_json FROM capsules WHERE session_id = ?1 ORDER BY at_seq DESC, created_at DESC")?;
        let rows = statement.query_map([session.as_str()], read_capsule_row)?;
        let mut capsules = Vec::new();
        for row in rows {
            let raw = row?;
            capsules.push(json_from_text(&raw.body_json, "capsules.body_json")?);
        }
        Ok(capsules)
    }

    /// Inserts one executor-switch row.
    pub fn insert_switch(&self, switch: &Switch) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO switches (
                id, session_id, from_executor, to_executor, from_node, to_node, capsule_id, reason,
                state, error, started_at, ended_at, at_seq
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                switch.id.as_str(),
                switch.session_id.as_str(),
                switch.from_executor.as_ref().map(ToString::to_string),
                switch.to_executor.to_string(),
                switch.from_node.as_ref().map(NodeName::as_str),
                switch.to_node.as_str(),
                switch.capsule_id.as_ref().map(CapsuleId::as_str),
                enum_to_text(&switch.reason)?,
                enum_to_text(&switch.state)?,
                switch.error,
                ts_to_text(switch.started_at),
                opt_ts_to_text(switch.ended_at),
                i64::try_from(switch.at_seq).map_err(|err| crate::convert::corrupt("switches.at_seq", err))?,
            ],
        )?;
        Ok(())
    }

    /// Finishes a switch as `completed` or `aborted`, stamping `ended_at` from the clock.
    ///
    /// A capsule supplied for a completed switch is linked on the same row (the column is
    /// left untouched when `capsule_id` is `None`). Any other state is rejected with
    /// [`StoreError::InvalidInput`].
    pub fn finish_switch(
        &self,
        id: &SwitchId,
        state: SwitchState,
        error: Option<&str>,
        capsule_id: Option<&CapsuleId>,
    ) -> Result<(), StoreError> {
        if !matches!(state, SwitchState::Completed | SwitchState::Aborted) {
            return Err(StoreError::InvalidInput(
                "finish_switch accepts only completed or aborted".to_owned(),
            ));
        }
        let changed = self.conn.execute(
            "UPDATE switches SET state = ?1, error = ?2, ended_at = ?3,
                capsule_id = COALESCE(?4, capsule_id)
              WHERE id = ?5",
            params![
                enum_to_text(&state)?,
                error,
                ts_to_text(self.clock().now()),
                capsule_id.map(CapsuleId::as_str),
                id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("switch", id.as_str()));
        }
        Ok(())
    }

    /// Returns the switch called `id`, when it exists.
    pub fn switch(&self, id: &SwitchId) -> Result<Option<Switch>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {SWITCH_COLUMNS} FROM switches WHERE id = ?1"),
                [id.as_str()],
                read_switch_row,
            )
            .optional()?;
        raw.map(switch_from_row).transpose()
    }

    /// Lists a session's switches, oldest first.
    pub fn list_switches(&self, session: &SessionId) -> Result<Vec<Switch>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {SWITCH_COLUMNS} FROM switches WHERE session_id = ?1 ORDER BY started_at, id"
        ))?;
        let rows = statement.query_map([session.as_str()], read_switch_row)?;
        let mut switches = Vec::new();
        for row in rows {
            switches.push(switch_from_row(row?)?);
        }
        Ok(switches)
    }

    /// Lists every switch still in `started` state, oldest first, for crash recovery.
    pub fn open_switches(&self) -> Result<Vec<Switch>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {SWITCH_COLUMNS} FROM switches WHERE state = 'started' ORDER BY started_at, id"
        ))?;
        let rows = statement.query_map([], read_switch_row)?;
        let mut switches = Vec::new();
        for row in rows {
            switches.push(switch_from_row(row?)?);
        }
        Ok(switches)
    }
}
