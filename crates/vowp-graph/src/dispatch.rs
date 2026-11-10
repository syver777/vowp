//! Deterministic dispatch planning: slots, provider budgets, profiles and ownership locks.
//! Spec: `docs/design/crates/vowp-graph.md` §5.
//!
//! [`plan_dispatch`] only plans: it mutates no job, starts no run and keeps no state between
//! calls. The caller applies each [`DispatchChoice`] as a `Dispatched` event.

use std::collections::{BTreeMap, HashMap};

use vowp_protocol::{Executor, Job, JobId, JobState, NodeName, Profile, Role, Timestamp};

use crate::graph::Graph;
use crate::owns::OwnsLocks;

/// The capacity available to one dispatch plan.
#[derive(Debug, Clone)]
pub struct DispatchCaps {
    /// Idle worker slots for worker-role jobs.
    pub free_workers: u32,
    /// Idle verifier slots for verifier-role jobs.
    pub free_verifiers: u32,
    /// Remaining concurrent sessions per provider; a missing key means unlimited.
    pub provider_free: HashMap<String, u32>,
    /// Providers currently limited and when they reset; a missing key means not limited.
    pub limited: HashMap<String, Option<Timestamp>>,
    /// Configured profiles by name.
    pub profiles: BTreeMap<String, Profile>,
    /// Instant used to decide whether a limit has expired.
    pub now: Timestamp,
}

/// One job selected for dispatch and the executor it should run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchChoice {
    /// The ready job to dispatch.
    pub job_id: JobId,
    /// Executor picked from the job's profile ladder.
    pub executor: Executor,
    /// Index of the picked rung in the profile ladder.
    pub rung: u32,
    /// Provider key of the picked executor, used for limits.
    pub provider: String,
    /// Node the profile pins, or `None` for the local node.
    pub node: Option<NodeName>,
}

/// Plans which ready jobs to dispatch now, in acceptance order.
///
/// Candidates run by priority (descending), creation time (ascending) and id (ascending).
/// A candidate is skipped when its role has no free slot, when its profile is missing, when
/// every rung is limited, when its provider has no capacity, or when its ownership patterns
/// overlap a running job or a choice already made. Planner and keeper jobs consume no slot,
/// but at most one of each role is chosen per call and none is chosen while a job of that
/// role is already running. Accepted choices keep their lock for the rest of the call.
pub fn plan_dispatch(graph: &Graph, running: &[Job], caps: &DispatchCaps) -> Vec<DispatchChoice> {
    let mut candidates: Vec<&Job> = graph.jobs().filter(|job| job.state == JobState::Ready).collect();
    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut locks = OwnsLocks::from_running(running, graph);
    let mut free_workers = caps.free_workers;
    let mut free_verifiers = caps.free_verifiers;
    let mut provider_left = caps.provider_free.clone();
    let mut planner_running = running.iter().any(|job| job.role == Role::Planner);
    let mut keeper_running = running.iter().any(|job| job.role == Role::Keeper);
    let mut planner_chosen = false;
    let mut keeper_chosen = false;
    let mut choices = Vec::new();

    for job in candidates {
        match job.role {
            Role::Worker if free_workers == 0 => continue,
            Role::Verifier if free_verifiers == 0 => continue,
            Role::Planner if planner_running || planner_chosen => continue,
            Role::Keeper if keeper_running || keeper_chosen => continue,
            _ => {}
        }

        let Some(profile) = caps.profiles.get(&job.profile) else {
            continue;
        };
        let Some((rung, executor)) = profile.pick(&caps.limited, caps.now) else {
            continue;
        };
        let provider = executor.provider();
        if provider_left.get(&provider).is_some_and(|left| *left == 0) {
            continue;
        }
        if locks.try_acquire(job, graph).is_err() {
            continue;
        }

        if let Some(left) = provider_left.get_mut(&provider) {
            *left -= 1;
        }
        match job.role {
            Role::Worker => free_workers -= 1,
            Role::Verifier => free_verifiers -= 1,
            Role::Planner => {
                planner_chosen = true;
                planner_running = true;
            }
            Role::Keeper => {
                keeper_chosen = true;
                keeper_running = true;
            }
        }

        choices.push(DispatchChoice {
            job_id: job.id.clone(),
            executor: executor.clone(),
            rung: u32::try_from(rung).unwrap_or(u32::MAX),
            provider,
            node: profile.node.clone(),
        });
    }

    choices
}
