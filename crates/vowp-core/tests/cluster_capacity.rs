//! Pooled cluster capacity across Home and online worker nodes.
//! Spec: `docs/design/v0.5/scope.md` §6.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::tempdir;
use vowp_core::capacity::CapacityManager;
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::{
    AGENT_PROTOCOL, AdmitDecision, AdmitRequest, AssignmentPayload, Cost, Executor, FakeClock, HarnessKind, HostSample,
    Job, JobId, JobKind, MemoryLog, NewJob, NodeName, NodeStatus, PathRef, Placement, Project, ProjectName,
    ResultTransport, Role, Run, RunId, RunPurpose, RunRole, SampleSource, SessionId, TargetMode, Timestamp, VowpConfig,
    WaitCode,
};
use vowp_store::Store;
use vowp_store::repo_offload::{AgentAssignment, AgentAssignmentState};
use vowp_store::{NodePath, NodePaths};

const NOW_TEXT: &str = "2026-11-10T12:00:00Z";
const GIB: u64 = 1024 * 1024 * 1024;

fn now() -> Timestamp {
    Timestamp::parse(NOW_TEXT).unwrap()
}

fn test_deps(root: &Path, project: &ProjectName) -> Deps {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.node.max_workers = 4;
    config.offload.enabled = true;
    config.offload.default = Placement::Any;
    config.offload.max_remote_runs = 4;
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(config),
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

fn store_at(root: &Path) -> Store {
    Store::open(&NodePaths::new(root))
        .unwrap()
        .with_clock(Arc::new(FakeClock::at(now())))
}

fn register_project(store: &Store, name: &ProjectName) {
    store
        .upsert_project(&Project {
            name: name.clone(),
            path: format!("/tmp/{name}"),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now(),
            updated_at: now(),
        })
        .unwrap();
}

fn insert_job(store: &Store, project: &ProjectName, id: &str) -> Job {
    let new = NewJob {
        id: JobId::parse(id).unwrap(),
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
        brief_text: None,
    };
    store.insert_job(project, &new, "test").unwrap()
}

fn insert_open_run(store: &Store, job: &Job, node: &NodeName) {
    store
        .insert_run(&Run {
            id: RunId::new(),
            job_id: Some(job.id.clone()),
            session_id: None,
            role: RunRole::Worker,
            purpose: RunPurpose::Start,
            profile: Some("worker".to_owned()),
            rung: 0,
            executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").unwrap()),
            provider: Some("deepseek".to_owned()),
            node: node.clone(),
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
        })
        .unwrap();
}

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
        pressure_mem_avg10: None,
        pressure_io_avg10: None,
        source: SampleSource::Proc,
    }
}

/// Adds one online active agent node with the given capacity and project map.
fn insert_worker_node(store: &Store, project: &ProjectName, node: &NodeName, busy_slots: u32, max_slots: u32) {
    store.insert_pending_node(node, &["linux".to_owned()], now()).unwrap();
    store.set_node_status(node, NodeStatus::Active, "", now()).unwrap();
    store
        .upsert_node_path(&NodePath {
            node: node.clone(),
            project: project.clone(),
            local_root: "/srv/vowp".to_owned(),
            worktree_root: "/srv/vowp-worktrees".to_owned(),
            allow: Vec::new(),
            updated_at: now(),
        })
        .unwrap();
    store
        .touch_node(
            node,
            now(),
            Some("0.3.0"),
            Some(AGENT_PROTOCOL),
            &serde_json::json!({
                "os": "linux",
                "arch": "x86_64",
                "agent_version": "0.3.0",
                "harnesses": {"opencode": "1.0.0"},
                "git_version": "git version 2.47.0",
                "labels": ["linux"],
                "secret_names": [],
                "projects": [project.as_str()],
                "capacity": {
                    "cpu_cores": 8,
                    "load1": 0.1,
                    "mem_total_bytes": 16 * GIB,
                    "mem_free_bytes": 8 * GIB,
                    "disk_free_bytes": 100 * GIB,
                    "busy_slots": busy_slots,
                    "max_slots": max_slots,
                    "on_battery": false,
                    "thermal_state": "nominal"
                }
            }),
        )
        .unwrap();
}

