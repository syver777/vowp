//! Repository methods for the capacity tables in the global database.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6 and §13.3. Every statement names
//! `main` explicitly because these tables live in the global file; a [`Store`] bound to a
//! project still shares the same connection.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use vowp_protocol::{
    AdviceState, BuildMode, CapacityAdvice, CapacityTarget, HarnessKind, HostSample, JobId, JobKind, NodeName,
    ProjectName, RunId, RunUsage, Timestamp, UsageEstimate,
};

use crate::convert::{
    bool_to_int, enum_from_text, enum_to_text, json_from_text, json_to_text, opt_ts_from_text, ts_from_text,
    ts_to_text, u32_from_i64,
};
use crate::db::Store;
use crate::error::StoreError;

/// Filter for [`Store::run_usage_for_group`].
#[derive(Debug, Clone, Default)]
pub struct UsageQuery {
    /// Restrict to one project.
    pub project: Option<ProjectName>,
    /// Restrict to one job kind.
    pub job_kind: Option<JobKind>,
    /// Restrict to one harness.
    pub harness: Option<HarnessKind>,
    /// Restrict to one model.
    pub model: Option<String>,
    /// Restrict to one build mode.
    pub build_mode: Option<BuildMode>,
    /// Include runs that started at or after this instant.
    pub since: Option<Timestamp>,
}

/// Exact key of one [`UsageEstimate`] row.
///
/// An empty key column is represented as `None`; `build_mode = None` is the stored
/// `unknown` pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstimateKey {
    /// Project key, or `None` for the pooled empty key.
    pub project: Option<ProjectName>,
    /// Job-kind key, or `None` for the pooled empty key.
    pub job_kind: Option<JobKind>,
    /// Harness key, or `None` for the pooled empty key.
    pub harness: Option<HarnessKind>,
    /// Model key, or `None` for the pooled empty key.
    pub model: Option<String>,
    /// Build-mode key, or `None` for the stored `unknown` key.
    pub build_mode: Option<BuildMode>,
}

const HOST_SAMPLE_COLUMNS: &str = "at, interval_secs, mem_total_bytes, mem_available_bytes, swap_used_bytes, \
     load1, load5, load15, cpu_count, cpu_quota_cores, disk_free_home_bytes, disk_free_worktrees_bytes, \
     pressure_cpu_avg10, pressure_mem_avg10, pressure_io_avg10, source";

const RUN_USAGE_COLUMNS: &str = "run_id, project, job_id, job_kind, role, harness, model, provider, node, \
     started_at, ended_at, duration_ms, peak_rss_bytes, anon_peak_bytes, file_peak_bytes, kernel_peak_bytes, \
     cpu_usec, mean_cpu_cores, max_cpu_cores, build_mode, source, oom_killed, exit_code";

const ESTIMATE_COLUMNS: &str = "project, job_kind, harness, model, build_mode, samples, ram_p50_bytes, \
     ram_p90_bytes, cpu_p50_cores, cpu_p90_cores, duration_p50_ms, duration_p90_ms, source";

const TARGET_COLUMNS: &str = "mode, target_workers, advised_workers, is_override, set_by, at, reason";

const ADVICE_COLUMNS: &str = "id, created_at, current_target, proposed_target, mode, reasons_json, \
     expected_effect, inbox_id, state, applied_at, applied_by";

/// Converts an unsigned count to SQLite's signed integer form.
fn i64_from_u64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|err| crate::convert::corrupt(what, err))
}

/// Converts an optional unsigned count to SQLite's signed integer form.
fn opt_i64_from_u64(value: Option<u64>, what: &str) -> Result<Option<i64>, StoreError> {
    value.map(|value| i64_from_u64(value, what)).transpose()
}

/// Converts a stored integer to `u64`.
fn u64_from_i64(value: i64, what: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|err| crate::convert::corrupt(what, err))
}

/// Converts a stored nullable integer to `u64`.
fn opt_u64_from_i64(value: Option<i64>, what: &str) -> Result<Option<u64>, StoreError> {
    value.map(|value| u64_from_i64(value, what)).transpose()
}

