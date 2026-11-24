//! Home-side offload dispatch: queue, claim, accept/reject, resume, cancel and fencing.
//!
//! Every test uses a temporary home, a temporary project checkout row, a fake git, a fake
//! clock and an in-memory node record. No test opens `~/.vowp`, starts tmux, touches the
//! network or runs a real harness. Spec: `docs/design/v0.4/local-orchestrator.md` §6.3,
//! §7 and §9.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::fake_git::FakeGit;
use tempfile::TempDir;
use vowp_core::agent::dispatch::{
    self, ClaimedAssignment, RUN_LEASE_TTL, SPOOL_CAP_BYTES, WAITING_PREFIX, claim_for_node, enqueue_ready_jobs,
};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    AGENT_PROTOCOL, AgentFrame, AssignmentPayload, Clock, FakeClock, Job, JobId, JobKind, JobState, MemoryLog, NewJob,
    NodeName, NodeStatus, PathRef, Placement, Project, ProjectName, Role, RunRejectReason, RunResume, Surface,
    SurfaceId, SurfaceKind, SurfaceResume, Timestamp, VowpConfig,
};
use vowp_store::repo_offload::AgentAssignmentState;
use vowp_store::{NodePath, NodePaths, StateChange, Store, hash_lease_token};

const GIB: u64 = 1024 * 1024 * 1024;
const BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

fn project() -> ProjectName {
    ProjectName::parse("alpha").expect("project")
}

fn node() -> NodeName {
    NodeName::parse("mac-studio").expect("node")
}

/// The knobs one dispatch fixture can vary.
#[derive(Clone)]
struct FixtureOptions {
    placement: Placement,
    kind: JobKind,
    project_home_only: bool,
    allow_secrets: Vec<vowp_protocol::SecretName>,
    env_pass: Vec<String>,
    path_map: bool,
    online: bool,
    busy_slots: u32,
    max_slots: u32,
    remote_url: Option<String>,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            placement: Placement::Any,
            kind: JobKind::Build,
            project_home_only: false,
            allow_secrets: vec![vowp_protocol::SecretName::parse("BUILD_TOKEN").expect("secret name")],
            env_pass: vec!["BUILD_TOKEN".to_owned()],
            path_map: true,
            online: true,
            busy_slots: 0,
            max_slots: 4,
            remote_url: Some("https://user:test@example.invalid/acme/alpha.git".to_owned()),
        }
    }
}

/// One temporary home, project registry row, enrolled node and ready job.
struct Fixture {
    _tmp: TempDir,
    deps: Deps,
    clock: Arc<FakeClock>,
    store: Store,
    project: ProjectName,
    node: NodeName,
    job: Job,
}

