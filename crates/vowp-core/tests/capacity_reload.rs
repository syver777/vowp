//! Live capacity reload: provider caps and target changes without a restart.
//!
//! Spec: `docs/design/v4/local-orchestrator.md` §16.4 and the job brief
//! `v4-c1-live-limits`. Every test uses a temporary node home, a temporary project checkout,
//! a fake clock, a fake window manager and no harness process or network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use vowp_core::capacity::{self, CapacityManager, reload};
use vowp_core::loops::NodeSupervisor;
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_graph::{DispatchCaps, Graph, plan_dispatch};
use vowp_protocol::{
    AdmitDecision, BuildMode, Cost, EstimateSource, FakeClock, HarnessKind, HostSample, InboxState, Job, JobId,
    JobKind, JobState, LedgerKind, MemoryLog, NewJob, Node, NodeName, Project, ProjectName, ProviderConfig, Role, Run,
    RunId, RunPurpose, RunRole, SampleSource, TargetMode, Timestamp, UsageEstimate, VowpConfig, WaitCode,
};
use vowp_store::{JobFilter, LedgerFilter, NodePaths, Store};

const NOW_TEXT: &str = "2026-11-13T12:00:00Z";
const GIB: u64 = 1024 * 1024 * 1024;

fn now() -> Timestamp {
    Timestamp::parse(NOW_TEXT).expect("fixed timestamp")
}

/// One temporary node home and project checkout with a real store and fake dependencies.
struct Env {
    _tmp: TempDir,
    project: ProjectName,
    project_dir: PathBuf,
    deps: Deps,
    store: Store,
}

