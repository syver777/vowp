//! Rebuilds the session index tables from the authoritative files on disk.
//! Spec: `docs/design/crates/vowp-store.md` §3.7 and §4.3 rules 8–9, and
//! `docs/design/sessions.md` §1 rule 7.
//!
//! `log.jsonl` is authoritative for session content; `capsules/*.json` and `adapters.json`
//! are authoritative for capsule bodies and harness-side ids. [`reindex`] rebuilds
//! `session_events` and `session_natives`, upserts `capsules` and recreates missing
//! `sessions` and `switches` rows. It never deletes capsule rows: `switches.capsule_id`
//! points at them and must survive a reindex.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use vowp_protocol::{
    AttachAction, Capsule, EventBody, Executor, HarnessKind, NodeName, SessionCreated, SessionId, SwitchEvent,
    Timestamp,
};

use crate::convert::{enum_to_text, json_to_text, ts_to_text};
use crate::db::Store;
use crate::error::StoreError;
use crate::paths::NodePaths;
use crate::session_files::{list_capsule_files, read_adapters_json, read_capsule_file};
use crate::session_log::{IndexedEvent, SessionLog, index_row_parts};

/// What one [`reindex`] run rebuilt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Session directories indexed successfully.
    pub sessions: u32,
    /// `session_events` rows written.
    pub events: u64,
    /// Capsule files upserted.
    pub capsules: u32,
    /// `session_natives` rows written.
    pub natives: u32,
    /// Logs whose final line was torn and therefore ignored.
    pub torn_tails: u32,
    /// Non-fatal problems, one line per skipped file or session.
    pub problems: Vec<String>,
}

/// The node name used when a session has no executor event to name its node.
const DEFAULT_NODE: &str = "local";

/// Rebuilds the index tables for one session, or for every directory under `sessions/`.
///
/// Corrupt sessions are reported in [`ReindexReport::problems`] and skipped; other sessions
/// continue. Running twice produces identical tables.
pub fn reindex(store: &Store, paths: &NodePaths, session: Option<&SessionId>) -> Result<ReindexReport, StoreError> {
    let mut report = ReindexReport::default();
    let ids = session_ids(paths, session, &mut report)?;
    for id in ids {
        if let Some(counts) = reindex_session(store, paths, &id, &mut report)? {
            report.sessions += 1;
            report.events += counts.events;
            report.capsules += counts.capsules;
            report.natives += counts.natives;
        }
    }
    Ok(report)
}

/// Counts contributed by one successfully indexed session.
struct Counts {
    events: u64,
    capsules: u32,
    natives: u32,
}

/// The values written to (or refreshed on) a `sessions` row.
struct SessionIndex {
    id: String,
    project: Option<String>,
    job_id: Option<String>,
    role: Option<String>,
    title: String,
    parent: Option<String>,
    node: String,
    cwd: String,
    executor: Option<String>,
    last_seq: u64,
    log_bytes: u64,
    created_at: String,
    updated_at: String,
}

