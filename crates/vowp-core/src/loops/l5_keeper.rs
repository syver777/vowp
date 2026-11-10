//! L5 keeper: diagnosing stalled work, and covering the planner when its providers are limited.
//!
//! Spec: `docs/design/loops.md` §7 and §12. The keeper is one session per project, resumed
//! one turn at a time. It is due on an unseen stall/failure, on the `l5-keeper` heartbeat, or
//! when the planner has no available provider; covering turns also carry the planner digest
//! and duties.

use std::time::Duration;

use sha2::{Digest as _, Sha256};
use vowp_protocol::{ActiveState, GateResult, Role, Timestamp};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

use super::digest::Digest;
use super::l4_planner::{SessionChoice, choose, gap_ok, last_turn};
use super::prompts;

/// One due keeper turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeeperTurn {
    /// Executor chosen for the turn.
    pub choice: SessionChoice,
    /// Rendered keeper prompt.
    pub prompt: String,
    /// True when this turn also covers planner duties.
    pub covering: bool,
    /// Digest start timestamp.
    pub digest_since: Timestamp,
}

/// Evaluates whether the keeper is due.
pub fn keeper_turn(
    deps: &Deps,
    store: &Store,
    now: Timestamp,
    covering: bool,
    planner_since: Timestamp,
    running: bool,
) -> Result<Option<KeeperTurn>, CoreError> {
    let loop_ = store.loop_("l5-keeper")?;
    let paused = loop_.as_ref().is_some_and(|loop_| loop_.state == ActiveState::Paused);
    if paused || running {
        return Ok(None);
    }
    let event_due = super::l4_planner::event_wake(store, "keeper")?;
    let interval_due = loop_
        .as_ref()
        .and_then(|loop_| loop_.next_run_at)
        .is_some_and(|due| due <= now);
    if !(covering || event_due || interval_due) {
        return Ok(None);
    }
    if !gap_ok(deps, store, now, "keeper")? {
        return Ok(None);
    }
    let Some(choice) = choose(deps, store, "keeper", now)? else {
        return Ok(None);
    };
    let digest_since = last_turn(store, "keeper")?.unwrap_or_else(|| now.minus(Duration::from_secs(24 * 3600)));
    let digest = Digest::build(store, &deps.config, Role::Keeper, digest_since, 24_000)?;
    let planner_digest = if covering {
        Some(Digest::build(
            store,
            &deps.config,
            Role::Planner,
            planner_since,
            24_000,
        )?)
    } else {
        None
    };
    Ok(Some(KeeperTurn {
        choice,
        prompt: prompts::keeper_prompt(&digest, planner_digest.as_deref()),
        covering,
        digest_since,
    }))
}

/// Returns a stable fingerprint of one gate evaluation's evidence.
///
/// The repeat guard only trips when three consecutive failures produce the same fingerprint:
/// the gate indexes, pass/fail flags and evidence tails. Any changed tail starts a fresh run.
pub fn gate_fingerprint(results: &[GateResult]) -> String {
    let mut hasher = Sha256::new();
    for result in results {
        hasher.update(result.gate_index.to_le_bytes());
        hasher.update([u8::from(result.passed)]);
        let mut evidence = serde_json::to_value(&result.evidence).unwrap_or(serde_json::Value::Null);
        strip_volatile(&mut evidence);
        let text = serde_json::to_string(&evidence).unwrap_or_default();
        hasher.update(text.as_bytes());
    }
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Removes wall-clock fields that change on every otherwise identical evaluation.
fn strip_volatile(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("duration_ms");
            for child in map.values_mut() {
                strip_volatile(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                strip_volatile(child);
            }
        }
        _ => {}
    }
}
