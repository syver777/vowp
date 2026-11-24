//! Capacity metrics storage: the v7 rollup table, its aggregation and its retention.
//! Spec: `docs/design/v0.4/hardening.md` §7.2.

use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{HistorySource, HostSample, SampleSource, Timestamp};
use vowp_store::{NodePaths, Store, StoreError};

/// A timestamp inside one 5-minute bucket.
fn at(offset_secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(1_800_000_000 + offset_secs)
}

/// Builds one raw host sample.
fn host_sample(at: Timestamp, mem_available: u64, load1: f64, disk_free: u64, source: SampleSource) -> HostSample {
    HostSample {
        at,
        interval_secs: 5,
        mem_total_bytes: 8_000_000_000,
        mem_available_bytes: mem_available,
        swap_used_bytes: 0,
        load1,
        load5: load1,
        load15: load1,
        cpu_count: 4,
        cpu_quota_cores: None,
        disk_free_home_bytes: disk_free,
        disk_free_worktrees_bytes: None,
        pressure_cpu_avg10: None,
        pressure_mem_avg10: None,
        pressure_io_avg10: None,
        source,
    }
}

/// Opens a temporary home.
fn store() -> (TempDir, NodePaths, Store) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = NodePaths::new(tmp.path());
    let store = Store::open(&paths).expect("open store");
    (tmp, paths, store)
}

#[test]
fn capacity_rollups_table_has_the_v7_columns_and_index() {
    let (_tmp, paths, _store) = store();
    let conn = Connection::open(paths.db_path()).expect("global db");
    let mut statement = conn.prepare("PRAGMA table_info(capacity_rollups)").expect("table info");
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("columns")
        .collect::<Result<_, _>>()
        .expect("column names");
    assert_eq!(
        columns,
        vec![
            "bucket",
            "bucket_secs",
            "samples",
            "mem_available_min_bytes",
            "mem_available_avg_bytes",
            "mem_available_max_bytes",
            "load1_avg",
            "disk_free_home_min_bytes",
            "source",
        ]
    );
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'capacity_rollups_bucket'",
            [],
            |row| row.get(0),
        )
        .expect("index");
    assert_eq!(index, 1, "the bucket index backs range reads and pruning");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version");
    assert_eq!(version, 8);
}

#[test]
fn rollup_host_samples_aggregates_the_bucket_and_skips_empty_ones() {
    let (_tmp, _paths, store) = store();
    let base = at(0);
    for (offset, mem, load, disk) in [(10, 1000, 0.5, 900), (70, 3000, 1.5, 700), (130, 2000, 2.5, 800)] {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                mem,
                load,
                disk,
                SampleSource::Proc,
            ))
            .expect("sample");
    }

    assert!(store.rollup_host_samples(base, 300).expect("rollup"));
    let rows = store.capacity_rollups_since(base, 300).expect("rollups");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, base);
    assert_eq!(rows[0].samples, 3);
    assert_eq!(rows[0].mem_available_min_bytes, 1000);
    assert_eq!(rows[0].mem_available_avg_bytes, 2000);
    assert_eq!(rows[0].mem_available_max_bytes, 3000);
    assert_eq!(rows[0].load1_avg, 1.5);
    assert_eq!(rows[0].disk_free_home_min_bytes, 700);
    assert_eq!(rows[0].source, HistorySource::Proc);

    // An empty bucket is a gap, not a zero row.
    assert!(
        !store
            .rollup_host_samples(base.plus(Duration::from_secs(300)), 300)
            .expect("empty")
    );
    assert_eq!(store.capacity_rollups_since(base, 300).expect("rollups").len(), 1);
}

#[test]
fn rollup_host_samples_is_idempotent_across_a_repeated_write() {
    let (_tmp, _paths, store) = store();
    let base = at(0);
    for offset in [10, 20, 30] {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                1000,
                1.0,
                900,
                SampleSource::Proc,
            ))
            .expect("sample");
    }
    assert!(store.rollup_host_samples(base, 300).expect("first"));
    assert!(store.rollup_host_samples(base, 300).expect("repeated"));
    let rows = store.capacity_rollups_since(base, 300).expect("rollups");
    assert_eq!(rows.len(), 1, "a repeated write must not duplicate the bucket");
    assert_eq!(rows[0].samples, 3);

    // A fallback and a proc sample in one bucket label the rollup mixed.
    store
        .insert_host_sample(&host_sample(
            base.plus(Duration::from_secs(240)),
            3000,
            3.0,
            700,
            SampleSource::Fallback,
        ))
        .expect("fallback sample");
    assert!(store.rollup_host_samples(base, 300).expect("recomputed"));
    let rows = store.capacity_rollups_since(base, 300).expect("rollups");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].samples, 4);
    assert_eq!(rows[0].source, HistorySource::Mixed);
}

#[test]
fn prune_capacity_rollups_keeps_thirty_days() {
    let (_tmp, _paths, store) = store();
    let day = 24 * 60 * 60_u64;
    let base = at(0);
    let old_bucket = base.minus(Duration::from_secs(40 * day));
    let recent_bucket = base.minus(Duration::from_secs(20 * day));
    for bucket in [old_bucket, recent_bucket] {
        store
            .insert_host_sample(&host_sample(
                bucket.plus(Duration::from_secs(10)),
                1000,
                1.0,
                900,
                SampleSource::Proc,
            ))
            .expect("sample");
        assert!(store.rollup_host_samples(bucket, 300).expect("rollup"));
    }

    let removed = store
        .prune_capacity_rollups(base.minus(Duration::from_secs(30 * day)))
        .expect("prune");
    assert_eq!(removed, 1, "only the 40-day-old bucket goes");
    let rows = store
        .capacity_rollups_since(base.minus(Duration::from_secs(60 * day)), 300)
        .expect("rollups");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, recent_bucket);

    // The bucket that contains the cutoff instant is newer than the cutoff and stays.
    let removed = store
        .prune_capacity_rollups(base.minus(Duration::from_secs(30 * day)))
        .expect("prune again");
    assert_eq!(removed, 0);
}

#[test]
fn rollup_reads_report_migration_required_on_a_v6_file() {
    let (_tmp, paths, _store) = store();
    {
        let conn = Connection::open(paths.db_path()).expect("global db");
        conn.execute_batch("DROP TABLE capacity_rollups; PRAGMA user_version = 6;")
            .expect("rewind to v6");
    }
    let read_only = Store::open_read_only(&paths, None).expect("read a v6 home");
    let error = read_only
        .capacity_rollups_since(at(0), 300)
        .expect_err("a v6 file has no rollup table");
    assert!(matches!(error, StoreError::MigrationRequired), "got {error:?}");
}
