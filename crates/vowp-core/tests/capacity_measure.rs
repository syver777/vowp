//! Capacity measurement tests: sampler, scopes, usage rows and estimates.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1 and the capacity test list in §15.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use tempfile::tempdir;
use vowp_adapters::proc::{ProcUsage, RunHandle, SpawnSpec, child_env};
use vowp_adapters::stream::StreamParser;
use vowp_core::capacity::sampler::sample_with;
use vowp_core::capacity::{self, FakeProcFs, HistoryBucket, RunScope, scope};
use vowp_core::gates::{GateCtx, GateRunner};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    Actor, AdapterRun, AdmitRequest, AdviceState, BuildMode, CapacityAdvice, CapacityTarget, CmdGate, Cost, EventBody,
    FakeClock, Gate, HarnessEvent, HarnessKind, HostSample, Job, JobId, JobKind, LedgerEntry, LedgerKind, MemoryLog,
    NewJob, NodeName, Note, Project, ProjectName, Role, Run, RunId, RunOutcome, RunPurpose, RunRole, RunSummary,
    RunUsage, SampleSource, StopReason, TargetMode, Timestamp, TurnEnded, UsageSource, VowpConfig,
};
use vowp_store::{NodePaths, Store};

fn now() -> Timestamp {
    Timestamp::parse("2026-11-10T12:00:00Z").unwrap()
}

fn test_deps(root: &Path) -> Deps {
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(VowpConfig::with_defaults_for_tests()),
        node: NodeName::parse("local").unwrap(),
        clock: Arc::new(FakeClock::at(now())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(SystemGit::default()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

fn test_store(root: &Path) -> Store {
    Store::open(&NodePaths::new(root))
        .unwrap()
        .with_clock(Arc::new(FakeClock::at(now())))
}

/// Builds one raw host sample with the fields the rollup aggregates and prunes.
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

/// Floors a timestamp to a whole bucket, matching the history reader.
fn bucket_start(at: Timestamp, secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(at.unix_secs().div_euclid(secs) * secs)
}

fn insert_project(store: &Store) -> ProjectName {
    let project = ProjectName::parse("test").unwrap();
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: "/tmp/test-project".to_owned(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now(),
            updated_at: now(),
        })
        .unwrap();
    project
}

fn insert_job(store: &Store, project: &ProjectName, id: &str) -> Job {
    let new = NewJob {
        id: JobId::parse(id).unwrap(),
        title: format!("job {id}"),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: None,
        needs: Vec::new(),
        owns: vec!["src/**".to_owned()],
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    };
    store.insert_job(project, &new, "test").unwrap()
}

fn insert_run(store: &Store, job: &Job, run_id: &RunId, node: &NodeName) -> Run {
    insert_run_with_cwd(store, job, run_id, node, None)
}

fn insert_run_with_cwd(store: &Store, job: &Job, run_id: &RunId, node: &NodeName, cwd: Option<String>) -> Run {
    let run = Run {
        id: run_id.clone(),
        job_id: Some(job.id.clone()),
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(vowp_protocol::Executor::parse("opencode:deepseek/flash").unwrap()),
        provider: Some("deepseek".to_owned()),
        node: node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd,
        started_at: now(),
        ended_at: None,
        exit_code: Some(0),
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).unwrap();
    run
}

fn default_sidecar(run_id: &RunId) -> ProcUsage {
    ProcUsage {
        run_id: run_id.as_str().to_owned(),
        started_at: now(),
        ended_at: now().plus(Duration::from_secs(1)),
        duration_ms: 1_000,
        peak_rss_bytes: scope::DEFAULT_PEAK_RSS_BYTES,
        anon_peak_bytes: None,
        file_peak_bytes: None,
        kernel_peak_bytes: None,
        cpu_usec: 0,
        mean_cpu_cores: scope::DEFAULT_MEAN_CPU_CORES,
        max_cpu_cores: scope::DEFAULT_MEAN_CPU_CORES,
        build_mode: BuildMode::Unknown,
        source: UsageSource::Default,
        oom_killed: false,
        exit_code: Some(0),
    }
}

fn usage_row(project: &ProjectName, kind: JobKind, model: &str, peak: u64, index: u64) -> RunUsage {
    RunUsage {
        run_id: RunId::new(),
        project: project.clone(),
        job_id: None,
        job_kind: Some(kind),
        role: RunRole::Worker,
        harness: Some(HarnessKind::Opencode),
        model: Some(model.to_owned()),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").unwrap(),
        started_at: now().plus(Duration::from_secs(index)),
        ended_at: now().plus(Duration::from_secs(index + 1)),
        duration_ms: 1_000,
        peak_rss_bytes: peak,
        anon_peak_bytes: Some(peak / 2),
        file_peak_bytes: Some(peak / 4),
        kernel_peak_bytes: None,
        cpu_usec: 500_000,
        mean_cpu_cores: 0.5,
        max_cpu_cores: 1.0,
        build_mode: BuildMode::Unknown,
        source: UsageSource::ProcTree,
        oom_killed: false,
        exit_code: Some(0),
    }
}

fn admit_request(project: &ProjectName, model: &str) -> AdmitRequest {
    AdmitRequest {
        project: project.clone(),
        job: None,
        kind: JobKind::Build,
        role: RunRole::Worker,
        harness: Some(HarnessKind::Opencode),
        model: Some(model.to_owned()),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").unwrap(),
    }
}

#[test]
fn sampler_parses_proc_meminfo_load_and_pressure() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let mut fake = FakeProcFs::default();
    fake.files.insert(
        "/proc/meminfo".to_owned(),
        "MemTotal: 8000000 kB\nMemAvailable: 6000000 kB\nSwapTotal: 1000000 kB\nSwapFree: 750000 kB\n".to_owned(),
    );
    fake.files
        .insert("/proc/loadavg".to_owned(), "0.50 1.25 2.00 2/300 1234\n".to_owned());
    fake.files.insert(
        "/proc/cpuinfo".to_owned(),
        "processor\t: 0\nprocessor\t: 1\nprocessor\t: 2\nprocessor\t: 3\n".to_owned(),
    );
    fake.files
        .insert("/sys/fs/cgroup/cpu.max".to_owned(), "200000 100000\n".to_owned());
    fake.files.insert(
        "/proc/pressure/cpu".to_owned(),
        "some avg10=1.50 avg60=1.00 avg300=0.50 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n".to_owned(),
    );
    fake.files.insert(
        "/proc/pressure/memory".to_owned(),
        "some avg10=2.25 avg60=1.00 avg300=0.50 total=1\n".to_owned(),
    );
    fake.files.insert(
        "/proc/pressure/io".to_owned(),
        "some avg10=3.75 avg60=1.00 avg300=0.50 total=1\n".to_owned(),
    );
    fake.disk.insert(root.display().to_string(), 42_000_000_000);

    let sample = sample_with(&fake, &NodePaths::new(&root), None, now(), 5).unwrap();

    assert_eq!(sample.mem_total_bytes, 8_000_000 * 1024);
    assert_eq!(sample.mem_available_bytes, 6_000_000 * 1024);
    assert_eq!(sample.swap_used_bytes, 250_000 * 1024);
    assert_eq!(sample.load1, 0.50);
    assert_eq!(sample.load5, 1.25);
    assert_eq!(sample.load15, 2.00);
    assert_eq!(sample.cpu_count, 4);
    assert_eq!(sample.cpu_quota_cores, Some(2.0));
    assert_eq!(sample.disk_free_home_bytes, 42_000_000_000);
    assert_eq!(sample.pressure_cpu_avg10, Some(1.50));
    assert_eq!(sample.pressure_mem_avg10, Some(2.25));
    assert_eq!(sample.pressure_io_avg10, Some(3.75));
    assert_eq!(sample.source, vowp_protocol::SampleSource::Proc);
}

#[test]
fn sampler_degrades_on_non_linux() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let mut fake = FakeProcFs::default();
    fake.disk.insert(root.display().to_string(), 7_000_000_000);
    let sample = sample_with(&fake, &NodePaths::new(&root), None, now(), 5).unwrap();
    assert_eq!(sample.source, vowp_protocol::SampleSource::Fallback);
    assert_eq!(sample.mem_total_bytes, 0);
    assert_eq!(sample.mem_available_bytes, 0);
    assert_eq!(sample.disk_free_home_bytes, 7_000_000_000);

    // The rollup keeps the fallback label and the real disk number, not a fake memory read.
    store.insert_host_sample(&sample).unwrap();
    assert!(capacity::record_rollup(&store, now().plus(Duration::from_secs(300))).unwrap());
    let history = capacity::read_history(
        &store,
        now(),
        Some(now().minus(Duration::from_secs(3600))),
        HistoryBucket::FiveMinutes,
    )
    .unwrap();
    assert_eq!(history.points.len(), 1);
    let point = &history.points[0];
    assert_eq!(point.source, vowp_protocol::HistorySource::Fallback);
    assert_eq!(point.mem_available.min, 0);
    assert_eq!(point.mem_available.avg, 0);
    assert_eq!(point.mem_available.max, 0);
    assert_eq!(point.load1_avg, 0.0);
    assert_eq!(point.disk_free_home_min, 7_000_000_000);
}

