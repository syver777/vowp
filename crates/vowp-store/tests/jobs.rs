//! Job and edge repository tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vowp_protocol::{
    Clock, Edge, EdgeKind, FakeClock, JobId, JobKind, JobState, NewJob, ParkedReason, Project, ProjectName, Role,
    Timestamp, default_gates,
};
use vowp_store::{JobFilter, NodePaths, StateChange, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";

/// A file-backed store with a clock the test controls and one project.
struct TestStore {
    _tmp: TempDir,
    store: Store,
    clock: FakeClock,
    project: ProjectName,
}

impl TestStore {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let paths = NodePaths::new(tmp.path());
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
        Self {
            _tmp: tmp,
            store,
            clock,
            project,
        }
    }

    fn insert(&self, job: NewJob) -> vowp_protocol::Job {
        self.store.insert_job(&self.project, &job, "human").expect("insert job")
    }
}

/// Builds a minimal job.
fn new_job(id: &str, kind: JobKind, role: Role) -> NewJob {
    NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("job {id}"),
        kind,
        role,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
    }
}

#[test]
fn job_insert_defaults_and_needs_edges() {
    let fixture = TestStore::new();
    let base = fixture.insert(new_job("base-job", JobKind::Spec, Role::Planner));

    let mut dependent = new_job("dep-job", JobKind::Build, Role::Worker);
    dependent.needs = vec![base.id.clone()];
    let job = fixture.insert(dependent);

    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.priority, 50);
    assert_eq!(job.profile, "worker");
    assert_eq!(job.brief, ".vowp/jobs/dep-job/brief.md");
    assert_eq!(job.report, ".vowp/jobs/dep-job/report.md");
    assert_eq!(job.gates, default_gates(JobKind::Build));
    assert_eq!(job.loop_, vowp_protocol::JobLoop::default());
    assert_eq!(job.created_by, "human");
    assert_eq!(job.needs, vec![base.id.clone()]);
    assert_eq!(fixture.store.needs_of(&job.id).expect("needs"), vec![base.id]);
    assert_eq!(job.attempts, 0);
    assert_eq!(job.continues, 0);
    assert_eq!(job.hang_restarts, 0);
    assert_eq!(job.created_at, fixture.clock.now());
    assert_eq!(job.state_changed_at, fixture.clock.now());
}

#[test]
fn job_insert_duplicate_and_unknown_need() {
    let fixture = TestStore::new();
    fixture.insert(new_job("base-job", JobKind::Build, Role::Worker));

    let duplicate = new_job("base-job", JobKind::Docs, Role::Worker);
    let error = fixture
        .store
        .insert_job(&fixture.project, &duplicate, "human")
        .expect_err("duplicate id");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");

    let mut orphan = new_job("orphan-job", JobKind::Build, Role::Worker);
    orphan.needs = vec![JobId::parse("ghost-job").expect("ghost id")];
    let error = fixture
        .store
        .insert_job(&fixture.project, &orphan, "human")
        .expect_err("unknown need");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );

    let missing_project = ProjectName::parse("nope").expect("project");
    let error = fixture
        .store
        .insert_job(
            &missing_project,
            &new_job("fresh-job", JobKind::Build, Role::Worker),
            "human",
        )
        .expect_err("unknown project");
    assert!(
        matches!(error, vowp_store::StoreError::WrongProject { .. }),
        "got {error:?}"
    );

    assert!(
        fixture.store.job(&orphan.id).expect("lookup").is_none(),
        "no partial row"
    );
    assert!(
        fixture
            .store
            .job(&JobId::parse("fresh-job").expect("id"))
            .expect("lookup")
            .is_none()
    );
    assert_eq!(fixture.store.list_jobs(&JobFilter::default()).expect("list").len(), 1);
}

