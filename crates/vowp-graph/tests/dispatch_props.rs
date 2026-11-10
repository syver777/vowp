//! Dispatch ordering, slots, provider budgets and the no-overlap property.
//! Spec: `docs/design/crates/vowp-graph.md` §5 and §10.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::prelude::*;
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

/// Builds a profile from a ladder of executor strings.
fn ladder(values: &[&str]) -> Profile {
    Profile {
        ladder: values.iter().map(|value| executor(value)).collect(),
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

/// Parses a test job id.
fn id(value: &str) -> JobId {
    JobId::parse(value).expect("test job id is valid")
}

/// Counts choices by role.
fn role_count(graph: &Graph, choices: &[DispatchChoice], role: Role) -> usize {
    choices
        .iter()
        .filter(|choice| graph.job(&choice.job_id).is_some_and(|job| job.role == role))
        .count()
}

#[test]
fn dispatch_orders_by_priority_then_age() {
    let low = job("a-low", JobState::Ready, Role::Worker, "worker", 10, 300, &["a/**"]);
    let older = job("b-older", JobState::Ready, Role::Worker, "worker", 50, 200, &["b/**"]);
    let newer = job("c-newer", JobState::Ready, Role::Worker, "worker", 50, 100, &["c/**"]);
    let graph = graph_of(vec![low, older, newer]);
    let caps = caps(&[("worker", one_rung("claude-code:claude-sonnet-5-5@medium"))], 10, 0);

    let choices = plan_dispatch(&graph, &[], &caps);
    assert_eq!(ids(&choices), vec![id("c-newer"), id("b-older"), id("a-low")]);
    assert_eq!(choices[0].rung, 0);
    assert_eq!(choices[0].provider, "claude");
    assert_eq!(choices[0].executor, executor("claude-code:claude-sonnet-5-5@medium"));
    assert!(choices[0].node.is_none());
}

#[test]
fn dispatch_skips_overlapping_owns_with_running() {
    let running = job(
        "running",
        JobState::Running,
        Role::Worker,
        "worker",
        90,
        100,
        &["crates/x/**"],
    );
    let overlapping = job(
        "overlapping",
        JobState::Ready,
        Role::Worker,
        "worker",
        90,
        200,
        &["crates/x/src/lib.rs"],
    );
    let free = job(
        "free",
        JobState::Ready,
        Role::Worker,
        "worker",
        80,
        300,
        &["crates/y/**"],
    );
    let graph = graph_of(vec![running.clone(), overlapping, free.clone()]);
    let caps = caps(&[("worker", one_rung("claude-code:claude-sonnet-5-5@medium"))], 10, 0);

    let choices = plan_dispatch(&graph, &[running], &caps);
    assert_eq!(ids(&choices), vec![free.id]);
}

#[test]
fn dispatch_skips_overlap_among_chosen() {
    let first = job(
        "first",
        JobState::Ready,
        Role::Worker,
        "worker",
        50,
        100,
        &["crates/z/**"],
    );
    let second = job(
        "second",
        JobState::Ready,
        Role::Worker,
        "worker",
        50,
        200,
        &["crates/z/src/lib.rs"],
    );
    let graph = graph_of(vec![first.clone(), second]);
    let caps = caps(&[("worker", one_rung("claude-code:claude-sonnet-5-5@medium"))], 10, 0);

    let choices = plan_dispatch(&graph, &[], &caps);
    assert_eq!(ids(&choices), vec![first.id]);
}

#[test]
fn dispatch_respects_worker_and_verifier_slots() {
    let worker_high = job("w-high", JobState::Ready, Role::Worker, "worker", 100, 100, &["w1/**"]);
    let worker_low = job("w-low", JobState::Ready, Role::Worker, "worker", 90, 100, &["w2/**"]);
    let verifier_high = job("v-high", JobState::Ready, Role::Verifier, "verifier", 100, 100, &[]);
    let verifier_low = job("v-low", JobState::Ready, Role::Verifier, "verifier", 90, 100, &[]);
    let graph = graph_of(vec![worker_high, worker_low, verifier_high, verifier_low]);
    let profiles = [
        ("worker", one_rung("claude-code:claude-sonnet-5-5@medium")),
        ("verifier", one_rung("claude-code:claude-sonnet-5-5@medium")),
    ];

    let choices = plan_dispatch(&graph, &[], &caps(&profiles, 1, 1));
    assert_eq!(choices.len(), 2);
    assert_eq!(role_count(&graph, &choices, Role::Worker), 1);
    assert_eq!(role_count(&graph, &choices, Role::Verifier), 1);

    assert!(plan_dispatch(&graph, &[], &caps(&profiles, 0, 0)).is_empty());

    let only_verifiers = plan_dispatch(&graph, &[], &caps(&profiles, 0, 2));
    assert_eq!(only_verifiers.len(), 2);
    assert_eq!(role_count(&graph, &only_verifiers, Role::Verifier), 2);
}

#[test]
fn dispatch_respects_provider_free() {
    let deep_one = job(
        "deep-one",
        JobState::Ready,
        Role::Worker,
        "deepseek",
        100,
        100,
        &["d1/**"],
    );
    let deep_two = job(
        "deep-two",
        JobState::Ready,
        Role::Worker,
        "deepseek",
        90,
        100,
        &["d2/**"],
    );
    let claude = job(
        "claude-one",
        JobState::Ready,
        Role::Worker,
        "claude",
        80,
        100,
        &["c1/**"],
    );
    let graph = graph_of(vec![deep_one, deep_two, claude]);
    let profiles = [
        ("deepseek", one_rung("opencode:deepseek/deepseek-flash#max")),
        ("claude", one_rung("claude-code:claude-sonnet-5-5@medium")),
    ];

    let mut one_deep = caps(&profiles, 10, 0);
    one_deep.provider_free.insert("deepseek".to_owned(), 1);
    let choices = plan_dispatch(&graph, &[], &one_deep);
    assert_eq!(choices.len(), 2);
    let providers: BTreeSet<&str> = choices.iter().map(|choice| choice.provider.as_str()).collect();
    assert_eq!(providers, BTreeSet::from(["claude", "deepseek"]));
    assert_eq!(choices.iter().filter(|choice| choice.provider == "deepseek").count(), 1);

    let mut no_deep = caps(&profiles, 10, 0);
    no_deep.provider_free.insert("deepseek".to_owned(), 0);
    let choices = plan_dispatch(&graph, &[], &no_deep);
    assert_eq!(ids(&choices), vec![id("claude-one")]);

    let choices = plan_dispatch(&graph, &[], &caps(&profiles, 10, 0));
    assert_eq!(choices.len(), 3, "an absent provider key means unlimited");
}

#[test]
fn dispatch_skips_job_when_all_rungs_limited() {
    let mixed = job("mixed", JobState::Ready, Role::Worker, "mixed", 50, 100, &["mixed/**"]);
    let graph = graph_of(vec![mixed]);
    let profiles = [(
        "mixed",
        ladder(&[
            "claude-code:claude-sonnet-5-5@medium",
            "opencode:deepseek/deepseek-flash#max",
        ]),
    )];

    let mut capped = caps(&profiles, 1, 0);
    capped.limited.insert("claude".to_owned(), Some(at(1_700_000_600)));
    capped.limited.insert("deepseek".to_owned(), Some(at(1_700_000_600)));
    assert!(plan_dispatch(&graph, &[], &capped).is_empty());

    let mut indefinite = caps(&profiles, 1, 0);
    indefinite.limited.insert("claude".to_owned(), None);
    indefinite.limited.insert("deepseek".to_owned(), None);
    assert!(plan_dispatch(&graph, &[], &indefinite).is_empty());
}

#[test]
fn dispatch_picks_next_rung_when_first_limited() {
    let mixed = job("mixed", JobState::Ready, Role::Worker, "mixed", 50, 100, &["mixed/**"]);
    let graph = graph_of(vec![mixed]);
    let profiles = [(
        "mixed",
        ladder(&[
            "claude-code:claude-sonnet-5-5@medium",
            "opencode:deepseek/deepseek-flash#max",
        ]),
    )];

    let mut limited = caps(&profiles, 1, 0);
    limited.limited.insert("claude".to_owned(), Some(at(1_700_000_600)));
    let choices = plan_dispatch(&graph, &[], &limited);
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].rung, 1);
    assert_eq!(choices[0].provider, "deepseek");
    assert_eq!(choices[0].executor, executor("opencode:deepseek/deepseek-flash#max"));

    let mut reset = caps(&profiles, 1, 0);
    reset.limited.insert("claude".to_owned(), Some(at(1_700_000_500)));
    let choices = plan_dispatch(&graph, &[], &reset);
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].rung, 0, "a limit that resets at `now` no longer blocks");
    assert_eq!(choices[0].provider, "claude");
}