#[test]
fn sampler_prunes_old_rows() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let mut sample = sample_with(
        &FakeProcFs::default(),
        &NodePaths::new(tmp.path().join("node")),
        None,
        now(),
        5,
    )
    .unwrap();
    sample.source = vowp_protocol::SampleSource::Fallback;
    sample.interval_secs = 5;
    sample.at = now().minus(Duration::from_secs(49 * 60 * 60));
    store.insert_host_sample(&sample).unwrap();
    sample.at = now().minus(Duration::from_secs(2 * 24 * 60 * 60 + 1));
    sample.interval_secs = 60;
    store.insert_host_sample(&sample).unwrap();
    sample.at = now().minus(Duration::from_secs(47 * 60 * 60));
    sample.interval_secs = 5;
    store.insert_host_sample(&sample).unwrap();
    sample.at = now();
    sample.interval_secs = 5;
    store.insert_host_sample(&sample).unwrap();

    // Raw rows live for 48 hours; the old minute row is not kept longer than the rest.
    let removed = store
        .prune_host_samples(now().minus(Duration::from_secs(capacity::RAW_RETENTION_SECS)))
        .unwrap();
    assert_eq!(removed, 2);
    let left = store
        .host_samples_since(now().minus(Duration::from_secs(40 * 24 * 60 * 60)))
        .unwrap();
    assert_eq!(left.len(), 2);
    assert!(
        left.iter()
            .all(|sample| sample.at >= now().minus(Duration::from_secs(48 * 60 * 60)))
    );
}

#[test]
fn sampler_tick_writes_one_rollup_for_the_previous_bucket() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let deps = test_deps(&root);
    let base = now();
    store
        .insert_host_sample(&host_sample(
            base.minus(Duration::from_secs(10)),
            1000,
            1.0,
            900,
            SampleSource::Proc,
        ))
        .unwrap();

    assert!(capacity::sample(&deps, &store, base).unwrap().is_some());
    let previous = base.minus(Duration::from_secs(capacity::ROLLUP_BUCKET_SECS));
    let rows = store.capacity_rollups_since(previous, 300).unwrap();
    assert_eq!(rows.len(), 1, "the tick writes exactly one rollup");
    assert_eq!(rows[0].bucket, previous);
    assert_eq!(rows[0].samples, 1, "the current bucket's new sample is not in it");
    assert_eq!(rows[0].mem_available_avg_bytes, 1000);
    assert_eq!(
        store
            .host_samples_since(base.minus(Duration::from_secs(60)))
            .unwrap()
            .len(),
        2,
        "the tick still writes the raw sample"
    );
}

