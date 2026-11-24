//! Lease acquisition, heartbeat, checks and takeover notifications.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3.
//!
//! A session has at most one lease row. [`LeaseGuard::acquire`] registers the surface,
//! takes the row, appends the attach note and keeps the heartbeat fresh every five
//! seconds. A lease whose heartbeat is sixty seconds old is stale and may be replaced
//! without asking the owner; a live lease needs an explicit takeover.
//!
//! The lease row is also the cross-process takeover signal: a takeover replaces the
//! token, and the old surface's ingest writer compares its token with the row on every
//! line and heartbeat. A token mismatch produces [`IngestAck::LeaseRevoked`] through
//! [`revocation_ack_for`], so no process-local channel is needed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use time::{OffsetDateTime, UtcOffset, macros::format_description};
use vowp_protocol::{
    Actor, EventBody, EventSource, HarnessKind, IngestAck, Lease, LeaseMode, Note, OwnershipState, Session, SessionId,
    SessionOwner, SessionRole, Surface, SurfaceActivity, SurfaceId, SurfaceInfo, SurfaceKind, Timestamp,
};
use vowp_store::repo_surfaces::OwnerRecord;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_store::{Store, StoreError, hash_lease_token};

use crate::CoreError;
use crate::deps::Deps;

pub use vowp_protocol::surface::new_lease_token;

/// How often a held lease sends a heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// A lease without a heartbeat for this long is free.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// The request to acquire a session lease.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquireLease {
    /// Session to attach to.
    pub session: SessionId,
    /// The surface asking for the lease.
    pub surface: Surface,
    /// Whether an interactive owner already agreed to take over.
    pub takeover: bool,
}

/// The result of the atomic decision in [`LeaseGuard::acquire`].
enum AcquireOutcome {
    /// The caller installed a new lease; `previous` is the stale or absent row it replaced.
    Installed { lease: Lease, previous: Option<Lease> },
    /// A live lease was left untouched for the caller to refuse or take over.
    Live(Lease),
}

/// The grant returned to a local surface after a successful lease acquisition.
///
/// `lease_token` is plaintext and must be written only to a 0600 grant file or kept in
/// memory; the home stores its SHA-256 in `leases.token_hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLeaseGrant {
    /// Plaintext lease token, shown once and never logged.
    pub lease_token: String,
    /// When the lease goes stale without a heartbeat.
    pub expires_at: Timestamp,
    /// Session sequence the holder may continue from.
    pub tail_seq: u64,
}

/// A live lease that refused an acquisition without takeover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseConflict {
    /// Human-visible holder name.
    pub holder: String,
    /// When the holder acquired the lease.
    pub since: Timestamp,
    /// Kind of client holding it.
    pub surface_kind: SurfaceKind,
}

/// Result of an agent-directed lease acquisition.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentLeaseAcquire {
    /// The surface now holds the lease.
    Granted {
        /// The one-time grant.
        grant: AgentLeaseGrant,
        /// The row this acquisition replaced, when one existed.
        previous: Option<Lease>,
    },
    /// A live lease is held by another surface and no takeover was requested.
    Conflict(LeaseConflict),
}

/// Result of an agent-directed lease heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLeaseHeartbeat {
    /// The heartbeat advanced `heartbeat_at`.
    Accepted,
    /// The row disappeared (released or swept); the surface is read-only.
    Lost(String),
    /// Another surface holds the lease; stop sending events.
    Revoked {
        /// Human-readable reason.
        reason: String,
        /// Human-visible holder of the lease now.
        holder: String,
    },
}

/// Result of an agent-directed lease release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLeaseRelease {
    /// The row was deleted, or no row existed.
    Released,
    /// The surface no longer holds the row; it was already read-only.
    Lost(String),
    /// Another surface holds the lease; the release was not applied.
    Revoked {
        /// Human-readable reason.
        reason: String,
        /// Human-visible holder of the lease now.
        holder: String,
    },
}

/// A held lease with its heartbeat thread.
pub struct LeaseGuard {
    deps: Deps,
    session: SessionId,
    token: String,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    detached: bool,
}

