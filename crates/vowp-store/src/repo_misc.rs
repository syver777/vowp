//! Repository methods for inboxes, limits, releases, `kv` and the CLI request queue.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.4, §3.6 and §3.7.
//!
//! Project rows live in `project.inbox` and `project.releases`; node-wide inbox and ledger
//! rows live in `main.node_inbox` and `main.node_ledger`. Requests are rows in `main.kv`;
//! the store treats `kind` and `payload` as opaque JSON so `vowp-core` remains the only owner
//! of the request vocabulary.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{
    GuardResult, HarnessKind, InboxId, InboxItem, InboxKind, InboxState, JobId, Limit, LimitRowKind, ProjectName,
    Release, ReleaseId, ReleaseState, Timestamp,
};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text, ts_from_text,
    ts_to_text, u32_from_i64,
};
use crate::db::{Request, RequestId, Store};
use crate::error::StoreError;

/// Column list shared by both inbox tables.
const INBOX_COLUMNS: &str =
    "id, project, job_id, kind, title, body, options_json, state, answer, answered_by, created_at, answered_at";

/// Column list shared by every `limits` query.
const LIMIT_COLUMNS: &str = "provider, harness, kind, resets_at, detail, since, last_probe_at, probe_fail_streak, \
     concurrency_cap, cap_until";

/// Column list shared by every `releases` query.
const RELEASE_COLUMNS: &str = "id, project, env, change_ref, from_commit, to_commit, state, guards_json, \
     snapshot_path, rollback_cmd, probe_json, approved_by, rolled_back_by, note, created_at, finished_at";

/// The two files an inbox row can live in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboxTable {
    Node,
    Project,
}

impl InboxTable {
    fn name(self) -> &'static str {
        match self {
            Self::Node => "main.node_inbox",
            Self::Project => "project.inbox",
        }
    }
}

/// Raw values of one inbox row before protocol conversion.
struct InboxRow {
    id: String,
    project: Option<String>,
    job_id: Option<String>,
    kind: String,
    title: String,
    body: String,
    options: String,
    state: String,
    answer: Option<String>,
    answered_by: Option<String>,
    created_at: String,
    answered_at: Option<String>,
}

/// Reads the textual columns of an inbox row.
fn read_inbox_row(row: &Row<'_>) -> rusqlite::Result<InboxRow> {
    Ok(InboxRow {
        id: row.get(0)?,
        project: row.get(1)?,
        job_id: row.get(2)?,
        kind: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        options: row.get(6)?,
        state: row.get(7)?,
        answer: row.get(8)?,
        answered_by: row.get(9)?,
        created_at: row.get(10)?,
        answered_at: row.get(11)?,
    })
}

/// Converts a raw inbox row into a protocol [`InboxItem`].
fn inbox_from_row(raw: InboxRow) -> Result<InboxItem, StoreError> {
    Ok(InboxItem {
        id: InboxId::parse(&raw.id)?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        job_id: raw.job_id.as_deref().map(JobId::parse).transpose()?,
        kind: enum_from_text::<InboxKind>(&raw.kind, "inbox.kind")?,
        title: raw.title,
        body: raw.body,
        options: json_from_text(&raw.options, "inbox.options_json")?,
        state: enum_from_text::<InboxState>(&raw.state, "inbox.state")?,
        answer: raw.answer,
        answered_by: raw.answered_by,
        created_at: ts_from_text(&raw.created_at, "inbox.created_at")?,
        answered_at: opt_ts_from_text(raw.answered_at.as_deref(), "inbox.answered_at")?,
    })
}

/// Raw values of one `limits` row before protocol conversion.
struct LimitRow {
    provider: String,
    harness: Option<String>,
    kind: String,
    resets_at: Option<String>,
    detail: String,
    since: String,
    last_probe_at: Option<String>,
    probe_fail_streak: i64,
    concurrency_cap: Option<i64>,
    cap_until: Option<String>,
}

/// Reads the textual columns of a `limits` row.
fn read_limit_row(row: &Row<'_>) -> rusqlite::Result<LimitRow> {
    Ok(LimitRow {
        provider: row.get(0)?,
        harness: row.get(1)?,
        kind: row.get(2)?,
        resets_at: row.get(3)?,
        detail: row.get(4)?,
        since: row.get(5)?,
        last_probe_at: row.get(6)?,
        probe_fail_streak: row.get(7)?,
        concurrency_cap: row.get(8)?,
        cap_until: row.get(9)?,
    })
}

