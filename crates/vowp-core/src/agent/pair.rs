//! Surface pairing: one-time codes, scoped surface tokens and their verification.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.1, §4.3, §4.6, §8.3 and §11.1.
//!
//! The home creates a `pending` surface and a one-time code. Only the code's SHA-256 hash
//! is stored, and the plaintext is returned once. Redeeming the code issues a 30-day
//! `vowp_surf_` token scoped to one session and its project; the home stores only the
//! token's SHA-256 hash, a 12-character display prefix and the scopes. No code or token is
//! logged, echoed or written by this module.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::time::Duration;

use serde::Serialize;
use vowp_protocol::{
    AgentFrame, LedgerEntry, LedgerKind, NodeName, NodeStatus, ProjectName, SessionId, Surface, SurfaceId, SurfaceInfo,
    SurfaceKind, SurfaceScope, SurfaceState, Timestamp,
};
use vowp_store::repo_surfaces::SurfaceTokenView;
use vowp_store::{EnrollmentCode, EnrollmentKind, Store, StoreError, SurfaceToken};

use super::code;
use super::spool::AgentFrameSpool;
use crate::ledger::Ledger;
use crate::{CoreError, Deps};

/// Lifetime of a one-time surface pairing code.
pub const SURFACE_CODE_TTL: Duration = code::ENROLLMENT_CODE_TTL;

/// Default lifetime of an issued surface token (30 days).
pub const SURFACE_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long an old surface token keeps working after a rotation (10 minutes).
pub const SURFACE_TOKEN_OVERLAP: Duration = Duration::from_secs(10 * 60);

/// Longest grace a rotation may grant the old token (24 hours).
pub const SURFACE_TOKEN_MAX_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Scheme carried by every surface token.
const TOKEN_SCHEME: &str = "vowp_surf_";

/// Number of random bytes in a surface token.
const TOKEN_RANDOM_BYTES: usize = 32;

/// Number of characters of the random part kept as the display prefix.
const PREFIX_CHARS: usize = 12;

/// Metadata key that binds a pending surface to the session it was paired for.
const SESSION_METADATA_KEY: &str = "session";

/// A fresh one-time surface pairing code.
#[derive(Clone)]
pub struct NewSurfaceCode {
    /// Plaintext code; shown once by the home and never stored.
    pub code: String,
    /// Lowercase hex SHA-256 of [`Self::code`].
    pub code_hash: String,
    /// Instant at which the code stops redeeming.
    pub expires_at: Timestamp,
}

impl fmt::Debug for NewSurfaceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewSurfaceCode")
            .field("code", &"[REDACTED]")
            .field("code_hash", &self.code_hash)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A pending surface plus the plaintext code shown once by `vowp surface pair`.
#[derive(Clone)]
pub struct PendingSurface {
    /// The pending surface row.
    pub surface: SurfaceInfo,
    /// Plaintext pairing code; shown once and never stored.
    pub code: String,
    /// Lowercase hex SHA-256 of [`Self::code`].
    pub code_hash: String,
    /// Instant at which the code stops redeeming.
    pub expires_at: Timestamp,
}

impl fmt::Debug for PendingSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSurface")
            .field("surface", &self.surface)
            .field("code", &"[REDACTED]")
            .field("code_hash", &self.code_hash)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The result of redeeming one surface pairing code.
#[derive(Clone)]
pub struct IssuedSurface {
    /// Surface the token belongs to.
    pub surface_id: SurfaceId,
    /// Plaintext token, returned to the agent exactly once.
    pub token: String,
    /// Scopes granted to the token.
    pub scopes: Vec<SurfaceScope>,
    /// When the token expires.
    pub expires_at: Timestamp,
}

impl fmt::Debug for IssuedSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedSurface")
            .field("surface_id", &self.surface_id)
            .field("token", &"[REDACTED]")
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A verified surface token and the surface row it belongs to.
#[derive(Debug, Clone)]
pub struct VerifiedSurface {
    /// Surface the token belongs to, read from its current row.
    pub surface: SurfaceInfo,
    /// Stored token row, with `last_used_at` updated.
    pub token: SurfaceToken,
}

/// The result of rotating one surface token.
#[derive(Clone)]
pub struct RotatedSurface {
    /// Surface row at rotation time.
    pub surface: SurfaceInfo,
    /// Plaintext replacement token, returned once and never stored.
    pub token: String,
    /// Display prefix of [`Self::token`]; safe to print.
    pub prefix: String,
    /// Scopes copied from the surface's previous token.
    pub scopes: Vec<SurfaceScope>,
    /// When the replacement token expires.
    pub expires_at: Timestamp,
    /// Earliest expiry among the surface's previous active tokens.
    ///
    /// This is the end of the overlap window: after it an agent that has not received the
    /// replacement token must re-pair.
    pub old_expires_at: Timestamp,
    /// Node the surface is bound to.
    pub node: NodeName,
}

impl fmt::Debug for RotatedSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RotatedSurface")
            .field("surface", &self.surface)
            .field("token", &"[REDACTED]")
            .field("prefix", &self.prefix)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .field("old_expires_at", &self.old_expires_at)
            .field("node", &self.node)
            .finish()
    }
}

/// The `--grace` and `--wait` values for one rotation.
///
/// The CLI parses the flags; this is the home policy the flags select. The default is the
/// shipped ten-minute overlap and no wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// How long the old token keeps working; capped at [`SURFACE_TOKEN_MAX_GRACE`].
    pub grace: Duration,
    /// How long to wait for the node to drain the queued frame; `None` does not wait.
    pub wait: Option<Duration>,
}

impl RotationPolicy {
    /// Returns the grace actually used: the request capped by the design maximum.
    pub fn effective_grace(&self) -> Duration {
        self.grace.min(SURFACE_TOKEN_MAX_GRACE)
    }
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            grace: SURFACE_TOKEN_OVERLAP,
            wait: None,
        }
    }
}

/// The result of one rotation and its delivery attempt.
#[derive(Debug, Clone)]
pub struct SurfaceRotation {
    /// Rotation result, including the replacement prefix and both expiries.
    pub rotated: RotatedSurface,
    /// When the replacement frame was queued.
    pub queued_at: Timestamp,
    /// Effective expiry of the old tokens (the overlap deadline).
    pub old_expires_at: Timestamp,
    /// Whether the queued frame was drained before the wait ended.
    pub delivered: bool,
}

/// The home-side read row for one `surface ls` entry.
///
/// It carries the display prefix and expiries, never the token or its hash, and it has no
/// agent-local `needs_pair` field. A queued rotation is reported as `pending` together
/// with its queue time and delivery deadline.
#[derive(Debug, Clone, Serialize)]
pub struct SurfaceListRow {
    /// Surface row with its lifecycle state.
    pub surface: SurfaceInfo,
    /// Scopes granted by the surface's tokens.
    pub scopes: Vec<SurfaceScope>,
    /// Display prefix of the newest active token.
    pub prefix: Option<String>,
    /// Expiry of the newest active token.
    pub expires_at: Option<Timestamp>,
    /// Latest expiry among older active tokens, while a rotation overlaps.
    pub overlap_until: Option<Timestamp>,
    /// Whether a rotation frame is still queued for the node.
    pub pending: bool,
    /// When the queued rotation was written.
    pub queued_at: Option<Timestamp>,
    /// When the queued replacement token expires (the delivery deadline).
    pub delivery_deadline: Option<Timestamp>,
}

