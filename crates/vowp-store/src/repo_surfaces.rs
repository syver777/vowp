//! Surfaces and leases repository.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.2 and §7.3.
//!
//! The tables live in the global database. A surface is the app or terminal a person is
//! talking through; a lease is the one row that gives it the right to write to a session.
//! `take_lease` replaces the row in one `BEGIN IMMEDIATE` transaction so two surfaces can
//! never both believe they own a session.

use rusqlite::{OptionalExtension, Row, params};
use sha2::{Digest, Sha256};
use vowp_protocol::{
    Lease, NodeName, SessionId, Surface, SurfaceId, SurfaceInfo, SurfaceKind, SurfaceScope, SurfaceState, Timestamp,
};

use crate::convert::{
    corrupt, enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, opt_ts_to_text,
    ts_from_text, ts_to_text,
};
use crate::db::Store;
use crate::error::StoreError;

/// One durable surface token, stored only as a hash plus a display prefix.
///
/// The plaintext token is returned once by the home and is never written here. Comparisons of
/// a presented token against `token_hash` are the caller's job and must be constant-time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceToken {
    /// Token row id (ULID).
    pub id: String,
    /// Surface the token belongs to.
    pub surface_id: SurfaceId,
    /// Lowercase hex SHA-256 of the token.
    pub token_hash: String,
    /// Short display prefix; never enough to reconstruct the token.
    pub prefix: String,
    /// Scopes granted to the token.
    pub scopes: Vec<SurfaceScope>,
    /// When the token was issued.
    pub created_at: Timestamp,
    /// When the token expires; `None` means no expiry.
    pub expires_at: Option<Timestamp>,
    /// When the token was revoked; `None` while it is active.
    pub revoked_at: Option<Timestamp>,
    /// Last successful use.
    pub last_used_at: Option<Timestamp>,
}

/// Home-side display facts for one surface's tokens.
///
/// Only the 12-character display prefix and the expiries are exposed. The token and its
/// SHA-256 hash never leave the store. The agent-local `needs_pair` state is not a home
/// fact and is deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceTokenView {
    /// Display prefix of the newest active token.
    pub prefix: Option<String>,
    /// Expiry of the newest active token.
    pub expires_at: Option<Timestamp>,
    /// Latest expiry among older active tokens, while a rotation overlaps.
    pub overlap_until: Option<Timestamp>,
}

impl SurfaceTokenView {
    /// Builds the view from one surface's token rows, oldest first.
    ///
    /// An active token is unrevoked and either has no expiry or expires after `now`. The
    /// newest active row supplies the prefix and `EXPIRES` value; an older active row
    /// supplies the overlap marker. Rows that are revoked or already expired are ignored.
    pub fn from_tokens(tokens: &[SurfaceToken], now: Timestamp) -> Self {
        let active: Vec<&SurfaceToken> = tokens
            .iter()
            .filter(|token| token.revoked_at.is_none())
            .filter(|token| token.expires_at.is_none_or(|expiry| expiry > now))
            .collect();
        let Some((newest, older)) = active.split_last() else {
            return Self {
                prefix: None,
                expires_at: None,
                overlap_until: None,
            };
        };
        Self {
            prefix: Some(newest.prefix.clone()),
            expires_at: newest.expires_at,
            overlap_until: older.iter().filter_map(|token| token.expires_at).max(),
        }
    }
}

/// Column list shared by every `surfaces` query.
const SURFACE_COLUMNS: &str = "id, kind, node, holder, app, cwd, metadata_json, created_at, last_seen_at";

/// Column list shared by every `leases` query.
const LEASE_COLUMNS: &str = "session_id, surface_id, surface_kind, node, holder, token_hash, acquired_at, heartbeat_at";

/// Column list used to read a surface with its v0.4 state.
const SURFACE_INFO_COLUMNS: &str =
    "id, kind, node, holder, app, cwd, metadata_json, state, created_at, last_seen_at, revoked_at";

/// Column list shared by every `surface_tokens` query.
const SURFACE_TOKEN_COLUMNS: &str =
    "id, surface_id, token_hash, prefix, scopes_json, created_at, expires_at, revoked_at, last_used_at";

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
    token_hash: String,
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
        token_hash: row.get(5)?,
        acquired_at: row.get(6)?,
        heartbeat_at: row.get(7)?,
    })
}

/// Converts a raw `leases` row into a protocol [`Lease`].
///
/// `Lease::token` carries the stored lowercase hex SHA-256, never the plaintext token.
/// The plaintext exists only in the grant returned once to the caller.
fn lease_from_row(raw: LeaseRow) -> Result<Lease, StoreError> {
    Ok(Lease {
        session_id: SessionId::parse(&raw.session_id)?,
        surface_id: raw.surface_id.map(SurfaceId),
        surface_kind: surface_kind_from_text(&raw.surface_kind, "leases.surface_kind")?,
        node: NodeName::parse(&raw.node)?,
        holder: raw.holder,
        token: raw.token_hash,
        acquired_at: ts_from_text(&raw.acquired_at, "leases.acquired_at")?,
        heartbeat_at: ts_from_text(&raw.heartbeat_at, "leases.heartbeat_at")?,
    })
}

