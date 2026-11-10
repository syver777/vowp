//! YAML export/import round-trips, validation and runtime-field exclusion.
//! Spec: `docs/design/crates/vowp-graph.md` §7 and §10.

use vowp_graph::{Graph, GraphError, export_yaml, import_yaml};
use vowp_protocol::{
    CmdGate, Edge, EdgeKind, Gate, Job, JobId, JobKind, JobLoop, JobState, NewJob, ParkedReason, ProjectName,
    PushedGate, ReportGate, ReviewGate, Role, Timestamp,
};

/// Builds a fixed timestamp.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Builds one job row with the fields the YAML format touches.
fn job(id: &str, kind: JobKind, role: Role, needs: &[&str], owns: &[&str]) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    Job {
        id: id.clone(),
        project,
        title: format!("job {id}"),
        kind,
        role,
        profile: role_name(role).to_owned(),
        state: JobState::Pending,
        priority: 50,
        needs: needs
            .iter()
            .map(|need| JobId::parse(need).expect("test need id is valid"))
            .collect(),
        owns: owns.iter().map(|pattern| (*pattern).to_owned()).collect(),
        reads: vec!["docs/design/**".to_owned()],
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

/// Returns the role's profile name.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Planner => "planner",
        Role::Keeper => "keeper",
        Role::Worker => "worker",
        Role::Verifier => "verifier",
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

/// The plan form of a job, as import should return it.
fn new_job(job: &Job) -> NewJob {
    NewJob {
        id: job.id.clone(),
        title: job.title.clone(),
        kind: job.kind,
        role: job.role,
        profile: Some(job.profile.clone()),
        needs: job.needs.clone(),
        owns: job.owns.clone(),
        reads: job.reads.clone(),
        gates: Some(job.gates.clone()),
        priority: Some(job.priority),
        loop_: Some(job.loop_.clone()),
        brief_text: None,
    }
}

/// Returns the stable edge rank used by export.
fn edge_rank(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Needs => 0,
        EdgeKind::Verifies => 1,
        EdgeKind::Fixes => 2,
        EdgeKind::Escalates => 3,
        EdgeKind::Blocks => 4,
    }
}

/// Builds the graph used by the round-trip test.
fn roundtrip_graph() -> Graph {
    let mut a = job(
        "plan-a",
        JobKind::Build,
        Role::Worker,
        &[],
        &["crates/a/**", "docs/a.md"],
    );
    a.title = "quoted: title / with spaces".to_owned();
    a.priority = 70;
    a.gates = vec![
        Gate::Report(ReportGate::default()),
        Gate::Cmd(CmdGate {
            run: "cargo test -p a -- --nocapture \"quoted\"".to_owned(),
            expect_exit: 0,
            expect_pattern: Some("^ok: [a-z]+$".to_owned()),
            timeout_secs: 1800,
            cwd: Some("sub dir".to_owned()),
        }),
        Gate::OwnsRespected(true),
        Gate::Pushed(PushedGate {
            remote: "origin".to_owned(),
            branch: None,
        }),
        Gate::Review(ReviewGate::default()),
        Gate::SecretScan(true),
    ];
    a.loop_.max_attempts = 5;

    let mut b = job("plan-b", JobKind::Verify, Role::Verifier, &["plan-a"], &[]);
    b.gates = vec![Gate::Report(ReportGate::default())];

    let mut fix = job(
        "plan-a-fix",
        JobKind::Fix,
        Role::Worker,
        &[],
        &["crates/a/**", "docs/a.md"],
    );
    fix.gates = vec![Gate::Report(ReportGate::default()), Gate::OwnsRespected(true)];

    let edges = vec![
        edge("plan-b", "plan-a", EdgeKind::Verifies),
        edge("plan-a-fix", "plan-a", EdgeKind::Fixes),
    ];
    Graph::new(vec![a, b, fix], edges).expect("round-trip graph is valid")
}

#[test]
fn yaml_roundtrip_preserves_jobs_and_edges() {
    let graph = roundtrip_graph();
    let yaml = export_yaml(&graph).expect("export succeeds");

    assert!(yaml.starts_with("version: 1\n"));
    assert!(yaml.contains("jobs:\n"));
    assert!(yaml.contains("    loop: {until: gates, max_attempts: 5, on_exhausted: escalate}\n"));

    let imported = import_yaml(&yaml).expect("import succeeds");

    let expected_jobs: Vec<NewJob> = graph.jobs().map(new_job).collect();
    assert_eq!(imported.jobs, expected_jobs);

    let mut expected_edges: Vec<Edge> = graph
        .edges()
        .iter()
        .filter(|edge| edge.kind != EdgeKind::Needs)
        .cloned()
        .collect();
    expected_edges.sort_by(|a, b| {
        edge_rank(a.kind)
            .cmp(&edge_rank(b.kind))
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });
    assert_eq!(imported.edges, expected_edges);
}

#[test]
fn yaml_import_rejects_bad_gate_and_cycle() {
    let bad_gate = r#"
version: 1
jobs:
  - id: bad-gate
    title: bad gate
    kind: build
    role: worker
    gates:
      - cmd: {run: "   ", expect_exit: 0}
"#;
    match import_yaml(bad_gate) {
        Err(GraphError::Protocol(_)) => {}
        other => panic!("expected a protocol gate error, got {other:?}"),
    }

    let cycle = r#"
version: 1
jobs:
  - id: a
    title: a
    kind: build
    role: worker
    needs: [b]
  - id: b
    title: b
    kind: build
    role: worker
    needs: [a]
"#;
    match import_yaml(cycle) {
        Err(GraphError::Cycle(path)) => {
            assert_eq!(path.first(), path.last());
            assert!(path.contains(&JobId::parse("a").expect("test id is valid")));
            assert!(path.contains(&JobId::parse("b").expect("test id is valid")));
        }
        other => panic!("expected a cycle error, got {other:?}"),
    }
}

#[test]
fn yaml_export_excludes_runtime_fields() {
    let mut job = job("runtime", JobKind::Build, Role::Worker, &[], &["crates/runtime/**"]);
    job.state = JobState::Running;
    job.branch = Some("job/secret-branch".to_owned());
    job.worktree = Some("/tmp/secret-worktree".to_owned());
    job.attempts = 2;
    job.continues = 1;
    job.hang_restarts = 1;
    job.parked_reason = Some(ParkedReason::Auth);
    job.note = "private note".to_owned();
    job.last_progress_at = Some(at(1_700_000_100));
    job.state_changed_at = at(1_700_000_100);
    job.updated_at = at(1_700_000_100);

    let graph = Graph::new(vec![job], Vec::new()).expect("graph is valid");
    let yaml = export_yaml(&graph).expect("export succeeds");

    for key in [
        "state",
        "branch",
        "worktree",
        "attempts",
        "continues",
        "hang_restarts",
        "parked_reason",
        "note",
        "created_at",
        "updated_at",
        "state_changed_at",
        "last_progress_at",
    ] {
        assert!(
            !yaml
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{key}:"))),
            "runtime field {key} leaked into:\n{yaml}"
        );
    }
    assert!(!yaml.contains("job/secret-branch"));
    assert!(!yaml.contains("/tmp/secret-worktree"));
    assert!(!yaml.contains("private note"));

    let imported = import_yaml(&yaml).expect("export is importable");
    assert_eq!(imported.jobs.len(), 1);
    assert_eq!(imported.jobs[0].id.as_str(), "runtime");
    assert_eq!(imported.jobs[0].profile.as_deref(), Some("worker"));
}