/// Inserts one active remote assignment so the next admission sees the slot busy.
fn insert_remote_assignment(store: &Store, project: &ProjectName, node: &NodeName, job: &Job) {
    let run_id = RunId::new();
    let session_id = SessionId::new();
    let assignment_id = format!("assignment-{}", run_id.as_str());
    store
        .insert_assignment(&AgentAssignment {
            id: assignment_id.clone(),
            run_id: run_id.clone(),
            session_id: session_id.clone(),
            project: project.clone(),
            node: node.clone(),
            epoch: 1,
            state: AgentAssignmentState::Running,
            payload: AssignmentPayload {
                assignment_id,
                run_id,
                session_id,
                project: project.clone(),
                job: job.id.clone(),
                executor: Executor::parse("opencode:deepseek/deepseek-flash").unwrap(),
                purpose: RunPurpose::Start,
                prompt: "test".to_owned(),
                cwd_ref: PathRef::Project {
                    project: project.clone(),
                },
                base_commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                remote: "origin".to_owned(),
                remote_url: String::new(),
                result_transport: ResultTransport::Bundle,
                env_names: Vec::new(),
                secret_names: Vec::new(),
                epoch: 1,
                deadline: None,
                spool_cap_bytes: 512 * 1024 * 1024,
            },
            created_at: now(),
            claimed_at: Some(now()),
            expires_at: None,
            detail: String::new(),
        })
        .unwrap();
}

fn request(project: &ProjectName, job: &Job) -> AdmitRequest {
    AdmitRequest {
        project: project.clone(),
        job: Some(job.id.clone()),
        kind: job.kind,
        role: RunRole::Worker,
        harness: Some(HarnessKind::Opencode),
        model: Some("deepseek/deepseek-flash".to_owned()),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").unwrap(),
    }
}

fn wait_code(decision: AdmitDecision) -> WaitCode {
    match decision {
        AdmitDecision::Wait(reason) => reason.code,
        AdmitDecision::Admit => panic!("expected a wait decision, got admit"),
    }
}

/// One home with four Home runs at the target and one online four-slot worker node.
fn four_plus_four_fixture() -> (tempfile::TempDir, Deps, Store, ProjectName, Job) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let project = ProjectName::parse("test").unwrap();
    let store = store_at(&root);
    register_project(&store, &project);
    let deps = test_deps(&root, &project);
    store.insert_host_sample(&healthy_sample()).unwrap();
    let manager = CapacityManager::new(deps.clone());
    manager
        .set_target(&store, TargetMode::Fixed, 4, true, "owner", "four home workers")
        .unwrap();
    for index in 0..4 {
        let job = insert_job(&store, &project, &format!("home-{index}"));
        insert_open_run(&store, &job, &deps.node);
    }
    insert_worker_node(&store, &project, &NodeName::parse("worker-a").unwrap(), 0, 4);
    let ready = insert_job(&store, &project, "ready-job");
    (tmp, deps, store, project, ready)
}

#[test]
fn four_worker_home_plus_four_worker_node_admits_eight_runs() {
    let (_tmp, deps, store, project, ready) = four_plus_four_fixture();
    let manager = CapacityManager::new(deps.clone());
    let worker = NodeName::parse("worker-a").unwrap();

    for admitted in 0..4 {
        let decision = manager.admit(&store, &request(&project, &ready), now()).unwrap();
        assert_eq!(
            decision,
            AdmitDecision::Admit,
            "remote admission {admitted} should use the worker's free slot"
        );
        insert_remote_assignment(&store, &project, &worker, &ready);
    }

    let ninth = manager.admit(&store, &request(&project, &ready), now()).unwrap();
    assert_eq!(
        wait_code(ninth),
        WaitCode::Target,
        "four Home runs plus four remote runs exhaust both machines"
    );
}

#[test]
fn offline_node_falls_back_to_the_home_target() {
    let (_tmp, deps, store, project, ready) = four_plus_four_fixture();
    let worker = NodeName::parse("worker-a").unwrap();
    store
        .touch_node(
            &worker,
            now().minus(Duration::from_secs(120)),
            Some("0.3.0"),
            Some(AGENT_PROTOCOL),
            &serde_json::json!({}),
        )
        .unwrap();
    let manager = CapacityManager::new(deps.clone());
    let decision = manager.admit(&store, &request(&project, &ready), now()).unwrap();
    assert_eq!(
        wait_code(decision),
        WaitCode::Target,
        "an offline worker cannot take the fifth run, so Home's four-run target decides"
    );
}