#[test]
fn dispatch_one_planner_job_at_a_time() {
    let planner_one = job("planner-one", JobState::Ready, Role::Planner, "planner", 100, 100, &[]);
    let planner_two = job("planner-two", JobState::Ready, Role::Planner, "planner", 90, 100, &[]);
    let keeper = job("keeper-one", JobState::Ready, Role::Keeper, "keeper", 80, 100, &[]);
    let graph = graph_of(vec![planner_one.clone(), planner_two, keeper.clone()]);
    let profiles = [
        ("planner", one_rung("claude-code:claude-sonnet-5-5@medium")),
        ("keeper", one_rung("claude-code:claude-sonnet-5-5@medium")),
    ];

    let choices = plan_dispatch(&graph, &[], &caps(&profiles, 0, 0));
    assert_eq!(choices.len(), 2);
    assert_eq!(role_count(&graph, &choices, Role::Planner), 1);
    assert_eq!(role_count(&graph, &choices, Role::Keeper), 1);
    assert_eq!(ids(&choices)[0], planner_one.id, "higher priority planner wins");

    // A running planner suppresses another planner but not the keeper.
    let running_planner = job(
        "running-planner",
        JobState::Running,
        Role::Planner,
        "planner",
        50,
        50,
        &[],
    );
    let graph = graph_of(vec![
        planner_one,
        running_planner.clone(),
        keeper.clone(),
        job("planner-three", JobState::Ready, Role::Planner, "planner", 70, 100, &[]),
    ]);
    let choices = plan_dispatch(&graph, &[running_planner], &caps(&profiles, 0, 0));
    assert_eq!(choices.len(), 1);
    assert_eq!(ids(&choices), vec![keeper.id]);
}

