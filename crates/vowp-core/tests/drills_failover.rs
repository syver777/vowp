//! L6 failover drills: quota failover with probe-back, and credits parking.
//! Spec: `docs/design/loops.md` §13 (`drill_failover_and_probe_back`, `drill_credits_park`).

#[path = "failover_support.rs"]
mod f;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use f::*;
use vowp_protocol::{
    ActiveState, Executor, HarnessKind, InboxKind, JobKind, JobState, LedgerKind, LimitKind, LimitRowKind, OnQuota,
    ParkedReason, Profile, Role, RunRole, SwitchReason,
};
use vowp_store::LedgerFilter;

#[test]
fn drill_failover_and_probe_back() {
    let back = Arc::new(AtomicBool::new(false));
    let back_for_claude = Arc::clone(&back);
    let claude = Arc::new(ScriptAdapter::new(
        HarnessKind::ClaudeCode,
        Arc::new(move |input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("claude handoff");
            }
            if is_probe_prompt(&input.prompt) {
                return ScriptAction::ok("ok");
            }
            if is_planner_prompt(&input.prompt) {
                if back_for_claude.load(Ordering::SeqCst) {
                    return ScriptAction::ok("planned again on claude");
                }
                return ScriptAction {
                    summary: Some(limit_summary(
                        LimitKind::Quota,
                        "claude",
                        Some(ts().plus(Duration::from_secs(3 * 3600))),
                    )),
                    ..ScriptAction::default()
                };
            }
            ScriptAction::ok("ok")
        }),
    ));
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("opencode handoff");
            }
            ScriptAction::ok("planned on opencode")
        }),
    ));
    let node = two_adapter_node(false, opencode.clone(), claude.clone(), |config| {
        set_ladders(
            config,
            &[
                "claude-code:claude-sonnet-5-5@medium",
                "opencode:deepseek/deepseek-flash#max",
            ],
            &["opencode:deepseek/deepseek-flash#max"],
        );
    });
    let store = node.store();
    store.kv_set("wake:planner", "1").expect("wake");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("planner tick");
    assert!(
        wait_until(Duration::from_secs(10), || {
            store.limit_for("claude").map(|limit| limit.is_some()).unwrap_or(false)
        }),
        "the planner run records the quota row"
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner).contains(&SwitchReason::Limit)
                && switch_ledger_reason_recorded(&node, "limit")
        }),
        "the planner session switches on the limit and records it in the ledger"
    );
    node.wait_runs();

    let limit = store.limit_for("claude").expect("limit").expect("quota row");
    assert_eq!(limit.kind, LimitRowKind::Quota);
    assert_eq!(limit.resets_at, Some(ts().plus(Duration::from_secs(3 * 3600 + 120))));
    let reasons = completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner);
    assert!(
        reasons.contains(&SwitchReason::Limit),
        "the planner session switched on the limit: {reasons:?}"
    );

    // The next planner turn runs on the opencode rung after the minimum gap.
    node.advance(5 * 60);
    store.kv_set("wake:planner", "1").expect("wake");
    store.kv_delete("planner:wake_consumed").expect("consumed");
    for _ in 0..600 {
        supervisor.tick().expect("failover planner tick");
        std::thread::sleep(Duration::from_millis(10));
        if opencode
            .starts()
            .into_iter()
            .any(|start| is_planner_prompt(&start.prompt))
        {
            break;
        }
    }
    node.wait_runs();
    let planner_claude = claude
        .starts()
        .into_iter()
        .filter(|start| is_planner_prompt(&start.prompt))
        .count();
    let planner_opencode = opencode
        .starts()
        .into_iter()
        .filter(|start| is_planner_prompt(&start.prompt))
        .count();
    assert_eq!(planner_claude, 1);
    assert_eq!(planner_opencode, 1, "the next turn uses the opencode rung");
    // After the reset the probe succeeds and the planner switches back.
    back.store(true, Ordering::SeqCst);
    node.advance(3 * 3600 + 121);
    let mut probed = false;
    for _ in 0..600 {
        node.wait_runs();
        supervisor.tick().expect("probe tick");
        if store.limit_for("claude").expect("limit").is_none()
            && completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner).contains(&SwitchReason::ProbeBack)
            && switch_ledger_reason_recorded(&node, "probe_back")
        {
            probed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        probed,
        "the probe clears the quota row and records the probe_back switch in the ledger"
    );
    let probes = claude
        .starts()
        .into_iter()
        .filter(|start| is_probe_prompt(&start.prompt))
        .count();
    assert_eq!(probes, 1, "exactly one probe run");
    let reasons = completed_switch_reasons(&node, vowp_protocol::SessionRole::Planner);
    assert!(
        reasons.contains(&SwitchReason::ProbeBack),
        "a second switch returns to rung 0: {reasons:?}"
    );
    let switches = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Switch),
            ..LedgerFilter::default()
        })
        .expect("ledger switches");
    assert!(
        switches
            .iter()
            .any(|entry| { entry.detail.get("reason").and_then(|value| value.as_str()) == Some("limit") })
    );
    assert!(
        switches
            .iter()
            .any(|entry| { entry.detail.get("reason").and_then(|value| value.as_str()) == Some("probe_back") })
    );
    let limits = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Limit),
            ..LedgerFilter::default()
        })
        .expect("ledger limits");
    assert!(limits.iter().any(|entry| entry.summary.contains("provider back")));
    let parked = store
        .list_jobs(&vowp_store::JobFilter {
            project: Some(node.project.clone()),
            ..vowp_store::JobFilter::default()
        })
        .expect("jobs")
        .into_iter()
        .filter(|job| job.state == JobState::Parked)
        .count();
    assert_eq!(parked, 0, "no jobs are parked by a quota failover");
}

