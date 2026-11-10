//! L9 schedule catch-up tests used by the failover drill.
//! Spec: `docs/design/loops.md` §11.

#[path = "failover_support.rs"]
mod f;

use std::time::Duration;

use f::*;
use vowp_protocol::{
    ActiveState, JobKind, MissedPolicy, NewJob, Role, Schedule, ScheduleAction, ScheduleId, ScheduleSpecKind,
};
use vowp_store::{JobFilter, LedgerFilter};

/// Inserts one active interval schedule with a template action.
fn insert_schedule(node: &TestNode, name: &str, template: &str, policy: MissedPolicy, due: vowp_protocol::Timestamp) {
    node.store()
        .insert_schedule(&Schedule {
            id: ScheduleId::new(),
            project: Some(node.project.clone()),
            name: name.to_owned(),
            spec_kind: ScheduleSpecKind::Interval,
            spec: "every 30m".to_owned(),
            tz: "UTC".to_owned(),
            action: ScheduleAction::JobTemplate {
                template: NewJob {
                    id: vowp_protocol::JobId::parse(template).expect("job id"),
                    title: format!("scheduled {template}"),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: None,
                    needs: Vec::new(),
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: Some(Vec::new()),
                    priority: None,
                    loop_: None,
                    brief_text: None,
                },
            },
            missed_policy: policy,
            state: ActiveState::Active,
            last_run_at: None,
            next_run_at: Some(due),
            last_status: String::new(),
            created_at: due.minus(Duration::from_secs(3600)),
        })
        .expect("schedule");
}

/// Returns the schedule row called `name`.
fn schedule(node: &TestNode, name: &str) -> Schedule {
    node.store().schedule(name).expect("schedule").expect("row")
}

#[test]
fn drill_schedule_missed_run() {
    let node = TestNode::new();
    let now = ts();
    let due = now.minus(Duration::from_secs(3 * 3600));
    insert_schedule(&node, "run-once", "sched-job", MissedPolicy::RunOnce, due);
    insert_schedule(&node, "skip-one", "skip-job", MissedPolicy::Skip, due);
    let mut supervisor = node.supervisor();
    let report = supervisor.startup().expect("startup");
    assert_eq!(report.schedules_caught_up, 1, "run_once fires exactly once");
    assert_eq!(jobs_with_prefix(&node, "sched-job-"), 1);
    assert_eq!(jobs_with_prefix(&node, "skip-job-"), 0, "skip creates no job");
    let schedule = schedule(&node, "run-once");
    assert!(
        schedule.next_run_at.expect("next") > now,
        "next_run_at is in the future"
    );
    let ledger = node
        .store()
        .ledger_since(&LedgerFilter {
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let key = format!("{}@{}", schedule.id, due.as_rfc3339());
    assert!(
        ledger
            .iter()
            .any(|entry| entry.detail.get("key").and_then(|value| value.as_str()) == Some(&key)),
        "the due occurrence is recorded under its idempotency key"
    );
    assert!(
        ledger.iter().any(|entry| entry.summary.contains("skipped missed run")),
        "the skip policy records a skipped row"
    );

    // A double tick at the same instant cannot create a second job.
    supervisor.tick().expect("tick one");
    supervisor.tick().expect("tick two");
    assert_eq!(jobs_with_prefix(&node, "sched-job-"), 1);
    assert_eq!(jobs_with_prefix(&node, "skip-job-"), 0);
}

#[test]
fn startup_catch_up_respects_paused_schedules_loop() {
    let node = TestNode::new();
    let mut supervisor = node.supervisor();
    node.store()
        .set_loop_state("l9-schedules", ActiveState::Paused)
        .expect("pause l9");
    let due = ts().minus(Duration::from_secs(3 * 3600));
    insert_schedule(&node, "paused-l9", "paused-job", MissedPolicy::RunOnce, due);
    let report = supervisor.startup().expect("startup");
    assert_eq!(report.schedules_caught_up, 0);
    assert_eq!(jobs_with_prefix(&node, "paused-job-"), 0);
    assert_eq!(
        schedule(&node, "paused-l9").next_run_at,
        Some(due),
        "the paused loop leaves the occurrence armed"
    );

    // Resuming lets the next tick apply the normal missed-run policy once.
    node.store()
        .set_loop_state("l9-schedules", ActiveState::Active)
        .expect("resume l9");
    supervisor.tick().expect("resume tick");
    assert_eq!(jobs_with_prefix(&node, "paused-job-"), 1);
    supervisor.tick().expect("second tick");
    assert_eq!(jobs_with_prefix(&node, "paused-job-"), 1);
}

#[test]
fn paused_schedule_does_not_fire() {
    let node = TestNode::new();
    let mut paused = schedule_template(&node, "paused", "paused-schedule-job");
    paused.state = ActiveState::Paused;
    node.store().insert_schedule(&paused).expect("schedule");
    let mut supervisor = node.supervisor();
    supervisor.tick().expect("tick");
    assert_eq!(jobs_with_prefix(&node, "paused-schedule-job-"), 0);
    let _ = JobFilter::default();
}

/// Builds one active schedule row without inserting it.
fn schedule_template(node: &TestNode, name: &str, template: &str) -> Schedule {
    Schedule {
        id: ScheduleId::new(),
        project: Some(node.project.clone()),
        name: name.to_owned(),
        spec_kind: ScheduleSpecKind::Interval,
        spec: "every 30m".to_owned(),
        tz: "UTC".to_owned(),
        action: ScheduleAction::JobTemplate {
            template: NewJob {
                id: vowp_protocol::JobId::parse(template).expect("job id"),
                title: name.to_owned(),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: None,
                needs: Vec::new(),
                owns: Vec::new(),
                reads: Vec::new(),
                gates: Some(Vec::new()),
                priority: None,
                loop_: None,
                brief_text: None,
            },
        },
        missed_policy: MissedPolicy::RunOnce,
        state: ActiveState::Active,
        last_run_at: None,
        next_run_at: Some(ts()),
        last_status: String::new(),
        created_at: ts().minus(Duration::from_secs(3600)),
    }
}