/// Parses an optional enum column.
fn opt_enum_from_text<T: serde::de::DeserializeOwned>(
    text: Option<String>,
    what: &str,
) -> Result<Option<T>, StoreError> {
    text.map(|text| enum_from_text(&text, what)).transpose()
}

/// Raw values of one `host_samples` row.
struct HostSampleRow {
    at: String,
    interval_secs: i64,
    mem_total_bytes: i64,
    mem_available_bytes: i64,
    swap_used_bytes: i64,
    load1: f64,
    load5: f64,
    load15: f64,
    cpu_count: i64,
    cpu_quota_cores: Option<f64>,
    disk_free_home_bytes: i64,
    disk_free_worktrees_bytes: Option<i64>,
    pressure_cpu_avg10: Option<f64>,
    pressure_mem_avg10: Option<f64>,
    pressure_io_avg10: Option<f64>,
    source: String,
}

/// Reads one host-sample row.
fn read_host_sample_row(row: &Row<'_>) -> rusqlite::Result<HostSampleRow> {
    Ok(HostSampleRow {
        at: row.get(0)?,
        interval_secs: row.get(1)?,
        mem_total_bytes: row.get(2)?,
        mem_available_bytes: row.get(3)?,
        swap_used_bytes: row.get(4)?,
        load1: row.get(5)?,
        load5: row.get(6)?,
        load15: row.get(7)?,
        cpu_count: row.get(8)?,
        cpu_quota_cores: row.get(9)?,
        disk_free_home_bytes: row.get(10)?,
        disk_free_worktrees_bytes: row.get(11)?,
        pressure_cpu_avg10: row.get(12)?,
        pressure_mem_avg10: row.get(13)?,
        pressure_io_avg10: row.get(14)?,
        source: row.get(15)?,
    })
}

/// Converts one raw host-sample row.
fn host_sample_from_row(raw: HostSampleRow) -> Result<HostSample, StoreError> {
    Ok(HostSample {
        at: ts_from_text(&raw.at, "host_samples.at")?,
        interval_secs: u64_from_i64(raw.interval_secs, "host_samples.interval_secs")?,
        mem_total_bytes: u64_from_i64(raw.mem_total_bytes, "host_samples.mem_total_bytes")?,
        mem_available_bytes: u64_from_i64(raw.mem_available_bytes, "host_samples.mem_available_bytes")?,
        swap_used_bytes: u64_from_i64(raw.swap_used_bytes, "host_samples.swap_used_bytes")?,
        load1: raw.load1,
        load5: raw.load5,
        load15: raw.load15,
        cpu_count: u32_from_i64(raw.cpu_count, "host_samples.cpu_count")?,
        cpu_quota_cores: raw.cpu_quota_cores,
        disk_free_home_bytes: u64_from_i64(raw.disk_free_home_bytes, "host_samples.disk_free_home_bytes")?,
        disk_free_worktrees_bytes: opt_u64_from_i64(
            raw.disk_free_worktrees_bytes,
            "host_samples.disk_free_worktrees_bytes",
        )?,
        pressure_cpu_avg10: raw.pressure_cpu_avg10,
        pressure_mem_avg10: raw.pressure_mem_avg10,
        pressure_io_avg10: raw.pressure_io_avg10,
        source: enum_from_text(&raw.source, "host_samples.source")?,
    })
}

/// Raw values of one `run_usage` row.
struct RunUsageRow {
    run_id: String,
    project: String,
    job_id: Option<String>,
    job_kind: Option<String>,
    role: String,
    harness: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    node: String,
    started_at: String,
    ended_at: String,
    duration_ms: i64,
    peak_rss_bytes: i64,
    anon_peak_bytes: Option<i64>,
    file_peak_bytes: Option<i64>,
    kernel_peak_bytes: Option<i64>,
    cpu_usec: i64,
    mean_cpu_cores: f64,
    max_cpu_cores: f64,
    build_mode: String,
    source: String,
    oom_killed: i64,
    exit_code: Option<i32>,
}