impl SessionIndex {
    /// Inserts the rebuilt row for a database that has lost its index.
    fn insert(&self, conn: &Connection) -> Result<(), StoreError> {
        conn.execute(
            "INSERT INTO main.sessions (id, project, job_id, role, title, parent_session_id, node, cwd, state,
                                   executor, last_seq, last_capsule_id, log_bytes, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'idle', ?9, ?10, NULL, ?11, ?12, ?13)",
            params![
                self.id,
                self.project,
                self.job_id,
                self.role,
                self.title,
                self.parent,
                self.node,
                self.cwd,
                self.executor,
                sql_i64(self.last_seq, "session last_seq")?,
                sql_i64(self.log_bytes, "session log_bytes")?,
                self.created_at,
                self.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Refreshes the fields reindex owns on an existing row; everything else is preserved.
    fn refresh(&self, conn: &Connection) -> Result<(), StoreError> {
        conn.execute(
            "UPDATE main.sessions SET last_seq = ?2, log_bytes = ?3, updated_at = ?4, executor = ?5 WHERE id = ?1",
            params![
                self.id,
                sql_i64(self.last_seq, "session last_seq")?,
                sql_i64(self.log_bytes, "session log_bytes")?,
                self.updated_at,
                self.executor
            ],
        )?;
        Ok(())
    }
}

/// Lists the session directories to index, sorted by id.
fn session_ids(
    paths: &NodePaths,
    requested: Option<&SessionId>,
    report: &mut ReindexReport,
) -> Result<Vec<SessionId>, StoreError> {
    if let Some(id) = requested {
        if !paths.session_dir(id).is_dir() {
            report.problems.push(format!("session {id}: directory does not exist"));
            return Ok(Vec::new());
        }
        return Ok(vec![id.clone()]);
    }

    let dir = paths.sessions_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(StoreError::io(dir.display(), err)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| StoreError::io(dir.display(), err))?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        match SessionId::parse(&name) {
            Ok(id) => ids.push(id),
            Err(_) => report
                .problems
                .push(format!("skipping {name}: directory name is not a session id")),
        }
    }
    ids.sort();
    Ok(ids)
}

/// Indexes one session directory; returns `None` when the session was skipped.
fn reindex_session(
    store: &Store,
    paths: &NodePaths,
    id: &SessionId,
    report: &mut ReindexReport,
) -> Result<Option<Counts>, StoreError> {
    let log_path = paths.log_path(id);
    if !log_path.is_file() {
        report.problems.push(format!("session {id}: missing log.jsonl"));
        return Ok(None);
    }
    let file_len = std::fs::metadata(&log_path)
        .map_err(|err| StoreError::io(log_path.display(), err))?
        .len();

    let reader = SessionLog::open_read(paths, id)?;
    let mut events = Vec::new();
    for item in reader.iter()? {
        match item {
            Ok(event) => events.push(event),
            Err(err) => {
                report.problems.push(format!("session {id}: skipped: {err}"));
                return Ok(None);
            }
        }
    }
    let last_end = events
        .last()
        .map(|event| event.byte_offset + event.byte_len)
        .unwrap_or(0);
    if file_len > last_end {
        report.torn_tails += 1;
        report
            .problems
            .push(format!("session {id}: torn tail ({} bytes)", file_len - last_end));
    }

    let Some((created, created_ts)) = first_session_created(&events) else {
        report
            .problems
            .push(format!("session {id}: no session.created event; skipped"));
        return Ok(None);
    };

    let session = build_session_row(store, id, &created, created_ts, &events, file_len, report)?;
    let natives = executor_natives(&events);
    let adapters = match read_adapters_json(paths, id) {
        Ok(map) => map.natives,
        Err(err) => {
            report.problems.push(format!("session {id}: adapters.json: {err}"));
            Vec::new()
        }
    };
    let capsules = read_capsules(paths, id, report)?;
    let switches = switch_events(&events);

    let counts = store.transaction(|store| {
        let conn = &store.conn;
        conn.execute(
            "DELETE FROM main.session_events WHERE session_id = ?1",
            params![session.id],
        )?;
        conn.execute(
            "DELETE FROM main.session_natives WHERE session_id = ?1",
            params![session.id],
        )?;

        let exists = conn
            .query_row("SELECT 1 FROM main.sessions WHERE id = ?1", params![session.id], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if exists {
            session.refresh(conn)?;
        } else {
            session.insert(conn)?;
        }

        let mut events_written = 0_u64;
        for event in &events {
            let row = index_row_parts(
                &event.envelope,
                event.byte_offset,
                event.byte_len,
                event.envelope.body().map(|body| body.summary()).unwrap_or_default(),
            );
            conn.execute(
                "INSERT INTO main.session_events (session_id, seq, id, ts, type, actor, harness, model, byte_offset,
                                             byte_len, summary)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.session_id.to_string(),
                    sql_i64(row.seq, "event seq")?,
                    row.id.to_string(),
                    ts_to_text(row.ts),
                    enum_to_text(&row.event_type)?,
                    enum_to_text(&row.actor)?,
                    row.harness.as_str(),
                    row.model,
                    sql_i64(row.byte_offset, "event byte_offset")?,
                    sql_i64(row.byte_len, "event byte_len")?,
                    row.summary,
                ],
            )?;
            events_written += 1;
        }

        let mut natives_written = 0_u32;
        for native in &natives {
            natives_written += conn.execute(
                "INSERT INTO main.session_natives (session_id, harness, native_session_id, node, first_seq, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id, harness, native_session_id) DO NOTHING",
                params![
                    session.id,
                    native.harness.as_str(),
                    native.native_session_id,
                    native.node,
                    sql_i64(native.first_seq, "native first_seq")?,
                    native.created_at,
                ],
            )? as u32;
        }
        for mapping in &adapters {
            natives_written += conn.execute(
                "INSERT INTO main.session_natives (session_id, harness, native_session_id, node, first_seq, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id, harness, native_session_id) DO NOTHING",
                params![
                    session.id,
                    mapping.harness.as_str(),
                    mapping.native_session_id,
                    mapping.node.to_string(),
                    sql_i64(mapping.first_seq, "adapter first_seq")?,
                    ts_to_text(mapping.created_at),
                ],
            )? as u32;
        }

        for (capsule, path) in &capsules {
            upsert_capsule(conn, capsule, path)?;
        }
        report_missing_capsule_files(conn, id, report)?;
        set_last_capsule(conn, &session.id)?;

        for (seq, ts, switch) in &switches {
            insert_missing_switch(conn, &session.id, &session.node, *seq, *ts, switch)?;
        }

        Ok(Counts {
            events: events_written,
            capsules: capsules.len() as u32,
            natives: natives_written,
        })
    })?;