/// Converts a raw `limits` row into a protocol [`Limit`].
fn limit_from_row(raw: LimitRow) -> Result<Limit, StoreError> {
    Ok(Limit {
        provider: raw.provider,
        harness: raw
            .harness
            .as_deref()
            .map(|harness| enum_from_text::<HarnessKind>(harness, "limits.harness"))
            .transpose()?,
        kind: enum_from_text::<LimitRowKind>(&raw.kind, "limits.kind")?,
        resets_at: opt_ts_from_text(raw.resets_at.as_deref(), "limits.resets_at")?,
        detail: raw.detail,
        since: ts_from_text(&raw.since, "limits.since")?,
        last_probe_at: opt_ts_from_text(raw.last_probe_at.as_deref(), "limits.last_probe_at")?,
        probe_fail_streak: u32_from_i64(raw.probe_fail_streak, "limits.probe_fail_streak")?,
        concurrency_cap: raw
            .concurrency_cap
            .map(|cap| u32_from_i64(cap, "limits.concurrency_cap"))
            .transpose()?,
        cap_until: opt_ts_from_text(raw.cap_until.as_deref(), "limits.cap_until")?,
    })
}

/// Raw values of one `releases` row before protocol conversion.
struct ReleaseRow {
    id: String,
    project: String,
    env: String,
    change_ref: String,
    from_commit: Option<String>,
    to_commit: Option<String>,
    state: String,
    guards: String,
    snapshot_path: Option<String>,
    rollback_cmd: Option<String>,
    probe: String,
    approved_by: Option<String>,
    rolled_back_by: Option<String>,
    note: String,
    created_at: String,
    finished_at: Option<String>,
}

