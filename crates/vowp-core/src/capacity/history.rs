//! Bounded capacity history: 5-minute rollups and the read-only history document.
//!
//! Spec: `docs/design/v0.4/hardening.md` §7.2–§7.3. The sampler writes one rollup for the
//! previous completed bucket on every machine tick; raw `host_samples` are kept for 48 hours
//! and rollups for 30 days. [`read`] builds a [`CapacityHistory`] from stored rollups only,
//! so it never writes a row and never invents a point for a missing bucket.

use std::time::Duration;

use vowp_protocol::{CapacityHistory, CapacityMemory, CapacityPoint, HistorySource, Timestamp};
use vowp_store::Store;
use vowp_store::repo_capacity::CapacityRollup;

use crate::CoreError;

/// Width of one stored capacity rollup in seconds (5 minutes).
pub const ROLLUP_BUCKET_SECS: u64 = 300;

/// How long raw `host_samples` rows are kept.
pub const RAW_RETENTION_SECS: u64 = 48 * 60 * 60;

/// How long `capacity_rollups` rows are kept.
pub const ROLLUP_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

/// Width of the points a caller asks the history reader for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryBucket {
    /// One stored 5-minute rollup per point.
    FiveMinutes,
    /// Read-time one-hour aggregation of the 5-minute rollups.
    Hour,
    /// Read-time one-day aggregation of the 5-minute rollups.
    Day,
}

impl HistoryBucket {
    /// Every bucket width the history reader accepts, in flag order.
    pub const ALL: [HistoryBucket; 3] = [HistoryBucket::FiveMinutes, HistoryBucket::Hour, HistoryBucket::Day];

    /// Returns the bucket width in seconds.
    pub fn secs(self) -> u64 {
        match self {
            HistoryBucket::FiveMinutes => ROLLUP_BUCKET_SECS,
            HistoryBucket::Hour => 60 * 60,
            HistoryBucket::Day => 24 * 60 * 60,
        }
    }

    /// Returns the wire/flag name (`5m`, `1h` or `1d`).
    pub fn label(self) -> &'static str {
        match self {
            HistoryBucket::FiveMinutes => "5m",
            HistoryBucket::Hour => "1h",
            HistoryBucket::Day => "1d",
        }
    }

    /// Parses `5m`, `1h` or `1d`; other values are refused for the CLI to report.
    pub fn parse(text: &str) -> Option<HistoryBucket> {
        Self::ALL.into_iter().find(|bucket| bucket.label() == text)
    }
}

/// Writes the rollup for the previous completed 5-minute bucket.
///
/// Returns `false` when that bucket has no raw samples (a gap is not a zero). A repeated
/// tick updates the same row in place, so the write is idempotent.
pub fn record_rollup(store: &Store, now: Timestamp) -> Result<bool, CoreError> {
    let bucket = previous_bucket_start(now);
    Ok(store.rollup_host_samples(bucket, ROLLUP_BUCKET_SECS)?)
}

/// Reads the retained capacity history without writing anything.
///
/// `since` defaults to 24 hours ago and is clamped to the 30-day rollup window (and to
/// `now`). The returned document is empty when no rollup is retained in range; the window
/// fields still describe what was asked so an empty home is visibly empty, not broken.
pub fn read(
    store: &Store,
    now: Timestamp,
    since: Option<Timestamp>,
    bucket: HistoryBucket,
) -> Result<CapacityHistory, CoreError> {
    let retention_start = bucket_start(
        now.minus(Duration::from_secs(ROLLUP_RETENTION_SECS)),
        ROLLUP_BUCKET_SECS,
    );
    let requested = since.unwrap_or_else(|| now.minus(Duration::from_secs(24 * 60 * 60)));
    let since = bucket_start(requested.max(retention_start).min(now), ROLLUP_BUCKET_SECS);
    let rollups = store.capacity_rollups_since(since, ROLLUP_BUCKET_SECS)?;
    Ok(CapacityHistory {
        generated_at: now,
        since,
        until: now,
        bucket_secs: bucket.secs(),
        retention_secs: ROLLUP_RETENTION_SECS,
        points: aggregate(&rollups, bucket.secs()),
    })
}

/// Returns the start of the completed bucket before `now`.
fn previous_bucket_start(now: Timestamp) -> Timestamp {
    bucket_start(now, ROLLUP_BUCKET_SECS).minus(Duration::from_secs(ROLLUP_BUCKET_SECS))
}

/// Floors `at` to the start of its `secs`-wide UTC bucket.
fn bucket_start(at: Timestamp, secs: u64) -> Timestamp {
    let Ok(secs) = i64::try_from(secs) else {
        return at;
    };
    if secs <= 0 {
        return at;
    }
    Timestamp::from_unix_secs(at.unix_secs().div_euclid(secs) * secs)
}

