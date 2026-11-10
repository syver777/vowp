//! L4 planner: when to plan, which executor to use, and the planner digest.
//!
//! Spec: `docs/design/loops.md` §7 and `docs/design/crates/vowp-core.md` §7. The planner is
//! one session per project, resumed one turn at a time. It is due on an unseen event, on the
//! `l4-planner` heartbeat, or after the idle gap; when none of its ladder providers is
//! available the keeper covers planning instead.

use std::collections::HashMap;
use std::time::Duration;

use vowp_protocol::{ActiveState, Executor, JobState, NodeName, Role, Timestamp};
use vowp_store::{JobFilter, Store};

use crate::CoreError;
use crate::deps::Deps;

use super::digest::Digest;
use super::prompts;

/// One executor choice for a planner/keeper turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChoice {
    /// Executor to start the turn on.
    pub executor: Executor,
    /// Node override from the profile.
    pub node: Option<NodeName>,
    /// Ladder rung index.
    pub rung: u32,
    /// Provider key used for limits.
    pub provider: String,
}

/// The planner's state for this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerStatus {
    /// True when a planner turn should run now (subject to the open-run check).
    pub wanted: bool,
    /// Executor chosen when one is available.
    pub choice: Option<SessionChoice>,
    /// True when the planner is active but none of its ladder rungs is available, so the
    /// keeper takes the planner duties instead.
    pub provider_limited: bool,
    /// Digest/since timestamp for this turn.
    pub digest_since: Timestamp,
    /// Rendered planner prompt.
    pub prompt: String,
}

/// Evaluates whether the planner is due and which executor it would use.
pub fn planner_status(deps: &Deps, store: &Store, now: Timestamp, running: bool) -> Result<PlannerStatus, CoreError> {
    let digest_since = last_turn(store, "planner")?.unwrap_or_else(|| now.minus(Duration::from_secs(24 * 3600)));
    let loop_ = store.loop_("l4-planner")?;
    let paused = loop_.as_ref().is_some_and(|loop_| loop_.state == ActiveState::Paused);
    let interval_due = loop_
        .as_ref()
        .and_then(|loop_| loop_.next_run_at)
        .is_some_and(|due| due <= now);
    let event_due = event_wake(store, "planner")?;
    let idle_gap = Duration::from_secs(u64::from(bounds(loop_.as_ref()).idle_gap_minutes.unwrap_or(15)) * 60);
    let idle_due = planner_idle(deps, store, now, digest_since, idle_gap)?;
    let mut wanted = !paused && !running && (event_due || interval_due || idle_due);
    if wanted && !gap_ok(deps, store, now, "planner")? {
        wanted = false;
    }
    let provider_choice = if paused {
        None
    } else {
        choose(deps, store, "planner", now)?
    };
    // Provider availability is independent of whether this turn is due and of `running`:
    // the planner run that recorded the limit may still be finishing (its run row is
    // closed but its thread has not been reaped), and the keeper turn woken by that limit
    // must still carry the planner duties.
    let provider_limited = !paused && provider_choice.is_none();
    if provider_limited {
        wanted = false;
    }
    let digest = if wanted {
        Digest::build(store, &deps.config, Role::Planner, digest_since, 24_000)?
    } else {
        String::new()
    };
    let prompt = if wanted {
        prompts::planner_prompt(&digest, deps.config.project.name.as_str())
    } else {
        String::new()
    };
    Ok(PlannerStatus {
        wanted,
        choice: if wanted { provider_choice } else { None },
        provider_limited,
        digest_since,
        prompt,
    })
}

/// Returns the timestamp of the last planner/keeper turn recorded in `kv`.
pub fn last_turn(store: &Store, role: &str) -> Result<Option<Timestamp>, CoreError> {
    Ok(store
        .kv_get(&format!("{role}:last_turn"))?
        .and_then(|text| super::parse_ts(&text)))
}

/// Records the start of a planner/keeper turn.
pub fn mark_turn(store: &Store, role: &str, at: Timestamp) -> Result<(), CoreError> {
    store.kv_set(&format!("{role}:last_turn"), &at.as_rfc3339())?;
    Ok(())
}

/// Returns true when `wake:<role>` holds an event that no turn has consumed yet.
pub fn event_wake(store: &Store, role: &str) -> Result<bool, CoreError> {
    Ok(store.kv_get(&format!("wake:{role}"))?.as_deref() == Some("1")
        && store.kv_get(&format!("{role}:wake_consumed"))?.as_deref() != Some("1"))
}

/// Marks the current `wake:<role>` event consumed by a started turn.
pub fn consume_wake(store: &Store, role: &str) -> Result<(), CoreError> {
    store.kv_set(&format!("{role}:wake_consumed"), "1")?;
    Ok(())
}

/// Raises `wake:<role>` for a new unseen event, clearing the consumed marker.
pub fn raise_wake(store: &Store, role: &str) -> Result<(), CoreError> {
    store.kv_set(&format!("wake:{role}"), "1")?;
    store.kv_delete(&format!("{role}:wake_consumed"))?;
    Ok(())
}

/// Returns true when the minimum gap since the role's last turn has elapsed.
pub fn gap_ok(deps: &Deps, store: &Store, now: Timestamp, role: &str) -> Result<bool, CoreError> {
    let Some(last) = last_turn(store, role)? else {
        return Ok(true);
    };
    let gap = Duration::from_secs(
        u64::from(
            deps.config
                .loops
                .get(match role {
                    "planner" => "l4-planner",
                    _ => "l5-keeper",
                })
                .and_then(|loop_| loop_.bounds.planner_min_gap_minutes)
                .unwrap_or(5),
        ) * 60,
    );
    Ok(now.since(last) >= gap)
}

/// Picks the first non-limited rung of `profile_name`.
pub fn choose(
    deps: &Deps,
    store: &Store,
    profile_name: &str,
    now: Timestamp,
) -> Result<Option<SessionChoice>, CoreError> {
    let Some(profile) = deps.config.profiles.get(profile_name) else {
        return Ok(None);
    };
    let limited = super::limited_map(store, now)?.into_iter().collect::<HashMap<_, _>>();
    let Some((rung, executor)) = profile.pick(&limited, now) else {
        return Ok(None);
    };
    Ok(Some(SessionChoice {
        executor: executor.clone(),
        node: profile.node.clone(),
        rung: u32::try_from(rung).unwrap_or(u32::MAX),
        provider: executor.provider(),
    }))
}

/// True when no job is active but non-terminal work remains and the idle gap elapsed.
fn planner_idle(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    since: Timestamp,
    gap: Duration,
) -> Result<bool, CoreError> {
    let jobs = store.list_jobs(&JobFilter {
        project: Some(deps.config.project.name.clone()),
        ..JobFilter::default()
    })?;
    let active = jobs.iter().any(|job| {
        matches!(
            job.state,
            JobState::Pending
                | JobState::Ready
                | JobState::Running
                | JobState::Gating
                | JobState::Reviewing
                | JobState::Fixing
                | JobState::Merging
        )
    });
    let remaining = jobs.iter().any(|job| !job.state.is_terminal());
    Ok(!active && remaining && now.since(since) >= gap)
}

/// Returns the loop bounds, or the defaults when the row is absent.
fn bounds(loop_: Option<&vowp_protocol::Loop>) -> vowp_protocol::LoopBounds {
    loop_.map(|loop_| loop_.bounds.clone()).unwrap_or_default()
}