impl Env {
    /// Builds the fixture; `cap` sets `[providers.deepseek].max_concurrent` when present.
    fn new(cap: Option<u32>) -> Self {
        let tmp = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(tmp.path().join("node"));
        paths.ensure().expect("node paths");
        let project = ProjectName::parse("test").expect("project slug");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(project_dir.join(".vowp/jobs")).expect("project dirs");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.project.worktree_root = Some(tmp.path().join("worktrees").display().to_string());
        if let Some(cap) = cap {
            config.providers.insert(
                "deepseek".to_owned(),
                ProviderConfig {
                    max_concurrent: cap,
                    ..ProviderConfig::default()
                },
            );
        }
        write_config(&project_dir, &config);

        let clock = FakeClock::at(now());
        let global = Store::open_global(&paths)
            .expect("global store")
            .with_clock(Arc::new(clock.clone()));
        global
            .upsert_node(&Node {
                name: NodeName::parse("local").expect("node"),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: Some(now()),
                created_at: now(),
            })
            .expect("node row");
        global
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now(),
                updated_at: now(),
            })
            .expect("project row");
        drop(global);

        let store = Store::open_with_project(&paths, &project)
            .expect("project store")
            .with_clock(Arc::new(clock.clone()));
        let cmd = Arc::new(SystemRunner);
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
            git: Arc::new(SystemGit::new(cmd.clone())),
            cmd,
            rng: Arc::new(FixedRng(0)),
        };
        Self {
            _tmp: tmp,
            project,
            project_dir,
            deps,
            store,
        }
    }

    /// Reads the config currently on disk.
    fn disk_config(&self) -> VowpConfig {
        let text = std::fs::read_to_string(self.project_dir.join("vowp.toml")).expect("read config");
        VowpConfig::from_toml(&text).expect("parse config")
    }

    /// Writes a config document to the checkout.
    fn set_config(&self, config: &VowpConfig) {
        write_config(&self.project_dir, config);
    }

    /// Sets the provider concurrency cap in the on-disk config.
    fn set_cap(&self, cap: u32) {
        let mut config = self.disk_config();
        config
            .providers
            .entry("deepseek".to_owned())
            .or_default()
            .max_concurrent = cap;
        self.set_config(&config);
    }

    /// Overwrites an exact fragment of the on-disk TOML text.
    fn replace_config_text(&self, from: &str, to: &str) {
        let path = self.project_dir.join("vowp.toml");
        let text = std::fs::read_to_string(&path).expect("read config text");
        assert!(text.contains(from), "fixture text missing {from:?}:\n{text}");
        std::fs::write(path, text.replacen(from, to, 1)).expect("write config text");
    }

    /// Inserts one build job and returns it.
    fn job(&self, id: &str) -> Job {
        let new = NewJob {
            id: JobId::parse(id).expect("job id"),
            title: format!("job {id}"),
            kind: JobKind::Build,
            role: Role::Worker,
            profile: Some("worker".to_owned()),
            needs: Vec::new(),
            owns: vec!["src/**".to_owned()],
            reads: Vec::new(),
            gates: None,
            priority: None,
            loop_: None,
            brief_text: Some("Do the work.\n".to_owned()),
        };
        self.store.insert_job(&self.project, &new, "test").expect("insert job")
    }

    /// Marks a job ready.
    fn ready(&self, job: &Job) {
        self.store
            .set_job_state(
                &job.id,
                JobState::Ready,
                &vowp_store::StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("ready");
    }

    /// Inserts one open worker run for `provider`.
    fn open_run(&self, provider: &str) -> Run {
        let run = Run {
            id: RunId::new(),
            job_id: None,
            session_id: None,
            role: RunRole::Worker,
            purpose: RunPurpose::Start,
            profile: Some("worker".to_owned()),
            rung: 0,
            executor: Some(vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
            provider: Some(provider.to_owned()),
            node: NodeName::parse("local").expect("node"),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at: now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        };
        self.store.insert_run(&run).expect("run row");
        run
    }
}

/// Writes one config as pretty TOML.
fn write_config(dir: &Path, config: &VowpConfig) {
    std::fs::write(
        dir.join("vowp.toml"),
        toml::to_string_pretty(config).expect("serialise config"),
    )
    .expect("write config");
}

/// Returns the wait code of a wait decision.
fn wait_code(decision: AdmitDecision) -> WaitCode {
    match decision {
        AdmitDecision::Wait(reason) => reason.code,
        AdmitDecision::Admit => panic!("expected a wait decision, got admit"),
    }
}

/// Builds the planner caps used by the dispatch step.
fn dispatch_caps(deps: &Deps, free: &std::collections::BTreeMap<String, u32>) -> DispatchCaps {
    DispatchCaps {
        free_workers: 4,
        free_verifiers: 2,
        provider_free: free
            .iter()
            .map(|(provider, count)| (provider.clone(), *count))
            .collect(),
        limited: HashMap::new(),
        profiles: deps.config_now().profiles.clone().into_iter().collect(),
        now: now(),
        overrides: HashMap::new(),
    }
}

/// Returns the ready jobs planned by dispatch for the fixture.
fn planned(env: &Env) -> Vec<JobId> {
    let jobs = env
        .store
        .list_jobs(&JobFilter {
            project: Some(env.project.clone()),
            ..JobFilter::default()
        })
        .expect("jobs");
    let edges = env.store.list_edges(Some(&env.project)).expect("edges");
    let graph = Graph::new(jobs, edges).expect("graph");
    let free = reload::provider_free(&env.deps.config_now(), &env.store.open_runs().expect("runs"));
    let caps = dispatch_caps(&env.deps, &free);
    let running: Vec<Job> = Vec::new();
    plan_dispatch(&graph, &running, &caps)
        .into_iter()
        .map(|choice| choice.job_id)
        .collect()
}

/// Healthy host sample with ample RAM, CPU and disk.
fn healthy_sample() -> HostSample {
    HostSample {
        at: now(),
        interval_secs: 5,
        mem_total_bytes: 8 * GIB,
        mem_available_bytes: 6 * GIB,
        swap_used_bytes: 0,
        load1: 0.1,
        load5: 0.1,
        load15: 0.1,
        cpu_count: 4,
        cpu_quota_cores: Some(4.0),
        disk_free_home_bytes: 50 * GIB,
        disk_free_worktrees_bytes: None,
        pressure_cpu_avg10: None,
        pressure_mem_avg10: Some(0.0),
        pressure_io_avg10: None,
        source: SampleSource::Proc,
    }
}

/// Seeds a small measured estimate so the auto target stays below the host ceiling.
fn seed_estimate(store: &Store, project: &ProjectName) {
    store
        .upsert_usage_estimate(&UsageEstimate {
            project: Some(project.clone()),
            job_kind: Some(JobKind::Build),
            harness: Some(HarnessKind::Opencode),
            model: Some("deepseek/deepseek-flash".to_owned()),
            build_mode: Some(BuildMode::Unknown),
            samples: 5,
            ram_p50_bytes: GIB,
            ram_p90_bytes: GIB,
            cpu_p50_cores: 1.0,
            cpu_p90_cores: 1.0,
            duration_p50_ms: 1_000,
            duration_p90_ms: 2_000,
            source: EstimateSource::Measured,
        })
        .expect("estimate");
}

/// Counts applied-reload ledger notes.
fn reload_notes(store: &Store) -> usize {
    store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            ..LedgerFilter::default()
        })
        .expect("ledger")
        .into_iter()
        .filter(|entry| entry.summary.starts_with("capacity config reloaded:"))
        .count()
}

