//! Additive cluster-wide capacity totals.
//!
//! Spec: `docs/design/v0.5/scope.md` §6. This module only reads: the per-Home report and
//! snapshot keep their existing fields, and this card adds the pooled totals across Home
//! and every online, active agent worker node.
//!
//! A node counts when it is enrolled `active`, its last heartbeat is under 30 seconds old,
//! and its kind is [`NodeKind::Agent`]. A legacy SSH node or the Home row is never counted
//! as a worker node here.

use vowp_protocol::{ClusterCapacity, NodeCard, NodeKind, NodeName, NodePresence, NodeStatus, Timestamp};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

use super::admit::{latest_sample, open_runs};
use super::targets;

/// Builds the additive cluster card for `now`.
pub fn build(deps: &Deps, store: &Store, now: Timestamp) -> Result<ClusterCapacity, CoreError> {
    let target = targets::current_for(deps, store, now)?;
    let mut cluster = ClusterCapacity {
        target_workers: target.target_workers,
        busy_workers: home_running(store, &deps.node)?,
        ..ClusterCapacity::default()
    };

    if let Some(sample) = latest_sample(store, None)? {
        cluster.cpu_cores = cluster.cpu_cores.saturating_add(sample.cpu_count);
        cluster.mem_total_bytes = cluster.mem_total_bytes.saturating_add(sample.mem_total_bytes);
        cluster.mem_available_bytes = cluster.mem_available_bytes.saturating_add(sample.mem_available_bytes);
        cluster.disk_free_bytes = cluster.disk_free_bytes.saturating_add(
            sample
                .disk_free_worktrees_bytes
                .map_or(sample.disk_free_home_bytes, |worktrees| {
                    sample.disk_free_home_bytes.min(worktrees)
                }),
        );
    }

    for card in online_workers(store, now)? {
        cluster.online_worker_nodes = cluster.online_worker_nodes.saturating_add(1);
        if let Some(capacity) = &card.capacity {
            cluster.target_workers = cluster.target_workers.saturating_add(capacity.max_slots);
            cluster.busy_workers = cluster.busy_workers.saturating_add(capacity.busy_slots);
            cluster.cpu_cores = cluster.cpu_cores.saturating_add(capacity.cpu_cores);
            cluster.mem_total_bytes = cluster.mem_total_bytes.saturating_add(capacity.mem_total_bytes);
            cluster.mem_available_bytes = cluster.mem_available_bytes.saturating_add(capacity.mem_free_bytes);
            cluster.disk_free_bytes = cluster.disk_free_bytes.saturating_add(capacity.disk_free_bytes);
        }
    }
    Ok(cluster)
}

/// Returns every online, active agent worker card.
pub(crate) fn online_workers(store: &Store, now: Timestamp) -> Result<Vec<NodeCard>, CoreError> {
    Ok(store
        .list_node_cards(now)?
        .into_iter()
        .filter(is_online_worker)
        .collect())
}

/// Returns whether `card` is an online, active agent worker node.
pub(crate) fn is_online_worker(card: &NodeCard) -> bool {
    card.kind == NodeKind::Agent && card.status == NodeStatus::Active && card.presence == NodePresence::Online
}

/// Counts open runs that execute on `home` rather than on a remote node.
pub(crate) fn home_running(store: &Store, home: &NodeName) -> Result<u32, CoreError> {
    Ok(open_runs(store)?
        .into_iter()
        .filter(|(_, run, _)| &run.node == home)
        .count()
        .try_into()
        .unwrap_or(u32::MAX))
}