impl LeaseGuard {
    /// Acquires the session lease, replacing a stale one or a live one the owner agreed
    /// to take over.
    ///
    /// A live lease without `req.takeover` returns [`StoreError::Conflict`] with the
    /// holder, kind, node and acquisition time so the caller can ask the owner.
    ///
    /// The decision and the write run in one `BEGIN IMMEDIATE` transaction, so a losing
    /// concurrent acquire never writes its token and then restores the winner's row. An
    /// explicit takeover still runs through [`super::takeover::takeover_inner`].
    pub fn acquire(deps: &Deps, store: &Store, req: &AcquireLease) -> Result<LeaseGuard, CoreError> {
        let now = deps.clock.now();
        store.upsert_surface(&req.surface)?;

        let outcome = store.transaction(|tx| {
            let previous = tx.ownership(&req.session)?;
            if let Some(record) = &previous
                && record.is_live_at(now)
            {
                return Ok(AcquireOutcome::Live(record.lease.clone()));
            }
            let lease = Lease {
                session_id: req.session.clone(),
                surface_id: Some(req.surface.id.clone()),
                surface_kind: req.surface.kind,
                node: req.surface.node.clone(),
                holder: req.surface.holder.clone(),
                token: new_lease_token(),
                acquired_at: now,
                heartbeat_at: now,
            };
            let replaced = tx.take_lease(&lease)?;
            Ok(AcquireOutcome::Installed {
                lease,
                previous: replaced,
            })
        })?;

        let token = match outcome {
            AcquireOutcome::Installed { lease, previous } => {
                match &previous {
                    Some(previous) => {
                        append_note(
                            deps,
                            store,
                            &req.session,
                            format!("stale surface {} replaced", previous.holder),
                        )?;
                    }
                    None => {
                        append_note(
                            deps,
                            store,
                            &req.session,
                            format!(
                                "surface {} ({}) attached",
                                surface_kind_name(req.surface.kind),
                                req.surface.holder
                            ),
                        )?;
                    }
                }
                lease.token
            }
            AcquireOutcome::Live(previous) => {
                if !req.takeover {
                    return Err(CoreError::Store(StoreError::Conflict(held_text(
                        &req.session,
                        &previous,
                    ))));
                }
                super::takeover::takeover_inner(deps, store, &req.session, &req.surface, now)?
                    .1
                    .token
            }
        };

        let (thread, stop) = start_heartbeat(deps, req.session.clone(), token.clone());
        Ok(LeaseGuard {
            deps: deps.clone(),
            session: req.session.clone(),
            token,
            thread: Some(thread),
            stop,
            detached: false,
        })
    }

    /// Returns the lease token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Stops heartbeating and returns the token without releasing the lease.
    ///
    /// The caller takes over the heartbeat (the one-shot `session lease --acquire` hand-off);
    /// dropping the detached guard later neither deletes the row nor touches it.
    pub fn detach(mut self) -> String {
        self.detached = true;
        self.token.clone()
    }

    /// Releases the lease and stops the heartbeat thread.
    pub fn release(self) -> Result<(), CoreError> {
        let store = self.deps.open_store()?;
        store.release_lease(&self.session, &self.token)?;
        Ok(())
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !self.detached
            && let Ok(store) = self.deps.open_store()
        {
            let _ = store.release_lease(&self.session, &self.token);
        }
        // The heartbeat thread checks the flag before its next write and exits.
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.thread.take();
    }
}

/// Checks that the caller owns `session` and may write it.
///
/// Reads `VOWP_LEASE_TOKEN`, which `chat` and `session ingest` set from the grant. See
/// [`check_lease_with`] for the rule.
pub fn check_lease(deps: &Deps, store: &Store, session: &SessionId) -> Result<(), CoreError> {
    let _ = deps;
    let token = std::env::var("VOWP_LEASE_TOKEN").ok();
    check_lease_with(store, session, token.as_deref())
}