#[test]
fn capacity_history_buckets_raw_samples_into_five_minute_points() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let base = Timestamp::parse("2026-11-10T12:00:00Z").unwrap();
    for (offset, mem, load, disk) in [(10, 1000, 0.5, 900), (70, 3000, 1.5, 700), (130, 2000, 2.5, 800)] {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                mem,
                load,
                disk,
                SampleSource::Proc,
            ))
            .unwrap();
    }
    store
        .insert_host_sample(&host_sample(
            base.plus(Duration::from_secs(310)),
            5000,
            4.0,
            600,
            SampleSource::Proc,
        ))
        .unwrap();

    let first_tick = base.plus(Duration::from_secs(7 * 60));
    assert!(capacity::record_rollup(&store, first_tick).unwrap());
    let history = capacity::read_history(&store, first_tick, Some(base), HistoryBucket::FiveMinutes).unwrap();
    assert_eq!(history.generated_at, first_tick);
    assert_eq!(history.until, first_tick);
    assert_eq!(history.since, base);
    assert_eq!(history.bucket_secs, 300);
    assert_eq!(history.retention_secs, capacity::ROLLUP_RETENTION_SECS);
    assert_eq!(history.points.len(), 1, "only the completed bucket is rolled up");
    let point = &history.points[0];
    assert_eq!(point.at, base);
    assert_eq!(point.samples, 3);
    assert_eq!(point.mem_available.min, 1000);
    assert_eq!(point.mem_available.avg, 2000);
    assert_eq!(point.mem_available.max, 3000);
    assert_eq!(point.load1_avg, 1.5);
    assert_eq!(point.disk_free_home_min, 700);
    assert_eq!(point.source, vowp_protocol::HistorySource::Proc);

    let second_tick = base.plus(Duration::from_secs(10 * 60 + 10));
    assert!(capacity::record_rollup(&store, second_tick).unwrap());
    let history = capacity::read_history(&store, second_tick, Some(base), HistoryBucket::FiveMinutes).unwrap();
    assert_eq!(history.points.len(), 2);
    assert_eq!(history.points[1].at, base.plus(Duration::from_secs(300)));
    assert_eq!(history.points[1].samples, 1);
    assert_eq!(history.points[1].mem_available.avg, 5000);
}

#[test]
fn capacity_history_is_idempotent_across_a_repeated_tick() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let base = Timestamp::parse("2026-11-10T12:00:00Z").unwrap();
    for offset in [10, 20, 30] {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                1000,
                1.0,
                900,
                SampleSource::Proc,
            ))
            .unwrap();
    }

    let tick = base.plus(Duration::from_secs(6 * 60));
    assert!(capacity::record_rollup(&store, tick).unwrap());
    assert!(capacity::record_rollup(&store, tick).unwrap());
    let rows = store.capacity_rollups_since(base, 300).unwrap();
    assert_eq!(rows.len(), 1, "a repeated tick must not duplicate the bucket");
    assert_eq!(rows[0].samples, 3);

    // The next tick recomputes the same bucket from the raw rows, including late rows.
    for offset in [240, 250] {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                3000,
                3.0,
                700,
                SampleSource::Proc,
            ))
            .unwrap();
    }
    assert!(capacity::record_rollup(&store, tick).unwrap());
    let rows = store.capacity_rollups_since(base, 300).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].samples, 5);
    assert_eq!(rows[0].mem_available_avg_bytes, 1800);

    // An empty bucket is a gap: it is skipped, never written as zeroes.
    let empty_tick = base.plus(Duration::from_secs(11 * 60));
    assert!(!capacity::record_rollup(&store, empty_tick).unwrap());
    assert_eq!(store.capacity_rollups_since(base, 300).unwrap().len(), 1);
}

#[test]
fn capacity_history_keeps_thirty_days_and_prunes_the_rest() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let base = now();
    let day = 24 * 60 * 60_u64;

    let old_at = base.minus(Duration::from_secs(40 * day)).plus(Duration::from_secs(10));
    store
        .insert_host_sample(&host_sample(old_at, 1000, 1.0, 900, SampleSource::Proc))
        .unwrap();
    assert!(capacity::record_rollup(&store, old_at.plus(Duration::from_secs(300))).unwrap());
    let recent_at = base.minus(Duration::from_secs(20 * day)).plus(Duration::from_secs(10));
    store
        .insert_host_sample(&host_sample(recent_at, 2000, 2.0, 800, SampleSource::Proc))
        .unwrap();
    assert!(capacity::record_rollup(&store, recent_at.plus(Duration::from_secs(300))).unwrap());

    store
        .insert_host_sample(&host_sample(
            base.minus(Duration::from_secs(49 * 60 * 60)),
            3000,
            3.0,
            700,
            SampleSource::Proc,
        ))
        .unwrap();
    store
        .insert_host_sample(&host_sample(
            base.minus(Duration::from_secs(47 * 60 * 60)),
            4000,
            4.0,
            600,
            SampleSource::Proc,
        ))
        .unwrap();

    // Raw rows now stop at 48 hours: the old rollup rows' raw sources go too.
    let removed = store
        .prune_host_samples(base.minus(Duration::from_secs(capacity::RAW_RETENTION_SECS)))
        .unwrap();
    assert_eq!(removed, 3);
    assert_eq!(
        store
            .host_samples_since(base.minus(Duration::from_secs(60 * day)))
            .unwrap()
            .len(),
        1,
        "only the 47-hour-old raw sample survives"
    );

    let removed = store
        .prune_capacity_rollups(base.minus(Duration::from_secs(capacity::ROLLUP_RETENTION_SECS)))
        .unwrap();
    assert_eq!(removed, 1, "the 40-day-old rollup goes");
    let rows = store
        .capacity_rollups_since(base.minus(Duration::from_secs(60 * day)), 300)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bucket, bucket_start(recent_at, 300));

    // The bucket that contains `now` is never deleted, even when it was just written.
    let current = base.plus(Duration::from_secs(10));
    store
        .insert_host_sample(&host_sample(current, 5000, 5.0, 500, SampleSource::Proc))
        .unwrap();
    assert!(capacity::record_rollup(&store, base.plus(Duration::from_secs(6 * 60))).unwrap());
    let removed = store
        .prune_capacity_rollups(base.minus(Duration::from_secs(capacity::ROLLUP_RETENTION_SECS)))
        .unwrap();
    assert_eq!(removed, 0, "the current bucket stays inside the window");
    let history = capacity::read_history(
        &store,
        base.plus(Duration::from_secs(6 * 60)),
        Some(base),
        HistoryBucket::FiveMinutes,
    )
    .unwrap();
    assert_eq!(history.points.len(), 1);
    assert_eq!(history.points[0].at, bucket_start(current, 300));
}