/// Reads the textual columns of a `releases` row.
fn read_release_row(row: &Row<'_>) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        id: row.get(0)?,
        project: row.get(1)?,
        env: row.get(2)?,
        change_ref: row.get(3)?,
        from_commit: row.get(4)?,
        to_commit: row.get(5)?,
        state: row.get(6)?,
        guards: row.get(7)?,
        snapshot_path: row.get(8)?,
        rollback_cmd: row.get(9)?,
        probe: row.get(10)?,
        approved_by: row.get(11)?,
        rolled_back_by: row.get(12)?,
        note: row.get(13)?,
        created_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

/// Converts a raw `releases` row into a protocol [`Release`].
fn release_from_row(raw: ReleaseRow) -> Result<Release, StoreError> {
    Ok(Release {
        id: ReleaseId::parse(&raw.id)?,
        project: ProjectName::parse(&raw.project)?,
        env: raw.env,
        change_ref: raw.change_ref,
        from_commit: raw.from_commit,
        to_commit: raw.to_commit,
        state: enum_from_text::<ReleaseState>(&raw.state, "releases.state")?,
        guards: json_from_text::<Vec<GuardResult>>(&raw.guards, "releases.guards_json")?,
        snapshot_path: raw.snapshot_path,
        rollback_cmd: raw.rollback_cmd,
        probe: json_from_text(&raw.probe, "releases.probe_json")?,
        approved_by: raw.approved_by,
        rolled_back_by: raw.rolled_back_by.as_deref().map(ReleaseId::parse).transpose()?,
        note: raw.note,
        created_at: ts_from_text(&raw.created_at, "releases.created_at")?,
        finished_at: opt_ts_from_text(raw.finished_at.as_deref(), "releases.finished_at")?,
    })
}

impl Store {
    /// Inserts one inbox item, routing by `item.project`.
    ///
    /// `None` writes `node_inbox`; `Some(p)` writes the bound project's `inbox`. A job-bearing
    /// item with no project is invalid because it would show in every project's L7.
    pub fn insert_inbox(&self, item: &InboxItem) -> Result<(), StoreError> {
        match &item.project {
            None => {
                if item.job_id.is_some() {
                    return Err(StoreError::InvalidInput(
                        "an inbox item with a job id must also name its project".to_owned(),
                    ));
                }
                self.insert_node_inbox(item)
            }
            Some(project) => {
                self.check_bound(project)?;
                if let Some(job_id) = &item.job_id
                    && !self.job_exists(job_id)?
                {
                    return Err(StoreError::not_found("job (inbox)", job_id.as_str()));
                }
                insert_inbox_row(&self.conn, InboxTable::Project, item)
            }
        }
    }

    /// Inserts one node-level inbox item into `node_inbox`.
    ///
    /// A project name must be registered; a job id requires a project.
    pub fn insert_node_inbox(&self, item: &InboxItem) -> Result<(), StoreError> {
        if item.job_id.is_some() && item.project.is_none() {
            return Err(StoreError::InvalidInput(
                "a node inbox item with a job id must name its project".to_owned(),
            ));
        }
        if let Some(project) = &item.project
            && !self.project_row_exists(project)?
        {
            return Err(StoreError::InvalidInput(format!(
                "node inbox item names unregistered project {project}"
            )));
        }
        insert_inbox_row(&self.conn, InboxTable::Node, item)
    }

    /// Returns the inbox item called `id`, looking in `node_inbox` first.
    pub fn inbox_item(&self, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
        let node = inbox_by_id(&self.conn, InboxTable::Node, id)?;
        if node.is_some() {
            return Ok(node);
        }
        if self.bound_project().is_some() {
            return inbox_by_id(&self.conn, InboxTable::Project, id);
        }
        Ok(None)
    }

    /// Lists inbox items, oldest first, optionally restricted to one state.
    ///
    /// A bound handle merges the project inbox with `node_inbox` rows whose project is `NULL`
    /// or equals the bound project. An unbound (fleet) handle returns only `node_inbox` rows;
    /// that is the deliberate extension to §3.6 used for fleet-wide reads.
    pub fn list_inbox(&self, state: Option<InboxState>) -> Result<Vec<InboxItem>, StoreError> {
        let bound = self.bound_project().cloned();
        let mut items = Vec::new();
        if let Some(bound) = &bound {
            match state {
                Some(state) => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM project.inbox WHERE state = ?1 ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map([enum_to_text(&state)?], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
                None => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM project.inbox ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map([], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
            }
            match state {
                Some(state) => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM main.node_inbox
                         WHERE state = ?1 AND (project IS NULL OR project = ?2) ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map(params![enum_to_text(&state)?, bound.as_str()], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
                None => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM main.node_inbox
                         WHERE project IS NULL OR project = ?1 ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map([bound.as_str()], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
            }
        } else {
            match state {
                Some(state) => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM main.node_inbox WHERE state = ?1 ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map([enum_to_text(&state)?], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
                None => {
                    let mut statement = self.conn.prepare(&format!(
                        "SELECT {INBOX_COLUMNS} FROM main.node_inbox ORDER BY created_at, id"
                    ))?;
                    let rows = statement.query_map([], read_inbox_row)?;
                    for row in rows {
                        items.push(inbox_from_row(row?)?);
                    }
                }
            }
        }
        items.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        items.dedup_by(|left, right| left.id == right.id);
        Ok(items)
    }

    /// Answers an open inbox item, returning the updated row.
    ///
    /// A second answer is a [`StoreError::Conflict`]; the job is not touched.
    pub fn answer_inbox(&self, id: &InboxId, answer: &str, by: &str) -> Result<InboxItem, StoreError> {
        let table = self.inbox_table(id)?;
        self.transaction(|tx| {
            let current = tx
                .item_in_table(table, id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))?;
            if current.state != InboxState::Open {
                return Err(StoreError::Conflict(format!("inbox item {id} is not open")));
            }
            tx.conn.execute(
                &format!(
                    "UPDATE {} SET state = 'answered', answer = ?1, answered_by = ?2, answered_at = ?3 WHERE id = ?4",
                    table.name()
                ),
                params![answer, by, ts_to_text(tx.clock().now()), id.as_str()],
            )?;
            tx.item_in_table(table, id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))
        })
    }

    /// Dismisses an open inbox item without an answer.
    pub fn dismiss_inbox(&self, id: &InboxId, by: &str) -> Result<(), StoreError> {
        let table = self.inbox_table(id)?;
        self.transaction(|tx| {
            let current = tx
                .item_in_table(table, id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))?;
            if current.state != InboxState::Open {
                return Err(StoreError::Conflict(format!("inbox item {id} is not open")));
            }
            tx.conn.execute(
                &format!(
                    "UPDATE {} SET state = 'dismissed', answer = NULL, answered_by = ?1, answered_at = ?2 WHERE id = ?3",
                    table.name()
                ),
                params![by, ts_to_text(tx.clock().now()), id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Inserts or updates one provider limit row, keyed by provider.
    pub fn upsert_limit(&self, limit: &Limit) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.limits (
                provider, harness, kind, resets_at, detail, since, last_probe_at, probe_fail_streak,
                concurrency_cap, cap_until, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(provider) DO UPDATE SET
               harness = excluded.harness,
               kind = excluded.kind,
               resets_at = excluded.resets_at,
               detail = excluded.detail,
               since = CASE WHEN limits.kind = excluded.kind THEN limits.since ELSE excluded.since END,
               last_probe_at = excluded.last_probe_at,
               probe_fail_streak = excluded.probe_fail_streak,
               concurrency_cap = excluded.concurrency_cap,
               cap_until = excluded.cap_until,
               updated_at = excluded.updated_at",
            params![
                limit.provider,
                limit.harness.as_ref().map(enum_to_text).transpose()?,
                enum_to_text(&limit.kind)?,
                opt_ts_to_text(limit.resets_at),
                limit.detail,
                ts_to_text(limit.since),
                opt_ts_to_text(limit.last_probe_at),
                i64::from(limit.probe_fail_streak),
                limit.concurrency_cap.map(i64::from),
                opt_ts_to_text(limit.cap_until),
                ts_to_text(self.clock().now()),
            ],
        )?;
        Ok(())
    }

    /// Returns a provider's limit row regardless of expiry, when it exists.
    pub fn limit_for(&self, provider: &str) -> Result<Option<Limit>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {LIMIT_COLUMNS} FROM main.limits WHERE provider = ?1"),
                [provider],
                read_limit_row,
            )
            .optional()?;
        raw.map(limit_from_row).transpose()
    }

    /// Removes a provider's limit row, returning whether one existed.
    pub fn clear_limit(&self, provider: &str) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM main.limits WHERE provider = ?1", [provider])?;
        Ok(changed > 0)
    }

    /// Lists limits still in force at `now`: no reset time, or a reset after `now`.
    pub fn active_limits(&self, now: Timestamp) -> Result<Vec<Limit>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {LIMIT_COLUMNS} FROM main.limits WHERE resets_at IS NULL OR resets_at > ?1 ORDER BY provider"
        ))?;
        let rows = statement.query_map([ts_to_text(now)], read_limit_row)?;
        let mut limits = Vec::new();
        for row in rows {
            limits.push(limit_from_row(row?)?);
        }
        Ok(limits)
    }

    /// Records one probe of a limited provider: stamps `last_probe_at` and updates the streak.
    pub fn record_probe(&self, provider: &str, at: Timestamp, ok: bool) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.limits SET last_probe_at = ?1,
                probe_fail_streak = CASE WHEN ?2 THEN 0 ELSE probe_fail_streak + 1 END,
                updated_at = ?3
             WHERE provider = ?4",
            params![ts_to_text(at), i64::from(ok), ts_to_text(self.clock().now()), provider],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("limit", provider));
        }
        Ok(())
    }

    /// Inserts one release row in the bound project.
    pub fn insert_release(&self, release: &Release) -> Result<(), StoreError> {
        self.check_bound(&release.project)?;
        self.conn.execute(
            "INSERT INTO project.releases (
                id, project, env, change_ref, from_commit, to_commit, state, guards_json,
                snapshot_path, rollback_cmd, probe_json, approved_by, rolled_back_by, note,
                created_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                release.id.as_str(),
                release.project.as_str(),
                release.env,
                release.change_ref,
                release.from_commit,
                release.to_commit,
                enum_to_text(&release.state)?,
                json_to_text(&release.guards)?,
                release.snapshot_path,
                release.rollback_cmd,
                json_to_text(&release.probe)?,
                release.approved_by,
                release.rolled_back_by.as_ref().map(ReleaseId::as_str),
                release.note,
                ts_to_text(release.created_at),
                opt_ts_to_text(release.finished_at),
            ],
        )?;
        Ok(())
    }

    /// Replaces a release's mutable fields, keyed by id; `created_at` is kept.
    pub fn update_release(&self, release: &Release) -> Result<(), StoreError> {
        self.check_bound(&release.project)?;
        let changed = self.conn.execute(
            "UPDATE project.releases SET
                project = ?1, env = ?2, change_ref = ?3, from_commit = ?4, to_commit = ?5, state = ?6,
                guards_json = ?7, snapshot_path = ?8, rollback_cmd = ?9, probe_json = ?10, approved_by = ?11,
                rolled_back_by = ?12, note = ?13, finished_at = ?14
             WHERE id = ?15",
            params![
                release.project.as_str(),
                release.env,
                release.change_ref,
                release.from_commit,
                release.to_commit,
                enum_to_text(&release.state)?,
                json_to_text(&release.guards)?,
                release.snapshot_path,
                release.rollback_cmd,
                json_to_text(&release.probe)?,
                release.approved_by,
                release.rolled_back_by.as_ref().map(ReleaseId::as_str),
                release.note,
                opt_ts_to_text(release.finished_at),
                release.id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("release", release.id.as_str()));
        }
        Ok(())
    }

    /// Returns the release called `id`, when it exists in the bound project.
    pub fn release(&self, id: &ReleaseId) -> Result<Option<Release>, StoreError> {
        let bound = self.require_bound()?;
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {RELEASE_COLUMNS} FROM project.releases WHERE id = ?1"),
                [id.as_str()],
                read_release_row,
            )
            .optional()?;
        match raw {
            Some(raw) => {
                let release = release_from_row(raw)?;
                if release.project != bound {
                    return Err(crate::convert::corrupt(
                        "releases.project",
                        format!("row names {} but the handle is bound to {bound}", release.project),
                    ));
                }
                Ok(Some(release))
            }
            None => Ok(None),
        }
    }

    /// Lists a project's releases, newest first, optionally restricted to one environment.
    pub fn list_releases(&self, project: &ProjectName, env: Option<&str>) -> Result<Vec<Release>, StoreError> {
        let bound = self.check_bound(project)?;
        let mut releases = Vec::new();
        match env {
            Some(env) => {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT {RELEASE_COLUMNS} FROM project.releases WHERE project = ?1 AND env = ?2
                     ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = statement.query_map(params![project.as_str(), env], read_release_row)?;
                for row in rows {
                    releases.push(release_from_row(row?)?);
                }
            }
            None => {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT {RELEASE_COLUMNS} FROM project.releases WHERE project = ?1 ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = statement.query_map([project.as_str()], read_release_row)?;
                for row in rows {
                    releases.push(release_from_row(row?)?);
                }
            }
        }
        for release in &releases {
            if release.project != bound {
                return Err(crate::convert::corrupt(
                    "releases.project",
                    format!("row names {} but the handle is bound to {bound}", release.project),
                ));
            }
        }
        Ok(releases)
    }

    /// Returns a `kv` value, when the key exists.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let value = self
            .conn
            .query_row("SELECT value FROM main.kv WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Inserts or replaces a `kv` value, stamping `updated_at` from the clock.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, ts_to_text(self.clock().now())],
        )?;
        Ok(())
    }

    /// Removes a `kv` key, returning whether it existed.
    pub fn kv_delete(&self, key: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute("DELETE FROM main.kv WHERE key = ?1", [key])?;
        Ok(changed > 0)
    }

    /// Lists every `kv` row whose key starts with `prefix`, ordered by key.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT key, value FROM main.kv WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key")?;
        let rows = statement.query_map([prefix], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Enqueues one CLI-to-supervisor request.
    ///
    /// A bound handle writes `req:<project>:<ULID>` unless `kind == "wake"`, which writes
    /// `req:node:<ULID>`; an unbound handle always writes the node key.
    pub fn push_request(&self, kind: &str, payload: &serde_json::Value) -> Result<RequestId, StoreError> {
        let ulid = vowp_protocol::SessionId::new();
        let id = RequestId(ulid.as_str().to_owned());
        let value = serde_json::json!({ "kind": kind, "payload": payload });
        let key = match self.bound_project() {
            Some(project) if kind != "wake" => format!("req:{}:{}", project.as_str(), id.0),
            _ => format!("req:node:{}", id.0),
        };
        self.kv_set(&key, &serde_json::to_string(&value)?)?;
        Ok(id)
    }

    /// Atomically drains the requests this handle may serve and deletes them.
    ///
    /// A bound handle drains its project's keys plus node-wide keys; an unbound handle drains
    /// only node-wide keys.
    pub fn take_requests(&self) -> Result<Vec<Request>, StoreError> {
        let bound = self.bound_project().cloned();
        self.transaction(|tx| {
            let mut statement = tx
                .conn
                .prepare("SELECT key, value, updated_at FROM main.kv WHERE substr(key, 1, 4) = 'req:' ORDER BY key")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut raw_requests = Vec::new();
            for row in rows {
                raw_requests.push(row?);
            }
            drop(statement);

            let mut requests = Vec::with_capacity(raw_requests.len());
            for (key, value, updated_at) in raw_requests {
                let id = match request_id_from_key(&key, bound.as_ref()) {
                    Some(id) => id,
                    None => continue,
                };
                let value: serde_json::Value = serde_json::from_str(&value)?;
                let kind =
                    value
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| StoreError::Corrupt {
                            path: format!("kv:{key}"),
                            line: None,
                            reason: "request has no kind".to_owned(),
                        })?;
                let payload = value.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                requests.push(Request {
                    id: RequestId(id.to_owned()),
                    kind: kind.to_owned(),
                    payload,
                    created_at: ts_from_text(&updated_at, "kv.updated_at")?,
                });
                tx.conn.execute("DELETE FROM main.kv WHERE key = ?1", [&key])?;
            }
            requests.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.0.cmp(&right.id.0))
            });
            Ok(requests)
        })
    }

    /// Returns the table an inbox item lives in.
    fn inbox_table(&self, id: &InboxId) -> Result<InboxTable, StoreError> {
        if inbox_by_id(&self.conn, InboxTable::Node, id)?.is_some() {
            return Ok(InboxTable::Node);
        }
        if self.bound_project().is_some() && inbox_by_id(&self.conn, InboxTable::Project, id)?.is_some() {
            return Ok(InboxTable::Project);
        }
        Err(StoreError::not_found("inbox item", id.as_str()))
    }

    /// Reads one item from a specific table.
    fn item_in_table(&self, table: InboxTable, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
        inbox_by_id(&self.conn, table, id)
    }
}