/// Checks an explicit lease token against the session's owner.
///
/// The rule depends on the session's role and the owner's liveness:
///
/// - an owner whose token matches and who is live (a fresh interactive heartbeat or an
///   unexpired job) passes; a matching token on a stale or expired owner is refused;
/// - another or a missing token is refused with the read-only notice;
/// - a session with no owner row is refused when it is interactive (`User`, `Orchestrator`,
///   or no role and no job); a local `Planner`, `Keeper`, `Worker` or `Verifier` session
///   still passes, preserving the supervisor's own runs.
pub fn check_lease_with(store: &Store, session: &SessionId, token: Option<&str>) -> Result<(), CoreError> {
    let now = store.clock().now();
    let Some(record) = store.ownership(session)? else {
        let row = store.require_session(session)?;
        return if is_interactive_session(&row) {
            Err(CoreError::Refused(NO_OWNER_NOTICE.to_owned()))
        } else {
            Ok(())
        };
    };
    match token {
        Some(token) if lease_token_matches(&record.lease, token) => {
            if record.is_live_at(now) {
                Ok(())
            } else {
                Err(CoreError::Refused(not_live_notice(&record, now)))
            }
        }
        _ => Err(CoreError::Refused(read_only_notice(&record.lease))),
    }
}

/// Builds the token-free [`vowp_protocol::LeaseStatus`] of an ownership record at `now`.
///
/// `token` is always `None`; only `session lease --acquire` sets it, after building this.
pub fn lease_status(record: &OwnerRecord, now: Timestamp) -> vowp_protocol::LeaseStatus {
    let owner = record.to_owner(now);
    vowp_protocol::LeaseStatus {
        held: true,
        holder: Some(owner.holder),
        kind: Some(owner.kind),
        node: Some(owner.node),
        acquired_at: Some(owner.acquired_at),
        heartbeat_at: Some(owner.heartbeat_at),
        token: None,
        mode: Some(owner.mode),
        state: Some(owner.state),
        epoch: Some(owner.epoch),
        expires_at: owner.expires_at,
        label: owner.label,
        activity: Some(owner.activity),
    }
}

/// The refusal for an interactive session that nobody owns.
pub const NO_OWNER_NOTICE: &str = "session has no owner; acquire one";

/// The marker every stop/handoff notice starts with, so a surface can recognise it.
pub const OWNERSHIP_CONFLICT: &str = "ownership_conflict";

/// Returns whether a session is a person's session (needs an owner to be written).
fn is_interactive_session(session: &Session) -> bool {
    match session.role {
        Some(SessionRole::User | SessionRole::Orchestrator) => true,
        Some(SessionRole::Planner | SessionRole::Keeper | SessionRole::Worker | SessionRole::Verifier) => false,
        None => session.job_id.is_none(),
    }
}

/// What a takeover of the current owner needs from the person asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverConfirmation {
    /// No live owner: acquiring is not a takeover.
    NotNeeded,
    /// A live idle (or waiting) owner: one confirmation is enough.
    Single,
    /// A live owner that is mid-turn: the person must confirm a second time.
    Extra,
}

/// Returns the confirmation a takeover of `session` needs at `now`.
pub fn takeover_confirmation(
    store: &Store,
    session: &SessionId,
    now: Timestamp,
) -> Result<TakeoverConfirmation, CoreError> {
    Ok(match store.session_owner(session, now)? {
        Some(owner) if owner.state == OwnershipState::Active => match owner.activity {
            SurfaceActivity::Running => TakeoverConfirmation::Extra,
            SurfaceActivity::Idle | SurfaceActivity::Waiting => TakeoverConfirmation::Single,
        },
        _ => TakeoverConfirmation::NotNeeded,
    })
}

/// Builds the stop/handoff notice a surface that lost ownership receives.
///
/// It starts with [`OWNERSHIP_CONFLICT`] and names the new owner (label when known) and the
/// epoch; it never carries a token.
pub fn stop_notice(new_owner: &SessionOwner) -> String {
    let name = new_owner.label.as_deref().unwrap_or(&new_owner.holder);
    format!(
        "{OWNERSHIP_CONFLICT}: session taken over by {name} ({}, mode {}), owner epoch {}; stop writing",
        new_owner.kind.display_name(),
        new_owner.mode,
        new_owner.epoch
    )
}

