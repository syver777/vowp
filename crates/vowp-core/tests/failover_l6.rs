//! L6 failover tests: rate backoff, auth/credits parking, unknown failures.
//! Spec: `docs/design/loops.md` §8 and `docs/design/crates/vowp-core.md` §7.

#[path = "failover_support.rs"]
mod f;

use std::sync::Arc;
use std::time::Duration;

use f::*;
use vowp_core::loops::l6_failover::{Failover, FailoverAction};
use vowp_protocol::{
    ActiveState, Cost, Executor, HarnessKind, InboxKind, Job, JobKind, JobState, LedgerKind, Limit, LimitKind,
    LimitRowKind, OnCreditsExhausted, ParkedReason, ProviderConfig, Role, Run, RunId, RunOutcome, RunRole, RunSummary,
    SessionId,
};
use vowp_store::LedgerFilter;

#[test]
fn rate_backoff_lowers_concurrency_cap() {
    let node = TestNode::new();
    let store = node.store();
    // Four runs are already live on the provider; a fifth reports the 429.
    for index in 0..4 {
        let mut run = run_for(
            &node,
            None,
            "deepseek",
            RunRole::Worker,
            RunOutcome::Rate,
            Some(LimitKind::Rate),
        );
        run.id = RunId::parse(&format!("01J0000000000000000000000{index}")).expect("run id");
        run.ended_at = None;
        run.outcome = None;
        run.limit_kind = None;
        run.started_at = ts();
        store.insert_run(&run).expect("open run");
    }
    let rate_run = run_for(
        &node,
        None,
        "deepseek",
        RunRole::Worker,
        RunOutcome::Rate,
        Some(LimitKind::Rate),
    );
    let summary = limit_summary(LimitKind::Rate, "deepseek", None);
    let failover = Failover::new(node.deps.clone());
    let action = failover.on_run_outcome(&store, &rate_run, &summary).expect("rate");
    let first_until = match action {
        FailoverAction::Backoff { until } => until,
        other => panic!("expected backoff, got {other:?}"),
    };
    assert_eq!(first_until, ts().plus(Duration::from_secs(600)));
    let limit = store.limit_for("deepseek").expect("limit").expect("rate row");
    assert_eq!(limit.kind, LimitRowKind::Rate);
    assert_eq!(limit.resets_at, Some(first_until));
    assert_eq!(limit.concurrency_cap, Some(3));
    assert_eq!(limit.cap_until, Some(first_until));

    // The next rate limit doubles the backoff; the cap never drops below one.
    node.advance(600);
    let second = failover
        .on_run_outcome(&store, &rate_run, &summary)
        .expect("second rate");
    let second_until = match second {
        FailoverAction::Backoff { until } => until,
        other => panic!("expected backoff, got {other:?}"),
    };
    assert_eq!(second_until, ts().plus(Duration::from_secs(600 + 1200)));
    let limit = store.limit_for("deepseek").expect("limit").expect("rate row");
    assert_eq!(limit.concurrency_cap, Some(3));
    assert_eq!(limit.cap_until, Some(second_until));
    let ledger = store
        .ledger_since(&LedgerFilter {
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert!(
        ledger
            .iter()
            .filter(|entry| entry.kind == vowp_protocol::LedgerKind::Limit)
            .count()
            >= 2
    );
}

#[test]
fn auth_parks_with_inbox() {
    let node = TestNode::new();
    let job = node.insert_job("auth-job", JobKind::Build, Role::Worker, Some(vec![]));
    let store = node.store();
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause planner");
    store
        .set_loop_state("l5-keeper", ActiveState::Paused)
        .expect("pause keeper");
    let run = run_for(
        &node,
        Some(&job),
        "deepseek",
        RunRole::Worker,
        RunOutcome::Auth,
        Some(LimitKind::Auth),
    );
    let summary = limit_summary(LimitKind::Auth, "deepseek", None);
    Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &summary)
        .expect("auth");
    assert!(
        vowp_core::loops::l4_planner::event_wake(&store, "planner").expect("wake"),
        "a limit/failover event is an unseen planner event"
    );

    supervisor.tick().expect("park tick");
    let job = store.require_job(&job.id).expect("job");
    assert_eq!(job.state, JobState::Parked);
    assert_eq!(job.parked_reason, Some(ParkedReason::Auth));
    let limit = store.limit_for("deepseek").expect("limit").expect("auth row");
    assert_eq!(limit.kind, LimitRowKind::Auth);
    assert_eq!(limit.resets_at, None);
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "one provider notice: {inbox:?}");
    assert_eq!(inbox[0].kind, InboxKind::Question);
    assert_eq!(inbox[0].title, "auth required: deepseek");

    // Resolving the notice clears the limit and returns the job to ready. Pause L2 so
    // "ready" is observable rather than dispatched again in the same tick.
    store
        .set_loop_state("l2-worker", ActiveState::Paused)
        .expect("pause l2");
    store.answer_inbox(&inbox[0].id, "resolved", "test").expect("answer");
    vowp_core::requests::push(
        &store,
        &vowp_core::requests::Request::AnswerInbox {
            id: inbox[0].id.clone(),
        },
    )
    .expect("request");
    supervisor.tick().expect("answer tick");
    assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Ready);
    assert!(store.limit_for("deepseek").expect("limit").is_none());
}