/// Reads one `run_usage` row.
fn read_run_usage_row(row: &Row<'_>) -> rusqlite::Result<RunUsageRow> {
    Ok(RunUsageRow {
        run_id: row.get(0)?,
        project: row.get(1)?,
        job_id: row.get(2)?,
        job_kind: row.get(3)?,
        role: row.get(4)?,
        harness: row.get(5)?,
        model: row.get(6)?,
        provider: row.get(7)?,
        node: row.get(8)?,
        started_at: row.get(9)?,
        ended_at: row.get(10)?,
        duration_ms: row.get(11)?,
        peak_rss_bytes: row.get(12)?,
        anon_peak_bytes: row.get(13)?,
        file_peak_bytes: row.get(14)?,
        kernel_peak_bytes: row.get(15)?,
        cpu_usec: row.get(16)?,
        mean_cpu_cores: row.get(17)?,
        max_cpu_cores: row.get(18)?,
        build_mode: row.get(19)?,
        source: row.get(20)?,
        oom_killed: row.get(21)?,
        exit_code: row.get(22)?,
    })
}

/// Converts a raw `run_usage` row into its protocol type.
fn run_usage_from_row(raw: RunUsageRow) -> Result<RunUsage, StoreError> {
    let job_id = match raw.job_id {
        Some(id) => Some(JobId::parse(&id)?),
        None => None,
    };
    Ok(RunUsage {
        run_id: RunId::parse(&raw.run_id)?,
        project: ProjectName::parse(&raw.project)?,
        job_id,
        job_kind: opt_enum_from_text(raw.job_kind, "run_usage.job_kind")?,
        role: enum_from_text(&raw.role, "run_usage.role")?,
        harness: opt_enum_from_text(raw.harness, "run_usage.harness")?,
        model: raw.model,
        provider: raw.provider,
        node: NodeName::parse(&raw.node)?,
        started_at: ts_from_text(&raw.started_at, "run_usage.started_at")?,
        ended_at: ts_from_text(&raw.ended_at, "run_usage.ended_at")?,
        duration_ms: u64_from_i64(raw.duration_ms, "run_usage.duration_ms")?,
        peak_rss_bytes: u64_from_i64(raw.peak_rss_bytes, "run_usage.peak_rss_bytes")?,
        anon_peak_bytes: opt_u64_from_i64(raw.anon_peak_bytes, "run_usage.anon_peak_bytes")?,
        file_peak_bytes: opt_u64_from_i64(raw.file_peak_bytes, "run_usage.file_peak_bytes")?,
        kernel_peak_bytes: opt_u64_from_i64(raw.kernel_peak_bytes, "run_usage.kernel_peak_bytes")?,
        cpu_usec: u64_from_i64(raw.cpu_usec, "run_usage.cpu_usec")?,
        mean_cpu_cores: raw.mean_cpu_cores,
        max_cpu_cores: raw.max_cpu_cores,
        build_mode: enum_from_text(&raw.build_mode, "run_usage.build_mode")?,
        source: enum_from_text(&raw.source, "run_usage.source")?,
        oom_killed: raw.oom_killed != 0,
        exit_code: raw.exit_code,
    })
}

/// Raw values of one `usage_estimates` row.
struct EstimateRow {
    project: String,
    job_kind: String,
    harness: String,
    model: String,
    build_mode: String,
    samples: i64,
    ram_p50_bytes: i64,
    ram_p90_bytes: i64,
    cpu_p50_cores: f64,
    cpu_p90_cores: f64,
    duration_p50_ms: i64,
    duration_p90_ms: i64,
    source: String,
}

/// Reads one `usage_estimates` row.
fn read_estimate_row(row: &Row<'_>) -> rusqlite::Result<EstimateRow> {
    Ok(EstimateRow {
        project: row.get(0)?,
        job_kind: row.get(1)?,
        harness: row.get(2)?,
        model: row.get(3)?,
        build_mode: row.get(4)?,
        samples: row.get(5)?,
        ram_p50_bytes: row.get(6)?,
        ram_p90_bytes: row.get(7)?,
        cpu_p50_cores: row.get(8)?,
        cpu_p90_cores: row.get(9)?,
        duration_p50_ms: row.get(10)?,
        duration_p90_ms: row.get(11)?,
        source: row.get(12)?,
    })
}