/// Creates a `pending` surface and its one-time pairing code.
///
/// The node must exist and not be revoked, and the session must exist. The surface stores
/// the session id in its metadata so [`issue_surface_token`] can refuse a code presented
/// for another session. The plaintext code is returned once; only its hash is stored.
pub fn create_pending_surface(
    store: &Store,
    node: &NodeName,
    session: &SessionId,
    kind: SurfaceKind,
    app: Option<&str>,
    now: Timestamp,
) -> Result<PendingSurface, CoreError> {
    if let Some(app) = app {
        validate_app(app)?;
    }
    let Some(card) = store.node_card(node, now)? else {
        return Err(CoreError::NotFound(format!("node {node}")));
    };
    if card.status == NodeStatus::Revoked {
        return Err(CoreError::Refused(format!("node {node} is revoked")));
    }
    store.require_session(session)?;

    let (surface, info) = new_pending_surface(node, session, kind, app, now);
    let generated = store
        .transaction(|tx| {
            tx.insert_pending_surface(&surface)?;
            insert_surface_code(tx, node, &surface.id, now)
        })
        .map_err(map_pair_error)?;
    Ok(PendingSurface {
        surface: info,
        code: generated.code,
        code_hash: generated.code_hash,
        expires_at: generated.expires_at,
    })
}

/// Builds the pending surface row and its read model.
fn new_pending_surface(
    node: &NodeName,
    session: &SessionId,
    kind: SurfaceKind,
    app: Option<&str>,
    now: Timestamp,
) -> (Surface, SurfaceInfo) {
    let surface = Surface {
        id: SurfaceId::new(),
        kind,
        node: node.clone(),
        holder: app.unwrap_or_else(|| node.as_str()).to_owned(),
        app: app.map(str::to_owned),
        cwd: None,
        metadata: serde_json::json!({ "session": session.as_str() }),
        created_at: now,
        last_seen_at: now,
    };
    let info = SurfaceInfo {
        id: surface.id.clone(),
        kind: surface.kind,
        node: surface.node.clone(),
        holder: surface.holder.clone(),
        app: surface.app.clone(),
        cwd: surface.cwd.clone(),
        metadata: surface.metadata.clone(),
        state: SurfaceState::Pending,
        created_at: surface.created_at,
        last_seen_at: surface.last_seen_at,
        revoked_at: None,
    };
    (surface, info)
}

/// Creates a one-time pairing code for an existing `pending` surface.
///
/// The code is a 52-character [`vowp_protocol::new_lease_token`] value with a ten-minute
/// TTL. Only its SHA-256 hash is written to `enrollment_codes` with `kind = surface`.
pub fn new_surface_code(
    store: &Store,
    node: &NodeName,
    surface_id: &SurfaceId,
    now: Timestamp,
) -> Result<NewSurfaceCode, CoreError> {
    store
        .transaction(|tx| insert_surface_code(tx, node, surface_id, now))
        .map_err(map_pair_error)
}

/// Redeems one pairing code and issues a scoped surface token.
///
/// The code, surface, session, app and active node are validated in one immediate
/// transaction: the code is consumed once, the token row is inserted and the surface
/// becomes `attached`. A refusal rolls the whole transaction back, so a code presented
/// with the wrong node, session or app stays redeemable. The plaintext token is returned
/// exactly once; only its hash, prefix and scopes are stored.
pub fn issue_surface_token(
    store: &Store,
    node: &NodeName,
    surface_id: &SurfaceId,
    session: &SessionId,
    app: &str,
    code: &str,
    now: Timestamp,
) -> Result<IssuedSurface, CoreError> {
    if !code::has_code_shape(code) {
        return Err(CoreError::Invalid("pairing code must be 52 characters".to_owned()));
    }
    validate_app(app)?;

    let code_hash = code::hash_secret(code);
    let (token, token_hash, prefix) = new_token()?;
    let token_id = ulid::Ulid::generate().to_string();
    let expires_at = now.plus(SURFACE_TOKEN_TTL);

    store
        .transaction(|tx| {
            let Some(stored) = tx.consume_enrollment_code(&code_hash, now)? else {
                return Err(StoreError::Conflict(
                    "pairing code is invalid, expired or already used".to_owned(),
                ));
            };
            if stored.kind != EnrollmentKind::Surface {
                return Err(StoreError::InvalidInput(
                    "pairing code is not a surface code".to_owned(),
                ));
            }
            if stored.surface_id.as_deref() != Some(surface_id.0.as_str()) {
                return Err(StoreError::Conflict("pairing code is for another surface".to_owned()));
            }
            if stored.node != *node {
                return Err(StoreError::Conflict("pairing code is for another node".to_owned()));
            }
            let Some(card) = tx.node_card(node, now)? else {
                return Err(StoreError::NotFound {
                    what: "node",
                    id: node.as_str().to_owned(),
                });
            };
            if card.status != NodeStatus::Active {
                return Err(StoreError::Conflict("node is not active".to_owned()));
            }

            let Some(surface) = tx.surface_by_id(surface_id)? else {
                return Err(StoreError::NotFound {
                    what: "surface",
                    id: surface_id.0.clone(),
                });
            };
            if surface.node != *node {
                return Err(StoreError::Conflict("surface belongs to another node".to_owned()));
            }
            if surface.state != SurfaceState::Pending {
                return Err(StoreError::Conflict("surface is not pending".to_owned()));
            }
            match surface.metadata.get(SESSION_METADATA_KEY) {
                Some(serde_json::Value::String(bound)) if bound == session.as_str() => {}
                Some(_) => {
                    return Err(StoreError::Conflict("pairing code is for another session".to_owned()));
                }
                None => return Err(StoreError::Conflict("surface is not bound to a session".to_owned())),
            }
            if let Some(surface_app) = &surface.app
                && surface_app != app
            {
                return Err(StoreError::Conflict("surface is paired for another app".to_owned()));
            }

            let session_row = tx.require_session(session)?;
            let scopes = default_scopes(session, session_row.project.as_ref());
            tx.insert_surface_token(&SurfaceToken {
                id: token_id.clone(),
                surface_id: surface_id.clone(),
                token_hash: token_hash.clone(),
                prefix: prefix.clone(),
                scopes: scopes.clone(),
                created_at: now,
                expires_at: Some(expires_at),
                revoked_at: None,
                last_used_at: None,
            })?;
            tx.set_surface_state(surface_id, SurfaceState::Attached, now)?;
            Ok(IssuedSurface {
                surface_id: surface_id.clone(),
                token: token.clone(),
                scopes,
                expires_at,
            })
        })
        .map_err(map_pair_error)
}