fn fixture(options: FixtureOptions) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("node");
    let paths = NodePaths::new(&root);
    paths.ensure().expect("node paths");
    let clock = Arc::new(FakeClock::at(ts()));
    let project = project();
    let node = node();

    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = project.clone();
    config.project.path = Some(tmp.path().join("project").display().to_string());
    config.project.remote = "origin".to_owned();
    config.offload.enabled = true;
    config.offload.projects.insert(
        project.clone(),
        vowp_protocol::config::OffloadProjectConfig {
            home_only: options.project_home_only,
            allow_secrets: options.allow_secrets.clone(),
            ..vowp_protocol::config::OffloadProjectConfig::default()
        },
    );
    if !options.env_pass.is_empty() {
        let harness = vowp_protocol::HarnessConfig {
            env_pass: options.env_pass.clone(),
            ..vowp_protocol::HarnessConfig::default()
        };
        config.harnesses.insert("opencode".to_owned(), harness);
    }
    let deps = Deps {
        paths: paths.clone(),
        config: Arc::new(config),
        node: NodeName::parse("home-1").expect("home node"),
        clock: clock.clone(),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(FakeGit::new().with_head_sha(BASE_COMMIT)),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(7)),
    };
    let store = Store::open(&paths).expect("store").with_clock(clock.clone());
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: tmp.path().join("project").display().to_string(),
            remote_url: options.remote_url.clone(),
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: ts(),
            updated_at: ts(),
        })
        .expect("project row");
    store
        .insert_pending_node(&node, &["linux".to_owned()], ts())
        .expect("pending node");
    store
        .set_node_status(&node, NodeStatus::Active, "", ts())
        .expect("active node");
    if options.path_map {
        store
            .upsert_node_path(&NodePath {
                node: node.clone(),
                project: project.clone(),
                local_root: "/srv/alpha".to_owned(),
                worktree_root: "/srv/alpha-wt".to_owned(),
                allow: Vec::new(),
                updated_at: ts(),
            })
            .expect("path map");
    }
    let seen = if options.online {
        ts()
    } else {
        ts().minus(Duration::from_secs(120))
    };
    store
        .touch_node(
            &node,
            seen,
            Some("0.3.0"),
            Some(AGENT_PROTOCOL),
            &capabilities(options.busy_slots, options.max_slots),
        )
        .expect("touch node");

    let job = insert_job(&store, &project, "job-a", options.kind);
    store
        .set_job_placement(&job.id, options.placement, ts())
        .expect("job placement");
    store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready job");
    Fixture {
        _tmp: tmp,
        deps,
        clock,
        store,
        project,
        node,
        job,
    }
}

/// A names-only capability document with one capacity snapshot.
fn capabilities(busy_slots: u32, max_slots: u32) -> serde_json::Value {
    serde_json::json!({
        "os": "linux",
        "arch": "x86_64",
        "agent_version": "0.3.0",
        "harnesses": {"opencode": "1.0.0"},
        "git_version": "git version 2.47.0",
        "labels": ["linux"],
        "secret_names": ["BUILD_TOKEN"],
        "projects": ["alpha"],
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
        },
        "pending_spool_bytes": 0
    })
}

fn insert_job(store: &Store, project: &ProjectName, id: &str, kind: JobKind) -> Job {
    let new = NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("job {id}"),
        kind,
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
    store.insert_job(project, &new, "test").expect("insert job")
}

fn ready_job(fixture: &Fixture) -> Job {
    fixture.store.require_job(&fixture.job.id).expect("job")
}

