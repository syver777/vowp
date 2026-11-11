//! Surfaces and leases repository.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.2 and §7.3.
//!
//! The tables live in the global database. A surface is the app or terminal a person is
//! talking through; a lease is the one row that gives it the right to write to a session.
//! `take_lease` replaces the row in one `BEGIN IMMEDIATE` transaction so two surfaces can
//! never both believe they own a session.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{Lease, NodeName, SessionId, Surface, SurfaceId, SurfaceKind, Timestamp};

use crate::convert::{corrupt, json_from_text, json_to_text, ts_from_text, ts_to_text};
use crate::db::Store;
use crate::error::StoreError;

/// Column list shared by every `surfaces` query.
const SURFACE_COLUMNS: &str = "id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at";

/// Column list shared by every `leases` query.
const LEASE_COLUMNS: &str = "session_id, surface_id, surface_kind, node, holder, token, acquired_at, heartbeat_at";

/// Raw values of one `surfaces` row before protocol conversion.
struct SurfaceRow {
    id: String,
    kind: String,
    node: String,
    holder: String,
    app: Option<String>,
    cwd: Option<String>,
    metadata: String,
    created_at: String,
    last_seen_at: String,
}

/// Raw values of one `leases` row before protocol conversion.
struct LeaseRow {
    session_id: String,
    surface_id: Option<String>,
    surface_kind: String,
    node: String,
    holder: String,
    token: String,
    acquired_at: String,
    heartbeat_at: String,
}

/// Reads the columns of a `surfaces` row.
fn read_surface_row(row: &Row<'_>) -> rusqlite::Result<SurfaceRow> {
    Ok(SurfaceRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        node: row.get(2)?,
        holder: row.get(3)?,
        app: row.get(4)?,
        cwd: row.get(5)?,
        metadata: row.get(6)?,
        created_at: row.get(7)?,
        last_seen_at: row.get(8)?,
    })
}

/// Converts a raw `surfaces` row into a protocol [`Surface`].
fn surface_from_row(raw: SurfaceRow) -> Result<Surface, StoreError> {
    Ok(Surface {
        id: SurfaceId(raw.id),
        kind: surface_kind_from_text(&raw.kind, "surfaces.kind")?,
        node: NodeName::parse(&raw.node)?,
        holder: raw.holder,
        app: raw.app,
        cwd: raw.cwd,
        metadata: json_from_text(&raw.metadata, "surfaces.metadata_json")?,
        created_at: ts_from_text(&raw.created_at, "surfaces.created_at")?,
        last_seen_at: ts_from_text(&raw.last_seen_at, "surfaces.last_seen_at")?,
    })
}

/// Reads the columns of a `leases` row.
fn read_lease_row(row: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        session_id: row.get(0)?,
        surface_id: row.get(1)?,
        surface_kind: row.get(2)?,
        node: row.get(3)?,
        holder: row.get(4)?,
        token: row.get(5)?,
        acquired_at: row.get(6)?,
        heartbeat_at: row.get(7)?,
    })
}

/// Converts a raw `leases` row into a protocol [`Lease`].
fn lease_from_row(raw: LeaseRow) -> Result<Lease, StoreError> {
    Ok(Lease {
        session_id: SessionId::parse(&raw.session_id)?,
        surface_id: raw.surface_id.map(SurfaceId),
        surface_kind: surface_kind_from_text(&raw.surface_kind, "leases.surface_kind")?,
        node: NodeName::parse(&raw.node)?,
        holder: raw.holder,
        token: raw.token,
        acquired_at: ts_from_text(&raw.acquired_at, "leases.acquired_at")?,
        heartbeat_at: ts_from_text(&raw.heartbeat_at, "leases.heartbeat_at")?,
    })
}

/// Returns the database wire name of a surface kind.
///
/// The protocol serialises `SurfaceKind` with `snake_case` (`claude_code`), while the
/// schema and the spec use the hyphenated app name (`claude-code`). The database is the
/// schema's vocabulary, so keep the mapping here instead of using the generic enum helper.
fn surface_kind_to_text(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Chat => "chat",
        SurfaceKind::Opencode => "opencode",
        SurfaceKind::ClaudeCode => "claude-code",
        SurfaceKind::Mcp => "mcp",
        SurfaceKind::Mirror => "mirror",
        SurfaceKind::Api => "api",
    }
}

/// Parses a surface kind from its database wire name.
fn surface_kind_from_text(text: &str, what: &str) -> Result<SurfaceKind, StoreError> {
    match text {
        "chat" => Ok(SurfaceKind::Chat),
        "opencode" => Ok(SurfaceKind::Opencode),
        "claude-code" | "claude_code" => Ok(SurfaceKind::ClaudeCode),
        "mcp" => Ok(SurfaceKind::Mcp),
        "mirror" => Ok(SurfaceKind::Mirror),
        "api" => Ok(SurfaceKind::Api),
        other => Err(corrupt(what, format!("unknown surface kind {other:?}"))),
    }
}