    Ok(Some(counts))
}

/// Finds the first `session.created` event and its timestamp.
fn first_session_created(events: &[IndexedEvent]) -> Option<(SessionCreated, Timestamp)> {
    for event in events {
        if let Ok(EventBody::SessionCreated(created)) = event.envelope.body() {
            return Some((created, event.envelope.ts));
        }
    }
    None
}

/// Builds the `sessions` row values from the log, checking optional foreign keys.
fn build_session_row(
    store: &Store,
    id: &SessionId,
    created: &SessionCreated,
    created_at: Timestamp,
    events: &[IndexedEvent],
    file_len: u64,
    report: &mut ReindexReport,
) -> Result<SessionIndex, StoreError> {
    let project = match &created.project {
        Some(project) if row_exists(&store.conn, "main.projects", "name", project.as_str())? => {
            Some(project.to_string())
        }
        Some(project) => {
            report.problems.push(format!(
                "session {id}: project {project} is not in the database; left null"
            ));
            None
        }
        None => None,
    };
    let job_id = match &created.job_id {
        Some(job) if store.bound_project().is_none() => Some(job.to_string()),
        Some(job) if store.job(job)?.is_some() => Some(job.to_string()),
        Some(job) => {
            report.problems.push(format!(
                "session {id}: job {job} is not in the bound project; left null"
            ));
            None
        }
        None => None,
    };
    let role = created.role.as_ref().map(enum_to_text).transpose()?;

    let mut executor: Option<Executor> = None;
    let mut node: Option<NodeName> = None;
    for event in events {
        let Ok(EventBody::SessionExecutor(payload)) = event.envelope.body() else {
            continue;
        };
        node = Some(payload.node.clone());
        executor = match payload.action {
            AttachAction::Attach => Some(payload.executor),
            AttachAction::Detach => None,
        };
    }
    let node = match node {
        Some(node) => node.to_string(),
        None => DEFAULT_NODE.to_owned(),
    };
    let updated_at = events.last().map(|event| event.envelope.ts).unwrap_or(created_at);
    let last_seq = events.last().map(|event| event.envelope.seq).unwrap_or(0);

    Ok(SessionIndex {
        id: id.to_string(),
        project,
        job_id,
        role,
        title: created.title.clone(),
        parent: created.parent_session_id.as_ref().map(ToString::to_string),
        node,
        cwd: created.cwd.clone(),
        executor: executor.map(|executor| executor.to_string()),
        last_seq,
        log_bytes: file_len,
        created_at: ts_to_text(created_at),
        updated_at: ts_to_text(updated_at),
    })
}

/// Returns whether a row with `column = value` exists in `table` (internal constants only).
fn row_exists(conn: &Connection, table: &str, column: &str, value: &str) -> Result<bool, StoreError> {
    // The identifiers come from this module, never from user input; only the value is bound.
    let sql = format!("SELECT 1 FROM {table} WHERE {column} = ?1");
    Ok(conn.query_row(&sql, params![value], |_| Ok(())).optional()?.is_some())
}

/// One harness-side id recovered from a `session.executor` attach event.
struct NativeRow {
    harness: HarnessKind,
    native_session_id: String,
    node: String,
    first_seq: u64,
    created_at: String,
}

/// Collects native session ids from attach events carrying one.
fn executor_natives(events: &[IndexedEvent]) -> Vec<NativeRow> {
    let mut natives = Vec::new();
    for event in events {
        let Ok(EventBody::SessionExecutor(payload)) = event.envelope.body() else {
            continue;
        };
        let Some(native_session_id) = payload.native_session_id else {
            continue;
        };
        if payload.action != AttachAction::Attach {
            continue;
        }
        natives.push(NativeRow {
            harness: payload.executor.harness,
            native_session_id,
            node: payload.node.to_string(),
            first_seq: event.envelope.seq,
            created_at: ts_to_text(event.envelope.ts),
        });
    }
    natives
}

