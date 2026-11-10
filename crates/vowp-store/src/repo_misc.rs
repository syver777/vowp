//! Repository methods for `inbox`, `limits`, `releases`, `kv` and the CLI request queue.
//! Spec: `docs/design/crates/vowp-store.md` §3.4, `docs/design/types.md` §9–10 and
//! `docs/design/crates/vowp-core.md` §2.
//!
//! Requests are rows in `kv` (`req:<ULID>`); the store treats `kind` and `payload` as opaque
//! JSON so `vowp-core` remains the only owner of the request vocabulary.

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

/// Column list shared by every `inbox` query.
const INBOX_COLUMNS: &str =
    "id, project, job_id, kind, title, body, options_json, state, answer, answered_by, created_at, answered_at";

/// Column list shared by every `limits` query.
const LIMIT_COLUMNS: &str = "provider, harness, kind, resets_at, detail, since, last_probe_at, probe_fail_streak, \
     concurrency_cap, cap_until";

/// Column list shared by every `releases` query.
const RELEASE_COLUMNS: &str = "id, project, env, change_ref, from_commit, to_commit, state, guards_json, \
     snapshot_path, rollback_cmd, probe_json, approved_by, rolled_back_by, note, created_at, finished_at";

/// Raw values of one `inbox` row before protocol conversion.
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

/// Reads the textual columns of an `inbox` row.
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