#[test]
fn capacity_history_clamps_since_to_the_retained_window() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let base = now();
    let day = 24 * 60 * 60_u64;
    let recent_at = base.minus(Duration::from_secs(2 * day)).plus(Duration::from_secs(10));
    store
        .insert_host_sample(&host_sample(recent_at, 2000, 2.0, 800, SampleSource::Proc))
        .unwrap();
    assert!(capacity::record_rollup(&store, recent_at.plus(Duration::from_secs(300))).unwrap());

    // A request older than retention starts at the oldest bucket the window can hold.
    let history = capacity::read_history(
        &store,
        base,
        Some(base.minus(Duration::from_secs(60 * day))),
        HistoryBucket::FiveMinutes,
    )
    .unwrap();
    let expected_since = bucket_start(base.minus(Duration::from_secs(capacity::ROLLUP_RETENTION_SECS)), 300);
    assert_eq!(history.since, expected_since);
    assert_eq!(history.until, base);
    assert_eq!(history.points.len(), 1);
    assert_eq!(history.points[0].at, bucket_start(recent_at, 300));

    // An empty home is a clear empty document, not an error and not invented zeroes.
    let empty = tempdir().unwrap();
    let empty_store = test_store(&empty.path().join("node"));
    let history = capacity::read_history(&empty_store, base, None, HistoryBucket::FiveMinutes).unwrap();
    assert!(history.points.is_empty());
    assert_eq!(history.since, base.minus(Duration::from_secs(24 * 60 * 60)));
    assert_eq!(history.until, base);
    assert_eq!(history.bucket_secs, 300);
    assert_eq!(history.retention_secs, capacity::ROLLUP_RETENTION_SECS);
}

#[test]
fn capacity_history_read_time_buckets_weight_by_samples() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let base = Timestamp::parse("2026-11-10T12:00:00Z").unwrap();

    // Bucket 12:00 has two samples averaging 2000 bytes.
    for offset in [10, 70] {
        let mem = if offset == 10 { 1000 } else { 3000 };
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(offset)),
                mem,
                1.0,
                900,
                SampleSource::Proc,
            ))
            .unwrap();
    }
    // Bucket 12:05 has eight samples averaging 4000 bytes.
    let four_thousand = 4000;
    for index in 0..8 {
        store
            .insert_host_sample(&host_sample(
                base.plus(Duration::from_secs(310 + index * 30)),
                four_thousand,
                2.0,
                800,
                SampleSource::Proc,
            ))
            .unwrap();
    }
    // Bucket 13:00 is a single fallback sample; the day point must label it mixed.
    store
        .insert_host_sample(&host_sample(
            base.plus(Duration::from_secs(3600 + 10)),
            0,
            0.0,
            500,
            SampleSource::Fallback,
        ))
        .unwrap();

    assert!(capacity::record_rollup(&store, base.plus(Duration::from_secs(5 * 60))).unwrap());
    assert!(capacity::record_rollup(&store, base.plus(Duration::from_secs(10 * 60))).unwrap());
    assert!(capacity::record_rollup(&store, base.plus(Duration::from_secs(65 * 60))).unwrap());

    let now = base.plus(Duration::from_secs(70 * 60));
    let hourly = capacity::read_history(&store, now, Some(base), HistoryBucket::Hour).unwrap();
    assert_eq!(hourly.bucket_secs, 3600);
    assert_eq!(hourly.points.len(), 2);
    let noon = &hourly.points[0];
    assert_eq!(noon.at, base);
    assert_eq!(noon.samples, 10);
    assert_eq!(noon.mem_available.min, 1000);
    assert_eq!(noon.mem_available.max, 4000);
    assert_eq!(noon.mem_available.avg, 3600, "(2000*2 + 4000*8) / 10");
    assert_eq!(noon.disk_free_home_min, 800);
    assert_eq!(noon.source, vowp_protocol::HistorySource::Proc);
    assert_eq!(hourly.points[1].at, base.plus(Duration::from_secs(3600)));
    assert_eq!(hourly.points[1].source, vowp_protocol::HistorySource::Fallback);

    let daily = capacity::read_history(&store, now, Some(base), HistoryBucket::Day).unwrap();
    assert_eq!(daily.bucket_secs, 86_400);
    assert_eq!(daily.points.len(), 1);
    let day = &daily.points[0];
    assert_eq!(day.at, bucket_start(base, 86_400));
    assert_eq!(day.samples, 11);
    assert_eq!(day.mem_available.min, 0);
    assert_eq!(day.mem_available.max, 4000);
    assert_eq!(day.mem_available.avg, 3273);
    assert_eq!(day.source, vowp_protocol::HistorySource::Mixed);
}

#[test]
fn capacity_history_is_read_only_and_writes_no_ledger_row() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let paths = NodePaths::new(&root);
    let store = test_store(&root);
    let base = now();
    store
        .insert_host_sample(&host_sample(
            base.plus(Duration::from_secs(10)),
            2000,
            2.0,
            800,
            SampleSource::Proc,
        ))
        .unwrap();
    assert!(capacity::record_rollup(&store, base.plus(Duration::from_secs(6 * 60))).unwrap());
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: base,
            kind: LedgerKind::Note,
            project: None,
            job_id: None,
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: "history read must not add a row".to_owned(),
            detail: serde_json::json!({}),
        })
        .unwrap();

    let read_only = Store::open_read_only(&paths, None).unwrap();
    let history = capacity::read_history(
        &read_only,
        base.plus(Duration::from_secs(6 * 60)),
        Some(base),
        HistoryBucket::FiveMinutes,
    )
    .unwrap();
    assert_eq!(history.points.len(), 1);
    assert_eq!(history.points[0].samples, 1);

    assert_eq!(
        read_only
            .ledger_since(&vowp_store::LedgerFilter::default())
            .unwrap()
            .len(),
        1,
        "the history read wrote no ledger row"
    );
    assert_eq!(read_only.host_samples_since(base).unwrap().len(), 1);
    assert_eq!(read_only.capacity_rollups_since(base, 300).unwrap().len(), 1);
}

