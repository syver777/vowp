//! Capacity brakes: memory pressure, fast swap growth, low disk and exhausted providers.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.3. A brake only stops new admissions; it
//! never signals, kills or parks a running process. It trips on measured conditions and
//! clears after two clean samples.

use std::time::Duration;

use vowp_protocol::{BrakeCode, BrakeReason, BrakeState, HostSample, Timestamp};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::Deps;

/// `/proc/pressure/memory` `avg10` above which memory pressure counts as high.
pub const MEMORY_PRESSURE_AVG10: f64 = 10.0;

/// Swap growth per minute above which the swap brake trips.
pub const SWAP_GROWTH_BYTES_PER_MIN: u64 = 64 * 1024 * 1024;

/// Minimum free disk before the disk brake trips.
///
/// §6.2 wants `max(5 GiB, 10% of the filesystem)`. `HostSample` currently carries free
/// bytes only, not the filesystem total, so this build can enforce the 5 GiB floor but not
/// the percentage; adding `disk_total_home_bytes`/`disk_total_worktrees_bytes` is the
/// protocol change needed before the percentage can be implemented honestly.
pub const MIN_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// How far back the brake reads host samples.
const HISTORY_WINDOW: Duration = Duration::from_secs(20 * 60);

/// Key of the persisted active flag.
const ACTIVE_KEY: &str = "capacity:brake:active";

/// Key of the persisted trip instant.
const SINCE_KEY: &str = "capacity:brake:since";

/// Key of the persisted reasons.
const REASONS_KEY: &str = "capacity:brake:reasons";

/// Evaluates the node brake from host samples alone.
pub fn evaluate(store: &Store, now: Timestamp) -> Result<BrakeState, CoreError> {
    evaluate_with(store, now, &[])
}

/// Evaluates the brake including the provider condition.
///
/// The provider condition trips only when every configured provider has an active limit;
/// the store alone cannot know which providers exist, so this overload takes `deps`.
pub fn evaluate_with_config(deps: &Deps, store: &Store, now: Timestamp) -> Result<BrakeState, CoreError> {
    let providers: Vec<String> = deps.config.providers.keys().cloned().collect();
    evaluate_with(store, now, &providers)
}

/// Evaluates the brake with an explicit provider list.
pub fn evaluate_with(store: &Store, now: Timestamp, providers: &[String]) -> Result<BrakeState, CoreError> {
    let samples = store.host_samples_since(now.minus(HISTORY_WINDOW))?;
    let mut reasons = sample_reasons(&samples);
    if !providers.is_empty() {
        let active = store.active_limits(now)?;
        if providers
            .iter()
            .all(|provider| active.iter().any(|limit| &limit.provider == provider))
        {
            reasons.push(BrakeReason {
                code: BrakeCode::Provider,
                detail: format!("every provider is limited: {}", providers.join(", ")),
            });
        }
    }

    let was_active = store.kv_get(ACTIVE_KEY)?.as_deref() == Some("1");
    if !reasons.is_empty() {
        let since = if was_active {
            stored_since(store)?.unwrap_or(now)
        } else {
            now
        };
        persist(store, true, since, &reasons)?;
        return Ok(BrakeState {
            active: true,
            reasons,
            since: Some(since),
        });
    }

    if was_active {
        if clears_after_two_clean_samples(&samples) {
            clear(store)?;
            return Ok(BrakeState::default());
        }
        let reasons = stored_reasons(store)?;
        return Ok(BrakeState {
            active: true,
            reasons,
            since: stored_since(store)?,
        });
    }
    Ok(BrakeState::default())
}

