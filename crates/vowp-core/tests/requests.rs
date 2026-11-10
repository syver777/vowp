//! CLI-to-supervisor request tests.
//! Spec: `docs/design/crates/vowp-core.md` §2, tests named in that section.

mod support;

use std::sync::Arc;

use serde_json::json;
use support::fake_git::FakeGit;
use tempfile::tempdir;
use vowp_core::deps::{AdapterRegistry, FixedRng, SystemRunner};
use vowp_core::requests::{Request, apply_direct, drain, push};
use vowp_core::{CoreError, Deps};
use vowp_protocol::{
    ActiveState, Clock, FakeClock, InboxId, InboxItem, InboxKind, InboxState, Job, JobId, JobKind, JobState,
    LedgerKind, Loop, LoopBounds, LoopKind, MemoryLog, NewJob, NodeName, ParkedReason, Project, ProjectName, Role,
    Timestamp, VowpConfig,
};
use vowp_store::{LedgerFilter, NodePaths, StateChange, Store, StoreError};
use vowp_tmux::fake::FakeWindows;

/// Fixed instant every test clock starts at.
fn now() -> Timestamp {
    Timestamp::parse("2026-11-08T19:00:00Z").unwrap()
}

/// Opens a hermetic store whose clock is fixed at [`now`].
fn test_store(root: &std::path::Path) -> Store {
    Store::open(&NodePaths::new(root))
        .unwrap()
        .with_clock(Arc::new(FakeClock::at(now())))
}

/// Builds hermetic dependencies sharing `clock` and `log`.
fn test_deps(root: &std::path::Path, clock: Arc<dyn Clock>, log: MemoryLog) -> Deps {
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(VowpConfig::with_defaults_for_tests()),
        node: NodeName::parse("local").unwrap(),
        clock,
        log: Arc::new(log),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

/// Inserts a project row so jobs can reference it.
fn insert_project(store: &Store) -> ProjectName {
    let project = ProjectName::parse("test").unwrap();
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: "/tmp/test-project".to_owned(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: json!({}),
            created_at: now(),
            updated_at: now(),
        })
        .unwrap();
    project
}

/// Inserts one pending job and returns it.
fn insert_job(store: &Store, project: &ProjectName, id: &str) -> Job {
    let new = NewJob {
        id: JobId::parse(id).unwrap(),
        title: format!("job {id}"),
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
    };
    store.insert_job(project, &new, "test").unwrap()
}

/// Forces a job into `state` without going through the state machine.
fn force_state(store: &Store, job: &Job, state: JobState) {
    store
        .set_job_state(
            &job.id,
            state,
            &StateChange {
                note: None,
                parked_reason: if state == JobState::Parked {
                    Some(ParkedReason::Manual)
                } else {
                    None
                },
            },
        )
        .unwrap();
}

#[test]
fn request_wire_form_matches_loops_md_for_every_kind() {
    let inbox = InboxId::new();
    let cases = vec![
        (
            Request::ResolveJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "resolve_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::RunLoop {
                name: "l4-planner".to_owned(),
            },
            json!({"kind": "run_loop", "payload": {"name": "l4-planner"}}),
        ),
        (
            Request::RetryJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "retry_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::ParkJob {
                job: JobId::parse("n04-graph").unwrap(),
                reason: "owner asked".to_owned(),
            },
            json!({"kind": "park_job", "payload": {"job": "n04-graph", "reason": "owner asked"}}),
        ),
        (
            Request::CancelJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "cancel_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::NudgeJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "nudge_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::RestartJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "restart_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::ReplanJob {
                job: JobId::parse("n04-graph").unwrap(),
            },
            json!({"kind": "replan_job", "payload": {"job": "n04-graph"}}),
        ),
        (
            Request::FailJob {
                job: JobId::parse("n04-graph").unwrap(),
                reason: "stuck".to_owned(),
            },
            json!({"kind": "fail_job", "payload": {"job": "n04-graph", "reason": "stuck"}}),
        ),
        (
            Request::AnswerInbox { id: inbox.clone() },
            json!({"kind": "answer_inbox", "payload": {"id": inbox.as_str()}}),
        ),
        (
            Request::PauseLoop {
                name: "l4-planner".to_owned(),
            },
            json!({"kind": "pause_loop", "payload": {"name": "l4-planner"}}),
        ),
        (
            Request::ResumeLoop {
                name: "l4-planner".to_owned(),
            },
            json!({"kind": "resume_loop", "payload": {"name": "l4-planner"}}),
        ),
        (Request::Wake, json!({"kind": "wake"})),
    ];

    let mut kinds = Vec::new();
    for (request, expected) in &cases {
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(&value, expected, "wire form for {request:?}");
        kinds.push(value["kind"].as_str().unwrap().to_owned());
        let decoded: Request = serde_json::from_value(value).unwrap();
        assert_eq!(&decoded, request);
    }
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            "answer_inbox",
            "cancel_job",
            "fail_job",
            "nudge_job",
            "park_job",
            "pause_loop",
            "replan_job",
            "resolve_job",
            "restart_job",
            "resume_loop",
            "retry_job",
            "run_loop",
            "wake",
        ],
        "every kind from loops.md §1 must be covered"
    );
}

