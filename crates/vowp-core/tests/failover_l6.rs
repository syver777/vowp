//! L6 failover tests: rate backoff, auth/credits parking, unknown failures.
//! Spec: `docs/design/loops.md` §8 and `docs/design/crates/vowp-core.md` §7.

#[path = "failover_support.rs"]
mod f;

use std::sync::Arc;
use std::time::Duration;

use f::*;
use vowp_core::loops::l6_failover::{Failover, FailoverAction};
use vowp_protocol::{
    ActiveState, Cost, Executor, HarnessKind, InboxKind, JobKind, JobState, LedgerKind, Limit, LimitKind, LimitRowKind,
    ParkedReason, Role, Run, RunId, RunOutcome, RunRole, RunSummary,
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

    // An ok run resets the streak; a probe is due only after the 15 minutes.
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