#[test]
fn provider_cap_raise_admits_without_restart() {
    let env = Env::new(Some(1));
    env.job("raise-a");
    env.job("raise-b");
    for id in ["raise-a", "raise-b"] {
        let job = env.store.require_job(&JobId::parse(id).expect("id")).expect("job");
        env.ready(&job);
    }
    env.open_run("deepseek");

    let first = reload::reload_project(&env.deps, &env.store, now(), true).expect("baseline");
    assert!(!first.pending, "{first:?}");
    assert_eq!(first.provider_caps.get("deepseek"), Some(&1));
    assert!(planned(&env).is_empty(), "one active run already fills a cap of one");

    env.set_cap(2);
    let raised = reload::reload_project(&env.deps, &env.store, now(), true).expect("raise");
    assert!(raised.applied, "{raised:?}");
    assert_eq!(raised.provider_caps.get("deepseek"), Some(&2));
    assert_eq!(raised.changed, vec!["providers.deepseek.max_concurrent"]);
    assert_eq!(
        env.store.open_runs().expect("runs").len(),
        1,
        "the active run is untouched"
    );
    assert_eq!(
        planned(&env).len(),
        1,
        "the raised cap admits one more run on the next dispatch plan"
    );
}

#[test]
fn provider_cap_lower_does_not_kill_active_runs() {
    let env = Env::new(Some(2));
    env.job("lower-a");
    env.job("lower-b");
    for id in ["lower-a", "lower-b"] {
        let job = env.store.require_job(&JobId::parse(id).expect("id")).expect("job");
        env.ready(&job);
    }
    let run = env.open_run("deepseek");
    reload::reload_project(&env.deps, &env.store, now(), true).expect("baseline");
    assert_eq!(planned(&env).len(), 1, "one slot is still free at cap two");

    env.set_cap(1);
    let lowered = reload::reload_project(&env.deps, &env.store, now(), true).expect("lower");
    assert!(lowered.applied, "{lowered:?}");
    assert_eq!(lowered.provider_caps.get("deepseek"), Some(&1));

    let runs = env.store.open_runs().expect("runs");
    assert_eq!(runs.len(), 1, "a lower cap never finishes a run");
    assert_eq!(runs[0].id, run.id);
    assert!(runs[0].ended_at.is_none());
    assert!(planned(&env).is_empty(), "no new run is admitted at the lower cap");
}

#[test]
fn target_change_is_live() {
    let env = Env::new(None);
    env.store.insert_host_sample(&healthy_sample()).expect("sample");
    seed_estimate(&env.store, &env.project);
    let job = env.job("target-a");
    env.ready(&job);
    let manager = CapacityManager::new(env.deps.clone());
    manager
        .set_target(&env.store, TargetMode::Fixed, 1, false, "owner", "one worker")
        .expect("target one");
    env.open_run("deepseek");

    let request = capacity::request_for(&env.deps, &env.store, &job).expect("request");
    let blocked = manager.admit(&env.store, &request, now()).expect("admit");
    assert_eq!(wait_code(blocked), WaitCode::Target);

    manager
        .set_target(&env.store, TargetMode::Fixed, 2, false, "owner", "two workers")
        .expect("target two");
    let admitted = manager.admit(&env.store, &request, now()).expect("admit");
    assert_eq!(
        admitted,
        AdmitDecision::Admit,
        "the target is read from the store on every admission"
    );
}

#[test]
fn invalid_config_keeps_previous_and_opens_notice() {
    let env = Env::new(Some(1));
    reload::reload_project(&env.deps, &env.store, now(), true).expect("baseline");
    env.replace_config_text("max_concurrent = 1", "max_concurrent = \"one\"");

    let failed = reload::reload_project(&env.deps, &env.store, now(), true).expect("invalid load");
    assert!(!failed.applied);
    assert!(failed.pending);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("max_concurrent")),
        "{failed:?}"
    );
    assert_eq!(
        env.deps
            .config_now()
            .providers
            .get("deepseek")
            .expect("provider")
            .max_concurrent,
        1,
        "the previous config stays effective"
    );
    let state = reload::read_state(&env.store, &env.project)
        .expect("state")
        .expect("published state");
    assert!(state.last_error.is_some());

    let notices = env.store.list_inbox(Some(InboxState::Open)).expect("inbox");
    assert!(
        notices
            .iter()
            .any(|item| { item.project.as_ref() == Some(&env.project) && item.title.contains("config reload failed") }),
        "{notices:?}"
    );
}