#[test]
fn drill_credits_park() {
    let opencode = Arc::new(ScriptAdapter::new(
        HarnessKind::Opencode,
        Arc::new(|input: &vowp_protocol::StartInput| {
            if is_capsule_prompt(&input.prompt) {
                return capsule_action("credits handoff");
            }
            if is_worker_prompt(&input.prompt) {
                return ScriptAction {
                    summary: Some(limit_summary(LimitKind::Credits, "deepseek", None)),
                    ..ScriptAction::default()
                };
            }
            ScriptAction::ok("ok")
        }),
    ));
    let claude = Arc::new(ScriptAdapter::always(
        HarnessKind::ClaudeCode,
        ScriptAction::ok("unused"),
    ));
    let node = two_adapter_node(false, opencode.clone(), claude.clone(), |config| {
        // A second worker provider makes "no run started on another provider" a
        // job-side claim: credits must park the jobs, never advance the ladder.
        let deepseek = Executor::parse("opencode:deepseek/deepseek-flash#max").expect("deepseek");
        let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude");
        config.profiles.insert(
            "worker".to_owned(),
            Profile {
                ladder: vec![deepseek, claude],
                on_quota: OnQuota::NextThenProbeBack,
                node: None,
            },
        );
    });
    let store = node.store();
    let mut supervisor = node.supervisor();
    store
        .set_loop_state("l4-planner", ActiveState::Paused)
        .expect("pause planner");
    store
        .set_loop_state("l5-keeper", ActiveState::Paused)
        .expect("pause keeper");
    let first = node.insert_job("credits-one", JobKind::Build, Role::Worker, Some(vec![]));
    let second = node.insert_job("credits-two", JobKind::Build, Role::Worker, Some(vec![]));
    // L6 writes the `park:<provider>` request after the run row ends but before its
    // durable `l6:<run>` marker. Answering while that request is still queued lets the
    // answer tick resolve the jobs at step 0 and then re-park them when harvest drains
    // the stale request, so wait for both runs' L6 policy before answering.
    let settled = [first.id.clone(), second.id.clone()];
    node.tick_until(&mut supervisor, 30, |store| {
        [&first, &second].iter().all(|job| {
            store
                .require_job(&job.id)
                .map(|job| job.state == JobState::Parked)
                .unwrap_or(false)
        }) && l6_policy_settled(&node, &settled)
    });
    for job in [&first, &second] {
        let job = store.require_job(&job.id).expect("job");
        assert_eq!(job.state, JobState::Parked);
        assert_eq!(job.parked_reason, Some(ParkedReason::Credits));
    }
    let limit = store.limit_for("deepseek").expect("limit").expect("credits row");
    assert_eq!(limit.kind, LimitRowKind::Credits);
    assert_eq!(limit.resets_at, None);
    let inbox = store.list_inbox(None).expect("inbox");
    assert_eq!(inbox.len(), 1, "exactly one credits notice");
    assert_eq!(inbox[0].kind, InboxKind::Question);
    assert_eq!(inbox[0].title, "credits exhausted: deepseek");
    assert_eq!(
        opencode
            .starts()
            .into_iter()
            .filter(|start| is_planner_prompt(&start.prompt) || is_keeper_prompt(&start.prompt))
            .count(),
        0,
        "no run starts on another role/provider after credits"
    );
    assert!(
        opencode.starts().iter().any(|start| is_worker_prompt(&start.prompt)),
        "the credits run started on the deepseek worker"
    );
    assert!(
        claude.starts().is_empty(),
        "no run started on the second worker provider: {:?}",
        claude.starts()
    );
    assert!(
        store
            .open_runs()
            .expect("open runs")
            .iter()
            .all(|run| run.role != RunRole::Worker),
        "the parked jobs have no live worker runs"
    );

    // Answering the notice resolves both jobs and clears the limit. Pause L2 so the
    // jobs stay observable as `ready` instead of being dispatched in the same tick.
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