#[test]
fn request_push_drain_roundtrip_via_store() {
    let tmp = tempdir().unwrap();
    let store = test_store(tmp.path());
    let inbox = InboxId::new();
    let requests = vec![
        Request::ResolveJob {
            job: JobId::parse("a-job").unwrap(),
        },
        Request::RunLoop {
            name: "l4-planner".to_owned(),
        },
        Request::RetryJob {
            job: JobId::parse("b-job").unwrap(),
        },
        Request::ParkJob {
            job: JobId::parse("c-job").unwrap(),
            reason: "manual".to_owned(),
        },
        Request::CancelJob {
            job: JobId::parse("d-job").unwrap(),
        },
        Request::NudgeJob {
            job: JobId::parse("e-job").unwrap(),
        },
        Request::RestartJob {
            job: JobId::parse("f-job").unwrap(),
        },
        Request::ReplanJob {
            job: JobId::parse("g-job").unwrap(),
        },
        Request::FailJob {
            job: JobId::parse("h-job").unwrap(),
            reason: "stuck".to_owned(),
        },
        Request::AnswerInbox { id: inbox },
        Request::PauseLoop {
            name: "l2-worker".to_owned(),
        },
        Request::ResumeLoop {
            name: "l2-worker".to_owned(),
        },
        Request::Wake,
    ];

    let mut pushed = Vec::new();
    for request in &requests {
        let id = push(&store, request).unwrap();
        pushed.push((id, request.clone()));
    }
    assert_eq!(store.kv_prefix("req:").unwrap().len(), requests.len());

    let drained = drain(&store).unwrap();
    assert_eq!(drained.len(), requests.len());
    for ((drained_id, drained_request), (pushed_id, pushed_request)) in drained.iter().zip(&pushed) {
        assert_eq!(drained_id, pushed_id);
        assert_eq!(drained_request, pushed_request);
    }
    assert!(store.kv_prefix("req:").unwrap().is_empty());
    assert!(drain(&store).unwrap().is_empty());
}

#[test]
fn undecodable_request_is_dropped_and_logged() {
    let tmp = tempdir().unwrap();
    let store = test_store(tmp.path());
    store.push_request("bogus_kind", &json!({"anything": true})).unwrap();
    store.push_request("retry_job", &json!({"wrong": "shape"})).unwrap();
    push(&store, &Request::Wake).unwrap();

    let drained = drain(&store).unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, Request::Wake);
    assert!(store.kv_prefix("req:").unwrap().is_empty());

    let notes: Vec<_> = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            ..LedgerFilter::default()
        })
        .unwrap();
    assert_eq!(notes.len(), 2, "each dropped row must leave a note");
    assert!(
        notes
            .iter()
            .all(|note| note.summary.contains("dropped undecodable request"))
    );
    assert!(notes.iter().any(|note| note.summary.contains("bogus_kind")));
    assert!(notes.iter().any(|note| note.summary.contains("retry_job")));
}