#[test]
fn scope_reads_cgroup_memory_peak() {
    let tmp = tempdir().unwrap();
    let deps = test_deps(&tmp.path().join("node"));
    let cgroup = tmp.path().join("scope");
    let mut fake = FakeProcFs::default();
    fake.files
        .insert(format!("{}/memory.peak", cgroup.display()), "1048576\n".to_owned());
    let run = RunId::new();
    let scope = RunScope::begin_cgroup_with(&deps, Arc::new(fake), &run, cgroup);
    let sample = scope.sample().unwrap();
    assert_eq!(sample.peak_rss_bytes, 1_048_576);
}

#[test]
fn scope_records_anon_file_kernel_and_cpu_usec() {
    let tmp = tempdir().unwrap();
    let deps = test_deps(&tmp.path().join("node"));
    let cgroup = tmp.path().join("scope");
    let mut fake = FakeProcFs::default();
    fake.files
        .insert(format!("{}/memory.peak", cgroup.display()), "2000000\n".to_owned());
    fake.files.insert(
        format!("{}/memory.stat", cgroup.display()),
        "anon 1200000\nfile 500000\nkernel 300000\n".to_owned(),
    );
    fake.files.insert(
        format!("{}/cpu.stat", cgroup.display()),
        "usage_usec 2500000\nuser_usec 2000000\nsystem_usec 500000\n".to_owned(),
    );
    fake.files
        .insert(format!("{}/memory.events", cgroup.display()), "oom_kill 0\n".to_owned());
    let run = RunId::new();
    let scope = RunScope::begin_cgroup_with(&deps, Arc::new(fake), &run, cgroup);
    let sample = scope.sample().unwrap();
    assert_eq!(sample.peak_rss_bytes, 2_000_000);
    assert_eq!(sample.anon_peak_bytes, Some(1_200_000));
    assert_eq!(sample.file_peak_bytes, Some(500_000));
    assert_eq!(sample.kernel_peak_bytes, Some(300_000));
    assert_eq!(sample.cpu_usec, 2_500_000);
    assert!(!sample.oom_killed);
}

#[test]
fn scope_falls_back_to_process_tree() {
    let tmp = tempdir().unwrap();
    let deps = test_deps(&tmp.path().join("node"));
    let mut fake = FakeProcFs::default();
    fake.files.insert(
        "/proc/100/status".to_owned(),
        "Name:\tsh\nVmRSS:\t100 kB\nRssAnon:\t60 kB\nRssFile:\t40 kB\n".to_owned(),
    );
    fake.files.insert(
        "/proc/101/status".to_owned(),
        "Name:\tchild\nVmRSS:\t200 kB\nRssAnon:\t120 kB\nRssFile:\t80 kB\n".to_owned(),
    );
    fake.files.insert(
        "/proc/100/stat".to_owned(),
        "100 (sh) S 0 100 100 0 -1 4194304 0 0 0 0 50 10 0 0 20 0 1 0 100 0 0\n".to_owned(),
    );
    fake.files.insert(
        "/proc/101/stat".to_owned(),
        "101 (child) S 100 101 100 0 -1 4194304 0 0 0 0 25 5 0 0 20 0 1 0 100 0 0\n".to_owned(),
    );
    fake.children.insert(100, vec![101]);
    let run = RunId::new();
    let scope = RunScope::begin_with(&deps, Arc::new(fake), &run, 100);
    let sample = scope.sample().unwrap();
    assert_eq!(sample.peak_rss_bytes, 300 * 1024);
    assert_eq!(sample.anon_peak_bytes, Some(180 * 1024));
    assert_eq!(sample.file_peak_bytes, Some(120 * 1024));
    assert_eq!(sample.cpu_usec, 900_000);
    assert_eq!(scope.source(), UsageSource::ProcTree);
}

#[test]
fn build_mode_cold_warm_split() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    assert_eq!(scope::infer_build_mode(root), BuildMode::Unknown);
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    assert_eq!(scope::infer_build_mode(root), BuildMode::Cold);
    std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
    std::fs::write(root.join("target/debug/deps/libx.rlib"), "x").unwrap();
    assert_eq!(scope::infer_build_mode(root), BuildMode::Warm);
    std::fs::create_dir_all(root.join("target/debug/incremental/x")).unwrap();
    std::fs::write(root.join("target/debug/incremental/x/state"), "x").unwrap();
    assert_eq!(scope::infer_build_mode(root), BuildMode::Incremental);
}

#[test]
fn scope_without_proc_writes_the_default_source() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "default-scope");
    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &NodeName::parse("local").unwrap());
    let deps = test_deps(&root);
    let scope = RunScope::begin_with(&deps, Arc::new(FakeProcFs::default()), &run_id, 999_999);
    let usage = scope.finish(&store, BuildMode::Unknown).unwrap();
    assert_eq!(usage.source, UsageSource::Default);
    assert_eq!(usage.peak_rss_bytes, scope::DEFAULT_PEAK_RSS_BYTES);
    assert_eq!(usage.mean_cpu_cores, scope::DEFAULT_MEAN_CPU_CORES);
    assert_eq!(usage.cpu_usec, 0);
}

#[test]
fn oom_kill_marks_run_usage() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "oom-job");
    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &NodeName::parse("local").unwrap());
    let deps = test_deps(&root);
    let cgroup = tmp.path().join("scope");
    let mut fake = FakeProcFs::default();
    fake.files
        .insert(format!("{}/memory.peak", cgroup.display()), "3000000\n".to_owned());
    fake.files.insert(
        format!("{}/memory.stat", cgroup.display()),
        "anon 2000000\nfile 500000\nkernel 500000\n".to_owned(),
    );
    fake.files.insert(
        format!("{}/cpu.stat", cgroup.display()),
        "usage_usec 1000000\n".to_owned(),
    );
    fake.files
        .insert(format!("{}/memory.events", cgroup.display()), "oom_kill 1\n".to_owned());
    let scope = RunScope::begin_cgroup_with(&deps, Arc::new(fake), &run_id, cgroup);
    let usage = scope.finish(&store, BuildMode::Warm).unwrap();
    assert!(usage.oom_killed);
    assert_eq!(usage.source, UsageSource::Cgroup);
    let stored = store.run_usage(&run_id).unwrap().expect("run_usage row");
    assert!(stored.oom_killed);
    assert_eq!(stored.build_mode, BuildMode::Warm);
    assert_eq!(stored.project, project);
}

