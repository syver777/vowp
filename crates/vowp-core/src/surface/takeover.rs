//! Takeover of a session from another surface.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.3.
//!
//! The lease row moves first, so the old surface is cut off immediately: its ingest
//! writer compares its token with the shared row on every line/heartbeat and emits
//! `IngestAck::LeaseRevoked`. When the session already has an executor, the existing
//! switch protocol drains the old run, interrupts it after five seconds, writes a capsule
//! and seeds the new surface. A session that has never attached an executor gets the note
//! only, because a switch has no valid target.

use vowp_protocol::{
    ExecutorRef, Lease, SessionId, Surface, SwitchReason, SwitchRequest, TakeoverReport, Timestamp, new_lease_token,
};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;
use crate::session::service::SessionService;

use super::lease::{append_note, clock_time, surface_kind_name};

/// How long a takeover lets the old surface's turn drain before interrupting it.
pub const TAKEOVER_DRAIN_SECS: u64 = 5;

/// Takes the session over for `target`.
pub fn takeover(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    target: &Surface,
    now: Timestamp,
) -> Result<TakeoverReport, CoreError> {
    Ok(takeover_inner(deps, store, session, target, now)?.0)
}

/// Takes the session over and also returns the inserted lease.
///
/// [`crate::surface::LeaseGuard::acquire`] uses the returned row instead of re-reading the
/// lease afterwards, which would let a second concurrent takeover make the guard adopt
/// another surface's token.
pub(crate) fn takeover_inner(
    deps: &Deps,
    store: &Store,
    session: &SessionId,
    target: &Surface,
    now: Timestamp,
) -> Result<(TakeoverReport, Lease), CoreError> {
    let row = store
        .session(session)?
        .ok_or_else(|| CoreError::NotFound(format!("session {session}")))?;
    store.upsert_surface(target)?;

    let previous = store.lease(session)?;
    let lease = Lease {
        session_id: session.clone(),
        surface_id: Some(target.id.clone()),
        surface_kind: target.kind,
        node: target.node.clone(),
        holder: target.holder.clone(),
        token: new_lease_token(),
        acquired_at: now,
        heartbeat_at: now,
    };
    let replaced = store.take_lease(&lease)?.or(previous);

    let Some(previous) = replaced else {
        // No old surface: this is a first attach, not a takeover.
        append_note(
            deps,
            store,
            session,
            format!(
                "surface {} ({}) attached",
                surface_kind_name(target.kind),
                target.holder
            ),
        )?;
        return Ok((
            TakeoverReport {
                previous: None,
                drained: false,
                capsule_id: None,
            },
            lease,
        ));
    };

    let active = row.state == vowp_protocol::SessionState::Running
        || store
            .open_runs()?
            .iter()
            .any(|run| run.session_id.as_ref() == Some(session));
    let mut drained = false;
    let mut capsule_id = None;
    if let Some(executor) = row.executor.clone() {
        // The switch target is the takeover target's node, not the session row's log
        // owner. A local target needs no node row; a remote target is named explicitly so
        // the switch probes it over SSH.
        let to_node = if target.node == deps.node {
            None
        } else {
            Some(target.node.clone())
        };
        let switched = SessionService::new(deps.clone()).switch(&SwitchRequest {
            session_id: session.clone(),
            to: ExecutorRef {
                executor,
                node: to_node,
            },
            reason: SwitchReason::Takeover,
            drain_secs: TAKEOVER_DRAIN_SECS,
        })?;
        drained = active;
        capsule_id = switched.capsule_id;
    }

    // The note is written after the switch: during an active turn the runner or ingest
    // writer owns the single-writer log lock; the switch and the revocation release it
    // before this point.
    append_note(
        deps,
        store,
        session,
        format!(
            "taken over by {} at {}",
            target.holder,
            clock_time(now, deps.config.node.tz_offset_minutes)
        ),
    )?;

    Ok((
        TakeoverReport {
            previous: Some(previous.holder),
            drained,
            capsule_id,
        },
        lease,
    ))
}

/// Returns a short human notice for a takeover, used by chat and tests.
pub fn notice(by: &str, at: Timestamp, tz_offset_minutes: i32) -> String {
    format!(
        "Taken over by {} at {}. This surface is now read-only (follow mode).",
        by,
        clock_time(at, tz_offset_minutes)
    )
}
