//! Run, gate-result and worker-slot repository tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{
    Clock, CmdGate, Cost, Evidence, Executor, FakeClock, Gate, GateResult, Job, JobId, JobKind, NewJob, NodeName,
    Project, ProjectName, Role, Run, RunOutcome, RunPurpose, RunRole, Timestamp, Tokens, Worker, WorkerKind,
    WorkerState,
};
use vowp_store::{NodePaths, RunFinish, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A file-backed store with a controllable clock, one project and one job.
struct TestStore {
    _tmp: TempDir,
    paths: NodePaths,
    store: Store,
    clock: FakeClock,
    project: ProjectName,
    job: Job,
}

impl TestStore {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project");
        let now = clock.now();
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: "/tmp/proj".to_owned(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("insert project");
        let job = store
            .insert_job(
                &project,
                &NewJob {
                    id: JobId::parse("run-job").expect("job id"),
                    title: "run job".to_owned(),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: None,
                    needs: Vec::new(),
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: None,
                    priority: None,
                    loop_: None,
                    brief_text: None,
                },
                "human",
            )
            .expect("insert job");
        Self {
            _tmp: tmp,
            paths,
            store,
            clock,
            project,
            job,
        }
    }
}

/// Builds a minimal run for `job`.
fn sample_run(job: &Job) -> Run {
    Run {
        id: vowp_protocol::RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor")),
        provider: Some("claude".to_owned()),
        node: NodeName::parse("local").expect("node"),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some("/tmp/proj".to_owned()),
        started_at: Timestamp::parse(EPOCH).expect("timestamp"),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    }
}

/// Builds a command-gate result.
fn cmd_gate_result(job: &Job, attempt: u32, gate_index: i32, tail: &str) -> GateResult {
    GateResult {
        id: Some(999),
        job_id: job.id.clone(),
        run_id: None,
        attempt,
        gate_index,
        gate: Gate::Cmd(CmdGate {
            run: "cargo test".to_owned(),
            expect_exit: 0,
            expect_pattern: None,
            timeout_secs: 1800,
            cwd: None,
        }),
        passed: true,
        evidence: Evidence::Cmd {
            command: "cargo test".to_owned(),
            exit_code: Some(0),
            timed_out: false,
            tail: tail.to_owned(),
            duration_ms: 42,
        },
        output_blob: None,
        duration_ms: 42,
        ts: Timestamp::parse(EPOCH).expect("timestamp"),
    }
}

#[test]
fn run_lifecycle_and_open_runs() {
    let fixture = TestStore::new();
    let run = sample_run(&fixture.job);
    fixture.store.insert_run(&run).expect("insert run");

    assert_eq!(fixture.store.open_runs().expect("open runs"), vec![run.clone()]);
    assert_eq!(
        fixture.store.runs_of_job(&fixture.job.id).expect("runs of job"),
        vec![run.clone()]
    );
    assert_eq!(
        fixture.store.last_run_of_job(&fixture.job.id).expect("last run"),
        Some(run.clone())
    );

    fixture.clock.advance(Duration::from_secs(30));
    fixture
        .store
        .update_run_attach(&run.id, Some(4242), Some("native-1"), Some("job-run-job"), None)
        .expect("attach run");

    let long_detail = format!("{}END", "x".repeat(2000));
    let cost = Cost::receipt(
        0.25,
        Tokens {
            input: 100,
            output: 40,
            reasoning: 5,
            cache_read: 7,
            cache_write: 3,
        },
    );
    fixture
        .store
        .finish_run(
            &run.id,
            &RunFinish {
                outcome: RunOutcome::Ok,
                exit_code: Some(0),
                limit_kind: None,
                cost: cost.clone(),
                native_session_id: Some("native-2".to_owned()),
                detail: long_detail,
            },
        )
        .expect("finish run");

    let finished = fixture.store.require_run(&run.id).expect("reload run");
    assert_eq!(finished.ended_at, Some(fixture.clock.now()));
    assert_eq!(finished.outcome, Some(RunOutcome::Ok));
    assert_eq!(finished.exit_code, Some(0));
    assert_eq!(finished.cost, cost);
    assert_eq!(finished.pid, Some(4242));
    assert_eq!(finished.window, Some("job-run-job".to_owned()));
    assert_eq!(finished.native_session_id, Some("native-2".to_owned()));
    assert_eq!(
        finished.executor,
        Some(Executor::parse("claude-code:claude-sonnet-5-5@medium").unwrap())
    );
    assert_eq!(finished.detail.chars().count(), 1500);
    assert!(finished.detail.ends_with("END"));
    assert!(fixture.store.open_runs().expect("open runs").is_empty());

    let error = fixture
        .store
        .finish_run(
            &run.id,
            &RunFinish {
                outcome: RunOutcome::Error,
                exit_code: Some(1),
                limit_kind: None,
                cost: Cost::unknown(),
                native_session_id: None,
                detail: "again".to_owned(),
            },
        )
        .expect_err("second finish");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");

    // The schema CHECK keeps ended_at and outcome in lockstep even for raw SQL.
    let connection = Connection::open(fixture.paths.project_db_path(&fixture.project)).expect("project raw connection");
    let error = connection
        .execute("UPDATE runs SET ended_at = NULL WHERE id = ?1", [run.id.as_str()])
        .expect_err("CHECK must reject half-finished state");
    assert!(error.to_string().contains("CHECK"), "got {error}");
}

#[test]
fn gate_results_ordering_and_latest() {
    let fixture = TestStore::new();

    let first = cmd_gate_result(&fixture.job, 0, 2, "attempt 0 index 2");
    let first_id = fixture.store.insert_gate_result(&first).expect("insert gate result");
    fixture
        .store
        .insert_gate_result(&cmd_gate_result(&fixture.job, 0, 0, "attempt 0 index 0"))
        .expect("insert gate result");
    fixture
        .store
        .insert_gate_result(&cmd_gate_result(&fixture.job, 0, 1, "attempt 0 index 1"))
        .expect("insert gate result");
    fixture
        .store
        .insert_gate_result(&cmd_gate_result(&fixture.job, 1, 1, "attempt 1 index 1"))
        .expect("insert gate result");
    fixture
        .store
        .insert_gate_result(&cmd_gate_result(&fixture.job, 1, 0, "attempt 1 index 0"))
        .expect("insert gate result");

    let attempt_zero = fixture
        .store
        .gate_results_for(&fixture.job.id, 0)
        .expect("attempt zero");
    assert_eq!(
        attempt_zero.iter().map(|result| result.gate_index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(attempt_zero[2].id, Some(first_id), "insert returns the rowid");
    assert_ne!(attempt_zero[2].id, Some(999), "the input id is ignored");
    let expected_gate = Gate::Cmd(CmdGate {
        run: "cargo test".to_owned(),
        expect_exit: 0,
        expect_pattern: None,
        timeout_secs: 1800,
        cwd: None,
    });
    assert_eq!(attempt_zero[2].gate, expected_gate);
    assert_eq!(
        attempt_zero[2].evidence,
        Evidence::Cmd {
            command: "cargo test".to_owned(),
            exit_code: Some(0),
            timed_out: false,
            tail: "attempt 0 index 2".to_owned(),
            duration_ms: 42,
        }
    );

    let latest = fixture.store.latest_gate_results(&fixture.job.id).expect("latest");
    assert_eq!(latest.len(), 2, "only the newest attempt");
    assert_eq!(
        latest.iter().map(|result| result.gate_index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(latest.iter().all(|result| result.attempt == 1));
    assert!(
        latest
            .iter()
            .all(|result| matches!(result.evidence, Evidence::Cmd { .. }))
    );

    let other = JobId::parse("other-job").expect("id");
    assert!(fixture.store.latest_gate_results(&other).expect("latest").is_empty());
}

#[test]
fn workers_slots_ensure() {
    let fixture = TestStore::new();
    let node = NodeName::parse("local").expect("node");

    fixture
        .store
        .ensure_worker_slots(&node, WorkerKind::Worker, 4)
        .expect("ensure 4");
    let workers = fixture.store.list_workers(&node).expect("list workers");
    assert_eq!(workers.len(), 4);
    assert_eq!(
        workers.iter().map(|worker| worker.slot).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(workers.iter().all(|worker| worker.state == WorkerState::Idle));
    assert!(workers.iter().all(|worker| worker.kind == WorkerKind::Worker));

    let run = sample_run(&fixture.job);
    fixture.store.insert_run(&run).expect("insert run");
    fixture
        .store
        .upsert_worker(&Worker {
            node: node.clone(),
            slot: 4,
            kind: WorkerKind::Worker,
            state: WorkerState::Busy,
            run_id: Some(run.id.clone()),
            job_id: Some(fixture.job.id.clone()),
            window: Some("job-run-job".to_owned()),
            updated_at: fixture.clock.now(),
        })
        .expect("occupy slot 4");

    fixture
        .store
        .ensure_worker_slots(&node, WorkerKind::Worker, 2)
        .expect("shrink to 2");
    let workers = fixture.store.list_workers(&node).expect("list workers");
    assert_eq!(
        workers.iter().map(|worker| worker.slot).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(workers[2].state, WorkerState::Busy, "busy slot 4 survives the shrink");
    assert_eq!(workers[2].run_id, Some(run.id));

    fixture
        .store
        .ensure_worker_slots(&node, WorkerKind::Worker, 4)
        .expect("grow back to 4");
    let workers = fixture.store.list_workers(&node).expect("list workers");
    assert_eq!(
        workers.iter().map(|worker| worker.slot).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(workers[2].state, WorkerState::Idle, "slot 3 is recreated idle");
    assert_eq!(workers[3].state, WorkerState::Busy, "slot 4 is untouched");

    fixture
        .store
        .ensure_worker_slots(&node, WorkerKind::Verifier, 1)
        .expect("one verifier slot");
    let workers = fixture.store.list_workers(&node).expect("list workers");
    assert_eq!(workers.len(), 5);
    assert_eq!(
        workers
            .iter()
            .filter(|worker| worker.kind == WorkerKind::Verifier)
            .count(),
        1
    );
    assert_eq!(fixture.project.as_str(), "proj");
    assert_eq!(fixture.store.schema_version().expect("version"), 1);
}

#[test]
fn insert_run_rejects_an_already_finished_run() {
    let fixture = TestStore::new();
    let ended_at = Timestamp::parse("2026-01-01T00:00:30.000Z").expect("timestamp");

    let mut with_end = sample_run(&fixture.job);
    with_end.ended_at = Some(ended_at);
    let error = fixture.store.insert_run(&with_end).expect_err("ended run");
    assert!(
        matches!(error, vowp_store::StoreError::InvalidInput(_)),
        "got {error:?}"
    );

    let mut with_outcome = sample_run(&fixture.job);
    with_outcome.outcome = Some(RunOutcome::Ok);
    let error = fixture.store.insert_run(&with_outcome).expect_err("finished run");
    assert!(
        matches!(error, vowp_store::StoreError::InvalidInput(_)),
        "got {error:?}"
    );

    assert!(fixture.store.open_runs().expect("open runs").is_empty());
    assert!(fixture.store.runs_of_job(&fixture.job.id).expect("runs").is_empty());
}