/// Owns a spawned child and its sentinel file; kills, reaps and removes both on drop.
///
/// Dropping runs even when the test panics, so no shell loop outlives the test body.
#[cfg(target_os = "linux")]
struct ProbeGuard {
    child: std::process::Child,
    sentinel: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
impl ProbeGuard {
    fn spawn(script: &str, sentinel: Option<PathBuf>) -> Self {
        let child = std::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self { child, sentinel }
    }

    /// Spawns `args[0]` with `args[1..]` (stdout discarded) under the same drop guard.
    fn spawn_args(args: &[String], sentinel: Option<PathBuf>) -> Self {
        let child = std::process::Command::new(&args[0])
            .args(&args[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self { child, sentinel }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

#[cfg(target_os = "linux")]
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(path) = self.sentinel.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Returns the pids of live processes whose command line mentions `needle`.
#[cfg(target_os = "linux")]
fn processes_mentioning(needle: &str) -> Vec<u32> {
    let me = std::process::id();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&raw).replace('\0', " ").contains(needle) {
            found.push(pid);
        }
    }
    found
}

#[cfg(target_os = "linux")]
#[test]
fn scope_leaves_no_probe_child_after_drop() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "no-leak");
    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &NodeName::parse("local").unwrap());
    let deps = test_deps(&root);
    let needle = root.display().to_string();
    {
        let child = ProbeGuard::spawn("while :; do :; done", None);
        let scope = RunScope::begin(&deps, &run_id, child.id()).unwrap();
        let _ = scope.sample().unwrap();
        // Dropped without `finish`: the holder shell and its hold file must still go away.
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let leaked = processes_mentioning(&needle);
        if leaked.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scope holder processes survived the drop: {leaked:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let leftovers: Vec<_> = std::fs::read_dir(deps.paths.tmp_dir())
        .map(|dir| dir.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(
        leftovers
            .iter()
            .all(|path| path.extension().is_none_or(|ext| ext != "hold")),
        "hold files survived the drop: {leftovers:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn holder_loop_exits_when_its_parent_is_gone() {
    let tmp = tempdir().unwrap();
    let hold = tmp.path().join("probe.hold");
    std::fs::write(&hold, b"hold").unwrap();
    // A reaped child's pid is dead, so `kill -0` fails while the hold file still exists.
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    let args = scope::holder_args(&hold.display().to_string(), dead_pid);
    let mut holder = ProbeGuard::spawn_args(&args, Some(hold.clone()));
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = loop {
        if holder.child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(exited, "the holder outlived its dead parent");
    assert!(
        hold.exists(),
        "the hold file must still exist: the exit was the parent check"
    );

    // With a live parent (this process) and the hold file present it keeps running.
    let args = scope::holder_args(&hold.display().to_string(), std::process::id());
    let mut live = ProbeGuard::spawn_args(&args, None);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        live.child.try_wait().unwrap().is_none(),
        "the holder exited although its parent is alive"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scope_begin_measures_a_real_child() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "real-scope");
    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &NodeName::parse("local").unwrap());
    let deps = test_deps(&root);
    // A fixed short child races the scope setup: under load the first sample can run
    // after `sleep 0.2` has exited and read zeros. Keep a real child alive and busy until
    // the scope has observed it, then poll for the measurement instead of betting on one
    // sample.
    let mut child = ProbeGuard::spawn("while :; do :; done", None);
    let scope = RunScope::begin(&deps, &run_id, child.id()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut sample = scope.sample().unwrap();
    while sample.peak_rss_bytes == 0 || sample.cpu_usec == 0 {
        assert!(
            Instant::now() < deadline,
            "the real child was not measured within 10s: {sample:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
        sample = scope.sample().unwrap();
    }
    assert!(sample.peak_rss_bytes > 0, "no memory measured: {sample:?}");
    assert!(sample.cpu_usec > 0, "no CPU measured: {sample:?}");
    let _ = child.child.kill();
    let _ = child.child.wait();
    let usage = scope.finish(&store, BuildMode::Unknown).unwrap();
    assert!(usage.peak_rss_bytes > 0);
    assert!(usage.cpu_usec > 0, "the usage row lost the child's CPU: {usage:?}");
    assert!(
        matches!(usage.source, UsageSource::Cgroup | UsageSource::ProcTree),
        "the usage row did not come from a real scope: {usage:?}"
    );
    assert_eq!(usage.run_id, run_id);
}

#[test]
fn adapter_default_sidecar_is_imported() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "default-adapter");
    let deps = test_deps(&root);
    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &deps.node);
    std::fs::create_dir_all(deps.paths.runs_dir()).unwrap();
    let sidecar = deps.paths.run_raw_path(&run_id).with_extension("usage.json");
    let record = default_sidecar(&run_id);
    std::fs::write(&sidecar, serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(capacity::usage::import_sidecars(&deps, &store).unwrap(), 1);
    let usage = store.run_usage(&run_id).unwrap().expect("default run_usage");
    assert_eq!(usage.source, UsageSource::Default);
    assert_eq!(usage.peak_rss_bytes, scope::DEFAULT_PEAK_RSS_BYTES);
    assert_eq!(usage.mean_cpu_cores, scope::DEFAULT_MEAN_CPU_CORES);
}

#[test]
fn sidecar_build_mode_uses_job_worktree_not_run_cwd() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let deps = test_deps(&root);
    std::fs::create_dir_all(deps.paths.runs_dir()).unwrap();

    // The job worktree is warm; the session cwd only looks cold. A build-mode inference that
    // falls back to run.cwd would mislabel this run as cold.
    let worktree = tmp.path().join("job-worktree");
    std::fs::create_dir_all(worktree.join("target/debug/deps")).unwrap();
    std::fs::write(worktree.join("Cargo.toml"), "[package]\nname = \"job\"\n").unwrap();
    std::fs::write(worktree.join("target/debug/deps/libjob.rlib"), "x").unwrap();
    let session = tmp.path().join("session-cwd");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("Cargo.toml"), "[package]\nname = \"session\"\n").unwrap();

    let job = insert_job(&store, &project, "build-mode-target");
    store
        .set_job_workspace(&job.id, Some("job/build-mode-target"), worktree.to_str())
        .unwrap();
    let run_id = RunId::new();
    insert_run_with_cwd(&store, &job, &run_id, &deps.node, Some(session.display().to_string()));
    let sidecar = deps.paths.run_raw_path(&run_id).with_extension("usage.json");
    std::fs::write(&sidecar, serde_json::to_vec(&default_sidecar(&run_id)).unwrap()).unwrap();
    assert_eq!(capacity::usage::import_sidecars(&deps, &store).unwrap(), 1);
    let usage = store.run_usage(&run_id).unwrap().expect("job-target usage row");
    assert_eq!(usage.build_mode, BuildMode::Warm);

    // Without a job worktree there is no trustworthy target directory to read.
    let job = insert_job(&store, &project, "build-mode-no-worktree");
    let run_id = RunId::new();
    insert_run_with_cwd(&store, &job, &run_id, &deps.node, Some(worktree.display().to_string()));
    let sidecar = deps.paths.run_raw_path(&run_id).with_extension("usage.json");
    std::fs::write(&sidecar, serde_json::to_vec(&default_sidecar(&run_id)).unwrap()).unwrap();
    assert_eq!(capacity::usage::import_sidecars(&deps, &store).unwrap(), 1);
    let usage = store.run_usage(&run_id).unwrap().expect("no-worktree usage row");
    assert_eq!(usage.build_mode, BuildMode::Unknown);
}

#[test]
fn run_usage_written_for_adapter_and_gate() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let store = test_store(&root);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "usage-job");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let deps = test_deps(&root);

    let run_id = RunId::new();
    insert_run(&store, &job, &run_id, &deps.node);
    let raw = deps.paths.run_raw_path(&run_id);
    std::fs::create_dir_all(deps.paths.runs_dir()).unwrap();
    let spec = SpawnSpec {
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), "sleep 0.15".to_owned()],
        cwd: worktree.clone(),
        env: child_env(&[]),
        stdin: None,
        tee_stdout: Some(raw.clone()),
        tee_stderr: None,
        timeout: None,
    };
    let mut handle = RunHandle::spawn(spec, Box::new(StubParser)).unwrap();
    handle.wait().unwrap();
    let sidecar = raw.with_extension("usage.json");
    assert!(sidecar.is_file(), "adapter sidecar missing at {}", sidecar.display());
    assert!(capacity::usage::import_sidecars(&deps, &store).unwrap() >= 1);
    let usage = store.run_usage(&run_id).unwrap().expect("adapter run_usage");
    assert!(usage.peak_rss_bytes > 0);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(usage.source, UsageSource::Default);
    if vowp_adapters::proc::cgroup_scopes_available() {
        assert_eq!(
            usage.source,
            UsageSource::Cgroup,
            "adapter did not use the preferred cgroup scope: {usage:?}"
        );
    } else {
        assert!(
            matches!(usage.source, UsageSource::ProcTree | UsageSource::Default),
            "adapter reported an unexpected source without a cgroup scope: {usage:?}"
        );
    }
    assert!(!sidecar.exists());

    let runner = GateRunner::new(deps.clone());
    let context = GateCtx {
        deps: &deps,
        job: &job,
        run_id: None,
        worktree: &worktree,
        repo: &worktree,
        store: &store,
        attempt: 0,
    };
    let result = runner
        .evaluate(
            &context,
            0,
            &Gate::Cmd(CmdGate {
                run: "sleep 0.02".to_owned(),
                ..CmdGate::default()
            }),
        )
        .unwrap();
    assert!(result.passed);
    let rows = store
        .run_usage_for_group(
            &vowp_store::repo_capacity::UsageQuery {
                project: Some(project),
                job_kind: Some(JobKind::Build),
                ..vowp_store::repo_capacity::UsageQuery::default()
            },
            100,
        )
        .unwrap();
    assert!(
        rows.iter().any(|row| row.role == RunRole::Gate),
        "gate usage row missing: {rows:?}"
    );
}