/// Reads every capsule file of one session, reporting and skipping invalid files.
fn read_capsules(
    paths: &NodePaths,
    id: &SessionId,
    report: &mut ReindexReport,
) -> Result<Vec<(Capsule, PathBuf)>, StoreError> {
    let mut capsules = Vec::new();
    for path in list_capsule_files(paths, id)? {
        match read_capsule_file(&path) {
            Ok(capsule) if capsule.session_id == *id => capsules.push((capsule, path)),
            Ok(capsule) => report.problems.push(format!(
                "session {id}: capsule file {} belongs to session {}",
                path.display(),
                capsule.session_id
            )),
            Err(err @ (StoreError::Corrupt { .. } | StoreError::Protocol(_) | StoreError::Json(_))) => {
                report
                    .problems
                    .push(format!("session {id}: capsule file {}: {err}", path.display()));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(capsules)
}

/// Inserts or updates one capsule row without ever deleting the row.
fn upsert_capsule(conn: &Connection, capsule: &Capsule, path: &Path) -> Result<(), StoreError> {
    let body = json_to_text(capsule)?;
    let author = capsule.from.executor.to_string();
    conn.execute(
        "INSERT INTO main.capsules (id, session_id, at_seq, version, trigger, author, path, body_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET session_id = excluded.session_id, at_seq = excluded.at_seq,
                                       version = excluded.version, trigger = excluded.trigger,
                                       author = excluded.author, path = excluded.path,
                                       body_json = excluded.body_json, created_at = excluded.created_at",
        params![
            capsule.capsule_id.to_string(),
            capsule.session_id.to_string(),
            sql_i64(capsule.at_seq, "capsule at_seq")?,
            i64::from(capsule.version),
            enum_to_text(&capsule.trigger)?,
            author,
            path.display().to_string(),
            body,
            ts_to_text(capsule.created_at),
        ],
    )?;
    Ok(())
}

/// Reports capsule rows whose authoritative file is missing; the rows stay untouched.
fn report_missing_capsule_files(
    conn: &Connection,
    id: &SessionId,
    report: &mut ReindexReport,
) -> Result<(), StoreError> {
    let mut statement = conn.prepare("SELECT id, path FROM main.capsules WHERE session_id = ?1")?;
    let rows = statement.query_map(params![id.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (capsule_id, path) = row?;
        if !Path::new(&path).is_file() {
            report
                .problems
                .push(format!("session {id}: capsule {capsule_id} file is missing: {path}"));
        }
    }
    Ok(())
}

/// Points `sessions.last_capsule_id` at the capsule with the highest `at_seq`.
fn set_last_capsule(conn: &Connection, id: &str) -> Result<(), StoreError> {
    let latest = conn
        .query_row(
            "SELECT id FROM main.capsules WHERE session_id = ?1 ORDER BY at_seq DESC, created_at DESC LIMIT 1",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(capsule_id) = latest {
        conn.execute(
            "UPDATE main.sessions SET last_capsule_id = ?2 WHERE id = ?1",
            params![id, capsule_id],
        )?;
    }
    Ok(())
}

/// Collects `switch` events with their sequence number and envelope timestamp.
fn switch_events(events: &[IndexedEvent]) -> Vec<(u64, Timestamp, SwitchEvent)> {
    let mut switches = Vec::new();
    for event in events {
        if let Ok(EventBody::Switch(payload)) = event.envelope.body() {
            switches.push((event.envelope.seq, event.envelope.ts, payload));
        }
    }
    switches
}

/// Recreates a missing `switches` row from its log event; existing rows are untouched.
fn insert_missing_switch(
    conn: &Connection,
    session: &str,
    local_node: &str,
    seq: u64,
    ts: Timestamp,
    switch: &SwitchEvent,
) -> Result<(), StoreError> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM main.switches WHERE id = ?1",
            params![switch.switch_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(());
    }
    let capsule_id = match &switch.capsule_id {
        Some(capsule_id) if row_exists(conn, "main.capsules", "id", capsule_id.as_str())? => {
            Some(capsule_id.to_string())
        }
        _ => None,
    };
    let started_at = ts_to_text(ts);
    conn.execute(
        "INSERT INTO main.switches (id, session_id, from_executor, to_executor, from_node, to_node, capsule_id, reason,
                               state, error, started_at, ended_at, at_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'completed', NULL, ?9, ?9, ?10)",
        params![
            switch.switch_id.to_string(),
            session,
            switch.from.as_ref().map(|from| from.executor.to_string()),
            switch.to.executor.to_string(),
            switch
                .from
                .as_ref()
                .and_then(|from| from.node.as_ref())
                .map(ToString::to_string),
            switch
                .to
                .node
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| local_node.to_owned()),
            capsule_id,
            enum_to_text(&switch.reason)?,
            started_at,
            sql_i64(seq, "switch at_seq")?,
        ],
    )?;
    Ok(())
}

/// Converts an unsigned counter to the signed integer SQLite stores.
fn sql_i64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidInput(format!("{what} does not fit in SQLite: {value}")))
}
