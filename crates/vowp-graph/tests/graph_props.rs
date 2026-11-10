//! Graph validation, readiness, layering and the two graph property tests.
//! Spec: `docs/design/crates/vowp-graph.md` §2 and §10.

use std::collections::{BTreeSet, HashMap};

use proptest::prelude::*;
use vowp_graph::{Graph, GraphError};
use vowp_protocol::{Edge, EdgeKind, Job, JobId, JobKind, JobLoop, JobState, ProjectName, Role, Timestamp};

/// Builds a fixed timestamp for deterministic test rows.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// The spec's graph test helper: a job with fixed defaults.
fn job(id: &str, state: JobState, owns: &[&str], needs: &[&str], priority: i32) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    Job {
        id: id.clone(),
        project,
        title: format!("job {id}"),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state,
        priority,
        needs: needs
            .iter()
            .map(|need| JobId::parse(need).expect("test need id is valid"))
            .collect(),
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
        created_at: at(1_700_000_000),
        updated_at: at(1_700_000_000),
    }
}

/// Builds an edge from ids.
fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
    Edge {
        src: JobId::parse(src).expect("test edge src is valid"),
        dst: JobId::parse(dst).expect("test edge dst is valid"),
        kind,
    }
}

fn position(order: &[JobId], id: &str) -> usize {
    let id = JobId::parse(id).expect("test id is valid");
    order
        .iter()
        .position(|candidate| *candidate == id)
        .expect("job is in the topological order")
}

#[test]
fn graph_new_rejects_unknown_edge_endpoint() {
    let jobs = vec![job("a", JobState::Pending, &["crates/a/**"], &[], 50)];
    let edges = vec![edge("a", "ghost", EdgeKind::Needs)];
    match Graph::new(jobs, edges) {
        Err(GraphError::UnknownJob(id)) => assert_eq!(id.as_str(), "ghost"),
        other => panic!("expected UnknownJob, got {other:?}"),
    }

    let jobs = vec![job("a", JobState::Pending, &["crates/a/**"], &[], 50)];
    let edges = vec![edge("ghost", "a", EdgeKind::Verifies)];
    match Graph::new(jobs, edges) {
        Err(GraphError::UnknownJob(id)) => assert_eq!(id.as_str(), "ghost"),
        other => panic!("expected UnknownJob, got {other:?}"),
    }

    let jobs = vec![job("a", JobState::Pending, &["crates/a/**"], &["ghost"], 50)];
    match Graph::new(jobs, Vec::new()) {
        Err(GraphError::UnknownJob(id)) => assert_eq!(id.as_str(), "ghost"),
        other => panic!("expected UnknownJob, got {other:?}"),
    }
}

#[test]
fn graph_new_rejects_duplicate_ids() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &[], 50),
        job("a", JobState::Ready, &[], &[], 50),
    ];
    match Graph::new(jobs, Vec::new()) {
        Err(GraphError::DuplicateJob(id)) => assert_eq!(id.as_str(), "a"),
        other => panic!("expected DuplicateJob, got {other:?}"),
    }
}

#[test]
fn graph_new_rejects_self_edge() {
    let jobs = vec![job("a", JobState::Pending, &[], &[], 50)];
    for kind in [
        EdgeKind::Needs,
        EdgeKind::Verifies,
        EdgeKind::Fixes,
        EdgeKind::Escalates,
        EdgeKind::Blocks,
    ] {
        let edges = vec![edge("a", "a", kind)];
        match Graph::new(jobs.clone(), edges) {
            Err(GraphError::SelfEdge(id)) => assert_eq!(id.as_str(), "a"),
            other => panic!("expected SelfEdge for {kind:?}, got {other:?}"),
        }
    }

    // A `needs` entry naming the job itself becomes a self `Needs` edge.
    let self_needing = vec![job("a", JobState::Pending, &[], &["a"], 50)];
    match Graph::new(self_needing, Vec::new()) {
        Err(GraphError::SelfEdge(id)) => assert_eq!(id.as_str(), "a"),
        other => panic!("expected SelfEdge for a self-need, got {other:?}"),
    }
}