/// Converts a raw `inbox` row into a protocol [`InboxItem`].
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
    /// Inserts one inbox item.
    pub fn insert_inbox(&self, item: &InboxItem) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO inbox (
                id, project, job_id, kind, title, body, options_json, state, answer, answered_by,
                created_at, answered_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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

    /// Returns the inbox item called `id`, when it exists.
    pub fn inbox_item(&self, id: &InboxId) -> Result<Option<InboxItem>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {INBOX_COLUMNS} FROM inbox WHERE id = ?1"),
                [id.as_str()],
                read_inbox_row,
            )
            .optional()?;
        raw.map(inbox_from_row).transpose()
    }

    /// Lists inbox items, oldest first, optionally restricted to one state.
    pub fn list_inbox(&self, state: Option<InboxState>) -> Result<Vec<InboxItem>, StoreError> {
        let mut items = Vec::new();
        match state {
            Some(state) => {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT {INBOX_COLUMNS} FROM inbox WHERE state = ?1 ORDER BY created_at, id"
                ))?;
                let rows = statement.query_map([enum_to_text(&state)?], read_inbox_row)?;
                for row in rows {
                    items.push(inbox_from_row(row?)?);
                }
            }
            None => {
                let mut statement = self
                    .conn
                    .prepare(&format!("SELECT {INBOX_COLUMNS} FROM inbox ORDER BY created_at, id"))?;
                let rows = statement.query_map([], read_inbox_row)?;
                for row in rows {
                    items.push(inbox_from_row(row?)?);
                }
            }
        }
        Ok(items)
    }

    /// Answers an open inbox item, returning the updated row.
    ///
    /// A second answer is a [`StoreError::Conflict`]; the job is not touched.
    pub fn answer_inbox(&self, id: &InboxId, answer: &str, by: &str) -> Result<InboxItem, StoreError> {
        self.transaction(|tx| {
            let current = tx
                .inbox_item(id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))?;
            if current.state != InboxState::Open {
                return Err(StoreError::Conflict(format!("inbox item {id} is not open")));
            }
            tx.conn.execute(
                "UPDATE inbox SET state = 'answered', answer = ?1, answered_by = ?2, answered_at = ?3 WHERE id = ?4",
                params![answer, by, ts_to_text(tx.clock().now()), id.as_str()],
            )?;
            tx.inbox_item(id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))
        })
    }

    /// Dismisses an open inbox item without an answer.
    ///
    /// The row keeps the schema's `(state = 'open') = (answered_at IS NULL)` invariant by
    /// storing `answer = NULL` and an `answered_at`. A non-open item is a [`StoreError::Conflict`].
    pub fn dismiss_inbox(&self, id: &InboxId, by: &str) -> Result<(), StoreError> {
        self.transaction(|tx| {
            let current = tx
                .inbox_item(id)?
                .ok_or_else(|| StoreError::not_found("inbox item", id.as_str()))?;
            if current.state != InboxState::Open {
                return Err(StoreError::Conflict(format!("inbox item {id} is not open")));
            }
            tx.conn.execute(
                "UPDATE inbox SET state = 'dismissed', answer = NULL, answered_by = ?1, answered_at = ?2 WHERE id = ?3",
                params![by, ts_to_text(tx.clock().now()), id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Inserts or updates one provider limit row, keyed by provider.
    ///
    /// An existing row keeps its `since` when the new limit has the same kind; a different
    /// kind starts a new observation window.
    pub fn upsert_limit(&self, limit: &Limit) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO limits (
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
                &format!("SELECT {LIMIT_COLUMNS} FROM limits WHERE provider = ?1"),
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
            .execute("DELETE FROM limits WHERE provider = ?1", [provider])?;
        Ok(changed > 0)
    }

    /// Lists limits still in force at `now`: no reset time, or a reset after `now`.
    pub fn active_limits(&self, now: Timestamp) -> Result<Vec<Limit>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {LIMIT_COLUMNS} FROM limits WHERE resets_at IS NULL OR resets_at > ?1 ORDER BY provider"
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
            "UPDATE limits SET last_probe_at = ?1,
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

    /// Inserts one release row.
    pub fn insert_release(&self, release: &Release) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO releases (
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
        let changed = self.conn.execute(
            "UPDATE releases SET
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

    /// Returns the release called `id`, when it exists.
    pub fn release(&self, id: &ReleaseId) -> Result<Option<Release>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {RELEASE_COLUMNS} FROM releases WHERE id = ?1"),
                [id.as_str()],
                read_release_row,
            )
            .optional()?;
        raw.map(release_from_row).transpose()
    }

    /// Lists a project's releases, newest first, optionally restricted to one environment.
    pub fn list_releases(&self, project: &ProjectName, env: Option<&str>) -> Result<Vec<Release>, StoreError> {
        let mut releases = Vec::new();
        match env {
            Some(env) => {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT {RELEASE_COLUMNS} FROM releases WHERE project = ?1 AND env = ?2
                     ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = statement.query_map(params![project.as_str(), env], read_release_row)?;
                for row in rows {
                    releases.push(release_from_row(row?)?);
                }
            }
            None => {
                let mut statement = self.conn.prepare(&format!(
                    "SELECT {RELEASE_COLUMNS} FROM releases WHERE project = ?1 ORDER BY created_at DESC, id DESC"
                ))?;
                let rows = statement.query_map([project.as_str()], read_release_row)?;
                for row in rows {
                    releases.push(release_from_row(row?)?);
                }
            }
        }
        Ok(releases)
    }

    /// Returns a `kv` value, when the key exists.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let value = self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Inserts or replaces a `kv` value, stamping `updated_at` from the clock.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, ts_to_text(self.clock().now())],
        )?;
        Ok(())
    }

    /// Removes a `kv` key, returning whether it existed.
    pub fn kv_delete(&self, key: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(changed > 0)
    }

    /// Lists every `kv` row whose key starts with `prefix`, ordered by key.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key")?;
        let rows = statement.query_map([prefix], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Enqueues one CLI-to-supervisor request as `kv` key `req:<ULID>`.
    pub fn push_request(&self, kind: &str, payload: &serde_json::Value) -> Result<RequestId, StoreError> {
        // The protocol id generator is process-wide monotonic, so same-millisecond requests
        // keep their push order and `take_requests` can return them in that order.
        let ulid = vowp_protocol::SessionId::new();
        let id = RequestId(ulid.as_str().to_owned());
        let value = serde_json::json!({ "kind": kind, "payload": payload });
        self.kv_set(&format!("req:{}", id.0), &serde_json::to_string(&value)?)?;
        Ok(id)
    }

    /// Atomically drains every `req:*` row in id order and deletes it.
    pub fn take_requests(&self) -> Result<Vec<Request>, StoreError> {
        self.transaction(|tx| {
            let mut statement = tx
                .conn
                .prepare("SELECT key, value, updated_at FROM kv WHERE substr(key, 1, 4) = 'req:' ORDER BY key")?;
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
                let id = key.strip_prefix("req:").ok_or_else(|| StoreError::Corrupt {
                    path: format!("kv:{key}"),
                    line: None,
                    reason: "request key has no req: prefix".to_owned(),
                })?;
                requests.push(Request {
                    id: RequestId(id.to_owned()),
                    kind: kind.to_owned(),
                    payload,
                    created_at: ts_from_text(&updated_at, "kv.updated_at")?,
                });
                tx.conn.execute("DELETE FROM kv WHERE key = ?1", [&key])?;
            }
            Ok(requests)
        })
    }
}