#[test]
fn job_state_roundtrip_all_states() {
    let fixture = TestStore::new();
    let job = fixture.insert(new_job("state-job", JobKind::Build, Role::Worker));

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

    let mut previous = JobState::Pending;
    for state in states {
        fixture.clock.advance(Duration::from_secs(1));
        let change = StateChange {
            note: Some(format!("moved to {}", state.as_str())),
            parked_reason: Some(ParkedReason::Credits),
        };
        let old = fixture.store.set_job_state(&job.id, state, &change).expect("set state");
        assert_eq!(old, previous, "returned previous state for {state:?}");
        previous = state;

        let stored = fixture.store.require_job(&job.id).expect("reload job");
        assert_eq!(stored.state, state);
        assert_eq!(stored.state_changed_at, fixture.clock.now());
        assert_eq!(stored.updated_at, fixture.clock.now());
        assert_eq!(stored.note, format!("moved to {}", state.as_str()));
        if state == JobState::Parked {
            assert_eq!(stored.parked_reason, Some(ParkedReason::Credits));
        } else {
            assert_eq!(stored.parked_reason, None, "parked reason must clear for {state:?}");
        }
    }

    // A change without a note leaves the existing note alone.
    fixture.clock.advance(Duration::from_secs(1));
    fixture
        .store
        .set_job_state(
            &job.id,
            JobState::Pending,
            &StateChange {
                note: None,
                parked_reason: Some(ParkedReason::Manual),
            },
        )
        .expect("set state");
    let stored = fixture.store.require_job(&job.id).expect("reload job");
    assert_eq!(stored.note, "moved to cancelled");
    assert_eq!(stored.parked_reason, None);
}