#[test]
fn three_unknown_failures_mark_unavailable_15min() {
    let node = TestNode::new();
    let store = node.store();
    let failover = Failover::new(node.deps.clone());
    let run = run_for(&node, None, "deepseek", RunRole::Worker, RunOutcome::Crash, None);
    let crash = failure_summary(RunOutcome::Crash);
    for _ in 0..2 {
        failover.on_run_outcome(&store, &run, &crash).expect("crash");
        assert!(store.limit_for("deepseek").expect("limit").is_none());
    }
    failover.on_run_outcome(&store, &run, &crash).expect("third crash");
    let limit = store.limit_for("deepseek").expect("limit").expect("unavailable row");
    assert_eq!(limit.kind, LimitRowKind::Unavailable);
    assert_eq!(limit.resets_at, Some(ts().plus(Duration::from_secs(15 * 60))));
    assert_eq!(store.kv_get("fail_streak:deepseek").expect("kv"), Some("0".to_owned()));

    // A probe is due only after the 15 minutes.
    assert!(
        !failover
            .probe_due(&store, ts().plus(Duration::from_secs(14 * 60)))
            .expect("due")
            .contains(&"deepseek".to_owned())
    );
    assert!(
        failover
            .probe_due(&store, ts().plus(Duration::from_secs(16 * 60)))
            .expect("due")
            .contains(&"deepseek".to_owned())
    );

    // An ok run resets the streak and is proof the provider works: it clears the
    // temporary row and the probe-back mark, so there is nothing left to probe.
    let ok = RunSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: None,
        stop_reason: vowp_protocol::StopReason::EndTurn,
    };
    failover.on_run_outcome(&store, &run, &ok).expect("ok");
    assert_eq!(store.kv_get("fail_streak:deepseek").expect("kv"), Some("0".to_owned()));
    assert!(
        store.limit_for("deepseek").expect("limit").is_none(),
        "an ok run clears the unavailable row"
    );
    assert_eq!(
        store.kv_get("probe_back:deepseek").expect("mark"),
        None,
        "an ok run clears the probe-back mark"
    );
    assert!(
        !failover
            .probe_due(&store, ts().plus(Duration::from_secs(16 * 60)))
            .expect("due")
            .contains(&"deepseek".to_owned())
    );
}

#[test]
fn credits_park_all_jobs_of_provider_and_inbox() {
    let node = TestNode::new();
    let first = node.insert_job("credits-a", JobKind::Build, Role::Worker, Some(vec![]));
    let second = node.insert_job("credits-b", JobKind::Build, Role::Worker, Some(vec![]));
    let store = node.store();
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause planner");
    store
        .set_loop_state("l5-keeper", ActiveState::Paused)
        .expect("pause keeper");
    let run = run_for(
        &node,
        Some(&first),
        "deepseek",
        RunRole::Worker,
        RunOutcome::Credits,
        Some(LimitKind::Credits),
    );
    let summary = limit_summary(LimitKind::Credits, "deepseek", None);
    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &summary)
        .expect("credits");
    assert!(matches!(
        action,
        FailoverAction::ParkProvider { ref provider } if provider == "deepseek"
    ));

    supervisor.tick().expect("park tick");
    for job in [&first, &second] {
        let job = store.require_job(&job.id).expect("job");
        assert_eq!(job.state, JobState::Parked);
        assert_eq!(job.parked_reason, Some(ParkedReason::Credits));
    }
    let limit = store.limit_for("deepseek").expect("limit").expect("credits row");
    assert_eq!(limit.kind, LimitRowKind::Credits);
    assert_eq!(limit.resets_at, None);
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "exactly one credits notice: {inbox:?}");
    assert_eq!(inbox[0].title, "credits exhausted: deepseek");

    store
        .set_loop_state("l2-worker", ActiveState::Paused)
        .expect("pause l2");
    store.answer_inbox(&inbox[0].id, "resolved", "test").expect("answer");
    vowp_core::requests::push(
        &store,
        &vowp_core::requests::Request::AnswerInbox {
            id: inbox[0].id.clone(),
        },
    )
    .expect("request");
    supervisor.tick().expect("resolve tick");
    for job in [&first, &second] {
        assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Ready);
    }
    assert!(store.limit_for("deepseek").expect("limit").is_none());
}

#[test]
fn credits_park_parks_verify_and_fix_child_jobs_and_leaves_waiting_parents() {
    let node = TestNode::new();
    let parent = node.insert_job("parent", JobKind::Build, Role::Worker, Some(vec![]));
    let verify = node.insert_job("parent-verify-0", JobKind::Verify, Role::Verifier, Some(vec![]));
    let fix = node.insert_job("parent-fix-1", JobKind::Fix, Role::Worker, Some(vec![]));
    node.force_state(&parent, JobState::Reviewing);
    node.force_state(&verify, JobState::Ready);
    node.force_state(&fix, JobState::Ready);
    let store = node.store();
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause planner");
    store
        .set_loop_state("l5-keeper", ActiveState::Paused)
        .expect("pause keeper");
    let run = run_for(
        &node,
        Some(&verify),
        "deepseek",
        RunRole::Verifier,
        RunOutcome::Credits,
        Some(LimitKind::Credits),
    );
    Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Credits, "deepseek", None))
        .expect("credits");

    supervisor.tick().expect("park tick");
    for child in [&verify, &fix] {
        let child = store.require_job(&child.id).expect("child");
        assert_eq!(child.state, JobState::Parked, "{}", child.id);
        assert_eq!(child.parked_reason, Some(ParkedReason::Credits));
    }
    assert_eq!(
        store.require_job(&parent.id).expect("parent").state,
        JobState::Reviewing,
        "a reviewing parent waits for its child and is not itself parked"
    );
    assert_eq!(store.list_inbox(None).expect("inbox").len(), 1);
}

