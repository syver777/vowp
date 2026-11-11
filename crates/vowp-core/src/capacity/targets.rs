//! Capacity targets: the measured `auto` number, the owner's `fixed` number and its history.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.4. `auto` derives the target from the
//! latest host sample and the pooled p90 estimate; `fixed` is the owner's number and may
//! exceed the advice when `--override` is passed. Every change is a history row.

use std::time::Duration;

use vowp_protocol::{CapacityTarget, TargetMode, Timestamp};
use vowp_store::Store;
use vowp_store::repo_capacity::EstimateKey;

use crate::CoreError;
use crate::deps::Deps;

use super::scope::{DEFAULT_MEAN_CPU_CORES, DEFAULT_PEAK_RSS_BYTES};

/// One gibibyte, the unit of the capacity reserves.
const GIB: u64 = 1024 * 1024 * 1024;

/// The smallest target the manager will propose.
pub const MIN_TARGET: u32 = 1;

/// The largest target the manager will propose.
pub const MAX_TARGET: u32 = 64;

/// Fraction of the machine's memory kept free for the node.
pub const RESERVE_RAM_FRACTION: f64 = 0.10;

/// Minimum RAM reserve in bytes.
pub const RESERVE_RAM_MIN_BYTES: u64 = GIB;

/// Fraction of the CPU quota that may be used by admitted runs.
pub const CPU_HEADROOM: f64 = 0.85;

/// How many host samples the advisor looks back over for an auto target.
const HISTORY_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Returns the current worker target using the store's clock.
pub fn current(store: &Store) -> Result<CapacityTarget, CoreError> {
    current_at(store, store.clock().now(), None)
}

/// Returns the current worker target, letting the caller supply the provider cap.
///
/// The `deps`-aware overload exists because the configured provider `max_concurrent` values
/// are not stored in the database; the exported [`current`] keeps its original signature.
pub fn current_for(deps: &Deps, store: &Store, now: Timestamp) -> Result<CapacityTarget, CoreError> {
    let provider_cap = deps
        .config
        .providers
        .values()
        .map(|provider| provider.max_concurrent)
        .min();
    current_at_with(store, now, provider_cap, deps.config.node.max_workers.max(MIN_TARGET))
}

/// Returns the current target with an explicit provider cap.
pub fn current_at(store: &Store, now: Timestamp, provider_cap: Option<u32>) -> Result<CapacityTarget, CoreError> {
    current_at_with(store, now, provider_cap, MIN_TARGET)
}

/// Returns the current target, with the bootstrap value used before the first sample.
fn current_at_with(
    store: &Store,
    now: Timestamp,
    provider_cap: Option<u32>,
    bootstrap: u32,
) -> Result<CapacityTarget, CoreError> {
    let advised = advised_workers_with(store, now, provider_cap, bootstrap)?;
    match store.latest_capacity_target()? {
        Some(mut target) => {
            target.advised_workers = Some(advised);
            if target.mode == TargetMode::Auto {
                target.target_workers = advised;
                target.is_override = false;
            }
            Ok(target)
        }
        None => Ok(CapacityTarget {
            mode: TargetMode::Auto,
            target_workers: advised,
            advised_workers: Some(advised),
            is_override: false,
            set_by: "supervisor".to_owned(),
            set_at: now,
            reason: "auto target from measurements".to_owned(),
        }),
    }
}

/// Computes the target the supervisor advises from measurements and estimates.
///
/// `provider_cap` is the smallest configured `max_concurrent`; `None` means no configured
/// cap (or a caller such as the exported [`current`] that has no config to read).
pub fn advised_workers(store: &Store, now: Timestamp, provider_cap: Option<u32>) -> Result<u32, CoreError> {
    advised_workers_with(store, now, provider_cap, MIN_TARGET)
}