#[test]
fn reload_is_atomic_and_no_partial_config_is_applied() {
    let env = Env::new(Some(1));
    reload::reload_project(&env.deps, &env.store, now(), true).expect("baseline");
    let before = reload_notes(&env.store);

    let mut broken = env.disk_config();
    broken
        .providers
        .entry("deepseek".to_owned())
        .or_default()
        .max_concurrent = 4;
    broken.node.max_workers = 0;
    env.set_config(&broken);

    let failed = reload::reload_project(&env.deps, &env.store, now(), true).expect("invalid load");
    assert!(!failed.applied, "{failed:?}");
    assert!(failed.error.is_some());
    let effective = env.deps.config_now();
    assert_eq!(
        effective.providers.get("deepseek").expect("provider").max_concurrent,
        1,
        "the invalid provider cap is not applied"
    );
    assert_eq!(effective.node.max_workers, 4, "no earlier field is applied either");
    assert_eq!(
        reload_notes(&env.store),
        before,
        "a rejected config writes no applied-reload ledger row"
    );
}

#[test]
fn capacity_reload_request_is_served_by_a_running_supervisor() {
    let env = Env::new(Some(1));
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    node.startup().expect("startup");
    node.tick().expect("baseline tick");
    let before = reload_notes(&env.store);

    env.set_cap(2);
    let token = "reload-request-token";
    let request = format!("{}{}", reload::request_prefix(&env.project), token);
    env.store.kv_set(&request, "1").expect("request row");
    node.tick().expect("request tick");

    let text = env
        .store
        .kv_get(&reload::result_key(token))
        .expect("result row")
        .expect("served result");
    let outcome: reload::ReloadOutcome = serde_json::from_str(&text).expect("outcome json");
    assert!(outcome.error.is_none(), "{outcome:?}");
    assert_eq!(outcome.provider_caps.get("deepseek"), Some(&2));
    assert!(!outcome.pending);
    assert!(
        !outcome.applied,
        "the periodic step already applied the change; the request found it current: {outcome:?}"
    );
    assert_eq!(
        reload_notes(&env.store),
        before + 1,
        "one ledger row for the change, none for the already-current request"
    );
    assert!(
        env.store.kv_get(&request).expect("request row").is_none(),
        "a served request is removed"
    );

    let again = "reload-request-again";
    let request = format!("{}{}", reload::request_prefix(&env.project), again);
    env.store.kv_set(&request, "1").expect("second request row");
    node.tick().expect("second request tick");
    let text = env
        .store
        .kv_get(&reload::result_key(again))
        .expect("result row")
        .expect("served result");
    let outcome: reload::ReloadOutcome = serde_json::from_str(&text).expect("outcome json");
    assert!(!outcome.applied, "{outcome:?}");
    assert!(outcome.changed.is_empty(), "{outcome:?}");
    assert_eq!(
        reload_notes(&env.store),
        before + 1,
        "an already-current forced reload writes no ledger row"
    );
    assert!(
        env.store.kv_get(&request).expect("request row").is_none(),
        "a served request is removed"
    );
}

#[test]
fn forced_reload_of_a_semantically_identical_file_records_no_ledger() {
    let env = Env::new(Some(2));
    reload::reload_project(&env.deps, &env.store, now(), true).expect("baseline");
    let before = reload_notes(&env.store);

    let path = env.project_dir.join("vowp.toml");
    let text = std::fs::read_to_string(&path).expect("config text");
    std::fs::write(&path, format!("# a formatting-only change\n{text}")).expect("rewrite config");

    let forced = reload::reload_project(&env.deps, &env.store, now(), true).expect("forced reload");
    assert!(!forced.applied, "{forced:?}");
    assert!(forced.changed.is_empty(), "{forced:?}");
    assert!(!forced.pending, "{forced:?}");
    assert_eq!(
        reload_notes(&env.store),
        before,
        "a semantically identical forced reload writes no ledger row"
    );
    assert!(
        !reload::pending(&env.deps, &env.store).expect("pending"),
        "the new fingerprint is recorded as applied"
    );
}

#[test]
fn supervisor_tick_reloads_config_once_per_change() {
    let env = Env::new(Some(1));
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    node.startup().expect("startup");
    let first = node.tick().expect("baseline tick");
    assert!(first.projects.contains_key(&env.project));
    reload::reload_project(&env.deps, &env.store, now(), false).expect("baseline");
    let before = reload_notes(&env.store);

    env.set_cap(3);
    let second = node.tick().expect("reload tick");
    assert!(second.projects.contains_key(&env.project));
    let project = node.project(&env.project).expect("loaded project");
    assert_eq!(
        reload::effective_config(project.deps())
            .providers
            .get("deepseek")
            .expect("provider")
            .max_concurrent,
        3
    );
    assert_eq!(reload_notes(&env.store), before + 1, "one applied-reload row");

    let third = node.tick().expect("quiet tick");
    assert!(third.projects.contains_key(&env.project));
    assert_eq!(
        reload_notes(&env.store),
        before + 1,
        "an unchanged file does not add another ledger row"
    );
}