#[test]
fn provider_blocked_answer_resolves_only_that_provider() {
    let node = TestNode::new();
    let blocked = node.insert_job("blocked-deepseek", JobKind::Build, Role::Worker, Some(vec![]));
    let other = node.insert_job("blocked-claude", JobKind::Build, Role::Worker, Some(vec![]));
    let store = node.store();
    let mut supervisor = node.supervisor();
    for (job, provider) in [(&blocked, "deepseek"), (&other, "claude")] {
        store
            .set_job_state(
                &job.id,
                JobState::Parked,
                &vowp_store::StateChange {
                    note: Some(format!("provider {provider} is blocked; resolve it")),
                    parked_reason: Some(ParkedReason::ProviderBlocked),
                },
            )
            .expect("park");
    }
    store
        .set_loop_state("l2-worker", ActiveState::Paused)
        .expect("pause l2");
    store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause planner");
    let inbox_id = vowp_protocol::InboxId::new();
    store
        .insert_inbox(&vowp_protocol::InboxItem {
            id: inbox_id.clone(),
            project: Some(node.project.clone()),
            job_id: None,
            kind: InboxKind::Question,
            title: "provider blocked: deepseek".to_owned(),
            body: "resolve it".to_owned(),
            options: vec!["resolved".to_owned()],
            state: vowp_protocol::InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: ts(),
            answered_at: None,
        })
        .expect("inbox");
    store.answer_inbox(&inbox_id, "resolved", "test").expect("answer");
    vowp_core::requests::push(
        &store,
        &vowp_core::requests::Request::AnswerInbox { id: inbox_id.clone() },
    )
    .expect("request");
    supervisor.tick().expect("answer tick");
    assert_eq!(store.require_job(&blocked.id).expect("job").state, JobState::Ready);
    assert_eq!(
        store.require_job(&other.id).expect("job").state,
        JobState::Parked,
        "a notice for deepseek must not resolve a claude job"
    );
}