#[test]
fn list_jobs_filter_and_order() {
    let fixture = TestStore::new();
    let mut a = new_job("a-worker", JobKind::Build, Role::Worker);
    a.priority = Some(10);
    let a = fixture.insert(a);
    fixture.clock.advance(Duration::from_secs(1));
    let mut b = new_job("b-verifier", JobKind::Verify, Role::Verifier);
    b.priority = Some(90);
    let b = fixture.insert(b);
    fixture.clock.advance(Duration::from_secs(1));
    let mut c = new_job("c-worker", JobKind::Build, Role::Worker);
    c.priority = Some(90);
    let c = fixture.insert(c);
    fixture
        .store
        .set_job_state(
            &c.id,
            JobState::Done,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("mark done");
    fixture.clock.advance(Duration::from_secs(1));
    let mut d = new_job("d-docs", JobKind::Docs, Role::Worker);
    d.priority = Some(50);
    let d = fixture.insert(d);

    let ids = |filter: &JobFilter| -> Vec<String> {
        fixture
            .store
            .list_jobs(filter)
            .expect("list jobs")
            .into_iter()
            .map(|job| job.id.to_string())
            .collect()
    };

    assert_eq!(
        ids(&JobFilter::default()),
        vec!["b-verifier", "c-worker", "d-docs", "a-worker"],
        "priority descending, then creation time"
    );

    let pending = JobFilter {
        states: vec![JobState::Pending],
        ..JobFilter::default()
    };
    assert_eq!(ids(&pending), vec!["b-verifier", "d-docs", "a-worker"]);

    let verifier = JobFilter {
        role: Some(Role::Verifier),
        ..JobFilter::default()
    };
    assert_eq!(ids(&verifier), vec![b.id.to_string()]);

    let docs = JobFilter {
        kind: Some(JobKind::Docs),
        ..JobFilter::default()
    };
    assert_eq!(ids(&docs), vec![d.id.to_string()]);

    let prefix = JobFilter {
        id_prefix: Some("c-".to_owned()),
        ..JobFilter::default()
    };
    assert_eq!(ids(&prefix), vec![c.id.to_string()]);

    let limited = JobFilter {
        limit: Some(2),
        ..JobFilter::default()
    };
    assert_eq!(ids(&limited), vec!["b-verifier", "c-worker"]);

    let other_project = JobFilter {
        project: Some(ProjectName::parse("other").expect("project")),
        ..JobFilter::default()
    };
    let error = fixture.store.list_jobs(&other_project).expect_err("wrong project");
    assert!(
        matches!(error, vowp_store::StoreError::WrongProject { .. }),
        "got {error:?}"
    );

    let this_project = JobFilter {
        project: Some(fixture.project.clone()),
        ..JobFilter::default()
    };
    assert_eq!(ids(&this_project).len(), 4);

    let combined = JobFilter {
        states: vec![JobState::Pending],
        role: Some(Role::Worker),
        limit: Some(1),
        ..JobFilter::default()
    };
    assert_eq!(ids(&combined), vec![d.id.to_string()]);
    assert_eq!(a.id.to_string(), "a-worker");
}

#[test]
fn edges_add_remove_and_queries() {
    let fixture = TestStore::new();
    let a = fixture.insert(new_job("edge-a", JobKind::Build, Role::Worker));
    let b = fixture.insert(new_job("edge-b", JobKind::Build, Role::Worker));
    let c = fixture.insert(new_job("edge-c", JobKind::Build, Role::Worker));

    let needs = Edge {
        src: a.id.clone(),
        dst: b.id.clone(),
        kind: EdgeKind::Needs,
    };
    let verifies = Edge {
        src: a.id.clone(),
        dst: c.id.clone(),
        kind: EdgeKind::Verifies,
    };
    fixture.store.add_edge(&needs).expect("add needs");
    fixture.store.add_edge(&verifies).expect("add verifies");
    fixture.store.add_edge(&needs).expect("add is idempotent");

    assert_eq!(fixture.store.edges_of(&a.id).expect("edges of a").len(), 2);
    assert_eq!(fixture.store.needs_of(&a.id).expect("needs of a"), vec![b.id.clone()]);
    assert_eq!(
        fixture.store.dependents_of(&b.id).expect("dependents of b"),
        vec![a.id.clone()]
    );
    assert_eq!(fixture.store.edges_of(&b.id).expect("edges of b"), vec![needs.clone()]);

    let project = Some(&fixture.project);
    let mut listed = fixture.store.list_edges(project).expect("list edges");
    listed.sort_by(|left, right| left.dst.cmp(&right.dst));
    assert_eq!(listed, vec![needs.clone(), verifies.clone()]);
    let other = ProjectName::parse("other").expect("project");
    let error = fixture.store.list_edges(Some(&other)).expect_err("wrong project");
    assert!(
        matches!(error, vowp_store::StoreError::WrongProject { .. }),
        "got {error:?}"
    );

    let self_edge = Edge {
        src: a.id.clone(),
        dst: a.id.clone(),
        kind: EdgeKind::Needs,
    };
    let error = fixture.store.add_edge(&self_edge).expect_err("self edge");
    assert!(
        matches!(error, vowp_store::StoreError::InvalidInput(_)),
        "got {error:?}"
    );

    let unknown = Edge {
        src: a.id.clone(),
        dst: JobId::parse("ghost-job").expect("id"),
        kind: EdgeKind::Needs,
    };
    let error = fixture.store.add_edge(&unknown).expect_err("unknown destination");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );

    assert!(fixture.store.remove_edge(&needs).expect("remove needs"));
    assert!(!fixture.store.remove_edge(&needs).expect("remove again"));

    fixture.store.add_edge(&needs).expect("re-add needs");
    fixture.store.delete_job(&b.id).expect("delete pending b");
    assert!(fixture.store.needs_of(&a.id).expect("needs of a").is_empty());
    assert!(fixture.store.edges_of(&b.id).expect("edges of b").is_empty());
}