#[test]
fn report_and_snapshot_carry_combined_cluster_totals() {
    let (_tmp, deps, store, project, _ready) = four_plus_four_fixture();
    // The fixture already opened four Home runs; end two so the busy count is distinct
    // from the Home target and the node's own busy slot.
    let home_runs: Vec<_> = store.open_runs().unwrap();
    for run in home_runs.iter().take(2) {
        store
            .finish_run(
                &run.id,
                &vowp_store::RunFinish {
                    outcome: vowp_protocol::RunOutcome::Ok,
                    exit_code: Some(0),
                    limit_kind: None,
                    cost: Cost::unknown(),
                    native_session_id: None,
                    detail: String::new(),
                },
            )
            .unwrap();
    }
    let worker = NodeName::parse("worker-a").unwrap();
    store
        .touch_node(
            &worker,
            now(),
            Some("0.3.0"),
            Some(AGENT_PROTOCOL),
            &serde_json::json!({
                "os": "linux",
                "arch": "x86_64",
                "agent_version": "0.3.0",
                "harnesses": {"opencode": "1.0.0"},
                "git_version": "git version 2.47.0",
                "labels": ["linux"],
                "secret_names": [],
                "projects": [project.as_str()],
                "capacity": {
                    "cpu_cores": 8,
                    "load1": 0.1,
                    "mem_total_bytes": 16 * GIB,
                    "mem_free_bytes": 8 * GIB,
                    "disk_free_bytes": 100 * GIB,
                    "busy_slots": 1,
                    "max_slots": 4,
                    "on_battery": false,
                    "thermal_state": "nominal"
                }
            }),
        )
        .unwrap();

    let manager = CapacityManager::new(deps.clone());
    let report = manager.report(&store, now()).unwrap();
    assert_eq!(report.running.runs, 2, "the Home card stays Home-only");
    assert_eq!(report.cluster.online_worker_nodes, 1);
    assert_eq!(report.cluster.target_workers, 8, "4 Home + 4 worker slots");
    assert_eq!(report.cluster.busy_workers, 3, "2 Home runs + 1 node busy slot");
    assert_eq!(report.cluster.cpu_cores, 12, "4 Home + 8 node cores");
    assert_eq!(report.cluster.mem_total_bytes, 24 * GIB);
    assert_eq!(report.cluster.mem_available_bytes, 14 * GIB);
    assert_eq!(report.cluster.disk_free_bytes, 150 * GIB);

    let snapshot = vowp_core::fleet::build_snapshot(&deps, &store, now()).unwrap();
    assert_eq!(snapshot.capacity.cluster, report.cluster);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["cluster"]["online_worker_nodes"], 1);
    assert_eq!(json["cluster"]["target_workers"], 8);
    assert_eq!(json["cluster"]["busy_workers"], 3);
    let snapshot_json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(snapshot_json["capacity"]["cluster"]["online_worker_nodes"], 1);
}

#[test]
fn a_single_home_reports_no_worker_nodes_and_its_own_totals() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("node");
    let project = ProjectName::parse("solo").unwrap();
    let store = store_at(&root);
    register_project(&store, &project);
    let deps = test_deps(&root, &project);
    store.insert_host_sample(&healthy_sample()).unwrap();
    let manager = CapacityManager::new(deps.clone());
    manager
        .set_target(&store, TargetMode::Fixed, 4, true, "owner", "four")
        .unwrap();

    let cluster = CapacityManager::new(deps.clone())
        .report(&store, now())
        .unwrap()
        .cluster;
    assert_eq!(cluster.online_worker_nodes, 0);
    assert_eq!(cluster.target_workers, 4);
    assert_eq!(cluster.busy_workers, 0);
    assert_eq!(cluster.cpu_cores, 4);
    assert_eq!(cluster.mem_total_bytes, 8 * GIB);
    assert_eq!(cluster.mem_available_bytes, 6 * GIB);
    assert_eq!(cluster.disk_free_bytes, 50 * GIB);
}