#[test]
fn fresh_supervisor_does_not_reapply_old_credit_limit() {
    let node = TestNode::new();
    let job = node.insert_job("resolved-credit", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Ready);
    let session = create_session(&node, &job);
    let store = node.store();
    let run = Run {
        id: RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: Some(session),
        role: RunRole::Worker,
        purpose: vowp_protocol::RunPurpose::Start,
        profile: Some("worker".to_owned()),
        rung: 0,
        executor: Some(vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: node.deps.node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some(node.main_repo.to_string_lossy().into_owned()),
        started_at: ts(),
        ended_at: None,
        exit_code: Some(1),
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: "insufficient balance".to_owned(),
    };
    store.insert_run(&run).expect("run");
    store
        .finish_run(
            &run.id,
            &vowp_store::RunFinish {
                outcome: RunOutcome::Credits,
                exit_code: Some(1),
                limit_kind: Some(LimitKind::Credits),
                cost: Cost::unknown(),
                native_session_id: None,
                detail: "insufficient balance".to_owned(),
            },
        )
        .expect("finish");
    // This is the state after `vowp inbox answer <id> resolved`: no limit row, job ready.
    assert!(store.limit_for("deepseek").expect("limit").is_none());

    for _ in 0..2 {
        let mut supervisor = node.supervisor();
        store
            .set_loop_state("l2-worker", ActiveState::Paused)
            .expect("pause l2");
        store
            .set_loop_state("l4-planner", ActiveState::Paused)
            .expect("pause planner");
        store
            .set_loop_state("l5-keeper", ActiveState::Paused)
            .expect("pause keeper");
        supervisor.startup().expect("startup");
        supervisor.tick().expect("tick");
        assert_eq!(
            store.require_job(&job.id).expect("job").state,
            JobState::Ready,
            "an old harvested credits run must not re-park the resolved job"
        );
        assert!(
            store.limit_for("deepseek").expect("limit").is_none(),
            "an old harvested credits run must not recreate the limit row"
        );
    }
    assert_eq!(
        store.kv_get(&format!("l6:{}", run.id)).expect("marker").as_deref(),
        Some("harvested"),
        "the harvested marker survives a supervisor restart"
    );
}

#[test]
fn provider_of_never_run_job_uses_profile_pick() {
    let node = TestNode::with_adapter_config(
        false,
        Arc::new(ScriptAdapter::always(HarnessKind::Opencode, ScriptAction::ok("ok"))),
        |config| {
            let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude");
            let deepseek = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("deepseek");
            config.profiles.get_mut("worker").expect("worker profile").ladder = vec![claude, deepseek];
        },
    );
    let store = node.store();
    store
        .upsert_limit(&Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: LimitRowKind::Quota,
            resets_at: Some(ts().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("claude quota");
    let job = node.insert_job("never-ran", JobKind::Build, Role::Worker, Some(vec![]));
    store.kv_set("park:deepseek", "provider_blocked").expect("park request");
    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("park tick");

    let job = store.require_job(&job.id).expect("job");
    assert_eq!(
        job.state,
        JobState::Parked,
        "the first rung is limited, so the current rung is the second provider"
    );
    assert_eq!(job.parked_reason, Some(ParkedReason::ProviderBlocked));
    assert!(
        store.last_run_of_job(&job.id).expect("run").is_none(),
        "the job never ran"
    );
}

#[test]
fn failed_quota_switch_writes_ledger_switch_failure() {
    let node = TestNode::with_adapter_config(
        false,
        Arc::new(ScriptAdapter::always(HarnessKind::Opencode, ScriptAction::ok("ok"))),
        |config| {
            let deepseek = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("deepseek");
            let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude");
            config.profiles.get_mut("worker").expect("worker profile").ladder = vec![deepseek, claude];
        },
    );
    let job = node.insert_job("switch-fails", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let store = node.store();
    let mut run = run_for(
        &node,
        Some(&job),
        "deepseek",
        RunRole::Worker,
        RunOutcome::Limit,
        Some(LimitKind::Quota),
    );
    run.session_id = Some(session.clone());
    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Quota, "deepseek", None))
        .expect("quota");
    assert!(
        matches!(action, FailoverAction::ParkProvider { ref provider } if provider == "deepseek"),
        "a failed switch parks the provider"
    );
    assert_eq!(
        store.kv_get("park:deepseek").expect("park request").as_deref(),
        Some("provider_blocked")
    );

    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Switch),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(ledger.len(), 1, "one switch failure row: {ledger:?}");
    let entry = &ledger[0];
    assert_eq!(entry.session_id.as_ref(), Some(&session));
    assert_eq!(entry.provider.as_deref(), Some("claude"), "the target provider");
    assert!(entry.summary.contains("failed"), "summary: {}", entry.summary);
    assert_eq!(entry.detail["reason"], "limit");
    assert_eq!(entry.detail["outcome"], "failed");
    assert!(
        entry.detail["error"]
            .as_str()
            .is_some_and(|error| error.contains("not installed")),
        "the failure reason is recorded: {}",
        entry.detail
    );
}

#[test]
fn second_rate_event_extends_backoff() {
    let node = TestNode::new();
    let store = node.store();
    let failover = Failover::new(node.deps.clone());
    let run = run_for(
        &node,
        None,
        "deepseek",
        RunRole::Worker,
        RunOutcome::Rate,
        Some(LimitKind::Rate),
    );
    let summary = limit_summary(LimitKind::Rate, "deepseek", None);
    let first = failover.on_run_outcome(&store, &run, &summary).expect("first rate");
    let first_until = match first {
        FailoverAction::Backoff { until } => until,
        other => panic!("expected backoff, got {other:?}"),
    };
    assert_eq!(first_until, ts().plus(Duration::from_secs(600)));

    // A second event inside the window applies `now + backoff` (streak 2 -> 1200 s)
    // instead of reusing the shorter first reset.
    node.advance(60);
    let second = failover.on_run_outcome(&store, &run, &summary).expect("second rate");
    let second_until = match second {
        FailoverAction::Backoff { until } => until,
        other => panic!("expected backoff, got {other:?}"),
    };
    assert_eq!(second_until, ts().plus(Duration::from_secs(60 + 1200)));
    assert!(second_until > first_until, "the second event extends the window");
    let limit = store.limit_for("deepseek").expect("limit").expect("rate row");
    assert_eq!(limit.resets_at, Some(second_until));
    assert_eq!(limit.cap_until, Some(second_until));

    // A stored reset further out is never shortened by a later, smaller backoff.
    let far = ts().plus(Duration::from_secs(3 * 3600));
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Rate,
            resets_at: Some(far),
            detail: "rate".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("far rate row");
    node.advance(60);
    let third = failover.on_run_outcome(&store, &run, &summary).expect("third rate");
    assert_eq!(third, FailoverAction::Backoff { until: far });
    assert_eq!(
        store.limit_for("deepseek").expect("limit").expect("rate row").resets_at,
        Some(far)
    );
}

#[test]
fn l6_marker_not_written_when_failover_errors() {
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("recovered handoff");
            }
            if is_worker_prompt(&input.prompt) {
                return ScriptAction {
                    summary: Some(limit_summary(LimitKind::Context, "deepseek", None)),
                    ..ScriptAction::default()
                };
            }
            ScriptAction::ok("ok")
        }),
    ));
    let node = TestNode::with_adapter(false, opencode);
    let store = node.store();
    let mut supervisor = node.supervisor();
    for name in ["l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    let job = node.insert_job("l6-retry", JobKind::Build, Role::Worker, Some(vec![]));
    supervisor.tick().expect("dispatch tick");
    // The context run records a context limit row, which leaves the capsule writer no
    // keeper rung; the runner thread's L6 call therefore fails.
    assert!(
        wait_until(Duration::from_secs(10), || node.log.contains("failover for run")),
        "the runner thread reports the L6 failure"
    );
    std::thread::sleep(Duration::from_millis(100));
    let run = store.last_run_of_job(&job.id).expect("run").expect("finished run");
    assert_eq!(
        store.kv_get(&format!("l6:{}", run.id)).expect("marker"),
        None,
        "a failed L6 must leave the done marker unset"
    );

    // Clear the transient context limit; the next tick retries L6 and harvests the run.
    store.clear_limit("deepseek").expect("clear context limit");
    supervisor.tick().expect("retry tick");
    assert!(
        wait_until(Duration::from_secs(10), || {
            store.kv_get(&format!("l6:{}", run.id)).expect("marker").as_deref() == Some("harvested")
        }),
        "the retry tick applies L6 and harvests the run"
    );
}