/// Returns the reasons one sample window trips, independent of the persisted state.
fn sample_reasons(samples: &[HostSample]) -> Vec<BrakeReason> {
    let mut reasons = Vec::new();
    if samples.len() >= 2 {
        let tail = &samples[samples.len() - 2..];
        if tail
            .iter()
            .all(|sample| sample.pressure_mem_avg10.is_some_and(|avg| avg > MEMORY_PRESSURE_AVG10))
        {
            let avg = tail
                .iter()
                .filter_map(|sample| sample.pressure_mem_avg10)
                .fold(0.0_f64, f64::max);
            reasons.push(BrakeReason {
                code: BrakeCode::MemoryPressure,
                detail: format!("memory pressure (avg10 {avg:.1})"),
            });
        }
    }
    if samples.len() >= 3 {
        let first = &samples[samples.len() - 3];
        let last = &samples[samples.len() - 1];
        let seconds = last.at.since(first.at).as_secs();
        if seconds > 0 {
            let growth = last.swap_used_bytes.saturating_sub(first.swap_used_bytes);
            let per_minute = (growth as f64) * 60.0 / seconds as f64;
            if per_minute > SWAP_GROWTH_BYTES_PER_MIN as f64 {
                reasons.push(BrakeReason {
                    code: BrakeCode::SwapGrowth,
                    detail: format!("swap growing {:.0} MiB/min", per_minute / (1024.0 * 1024.0)),
                });
            }
        }
    }
    if let Some(sample) = samples.last() {
        // §6.3 reads one low sample as `DiskLow`; disk pressure can fill a filesystem
        // between samples, and the admission disk check already trusts the latest sample.
        let low = |sample: &HostSample| {
            sample.disk_free_home_bytes < MIN_DISK_BYTES
                || sample
                    .disk_free_worktrees_bytes
                    .is_some_and(|free| free < MIN_DISK_BYTES)
        };
        if low(sample) {
            reasons.push(BrakeReason {
                code: BrakeCode::DiskLow,
                detail: "free disk below 5 GiB".to_owned(),
            });
        }
    }
    reasons
}

/// Returns true once the last two samples are clean.
fn clears_after_two_clean_samples(samples: &[HostSample]) -> bool {
    if samples.len() < 2 {
        return false;
    }
    samples[samples.len() - 2..].iter().all(sample_is_clean)
}

/// Returns true when one sample does not itself trip the disk, memory or swap brakes.
fn sample_is_clean(sample: &HostSample) -> bool {
    sample.disk_free_home_bytes >= MIN_DISK_BYTES
        && sample
            .disk_free_worktrees_bytes
            .is_none_or(|free| free >= MIN_DISK_BYTES)
        && sample.pressure_mem_avg10.is_none_or(|avg| avg <= MEMORY_PRESSURE_AVG10)
}

/// Persists the brake state in the global `kv` table.
fn persist(store: &Store, active: bool, since: Timestamp, reasons: &[BrakeReason]) -> Result<(), CoreError> {
    store.kv_set(ACTIVE_KEY, if active { "1" } else { "0" })?;
    store.kv_set(SINCE_KEY, &since.as_rfc3339())?;
    let json = serde_json::to_string(reasons)
        .map_err(|error| CoreError::Command(format!("capacity brake reasons: {error}")))?;
    store.kv_set(REASONS_KEY, &json)?;
    Ok(())
}

/// Reads the persisted trip instant.
fn stored_since(store: &Store) -> Result<Option<Timestamp>, CoreError> {
    let Some(text) = store.kv_get(SINCE_KEY)? else {
        return Ok(None);
    };
    Ok(Timestamp::parse(&text).ok())
}

/// Reads the persisted reasons.
fn stored_reasons(store: &Store) -> Result<Vec<BrakeReason>, CoreError> {
    let Some(text) = store.kv_get(REASONS_KEY)? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str(&text).unwrap_or_default())
}

/// Removes the persisted brake state.
fn clear(store: &Store) -> Result<(), CoreError> {
    store.kv_delete(ACTIVE_KEY)?;
    store.kv_delete(SINCE_KEY)?;
    store.kv_delete(REASONS_KEY)?;
    Ok(())
}
