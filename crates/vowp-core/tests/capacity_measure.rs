//! Capacity measurement tests: sampler, scopes, usage rows and estimates.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §6.1 and the capacity test list in §15.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use vowp_adapters::proc::{ProcUsage, RunHandle, SpawnSpec, child_env};
use vowp_adapters::stream::StreamParser;
use vowp_core::capacity::sampler::sample_with;
use vowp_core::capacity::{self, FakeProcFs, RunScope, scope};
use vowp_core::gates::{GateCtx, GateRunner};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    Actor, AdapterRun, AdmitRequest, AdviceState, BuildMode, CapacityAdvice, CapacityTarget, CmdGate, Cost, EventBody,
    FakeClock, Gate, HarnessEvent, HarnessKind, Job, JobId, JobKind, MemoryLog, NewJob, NodeName, Note, Project,
    ProjectName, Role, Run, RunId, RunOutcome, RunPurpose, RunRole, RunSummary, RunUsage, StopReason, TargetMode,
    Timestamp, TurnEnded, UsageSource, VowpConfig,
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
    let mut fake = FakeProcFs::default();
    fake.disk.insert(root.display().to_string(), 7_000_000_000);
    let sample = sample_with(&fake, &NodePaths::new(&root), None, now(), 5).unwrap();
    assert_eq!(sample.source, vowp_protocol::SampleSource::Fallback);
    assert_eq!(sample.mem_total_bytes, 0);
    assert_eq!(sample.mem_available_bytes, 0);
    assert_eq!(sample.disk_free_home_bytes, 7_000_000_000);
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
    sample.at = now().minus(Duration::from_secs(31 * 24 * 60 * 60));
    store.insert_host_sample(&sample).unwrap();
    sample.at = now().minus(Duration::from_secs(2 * 24 * 60 * 60));
    sample.interval_secs = 60;
    store.insert_host_sample(&sample).unwrap();
    sample.interval_secs = 5;
    store.insert_host_sample(&sample).unwrap();
    sample.at = now();
    sample.interval_secs = 5;
    store.insert_host_sample(&sample).unwrap();

    let removed = store
        .prune_host_samples(
            now().minus(Duration::from_secs(24 * 60 * 60)),
            now().minus(Duration::from_secs(30 * 24 * 60 * 60)),
        )
        .unwrap();
    assert_eq!(removed, 2);
    let left = store
        .host_samples_since(now().minus(Duration::from_secs(40 * 24 * 60 * 60)))
        .unwrap();
    assert_eq!(left.len(), 2);
    assert!(left.iter().any(|sample| sample.interval_secs == 60));
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
    let mut child = std::process::Command::new("sh")
        .args(["-c", "sleep 0.2"])
        .spawn()
        .unwrap();
    let scope = RunScope::begin(&deps, &run_id, child.id()).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let sample = scope.sample().unwrap();
    assert!(sample.peak_rss_bytes > 0, "no memory measured: {sample:?}");
    let _ = child.wait();
    let usage = scope.finish(&store, BuildMode::Unknown).unwrap();
    assert!(usage.peak_rss_bytes > 0);
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
    assert_eq!(exact.ram_p90_bytes, 500);

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
    assert_eq!(pooled.ram_p90_bytes, 600);
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