/// A two-rung worker ladder (claude then deepseek) whose opencode adapter answers a capsule
/// write with a valid draft, so a failover can complete the switch.
///
/// `credits_policy` sets `[providers.claude].on_credits_exhausted`; `None` leaves the
/// provider table empty, which must behave like the documented `park` default.
fn two_rung_worker_node(credits_policy: Option<OnCreditsExhausted>) -> TestNode {
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                capsule_action("deepseek handoff")
            } else {
                ScriptAction::ok("ok")
            }
        }),
    ));
    let claude = Arc::new(ScriptAdapter::always(
        HarnessKind::ClaudeCode,
        ScriptAction::ok("unused"),
    ));
    two_adapter_node(false, opencode, claude, |config| {
        let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude");
        let deepseek = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("deepseek");
        config.profiles.get_mut("worker").expect("worker profile").ladder = vec![claude, deepseek];
        if let Some(policy) = credits_policy {
            config.providers.insert(
                "claude".to_owned(),
                ProviderConfig {
                    on_credits_exhausted: policy,
                    ..ProviderConfig::default()
                },
            );
        }
    })
}

/// A two-rung worker ladder (claude then deepseek) for quota failover tests.
fn quota_failover_node() -> TestNode {
    two_rung_worker_node(None)
}

/// Inserts and finishes one worker run for `job` on `provider`.
///
/// `limit_kind` selects the classified outcome (`credits`/`auth`); `None` is an
/// ordinary ok run. The run carries `session` so L6 can switch it.
fn finish_worker_run(node: &TestNode, job: &Job, session: &SessionId, provider: &str, limit_kind: Option<LimitKind>) {
    let store = node.store();
    let outcome = match limit_kind {
        Some(LimitKind::Credits) => RunOutcome::Credits,
        Some(LimitKind::Auth) => RunOutcome::Auth,
        Some(_) => RunOutcome::Limit,
        None => RunOutcome::Ok,
    };
    let mut run = run_for(node, Some(job), provider, RunRole::Worker, outcome, limit_kind);
    run.session_id = Some(session.clone());
    run.ended_at = None;
    run.outcome = None;
    run.limit_kind = None;
    store.insert_run(&run).expect("open run");
    store
        .finish_run(
            &run.id,
            &vowp_store::RunFinish {
                outcome,
                exit_code: Some(if limit_kind.is_some() { 1 } else { 0 }),
                limit_kind,
                cost: Cost::unknown(),
                native_session_id: None,
                detail: "test run".to_owned(),
            },
        )
        .expect("finish run");
}

/// Creates one worker session already on `spec`, so a later L6 switch is observable.
fn create_session_on(node: &TestNode, job: &Job, spec: &str) -> SessionId {
    use vowp_core::session::service::SessionService;
    SessionService::new(node.deps.clone())
        .create_session(vowp_protocol::NewSession {
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            role: vowp_protocol::SessionRole::Worker,
            title: job.id.as_str().to_owned(),
            cwd: std::path::PathBuf::from(
                job.worktree
                    .clone()
                    .unwrap_or_else(|| node.main_repo.to_string_lossy().into_owned()),
            ),
            parent: None,
            executor: Some(Executor::parse(spec).expect("executor")),
        })
        .expect("create session")
}

/// Builds the incident's stale deepseek row: `unavailable` with an expired reset.
fn stale_unavailable_row() -> Limit {
    Limit {
        provider: "deepseek".to_owned(),
        harness: Some(HarnessKind::Opencode),
        kind: LimitRowKind::Unavailable,
        resets_at: Some(ts().minus(Duration::from_secs(60))),
        detail: "3 consecutive unknown failures".to_owned(),
        since: ts().minus(Duration::from_secs(18 * 60)),
        last_probe_at: None,
        probe_fail_streak: 0,
        concurrency_cap: None,
        cap_until: None,
    }
}

#[test]
fn stale_unavailable_and_probe_back_still_fail_over_to_the_healthy_rung() {
    let node = quota_failover_node();
    let store = node.store();
    // The exact sequence from the incident: claude quota with a future reset, while
    // deepseek carries the stale marks left by a failed spawn.
    store.upsert_limit(&stale_unavailable_row()).expect("stale row");
    store.kv_set("limit:deepseek", "unavailable").expect("limit mark");
    store.kv_set("probe_back:deepseek", "1").expect("legacy probe-back");
    let job = node.insert_job("stale-mark", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Limit,
        Some(LimitKind::Quota),
    );
    run.session_id = Some(session.clone());

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(
            &store,
            &run,
            &limit_summary(
                LimitKind::Quota,
                "claude",
                Some(ts().plus(Duration::from_secs(3 * 3600))),
            ),
        )
        .expect("quota failover");
    assert_eq!(
        action,
        FailoverAction::NextRung { rung: 1 },
        "a stale deepseek mark must never turn the ladder into ParkProvider"
    );
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("deepseek"),
        "the session actually moved to the healthy rung"
    );
}