/// Adds one more ready job to the fixture's project.
fn add_ready_job(fixture: &Fixture, id: &str, placement: Placement) -> Job {
    let job = insert_job(&fixture.store, &fixture.project, id, JobKind::Build);
    fixture
        .store
        .set_job_placement(&job.id, placement, ts())
        .expect("job placement");
    fixture
        .store
        .set_job_state(
            &job.id,
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready job");
    job
}

fn enqueue(fixture: &Fixture) -> dispatch::EnqueueReport {
    enqueue_ready_jobs(&fixture.deps, &fixture.store, fixture.clock.now()).expect("enqueue")
}

fn claim(fixture: &Fixture) -> Vec<ClaimedAssignment> {
    claim_for_node(&fixture.deps, &fixture.store, &fixture.node, fixture.clock.now()).expect("claim")
}

fn assign_parts(claimed: &ClaimedAssignment) -> (&AssignmentPayload, &str, Timestamp) {
    match &claimed.frame {
        AgentFrame::AssignRun {
            assignment,
            lease_token,
            lease_expires_at,
        } => (assignment, lease_token, *lease_expires_at),
        other => panic!("expected AssignRun, got {other:?}"),
    }
}

#[test]
fn dispatcher_places_an_any_job_on_an_eligible_node() {
    let fixture = fixture(FixtureOptions::default());
    let report = enqueue(&fixture);
    assert_eq!(report.enqueued.len(), 1, "{report:?}");
    let assignment = &report.enqueued[0];
    assert_eq!(assignment.node, fixture.node);
    assert_eq!(assignment.state, AgentAssignmentState::Queued);
    assert_eq!(assignment.payload.job, fixture.job.id);
    let again = enqueue(&fixture);
    assert!(
        again.enqueued.is_empty(),
        "a second pass does not queue the same job twice"
    );
    let job = ready_job(&fixture);
    assert_eq!(job.state, JobState::Ready, "the job runs only after the agent accepts");
    assert!(!job.note.starts_with(WAITING_PREFIX), "{:?}", job.note);

    let claimed = claim(&fixture);
    assert_eq!(claimed.len(), 1);
    let (payload, token, expires_at) = assign_parts(&claimed[0]);
    assert_eq!(payload.run_id, assignment.run_id);
    assert_eq!(payload.session_id, assignment.session_id);
    assert_eq!(token.len(), 52);
    assert!(expires_at > fixture.clock.now());
    let lease = fixture
        .store
        .lease(&payload.session_id)
        .expect("lease")
        .expect("run lease");
    assert_eq!(lease.token, hash_lease_token(token), "only the hash is stored");
    assert_ne!(lease.token, token);
    assert_eq!(lease.token.len(), 64);
}

#[test]
fn claim_is_bounded_by_the_fleet_remote_run_cap() {
    let fixture = fixture(FixtureOptions::default());
    add_ready_job(&fixture, "job-b", Placement::Any);
    add_ready_job(&fixture, "job-c", Placement::Any);

    // Queued rows do not occupy a run slot yet, so a later pass can queue the third job
    // even though the enqueue side stopped at the cap in the first pass.
    assert_eq!(enqueue(&fixture).enqueued.len(), 2);
    assert_eq!(enqueue(&fixture).enqueued.len(), 1);
    let claimed = claim(&fixture);
    assert_eq!(
        claimed.len(),
        2,
        "the fleet cap, not the node's four slots, bounds one heartbeat"
    );
    let rows = fixture.store.assignments_for_node(&fixture.node).expect("assignments");
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .filter(|row| row.state == AgentAssignmentState::Sent)
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.state == AgentAssignmentState::Queued)
            .count(),
        1
    );
}

#[test]
fn enqueue_counts_each_insert_against_the_fleet_cap() {
    let pinned = Placement::Node {
        node: node(),
        fallback: None,
    };
    let fixture = fixture(FixtureOptions {
        placement: pinned.clone(),
        ..FixtureOptions::default()
    });
    assert_eq!(enqueue(&fixture).enqueued.len(), 1);
    assert_eq!(claim(&fixture).len(), 1, "the first job occupies one fleet slot");
    add_ready_job(&fixture, "job-b", pinned.clone());
    add_ready_job(&fixture, "job-c", pinned);

    let report = enqueue(&fixture);
    assert_eq!(report.enqueued.len(), 1, "only one of the two fleet slots remains");
    assert_eq!(report.waiting.len(), 1, "{report:?}");
    assert_eq!(report.waiting[0].job.as_str(), "job-c");
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::NodeBusy);
    assert!(report.waiting[0].detail.contains("max_remote_runs"));
    let waiting = fixture.store.require_job(&report.waiting[0].job).expect("waiting job");
    assert_eq!(waiting.state, JobState::Ready, "a capped job stays ready");
    assert!(waiting.note.starts_with(WAITING_PREFIX), "{:?}", waiting.note);
    assert!(waiting.note.contains("max_remote_runs"), "{:?}", waiting.note);
}

#[test]
fn dispatcher_waits_when_a_pinned_node_is_offline() {
    let options = FixtureOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        online: false,
        ..FixtureOptions::default()
    };
    let fixture = fixture(options);
    let report = enqueue(&fixture);
    assert!(report.enqueued.is_empty());
    assert_eq!(report.waiting.len(), 1, "{report:?}");
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::NodeOffline);
    assert_eq!(report.waiting[0].node.as_ref(), Some(&fixture.node));
    let job = ready_job(&fixture);
    assert_eq!(job.state, JobState::Ready, "a waiting job is never failed");
    assert!(job.note.starts_with(WAITING_PREFIX), "{:?}", job.note);
    assert!(job.note.contains("node_offline"), "{:?}", job.note);
    assert!(
        fixture
            .store
            .assignments_for_job(&fixture.job.id)
            .expect("rows")
            .is_empty()
    );
}