/// Encodes an estimate's optional project key.
fn estimate_project(project: Option<&ProjectName>) -> String {
    project.map(ProjectName::as_str).unwrap_or_default().to_owned()
}

/// Encodes an estimate's optional job-kind key.
fn estimate_job_kind(kind: Option<JobKind>) -> Result<String, StoreError> {
    kind.map(|kind| enum_to_text(&kind))
        .transpose()
        .map(|text| text.unwrap_or_default())
}

/// Encodes an estimate's optional harness key.
fn estimate_harness(harness: Option<HarnessKind>) -> Result<String, StoreError> {
    harness
        .map(|harness| enum_to_text(&harness))
        .transpose()
        .map(|text| text.unwrap_or_default())
}

/// Encodes an estimate's optional build-mode key.
fn estimate_build_mode(mode: Option<BuildMode>) -> Result<String, StoreError> {
    enum_to_text(&mode.unwrap_or(BuildMode::Unknown))
}

/// Decodes an estimate's optional project key.
fn decode_estimate_project(text: &str) -> Result<Option<ProjectName>, StoreError> {
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ProjectName::parse(text)?))
    }
}

/// Decodes an estimate's optional job-kind key.
fn decode_estimate_job_kind(text: &str) -> Result<Option<JobKind>, StoreError> {
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(enum_from_text(text, "usage_estimates.job_kind")?))
    }
}

/// Decodes an estimate's optional harness key.
fn decode_estimate_harness(text: &str) -> Result<Option<HarnessKind>, StoreError> {
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(enum_from_text(text, "usage_estimates.harness")?))
    }
}

/// Decodes an estimate's build-mode key; `unknown` is the pooled empty key.
fn decode_estimate_build_mode(text: &str) -> Result<Option<BuildMode>, StoreError> {
    let mode: BuildMode = enum_from_text(text, "usage_estimates.build_mode")?;
    Ok(if mode == BuildMode::Unknown { None } else { Some(mode) })
}

/// Converts a raw estimate row.
fn estimate_from_row(raw: EstimateRow) -> Result<UsageEstimate, StoreError> {
    Ok(UsageEstimate {
        project: decode_estimate_project(&raw.project)?,
        job_kind: decode_estimate_job_kind(&raw.job_kind)?,
        harness: decode_estimate_harness(&raw.harness)?,
        model: if raw.model.is_empty() { None } else { Some(raw.model) },
        build_mode: decode_estimate_build_mode(&raw.build_mode)?,
        samples: u32_from_i64(raw.samples, "usage_estimates.samples")?,
        ram_p50_bytes: u64_from_i64(raw.ram_p50_bytes, "usage_estimates.ram_p50_bytes")?,
        ram_p90_bytes: u64_from_i64(raw.ram_p90_bytes, "usage_estimates.ram_p90_bytes")?,
        cpu_p50_cores: raw.cpu_p50_cores,
        cpu_p90_cores: raw.cpu_p90_cores,
        duration_p50_ms: u64_from_i64(raw.duration_p50_ms, "usage_estimates.duration_p50_ms")?,
        duration_p90_ms: u64_from_i64(raw.duration_p90_ms, "usage_estimates.duration_p90_ms")?,
        source: enum_from_text(&raw.source, "usage_estimates.source")?,
    })
}

/// Raw values of one `capacity_targets` row.
struct TargetRow {
    mode: String,
    target_workers: i64,
    advised_workers: Option<i64>,
    is_override: i64,
    set_by: String,
    at: String,
    reason: String,
}

/// Reads one capacity-target row.
fn read_target_row(row: &Row<'_>) -> rusqlite::Result<TargetRow> {
    Ok(TargetRow {
        mode: row.get(0)?,
        target_workers: row.get(1)?,
        advised_workers: row.get(2)?,
        is_override: row.get(3)?,
        set_by: row.get(4)?,
        at: row.get(5)?,
        reason: row.get(6)?,
    })
}