#[test]
fn successful_run_clears_temporary_provider_marks() {
    let node = TestNode::new();
    let store = node.store();
    store.upsert_limit(&stale_unavailable_row()).expect("stale row");
    for (key, value) in [
        ("limit:deepseek", "unavailable"),
        ("probe_back:deepseek", "1"),
        ("probe_back_until:deepseek", "2026-11-08T17:59:00Z"),
        ("probe_pending:deepseek", "1"),
        ("park:deepseek", "provider_blocked"),
    ] {
        store.kv_set(key, value).expect("mark");
    }

    let run = run_for(&node, None, "deepseek", RunRole::Worker, RunOutcome::Ok, None);
    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &ok_summary())
        .expect("ok run");
    assert_eq!(action, FailoverAction::None);
    assert!(
        store.limit_for("deepseek").expect("limit").is_none(),
        "an ok run proves the provider works and clears its limit row"
    );
    for key in [
        "limit:deepseek",
        "probe_back:deepseek",
        "probe_back_until:deepseek",
        "probe_pending:deepseek",
        "park:deepseek",
    ] {
        assert_eq!(store.kv_get(key).expect("mark"), None, "cleared {key}");
    }
}

#[test]
fn current_credits_block_on_the_only_alternative_still_parks() {
    let node = quota_failover_node();
    let store = node.store();
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Credits,
            resets_at: None,
            detail: "insufficient balance".to_owned(),
            since: ts().minus(Duration::from_secs(60)),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("credits row");
    store.kv_set("limit:deepseek", "credits").expect("limit mark");
    store.kv_set("probe_back:deepseek", "0").expect("no probe-back");
    let job = node.insert_job("credits-elsewhere", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Limit,
        Some(LimitKind::Quota),
    );
    run.session_id = Some(session);

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Quota, "claude", None))
        .expect("quota failover");
    assert_eq!(
        action,
        FailoverAction::ParkProvider {
            provider: "claude".to_owned()
        },
        "credits on the only alternative rung is a genuine block, not a stale mark"
    );
    assert_eq!(
        store.limit_for("deepseek").expect("limit").expect("credits row").kind,
        LimitRowKind::Credits
    );

    // A forced successful run must not resurrect a provider whose credits the owner has
    // not resolved; credits stay sticky.
    let ok_run = run_for(&node, None, "deepseek", RunRole::Worker, RunOutcome::Ok, None);
    Failover::new(node.deps.clone())
        .on_run_outcome(&store, &ok_run, &ok_summary())
        .expect("ok run");
    assert_eq!(
        store.limit_for("deepseek").expect("limit").expect("credits row").kind,
        LimitRowKind::Credits,
        "credits and auth are only cleared by the owner's inbox answer"
    );
    assert_eq!(store.kv_get("probe_back:deepseek").expect("mark").as_deref(), Some("0"));
}

#[test]
fn probe_back_deadline_in_the_past_makes_the_provider_eligible() {
    let node = quota_failover_node();
    let store = node.store();
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Quota,
            resets_at: Some(ts().minus(Duration::from_secs(90))),
            detail: "quota".to_owned(),
            since: ts().minus(Duration::from_secs(2 * 60)),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("quota row");
    store.kv_set("limit:deepseek", "quota").expect("limit mark");
    store.kv_set("probe_back:deepseek", "1").expect("probe-back");
    store
        .kv_set(
            "probe_back_until:deepseek",
            &ts().minus(Duration::from_secs(1)).as_rfc3339(),
        )
        .expect("deadline");
    let job = node.insert_job("past-deadline", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Limit,
        Some(LimitKind::Quota),
    );
    run.session_id = Some(session);

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Quota, "claude", None))
        .expect("quota failover");
    assert_eq!(
        action,
        FailoverAction::NextRung { rung: 1 },
        "an expired probe-back deadline must not hide the rung"
    );
}

#[test]
fn credits_next_switches_to_declared_rung_and_records_limit_and_switch() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    let job = node.insert_job("credits-next", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Credits,
        Some(LimitKind::Credits),
    );
    run.session_id = Some(session.clone());

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Credits, "claude", None))
        .expect("credits next");
    assert_eq!(
        action,
        FailoverAction::NextRung { rung: 1 },
        "the explicit opt-in must use the declared fallback rung"
    );

    // The limit row still explains why the rung changed, and the provider is marked
    // probe-able instead of being parked.
    let limit = store.limit_for("claude").expect("limit").expect("credits row");
    assert_eq!(limit.kind, LimitRowKind::Credits);
    assert_eq!(limit.resets_at, None);
    assert_eq!(
        store.kv_get("probe_back:claude").expect("mark").as_deref(),
        Some("1"),
        "the exhausted provider is marked probe-able"
    );
    assert_eq!(
        store.kv_get("park:claude").expect("park request"),
        None,
        "next must not request a park"
    );
    assert!(
        store.list_inbox(None).expect("inbox").is_empty(),
        "next must not open a park notice"
    );

    let switched = store.require_session(&session).expect("session");
    assert_eq!(
        switched.executor.as_ref().map(Executor::provider).as_deref(),
        Some("deepseek"),
        "the session moved to rung 1"
    );
    assert_eq!(
        switched.executor.as_ref().map(ToString::to_string).as_deref(),
        Some("opencode:deepseek/deepseek-flash#max")
    );

    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Switch),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(ledger.len(), 1, "one switch ledger entry: {ledger:?}");
    let entry = &ledger[0];
    assert_eq!(entry.session_id.as_ref(), Some(&session));
    assert_eq!(entry.provider.as_deref(), Some("deepseek"));
    assert_eq!(entry.detail["reason"], "limit");
    assert!(
        entry.summary.contains("opencode:deepseek/deepseek-flash#max"),
        "summary: {}",
        entry.summary
    );
}