/// Rotates one attached surface's token, keeping the old tokens for the grace window.
///
/// The replacement token has the same scopes as the surface's most recent token and the
/// normal 30-day TTL. Every other active token is shortened to `now + grace`, never
/// extended; an older token that already expires sooner keeps its earlier expiry. `grace`
/// is capped at [`SURFACE_TOKEN_MAX_GRACE`]; pass [`SURFACE_TOKEN_OVERLAP`] for the default.
/// The plaintext replacement token is returned once; only its hash, prefix and scopes are
/// stored, and one `node_ledger` note records the rotation without the token. A missing
/// surface is [`CoreError::NotFound`]; a pending or revoked surface or a node that is not
/// active is a typed refusal.
pub fn rotate_surface_token(
    store: &Store,
    deps: &Deps,
    surface_id: &SurfaceId,
    grace: Duration,
    now: Timestamp,
) -> Result<RotatedSurface, CoreError> {
    let (token, token_hash, prefix) = new_token()?;
    let token_id = ulid::Ulid::generate().to_string();
    let expires_at = now.plus(SURFACE_TOKEN_TTL);
    let overlap_expires_at = now.plus(grace.min(SURFACE_TOKEN_MAX_GRACE));

    let rotated = store
        .transaction(|tx| {
            let Some(surface) = tx.surface_by_id(surface_id)? else {
                return Err(StoreError::NotFound {
                    what: "surface",
                    id: surface_id.0.clone(),
                });
            };
            match surface.state {
                SurfaceState::Attached => {}
                SurfaceState::Pending => {
                    return Err(StoreError::Conflict(
                        "surface is pending; pair it before rotating its token".to_owned(),
                    ));
                }
                SurfaceState::Revoked => return Err(StoreError::Conflict("surface was revoked".to_owned())),
            }
            let Some(card) = tx.node_card(&surface.node, now)? else {
                return Err(StoreError::NotFound {
                    what: "node",
                    id: surface.node.as_str().to_owned(),
                });
            };
            if card.status != NodeStatus::Active {
                return Err(StoreError::Conflict(format!("node {} is not active", surface.node)));
            }

            let tokens = tx.list_surface_tokens(surface_id)?;
            let scopes = match tokens
                .iter()
                .rev()
                .find(|token| token.revoked_at.is_none())
                .or_else(|| tokens.last())
            {
                Some(token) => token.scopes.clone(),
                None => scopes_from_surface(tx, &surface)?,
            };
            let mut old_expires_at = overlap_expires_at;
            for old in &tokens {
                if old.revoked_at.is_some() {
                    continue;
                }
                let Some(old_expiry) = old.expires_at else {
                    tx.shorten_surface_token_expiry(&old.id, overlap_expires_at)?;
                    continue;
                };
                if old_expiry <= now {
                    continue;
                }
                old_expires_at = old_expires_at.min(old_expiry);
                tx.shorten_surface_token_expiry(&old.id, overlap_expires_at)?;
            }

            tx.insert_surface_token(&SurfaceToken {
                id: token_id.clone(),
                surface_id: surface_id.clone(),
                token_hash: token_hash.clone(),
                prefix: prefix.clone(),
                scopes: scopes.clone(),
                created_at: now,
                expires_at: Some(expires_at),
                revoked_at: None,
                last_used_at: None,
            })?;

            let node = surface.node.clone();
            Ok(RotatedSurface {
                surface,
                token: token.clone(),
                prefix,
                scopes,
                expires_at,
                old_expires_at,
                node,
            })
        })
        .map_err(map_pair_error)?;

    record_surface_rotation(store, deps, &rotated, now)?;
    Ok(rotated)
}

/// Rotates one surface, queues its replacement and optionally waits for delivery.
///
/// This is the library half of `surface rotate --grace/--wait`: the CLI parses the values
/// and this function applies the cap, writes the frame envelope with `queued_at` and the
/// effective `overlap_until`, and reports whether the frame was drained. An offline node
/// is not an error; the frame stays queued for its next authenticated connection.
pub fn rotate_surface_with_policy(
    store: &Store,
    deps: &Deps,
    surface_id: &SurfaceId,
    policy: RotationPolicy,
    now: Timestamp,
) -> Result<SurfaceRotation, CoreError> {
    let rotated = rotate_surface_token(store, deps, surface_id, policy.effective_grace(), now)?;
    let frame = AgentFrame::SurfaceIssued {
        surface_id: surface_id.clone(),
        token: rotated.token.clone(),
        scopes: rotated.scopes.clone(),
        expires_at: rotated.expires_at,
    };
    let spool = AgentFrameSpool::new(deps.paths.root());
    spool.queue_rotation(
        &rotated.node,
        surface_id.0.as_str(),
        &frame,
        now,
        rotated.old_expires_at,
    )?;
    let delivered = match policy.wait {
        Some(timeout) => spool.wait(&rotated.node, surface_id.0.as_str(), timeout)?,
        None => spool.read(&rotated.node, surface_id.0.as_str())?.is_none(),
    };
    let old_expires_at = rotated.old_expires_at;
    Ok(SurfaceRotation {
        rotated,
        queued_at: now,
        old_expires_at,
        delivered,
    })
}

/// Verifies a presented surface token at `now` and records its use.
///
/// The comparison against the stored hash is constant-time. A malformed, unknown,
/// revoked, expired or non-attached token is a typed refusal. Callers must still compare
/// [`VerifiedSurface::surface`]'s node with the connection's node before authorizing a
/// write.
pub fn verify_surface_token(store: &Store, token: &str, now: Timestamp) -> Result<VerifiedSurface, CoreError> {
    if !is_surface_token(token) {
        return Err(CoreError::Refused("surface token is invalid".to_owned()));
    }
    let token_hash = code::hash_secret(token);
    let Some(stored) = store.surface_token_by_hash(&token_hash)? else {
        return Err(CoreError::Refused("surface token is invalid".to_owned()));
    };
    if !constant_time_eq(stored.token_hash.as_bytes(), token_hash.as_bytes()) {
        return Err(CoreError::Refused("surface token is invalid".to_owned()));
    }
    if stored.revoked_at.is_some() {
        return Err(CoreError::Refused("surface token was revoked".to_owned()));
    }
    if stored.expires_at.is_some_and(|expires_at| expires_at <= now) {
        return Err(CoreError::Refused("surface token has expired".to_owned()));
    }
    let Some(surface) = store.surface_by_id(&stored.surface_id)? else {
        return Err(CoreError::NotFound(format!("surface {}", stored.surface_id)));
    };
    match surface.state {
        SurfaceState::Attached => {}
        SurfaceState::Pending => return Err(CoreError::Refused("surface is not attached".to_owned())),
        SurfaceState::Revoked => return Err(CoreError::Refused("surface was revoked".to_owned())),
    }
    if !store.touch_surface_token(&stored.id, now)? {
        return Err(CoreError::Refused("surface token is invalid".to_owned()));
    }
    Ok(VerifiedSurface {
        surface,
        token: SurfaceToken {
            last_used_at: Some(now),
            ..stored
        },
    })
}