/// Converts one raw target row.
fn target_from_row(raw: TargetRow) -> Result<CapacityTarget, StoreError> {
    Ok(CapacityTarget {
        mode: enum_from_text(&raw.mode, "capacity_targets.mode")?,
        target_workers: u32_from_i64(raw.target_workers, "capacity_targets.target_workers")?,
        advised_workers: raw
            .advised_workers
            .map(|value| u32_from_i64(value, "capacity_targets.advised_workers"))
            .transpose()?,
        is_override: raw.is_override != 0,
        set_by: raw.set_by,
        set_at: ts_from_text(&raw.at, "capacity_targets.at")?,
        reason: raw.reason,
    })
}

/// Raw values of one `capacity_advice` row.
struct AdviceRow {
    id: String,
    created_at: String,
    current_target: i64,
    proposed_target: i64,
    mode: String,
    reasons_json: String,
    expected_effect: String,
    inbox_id: Option<String>,
    state: String,
    applied_at: Option<String>,
    applied_by: Option<String>,
}

/// Reads one capacity-advice row.
fn read_advice_row(row: &Row<'_>) -> rusqlite::Result<AdviceRow> {
    Ok(AdviceRow {
        id: row.get(0)?,
        created_at: row.get(1)?,
        current_target: row.get(2)?,
        proposed_target: row.get(3)?,
        mode: row.get(4)?,
        reasons_json: row.get(5)?,
        expected_effect: row.get(6)?,
        inbox_id: row.get(7)?,
        state: row.get(8)?,
        applied_at: row.get(9)?,
        applied_by: row.get(10)?,
    })
}

/// Converts one raw advice row.
fn advice_from_row(raw: AdviceRow) -> Result<CapacityAdvice, StoreError> {
    Ok(CapacityAdvice {
        id: raw.id,
        created_at: ts_from_text(&raw.created_at, "capacity_advice.created_at")?,
        current_target: u32_from_i64(raw.current_target, "capacity_advice.current_target")?,
        proposed_target: u32_from_i64(raw.proposed_target, "capacity_advice.proposed_target")?,
        mode: enum_from_text(&raw.mode, "capacity_advice.mode")?,
        reasons: json_from_text(&raw.reasons_json, "capacity_advice.reasons_json")?,
        expected_effect: raw.expected_effect,
        inbox_id: match raw.inbox_id {
            Some(id) => Some(vowp_protocol::InboxId::parse(&id)?),
            None => None,
        },
        state: enum_from_text(&raw.state, "capacity_advice.state")?,
        applied_at: opt_ts_from_text(raw.applied_at.as_deref(), "capacity_advice.applied_at")?,
        applied_by: raw.applied_by,
    })
}