#[test]
fn estimates_use_exact_group_then_pool_then_default() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let project = insert_project(&store);
    for index in 0..5 {
        store
            .upsert_run_usage(&usage_row(&project, JobKind::Build, "exact", 100 + index * 100, index))
            .unwrap();
    }
    capacity::recompute(&store, now()).unwrap();
    let exact = capacity::estimate_for(&store, &admit_request(&project, "exact")).unwrap();
    assert_eq!(exact.source, vowp_protocol::EstimateSource::Measured);
    assert_eq!(exact.samples, 5);
    assert_eq!(
        exact.ram_p90_bytes, 250,
        "p90 uses the anon working set (peak / 2 in the fixture)"
    );

    let other = ProjectName::parse("other").unwrap();
    for index in 0..5 {
        store
            .upsert_run_usage(&usage_row(
                &project,
                JobKind::Build,
                "other-model",
                900 + index,
                index + 10,
            ))
            .unwrap();
        store
            .upsert_run_usage(&usage_row(
                &other,
                JobKind::Build,
                "other-model",
                1_000 + index,
                index + 20,
            ))
            .unwrap();
    }
    capacity::recompute(&store, now()).unwrap();
    let pooled = capacity::estimate_for(&store, &admit_request(&project, "missing-model")).unwrap();
    assert_eq!(pooled.source, vowp_protocol::EstimateSource::Pooled);
    assert!(pooled.samples >= 5);

    let empty = ProjectName::parse("empty").unwrap();
    let mut request = admit_request(&empty, "none");
    request.kind = JobKind::Docs;
    let default = capacity::estimate_for(&store, &request).unwrap();
    assert_eq!(default.source, vowp_protocol::EstimateSource::Default);
    assert_eq!(default.ram_p90_bytes, scope::DEFAULT_PEAK_RSS_BYTES);
    assert_eq!(default.cpu_p90_cores, scope::DEFAULT_MEAN_CPU_CORES);
}

#[test]
fn estimates_pool_across_harnesses() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let project = insert_project(&store);
    for index in 0..5 {
        store
            .upsert_run_usage(&usage_row(
                &project,
                JobKind::Build,
                "opencode-model",
                200 + index * 100,
                index,
            ))
            .unwrap();
    }
    capacity::recompute(&store, now()).unwrap();
    let mut request = admit_request(&project, "claude-model");
    request.harness = Some(HarnessKind::ClaudeCode);
    let pooled = capacity::estimate_for(&store, &request).unwrap();
    assert_eq!(pooled.source, vowp_protocol::EstimateSource::Pooled);
    assert_eq!(pooled.samples, 5);
    assert_eq!(pooled.ram_p90_bytes, 300);
}