/// Revokes one surface token row; an unknown or already-revoked id is a typed refusal.
///
/// This is the token-row primitive. The audited `surface revoke` path is
/// [`revoke_surface`], which writes the single `node_ledger` note for the surface.
pub fn revoke_surface_token(store: &Store, id: &str, now: Timestamp) -> Result<(), CoreError> {
    if id.is_empty() {
        return Err(CoreError::Invalid("surface token id is empty".to_owned()));
    }
    if store.revoke_surface_token(id, now)? {
        Ok(())
    } else {
        Err(CoreError::Refused(format!(
            "surface token {id} is unknown or already revoked"
        )))
    }
}

/// Revokes every active token of `surface_id`, marks the surface revoked and records one note.
///
/// Returns the number of token rows changed. The next surface frame is refused by
/// [`verify_surface_token`] and by the gateway's `SurfaceRevoked` answer. A queued rotation
/// for the surface is removed so it cannot be delivered after the revoke. The ledger note
/// has the action, surface id, node, display prefix and expiry, never the token or a path.
pub fn revoke_surface(store: &Store, deps: &Deps, surface_id: &SurfaceId, now: Timestamp) -> Result<u64, CoreError> {
    let Some(surface) = store.surface_by_id(surface_id)? else {
        return Err(CoreError::NotFound(format!("surface {surface_id}")));
    };
    let tokens = store.list_surface_tokens(surface_id)?;
    let mut revoked = 0_u64;
    for token in &tokens {
        if token.revoked_at.is_none() && store.revoke_surface_token(&token.id, now)? {
            revoked += 1;
        }
    }
    store.set_surface_state(surface_id, SurfaceState::Revoked, now)?;
    record_surface_revocation(store, deps, &surface, &tokens, now)?;
    AgentFrameSpool::new(deps.paths.root()).complete(&surface.node, surface_id.0.as_str())?;
    Ok(revoked)
}

/// Revokes `replace` and creates one replacement `pending` surface in a single transaction.
///
/// The old surface must belong to the same node and session and must not already be
/// revoked. Its queued rotation frame is removed after the transaction, so the revoked
/// surface cannot receive a stale replacement token; the old token is refused immediately.
/// The new surface is pending with a fresh one-time code. No token is issued or queued until
/// the agent redeems that code.
#[allow(clippy::too_many_arguments)]
pub fn replace_pending_surface(
    store: &Store,
    deps: &Deps,
    replace: &SurfaceId,
    node: &NodeName,
    session: &SessionId,
    kind: SurfaceKind,
    app: Option<&str>,
    now: Timestamp,
) -> Result<PendingSurface, CoreError> {
    if let Some(app) = app {
        validate_app(app)?;
    }
    let (surface, info) = new_pending_surface(node, session, kind, app, now);
    let (old, old_tokens, generated) = store
        .transaction(|tx| {
            let Some(card) = tx.node_card(node, now)? else {
                return Err(StoreError::NotFound {
                    what: "node",
                    id: node.as_str().to_owned(),
                });
            };
            if card.status == NodeStatus::Revoked {
                return Err(StoreError::Conflict(format!("node {node} is revoked")));
            }
            if card.status != NodeStatus::Active {
                return Err(StoreError::Conflict(format!("node {node} is not active")));
            }
            tx.require_session(session)?;

            let Some(old) = tx.surface_by_id(replace)? else {
                return Err(StoreError::NotFound {
                    what: "surface",
                    id: replace.0.clone(),
                });
            };
            if old.node != *node {
                return Err(StoreError::Conflict("surface belongs to another node".to_owned()));
            }
            if old.state == SurfaceState::Revoked {
                return Err(StoreError::Conflict("surface was revoked".to_owned()));
            }
            match old.metadata.get(SESSION_METADATA_KEY) {
                Some(serde_json::Value::String(bound)) if bound == session.as_str() => {}
                Some(_) => {
                    return Err(StoreError::Conflict("surface is bound to another session".to_owned()));
                }
                None => return Err(StoreError::Conflict("surface is not bound to a session".to_owned())),
            }

            let tokens = tx.list_surface_tokens(replace)?;
            for token in &tokens {
                if token.revoked_at.is_none() {
                    tx.revoke_surface_token(&token.id, now)?;
                }
            }
            tx.set_surface_state(replace, SurfaceState::Revoked, now)?;
            tx.insert_pending_surface(&surface)?;
            let generated = insert_surface_code(tx, node, &surface.id, now)?;
            Ok((old, tokens, generated))
        })
        .map_err(map_pair_error)?;

    record_surface_revocation(store, deps, &old, &old_tokens, now)?;
    AgentFrameSpool::new(deps.paths.root()).complete(node, replace.0.as_str())?;
    Ok(PendingSurface {
        surface: info,
        code: generated.code,
        code_hash: generated.code_hash,
        expires_at: generated.expires_at,
    })
}

/// Builds the home-side read row for one `surface ls` entry.
///
/// The row carries the display prefix and expiries from [`Store::surface_token_view`] and
/// the pending rotation times from the frame envelope. It never carries the token or its
/// hash, and the agent-local `needs_pair` state is not part of the home document.
pub fn surface_list_row(
    store: &Store,
    deps: &Deps,
    surface_id: &SurfaceId,
    now: Timestamp,
) -> Result<Option<SurfaceListRow>, CoreError> {
    let Some(surface) = store.surface_by_id(surface_id)? else {
        return Ok(None);
    };
    let tokens = store.list_surface_tokens(surface_id)?;
    let view = SurfaceTokenView::from_tokens(&tokens, now);
    let mut scopes: Vec<SurfaceScope> = Vec::new();
    for scope in tokens.iter().flat_map(|token| token.scopes.iter()) {
        if !scopes.contains(scope) {
            scopes.push(scope.clone());
        }
    }
    let queued = AgentFrameSpool::new(deps.paths.root())
        .read(&surface.node, surface_id.0.as_str())?
        .filter(|queued| match &queued.frame {
            AgentFrame::SurfaceIssued {
                surface_id: queued_surface,
                ..
            } => queued_surface == surface_id,
            _ => false,
        });
    let (pending, queued_at, delivery_deadline) = match queued {
        Some(queued) => (true, queued.queued_at, queued.delivery_deadline()),
        None => (false, None, None),
    };
    Ok(Some(SurfaceListRow {
        surface,
        scopes,
        prefix: view.prefix,
        expires_at: view.expires_at,
        overlap_until: view.overlap_until,
        pending,
        queued_at,
        delivery_deadline,
    }))
}

/// Appends the one `node_ledger` note for a rotation.
fn record_surface_rotation(
    store: &Store,
    deps: &Deps,
    rotated: &RotatedSurface,
    now: Timestamp,
) -> Result<(), CoreError> {
    let entry = LedgerEntry {
        id: None,
        ts: now,
        kind: LedgerKind::Note,
        project: None,
        job_id: None,
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("surface {} rotated", rotated.surface.id),
        detail: serde_json::json!({
            "action": "rotate",
            "surface": rotated.surface.id.0.as_str(),
            "node": rotated.node.as_str(),
            "prefix": rotated.prefix,
            "expires_at": rotated.expires_at.as_rfc3339(),
            "old_expires_at": rotated.old_expires_at.as_rfc3339(),
        }),
    };
    Ledger::new(deps.clone()).record(store, entry)?;
    Ok(())
}