#[test]
fn offload_refuses_a_home_only_project_and_a_denied_kind() {
    let home_only = FixtureOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        project_home_only: true,
        ..FixtureOptions::default()
    };
    let home_only_fixture = fixture(home_only);
    let report = enqueue(&home_only_fixture);
    assert!(report.enqueued.is_empty());
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::ProjectHomeOnly);
    assert!(report.waiting[0].detail.contains("home_only"));

    let denied = FixtureOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        kind: JobKind::Merge,
        ..FixtureOptions::default()
    };
    let denied_fixture = fixture(denied);
    let report = enqueue(&denied_fixture);
    assert!(report.enqueued.is_empty());
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::KindNotAllowed);
    assert!(report.waiting[0].detail.contains("merge"));
}

#[test]
fn offload_refuses_a_job_that_needs_a_home_only_secret() {
    let options = FixtureOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        env_pass: vec!["PROD_DEPLOY_KEY".to_owned()],
        ..FixtureOptions::default()
    };
    let fixture = fixture(options);
    let report = enqueue(&fixture);
    assert!(report.enqueued.is_empty());
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::HomeOnlySecret);
    assert!(report.waiting[0].detail.contains("PROD_DEPLOY_KEY"));
    assert!(
        fixture
            .store
            .assignments_for_job(&fixture.job.id)
            .expect("rows")
            .is_empty(),
        "no assignment carries a secret name the project did not allow"
    );
}

#[test]
fn offload_requires_a_valid_path_map() {
    let options = FixtureOptions {
        placement: Placement::Node {
            node: node(),
            fallback: None,
        },
        path_map: false,
        ..FixtureOptions::default()
    };
    let fixture = fixture(options);
    let report = enqueue(&fixture);
    assert!(report.enqueued.is_empty());
    assert_eq!(report.waiting[0].reason, vowp_protocol::WaitingReason::PathUnmapped);
    assert!(
        fixture
            .store
            .assignments_for_job(&fixture.job.id)
            .expect("rows")
            .is_empty()
    );
}

#[test]
fn assignment_payload_is_durable_and_has_no_lease_token() {
    let fixture = fixture(FixtureOptions::default());
    let report = enqueue(&fixture);
    let queued = &report.enqueued[0];
    let claimed = claim(&fixture);
    let (payload, token, _) = assign_parts(&claimed[0]);
    let stored = fixture
        .store
        .assignment(&queued.id)
        .expect("read assignment")
        .expect("exists");
    assert_eq!(stored.payload, *payload);
    assert_eq!(
        stored.payload.cwd_ref,
        PathRef::Worktree {
            project: fixture.project.clone(),
            job: fixture.job.id.clone(),
        }
    );
    assert_eq!(stored.payload.base_commit, BASE_COMMIT);
    assert_eq!(stored.payload.remote, "origin");
    assert_eq!(
        stored.payload.remote_url, "https://example.invalid/acme/alpha.git",
        "userinfo is stripped before the payload is stored"
    );
    assert!(!stored.payload.remote_url.contains("token"));
    assert_eq!(stored.payload.env_names, vec!["BUILD_TOKEN".to_owned()]);
    assert_eq!(stored.payload.secret_names.len(), 1);
    assert_eq!(stored.payload.spool_cap_bytes, SPOOL_CAP_BYTES);
    assert_eq!(stored.payload.epoch, 1);
    assert!(stored.payload.deadline.is_none());

    let raw = serde_json::to_string(&stored.payload).expect("payload JSON");
    assert!(!raw.contains(token), "the lease token never enters the payload");
    assert!(!raw.contains("lease_token"));
    assert!(!raw.contains("/srv/alpha"));
    assert!(!raw.contains(&fixture.deps.paths.root().display().to_string()));
    assert!(raw.contains("secret_names"));
    assert!(raw.contains("PROD_DEPLOY_KEY") || raw.contains("BUILD_TOKEN"));
}