#[test]
fn estimates_need_five_samples() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let project = insert_project(&store);
    let mut rows = Vec::new();
    for index in 0..4 {
        let row = usage_row(&project, JobKind::Build, "five", 100 + index, index);
        store.upsert_run_usage(&row).unwrap();
        rows.push(row);
    }
    capacity::recompute(&store, now()).unwrap();
    let four = capacity::estimate_for(&store, &admit_request(&project, "five")).unwrap();
    assert_eq!(four.source, vowp_protocol::EstimateSource::Default);

    store
        .upsert_run_usage(&usage_row(&project, JobKind::Build, "five", 500, 5))
        .unwrap();
    capacity::recompute(&store, now()).unwrap();
    let five = capacity::estimate_for(&store, &admit_request(&project, "five")).unwrap();
    assert_eq!(five.source, vowp_protocol::EstimateSource::Measured);
    assert_eq!(five.samples, 5);
}

#[test]
fn capacity_targets_and_advice_round_trip() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let target = CapacityTarget {
        mode: TargetMode::Fixed,
        target_workers: 4,
        advised_workers: Some(3),
        is_override: true,
        set_by: "owner".to_owned(),
        set_at: now(),
        reason: "burst".to_owned(),
    };
    store.insert_capacity_target(&target).unwrap();
    assert_eq!(store.latest_capacity_target().unwrap(), Some(target));

    let advice = CapacityAdvice {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        created_at: now(),
        current_target: 3,
        proposed_target: 4,
        mode: TargetMode::Auto,
        reasons: vec!["RAM headroom".to_owned()],
        expected_effect: "one more worker".to_owned(),
        inbox_id: None,
        state: AdviceState::Open,
        applied_at: None,
        applied_by: None,
    };
    store.insert_capacity_advice(&advice).unwrap();
    assert_eq!(store.capacity_advice(&advice.id).unwrap(), Some(advice.clone()));
    assert_eq!(store.latest_open_advice().unwrap(), Some(advice.clone()));
    store
        .finish_capacity_advice(&advice.id, AdviceState::Applied, "owner", now())
        .unwrap();
    let finished = store.capacity_advice(&advice.id).unwrap().unwrap();
    assert_eq!(finished.state, AdviceState::Applied);
    assert_eq!(finished.applied_by.as_deref(), Some("owner"));
    assert!(store.latest_open_advice().unwrap().is_none());
}

/// A parser that turns every stdout line into a note.
struct StubParser;

impl StreamParser for StubParser {
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        vec![HarnessEvent {
            ts: None,
            actor: Actor::System,
            body: EventBody::Note(Note { text: line.to_owned() }),
            raw: None,
        }]
    }

    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, _stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary) {
        let (outcome, stop_reason) = if killed {
            (RunOutcome::Killed, StopReason::Interrupted)
        } else {
            match exit {
                Some(0) => (RunOutcome::Ok, StopReason::EndTurn),
                Some(_) => (RunOutcome::Error, StopReason::Error),
                None => (RunOutcome::Crash, StopReason::Crash),
            }
        };
        (
            vec![HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason,
                    duration_ms: None,
                }),
                raw: None,
            }],
            RunSummary {
                exit_code: exit,
                outcome,
                native_session_id: None,
                cost: Cost::unknown(),
                limit: None,
                result_text: None,
                stop_reason,
            },
        )
    }
}

#[allow(dead_code)]
fn raw_path(root: &Path, run: &RunId) -> PathBuf {
    NodePaths::new(root).run_raw_path(run)
}

#[test]
fn scope_working_set_tracks_anon_peak_not_cache() {
    let tmp = tempdir().unwrap();
    let deps = test_deps(&tmp.path().join("node"));
    let cgroup = tmp.path().join("scope");
    let mut fake = FakeProcFs::default();
    // 9.6 GiB memory.peak, almost all reclaimable file cache from a cargo build.
    fake.files
        .insert(format!("{}/memory.peak", cgroup.display()), "10307921510\n".to_owned());
    fake.files.insert(
        format!("{}/memory.stat", cgroup.display()),
        "anon 1500000000\nfile 8500000000\nkernel 100000000\n".to_owned(),
    );
    let run = RunId::new();
    let scope = RunScope::begin_cgroup_with(&deps, Arc::new(fake), &run, cgroup);
    let sample = scope.sample().unwrap();
    assert_eq!(sample.anon_peak_bytes, Some(1_500_000_000));
    assert_eq!(sample.file_peak_bytes, Some(8_500_000_000));
}

#[test]
fn estimates_exclude_reclaimable_page_cache() {
    let tmp = tempdir().unwrap();
    let store = test_store(&tmp.path().join("node"));
    let project = insert_project(&store);
    for index in 0..5 {
        let mut row = usage_row(&project, JobKind::Build, "cache", 9_600 * 1024 * 1024, index);
        row.anon_peak_bytes = Some(1_400 * 1024 * 1024);
        row.kernel_peak_bytes = Some(100 * 1024 * 1024);
        row.file_peak_bytes = Some(8_000 * 1024 * 1024);
        store.upsert_run_usage(&row).unwrap();
    }
    // A legacy row with only a file share still drops the cache.
    let mut legacy = usage_row(&project, JobKind::Build, "cache", 9_600 * 1024 * 1024, 9);
    legacy.anon_peak_bytes = None;
    legacy.kernel_peak_bytes = None;
    legacy.file_peak_bytes = Some(8_100 * 1024 * 1024);
    assert_eq!(capacity::estimates::working_set_bytes(&legacy), 1_500 * 1024 * 1024);
    capacity::recompute(&store, now()).unwrap();
    let estimate = capacity::estimate_for(&store, &admit_request(&project, "cache")).unwrap();
    assert_eq!(estimate.ram_p90_bytes, 1_500 * 1024 * 1024);
    assert_eq!(estimate.ram_p50_bytes, 1_500 * 1024 * 1024);
}