#[test]
fn graph_new_detects_needs_cycle_and_reports_path() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b"], 50),
        job("b", JobState::Pending, &[], &["c"], 50),
        job("c", JobState::Pending, &[], &["a"], 50),
    ];
    match Graph::new(jobs, Vec::new()) {
        Err(GraphError::Cycle(path)) => {
            assert_eq!(
                path.first(),
                path.last(),
                "cycle path must start and end at the same job"
            );
            assert!(path.len() >= 4, "cycle path must contain at least one full loop");
            for id in ["a", "b", "c"] {
                let id = JobId::parse(id).expect("test id is valid");
                assert!(path.contains(&id), "cycle path {path:?} misses {id}");
            }
        }
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[test]
fn graph_needs_plus_blocks_same_direction_is_not_a_cycle() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b"], 50),
        job("b", JobState::Pending, &[], &[], 50),
    ];
    let edges = vec![edge("b", "a", EdgeKind::Blocks)];
    let graph = Graph::new(jobs, edges).expect("both constraints demand b before a");
    let order = graph.topo_order();
    assert!(position(&order, "b") < position(&order, "a"));
}

#[test]
fn graph_fixes_edge_may_form_loop() {
    let jobs = vec![
        job("orig", JobState::Fixing, &["crates/x/**"], &["fix"], 50),
        job("fix", JobState::Pending, &["crates/x/**"], &[], 50),
    ];
    let edges = vec![edge("fix", "orig", EdgeKind::Fixes)];
    let graph = Graph::new(jobs, edges).expect("a fixes back-edge never counts as a cycle");
    let fix = JobId::parse("fix").expect("test id is valid");
    let orig = JobId::parse("orig").expect("test id is valid");
    assert_eq!(graph.fix_parent(&fix), Some(&orig));
    assert!(
        graph
            .edges()
            .iter()
            .any(|candidate| candidate.kind == EdgeKind::Needs && candidate.src == orig && candidate.dst == fix)
    );

    // The other wrong reading of the loop edge (src -> dst) would form a cycle with this
    // block edge; the correct implementation ignores Fixes and accepts the graph.
    let jobs = vec![
        job("orig", JobState::Fixing, &["crates/x/**"], &[], 50),
        job("fix", JobState::Pending, &["crates/x/**"], &[], 50),
    ];
    let edges = vec![
        edge("fix", "orig", EdgeKind::Fixes),
        edge("orig", "fix", EdgeKind::Blocks),
    ];
    let graph = Graph::new(jobs, edges).expect("a fixes edge never participates in cycle detection");
    assert_eq!(graph.fix_parent(&fix), Some(&orig));
}

#[test]
fn graph_new_detects_cycle_through_verifies_and_escalates() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &[], 50),
        job("b", JobState::Pending, &[], &[], 50),
        job("c", JobState::Pending, &[], &["a"], 50),
    ];
    let edges = vec![edge("a", "b", EdgeKind::Verifies), edge("b", "c", EdgeKind::Escalates)];
    match Graph::new(jobs, edges) {
        Err(GraphError::Cycle(path)) => {
            assert_eq!(path.first(), path.last());
            assert!(path.len() >= 4);
        }
        other => panic!("expected Cycle through verifies/escalates, got {other:?}"),
    }
}

#[test]
fn graph_adds_needs_edges_from_job_needs() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b"], 50),
        job("b", JobState::Done, &[], &[], 50),
    ];
    let graph = Graph::new(jobs, Vec::new()).expect("needs entries are mirrored into edges");
    let a = JobId::parse("a").expect("test id is valid");
    let b = JobId::parse("b").expect("test id is valid");
    assert_eq!(graph.edges().len(), 1);
    assert_eq!(graph.edges()[0].src, a);
    assert_eq!(graph.edges()[0].dst, b);
    assert_eq!(graph.edges()[0].kind, EdgeKind::Needs);
    let needs: Vec<&JobId> = graph.needs_of(&a).iter().map(|job| &job.id).collect();
    assert_eq!(needs, vec![&b]);
}

#[test]
fn graph_rejects_needs_edge_missing_in_job() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &[], 50),
        job("b", JobState::Done, &[], &[], 50),
    ];
    let edges = vec![edge("a", "b", EdgeKind::Needs)];
    match Graph::new(jobs, edges) {
        Err(GraphError::EdgeMismatch(id)) => assert_eq!(id.as_str(), "a"),
        other => panic!("expected EdgeMismatch, got {other:?}"),
    }
}

#[test]
fn ready_jobs_requires_all_needs_done() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b", "c"], 50),
        job("b", JobState::Done, &[], &[], 50),
        job("c", JobState::Running, &[], &[], 50),
    ];
    let graph = Graph::new(jobs, Vec::new()).expect("valid graph");
    let a = JobId::parse("a").expect("test id is valid");
    assert!(!graph.ready_jobs().contains(&a), "c is still running");

    let mut c = graph
        .job(&JobId::parse("c").expect("test id is valid"))
        .expect("c exists")
        .clone();
    c.state = JobState::Done;
    let graph = graph.with_job_replaced(c).expect("state update keeps the graph valid");
    assert!(graph.ready_jobs().contains(&a), "all needs are done now");

    let mut b = graph
        .job(&JobId::parse("b").expect("test id is valid"))
        .expect("b exists")
        .clone();
    b.state = JobState::Failed;
    let graph = graph.with_job_replaced(b).expect("state update keeps the graph valid");
    assert!(!graph.ready_jobs().contains(&a), "a failed need is not done");
}