impl Store {
    /// Inserts or updates one surface, keyed by id.
    ///
    /// `created_at` is kept from the first registration; every later call refreshes the
    /// mutable fields and `last_seen_at`.
    pub fn upsert_surface(&self, surface: &Surface) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.surfaces (
                id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
               kind = excluded.kind,
               node = excluded.node,
               holder = excluded.holder,
               app = excluded.app,
               cwd = excluded.cwd,
               metadata_json = excluded.metadata_json,
               last_seen_at = excluded.last_seen_at",
            params![
                surface.id.0.as_str(),
                surface_kind_to_text(surface.kind),
                surface.node.as_str(),
                surface.holder,
                surface.app,
                surface.cwd,
                json_to_text(&surface.metadata)?,
                ts_to_text(surface.created_at),
                ts_to_text(surface.last_seen_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the surface called `id`, when it exists.
    pub fn surface(&self, id: &SurfaceId) -> Result<Option<Surface>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {SURFACE_COLUMNS} FROM main.surfaces WHERE id = ?1"),
                [id.0.as_str()],
                read_surface_row,
            )
            .optional()?;
        raw.map(surface_from_row).transpose()
    }

    /// Deletes one surface; returns whether a row existed.
    pub fn delete_surface(&self, id: &SurfaceId) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute("DELETE FROM main.surfaces WHERE id = ?1", [id.0.as_str()])?;
        Ok(changed > 0)
    }

    /// Returns the live lease of `session`, when one exists.
    pub fn lease(&self, session: &SessionId) -> Result<Option<Lease>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {LEASE_COLUMNS} FROM main.leases WHERE session_id = ?1"),
                [session.as_str()],
                read_lease_row,
            )
            .optional()?;
        raw.map(lease_from_row).transpose()
    }

    /// Replaces the session's lease with `lease` in one transaction.
    ///
    /// Returns the previous holder, or `None` when the session had no lease. The write is
    /// a single `BEGIN IMMEDIATE` transaction, so concurrent callers cannot both see an
    /// empty row and both insert.
    pub fn take_lease(&self, lease: &Lease) -> Result<Option<Lease>, StoreError> {
        self.transaction(|tx| {
            if tx.session(&lease.session_id)?.is_none() {
                return Err(StoreError::not_found("session", lease.session_id.as_str()));
            }
            let previous = tx.lease(&lease.session_id)?;
            tx.conn.execute(
                "INSERT INTO main.leases (
                    session_id, surface_id, surface_kind, node, holder, token, acquired_at, heartbeat_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(session_id) DO UPDATE SET
                   surface_id = excluded.surface_id,
                   surface_kind = excluded.surface_kind,
                   node = excluded.node,
                   holder = excluded.holder,
                   token = excluded.token,
                   acquired_at = excluded.acquired_at,
                   heartbeat_at = excluded.heartbeat_at",
                params![
                    lease.session_id.as_str(),
                    lease.surface_id.as_ref().map(|id| id.0.as_str()),
                    surface_kind_to_text(lease.surface_kind),
                    lease.node.as_str(),
                    lease.holder,
                    lease.token,
                    ts_to_text(lease.acquired_at),
                    ts_to_text(lease.heartbeat_at),
                ],
            )?;
            Ok(previous)
        })
    }

    /// Stamps the lease's heartbeat when `token` still holds it.
    ///
    /// Returns false when the session has no lease or another surface holds it.
    pub fn heartbeat_lease(&self, session: &SessionId, token: &str, at: Timestamp) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.leases SET heartbeat_at = ?1 WHERE session_id = ?2 AND token = ?3",
            params![ts_to_text(at), session.as_str(), token],
        )?;
        Ok(changed > 0)
    }

    /// Deletes the lease when `token` still holds it; returns whether it was deleted.
    pub fn release_lease(&self, session: &SessionId, token: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.leases WHERE session_id = ?1 AND token = ?2",
            params![session.as_str(), token],
        )?;
        Ok(changed > 0)
    }

    /// Deletes a lease only when it is still stale at `before`.
    ///
    /// The heartbeat predicate is part of the same DELETE, so a heartbeat committed
    /// between `stale_leases` and this call keeps the lease. The sweep must use this
    /// method rather than `release_lease`, which matches only `session_id` and `token`.
    pub fn release_stale_lease(&self, session: &SessionId, token: &str, before: Timestamp) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.leases
             WHERE session_id = ?1 AND token = ?2 AND heartbeat_at < ?3",
            params![session.as_str(), token, ts_to_text(before)],
        )?;
        Ok(changed > 0)
    }

    /// Lists leases whose heartbeat is older than `before`, oldest first.
    pub fn stale_leases(&self, before: Timestamp) -> Result<Vec<Lease>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {LEASE_COLUMNS} FROM main.leases WHERE heartbeat_at < ?1
             ORDER BY heartbeat_at, session_id"
        ))?;
        let rows = statement.query_map([ts_to_text(before)], read_lease_row)?;
        let mut leases = Vec::new();
        for row in rows {
            leases.push(lease_from_row(row?)?);
        }
        Ok(leases)
    }

    /// Lists every lease, ordered by session id.
    pub fn list_leases(&self) -> Result<Vec<Lease>, StoreError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {LEASE_COLUMNS} FROM main.leases ORDER BY session_id"))?;
        let rows = statement.query_map([], read_lease_row)?;
        let mut leases = Vec::new();
        for row in rows {
            leases.push(lease_from_row(row?)?);
        }
        Ok(leases)
    }
}
