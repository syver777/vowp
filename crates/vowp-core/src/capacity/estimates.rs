//! Usage estimates: nearest-rank p50/p90 from real runs, with the spec's fallback pools.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1. An exact group needs five samples;
//! smaller groups pool in a fixed order until a pool reaches five, then fall back to the
//! compiled `1.5 GB / 1 core` default.

use std::collections::BTreeMap;

use vowp_protocol::{AdmitRequest, BuildMode, EstimateSource, ProjectName, RunUsage, Timestamp, UsageEstimate};
use vowp_store::Store;
use vowp_store::repo_capacity::{EstimateKey, UsageQuery};

use crate::CoreError;

use super::scope::{DEFAULT_MEAN_CPU_CORES, DEFAULT_PEAK_RSS_BYTES};

/// Number of samples a group needs before it stops pooling.
pub const MIN_SAMPLES: usize = 5;

/// Number of newest runs kept per exact group.
pub const WINDOW: usize = 50;

/// Recomputes every exact-group estimate and the constant default row.
pub fn recompute(store: &Store, _now: Timestamp) -> Result<u32, CoreError> {
    let rows = store.run_usage_for_group(&UsageQuery::default(), u32::MAX)?;
    let mut groups: BTreeMap<String, EstimateKey> = BTreeMap::new();
    for row in &rows {
        let key = key_of(row);
        groups.entry(key_text(&key)).or_insert(key);
    }
    let mut written = 0_u32;
    for key in groups.into_values() {
        let build = key.build_mode.unwrap_or(BuildMode::Unknown);
        let estimate = estimate_for_build(&key, build, &rows);
        store.upsert_usage_estimate(&estimate)?;
        written = written.saturating_add(1);
    }
    store.upsert_usage_estimate(&constant_default())?;
    Ok(written.saturating_add(1))
}

/// Returns the estimate admission should use for `req`.
pub fn estimate_for(store: &Store, req: &AdmitRequest) -> Result<UsageEstimate, CoreError> {
    let key = EstimateKey {
        project: Some(req.project.clone()),
        job_kind: Some(req.kind),
        harness: req.harness,
        model: req.model.clone(),
        build_mode: None,
    };
    if let Some(stored) = store.usage_estimate(&key)?
        && stored.samples > 0
    {
        return Ok(stored);
    }
    let rows = store.run_usage_for_group(
        &UsageQuery {
            project: None,
            job_kind: Some(req.kind),
            // Deliberately unfiltered: the same-project and same-kind pools may contain
            // another harness (including `harness = None` gate rows). `estimate_for_build`
            // applies the exact -> harness -> project -> kind order itself.
            harness: None,
            model: None,
            build_mode: None,
            since: None,
        },
        u32::MAX,
    )?;
    if !rows.is_empty() {
        let mut by_mode: BTreeMap<String, Vec<RunUsage>> = BTreeMap::new();
        for row in rows {
            by_mode.entry(format!("{:?}", row.build_mode)).or_default().push(row);
        }
        let mut candidates = Vec::new();
        for rows in by_mode.into_values() {
            let build = rows.first().map(|row| row.build_mode).unwrap_or(BuildMode::Unknown);
            let estimate = estimate_for_build(&key, build, &rows);
            if estimate.samples > 0 {
                candidates.push(estimate);
            }
        }
        if let Some(best) = candidates.into_iter().max_by(|left, right| {
            left.ram_p90_bytes
                .cmp(&right.ram_p90_bytes)
                .then_with(|| left.cpu_p90_cores.total_cmp(&right.cpu_p90_cores))
        }) {
            return Ok(best);
        }
    }
    if let Some(stored) = store.usage_estimate(&key)? {
        return Ok(stored);
    }
    Ok(default_estimate(&key))
}

/// Builds the estimate for one exact group plus its fallback pools.
fn estimate_for_build(key: &EstimateKey, build: BuildMode, rows: &[RunUsage]) -> UsageEstimate {
    let exact = last_window(rows_matching(rows, |row| {
        key.project.as_ref().is_none_or(|project| project == &row.project)
            && row.job_kind == key.job_kind
            && row.harness == key.harness
            && row.model == key.model
            && row.build_mode == build
    }));
    if exact.len() >= MIN_SAMPLES {
        return from_stats(key, build, &exact, EstimateSource::Measured);
    }
    let same_harness = last_window(rows_matching(rows, |row| {
        key.project.as_ref().is_none_or(|project| project == &row.project)
            && row.job_kind == key.job_kind
            && row.harness == key.harness
            && row.build_mode == build
    }));
    if same_harness.len() >= MIN_SAMPLES {
        return from_stats(key, build, &same_harness, EstimateSource::Pooled);
    }
    let same_project = last_window(rows_matching(rows, |row| {
        key.project.as_ref().is_none_or(|project| project == &row.project)
            && row.job_kind == key.job_kind
            && row.build_mode == build
    }));
    if same_project.len() >= MIN_SAMPLES {
        return from_stats(key, build, &same_project, EstimateSource::Pooled);
    }
    let same_kind = last_window(rows_matching(rows, |row| {
        row.job_kind == key.job_kind && row.build_mode == build
    }));
    if same_kind.len() >= MIN_SAMPLES {
        return from_stats(key, build, &same_kind, EstimateSource::Pooled);
    }
    default_estimate(key)
}