/// Computes the advised target, using `bootstrap` until the first host sample exists.
fn advised_workers_with(
    store: &Store,
    now: Timestamp,
    provider_cap: Option<u32>,
    bootstrap: u32,
) -> Result<u32, CoreError> {
    let sample = latest_sample(store, now)?;
    let Some(sample) = sample else {
        // Before the first sample there is no measured limit. The configured preference is
        // the v0.1 bootstrap, not a permanent cap; the first sample replaces it.
        let cap = provider_cap.unwrap_or(MAX_TARGET);
        return Ok(bootstrap.clamp(MIN_TARGET, MAX_TARGET).min(cap));
    };
    if sample.source == vowp_protocol::SampleSource::Fallback || sample.mem_total_bytes == 0 {
        // §6.1: `auto` never silently claims a measured target on a host without `/proc`.
        return Ok(MIN_TARGET);
    }
    let (ram_p90, cpu_p90) = pooled_estimate(store)?;
    let reserve = reserve_ram_bytes(sample.mem_total_bytes);
    let ram_fit = if ram_p90 == 0 {
        MAX_TARGET
    } else {
        sample
            .mem_available_bytes
            .saturating_sub(reserve)
            .checked_div(ram_p90)
            .unwrap_or(0)
            .min(u64::from(MAX_TARGET)) as u32
    };
    let quota = sample
        .cpu_quota_cores
        .unwrap_or_else(|| f64::from(sample.cpu_count.max(1)));
    let cpu_fit = if cpu_p90 <= 0.0 {
        MAX_TARGET
    } else {
        ((quota * CPU_HEADROOM) / cpu_p90)
            .floor()
            .clamp(0.0, f64::from(MAX_TARGET)) as u32
    };
    let cap = provider_cap.unwrap_or(MAX_TARGET);
    Ok(ram_fit.min(cpu_fit).min(cap).clamp(MIN_TARGET, MAX_TARGET))
}

/// Writes one capacity target row and returns it.
///
/// When `mode` is [`TargetMode::Auto`] the target is recomputed and `target` is ignored.
#[allow(clippy::too_many_arguments)]
pub fn set(
    store: &Store,
    mode: TargetMode,
    target: u32,
    is_override: bool,
    by: &str,
    reason: &str,
    now: Timestamp,
) -> Result<CapacityTarget, CoreError> {
    let advised = advised_workers(store, now, None)?;
    let target_workers = match mode {
        TargetMode::Auto => advised,
        TargetMode::Fixed => target.max(MIN_TARGET),
    };
    let written = CapacityTarget {
        mode,
        target_workers,
        advised_workers: Some(advised),
        is_override: is_override && target_workers > advised,
        set_by: by.to_owned(),
        set_at: now,
        reason: reason.to_owned(),
    };
    store.insert_capacity_target(&written)?;
    Ok(written)
}

/// Returns the RAM reserve for a host with `mem_total` bytes.
pub fn reserve_ram_bytes(mem_total: u64) -> u64 {
    let fraction = (mem_total as f64 * RESERVE_RAM_FRACTION) as u64;
    fraction.max(RESERVE_RAM_MIN_BYTES)
}

/// Returns the latest host sample, preferably from the last hour.
fn latest_sample(store: &Store, now: Timestamp) -> Result<Option<vowp_protocol::HostSample>, CoreError> {
    let recent = now.minus(HISTORY_WINDOW);
    let mut samples = store.host_samples_since(recent)?;
    if samples.is_empty() {
        samples = store.host_samples_since(Timestamp::from_unix_secs(0))?;
    }
    Ok(samples.into_iter().last())
}

/// Returns the pooled p90 RAM/CPU estimate used by the auto target.
///
/// The most conservative stored p90 wins: a node target must fit the largest common run,
/// and an empty estimate table falls back to the compiled default.
fn pooled_estimate(store: &Store) -> Result<(u64, f64), CoreError> {
    let estimates = store.list_usage_estimates(None)?;
    let mut measured = estimates.iter().filter(|estimate| estimate.samples > 0).peekable();
    if measured.peek().is_some() {
        let ram = measured
            .clone()
            .map(|estimate| estimate.ram_p90_bytes)
            .max()
            .unwrap_or(0);
        let cpu = measured.map(|estimate| estimate.cpu_p90_cores).fold(0.0_f64, f64::max);
        return Ok((ram.max(1), cpu.max(0.0)));
    }
    if let Some(estimate) = store.usage_estimate(&EstimateKey {
        project: None,
        job_kind: None,
        harness: None,
        model: None,
        build_mode: None,
    })? {
        return Ok((estimate.ram_p90_bytes.max(1), estimate.cpu_p90_cores));
    }
    Ok((DEFAULT_PEAK_RSS_BYTES, DEFAULT_MEAN_CPU_CORES))
}