#[test]
fn duplicate_assign_run_for_the_same_epoch_resumes_and_a_higher_epoch_supersedes() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, first_token, _) = assign_parts(&claimed[0]);
    let run = payload.run_id.clone();

    let resumed = dispatch::resume_assignment(&fixture.deps, &fixture.store, &fixture.node, &run, 1, ts())
        .expect("resume")
        .expect("live assignment resumes");
    let (resumed_payload, resumed_token, _) = assign_parts(&resumed);
    assert_eq!(resumed_payload, payload, "the durable payload is unchanged");
    assert_ne!(resumed_token, first_token, "a resume mints a fresh token");

    let higher = dispatch::supersede_assignment(&fixture.store, &run, 2, ts()).expect("supersede");
    assert_eq!(higher.epoch, 2);
    assert_eq!(higher.state, AgentAssignmentState::Queued);
    assert_eq!(higher.payload.epoch, 2);
    let old = fixture
        .store
        .assignments_for_run(&run)
        .expect("rows")
        .into_iter()
        .find(|assignment| assignment.epoch == 1)
        .expect("old epoch");
    assert_eq!(old.state, AgentAssignmentState::Lost);
    assert!(old.detail.contains("superseded by epoch 2"));

    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    assert_eq!(payload.epoch, 2, "the queued higher epoch is the one sent");

    let error =
        dispatch::supersede_assignment(&fixture.store, &run, 2, ts()).expect_err("an equal epoch does not supersede");
    assert!(error.to_string().contains("does not supersede"), "{error}");
}

#[test]
fn run_rejected_keeps_the_job_ready_with_a_reason() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    let rejected = dispatch::reject(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &claimed[0].assignment.id,
        &payload.run_id,
        1,
        RunRejectReason::PathMissing,
        "the worktree root is missing",
        ts(),
    )
    .expect("reject");
    assert_eq!(rejected.state, AgentAssignmentState::Rejected);
    let job = ready_job(&fixture);
    assert_eq!(job.state, JobState::Ready);
    assert!(job.note.starts_with(WAITING_PREFIX), "{:?}", job.note);
    assert!(job.note.contains("path_missing"), "{:?}", job.note);
    assert!(job.note.contains("the worktree root is missing"));
    let run = fixture.store.require_run(&payload.run_id).expect("run");
    assert!(run.ended_at.is_some());
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Error));
    assert!(
        fixture.store.lease(&payload.session_id).expect("lease").is_none(),
        "a rejected run releases its lease"
    );

    let retry = enqueue(&fixture);
    assert!(
        retry.enqueued.is_empty(),
        "the rejection cooldown suppresses an immediate retry"
    );
}

#[test]
fn a_reconnected_agent_re_accepts_a_disconnected_run() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    let assignment_id = claimed[0].assignment.id.clone();

    dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &assignment_id,
        &payload.run_id,
        1,
        ts(),
    )
    .expect("first accept");
    let changed = dispatch::disconnect_node(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        "the transport dropped",
        ts(),
    )
    .expect("disconnect");
    assert_eq!(changed[0].state, AgentAssignmentState::Disconnected);

    // The real agent re-sends RunAccepted for the live run after Resume. That is not a
    // transport error: the row returns to running and the job stays running.
    let reaccepted = dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &assignment_id,
        &payload.run_id,
        1,
        ts(),
    )
    .expect("re-accept after reconnect");
    assert_eq!(reaccepted.state, AgentAssignmentState::Running);
    assert_eq!(reaccepted.id, assignment_id);
    assert_eq!(ready_job(&fixture).state, JobState::Running);

    // A fenced run is not silently reopened by a late RunAccepted.
    dispatch::lose(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &payload.run_id,
        1,
        "the run lease expired",
        ts(),
    )
    .expect("fence");
    let error = dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &assignment_id,
        &payload.run_id,
        1,
        ts(),
    )
    .expect_err("a fenced assignment cannot be re-accepted");
    assert!(matches!(error, vowp_core::CoreError::Refused(_)), "{error}");
}