/// Keeps the rows matching `predicate`.
fn rows_matching(rows: &[RunUsage], predicate: impl Fn(&RunUsage) -> bool) -> Vec<&RunUsage> {
    rows.iter().filter(|row| predicate(row)).collect()
}

/// Keeps the newest [`WINDOW`] rows, in ascending start order.
fn last_window(mut rows: Vec<&RunUsage>) -> Vec<&RunUsage> {
    rows.sort_by_key(|row| (row.started_at, row.run_id.clone()));
    if rows.len() > WINDOW {
        rows.drain(..rows.len() - WINDOW);
    }
    rows
}

/// Builds one estimate from a non-empty sample set.
fn from_stats(key: &EstimateKey, build: BuildMode, rows: &[&RunUsage], source: EstimateSource) -> UsageEstimate {
    let mut ram: Vec<u64> = rows.iter().map(|row| row.peak_rss_bytes).collect();
    let mut cpu: Vec<f64> = rows.iter().map(|row| row.mean_cpu_cores).collect();
    let mut duration: Vec<u64> = rows.iter().map(|row| row.duration_ms).collect();
    ram.sort_unstable();
    cpu.sort_by(f64::total_cmp);
    duration.sort_unstable();
    UsageEstimate {
        project: key.project.clone(),
        job_kind: key.job_kind,
        harness: key.harness,
        model: key.model.clone(),
        build_mode: Some(build),
        samples: u32::try_from(rows.len()).unwrap_or(u32::MAX),
        ram_p50_bytes: percentile_u64(&ram, 50.0),
        ram_p90_bytes: percentile_u64(&ram, 90.0),
        cpu_p50_cores: percentile_f64(&cpu, 50.0),
        cpu_p90_cores: percentile_f64(&cpu, 90.0),
        duration_p50_ms: percentile_u64(&duration, 50.0),
        duration_p90_ms: percentile_u64(&duration, 90.0),
        source,
    }
}

/// Returns the compiled conservative default for `key`.
fn default_estimate(key: &EstimateKey) -> UsageEstimate {
    UsageEstimate {
        project: key.project.clone(),
        job_kind: key.job_kind,
        harness: key.harness,
        model: key.model.clone(),
        build_mode: key.build_mode,
        samples: 0,
        ram_p50_bytes: DEFAULT_PEAK_RSS_BYTES,
        ram_p90_bytes: DEFAULT_PEAK_RSS_BYTES,
        cpu_p50_cores: DEFAULT_MEAN_CPU_CORES,
        cpu_p90_cores: DEFAULT_MEAN_CPU_CORES,
        duration_p50_ms: 0,
        duration_p90_ms: 0,
        source: EstimateSource::Default,
    }
}

/// Returns the constant pooled row `('*','*','','','unknown')`.
fn constant_default() -> UsageEstimate {
    default_estimate(&EstimateKey {
        project: None,
        job_kind: None,
        harness: None,
        model: None,
        build_mode: None,
    })
}

/// Builds the exact key of one usage row.
fn key_of(row: &RunUsage) -> EstimateKey {
    EstimateKey {
        project: Some(row.project.clone()),
        job_kind: row.job_kind,
        harness: row.harness,
        model: row.model.clone(),
        build_mode: Some(row.build_mode),
    }
}

/// Returns a stable map key for an estimate key.
fn key_text(key: &EstimateKey) -> String {
    format!(
        "{}\u{1f}{:?}\u{1f}{:?}\u{1f}{}\u{1f}{:?}",
        key.project.as_ref().map(ProjectName::as_str).unwrap_or(""),
        key.job_kind,
        key.harness,
        key.model.as_deref().unwrap_or(""),
        key.build_mode
    )
}

/// Nearest-rank percentile over an ascending integer slice.
fn percentile_u64(values: &[u64], percentile: f64) -> u64 {
    let Some(&last) = values.last() else {
        return 0;
    };
    if values.len() == 1 {
        return last;
    }
    let rank = ((percentile / 100.0) * values.len() as f64).ceil().max(1.0) as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

/// Nearest-rank percentile over an ascending float slice.
fn percentile_f64(values: &[f64], percentile: f64) -> f64 {
    let Some(&last) = values.last() else {
        return 0.0;
    };
    if values.len() == 1 {
        return last;
    }
    let rank = ((percentile / 100.0) * values.len() as f64).ceil().max(1.0) as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}