/// Returns the refusal for a matching token whose owner is no longer live.
fn not_live_notice(record: &OwnerRecord, now: Timestamp) -> String {
    format!(
        "this surface is read-only; its ownership is {} (holder {}); take the session over to write",
        record.state_at(now),
        record.lease.holder
    )
}

/// Returns whether `token` is the plaintext token behind the stored `lease` hash.
fn lease_token_matches(lease: &Lease, token: &str) -> bool {
    constant_time_eq(lease.token.as_bytes(), hash_lease_token(token).as_bytes())
}

/// Returns whether `token` has the 52-character lease-token shape.
fn is_lease_token(token: &str) -> bool {
    token.len() == 52 && token.bytes().all(|byte| byte.is_ascii_alphanumeric())
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

/// Acquires or takes over the lease of `session` for a paired local surface.
///
/// The surface token has already been verified by the caller and `surface` is its current
/// `attached` row. A live lease without [`LeaseMode::Takeover`] returns
/// [`AgentLeaseAcquire::Conflict`] and changes nothing. A stale lease is replaced without a
/// conflict, and a takeover replaces a live one. The returned grant is the only place the
/// plaintext token appears; the store keeps only its hash.
pub fn acquire_agent_lease(
    store: &Store,
    session: &SessionId,
    surface: &SurfaceInfo,
    mode: LeaseMode,
    now: Timestamp,
) -> Result<AgentLeaseAcquire, CoreError> {
    let session_row = store.require_session(session)?;
    // A paired surface on another node is legitimately authorized to acquire the lease of
    // a session the home created before pairing. The surface-token scope and the live
    // lease are the authority; the session row's original node is not.
    let tail_seq = session_row.last_seq;
    let takeover = mode == LeaseMode::Takeover;
    let outcome = store.transaction(|tx| {
        let record = tx.ownership(session)?;
        let previous = record.as_ref().map(|record| record.lease.clone());
        if let Some(previous) = &previous
            && record.as_ref().is_some_and(|record| record.is_live_at(now))
            && !takeover
        {
            return Ok(AgentLeaseAcquire::Conflict(LeaseConflict {
                holder: previous.holder.clone(),
                since: previous.acquired_at,
                surface_kind: previous.surface_kind,
            }));
        }
        let lease = Lease {
            session_id: session.clone(),
            surface_id: Some(surface.id.clone()),
            surface_kind: surface.kind,
            node: surface.node.clone(),
            holder: surface.holder.clone(),
            token: new_lease_token(),
            acquired_at: now,
            heartbeat_at: now,
        };
        let replaced = tx.take_lease(&lease)?;
        Ok(AgentLeaseAcquire::Granted {
            grant: AgentLeaseGrant {
                lease_token: lease.token,
                expires_at: now.plus(STALE_AFTER),
                tail_seq,
            },
            previous: replaced,
        })
    })?;
    Ok(outcome)
}

/// Advances the heartbeat of an agent-held lease.
///
/// A missing row is [`AgentLeaseHeartbeat::Lost`]; a row held by another surface after a
/// takeover is [`AgentLeaseHeartbeat::Revoked`]. The plaintext token is hashed for the
/// comparison and never stored or logged.
pub fn heartbeat_agent_lease(
    store: &Store,
    session: &SessionId,
    surface: &SurfaceId,
    token: &str,
    now: Timestamp,
) -> Result<AgentLeaseHeartbeat, CoreError> {
    if !is_lease_token(token) {
        return Err(CoreError::Invalid("lease token must be 52 characters".to_owned()));
    }
    if store.heartbeat_lease(session, token, now)? {
        return Ok(AgentLeaseHeartbeat::Accepted);
    }
    Ok(match store.lease(session)? {
        Some(current) if current.surface_id.as_ref() == Some(surface) => {
            AgentLeaseHeartbeat::Lost("the lease was replaced; this surface is read-only".to_owned())
        }
        Some(current) => AgentLeaseHeartbeat::Revoked {
            reason: format!("this surface is read-only; taken over by {}", current.holder),
            holder: current.holder,
        },
        None => AgentLeaseHeartbeat::Lost("the lease was released or expired; this surface is read-only".to_owned()),
    })
}

/// Releases an agent-held lease.
///
/// The release is idempotent when the row is already gone. A row now held by another
/// surface is left untouched and reported as [`AgentLeaseRelease::Revoked`].
pub fn release_agent_lease(
    store: &Store,
    session: &SessionId,
    surface: &SurfaceId,
    token: &str,
) -> Result<AgentLeaseRelease, CoreError> {
    if !is_lease_token(token) {
        return Err(CoreError::Invalid("lease token must be 52 characters".to_owned()));
    }
    if store.release_lease(session, token)? {
        return Ok(AgentLeaseRelease::Released);
    }
    Ok(match store.lease(session)? {
        Some(current) if current.surface_id.as_ref() == Some(surface) => {
            AgentLeaseRelease::Lost("the lease was replaced; this surface is read-only".to_owned())
        }
        Some(current) => AgentLeaseRelease::Revoked {
            reason: format!("this surface is read-only; taken over by {}", current.holder),
            holder: current.holder,
        },
        None => AgentLeaseRelease::Released,
    })
}

/// Returns whether a lease heartbeat is younger than [`STALE_AFTER`].
pub fn is_live(now: Timestamp, lease: &Lease) -> bool {
    now.since(lease.heartbeat_at) < STALE_AFTER
}

/// Returns the `LeaseRevoked` acknowledgement for an ingest writer whose token no longer
/// matches the live lease row.
///
/// The lease row is the shared cross-process state: a takeover replaces its token, the
/// stale sweep deletes it, and an ingest process in another process sees either change
/// when it calls this on every line/heartbeat. `None` means the token is still current.
pub fn revocation_ack_for(
    store: &Store,
    session: &SessionId,
    token: &str,
    last_seq: u64,
) -> Result<Option<IngestAck>, CoreError> {
    match store.lease(session)? {
        Some(lease) if lease_token_matches(&lease, token) => Ok(None),
        Some(lease) => Ok(Some(IngestAck::LeaseRevoked {
            reason: match store.session_owner(session, store.clock().now())? {
                Some(owner) => stop_notice(&owner),
                None => format!("{OWNERSHIP_CONFLICT}: session taken over"),
            },
            by: lease.holder,
            at: lease.acquired_at,
            last_seq,
        })),
        None => Ok(Some(IngestAck::LeaseRevoked {
            reason: "lease released".to_owned(),
            by: "none".to_owned(),
            at: store.clock().now(),
            last_seq,
        })),
    }
}

/// Returns the exclusive `before` bound for the store's stale-lease queries.
///
/// The queries compare `heartbeat_at < before`; timestamps have millisecond precision, so
/// adding 1 ms turns that into "at least [`STALE_AFTER`] old", including the exact boundary.
fn stale_before(now: Timestamp) -> Timestamp {
    now.minus(STALE_AFTER).plus(Duration::from_millis(1))
}

/// Removes leases whose heartbeat is at least [`STALE_AFTER`] old.
///
/// Returns how many rows were deleted. The sweep is the node tick's safety net for a
/// surface that died without calling [`LeaseGuard::release`]; the conditional delete
/// re-checks `heartbeat_at`, so a lease refreshed after the stale snapshot survives.
pub fn sweep_leases(deps: &Deps, store: &Store, now: Timestamp) -> Result<u32, CoreError> {
    let before = stale_before(now);
    let mut dropped = 0_u32;
    for lease in store.stale_leases(before)? {
        // A running orchestrator keeps its stale row so the owner card still names the
        // paired surface (state `stale`) instead of dropping to `null` while a sleeping
        // laptop is away. An explicit release or a takeover still removes it.
        if is_live_orchestrator(store, &lease.session_id)? {
            continue;
        }
        if store.release_stale_lease(&lease.session_id, &lease.token, before)? {
            dropped = dropped.saturating_add(1);
        }
    }
    if dropped > 0 {
        deps.log.log(
            vowp_protocol::Level::Info,
            "surface",
            &format!("swept {dropped} stale lease(s)"),
        );
    }
    Ok(dropped)
}

/// Returns whether `session` is an orchestrator that is still running or switching.
fn is_live_orchestrator(store: &Store, session: &SessionId) -> Result<bool, CoreError> {
    Ok(store.session(session)?.is_some_and(|row| {
        row.role == Some(SessionRole::Orchestrator)
            && matches!(
                row.state,
                vowp_protocol::SessionState::Running | vowp_protocol::SessionState::Switching
            )
    }))
}

/// Renews the interactive lease of every running orchestrator paired to `node`.
///
/// The node agent calls this on each heartbeat: while the agent is connected and the
/// orchestrator session is running, the paired surface (OpenCode desktop) still owns it,
/// so its lease heartbeat is advanced and the owner stays `active`. Returns how many
/// leases were renewed.
pub fn renew_running_orchestrator_leases(
    store: &Store,
    node: &vowp_protocol::NodeName,
    now: Timestamp,
) -> Result<u32, CoreError> {
    let mut renewed = 0_u32;
    for lease in store.list_leases()? {
        let Some(surface) = lease.surface_id.as_ref() else {
            continue;
        };
        if lease.node != *node || !is_live_orchestrator(store, &lease.session_id)? {
            continue;
        }
        if store.heartbeat_surface_lease(&lease.session_id, surface, node, now)? {
            renewed = renewed.saturating_add(1);
        }
    }
    Ok(renewed)
}

/// Appends one free-form note event and indexes it.
pub(crate) fn append_note(deps: &Deps, store: &Store, session: &SessionId, text: String) -> Result<(), CoreError> {
    let log = SessionLog::open(&deps.paths, session, deps.clock.clone())?;
    let appended = log.append(NewEvent {
        actor: Actor::System,
        source: EventSource {
            harness: HarnessKind::Vowp,
            model: None,
            node: Some(deps.node.clone()),
            native_session_id: None,
        },
        body: EventBody::Note(Note { text }),
        raw: None,
        ts: None,
    })?;
    store.index_event(&index_row(&appended))?;
    log.sync()?;
    Ok(())
}

/// Returns the database/help name of a surface kind.
pub(crate) fn surface_kind_name(kind: SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Chat => "chat",
        SurfaceKind::Opencode => "opencode",
        SurfaceKind::ClaudeCode => "claude-code",
        SurfaceKind::Mcp => "mcp",
        SurfaceKind::Mirror => "mirror",
        SurfaceKind::Api => "api",
        SurfaceKind::Agent => "agent",
        SurfaceKind::DesktopUi => "desktop-ui",
    }
}