/// Appends the one `node_ledger` note for a surface revoke.
fn record_surface_revocation(
    store: &Store,
    deps: &Deps,
    surface: &SurfaceInfo,
    tokens: &[SurfaceToken],
    now: Timestamp,
) -> Result<(), CoreError> {
    let view = SurfaceTokenView::from_tokens(tokens, now);
    let entry = LedgerEntry {
        id: None,
        ts: now,
        kind: LedgerKind::Note,
        project: None,
        job_id: None,
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("surface {} revoked", surface.id),
        detail: serde_json::json!({
            "action": "revoke",
            "surface": surface.id.0.as_str(),
            "node": surface.node.as_str(),
            "prefix": view.prefix,
            "expires_at": view.expires_at.map(|expiry| expiry.as_rfc3339()),
        }),
    };
    Ledger::new(deps.clone()).record(store, entry)?;
    Ok(())
}

/// Builds the default token scopes for one session.
fn default_scopes(session: &SessionId, project: Option<&ProjectName>) -> Vec<SurfaceScope> {
    let mut scopes = vec![
        SurfaceScope::new(format!("session.read:{}", session.as_str())),
        SurfaceScope::new(format!("session.write:{}", session.as_str())),
    ];
    if let Some(project) = project {
        scopes.push(SurfaceScope::new(format!("project.read:{}", project.as_str())));
    }
    scopes.push(SurfaceScope::new("fleet.read"));
    scopes
}

/// Rebuilds the default scopes of a surface that has no token left to copy.
///
/// A surface is created with its session in metadata and pairing attaches it with the
/// default scopes, so this covers an attached surface whose tokens were all revoked or
/// pruned.
fn scopes_from_surface(store: &Store, surface: &SurfaceInfo) -> Result<Vec<SurfaceScope>, StoreError> {
    let Some(serde_json::Value::String(bound)) = surface.metadata.get(SESSION_METADATA_KEY) else {
        return Err(StoreError::Conflict("surface is not bound to a session".to_owned()));
    };
    let session = SessionId::parse(bound)?;
    let row = store.require_session(&session)?;
    Ok(default_scopes(&session, row.project.as_ref()))
}

/// Inserts one surface-kind code for a pending surface.
fn insert_surface_code(
    store: &Store,
    node: &NodeName,
    surface_id: &SurfaceId,
    now: Timestamp,
) -> Result<NewSurfaceCode, StoreError> {
    let Some(surface) = store.surface_by_id(surface_id)? else {
        return Err(StoreError::NotFound {
            what: "surface",
            id: surface_id.0.clone(),
        });
    };
    if surface.node != *node {
        return Err(StoreError::Conflict("surface belongs to another node".to_owned()));
    }
    if surface.state != SurfaceState::Pending {
        return Err(StoreError::Conflict("surface is not pending".to_owned()));
    }
    let generated = code::new_enrollment_code(now);
    store.insert_enrollment_code(&EnrollmentCode {
        code_hash: generated.code_hash.clone(),
        node: node.clone(),
        kind: EnrollmentKind::Surface,
        surface_id: Some(surface_id.0.clone()),
        expires_at: generated.expires_at,
        used_at: None,
        created_by: "owner".to_owned(),
        created_at: now,
    })?;
    Ok(NewSurfaceCode {
        code: generated.code,
        code_hash: generated.code_hash,
        expires_at: generated.expires_at,
    })
}

/// Maps the store errors this module raises onto the core vocabulary.
///
/// Inside the pairing transaction a conflict is a policy refusal; a missing row is a
/// not-found error; an invalid code kind is invalid input. Every other store failure is
/// passed through unchanged so the caller can close the connection.
fn map_pair_error(error: StoreError) -> CoreError {
    match error {
        StoreError::Conflict(message) => CoreError::Refused(message),
        StoreError::NotFound { what, id } => CoreError::NotFound(format!("{what} {id}")),
        StoreError::InvalidInput(message) => CoreError::Invalid(message),
        other => CoreError::Store(other),
    }
}

/// Returns a fresh token, its hash and its display prefix.
fn new_token() -> Result<(String, String, String), CoreError> {
    let bytes = random_bytes::<TOKEN_RANDOM_BYTES>()?;
    let encoded = base64url(&bytes);
    let token = format!("{TOKEN_SCHEME}{encoded}");
    let token_hash = code::hash_secret(&token);
    let prefix = encoded.chars().take(PREFIX_CHARS).collect();
    Ok((token, token_hash, prefix))
}

/// Reads `N` cryptographically random bytes from `/dev/urandom`.
fn random_bytes<const N: usize>() -> Result<[u8; N], CoreError> {
    let mut bytes = [0_u8; N];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|source| CoreError::Io {
            path: "/dev/urandom".to_owned(),
            source,
        })?;
    Ok(bytes)
}