#[test]
fn credits_next_without_a_healthy_rung_falls_back_to_park_provider() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    // The only fallback rung is limited, so there is nowhere healthy to move.
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Quota,
            resets_at: Some(ts().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("deepseek quota");
    let job = node.insert_job("credits-next-no-rung", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Credits,
        Some(LimitKind::Credits),
    );
    run.session_id = Some(session);

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Credits, "claude", None))
        .expect("credits next without a rung");
    assert_eq!(
        action,
        FailoverAction::ParkProvider {
            provider: "claude".to_owned()
        }
    );
    assert_eq!(
        store.limit_for("claude").expect("limit").expect("credits row").kind,
        LimitRowKind::Credits,
        "the row is written even when the fallback is blocked"
    );
    assert_eq!(
        store.kv_get("park:claude").expect("park request").as_deref(),
        Some("provider_blocked")
    );
}

#[test]
fn credits_park_still_parks_even_with_a_healthy_rung() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Park));
    let store = node.store();
    let job = node.insert_job("credits-park", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Credits,
        Some(LimitKind::Credits),
    );
    run.session_id = Some(session);

    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Credits, "claude", None))
        .expect("credits park");
    assert_eq!(
        action,
        FailoverAction::ParkProvider {
            provider: "claude".to_owned()
        },
        "park remains the safe default even when another rung is healthy"
    );
    assert_eq!(
        store.kv_get("park:claude").expect("park request").as_deref(),
        Some("credits")
    );
    assert_eq!(
        store.kv_get("probe_back:claude").expect("mark").as_deref(),
        Some("0"),
        "a parked provider is not probe-backed"
    );
    assert!(
        store
            .ledger_since(&LedgerFilter {
                kind: Some(LedgerKind::Switch),
                ..LedgerFilter::default()
            })
            .expect("ledger")
            .is_empty(),
        "park must not switch the session"
    );
}

#[test]
fn auth_requires_the_explicit_provider_opt_in() {
    // Without a provider entry the effective policy is the documented `park` default.
    let node = two_rung_worker_node(None);
    let store = node.store();
    let job = node.insert_job("auth-default", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Auth,
        Some(LimitKind::Auth),
    );
    run.session_id = Some(session);
    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Auth, "claude", None))
        .expect("auth default");
    assert_eq!(
        action,
        FailoverAction::ParkProvider {
            provider: "claude".to_owned()
        },
        "auth defaults to park even when a healthy rung exists"
    );
    assert_eq!(
        store.kv_get("park:claude").expect("park request").as_deref(),
        Some("auth")
    );
    assert!(store.list_inbox(None).expect("inbox").is_empty());
    assert!(
        store
            .ledger_since(&LedgerFilter {
                kind: Some(LedgerKind::Switch),
                ..LedgerFilter::default()
            })
            .expect("ledger")
            .is_empty(),
        "the default auth policy never switches"
    );

    // The same explicit opt-in covers auth: it advances only when the provider sets it.
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    let job = node.insert_job("auth-next", JobKind::Build, Role::Worker, Some(vec![]));
    let session = create_session(&node, &job);
    let mut run = run_for(
        &node,
        Some(&job),
        "claude",
        RunRole::Worker,
        RunOutcome::Auth,
        Some(LimitKind::Auth),
    );
    run.session_id = Some(session.clone());
    let action = Failover::new(node.deps.clone())
        .on_run_outcome(&store, &run, &limit_summary(LimitKind::Auth, "claude", None))
        .expect("auth next");
    assert_eq!(action, FailoverAction::NextRung { rung: 1 });
    assert_eq!(
        store.limit_for("claude").expect("limit").expect("auth row").kind,
        LimitRowKind::Auth
    );
    assert_eq!(store.kv_get("probe_back:claude").expect("mark").as_deref(), Some("1"));
    assert_eq!(store.kv_get("park:claude").expect("park request"), None);
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("deepseek")
    );
}

/// `next` + a healthy rung: harvest must not undo the switch L6 made.
#[test]
fn credits_next_harvest_keeps_the_switched_job_running() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    let job = node.insert_job("credits-next-harvest", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Running);
    let session = create_session_on(&node, &job, "claude-code:claude-sonnet-5-5@medium");
    // A second job already ran on the exhausted provider. The opt-in applies to the
    // affected session, so the harvest backstop must not park this one either.
    let other = node.insert_job("credits-next-other", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&other, JobState::Running);
    let other_session = create_session_on(&node, &other, "claude-code:claude-sonnet-5-5@medium");
    finish_worker_run(&node, &other, &other_session, "claude", None);
    finish_worker_run(&node, &job, &session, "claude", Some(LimitKind::Credits));

    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("harvest tick");

    let harvested = store.require_job(&job.id).expect("job");
    assert_eq!(
        harvested.state,
        JobState::Running,
        "the explicit next policy must not be undone by the harvest park path"
    );
    assert_eq!(harvested.parked_reason, None);
    assert_eq!(
        store.require_job(&other.id).expect("other job").state,
        JobState::Running,
        "the opt-in does not park other jobs that already ran on the provider"
    );
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("deepseek"),
        "the session stays on the declared fallback rung"
    );
    let limit = store.limit_for("claude").expect("limit").expect("credits row");
    assert_eq!(limit.kind, LimitRowKind::Credits);
    assert!(
        store.list_inbox(None).expect("inbox").is_empty(),
        "no park notice is opened for the opt-in"
    );
    assert_eq!(
        store.kv_get("park:claude").expect("park request"),
        None,
        "harvest must not queue a second park request"
    );
}