/// Returns the lowercase hex SHA-256 of a plaintext lease token.
///
/// The database stores only this value. The 52-character token itself is returned once in
/// `LeaseGrant` and is never written to the database or a log.
pub fn hash_lease_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Raw values of one `surfaces` row with its v0.4 lifecycle columns.
struct SurfaceInfoRow {
    id: String,
    kind: String,
    node: String,
    holder: String,
    app: Option<String>,
    cwd: Option<String>,
    metadata: String,
    state: String,
    created_at: String,
    last_seen_at: String,
    revoked_at: Option<String>,
}

/// Raw values of one `surface_tokens` row before protocol conversion.
struct SurfaceTokenRow {
    id: String,
    surface_id: String,
    token_hash: String,
    prefix: String,
    scopes: String,
    created_at: String,
    expires_at: Option<String>,
    revoked_at: Option<String>,
    last_used_at: Option<String>,
}

/// Reads the columns of a `surfaces` row with lifecycle state.
fn read_surface_info_row(row: &Row<'_>) -> rusqlite::Result<SurfaceInfoRow> {
    Ok(SurfaceInfoRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        node: row.get(2)?,
        holder: row.get(3)?,
        app: row.get(4)?,
        cwd: row.get(5)?,
        metadata: row.get(6)?,
        state: row.get(7)?,
        created_at: row.get(8)?,
        last_seen_at: row.get(9)?,
        revoked_at: row.get(10)?,
    })
}

/// Converts a raw `surfaces` row into a [`SurfaceInfo`].
fn surface_info_from_row(raw: SurfaceInfoRow) -> Result<SurfaceInfo, StoreError> {
    Ok(SurfaceInfo {
        id: SurfaceId(raw.id),
        kind: surface_kind_from_text(&raw.kind, "surfaces.kind")?,
        node: NodeName::parse(&raw.node)?,
        holder: raw.holder,
        app: raw.app,
        cwd: raw.cwd,
        metadata: json_from_text(&raw.metadata, "surfaces.metadata_json")?,
        state: enum_from_text(&raw.state, "surfaces.state")?,
        created_at: ts_from_text(&raw.created_at, "surfaces.created_at")?,
        last_seen_at: ts_from_text(&raw.last_seen_at, "surfaces.last_seen_at")?,
        revoked_at: opt_ts_from_text(raw.revoked_at.as_deref(), "surfaces.revoked_at")?,
    })
}

/// Reads the columns of a `surface_tokens` row.
fn read_surface_token_row(row: &Row<'_>) -> rusqlite::Result<SurfaceTokenRow> {
    Ok(SurfaceTokenRow {
        id: row.get(0)?,
        surface_id: row.get(1)?,
        token_hash: row.get(2)?,
        prefix: row.get(3)?,
        scopes: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        revoked_at: row.get(7)?,
        last_used_at: row.get(8)?,
    })
}