#[test]
fn resume_returns_the_run_and_surface_tails() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    let session = fixture.store.require_session(&payload.session_id).expect("session");
    let surface = SurfaceId::new();
    fixture
        .store
        .upsert_surface(&Surface {
            id: surface.clone(),
            kind: SurfaceKind::Opencode,
            node: fixture.node.clone(),
            holder: "laptop".to_owned(),
            app: Some("opencode".to_owned()),
            cwd: None,
            metadata: serde_json::json!({"session": payload.session_id.as_str()}),
            created_at: ts(),
            last_seen_at: ts(),
        })
        .expect("surface");

    let outcome = dispatch::resume_connection(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &[RunResume {
            run_id: payload.run_id.clone(),
            epoch: 1,
            last_acked: 1,
        }],
        &[SurfaceResume { surface, last_acked: 1 }],
        ts(),
    )
    .expect("resume");
    let AgentFrame::ResumeOk { runs, surfaces } = &outcome.resume_ok else {
        panic!("expected ResumeOk");
    };
    assert!(runs[0].accepted, "{runs:?}");
    assert_eq!(runs[0].tail_seq, session.last_seq, "the run tail is the session tail");
    assert!(surfaces[0].accepted, "{surfaces:?}");
    assert_eq!(surfaces[0].tail_seq, session.last_seq);
    assert_eq!(outcome.assignments.len(), 1, "a resume re-sends AssignRun");
}

#[test]
fn cancelled_job_sends_cancel_run_and_fences_the_assignment() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &claimed[0].assignment.id,
        &payload.run_id,
        1,
        ts(),
    )
    .expect("accept");
    let frames = dispatch::cancel_job(
        &fixture.deps,
        &fixture.store,
        &fixture.job.id,
        "owner cancelled the job",
        ts(),
    )
    .expect("cancel");
    assert_eq!(frames.len(), 1);
    let AgentFrame::CancelRun { run_id, epoch, reason } = &frames[0] else {
        panic!("expected CancelRun, got {:?}", frames[0]);
    };
    assert_eq!(run_id, &payload.run_id);
    assert_eq!(*epoch, 1);
    assert!(reason.contains("owner cancelled"));
    let assignment = fixture
        .store
        .assignment(&claimed[0].assignment.id)
        .expect("read")
        .expect("exists");
    assert_eq!(assignment.state, AgentAssignmentState::Cancelled);
    assert!(fixture.store.lease(&payload.session_id).expect("lease").is_none());

    let cleanup = dispatch::cleanup_assignment(&fixture.store, &assignment.id)
        .expect("cleanup")
        .expect("terminal assignments clean up");
    let AgentFrame::Cleanup { job, worktree, .. } = cleanup else {
        panic!("expected Cleanup");
    };
    assert_eq!(job, fixture.job.id);
    assert_eq!(
        worktree,
        PathRef::Worktree {
            project: fixture.project.clone(),
            job: fixture.job.id.clone(),
        }
    );
}

#[test]
fn run_lost_after_an_expired_lease_marks_the_assignment_lost() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    let later = ts().plus(RUN_LEASE_TTL).plus(Duration::from_secs(1));
    let lost = dispatch::sweep_expired(&fixture.deps, &fixture.store, &fixture.node, later).expect("sweep");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].state, AgentAssignmentState::Lost);
    let assignment = fixture
        .store
        .assignment(&claimed[0].assignment.id)
        .expect("read")
        .expect("exists");
    assert_eq!(assignment.state, AgentAssignmentState::Lost);
    assert!(assignment.detail.contains("expired"), "{:?}", assignment.detail);
    let run = fixture.store.require_run(&payload.run_id).expect("run");
    assert!(run.ended_at.is_some(), "a fenced run is closed");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Error));
    assert!(fixture.store.lease(&payload.session_id).expect("lease").is_none());
    let job = ready_job(&fixture);
    assert_eq!(job.state, JobState::Ready, "the job becomes ready for a retry");
    assert!(job.note.contains("run lost"), "{:?}", job.note);
}

