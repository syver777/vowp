//! Planner and keeper dispatch: ready rows are selectable, live runs block.
//! Spec: `docs/design/crates/vowp-graph.md` §5.
//!
//! `step_dispatch` passes the whole job list as `plan_dispatch`'s `running` argument, so these
//! tests do the same: a row of the right role is not enough to count as a live run.

use std::collections::{BTreeMap, HashMap};

use vowp_graph::{DispatchCaps, DispatchChoice, Graph, plan_dispatch};
use vowp_protocol::{Executor, Job, JobId, JobKind, JobLoop, JobState, OnQuota, Profile, ProjectName, Role, Timestamp};

/// Builds a fixed timestamp for deterministic rows.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Parses a test executor.
fn executor(value: &str) -> Executor {
    Executor::parse(value).expect("test executor is valid")
}

/// Builds a role's single-rung profile.
fn one_rung(value: &str) -> Profile {
    Profile {
        ladder: vec![executor(value)],
        on_quota: OnQuota::NextThenProbeBack,
        node: None,
    }
}

/// Builds a job row with the dispatch-relevant fields.
fn job(id: &str, state: JobState, role: Role, profile: &str, priority: i32, created: i64, owns: &[&str]) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    let kind = match role {
        Role::Verifier => JobKind::Verify,
        Role::Planner => JobKind::Plan,
        Role::Keeper => JobKind::Decision,
        Role::Worker => JobKind::Build,
    };
    Job {
        id: id.clone(),
        project,
        title: format!("job {id}"),
        kind,
        role,
        profile: profile.to_owned(),
        state,
        priority,
        needs: Vec::new(),
        owns: owns.iter().map(|pattern| (*pattern).to_owned()).collect(),
        reads: Vec::new(),
        brief: format!(".vowp/jobs/{id}/brief.md"),
        report: format!(".vowp/jobs/{id}/report.md"),
        gates: Vec::new(),
        loop_: JobLoop::default(),
        branch: Some(format!("job/{id}")),
        worktree: Some(format!("/tmp/wt/{id}")),
        attempts: 0,
        continues: 0,
        hang_restarts: 0,
        parked_reason: None,
        note: String::new(),
        created_by: "test".to_owned(),
        last_progress_at: None,
        state_changed_at: at(1_700_000_000),
        created_at: at(created),
        updated_at: at(1_700_000_000),
    }
}

/// Builds a graph from jobs without explicit edges.
fn graph_of(jobs: Vec<Job>) -> Graph {
    Graph::new(jobs, Vec::new()).expect("test graph is valid")
}

/// Builds caps with no provider or limit pressure.
fn caps(profiles: &[(&str, Profile)], free_workers: u32, free_verifiers: u32) -> DispatchCaps {
    DispatchCaps {
        free_workers,
        free_verifiers,
        provider_free: HashMap::new(),
        limited: HashMap::new(),
        profiles: profiles
            .iter()
            .map(|(name, profile)| ((*name).to_owned(), profile.clone()))
            .collect::<BTreeMap<_, _>>(),
        now: at(1_700_000_500),
    }
}

/// Maps choices to their job ids.
fn ids(choices: &[DispatchChoice]) -> Vec<JobId> {
    choices.iter().map(|choice| choice.job_id.clone()).collect()
}

/// Counts choices by role.
fn role_count(graph: &Graph, choices: &[DispatchChoice], role: Role) -> usize {
    choices
        .iter()
        .filter(|choice| graph.job(&choice.job_id).is_some_and(|job| job.role == role))
        .count()
}

/// Profiles for the planner and keeper roles in these tests.
fn roles() -> [(&'static str, Profile); 2] {
    [
        ("planner", one_rung("claude-code:claude-sonnet-5-5@medium")),
        ("keeper", one_rung("claude-code:claude-sonnet-5-5@medium")),
    ]
}

#[test]
fn ready_planner_job_is_dispatched() {
    let planner = job("asif-0-plan", JobState::Ready, Role::Planner, "planner", 100, 100, &[]);
    let graph = graph_of(vec![planner.clone()]);

    // `step_dispatch` passes the full job list, so this Ready row is in `running` too.
    let choices = plan_dispatch(&graph, std::slice::from_ref(&planner), &caps(&roles(), 0, 0));

    assert_eq!(ids(&choices), vec![planner.id]);
    assert_eq!(choices[0].provider, "claude");
}

#[test]
fn ready_keeper_job_is_dispatched() {
    let keeper = job("keep-asif", JobState::Ready, Role::Keeper, "keeper", 100, 100, &[]);
    let graph = graph_of(vec![keeper.clone()]);

    let choices = plan_dispatch(&graph, std::slice::from_ref(&keeper), &caps(&roles(), 0, 0));

    assert_eq!(ids(&choices), vec![keeper.id]);
}

#[test]
fn only_one_planner_is_chosen_per_call() {
    let high = job("plan-high", JobState::Ready, Role::Planner, "planner", 100, 100, &[]);
    let low = job("plan-low", JobState::Ready, Role::Planner, "planner", 90, 200, &[]);
    let keeper = job("keep-one", JobState::Ready, Role::Keeper, "keeper", 80, 300, &[]);
    let graph = graph_of(vec![high.clone(), low.clone(), keeper.clone()]);
    let jobs = vec![high.clone(), low, keeper.clone()];

    let choices = plan_dispatch(&graph, &jobs, &caps(&roles(), 0, 0));

    assert_eq!(role_count(&graph, &choices, Role::Planner), 1);
    assert_eq!(role_count(&graph, &choices, Role::Keeper), 1);
    assert_eq!(ids(&choices), vec![high.id, keeper.id]);
}

#[test]
fn a_running_planner_blocks_another_planner() {
    let live = job("plan-live", JobState::Running, Role::Planner, "planner", 100, 100, &[]);
    let ready = job("plan-next", JobState::Ready, Role::Planner, "planner", 90, 200, &[]);
    let keeper = job("keep-next", JobState::Ready, Role::Keeper, "keeper", 80, 300, &[]);
    let graph = graph_of(vec![live.clone(), ready.clone(), keeper.clone()]);
    let jobs = vec![live, ready, keeper.clone()];

    let choices = plan_dispatch(&graph, &jobs, &caps(&roles(), 0, 0));

    assert_eq!(ids(&choices), vec![keeper.id], "a live planner blocks only planners");
}

#[test]
fn running_planner_does_not_block_worker_jobs() {
    let live = job("plan-live", JobState::Running, Role::Planner, "planner", 100, 100, &[]);
    let worker = job(
        "build-one",
        JobState::Ready,
        Role::Worker,
        "worker",
        90,
        200,
        &["crates/x/**"],
    );
    let graph = graph_of(vec![live.clone(), worker.clone()]);
    let profiles = [
        ("planner", one_rung("claude-code:claude-sonnet-5-5@medium")),
        ("worker", one_rung("claude-code:claude-sonnet-5-5@medium")),
    ];

    let choices = plan_dispatch(&graph, &[live, worker.clone()], &caps(&profiles, 1, 0));

    assert_eq!(ids(&choices), vec![worker.id]);
}

#[test]
fn a_finished_planner_does_not_block_a_ready_planner() {
    let finished = job("plan-old", JobState::Done, Role::Planner, "planner", 100, 100, &[]);
    let ready = job("plan-new", JobState::Ready, Role::Planner, "planner", 90, 200, &[]);
    let graph = graph_of(vec![finished.clone(), ready.clone()]);

    let choices = plan_dispatch(&graph, &[finished, ready.clone()], &caps(&roles(), 0, 0));

    assert_eq!(ids(&choices), vec![ready.id]);
}