/// `next` + no healthy rung: L6's fallback park remains the one and only park.
#[test]
fn credits_next_without_a_healthy_rung_parks_once_in_harvest() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    // The only fallback rung is limited, so L6 has nowhere healthy to move.
    store
        .upsert_limit(&Limit {
            provider: "deepseek".to_owned(),
            harness: Some(HarnessKind::Opencode),
            kind: LimitRowKind::Quota,
            resets_at: Some(ts().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("deepseek quota");
    let job = node.insert_job("credits-next-no-rung", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Running);
    let session = create_session_on(&node, &job, "claude-code:claude-sonnet-5-5@medium");
    finish_worker_run(&node, &job, &session, "claude", Some(LimitKind::Credits));

    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("harvest tick");

    let parked = store.require_job(&job.id).expect("job");
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(
        parked.parked_reason,
        Some(ParkedReason::ProviderBlocked),
        "the fallback park is L6's, not the harvest credits backstop"
    );
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "exactly one L6 fallback notice: {inbox:?}");
    assert_eq!(inbox[0].title, "provider blocked: claude");
    assert_eq!(
        store.kv_get("park:claude").expect("park request"),
        None,
        "the request is applied once and drained"
    );
    assert!(
        store.limit_for("claude").expect("limit").is_some(),
        "the credits row still explains the failed advance"
    );
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("claude"),
        "no healthy rung means no switch"
    );
}

/// `park` (the default): the harvest backstop still parks the provider.
#[test]
fn credits_park_harvest_backstop_is_unchanged() {
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Park));
    let store = node.store();
    let job = node.insert_job("credits-park-harvest", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Running);
    let session = create_session_on(&node, &job, "claude-code:claude-sonnet-5-5@medium");
    finish_worker_run(&node, &job, &session, "claude", Some(LimitKind::Credits));

    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("harvest tick");

    let parked = store.require_job(&job.id).expect("job");
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(parked.parked_reason, Some(ParkedReason::Credits));
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "exactly one credits notice: {inbox:?}");
    assert_eq!(inbox[0].title, "credits exhausted: claude");
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("claude"),
        "park never switches the session"
    );
    assert!(
        store
            .ledger_since(&LedgerFilter {
                kind: Some(LedgerKind::Switch),
                ..LedgerFilter::default()
            })
            .expect("ledger")
            .is_empty(),
        "park writes no switch ledger entry"
    );
    assert_eq!(store.kv_get("park:claude").expect("park request"), None);
}

/// Auth uses the same explicit provider gate as L6: park by default, advance on `next`.
#[test]
fn auth_harvest_uses_the_same_explicit_policy_as_l6() {
    // No provider table: the effective policy is the documented `park` default.
    let node = two_rung_worker_node(None);
    let store = node.store();
    let job = node.insert_job("auth-default-harvest", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Running);
    let session = create_session_on(&node, &job, "claude-code:claude-sonnet-5-5@medium");
    finish_worker_run(&node, &job, &session, "claude", Some(LimitKind::Auth));
    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("harvest tick");

    let parked = store.require_job(&job.id).expect("job");
    assert_eq!(parked.state, JobState::Parked);
    assert_eq!(parked.parked_reason, Some(ParkedReason::Auth));
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "exactly one auth notice: {inbox:?}");
    assert_eq!(inbox[0].title, "auth required: claude");
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("claude"),
        "auth does not silently advance without the owner's opt-in"
    );

    // The same explicit opt-in L6 reads also covers auth; harvest must not undo it.
    let node = two_rung_worker_node(Some(OnCreditsExhausted::Next));
    let store = node.store();
    let job = node.insert_job("auth-next-harvest", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&job, JobState::Running);
    let session = create_session_on(&node, &job, "claude-code:claude-sonnet-5-5@medium");
    // Another running job on the same provider catches a provider-wide harvest park.
    let other = node.insert_job("auth-next-other", JobKind::Build, Role::Worker, Some(vec![]));
    node.force_state(&other, JobState::Running);
    let other_session = create_session_on(&node, &other, "claude-code:claude-sonnet-5-5@medium");
    finish_worker_run(&node, &other, &other_session, "claude", None);
    finish_worker_run(&node, &job, &session, "claude", Some(LimitKind::Auth));
    let mut supervisor = node.supervisor();
    for name in ["l2-worker", "l4-planner", "l5-keeper"] {
        store.set_loop_state(name, ActiveState::Paused).expect("pause loop");
    }
    supervisor.tick().expect("harvest tick");

    let harvested = store.require_job(&job.id).expect("job");
    assert_eq!(
        harvested.state,
        JobState::Running,
        "auth under the explicit next policy follows L6, not the harvest backstop"
    );
    assert_eq!(harvested.parked_reason, None);
    assert_eq!(
        store.require_job(&other.id).expect("other job").state,
        JobState::Running,
        "auth next is per affected session and does not park other jobs"
    );
    assert_eq!(
        store
            .require_session(&session)
            .expect("session")
            .executor
            .as_ref()
            .map(Executor::provider)
            .as_deref(),
        Some("deepseek"),
        "auth advanced because the provider set the same opt-in"
    );
    assert!(
        store.list_inbox(None).expect("inbox").is_empty(),
        "the opt-in opens no auth notice"
    );
}