/// Strategy for one job's `owns` list.
fn owns_strategy() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            Just("crates/a/**".to_owned()),
            Just("crates/b/**".to_owned()),
            Just("crates/a/src/lib.rs".to_owned()),
            Just("docs/*.md".to_owned()),
            Just("docs/design/**".to_owned()),
            Just("**/*.rs".to_owned()),
            Just("Cargo.toml".to_owned()),
            Just("crates/c/**".to_owned()),
            Just(String::new()),
        ],
        0..3,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn prop_dispatch_never_two_overlapping_owns(
        ready in prop::collection::vec((any::<bool>(), 0i32..100, owns_strategy()), 0..7),
        running in prop::collection::vec((any::<bool>(), owns_strategy()), 0..4),
        free_workers in 0u32..4,
        free_verifiers in 0u32..3,
        provider_free in proptest::option::of(0u32..4),
    ) {
        let mut jobs = Vec::new();
        for (index, (verifier, priority, owns)) in ready.iter().enumerate() {
            let role = if *verifier { Role::Verifier } else { Role::Worker };
            let profile = if *verifier { "verifier" } else { "worker" };
            let own_refs: Vec<&str> = owns.iter().map(String::as_str).collect();
            jobs.push(job(
                &format!("r{index}"),
                JobState::Ready,
                role,
                profile,
                *priority,
                1_700_000_000 + index as i64,
                &own_refs,
            ));
        }
        for (index, (verifier, owns)) in running.iter().enumerate() {
            let role = if *verifier { Role::Verifier } else { Role::Worker };
            let profile = if *verifier { "verifier" } else { "worker" };
            let own_refs: Vec<&str> = owns.iter().map(String::as_str).collect();
            jobs.push(job(
                &format!("run{index}"),
                JobState::Running,
                role,
                profile,
                50,
                1_700_000_000,
                &own_refs,
            ));
        }
        let running_jobs: Vec<Job> = jobs
            .iter()
            .filter(|job| job.state == JobState::Running)
            .cloned()
            .collect();
        let graph = graph_of(jobs);
        let profiles = [
            ("worker", one_rung("claude-code:claude-sonnet-5-5@medium")),
            ("verifier", one_rung("claude-code:claude-sonnet-5-5@medium")),
        ];
        let mut dispatch_caps = caps(&profiles, free_workers, free_verifiers);
        if let Some(limit) = provider_free {
            dispatch_caps.provider_free.insert("claude".to_owned(), limit);
        }

        let choices = plan_dispatch(&graph, &running_jobs, &dispatch_caps);

        prop_assert!(choices.len() <= (free_workers + free_verifiers) as usize);
        if let Some(limit) = provider_free {
            prop_assert!(choices.len() <= limit as usize);
        }
        prop_assert!(role_count(&graph, &choices, Role::Worker) <= free_workers as usize);
        prop_assert!(role_count(&graph, &choices, Role::Verifier) <= free_verifiers as usize);

        for choice in &choices {
            let chosen = graph.job(&choice.job_id).expect("a choice is a graph job");
            prop_assert_eq!(chosen.state, JobState::Ready);
            for running in &running_jobs {
                prop_assert!(
                    !vowp_graph::owns_overlap(&chosen.owns, &running.owns),
                    "chosen {choice:?} overlaps running {}",
                    running.id
                );
            }
        }
        for (index, left) in choices.iter().enumerate() {
            for right in &choices[index + 1..] {
                let left = graph.job(&left.job_id).expect("a choice is a graph job");
                let right = graph.job(&right.job_id).expect("a choice is a graph job");
                prop_assert!(
                    !vowp_graph::owns_overlap(&left.owns, &right.owns),
                    "chosen jobs {} and {} overlap",
                    left.id,
                    right.id
                );
            }
        }
    }
}