/// Formats the `HH:MM` of an instant in the given UTC offset.
pub(crate) fn clock_time(ts: Timestamp, tz_offset_minutes: i32) -> String {
    let Some(utc) = OffsetDateTime::from_unix_timestamp(ts.unix_secs()).ok() else {
        return ts.as_rfc3339();
    };
    let offset = UtcOffset::from_whole_seconds(tz_offset_minutes.saturating_mul(60)).unwrap_or(UtcOffset::UTC);
    utc.to_offset(offset)
        .format(format_description!("[hour]:[minute]"))
        .unwrap_or_else(|_| ts.as_rfc3339())
}

/// Returns the read-only refusal text for a lease held by another surface.
fn read_only_notice(lease: &Lease) -> String {
    format!(
        "this surface is read-only; taken over by {} ({})",
        lease.holder,
        surface_kind_name(lease.surface_kind)
    )
}

/// Returns the conflict text that lets an interactive caller ask the owner.
fn held_text(session: &SessionId, lease: &Lease) -> String {
    format!(
        "session {} is held by {} ({} on {}) since {}",
        session,
        lease.holder,
        surface_kind_name(lease.surface_kind),
        lease.node,
        clock_time(lease.acquired_at, 0)
    )
}

/// Starts the heartbeat thread for a held lease.
fn start_heartbeat(deps: &Deps, session: SessionId, token: String) -> (JoinHandle<()>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let deps = deps.clone();
    let handle = thread::spawn(move || {
        let Ok(store) = deps.open_store() else {
            return;
        };
        loop {
            thread::sleep(HEARTBEAT_INTERVAL);
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let now = deps.clock.now();
            match store.heartbeat_lease(&session, &token, now) {
                Ok(true) => {}
                _ => return,
            }
        }
    });
    (handle, stop)
}