#[test]
fn apply_direct_retry_resolve_and_park() {
    let tmp = tempdir().unwrap();
    let store = test_store(tmp.path());
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(now()));
    let log = MemoryLog::new();
    let deps = test_deps(tmp.path(), clock, log.clone());
    let project = insert_project(&store);

    // Retry a failed job: ready, counters reset.
    let retry = insert_job(&store, &project, "retry-me");
    force_state(&store, &retry, JobState::Failed);
    store.set_job_counters(&retry.id, Some(2), Some(1), Some(1)).unwrap();
    apply_direct(&deps, &store, &Request::RetryJob { job: retry.id.clone() }).unwrap();
    let retried = store.require_job(&retry.id).unwrap();
    assert_eq!(retried.state, JobState::Ready);
    assert_eq!((retried.attempts, retried.continues, retried.hang_restarts), (0, 0, 0));

    // Resolve a parked job: ready with the parked reason cleared.
    let resolve = insert_job(&store, &project, "resolve-me");
    force_state(&store, &resolve, JobState::Parked);
    apply_direct(
        &deps,
        &store,
        &Request::ResolveJob {
            job: resolve.id.clone(),
        },
    )
    .unwrap();
    let resolved = store.require_job(&resolve.id).unwrap();
    assert_eq!(resolved.state, JobState::Ready);
    assert_eq!(resolved.parked_reason, None);

    // Park a ready job: parked with the manual reason and the request reason as note.
    let park = insert_job(&store, &project, "park-me");
    apply_direct(
        &deps,
        &store,
        &Request::ParkJob {
            job: park.id.clone(),
            reason: "owner asked".to_owned(),
        },
    )
    .unwrap();
    let parked = store.require_job(&park.id).unwrap();
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(parked.parked_reason, Some(ParkedReason::Manual));
    assert_eq!(parked.note, "owner asked");
    assert!(log.contains("applied park_job"));
}

#[test]
fn apply_direct_writes_state_ledger_row() {
    let tmp = tempdir().unwrap();
    let store = test_store(tmp.path());
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(now()));
    let deps = test_deps(tmp.path(), clock, MemoryLog::new());
    let project = insert_project(&store);

    // Resolving a parked job writes one state row for the transition.
    let resolve = insert_job(&store, &project, "resolve-ledger");
    force_state(&store, &resolve, JobState::Parked);
    apply_direct(
        &deps,
        &store,
        &Request::ResolveJob {
            job: resolve.id.clone(),
        },
    )
    .unwrap();

    let rows = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::State),
            job_id: Some(resolve.id.clone()),
            ..LedgerFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1, "one transition must write exactly one state row");
    let row = &rows[0];
    assert_eq!(row.kind, LedgerKind::State);
    assert_eq!(row.ts, now());
    assert_eq!(row.project.as_ref(), Some(&project));
    assert_eq!(row.job_id.as_ref(), Some(&resolve.id));
    assert_eq!(row.summary, "resolve-ledger: parked -> ready (Resolved)");
    assert_eq!(row.detail, json!({"from": "parked", "to": "ready", "why": "Resolved"}));

    // Retry is a second transition and carries its own event name.
    let retry = insert_job(&store, &project, "retry-ledger");
    force_state(&store, &retry, JobState::Failed);
    apply_direct(&deps, &store, &Request::RetryJob { job: retry.id.clone() }).unwrap();
    let rows = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::State),
            job_id: Some(retry.id.clone()),
            ..LedgerFilter::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary, "retry-ledger: failed -> ready (Retry)");
    assert_eq!(rows[0].detail, json!({"from": "failed", "to": "ready", "why": "Retry"}));
}