#[test]
fn ready_jobs_respects_blocks_edge() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &[], 50),
        job("b", JobState::Pending, &[], &[], 50),
    ];
    let edges = vec![edge("b", "a", EdgeKind::Blocks)];
    let graph = Graph::new(jobs, edges).expect("valid graph");
    let a = JobId::parse("a").expect("test id is valid");
    let b = JobId::parse("b").expect("test id is valid");
    assert!(!graph.ready_jobs().contains(&a), "b has not reached a terminal state");

    let mut b_job = graph.job(&b).expect("b exists").clone();
    b_job.state = JobState::Done;
    let graph = graph
        .with_job_replaced(b_job)
        .expect("state update keeps the graph valid");
    assert!(graph.ready_jobs().contains(&a), "a terminal blocker no longer blocks");

    let mut b_job = graph.job(&b).expect("b exists").clone();
    b_job.state = JobState::Replanned;
    let graph = graph
        .with_job_replaced(b_job)
        .expect("state update keeps the graph valid");
    assert!(graph.ready_jobs().contains(&a), "replanned is terminal too");

    let mut b_job = graph.job(&b).expect("b exists").clone();
    b_job.state = JobState::Cancelled;
    let graph = graph
        .with_job_replaced(b_job)
        .expect("state update keeps the graph valid");
    assert!(graph.ready_jobs().contains(&a), "cancelled is terminal too");
}

#[test]
fn ready_jobs_ignores_non_pending() {
    let jobs = vec![
        job("a", JobState::Running, &[], &["b"], 50),
        job("b", JobState::Done, &[], &[], 50),
        job("c", JobState::Pending, &[], &[], 50),
    ];
    let graph = Graph::new(jobs, Vec::new()).expect("valid graph");
    let ready = graph.ready_jobs();
    assert!(!ready.contains(&JobId::parse("a").expect("test id is valid")));
    assert!(ready.contains(&JobId::parse("c").expect("test id is valid")));

    let mut a = graph
        .job(&JobId::parse("a").expect("test id is valid"))
        .expect("a exists")
        .clone();
    a.state = JobState::Gating;
    let graph = graph.with_job_replaced(a).expect("state update keeps the graph valid");
    assert!(
        !graph
            .ready_jobs()
            .contains(&JobId::parse("a").expect("test id is valid"))
    );
}

#[test]
fn ready_jobs_order_priority_then_age() {
    let mut a = job("a", JobState::Pending, &[], &[], 10);
    a.created_at = at(300);
    let mut b = job("b", JobState::Pending, &[], &[], 50);
    b.created_at = at(200);
    let mut c = job("c", JobState::Pending, &[], &[], 50);
    c.created_at = at(100);
    let mut d = job("d", JobState::Pending, &[], &[], 50);
    d.created_at = at(100);

    let graph = Graph::new(vec![a, b, c, d], Vec::new()).expect("valid graph");
    let ready: Vec<String> = graph.ready_jobs().iter().map(|id| id.to_string()).collect();
    assert_eq!(ready, vec!["c", "d", "b", "a"]);
}

#[test]
fn topo_order_deterministic_ties() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b", "c"], 10),
        job("b", JobState::Pending, &[], &["d"], 20),
        job("c", JobState::Pending, &[], &["d"], 10),
        job("d", JobState::Done, &[], &[], 30),
    ];
    let graph = Graph::new(jobs, Vec::new()).expect("valid diamond graph");
    let order: Vec<String> = graph.topo_order().iter().map(|id| id.to_string()).collect();
    assert_eq!(order, vec!["d", "b", "c", "a"]);
}

#[test]
fn layer_is_longest_needs_path() {
    let jobs = vec![
        job("a", JobState::Pending, &[], &["b", "c"], 50),
        job("b", JobState::Pending, &[], &["d"], 50),
        job("c", JobState::Pending, &[], &[], 50),
        job("d", JobState::Pending, &[], &[], 50),
    ];
    let graph = Graph::new(jobs, Vec::new()).expect("valid graph");
    assert_eq!(graph.layer(&JobId::parse("d").expect("test id is valid")), 0);
    assert_eq!(graph.layer(&JobId::parse("c").expect("test id is valid")), 0);
    assert_eq!(graph.layer(&JobId::parse("b").expect("test id is valid")), 1);
    assert_eq!(graph.layer(&JobId::parse("a").expect("test id is valid")), 2);
    assert_eq!(graph.layer(&JobId::parse("unknown").expect("test id is valid")), 0);
}