/// One bucket while read-time points are being merged.
struct Accumulator {
    at: Timestamp,
    samples: u64,
    mem_min: u64,
    mem_max: u64,
    mem_sum: u128,
    load_sum: f64,
    disk_min: u64,
    source: HistorySource,
}

impl Accumulator {
    /// Starts an accumulator from one stored rollup.
    fn from_rollup(at: Timestamp, rollup: &CapacityRollup) -> Accumulator {
        let samples = u64::from(rollup.samples);
        Accumulator {
            at,
            samples,
            mem_min: rollup.mem_available_min_bytes,
            mem_max: rollup.mem_available_max_bytes,
            mem_sum: u128::from(rollup.mem_available_avg_bytes) * u128::from(samples),
            load_sum: rollup.load1_avg * samples as f64,
            disk_min: rollup.disk_free_home_min_bytes,
            source: rollup.source,
        }
    }

    /// Merges one more rollup into the bucket, weighting means by `samples`.
    fn merge(&mut self, rollup: &CapacityRollup) {
        let samples = u64::from(rollup.samples);
        self.samples = self.samples.saturating_add(samples);
        self.mem_min = self.mem_min.min(rollup.mem_available_min_bytes);
        self.mem_max = self.mem_max.max(rollup.mem_available_max_bytes);
        self.mem_sum += u128::from(rollup.mem_available_avg_bytes) * u128::from(samples);
        self.load_sum += rollup.load1_avg * samples as f64;
        self.disk_min = self.disk_min.min(rollup.disk_free_home_min_bytes);
        if self.source != rollup.source {
            self.source = HistorySource::Mixed;
        }
    }

    /// Finishes the bucket as one wire point.
    fn finish(self) -> CapacityPoint {
        let samples = u32::try_from(self.samples).unwrap_or(u32::MAX);
        let avg = if self.samples == 0 {
            0
        } else {
            let rounded = (self.mem_sum + u128::from(self.samples) / 2) / u128::from(self.samples);
            u64::try_from(rounded).unwrap_or(u64::MAX)
        };
        CapacityPoint {
            at: self.at,
            samples,
            mem_available: CapacityMemory {
                min: self.mem_min,
                avg,
                max: self.mem_max,
            },
            load1_avg: if self.samples == 0 {
                0.0
            } else {
                self.load_sum / self.samples as f64
            },
            disk_free_home_min: self.disk_min,
            source: self.source,
        }
    }
}

/// Groups stored 5-minute rollups into `bucket_secs`-wide read-time points.
fn aggregate(rollups: &[CapacityRollup], bucket_secs: u64) -> Vec<CapacityPoint> {
    let mut points: Vec<CapacityPoint> = Vec::new();
    let mut current: Option<Accumulator> = None;
    for rollup in rollups {
        let at = bucket_start(rollup.bucket, bucket_secs);
        if current.as_ref().is_some_and(|accumulator| accumulator.at == at) {
            if let Some(accumulator) = current.as_mut() {
                accumulator.merge(rollup);
            }
        } else {
            if let Some(finished) = current.take() {
                points.push(finished.finish());
            }
            current = Some(Accumulator::from_rollup(at, rollup));
        }
    }
    if let Some(finished) = current {
        points.push(finished.finish());
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("timestamp")
    }

    #[test]
    fn history_buckets_parse_and_name_the_accepted_widths() {
        assert_eq!(HistoryBucket::parse("5m"), Some(HistoryBucket::FiveMinutes));
        assert_eq!(HistoryBucket::parse("1h"), Some(HistoryBucket::Hour));
        assert_eq!(HistoryBucket::parse("1d"), Some(HistoryBucket::Day));
        assert_eq!(HistoryBucket::parse("30m"), None);
        assert_eq!(HistoryBucket::FiveMinutes.secs(), 300);
        assert_eq!(HistoryBucket::Hour.secs(), 3600);
        assert_eq!(HistoryBucket::Day.secs(), 86_400);
    }

    #[test]
    fn bucket_start_floors_to_utc_boundaries() {
        let at = ts("2026-11-10T12:07:42.123Z");
        assert_eq!(bucket_start(at, 300), ts("2026-11-10T12:05:00Z"));
        assert_eq!(bucket_start(at, 3600), ts("2026-11-10T12:00:00Z"));
        assert_eq!(bucket_start(at, 86_400), ts("2026-11-10T00:00:00Z"));
    }

    #[test]
    fn previous_bucket_is_the_last_completed_five_minutes() {
        assert_eq!(
            previous_bucket_start(ts("2026-11-10T12:07:42Z")),
            ts("2026-11-10T12:00:00Z")
        );
        assert_eq!(
            previous_bucket_start(ts("2026-11-10T12:05:00Z")),
            ts("2026-11-10T12:00:00Z")
        );
    }
}