#[test]
fn delete_job_only_when_unstarted() {
    let fixture = TestStore::new();
    let job = fixture.insert(new_job("running-job", JobKind::Build, Role::Worker));
    fixture
        .store
        .set_job_state(
            &job.id,
            JobState::Running,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("start job");
    let error = fixture
        .store
        .delete_job(&job.id)
        .expect_err("running job cannot be deleted");
    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");

    fixture
        .store
        .set_job_state(
            &job.id,
            JobState::Pending,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("reset job");
    fixture.store.delete_job(&job.id).expect("pending job can be deleted");
    assert!(fixture.store.job(&job.id).expect("lookup").is_none());

    for state in [JobState::Cancelled, JobState::Replanned] {
        let job = fixture.insert(new_job(
            if state == JobState::Cancelled {
                "cancelled-job"
            } else {
                "replanned-job"
            },
            JobKind::Build,
            Role::Worker,
        ));
        fixture
            .store
            .set_job_state(
                &job.id,
                state,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("set terminal state");
        fixture.store.delete_job(&job.id).expect("terminal job can be deleted");
    }

    let missing = JobId::parse("missing-job").expect("id");
    let error = fixture.store.delete_job(&missing).expect_err("missing job");
    assert!(
        matches!(error, vowp_store::StoreError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn count_jobs_by_state_groups_and_filters() {
    let fixture = TestStore::new();
    fixture.insert(new_job("pending-a", JobKind::Build, Role::Worker));
    fixture.insert(new_job("pending-b", JobKind::Build, Role::Worker));
    let done = fixture.insert(new_job("done-job", JobKind::Build, Role::Worker));
    fixture
        .store
        .set_job_state(
            &done.id,
            JobState::Done,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("mark done");

    let counts = fixture.store.count_jobs_by_state(None).expect("state counts");
    assert_eq!(counts.get("pending"), Some(&2));
    assert_eq!(counts.get("done"), Some(&1));
    assert_eq!(counts.len(), 2);

    let project = fixture
        .store
        .count_jobs_by_state(Some(&fixture.project))
        .expect("project counts");
    assert_eq!(project, counts);

    let other = ProjectName::parse("other").expect("project");
    let error = fixture
        .store
        .count_jobs_by_state(Some(&other))
        .expect_err("wrong project");
    assert!(
        matches!(error, vowp_store::StoreError::WrongProject { .. }),
        "got {error:?}"
    );
}

#[test]
fn nested_transaction_rolls_back_on_error() {
    let fixture = TestStore::new();
    let outer = JobId::parse("outer-job").expect("id");
    let inner = JobId::parse("inner-job").expect("id");

    let error = fixture
        .store
        .transaction(|tx| -> Result<(), vowp_store::StoreError> {
            tx.insert_job(
                &fixture.project,
                &new_job("outer-job", JobKind::Build, Role::Worker),
                "human",
            )?;
            let inner_result = tx.transaction(|nested| -> Result<(), vowp_store::StoreError> {
                nested.insert_job(
                    &fixture.project,
                    &new_job("inner-job", JobKind::Build, Role::Worker),
                    "human",
                )?;
                Err(vowp_store::StoreError::Conflict("inner rollback".to_owned()))
            });
            assert!(inner_result.is_err());
            assert!(
                tx.job(&inner).expect("lookup inner").is_none(),
                "inner savepoint rolled back"
            );
            assert!(
                tx.job(&outer).expect("lookup outer").is_some(),
                "outer work still visible"
            );
            Err(vowp_store::StoreError::Conflict("outer rollback".to_owned()))
        })
        .expect_err("outer transaction must fail");

    assert!(matches!(error, vowp_store::StoreError::Conflict(_)), "got {error:?}");
    assert!(
        fixture.store.job(&outer).expect("lookup outer").is_none(),
        "outer rolled back"
    );
    assert!(
        fixture.store.job(&inner).expect("lookup inner").is_none(),
        "inner rolled back"
    );
    assert_eq!(fixture.store.list_jobs(&JobFilter::default()).expect("list").len(), 0);
}