impl Store {
    /// Inserts one host sample.
    pub fn insert_host_sample(&self, sample: &HostSample) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.host_samples (
                at, interval_secs, mem_total_bytes, mem_available_bytes, swap_used_bytes,
                load1, load5, load15, cpu_count, cpu_quota_cores, disk_free_home_bytes,
                disk_free_worktrees_bytes, pressure_cpu_avg10, pressure_mem_avg10,
                pressure_io_avg10, source
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                ts_to_text(sample.at),
                i64_from_u64(sample.interval_secs, "host_samples.interval_secs")?,
                i64_from_u64(sample.mem_total_bytes, "host_samples.mem_total_bytes")?,
                i64_from_u64(sample.mem_available_bytes, "host_samples.mem_available_bytes")?,
                i64_from_u64(sample.swap_used_bytes, "host_samples.swap_used_bytes")?,
                sample.load1,
                sample.load5,
                sample.load15,
                i64::from(sample.cpu_count),
                sample.cpu_quota_cores,
                i64_from_u64(sample.disk_free_home_bytes, "host_samples.disk_free_home_bytes")?,
                opt_i64_from_u64(
                    sample.disk_free_worktrees_bytes,
                    "host_samples.disk_free_worktrees_bytes"
                )?,
                sample.pressure_cpu_avg10,
                sample.pressure_mem_avg10,
                sample.pressure_io_avg10,
                enum_to_text(&sample.source)?,
            ],
        )?;
        Ok(())
    }

    /// Returns every host sample at or after `since`, oldest first.
    pub fn host_samples_since(&self, since: Timestamp) -> Result<Vec<HostSample>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {HOST_SAMPLE_COLUMNS} FROM main.host_samples WHERE at >= ?1 ORDER BY at, id"
        ))?;
        let rows = statement.query_map([ts_to_text(since)], read_host_sample_row)?;
        let mut samples = Vec::new();
        for row in rows {
            samples.push(host_sample_from_row(row?)?);
        }
        Ok(samples)
    }

    /// Deletes old host samples and returns the number removed.
    ///
    /// Full-rate rows older than `before` go first; every row older than `minute_before`
    /// goes too. Minute averages are written by the lifecycle job and carry
    /// `interval_secs >= 60`, so they survive until the second cutoff.
    pub fn prune_host_samples(&self, before: Timestamp, minute_before: Timestamp) -> Result<u64, StoreError> {
        let removed = self.conn.execute(
            "DELETE FROM main.host_samples
             WHERE at < ?2 OR (at < ?1 AND interval_secs < 60)",
            params![ts_to_text(before), ts_to_text(minute_before)],
        )?;
        Ok(u64::try_from(removed).unwrap_or(u64::MAX))
    }

    /// Inserts or replaces one finished run's usage.
    pub fn upsert_run_usage(&self, usage: &RunUsage) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.run_usage (
                run_id, project, job_id, job_kind, role, harness, model, provider, node,
                started_at, ended_at, duration_ms, peak_rss_bytes, anon_peak_bytes,
                file_peak_bytes, kernel_peak_bytes, cpu_usec, mean_cpu_cores, max_cpu_cores,
                build_mode, source, oom_killed, exit_code
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21, ?22, ?23
             )
             ON CONFLICT(run_id) DO UPDATE SET
                project = excluded.project, job_id = excluded.job_id, job_kind = excluded.job_kind,
                role = excluded.role, harness = excluded.harness, model = excluded.model,
                provider = excluded.provider, node = excluded.node, started_at = excluded.started_at,
                ended_at = excluded.ended_at, duration_ms = excluded.duration_ms,
                peak_rss_bytes = excluded.peak_rss_bytes, anon_peak_bytes = excluded.anon_peak_bytes,
                file_peak_bytes = excluded.file_peak_bytes, kernel_peak_bytes = excluded.kernel_peak_bytes,
                cpu_usec = excluded.cpu_usec, mean_cpu_cores = excluded.mean_cpu_cores,
                max_cpu_cores = excluded.max_cpu_cores, build_mode = excluded.build_mode,
                source = excluded.source, oom_killed = excluded.oom_killed, exit_code = excluded.exit_code",
            params![
                usage.run_id.as_str(),
                usage.project.as_str(),
                usage.job_id.as_ref().map(JobId::as_str),
                usage.job_kind.map(|kind| enum_to_text(&kind)).transpose()?,
                enum_to_text(&usage.role)?,
                usage.harness.map(|harness| enum_to_text(&harness)).transpose()?,
                usage.model,
                usage.provider,
                usage.node.as_str(),
                ts_to_text(usage.started_at),
                ts_to_text(usage.ended_at),
                i64_from_u64(usage.duration_ms, "run_usage.duration_ms")?,
                i64_from_u64(usage.peak_rss_bytes, "run_usage.peak_rss_bytes")?,
                opt_i64_from_u64(usage.anon_peak_bytes, "run_usage.anon_peak_bytes")?,
                opt_i64_from_u64(usage.file_peak_bytes, "run_usage.file_peak_bytes")?,
                opt_i64_from_u64(usage.kernel_peak_bytes, "run_usage.kernel_peak_bytes")?,
                i64_from_u64(usage.cpu_usec, "run_usage.cpu_usec")?,
                usage.mean_cpu_cores,
                usage.max_cpu_cores,
                enum_to_text(&usage.build_mode)?,
                enum_to_text(&usage.source)?,
                bool_to_int(usage.oom_killed),
                usage.exit_code,
            ],
        )?;
        Ok(())
    }

    /// Returns one run's usage, when it has a row.
    pub fn run_usage(&self, run: &RunId) -> Result<Option<RunUsage>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {RUN_USAGE_COLUMNS} FROM main.run_usage WHERE run_id = ?1"),
                [run.as_str()],
                read_run_usage_row,
            )
            .optional()?;
        raw.map(run_usage_from_row).transpose()
    }

    /// Returns up to `limit` newest usage rows matching `query`.
    pub fn run_usage_for_group(&self, query: &UsageQuery, limit: u32) -> Result<Vec<RunUsage>, StoreError> {
        let mut sql = format!("SELECT {RUN_USAGE_COLUMNS} FROM main.run_usage WHERE 1 = 1");
        let mut values: Vec<Value> = Vec::new();
        if let Some(project) = &query.project {
            sql.push_str(" AND project = ?");
            values.push(Value::Text(project.as_str().to_owned()));
        }
        if let Some(kind) = query.job_kind {
            sql.push_str(" AND job_kind = ?");
            values.push(Value::Text(enum_to_text(&kind)?));
        }
        if let Some(harness) = query.harness {
            sql.push_str(" AND harness = ?");
            values.push(Value::Text(enum_to_text(&harness)?));
        }
        if let Some(model) = &query.model {
            sql.push_str(" AND model = ?");
            values.push(Value::Text(model.clone()));
        }
        if let Some(mode) = query.build_mode {
            sql.push_str(" AND build_mode = ?");
            values.push(Value::Text(enum_to_text(&mode)?));
        }
        if let Some(since) = query.since {
            sql.push_str(" AND started_at >= ?");
            values.push(Value::Text(ts_to_text(since)));
        }
        sql.push_str(" ORDER BY started_at DESC, run_id DESC LIMIT ?");
        values.push(Value::Integer(i64::from(limit.max(1))));
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), read_run_usage_row)?;
        let mut usage = Vec::new();
        for row in rows {
            usage.push(run_usage_from_row(row?)?);
        }
        Ok(usage)
    }

    /// Inserts or replaces one pooled usage estimate.
    pub fn upsert_usage_estimate(&self, estimate: &UsageEstimate) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.usage_estimates (
                project, job_kind, harness, model, build_mode, samples, ram_p50_bytes,
                ram_p90_bytes, cpu_p50_cores, cpu_p90_cores, duration_p50_ms,
                duration_p90_ms, source, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(project, job_kind, harness, model, build_mode) DO UPDATE SET
                samples = excluded.samples, ram_p50_bytes = excluded.ram_p50_bytes,
                ram_p90_bytes = excluded.ram_p90_bytes, cpu_p50_cores = excluded.cpu_p50_cores,
                cpu_p90_cores = excluded.cpu_p90_cores, duration_p50_ms = excluded.duration_p50_ms,
                duration_p90_ms = excluded.duration_p90_ms, source = excluded.source,
                updated_at = excluded.updated_at",
            params![
                estimate_project(estimate.project.as_ref()),
                estimate_job_kind(estimate.job_kind)?,
                estimate_harness(estimate.harness)?,
                estimate.model.clone().unwrap_or_default(),
                estimate_build_mode(estimate.build_mode)?,
                i64::from(estimate.samples),
                i64_from_u64(estimate.ram_p50_bytes, "usage_estimates.ram_p50_bytes")?,
                i64_from_u64(estimate.ram_p90_bytes, "usage_estimates.ram_p90_bytes")?,
                estimate.cpu_p50_cores,
                estimate.cpu_p90_cores,
                i64_from_u64(estimate.duration_p50_ms, "usage_estimates.duration_p50_ms")?,
                i64_from_u64(estimate.duration_p90_ms, "usage_estimates.duration_p90_ms")?,
                enum_to_text(&estimate.source)?,
                ts_to_text(self.clock().now()),
            ],
        )?;
        Ok(())
    }

    /// Returns the estimate stored under exactly `key`.
    pub fn usage_estimate(&self, key: &EstimateKey) -> Result<Option<UsageEstimate>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!(
                    "SELECT {ESTIMATE_COLUMNS} FROM main.usage_estimates
                     WHERE project = ?1 AND job_kind = ?2 AND harness = ?3 AND model = ?4
                       AND build_mode = ?5"
                ),
                params![
                    estimate_project(key.project.as_ref()),
                    estimate_job_kind(key.job_kind)?,
                    estimate_harness(key.harness)?,
                    key.model.clone().unwrap_or_default(),
                    estimate_build_mode(key.build_mode)?,
                ],
                read_estimate_row,
            )
            .optional()?;
        raw.map(estimate_from_row).transpose()
    }

    /// Lists estimates, optionally restricted to one project.
    pub fn list_usage_estimates(&self, project: Option<&ProjectName>) -> Result<Vec<UsageEstimate>, StoreError> {
        let mut sql = format!("SELECT {ESTIMATE_COLUMNS} FROM main.usage_estimates");
        let mut values: Vec<Value> = Vec::new();
        if let Some(project) = project {
            sql.push_str(" WHERE project = ?");
            values.push(Value::Text(project.as_str().to_owned()));
        }
        sql.push_str(" ORDER BY project, job_kind, harness, model, build_mode");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), read_estimate_row)?;
        let mut estimates = Vec::new();
        for row in rows {
            estimates.push(estimate_from_row(row?)?);
        }
        Ok(estimates)
    }

    /// Inserts one capacity-target history row and returns its row id.
    pub fn insert_capacity_target(&self, target: &CapacityTarget) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO main.capacity_targets (
                at, mode, target_workers, advised_workers, is_override, set_by, reason, advice_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                ts_to_text(target.set_at),
                enum_to_text(&target.mode)?,
                i64::from(target.target_workers),
                target.advised_workers.map(i64::from),
                bool_to_int(target.is_override),
                target.set_by,
                target.reason,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Returns the newest capacity target, when one has been set.
    pub fn latest_capacity_target(&self) -> Result<Option<CapacityTarget>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {TARGET_COLUMNS} FROM main.capacity_targets ORDER BY at DESC, id DESC LIMIT 1"),
                [],
                read_target_row,
            )
            .optional()?;
        raw.map(target_from_row).transpose()
    }

    /// Inserts one capacity advice row.
    pub fn insert_capacity_advice(&self, advice: &CapacityAdvice) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO main.capacity_advice (
                id, created_at, current_target, proposed_target, mode, reasons_json,
                expected_effect, inbox_id, state, applied_at, applied_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                advice.id,
                ts_to_text(advice.created_at),
                i64::from(advice.current_target),
                i64::from(advice.proposed_target),
                enum_to_text(&advice.mode)?,
                json_to_text(&advice.reasons)?,
                advice.expected_effect,
                advice.inbox_id.as_ref().map(vowp_protocol::InboxId::as_str),
                enum_to_text(&advice.state)?,
                advice.applied_at.map(ts_to_text),
                advice.applied_by,
            ],
        )?;
        Ok(())
    }

    /// Returns one capacity advice row.
    pub fn capacity_advice(&self, id: &str) -> Result<Option<CapacityAdvice>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {ADVICE_COLUMNS} FROM main.capacity_advice WHERE id = ?1"),
                [id],
                read_advice_row,
            )
            .optional()?;
        raw.map(advice_from_row).transpose()
    }

    /// Returns the newest open advice row.
    pub fn latest_open_advice(&self) -> Result<Option<CapacityAdvice>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!(
                    "SELECT {ADVICE_COLUMNS} FROM main.capacity_advice
                     WHERE state = 'open' ORDER BY created_at DESC, id DESC LIMIT 1"
                ),
                [],
                read_advice_row,
            )
            .optional()?;
        raw.map(advice_from_row).transpose()
    }

    /// Marks one advice row applied or dismissed.
    pub fn finish_capacity_advice(
        &self,
        id: &str,
        state: AdviceState,
        by: &str,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE main.capacity_advice SET state = ?1, applied_at = ?2, applied_by = ?3 WHERE id = ?4",
            params![enum_to_text(&state)?, ts_to_text(now), by, id],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("capacity advice", id));
        }
        Ok(())
    }
}