#[test]
fn second_connection_supersedes_the_first_and_disconnects_runs() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &claimed[0].assignment.id,
        &claimed[0].assignment.payload.run_id,
        1,
        ts(),
    )
    .expect("accept");
    let changed = dispatch::disconnect_node(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        "a second connection superseded this one",
        ts(),
    )
    .expect("disconnect");
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].state, AgentAssignmentState::Disconnected);
}

#[test]
fn strip_userinfo_keeps_scp_ssh_urls_and_removes_http_credentials() {
    assert_eq!(
        dispatch::strip_userinfo("https://user:test@example.invalid/a/b.git"),
        "https://example.invalid/a/b.git"
    );
    assert_eq!(
        dispatch::strip_userinfo("ssh://git@example.com/a/b.git"),
        "ssh://example.com/a/b.git"
    );
    assert_eq!(
        dispatch::strip_userinfo("git@example.com:a/b.git"),
        "git@example.com:a/b.git"
    );
    assert_eq!(
        dispatch::strip_userinfo("https://example.invalid/a/b.git"),
        "https://example.invalid/a/b.git"
    );
}

#[test]
fn a_home_placed_job_is_never_queued() {
    let options = FixtureOptions {
        placement: Placement::Home,
        ..FixtureOptions::default()
    };
    let fixture = fixture(options);
    let report = enqueue(&fixture);
    assert!(report.enqueued.is_empty());
    assert!(report.waiting.is_empty());
    assert_eq!(report.home, 1);
    assert!(
        fixture
            .store
            .assignments_for_job(&fixture.job.id)
            .expect("rows")
            .is_empty()
    );
}

/// Reads the job owner row of `session`.
fn owner_of(fixture: &Fixture, session: &vowp_protocol::SessionId) -> Option<vowp_store::repo_surfaces::OwnerRecord> {
    fixture.store.ownership(session).expect("ownership")
}

#[test]
fn run_lease_is_job_mode_and_survives_past_sixty_seconds() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, token, expires) = assign_parts(&claimed[0]);
    let owner = owner_of(&fixture, &payload.session_id).expect("the claim writes an owner");
    assert_eq!(owner.mode, vowp_protocol::OwnershipMode::Job);
    assert_eq!(
        owner.expires_at,
        Some(expires),
        "the owner expires with the issued lease"
    );
    assert_eq!(expires, ts().plus(RUN_LEASE_TTL));

    // Two minutes of silence is far past the 60 s interactive window.
    fixture.clock.advance(Duration::from_secs(120));
    let swept = vowp_core::surface::sweep_leases(&fixture.deps, &fixture.store, fixture.clock.now()).expect("sweep");
    assert_eq!(swept, 0, "the interactive sweep ignores job rows");
    let owner = owner_of(&fixture, &payload.session_id).expect("owner survives the sweep");
    assert_eq!(
        owner.state_at(fixture.clock.now()),
        vowp_protocol::OwnershipState::Active
    );
    vowp_core::surface::check_lease_with(&fixture.store, &payload.session_id, Some(token))
        .expect("the run still writes after sixty seconds");
}

#[test]
fn node_disconnect_keeps_the_job_owner_until_the_assignment_expires() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, token, _) = assign_parts(&claimed[0]);
    dispatch::accept(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &claimed[0].assignment.id,
        &payload.run_id,
        1,
        fixture.clock.now(),
    )
    .expect("accept");
    dispatch::disconnect_node(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        "transport dropped",
        fixture.clock.now(),
    )
    .expect("disconnect");

    fixture.clock.advance(Duration::from_secs(300));
    let still = dispatch::sweep_expired(&fixture.deps, &fixture.store, &fixture.node, fixture.clock.now())
        .expect("sweep before expiry");
    assert!(still.is_empty(), "a disconnected run is not fenced before its expiry");
    let assignment = fixture
        .store
        .assignment(&claimed[0].assignment.id)
        .expect("read")
        .expect("row");
    assert_eq!(assignment.state, AgentAssignmentState::Disconnected);
    let owner = owner_of(&fixture, &payload.session_id).expect("the job owner stays");
    assert_eq!(
        owner.state_at(fixture.clock.now()),
        vowp_protocol::OwnershipState::Active
    );
    vowp_core::surface::check_lease_with(&fixture.store, &payload.session_id, Some(token))
        .expect("the reconnecting run can still write");
}