/// Converts a raw `surface_tokens` row into a [`SurfaceToken`].
fn surface_token_from_row(raw: SurfaceTokenRow) -> Result<SurfaceToken, StoreError> {
    Ok(SurfaceToken {
        id: raw.id,
        surface_id: SurfaceId(raw.surface_id),
        token_hash: raw.token_hash,
        prefix: raw.prefix,
        scopes: json_from_text(&raw.scopes, "surface_tokens.scopes_json")?,
        created_at: ts_from_text(&raw.created_at, "surface_tokens.created_at")?,
        expires_at: opt_ts_from_text(raw.expires_at.as_deref(), "surface_tokens.expires_at")?,
        revoked_at: opt_ts_from_text(raw.revoked_at.as_deref(), "surface_tokens.revoked_at")?,
        last_used_at: opt_ts_from_text(raw.last_used_at.as_deref(), "surface_tokens.last_used_at")?,
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
        SurfaceKind::Agent => "agent",
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
        "agent" => Ok(SurfaceKind::Agent),
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
    /// `lease.token` is the plaintext token; only its SHA-256 hash is written. Returns the
    /// previous row, or `None` when the session had no lease. The write is a single
    /// `BEGIN IMMEDIATE` transaction, so concurrent callers cannot both see an empty row
    /// and both insert.
    pub fn take_lease(&self, lease: &Lease) -> Result<Option<Lease>, StoreError> {
        let token_hash = hash_lease_token(&lease.token);
        self.transaction(|tx| {
            if tx.session(&lease.session_id)?.is_none() {
                return Err(StoreError::not_found("session", lease.session_id.as_str()));
            }
            let previous = tx.lease(&lease.session_id)?;
            tx.conn.execute(
                "INSERT INTO main.leases (
                    session_id, surface_id, surface_kind, node, holder, token_hash, acquired_at, heartbeat_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(session_id) DO UPDATE SET
                   surface_id = excluded.surface_id,
                   surface_kind = excluded.surface_kind,
                   node = excluded.node,
                   holder = excluded.holder,
                   token_hash = excluded.token_hash,
                   acquired_at = excluded.acquired_at,
                   heartbeat_at = excluded.heartbeat_at",
                params![
                    lease.session_id.as_str(),
                    lease.surface_id.as_ref().map(|id| id.0.as_str()),
                    surface_kind_to_text(lease.surface_kind),
                    lease.node.as_str(),
                    lease.holder,
                    token_hash,
                    ts_to_text(lease.acquired_at),
                    ts_to_text(lease.heartbeat_at),
                ],
            )?;
            Ok(previous)
        })
    }

    /// Stamps the lease's heartbeat when the plaintext `token` still holds it.
    ///
    /// The presented token is hashed and compared with `token_hash`. Returns false when
    /// the session has no lease or another surface holds it.
    pub fn heartbeat_lease(&self, session: &SessionId, token: &str, at: Timestamp) -> Result<bool, StoreError> {
        let token_hash = hash_lease_token(token);
        let changed = self.conn.execute(
            "UPDATE main.leases SET heartbeat_at = ?1 WHERE session_id = ?2 AND token_hash = ?3",
            params![ts_to_text(at), session.as_str(), token_hash],
        )?;
        Ok(changed > 0)
    }

    /// Deletes the lease when the plaintext `token` still holds it; returns whether it was
    /// deleted.
    pub fn release_lease(&self, session: &SessionId, token: &str) -> Result<bool, StoreError> {
        self.release_lease_hash(session, &hash_lease_token(token))
    }

    /// Deletes the lease when its stored hash still matches; returns whether it was deleted.
    ///
    /// `token_hash` is the stored value read from [`Store::lease`], not a plaintext token.
    /// Callers that hold only the stored row (for example a token file that is already
    /// gone) use this method rather than `release_lease`, which hashes a plaintext token.
    pub fn release_lease_hash(&self, session: &SessionId, token_hash: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.leases WHERE session_id = ?1 AND token_hash = ?2",
            params![session.as_str(), token_hash],
        )?;
        Ok(changed > 0)
    }

    /// Deletes a lease only when its stored hash still matches and it is stale at `before`.
    ///
    /// `token_hash` is the stored value read from [`Store::lease`], not a plaintext token.
    /// The heartbeat predicate is part of the same DELETE, so a heartbeat committed
    /// between `stale_leases` and this call keeps the lease. The sweep must use this
    /// method rather than `release_lease`, which hashes a plaintext token.
    pub fn release_stale_lease(
        &self,
        session: &SessionId,
        token_hash: &str,
        before: Timestamp,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.leases
             WHERE session_id = ?1 AND token_hash = ?2 AND heartbeat_at < ?3",
            params![session.as_str(), token_hash, ts_to_text(before)],
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

    /// Stamps the heartbeat of the lease identified by its session, surface and node.
    ///
    /// A v0.4 local surface identifies its lease by these three values on every mirrored
    /// frame, so it never has to carry the 52-character plaintext token. Returns false when
    /// no matching row exists.
    pub fn heartbeat_surface_lease(
        &self,
        session: &SessionId,
        surface: &SurfaceId,
        node: &NodeName,
        at: Timestamp,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.leases SET heartbeat_at = ?1
             WHERE session_id = ?2 AND surface_id = ?3 AND node = ?4",
            params![ts_to_text(at), session.as_str(), surface.0.as_str(), node.as_str()],
        )?;
        Ok(changed > 0)
    }

    /// Deletes the lease identified by its session, surface and node.
    ///
    /// This is the release path of a local surface that identifies itself by its paired
    /// surface rather than by the plaintext lease token. Returns false when no matching row
    /// exists.
    pub fn release_surface_lease(
        &self,
        session: &SessionId,
        surface: &SurfaceId,
        node: &NodeName,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM main.leases WHERE session_id = ?1 AND surface_id = ?2 AND node = ?3",
            params![session.as_str(), surface.0.as_str(), node.as_str()],
        )?;
        Ok(changed > 0)
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

    /// Inserts one surface in the `pending` state, with no revocation time.
    ///
    /// The v0.2 [`Surface`] has no state field, so pairing uses this constructor rather than
    /// [`Store::upsert_surface`], which leaves the state at its `attached` default.
    pub fn insert_pending_surface(&self, surface: &Surface) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.surfaces (
                id, kind, node, holder, app, cwd, metadata_json, state, revoked_at, created_at, last_seen_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', NULL, ?8, ?9)",
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

    /// Returns the surface called `id` with its lifecycle state, when it exists.
    pub fn surface_by_id(&self, id: &SurfaceId) -> Result<Option<SurfaceInfo>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {SURFACE_INFO_COLUMNS} FROM main.surfaces WHERE id = ?1"),
                [id.0.as_str()],
                read_surface_info_row,
            )
            .optional()?;
        raw.map(surface_info_from_row).transpose()
    }

    /// Sets the lifecycle state of `id`, recording `at` when it becomes revoked.
    ///
    /// A `pending` or `attached` transition clears `revoked_at`; `revoked` sets it to `at`.
    /// Returns [`StoreError::NotFound`] when the surface does not exist.
    pub fn set_surface_state(&self, id: &SurfaceId, state: SurfaceState, at: Timestamp) -> Result<(), StoreError> {
        let revoked_at = if state == SurfaceState::Revoked {
            Some(ts_to_text(at))
        } else {
            None
        };
        let changed = self.conn.execute(
            "UPDATE main.surfaces SET state = ?1, revoked_at = ?2 WHERE id = ?3",
            params![enum_to_text(&state)?, revoked_at, id.0.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("surface", id.0.as_str()));
        }
        Ok(())
    }

    /// Inserts one surface token; the unique hash index rejects a duplicate token.
    pub fn insert_surface_token(&self, token: &SurfaceToken) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.surface_tokens (
                id, surface_id, token_hash, prefix, scopes_json, created_at, expires_at, revoked_at, last_used_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                token.id,
                token.surface_id.0.as_str(),
                token.token_hash,
                token.prefix,
                json_to_text(&token.scopes)?,
                ts_to_text(token.created_at),
                opt_ts_to_text(token.expires_at),
                opt_ts_to_text(token.revoked_at),
                opt_ts_to_text(token.last_used_at),
            ],
        )?;
        Ok(())
    }

    /// Returns the token row with `token_hash`, when one exists.
    ///
    /// The caller compares the presented token against `token_hash` in constant time; this
    /// method only performs the indexed lookup.
    pub fn surface_token_by_hash(&self, token_hash: &str) -> Result<Option<SurfaceToken>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {SURFACE_TOKEN_COLUMNS} FROM main.surface_tokens WHERE token_hash = ?1"),
                [token_hash],
                read_surface_token_row,
            )
            .optional()?;
        raw.map(surface_token_from_row).transpose()
    }

    /// Lists one surface's tokens, oldest first.
    ///
    /// Tokens created in the same clock tick are ordered by insertion, so a rotation's
    /// replacement is always the newest row even when its random ULID sorts lower.
    pub fn list_surface_tokens(&self, surface: &SurfaceId) -> Result<Vec<SurfaceToken>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {SURFACE_TOKEN_COLUMNS} FROM main.surface_tokens
             WHERE surface_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = statement.query_map([surface.0.as_str()], read_surface_token_row)?;
        let mut tokens = Vec::new();
        for row in rows {
            tokens.push(surface_token_from_row(row?)?);
        }
        Ok(tokens)
    }

    /// Returns the display prefix, expiry and overlap for one surface's tokens.
    ///
    /// This is the read data behind `surface ls`; it never reads or returns the plaintext
    /// token or its hash. A surface with no active token gets `None` for every field.
    pub fn surface_token_view(&self, surface: &SurfaceId, now: Timestamp) -> Result<SurfaceTokenView, StoreError> {
        let tokens = self.list_surface_tokens(surface)?;
        Ok(SurfaceTokenView::from_tokens(&tokens, now))
    }

    /// Revokes one token at `at`; returns false when it was already revoked or is unknown.
    pub fn revoke_surface_token(&self, id: &str, at: Timestamp) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.surface_tokens SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![ts_to_text(at), id],
        )?;
        Ok(changed > 0)
    }

    /// Records a successful use of one token at `at`; returns false when the id is unknown.
    pub fn touch_surface_token(&self, id: &str, at: Timestamp) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.surface_tokens SET last_used_at = ?1 WHERE id = ?2",
            params![ts_to_text(at), id],
        )?;
        Ok(changed > 0)
    }

    /// Shortens one active token's expiry to `expires_at` without ever extending it.
    ///
    /// Returns false when the id is unknown, the token is revoked, or its current expiry is
    /// already at or before `expires_at`. The predicate is part of the same `UPDATE`, so a
    /// concurrent rotation can neither extend a token nor shorten a revoked one.
    pub fn shorten_surface_token_expiry(&self, id: &str, expires_at: Timestamp) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.surface_tokens SET expires_at = ?1
             WHERE id = ?2 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?1)",
            params![ts_to_text(expires_at), id],
        )?;
        Ok(changed > 0)
    }
}