/// Inserts one inbox row into the chosen table.
fn insert_inbox_row(conn: &rusqlite::Connection, table: InboxTable, item: &InboxItem) -> Result<(), StoreError> {
    conn.execute(
        &format!(
            "INSERT INTO {} (
                id, project, job_id, kind, title, body, options_json, state, answer, answered_by,
                created_at, answered_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            table.name()
        ),
        params![
            item.id.as_str(),
            item.project.as_ref().map(ProjectName::as_str),
            item.job_id.as_ref().map(JobId::as_str),
            enum_to_text(&item.kind)?,
            item.title,
            item.body,
            json_to_text(&item.options)?,
            enum_to_text(&item.state)?,
            item.answer,
            item.answered_by,
            ts_to_text(item.created_at),
            opt_ts_to_text(item.answered_at),
        ],
    )?;
    Ok(())
}

/// Reads one inbox item from a specific table.
fn inbox_by_id(conn: &rusqlite::Connection, table: InboxTable, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
    let raw = conn
        .query_row(
            &format!("SELECT {INBOX_COLUMNS} FROM {} WHERE id = ?1", table.name()),
            [id.as_str()],
            read_inbox_row,
        )
        .optional()?;
    raw.map(inbox_from_row).transpose()
}

/// Returns the request ULID when this key belongs to the handle, else `None`.
fn request_id_from_key<'a>(key: &'a str, bound: Option<&ProjectName>) -> Option<&'a str> {
    if let Some(bound) = bound {
        let project_prefix = format!("req:{}:", bound.as_str());
        if let Some(id) = key.strip_prefix(&project_prefix) {
            return Some(id);
        }
    }
    key.strip_prefix("req:node:")
}