#[test]
fn expired_assignment_fences_the_run_and_releases_the_owner() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, token, _) = assign_parts(&claimed[0]);
    fixture.clock.advance(RUN_LEASE_TTL + Duration::from_secs(1));
    let owner = owner_of(&fixture, &payload.session_id).expect("expired owner row is still visible");
    assert_eq!(
        owner.state_at(fixture.clock.now()),
        vowp_protocol::OwnershipState::Expired
    );
    vowp_core::surface::check_lease_with(&fixture.store, &payload.session_id, Some(token))
        .expect_err("an expired owner is read-only even with the right token");

    let lost =
        dispatch::sweep_expired(&fixture.deps, &fixture.store, &fixture.node, fixture.clock.now()).expect("sweep");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].state, AgentAssignmentState::Lost);
    assert!(
        owner_of(&fixture, &payload.session_id).is_none(),
        "the owner is released"
    );
    let run = fixture.store.require_run(&payload.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Error));
    assert_eq!(ready_job(&fixture).state, JobState::Ready);
}

#[test]
fn an_expired_owner_fences_a_run_whose_assignment_row_was_renewed() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    // The assignment row is renewed (for example by a RunState on a connection that did not
    // see the token) but nothing renewed the owner.
    fixture.clock.advance(RUN_LEASE_TTL + Duration::from_secs(1));
    fixture
        .store
        .renew_assignment(
            &claimed[0].assignment.id,
            fixture.clock.now(),
            fixture.clock.now().plus(RUN_LEASE_TTL),
            "renewed",
        )
        .expect("renew assignment");

    let lost =
        dispatch::sweep_expired(&fixture.deps, &fixture.store, &fixture.node, fixture.clock.now()).expect("sweep");
    assert_eq!(lost.len(), 1, "the expired owner fences its run");
    assert!(owner_of(&fixture, &payload.session_id).is_none());
}

#[test]
fn a_late_lose_never_releases_an_interactive_owner() {
    let fixture = fixture(FixtureOptions::default());
    enqueue(&fixture);
    let claimed = claim(&fixture);
    let (payload, _, _) = assign_parts(&claimed[0]);
    let surface = SurfaceId::new();
    fixture
        .store
        .upsert_surface(&Surface {
            id: surface.clone(),
            kind: SurfaceKind::Opencode,
            node: fixture.node.clone(),
            holder: "laptop".to_owned(),
            app: Some("opencode".to_owned()),
            cwd: None,
            metadata: serde_json::json!({}),
            created_at: ts(),
            last_seen_at: ts(),
        })
        .expect("surface");
    fixture
        .store
        .take_lease(&vowp_protocol::Lease {
            session_id: payload.session_id.clone(),
            surface_id: Some(surface),
            surface_kind: SurfaceKind::Opencode,
            node: fixture.node.clone(),
            holder: "laptop".to_owned(),
            token: "t".repeat(52),
            acquired_at: ts(),
            heartbeat_at: ts(),
        })
        .expect("interactive lease");

    dispatch::lose(
        &fixture.deps,
        &fixture.store,
        &fixture.node,
        &payload.run_id,
        1,
        "late fence",
        ts(),
    )
    .expect("lose");
    let owner = owner_of(&fixture, &payload.session_id).expect("the interactive owner survives");
    assert_eq!(owner.mode, vowp_protocol::OwnershipMode::Interactive);
}