/// Generates a job id for an index.
fn indexed_id(index: usize) -> String {
    format!("j{index:02}")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

    #[test]
    fn prop_topo_order_is_valid(need_lists in prop::collection::vec(prop::collection::vec(0usize..30, 0..4), 1..=30)) {
        let mut jobs = Vec::new();
        for (index, listed) in need_lists.iter().enumerate() {
            let needs: BTreeSet<usize> = listed.iter().copied().filter(|dependency| *dependency < index).collect();
            let need_ids: Vec<String> = needs.iter().map(|dependency| indexed_id(*dependency)).collect();
            let need_refs: Vec<&str> = need_ids.iter().map(String::as_str).collect();
            jobs.push(job(&indexed_id(index), JobState::Pending, &[], &need_refs, 50));
        }
        let count = jobs.len();
        let graph = Graph::new(jobs, Vec::new()).expect("needs only point at earlier jobs, so the graph is acyclic");
        let order = graph.topo_order();
        prop_assert_eq!(order.len(), count);

        let mut positions: HashMap<JobId, usize> = HashMap::new();
        for (index, id) in order.iter().enumerate() {
            prop_assert!(positions.insert(id.clone(), index).is_none(), "job appears twice: {id}");
        }
        for job in graph.jobs() {
            let dependent = positions[&job.id];
            for need in &job.needs {
                prop_assert!(positions[need] < dependent, "{} must run after {}", job.id, need);
            }
        }
    }

    #[test]
    fn prop_ready_jobs_matches_brute_force(
        rows in prop::collection::vec(
            (
                0usize..13,
                -20i32..20,
                0i64..100,
                prop::collection::vec(0usize..30, 0..3),
                prop::collection::vec(0usize..30, 0..3),
            ),
            1..=30,
        ),
    ) {
        let states = [
            JobState::Pending,
            JobState::Ready,
            JobState::Running,
            JobState::Stalled,
            JobState::Gating,
            JobState::Reviewing,
            JobState::Fixing,
            JobState::Merging,
            JobState::Done,
            JobState::Failed,
            JobState::Parked,
            JobState::Replanned,
            JobState::Cancelled,
        ];
        let mut jobs = Vec::new();
        let mut block_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
        for (index, (state_index, priority, age, needs, blockers)) in rows.iter().enumerate() {
            let need_indices: BTreeSet<usize> = needs.iter().copied().filter(|dependency| *dependency < index).collect();
            let need_ids: Vec<String> = need_indices.iter().map(|dependency| indexed_id(*dependency)).collect();
            let need_refs: Vec<&str> = need_ids.iter().map(String::as_str).collect();
            let mut node = job(&indexed_id(index), states[*state_index], &[], &need_refs, *priority);
            node.created_at = at(*age);
            jobs.push(node);
            for blocker in blockers.iter().copied().filter(|blocker| *blocker < index) {
                block_pairs.insert((blocker, index));
            }
        }

        let state_by_id: HashMap<JobId, JobState> =
            jobs.iter().map(|node| (node.id.clone(), node.state)).collect();
        let mut expected: Vec<&Job> = Vec::new();
        for (index, node) in jobs.iter().enumerate() {
            if node.state != JobState::Pending {
                continue;
            }
            if !node
                .needs
                .iter()
                .all(|need| state_by_id.get(need) == Some(&JobState::Done))
            {
                continue;
            }
            let blocked = block_pairs.iter().any(|(blocker, blocked)| {
                *blocked == index
                    && state_by_id
                        .get(&JobId::parse(&indexed_id(*blocker)).expect("valid id"))
                        .is_some_and(|state| !state.is_terminal())
            });
            if !blocked {
                expected.push(node);
            }
        }
        expected.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        let expected: Vec<JobId> = expected.into_iter().map(|node| node.id.clone()).collect();

        let block_edges: Vec<Edge> = block_pairs
            .iter()
            .map(|(blocker, blocked)| {
                Edge {
                    src: JobId::parse(&indexed_id(*blocker)).expect("valid id"),
                    dst: JobId::parse(&indexed_id(*blocked)).expect("valid id"),
                    kind: EdgeKind::Blocks,
                }
            })
            .collect();
        let graph = Graph::new(jobs, block_edges).expect("all constraints follow index order, so the graph is acyclic");
        prop_assert_eq!(graph.ready_jobs(), expected);
    }
}