/// Returns whether `token` has the `vowp_surf_` scheme and 43 base64url characters.
fn is_surface_token(token: &str) -> bool {
    let Some(encoded) = token.strip_prefix(TOKEN_SCHEME) else {
        return false;
    };
    encoded.len() == 43
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Encodes bytes as unpadded base64url.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // `chunks(3)` never yields an empty chunk.
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        encoded.push(ALPHABET[((first >> 2) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        }
    }
    encoded
}

/// Compares two byte strings without exiting early on the first difference.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Refuses an empty or control-character app name.
fn validate_app(app: &str) -> Result<(), CoreError> {
    if app.is_empty() || app.chars().count() > 64 || app.chars().any(char::is_control) {
        return Err(CoreError::Invalid(
            "app must be a printable name of at most 64 characters".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;
    use vowp_protocol::{
        FakeClock, MemoryLog, ProjectName, Session, SessionId, SessionRole, SessionState, SurfaceKind, SurfaceState,
        Timestamp, VowpConfig,
    };
    use vowp_tmux::fake::FakeWindows;

    use super::*;
    use crate::agent::code::hash_code;
    use crate::{AdapterRegistry, CommandRunner, FixedRng, SystemGit, SystemRunner};

    const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const OTHER_SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";

    fn ts() -> Timestamp {
        Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
    }

    fn node(name: &str) -> NodeName {
        NodeName::parse(name).expect("node")
    }

    fn project(name: &str) -> ProjectName {
        ProjectName::parse(name).expect("project")
    }

    fn store() -> Store {
        Store::open_memory().expect("store")
    }

    /// Builds injected dependencies on a temporary home for the ledger and spool paths.
    fn test_deps() -> (TempDir, Deps) {
        let root = tempfile::tempdir().expect("temp home");
        let cmd: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
        let deps = Deps {
            paths: vowp_store::NodePaths::new(root.path()),
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: node("home-1"),
            clock: Arc::new(FakeClock::at(ts())),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::clone(&cmd))),
            cmd,
            rng: Arc::new(FixedRng(0)),
        };
        (root, deps)
    }

    fn seed_node(store: &Store, node: &NodeName, now: Timestamp) {
        store.insert_pending_node(node, &[], now).expect("node");
        store
            .set_node_status(node, NodeStatus::Active, "", now)
            .expect("active node");
    }

    fn seed_session(store: &Store, node: &NodeName, id: &str, name: &str, now: Timestamp) -> SessionId {
        let id = SessionId::parse(id).expect("session id");
        store
            .upsert_session(&Session {
                id: id.clone(),
                project: Some(project(name)),
                job_id: None,
                role: Some(SessionRole::User),
                title: "surface pairing".to_owned(),
                parent_session_id: None,
                node: node.clone(),
                cwd: "/tmp/vowp".to_owned(),
                state: SessionState::Idle,
                executor: None,
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: now,
                updated_at: now,
            })
            .expect("session");
        id
    }

    fn seed_pending(store: &Store, node: &NodeName, session: &SessionId, app: &str, now: Timestamp) -> PendingSurface {
        create_pending_surface(store, node, session, SurfaceKind::Opencode, Some(app), now).expect("pending surface")
    }

    #[test]
    fn surface_pair_issues_a_scoped_token_and_stores_only_its_hash() {
        let store = store();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);
        let pending = seed_pending(&store, &node, &session, "opencode", now);
        assert_eq!(pending.surface.state, SurfaceState::Pending);

        let issued = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");

        assert!(issued.token.starts_with(TOKEN_SCHEME));
        assert_eq!(issued.token.len(), TOKEN_SCHEME.len() + 43);
        assert_eq!(
            issued.scopes,
            vec![
                SurfaceScope::new(format!("session.read:{SESSION}")),
                SurfaceScope::new(format!("session.write:{SESSION}")),
                SurfaceScope::new("project.read:vowp"),
                SurfaceScope::new("fleet.read"),
            ]
        );
        for scope in &issued.scopes {
            assert!(!scope.as_str().starts_with("node."));
            assert!(!scope.as_str().starts_with("policy."));
            assert!(!scope.as_str().starts_with("secret."));
        }
        assert_eq!(issued.expires_at, now.plus(SURFACE_TOKEN_TTL));

        let token_hash = hash_code(&issued.token);
        let stored = store
            .surface_token_by_hash(&token_hash)
            .expect("hash lookup")
            .expect("token row");
        assert_eq!(stored.token_hash, token_hash);
        assert_ne!(stored.token_hash, issued.token);
        assert_eq!(stored.token_hash.len(), 64);
        assert!(
            store
                .surface_token_by_hash(&issued.token)
                .expect("plaintext lookup")
                .is_none(),
            "the plaintext token is not a stored hash"
        );
        let random_part = issued.token.strip_prefix(TOKEN_SCHEME).expect("scheme");
        assert_eq!(
            stored.prefix,
            random_part.chars().take(PREFIX_CHARS).collect::<String>()
        );
        assert_eq!(stored.prefix.len(), PREFIX_CHARS);
        assert_eq!(stored.surface_id, pending.surface.id);
        assert_eq!(stored.scopes, issued.scopes);
        assert_eq!(stored.created_at, now);
        assert_eq!(stored.expires_at, Some(now.plus(SURFACE_TOKEN_TTL)));
        assert!(stored.revoked_at.is_none());
        assert!(stored.last_used_at.is_none());

        let tokens = store.list_surface_tokens(&pending.surface.id).expect("tokens");
        assert_eq!(tokens.len(), 1);
        for token in &tokens {
            assert_ne!(token.token_hash, issued.token);
            assert!(!token.prefix.starts_with(TOKEN_SCHEME));
            assert!(!token.token_hash.contains(&issued.token));
        }

        let surface = store
            .surface_by_id(&pending.surface.id)
            .expect("surface lookup")
            .expect("surface row");
        assert_eq!(surface.state, SurfaceState::Attached);
        assert!(
            store
                .consume_enrollment_code(&hash_code(&pending.code), now)
                .expect("code lookup")
                .is_none(),
            "the pairing code is single-use"
        );

        let debug = format!("{issued:?}");
        assert!(!debug.contains(&issued.token), "Debug must not carry the token");
        let debug = format!("{pending:?}");
        assert!(!debug.contains(&pending.code), "Debug must not carry the code");
    }

    #[test]
    fn surface_pair_refuses_expired_or_used_code() {
        let store = store();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);

        let expired = seed_pending(&store, &node, &session, "opencode", now);
        let later = now.plus(Duration::from_secs(601));
        let error = issue_surface_token(
            &store,
            &node,
            &expired.surface.id,
            &session,
            "opencode",
            &expired.code,
            later,
        )
        .expect_err("expired code");
        assert!(matches!(error, CoreError::Refused(_)));
        assert!(!error.to_string().contains(&expired.code));
        assert!(
            store
                .list_surface_tokens(&expired.surface.id)
                .expect("tokens")
                .is_empty()
        );
        assert_eq!(
            store
                .surface_by_id(&expired.surface.id)
                .expect("surface lookup")
                .expect("surface row")
                .state,
            SurfaceState::Pending
        );

        let used = seed_pending(&store, &node, &session, "opencode", later);
        let first = issue_surface_token(&store, &node, &used.surface.id, &session, "opencode", &used.code, later)
            .expect("first use");
        let error = issue_surface_token(&store, &node, &used.surface.id, &session, "opencode", &used.code, later)
            .expect_err("replayed code");
        assert!(matches!(error, CoreError::Refused(_)));
        assert!(!error.to_string().contains(&used.code));
        assert_eq!(store.list_surface_tokens(&used.surface.id).expect("tokens").len(), 1);
        assert!(
            store
                .surface_token_by_hash(&hash_code(&first.token))
                .expect("hash lookup")
                .is_some()
        );

        // A node enrollment code is not a surface code, and the failed attempt does not
        // consume it.
        let node_code = code::new_enrollment_code(later);
        store
            .insert_enrollment_code(&EnrollmentCode {
                code_hash: node_code.code_hash.clone(),
                node: node.clone(),
                kind: EnrollmentKind::Node,
                surface_id: None,
                expires_at: node_code.expires_at,
                used_at: None,
                created_by: "owner".to_owned(),
                created_at: later,
            })
            .expect("node code");
        let pending = seed_pending(&store, &node, &session, "opencode", later);
        let error = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &node_code.code,
            later,
        )
        .expect_err("node code");
        assert!(matches!(error, CoreError::Invalid(_)));
        assert!(!error.to_string().contains(&node_code.code));
        assert!(
            store
                .consume_enrollment_code(&node_code.code_hash, later)
                .expect("node code lookup")
                .is_some(),
            "a refusal rolls the consumption back"
        );
    }

    #[test]
    fn surface_pair_binds_node_and_session() {
        let store = store();
        let now = ts();
        let mac_studio = node("mac-studio");
        let mac_mini = node("mac-mini");
        seed_node(&store, &mac_studio, now);
        seed_node(&store, &mac_mini, now);
        let session = seed_session(&store, &mac_studio, SESSION, "vowp", now);
        let other = seed_session(&store, &mac_studio, OTHER_SESSION, "other", now);
        let pending = seed_pending(&store, &mac_studio, &session, "opencode", now);

        let error = issue_surface_token(
            &store,
            &mac_mini,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect_err("wrong node");
        assert!(matches!(error, CoreError::Refused(_)));

        let error = issue_surface_token(
            &store,
            &mac_studio,
            &pending.surface.id,
            &other,
            "opencode",
            &pending.code,
            now,
        )
        .expect_err("wrong session");
        assert!(matches!(error, CoreError::Refused(_)));

        let error = issue_surface_token(
            &store,
            &mac_studio,
            &pending.surface.id,
            &session,
            "other",
            &pending.code,
            now,
        )
        .expect_err("wrong app");
        assert!(matches!(error, CoreError::Refused(_)));

        // A pending node cannot redeem a code until enrollment activates it.
        let pending_node = node("pending-node");
        store
            .insert_pending_node(&pending_node, &[], now)
            .expect("pending node");
        let pending_surface = seed_pending(&store, &pending_node, &session, "opencode", now);
        let error = issue_surface_token(
            &store,
            &pending_node,
            &pending_surface.surface.id,
            &session,
            "opencode",
            &pending_surface.code,
            now,
        )
        .expect_err("inactive node");
        assert!(matches!(error, CoreError::Refused(_)));
        assert!(
            store
                .list_surface_tokens(&pending_surface.surface.id)
                .expect("tokens")
                .is_empty()
        );

        assert!(
            store
                .list_surface_tokens(&pending.surface.id)
                .expect("tokens")
                .is_empty()
        );
        assert_eq!(
            store
                .surface_by_id(&pending.surface.id)
                .expect("surface lookup")
                .expect("surface row")
                .state,
            SurfaceState::Pending
        );

        let issued = issue_surface_token(
            &store,
            &mac_studio,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("correct pairing");
        assert!(
            issued
                .scopes
                .contains(&SurfaceScope::new(format!("session.read:{SESSION}")))
        );
        assert!(
            !issued
                .scopes
                .contains(&SurfaceScope::new(format!("session.read:{OTHER_SESSION}")))
        );
    }

    #[test]
    fn surface_token_verify_rejects_revoked_and_expired() {
        let store = store();
        let (_root, deps) = test_deps();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);

        let first = seed_pending(&store, &node, &session, "opencode", now);
        let first_token = issue_surface_token(&store, &node, &first.surface.id, &session, "opencode", &first.code, now)
            .expect("issued token");
        let verified = verify_surface_token(&store, &first_token.token, now).expect("verify");
        assert_eq!(verified.surface.id, first.surface.id);
        assert_eq!(verified.surface.node, node);
        assert_eq!(verified.token.surface_id, first.surface.id);
        assert_eq!(verified.token.scopes, first_token.scopes);
        assert_eq!(verified.token.last_used_at, Some(now));

        let later = now.plus(Duration::from_secs(1));
        let verified = verify_surface_token(&store, &first_token.token, later).expect("verify later");
        assert_eq!(verified.token.last_used_at, Some(later));

        let expired = now.plus(SURFACE_TOKEN_TTL);
        let error = verify_surface_token(&store, &first_token.token, expired).expect_err("expired token");
        assert!(matches!(error, CoreError::Refused(_)));

        let second = seed_pending(&store, &node, &session, "opencode", now);
        let second_token = issue_surface_token(
            &store,
            &node,
            &second.surface.id,
            &session,
            "opencode",
            &second.code,
            now,
        )
        .expect("issued token");
        let row = store.list_surface_tokens(&second.surface.id).expect("tokens").remove(0);
        revoke_surface_token(&store, &row.id, now).expect("revoke token");
        let error = verify_surface_token(&store, &second_token.token, now).expect_err("revoked token");
        assert!(matches!(error, CoreError::Refused(_)));
        let error = revoke_surface_token(&store, &row.id, now).expect_err("already revoked");
        assert!(matches!(error, CoreError::Refused(_)));

        let third = seed_pending(&store, &node, &session, "opencode", now);
        let third_token = issue_surface_token(&store, &node, &third.surface.id, &session, "opencode", &third.code, now)
            .expect("issued token");
        assert_eq!(
            revoke_surface(&store, &deps, &third.surface.id, now).expect("revoke surface"),
            1
        );
        let error = verify_surface_token(&store, &third_token.token, now).expect_err("revoked surface");
        assert!(matches!(error, CoreError::Refused(_)));
        assert_eq!(
            store
                .surface_by_id(&third.surface.id)
                .expect("surface lookup")
                .expect("surface row")
                .state,
            SurfaceState::Revoked
        );

        assert!(matches!(
            verify_surface_token(&store, "not-a-token", now).expect_err("garbage token"),
            CoreError::Refused(_)
        ));
        let unknown = format!("{TOKEN_SCHEME}{}", "A".repeat(43));
        assert!(matches!(
            verify_surface_token(&store, &unknown, now).expect_err("unknown token"),
            CoreError::Refused(_)
        ));
    }

    #[test]
    fn surface_token_rotation_keeps_the_old_token_for_the_overlap() {
        let store = store();
        let (_root, deps) = test_deps();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);
        let pending = seed_pending(&store, &node, &session, "opencode", now);
        let old = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");

        let rotated = rotate_surface_token(&store, &deps, &pending.surface.id, SURFACE_TOKEN_OVERLAP, now)
            .expect("rotated token");
        assert_eq!(rotated.surface.id, pending.surface.id);
        assert_eq!(rotated.node, node);
        assert_eq!(rotated.scopes, old.scopes);
        assert_eq!(rotated.expires_at, now.plus(SURFACE_TOKEN_TTL));
        assert_eq!(rotated.old_expires_at, now.plus(SURFACE_TOKEN_OVERLAP));
        assert_ne!(rotated.token, old.token);
        assert!(rotated.token.starts_with(TOKEN_SCHEME));
        let random_part = rotated.token.strip_prefix(TOKEN_SCHEME).expect("scheme");
        assert_eq!(
            rotated.prefix,
            random_part.chars().take(PREFIX_CHARS).collect::<String>()
        );

        let tokens = store.list_surface_tokens(&pending.surface.id).expect("tokens");
        assert_eq!(tokens.len(), 2);
        for token in &tokens {
            assert_ne!(token.token_hash, rotated.token);
            assert_ne!(token.token_hash, old.token);
        }
        let old_row = tokens
            .iter()
            .find(|token| token.token_hash == hash_code(&old.token))
            .expect("old token row");
        assert_eq!(old_row.expires_at, Some(now.plus(SURFACE_TOKEN_OVERLAP)));
        let new_row = tokens
            .iter()
            .find(|token| token.token_hash == hash_code(&rotated.token))
            .expect("new token row");
        assert_eq!(new_row.expires_at, Some(now.plus(SURFACE_TOKEN_TTL)));
        assert_eq!(new_row.scopes, old.scopes);
        assert_eq!(new_row.prefix, rotated.prefix);

        let overlap = now.plus(Duration::from_secs(9 * 60));
        assert!(
            verify_surface_token(&store, &old.token, overlap).is_ok(),
            "the old token still verifies inside the overlap"
        );
        assert!(
            verify_surface_token(&store, &rotated.token, overlap).is_ok(),
            "the new token verifies inside the overlap"
        );

        let after = now.plus(Duration::from_secs(11 * 60));
        let error = verify_surface_token(&store, &old.token, after).expect_err("old token expires");
        assert!(matches!(error, CoreError::Refused(_)));
        assert!(
            verify_surface_token(&store, &rotated.token, after).is_ok(),
            "the new token still verifies after the overlap"
        );

        let debug = format!("{rotated:?}");
        assert!(!debug.contains(&rotated.token), "Debug must not carry the token");
        assert!(
            !error.to_string().contains(&old.token),
            "the refusal must not echo a token"
        );
    }

    #[test]
    fn surface_rotate_refuses_pending_and_revoked() {
        let store = store();
        let (_root, deps) = test_deps();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);

        let pending = seed_pending(&store, &node, &session, "opencode", now);
        let error = rotate_surface_token(&store, &deps, &pending.surface.id, SURFACE_TOKEN_OVERLAP, now)
            .expect_err("pending surface");
        assert!(matches!(error, CoreError::Refused(_)));
        assert!(
            store
                .list_surface_tokens(&pending.surface.id)
                .expect("tokens")
                .is_empty()
        );

        let attached = seed_pending(&store, &node, &session, "opencode", now);
        let issued = issue_surface_token(
            &store,
            &node,
            &attached.surface.id,
            &session,
            "opencode",
            &attached.code,
            now,
        )
        .expect("issued token");
        revoke_surface(&store, &deps, &attached.surface.id, now).expect("revoke surface");
        let error = rotate_surface_token(&store, &deps, &attached.surface.id, SURFACE_TOKEN_OVERLAP, now)
            .expect_err("revoked surface");
        assert!(matches!(error, CoreError::Refused(_)));
        let tokens = store.list_surface_tokens(&attached.surface.id).expect("tokens");
        assert_eq!(tokens.len(), 1, "a refused rotation mints no token");
        assert!(tokens[0].revoked_at.is_some());
        assert!(
            verify_surface_token(&store, &issued.token, now).is_err(),
            "the old token still refuses after a revoked rotation"
        );

        let unknown = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAZ".to_owned());
        assert!(matches!(
            rotate_surface_token(&store, &deps, &unknown, SURFACE_TOKEN_OVERLAP, now).expect_err("unknown surface"),
            CoreError::NotFound(_)
        ));
    }

    #[test]
    fn surface_rotation_never_extends_a_shorter_expiry() {
        let store = store();
        let (_root, deps) = test_deps();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);
        let pending = seed_pending(&store, &node, &session, "opencode", now);
        let issued = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");

        let near_expiry = now.plus(Duration::from_secs(2 * 60));
        store
            .insert_surface_token(&SurfaceToken {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAY".to_owned(),
                surface_id: pending.surface.id.clone(),
                token_hash: hash_code("short-lived-token"),
                prefix: "shortlived01".to_owned(),
                scopes: issued.scopes.clone(),
                created_at: now,
                expires_at: Some(near_expiry),
                revoked_at: None,
                last_used_at: None,
            })
            .expect("short-lived token");

        let rotated = rotate_surface_token(&store, &deps, &pending.surface.id, SURFACE_TOKEN_OVERLAP, now)
            .expect("rotated token");
        assert_eq!(rotated.old_expires_at, near_expiry);
        let short = store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .into_iter()
            .find(|token| token.id == "01ARZ3NDEKTSV4RRFFQ69G5FAY")
            .expect("short-lived row");
        assert_eq!(short.expires_at, Some(near_expiry), "a rotation never extends a token");
        let old = store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .into_iter()
            .find(|token| token.token_hash == hash_code(&issued.token))
            .expect("old token row");
        assert_eq!(old.expires_at, Some(now.plus(SURFACE_TOKEN_OVERLAP)));
    }

    #[test]
    fn surface_rotate_grace_extends_the_old_token_but_never_past_the_cap() {
        let store = store();
        let (_root, deps) = test_deps();
        let now = ts();
        let node = node("mac-studio");
        seed_node(&store, &node, now);
        let session = seed_session(&store, &node, SESSION, "vowp", now);

        // A two-hour grace keeps the old token for two hours.
        let two_hours = Duration::from_secs(2 * 60 * 60);
        let pending = seed_pending(&store, &node, &session, "opencode", now);
        let issued = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");
        let rotation = rotate_surface_with_policy(
            &store,
            &deps,
            &pending.surface.id,
            RotationPolicy {
                grace: two_hours,
                wait: None,
            },
            now,
        )
        .expect("rotated with a two-hour grace");
        assert_eq!(rotation.old_expires_at, now.plus(two_hours));
        assert_eq!(rotation.rotated.old_expires_at, now.plus(two_hours));
        assert!(!rotation.delivered, "no wait means no drained frame");
        let old = store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .into_iter()
            .find(|token| token.token_hash == hash_code(&issued.token))
            .expect("old token row");
        assert_eq!(old.expires_at, Some(now.plus(two_hours)));
        assert!(
            verify_surface_token(&store, &issued.token, now.plus(two_hours).minus(Duration::from_secs(1))).is_ok(),
            "the old token works inside the grace"
        );
        assert!(
            verify_surface_token(&store, &issued.token, now.plus(two_hours)).is_err(),
            "the old token dies at the grace"
        );

        // A two-day grace is capped at the design maximum of 24 hours.
        let pending = seed_pending(&store, &node, &session, "opencode", now);
        let issued = issue_surface_token(
            &store,
            &node,
            &pending.surface.id,
            &session,
            "opencode",
            &pending.code,
            now,
        )
        .expect("issued token");
        let rotation = rotate_surface_with_policy(
            &store,
            &deps,
            &pending.surface.id,
            RotationPolicy {
                grace: Duration::from_secs(48 * 60 * 60),
                wait: None,
            },
            now,
        )
        .expect("rotated with a capped grace");
        assert_eq!(rotation.old_expires_at, now.plus(SURFACE_TOKEN_MAX_GRACE));
        assert_eq!(rotation.rotated.old_expires_at, now.plus(SURFACE_TOKEN_MAX_GRACE));
        let old = store
            .list_surface_tokens(&pending.surface.id)
            .expect("tokens")
            .into_iter()
            .find(|token| token.token_hash == hash_code(&issued.token))
            .expect("old token row");
        assert_eq!(old.expires_at, Some(now.plus(SURFACE_TOKEN_MAX_GRACE)));
        assert!(
            verify_surface_token(
                &store,
                &issued.token,
                now.plus(SURFACE_TOKEN_MAX_GRACE).minus(Duration::from_secs(1))
            )
            .is_ok(),
            "the old token works inside the capped grace"
        );
        assert!(
            verify_surface_token(&store, &issued.token, now.plus(SURFACE_TOKEN_MAX_GRACE)).is_err(),
            "the old token never outlives the cap"
        );
    }
}