#[test]
fn apply_direct_handles_every_other_kind() {
    let tmp = tempdir().unwrap();
    let store = test_store(tmp.path());
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(now()));
    let log = MemoryLog::new();
    let deps = test_deps(tmp.path(), clock, log.clone());
    let project = insert_project(&store);

    let nudge = insert_job(&store, &project, "nudge-me");
    force_state(&store, &nudge, JobState::Stalled);
    apply_direct(&deps, &store, &Request::NudgeJob { job: nudge.id.clone() }).unwrap();
    assert_eq!(store.require_job(&nudge.id).unwrap().state, JobState::Running);

    let restart = insert_job(&store, &project, "restart-me");
    force_state(&store, &restart, JobState::Stalled);
    apply_direct(
        &deps,
        &store,
        &Request::RestartJob {
            job: restart.id.clone(),
        },
    )
    .unwrap();
    assert_eq!(store.require_job(&restart.id).unwrap().state, JobState::Running);

    let replan = insert_job(&store, &project, "replan-me");
    force_state(&store, &replan, JobState::Stalled);
    apply_direct(&deps, &store, &Request::ReplanJob { job: replan.id.clone() }).unwrap();
    assert_eq!(store.require_job(&replan.id).unwrap().state, JobState::Replanned);

    let fail = insert_job(&store, &project, "fail-me");
    force_state(&store, &fail, JobState::Stalled);
    apply_direct(
        &deps,
        &store,
        &Request::FailJob {
            job: fail.id.clone(),
            reason: "keeper gave up".to_owned(),
        },
    )
    .unwrap();
    let failed = store.require_job(&fail.id).unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.note, "keeper gave up");

    let cancel = insert_job(&store, &project, "cancel-me");
    apply_direct(&deps, &store, &Request::CancelJob { job: cancel.id.clone() }).unwrap();
    assert_eq!(store.require_job(&cancel.id).unwrap().state, JobState::Cancelled);

    // Answering an inbox item resolves the job parked for it.
    let answer = insert_job(&store, &project, "answer-me");
    force_state(&store, &answer, JobState::Parked);
    let inbox = InboxItem {
        id: InboxId::new(),
        project: Some(project.clone()),
        job_id: Some(answer.id.clone()),
        kind: InboxKind::Question,
        title: "Proceed?".to_owned(),
        body: "May I proceed?".to_owned(),
        options: vec!["yes".to_owned(), "no".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now(),
        answered_at: None,
    };
    store.insert_inbox(&inbox).unwrap();
    store.answer_inbox(&inbox.id, "yes", "human").unwrap();
    apply_direct(&deps, &store, &Request::AnswerInbox { id: inbox.id.clone() }).unwrap();
    assert_eq!(store.require_job(&answer.id).unwrap().state, JobState::Ready);

    // Loops: pause, resume, then make due now.
    let loop_ = Loop {
        name: "l4-planner".to_owned(),
        kind: LoopKind::L4,
        project: Some(project.clone()),
        interval_secs: 300,
        state: ActiveState::Active,
        bounds: LoopBounds::default(),
        last_run_at: None,
        next_run_at: None,
        last_status: String::new(),
        fail_streak: 0,
        updated_at: now(),
    };
    store.upsert_loop(&loop_).unwrap();
    apply_direct(
        &deps,
        &store,
        &Request::PauseLoop {
            name: "l4-planner".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(store.loop_("l4-planner").unwrap().unwrap().state, ActiveState::Paused);
    apply_direct(
        &deps,
        &store,
        &Request::ResumeLoop {
            name: "l4-planner".to_owned(),
        },
    )
    .unwrap();
    apply_direct(
        &deps,
        &store,
        &Request::RunLoop {
            name: "l4-planner".to_owned(),
        },
    )
    .unwrap();
    let due = store.loop_("l4-planner").unwrap().unwrap();
    assert_eq!(due.state, ActiveState::Active);
    assert_eq!(due.next_run_at, Some(now()));
    assert_eq!(due.fail_streak, 0);

    apply_direct(&deps, &store, &Request::Wake).unwrap();
    assert!(log.contains("applied wake"));

    // The pause guard protects L1.
    let error = apply_direct(
        &deps,
        &store,
        &Request::PauseLoop {
            name: "l1-supervisor".to_owned(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, CoreError::Invalid(_)));

    // Unknown loops are reported as not found.
    let error = apply_direct(
        &deps,
        &store,
        &Request::PauseLoop {
            name: "l9-missing".to_owned(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, CoreError::NotFound(_)));

    // Retry from a state the state machine forbids is an error, not a silent state write.
    let wrong_state = insert_job(&store, &project, "ready-retry");
    let error = apply_direct(
        &deps,
        &store,
        &Request::RetryJob {
            job: wrong_state.id.clone(),
        },
    )
    .unwrap_err();
    assert!(matches!(error, CoreError::Invalid(_)));

    // An unanswered inbox item cannot be applied.
    let unanswered = InboxItem {
        id: InboxId::new(),
        project: None,
        job_id: None,
        kind: InboxKind::Notice,
        title: "notice".to_owned(),
        body: "notice".to_owned(),
        options: Vec::new(),
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now(),
        answered_at: None,
    };
    store.insert_inbox(&unanswered).unwrap();
    let error = apply_direct(&deps, &store, &Request::AnswerInbox { id: unanswered.id }).unwrap_err();
    assert!(matches!(error, CoreError::Invalid(_)));

    // Store errors keep their kind through the CoreError wrapper.
    let missing = JobId::parse("does-not-exist").unwrap();
    let error = apply_direct(&deps, &store, &Request::CancelJob { job: missing }).unwrap_err();
    assert!(matches!(error, CoreError::Store(StoreError::NotFound { .. })));
}
