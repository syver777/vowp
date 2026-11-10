//! The transition table (loops.md §2, rows T1–T37) and the state-machine properties.
//! Spec: `docs/design/crates/vowp-graph.md` §3 and §10.

use proptest::prelude::*;
use vowp_graph::{
    Effect, GraphError, JobEvent, MAX_CONTINUES, MAX_HANG_RESTARTS, Transition, TransitionBounds, attempts_left,
    is_exhausted, transition, transition_with,
};
use vowp_protocol::{
    Gate, InboxKind, Job, JobId, JobKind, JobLoop, JobState, OnExhausted, ParkedReason, ProjectName, ReviewGate, Role,
    RunPurpose, Timestamp, default_gates,
};

/// Builds a fixed timestamp for deterministic test rows.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Builds a job row in `state` with the default gates for `kind`.
fn job(id: &str, state: JobState, kind: JobKind) -> Job {
    let id = JobId::parse(id).expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    Job {
        id: id.clone(),
        project,
        title: format!("job {id}"),
        kind,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state,
        priority: 50,
        needs: Vec::new(),
        owns: vec!["crates/x/**".to_owned()],
        reads: Vec::new(),
        brief: format!(".vowp/jobs/{id}/brief.md"),
        report: format!(".vowp/jobs/{id}/report.md"),
        gates: default_gates(kind),
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

/// Returns the transition for a pair the table defines, or panics with the error.
fn ok(job_row: &Job, event: JobEvent) -> Transition {
    transition(job_row, &event).unwrap_or_else(|error| panic!("expected a valid transition for {event:?}: {error}"))
}

fn start(purpose: RunPurpose) -> Effect {
    Effect::StartRun { purpose }
}

fn inbox(kind: InboxKind) -> Effect {
    Effect::NotifyInbox { kind }
}

#[test]
fn t01_pending_needs_satisfied_to_ready() {
    let job_row = job("t01", JobState::Pending, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::NeedsSatisfied),
        Transition {
            to: JobState::Ready,
            effects: Vec::new(),
        }
    );
}

#[test]
fn t02_ready_dispatched_to_running() {
    let job_row = job("t02", JobState::Ready, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::Dispatched),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::CreateWorktree, start(RunPurpose::Start)],
        }
    );

    let mut continued = job_row.clone();
    continued.continues = 1;
    assert_eq!(
        ok(&continued, JobEvent::Dispatched),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::CreateWorktree, start(RunPurpose::Continue)],
        }
    );

    for kind in [JobKind::Fix, JobKind::Merge] {
        let child = job("t02-child", JobState::Ready, kind);
        assert_eq!(
            ok(&child, JobEvent::Dispatched),
            Transition {
                to: JobState::Running,
                effects: vec![Effect::CreateWorktree, start(RunPurpose::Fix)],
            }
        );
    }
}

#[test]
fn t03_running_report_ready_to_gating() {
    let job_row = job("t03", JobState::Running, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::ReportReady),
        Transition {
            to: JobState::Gating,
            effects: vec![Effect::RunGates],
        }
    );
}

#[test]
fn t04_t05_continue_guard_boundary() {
    let mut job_row = job("t04", JobState::Running, JobKind::Build);
    job_row.continues = MAX_CONTINUES - 1;
    assert_eq!(
        ok(&job_row, JobEvent::RunEndedNoReport),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::IncrementContinues, start(RunPurpose::Continue)],
        }
    );

    job_row.continues = MAX_CONTINUES;
    assert_eq!(
        ok(&job_row, JobEvent::RunEndedNoReport),
        Transition {
            to: JobState::Stalled,
            effects: vec![Effect::WakeKeeper],
        }
    );

    let bounds = TransitionBounds {
        max_continues: 1,
        max_hang_restarts: MAX_HANG_RESTARTS,
    };
    let mut fresh = job("t04-custom", JobState::Running, JobKind::Build);
    fresh.continues = 1;
    assert_eq!(
        transition_with(&fresh, &JobEvent::RunEndedNoReport, &bounds).expect("valid transition"),
        Transition {
            to: JobState::Stalled,
            effects: vec![Effect::WakeKeeper],
        }
    );
}

#[test]
fn t06_t07_stall_continue_boundary() {
    let mut job_row = job("t06", JobState::Running, JobKind::Build);
    job_row.continues = MAX_CONTINUES - 1;
    assert_eq!(
        ok(&job_row, JobEvent::Stalled { silent: false }),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::StopRun, Effect::IncrementContinues, start(RunPurpose::Continue),],
        }
    );

    job_row.continues = MAX_CONTINUES;
    assert_eq!(
        ok(&job_row, JobEvent::Stalled { silent: false }),
        Transition {
            to: JobState::Stalled,
            effects: vec![Effect::StopRun, Effect::WakeKeeper],
        }
    );
}

#[test]
fn t08_t09_hang_restart_boundary() {
    let mut job_row = job("t08", JobState::Running, JobKind::Build);
    job_row.hang_restarts = MAX_HANG_RESTARTS - 1;
    assert_eq!(
        ok(&job_row, JobEvent::Stalled { silent: true }),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::StopRun, Effect::IncrementHangRestarts, start(RunPurpose::Start),],
        }
    );

    job_row.hang_restarts = MAX_HANG_RESTARTS;
    assert_eq!(
        ok(&job_row, JobEvent::Stalled { silent: true }),
        Transition {
            to: JobState::Stalled,
            effects: vec![Effect::StopRun, Effect::WakeKeeper],
        }
    );
}

#[test]
fn t10_stalled_keeper_nudge_to_running() {
    let job_row = job("t10", JobState::Stalled, JobKind::Build);
    for text in [None, Some("the last run stopped after its report".to_owned())] {
        assert_eq!(
            ok(&job_row, JobEvent::KeeperNudge { text }),
            Transition {
                to: JobState::Running,
                effects: vec![Effect::ResetContinues, start(RunPurpose::Continue)],
            }
        );
    }
}

#[test]
fn t11_stalled_keeper_restart_to_running() {
    let job_row = job("t11", JobState::Stalled, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::KeeperRestart),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::ResetContinues, start(RunPurpose::Start)],
        }
    );
}

#[test]
fn t12_stalled_keeper_replan_to_replanned() {
    let job_row = job("t12", JobState::Stalled, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::KeeperReplan),
        Transition {
            to: JobState::Replanned,
            effects: vec![Effect::WakePlanner],
        }
    );
}

#[test]
fn t13_keeper_fail_notifies_parent_for_child_kinds() {
    for kind in [JobKind::Fix, JobKind::Merge, JobKind::Verify] {
        let job_row = job("t13-child", JobState::Stalled, kind);
        assert_eq!(
            ok(&job_row, JobEvent::KeeperFail),
            Transition {
                to: JobState::Failed,
                effects: vec![inbox(InboxKind::Notice), Effect::NotifyParent],
            },
            "child kind {kind:?}"
        );
    }

    for kind in [JobKind::Build, JobKind::Docs, JobKind::Decision] {
        let job_row = job("t13-parent", JobState::Stalled, kind);
        assert_eq!(
            ok(&job_row, JobEvent::KeeperFail),
            Transition {
                to: JobState::Failed,
                effects: vec![inbox(InboxKind::Notice)],
            },
            "non-child kind {kind:?}"
        );
    }
}

#[test]
fn t14_fix_merge_verify_job_gates_passed_goes_done() {
    for kind in [JobKind::Fix, JobKind::Merge, JobKind::Verify] {
        let mut job_row = job("t14", JobState::Gating, kind);
        // T14 wins over T15 even if the child kind carries a review gate: a child job never
        // reaches reviewing/merging.
        job_row.gates = vec![Gate::Review(ReviewGate { by: Role::Verifier })];
        assert_eq!(
            ok(&job_row, JobEvent::GatesPassed),
            Transition {
                to: JobState::Done,
                effects: vec![Effect::NotifyParent],
            },
            "child kind {kind:?}"
        );
    }
}

#[test]
fn t15_t16_review_gate_selects_reviewing_or_merging() {
    let reviewed = job("t15", JobState::Gating, JobKind::Build);
    assert!(reviewed.gates.iter().any(|gate| matches!(gate, Gate::Review(_))));
    assert_eq!(
        ok(&reviewed, JobEvent::GatesPassed),
        Transition {
            to: JobState::Reviewing,
            effects: vec![Effect::StartVerify],
        }
    );

    let mut plain = job("t16", JobState::Gating, JobKind::Build);
    plain.gates = vec![Gate::Report(Default::default()), Gate::OwnsRespected(true)];
    assert_eq!(
        ok(&plain, JobEvent::GatesPassed),
        Transition {
            to: JobState::Merging,
            effects: vec![Effect::EnqueueMerge],
        }
    );
}

#[test]
fn t17_t18_attempts_boundary_and_on_exhausted_variants() {
    let mut fixing = job("t17", JobState::Gating, JobKind::Build);
    fixing.attempts = fixing.loop_.max_attempts - 1;
    assert_eq!(
        ok(&fixing, JobEvent::GatesFailed),
        Transition {
            to: JobState::Fixing,
            effects: vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix },],
        }
    );

    let mut exhausted = fixing.clone();
    exhausted.attempts = exhausted.loop_.max_attempts;

    exhausted.loop_.on_exhausted = OnExhausted::Escalate;
    assert_eq!(
        ok(&exhausted, JobEvent::GatesFailed),
        Transition {
            to: JobState::Failed,
            effects: vec![
                Effect::CreateDecisionJob,
                inbox(InboxKind::Decision),
                Effect::WakeKeeper,
            ],
        }
    );

    exhausted.loop_.on_exhausted = OnExhausted::Park;
    assert_eq!(
        ok(&exhausted, JobEvent::GatesFailed),
        Transition {
            to: JobState::Parked,
            effects: vec![
                Effect::SetParkedReason(ParkedReason::Exhausted),
                inbox(InboxKind::Question),
            ],
        }
    );

    exhausted.loop_.on_exhausted = OnExhausted::Fail;
    assert_eq!(
        ok(&exhausted, JobEvent::GatesFailed),
        Transition {
            to: JobState::Failed,
            effects: vec![Effect::WakeKeeper],
        }
    );
}

#[test]
fn t19_reviewing_verify_passed_to_merging() {
    let job_row = job("t19", JobState::Reviewing, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::VerifyPassed),
        Transition {
            to: JobState::Merging,
            effects: vec![Effect::EnqueueMerge],
        }
    );
}

#[test]
fn t20_t21_verify_failed_attempts_boundary() {
    let mut job_row = job("t20", JobState::Reviewing, JobKind::Build);
    job_row.attempts = job_row.loop_.max_attempts - 1;
    assert_eq!(
        ok(&job_row, JobEvent::VerifyFailed),
        Transition {
            to: JobState::Fixing,
            effects: vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix },],
        }
    );

    job_row.attempts = job_row.loop_.max_attempts;
    job_row.loop_.on_exhausted = OnExhausted::Park;
    assert_eq!(
        ok(&job_row, JobEvent::VerifyFailed),
        Transition {
            to: JobState::Parked,
            effects: vec![
                Effect::SetParkedReason(ParkedReason::Exhausted),
                inbox(InboxKind::Question),
            ],
        }
    );
}

#[test]
fn t22_fixing_fix_done_to_gating() {
    let job_row = job("t22", JobState::Fixing, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::FixDone { kind: JobKind::Fix }),
        Transition {
            to: JobState::Gating,
            effects: vec![Effect::ResetContinues, Effect::RunGates],
        }
    );
}

#[test]
fn t23_fixing_merge_fix_done_to_merging() {
    let job_row = job("t23", JobState::Fixing, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::FixDone { kind: JobKind::Merge }),
        Transition {
            to: JobState::Merging,
            effects: vec![Effect::EnqueueMerge],
        }
    );
}

#[test]
fn t24_fix_failed_fails_original() {
    let job_row = job("t24", JobState::Fixing, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::FixFailed),
        Transition {
            to: JobState::Failed,
            effects: vec![inbox(InboxKind::Notice), Effect::WakeKeeper],
        }
    );
}

#[test]
fn t25_merging_succeeded_to_done() {
    let job_row = job("t25", JobState::Merging, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::MergeSucceeded),
        Transition {
            to: JobState::Done,
            effects: vec![Effect::CleanupWorktree, Effect::UnblockDependents],
        }
    );
}

#[test]
fn t26_t27_merge_failure_creates_fix() {
    let job_row = job("t26", JobState::Merging, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::MergeConflict),
        Transition {
            to: JobState::Fixing,
            effects: vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Merge },],
        }
    );
    assert_eq!(
        ok(&job_row, JobEvent::MergeChecksFailed),
        Transition {
            to: JobState::Fixing,
            effects: vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix },],
        }
    );
}

#[test]
fn t28_merge_exhausted_on_exhausted_variants() {
    let mut job_row = job("t28", JobState::Merging, JobKind::Build);
    job_row.attempts = job_row.loop_.max_attempts;

    job_row.loop_.on_exhausted = OnExhausted::Escalate;
    for event in [JobEvent::MergeConflict, JobEvent::MergeChecksFailed] {
        assert_eq!(
            ok(&job_row, event),
            Transition {
                to: JobState::Failed,
                effects: vec![
                    Effect::CreateDecisionJob,
                    inbox(InboxKind::Decision),
                    Effect::WakeKeeper,
                ],
            }
        );
    }

    job_row.loop_.on_exhausted = OnExhausted::Park;
    assert_eq!(
        ok(&job_row, JobEvent::MergeConflict),
        Transition {
            to: JobState::Parked,
            effects: vec![
                Effect::SetParkedReason(ParkedReason::Exhausted),
                inbox(InboxKind::Question),
            ],
        }
    );

    job_row.loop_.on_exhausted = OnExhausted::Fail;
    assert_eq!(
        ok(&job_row, JobEvent::MergeChecksFailed),
        Transition {
            to: JobState::Failed,
            effects: vec![Effect::WakeKeeper],
        }
    );
}

#[test]
fn blocked_effects_depend_on_reason_and_state() {
    let reasons = [
        ParkedReason::Credits,
        ParkedReason::Auth,
        ParkedReason::Question,
        ParkedReason::ProviderBlocked,
        ParkedReason::Manual,
        ParkedReason::Exhausted,
    ];
    let states = [
        JobState::Pending,
        JobState::Ready,
        JobState::Running,
        JobState::Stalled,
        JobState::Failed,
    ];

    for state in states {
        for reason in reasons {
            let job_row = job("t29", state, JobKind::Build);
            let mut effects = Vec::new();
            if state == JobState::Running {
                effects.push(Effect::StopRun);
            }
            if matches!(
                reason,
                ParkedReason::Question | ParkedReason::Auth | ParkedReason::Credits | ParkedReason::ProviderBlocked
            ) {
                effects.push(inbox(InboxKind::Question));
            }
            effects.push(Effect::SetParkedReason(reason));
            assert_eq!(
                ok(&job_row, JobEvent::Blocked { reason }),
                Transition {
                    to: JobState::Parked,
                    effects,
                },
                "state {state:?}, reason {reason:?}"
            );
        }
    }

    for state in [
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
        JobState::Parked,
    ] {
        let job_row = job("t29-invalid", state, JobKind::Build);
        assert!(
            matches!(
                transition(
                    &job_row,
                    &JobEvent::Blocked {
                        reason: ParkedReason::Question
                    }
                ),
                Err(GraphError::InvalidTransition { .. })
            ),
            "Blocked must not be accepted in {state:?}"
        );
    }
}

#[test]
fn t30_human_park_stops_running_and_parks() {
    let running = job("t30-running", JobState::Running, JobKind::Build);
    assert_eq!(
        ok(&running, JobEvent::HumanPark),
        Transition {
            to: JobState::Parked,
            effects: vec![Effect::StopRun, Effect::SetParkedReason(ParkedReason::Manual)],
        }
    );

    for state in [
        JobState::Pending,
        JobState::Ready,
        JobState::Stalled,
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
        JobState::Failed,
        JobState::Parked,
    ] {
        let job_row = job("t30", state, JobKind::Build);
        assert_eq!(
            ok(&job_row, JobEvent::HumanPark),
            Transition {
                to: JobState::Parked,
                effects: vec![Effect::SetParkedReason(ParkedReason::Manual)],
            },
            "state {state:?}"
        );
    }
}

#[test]
fn t31_parked_resolved_to_ready() {
    let job_row = job("t31", JobState::Parked, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::Resolved),
        Transition {
            to: JobState::Ready,
            effects: vec![Effect::ClearParkedReason, Effect::ResetContinues],
        }
    );
}

#[test]
fn t32_failed_retry_to_ready() {
    let job_row = job("t32", JobState::Failed, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::Retry),
        Transition {
            to: JobState::Ready,
            effects: vec![Effect::ResetCounters],
        }
    );
}

#[test]
fn t33_cancel_keeps_worktree_and_stops_only_running() {
    let running = job("t33", JobState::Running, JobKind::Build);
    let outcome = ok(&running, JobEvent::Cancel);
    assert_eq!(outcome.to, JobState::Cancelled);
    assert_eq!(outcome.effects, vec![Effect::StopRun]);
    assert!(
        !outcome.effects.contains(&Effect::CleanupWorktree),
        "cancel keeps the worktree for inspection"
    );

    for state in [
        JobState::Pending,
        JobState::Ready,
        JobState::Stalled,
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
        JobState::Failed,
        JobState::Parked,
    ] {
        let job_row = job("t33-idle", state, JobKind::Build);
        assert_eq!(
            ok(&job_row, JobEvent::Cancel),
            Transition {
                to: JobState::Cancelled,
                effects: Vec::new(),
            },
            "state {state:?}"
        );
    }

    let done = job("t33-done", JobState::Done, JobKind::Build);
    assert!(matches!(
        transition(&done, &JobEvent::Cancel),
        Err(GraphError::InvalidTransition { .. })
    ));
}

#[test]
fn t34_parked_or_failed_keeper_replan_to_replanned() {
    for state in [JobState::Parked, JobState::Failed] {
        let job_row = job("t34", state, JobKind::Build);
        assert_eq!(
            ok(&job_row, JobEvent::KeeperReplan),
            Transition {
                to: JobState::Replanned,
                effects: vec![Effect::WakePlanner],
            },
            "state {state:?}"
        );
    }
}

#[test]
fn t35_run_rate_limited_delays_continue_without_counter_change() {
    let until = at(1_800_000_000);
    let mut job_row = job("t35", JobState::Running, JobKind::Build);
    job_row.continues = MAX_CONTINUES;
    assert_eq!(
        ok(&job_row, JobEvent::RunRateLimited { until }),
        Transition {
            to: JobState::Running,
            effects: vec![Effect::DelayRun {
                purpose: RunPurpose::Continue,
                until,
            }],
        }
    );
}

#[test]
fn t36_gates_failed_on_fix_merge_verify_job_fails_with_notify_parent_no_fix_of_fix() {
    for kind in [JobKind::Fix, JobKind::Merge, JobKind::Verify] {
        let job_row = job("t36", JobState::Gating, kind);
        assert!(job_row.attempts < job_row.loop_.max_attempts);
        assert_eq!(
            ok(&job_row, JobEvent::GatesFailed),
            Transition {
                to: JobState::Failed,
                effects: vec![Effect::NotifyParent, inbox(InboxKind::Notice)],
            },
            "child kind {kind:?} must never create a fix-of-fix"
        );
    }
}

#[test]
fn t37_verifier_failed_parks_original_with_question() {
    let job_row = job("t37", JobState::Reviewing, JobKind::Build);
    assert_eq!(
        ok(&job_row, JobEvent::VerifierFailed),
        Transition {
            to: JobState::Parked,
            effects: vec![
                Effect::SetParkedReason(ParkedReason::Question),
                inbox(InboxKind::Question),
            ],
        }
    );
}

#[test]
fn attempts_left_saturates() {
    let mut job_row = job("attempts-left", JobState::Gating, JobKind::Build);
    job_row.loop_.max_attempts = 3;
    job_row.attempts = 1;
    assert_eq!(attempts_left(&job_row), 2);
    assert!(!is_exhausted(&job_row));

    job_row.attempts = 3;
    assert_eq!(attempts_left(&job_row), 0);
    assert!(is_exhausted(&job_row));

    job_row.attempts = 9;
    assert_eq!(attempts_left(&job_row), 0);
    assert!(is_exhausted(&job_row));
}

/// All job states in discriminant order.
fn all_states() -> [JobState; 13] {
    [
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
    ]
}

/// One representative event for every variant and every payload shape the table uses.
fn representative_events() -> Vec<JobEvent> {
    let mut events = vec![
        JobEvent::NeedsSatisfied,
        JobEvent::Dispatched,
        JobEvent::ReportReady,
        JobEvent::RunEndedNoReport,
        JobEvent::RunRateLimited {
            until: at(2_000_000_000),
        },
        JobEvent::Stalled { silent: false },
        JobEvent::Stalled { silent: true },
        JobEvent::KeeperNudge { text: None },
        JobEvent::KeeperNudge {
            text: Some("continue".to_owned()),
        },
        JobEvent::KeeperRestart,
        JobEvent::KeeperReplan,
        JobEvent::KeeperFail,
        JobEvent::GatesPassed,
        JobEvent::GatesFailed,
        JobEvent::VerifyPassed,
        JobEvent::VerifyFailed,
        JobEvent::FixDone { kind: JobKind::Fix },
        JobEvent::FixDone { kind: JobKind::Merge },
        JobEvent::FixDone { kind: JobKind::Build },
        JobEvent::FixFailed,
        JobEvent::VerifierFailed,
        JobEvent::MergeSucceeded,
        JobEvent::MergeConflict,
        JobEvent::MergeChecksFailed,
        JobEvent::HumanPark,
        JobEvent::Resolved,
        JobEvent::Retry,
        JobEvent::Cancel,
    ];
    for reason in [
        ParkedReason::Credits,
        ParkedReason::Auth,
        ParkedReason::Question,
        ParkedReason::ProviderBlocked,
        ParkedReason::Manual,
        ParkedReason::Exhausted,
    ] {
        events.push(JobEvent::Blocked { reason });
    }
    events
}

/// The allow-list derived from loops.md §2: every source state and event the table defines.
fn allowed_pairs() -> Vec<(JobState, &'static str)> {
    use JobState::{Failed, Fixing, Gating, Merging, Parked, Pending, Ready, Reviewing, Running, Stalled};
    vec![
        (Pending, "NeedsSatisfied"),
        (Pending, "Blocked"),
        (Pending, "HumanPark"),
        (Pending, "Cancel"),
        (Ready, "Dispatched"),
        (Ready, "Blocked"),
        (Ready, "HumanPark"),
        (Ready, "Cancel"),
        (Running, "ReportReady"),
        (Running, "RunEndedNoReport"),
        (Running, "Stalled(false)"),
        (Running, "Stalled(true)"),
        (Running, "RunRateLimited"),
        (Running, "Blocked"),
        (Running, "HumanPark"),
        (Running, "Cancel"),
        (Stalled, "KeeperNudge"),
        (Stalled, "KeeperRestart"),
        (Stalled, "KeeperReplan"),
        (Stalled, "KeeperFail"),
        (Stalled, "Blocked"),
        (Stalled, "HumanPark"),
        (Stalled, "Cancel"),
        (Gating, "GatesPassed"),
        (Gating, "GatesFailed"),
        (Gating, "HumanPark"),
        (Gating, "Cancel"),
        (Reviewing, "VerifyPassed"),
        (Reviewing, "VerifyFailed"),
        (Reviewing, "VerifierFailed"),
        (Reviewing, "HumanPark"),
        (Reviewing, "Cancel"),
        (Fixing, "FixDone(Fix)"),
        (Fixing, "FixDone(Merge)"),
        (Fixing, "FixFailed"),
        (Fixing, "HumanPark"),
        (Fixing, "Cancel"),
        (Merging, "MergeSucceeded"),
        (Merging, "MergeConflict"),
        (Merging, "MergeChecksFailed"),
        (Merging, "HumanPark"),
        (Merging, "Cancel"),
        (Failed, "Blocked"),
        (Failed, "Retry"),
        (Failed, "KeeperReplan"),
        (Failed, "HumanPark"),
        (Failed, "Cancel"),
        (Parked, "Resolved"),
        (Parked, "KeeperReplan"),
        (Parked, "HumanPark"),
        (Parked, "Cancel"),
        // Done, Replanned and Cancelled are terminal and have no rows at all.
    ]
}

/// Stable label for an event; payload reasons are collapsed but kinds are kept.
fn event_key(event: &JobEvent) -> String {
    match event {
        JobEvent::NeedsSatisfied => "NeedsSatisfied".to_owned(),
        JobEvent::Dispatched => "Dispatched".to_owned(),
        JobEvent::ReportReady => "ReportReady".to_owned(),
        JobEvent::RunEndedNoReport => "RunEndedNoReport".to_owned(),
        JobEvent::RunRateLimited { .. } => "RunRateLimited".to_owned(),
        JobEvent::Stalled { silent } => format!("Stalled({silent})"),
        JobEvent::KeeperNudge { .. } => "KeeperNudge".to_owned(),
        JobEvent::KeeperRestart => "KeeperRestart".to_owned(),
        JobEvent::KeeperReplan => "KeeperReplan".to_owned(),
        JobEvent::KeeperFail => "KeeperFail".to_owned(),
        JobEvent::GatesPassed => "GatesPassed".to_owned(),
        JobEvent::GatesFailed => "GatesFailed".to_owned(),
        JobEvent::VerifyPassed => "VerifyPassed".to_owned(),
        JobEvent::VerifyFailed => "VerifyFailed".to_owned(),
        JobEvent::FixDone { kind } => format!("FixDone({kind:?})"),
        JobEvent::FixFailed => "FixFailed".to_owned(),
        JobEvent::VerifierFailed => "VerifierFailed".to_owned(),
        JobEvent::MergeSucceeded => "MergeSucceeded".to_owned(),
        JobEvent::MergeConflict => "MergeConflict".to_owned(),
        JobEvent::MergeChecksFailed => "MergeChecksFailed".to_owned(),
        JobEvent::Blocked { .. } => "Blocked".to_owned(),
        JobEvent::HumanPark => "HumanPark".to_owned(),
        JobEvent::Resolved => "Resolved".to_owned(),
        JobEvent::Retry => "Retry".to_owned(),
        JobEvent::Cancel => "Cancel".to_owned(),
    }
}

#[test]
fn invalid_pairs_are_rejected_for_every_state() {
    let allowed = allowed_pairs();
    let events = representative_events();
    for state in all_states() {
        let job_row = job("invalid-pairs", state, JobKind::Build);
        for event in &events {
            let key = event_key(event);
            let expected_valid = allowed.contains(&(state, key.as_str()));
            match transition(&job_row, event) {
                Ok(transition) => {
                    assert!(
                        expected_valid,
                        "{state:?} + {key} is not in the table but returned {transition:?}"
                    );
                }
                Err(GraphError::InvalidTransition {
                    state: actual_state,
                    event: actual_event,
                }) => {
                    assert!(!expected_valid, "{state:?} + {key} is in the table but was rejected");
                    assert_eq!(actual_state, state);
                    assert_eq!(actual_event, format!("{event:?}"));
                }
                Err(other) => panic!("unexpected error for {state:?} + {key}: {other}"),
            }
        }
    }
}

/// Returns the job kind at a generated index.
fn kind_at(index: u8) -> JobKind {
    match index % 12 {
        0 => JobKind::Plan,
        1 => JobKind::Spec,
        2 => JobKind::Build,
        3 => JobKind::Test,
        4 => JobKind::Verify,
        5 => JobKind::Fix,
        6 => JobKind::Merge,
        7 => JobKind::Docs,
        8 => JobKind::Research,
        9 => JobKind::Promote,
        10 => JobKind::Release,
        _ => JobKind::Decision,
    }
}

/// Returns the parked reason at a generated index.
fn reason_at(index: u8) -> ParkedReason {
    match index % 6 {
        0 => ParkedReason::Credits,
        1 => ParkedReason::Auth,
        2 => ParkedReason::Question,
        3 => ParkedReason::ProviderBlocked,
        4 => ParkedReason::Manual,
        _ => ParkedReason::Exhausted,
    }
}

/// Returns the state at a generated index.
fn state_at(index: usize) -> JobState {
    all_states()[index % 13]
}

/// Returns the loop exhaust policy at a generated index.
fn exhausted_at(index: u8) -> OnExhausted {
    match index % 3 {
        0 => OnExhausted::Escalate,
        1 => OnExhausted::Park,
        _ => OnExhausted::Fail,
    }
}

/// Generates any event shape the table defines.
fn arb_event() -> impl Strategy<Value = JobEvent> {
    (0u8..25, any::<bool>(), 0i64..2_000_000_000, 0u8..12, 0u8..6).prop_map(
        |(index, flag, secs, kind_index, reason_index)| match index {
            0 => JobEvent::NeedsSatisfied,
            1 => JobEvent::Dispatched,
            2 => JobEvent::ReportReady,
            3 => JobEvent::RunEndedNoReport,
            4 => JobEvent::RunRateLimited {
                until: Timestamp::from_unix_secs(secs),
            },
            5 => JobEvent::Stalled { silent: flag },
            6 => JobEvent::KeeperNudge {
                text: flag.then(|| "carry on".to_owned()),
            },
            7 => JobEvent::KeeperRestart,
            8 => JobEvent::KeeperReplan,
            9 => JobEvent::KeeperFail,
            10 => JobEvent::GatesPassed,
            11 => JobEvent::GatesFailed,
            12 => JobEvent::VerifyPassed,
            13 => JobEvent::VerifyFailed,
            14 => JobEvent::FixDone {
                kind: kind_at(kind_index),
            },
            15 => JobEvent::FixFailed,
            16 => JobEvent::VerifierFailed,
            17 => JobEvent::MergeSucceeded,
            18 => JobEvent::MergeConflict,
            19 => JobEvent::MergeChecksFailed,
            20 => JobEvent::Blocked {
                reason: reason_at(reason_index),
            },
            21 => JobEvent::HumanPark,
            22 => JobEvent::Resolved,
            23 => JobEvent::Retry,
            _ => JobEvent::Cancel,
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

    #[test]
    fn prop_terminal_states_accept_nothing(
        state in prop::sample::select(vec![JobState::Done, JobState::Replanned, JobState::Cancelled]),
        event in arb_event(),
    ) {
        let job_row = job("prop-terminal", state, JobKind::Build);
        match transition(&job_row, &event) {
            Err(GraphError::InvalidTransition { state: actual, .. }) => prop_assert_eq!(actual, state),
            other => prop_assert!(false, "terminal state {state:?} accepted {event:?}: {other:?}"),
        }
    }

    #[test]
    fn prop_transition_never_panics_and_is_pure(
        shape in (
            0usize..13,
            0u8..12,
            0u32..6,
            0u32..8,
            0u32..6,
            0u32..6,
            0u8..3,
            any::<bool>(),
        ),
        event in arb_event(),
    ) {
        let (state, kind, continues, hang_restarts, attempts, max_attempts, exhausted, review) = shape;
        let mut job_row = job("prop-pure", state_at(state), kind_at(kind));
        job_row.continues = continues;
        job_row.hang_restarts = hang_restarts;
        job_row.attempts = attempts;
        job_row.loop_.max_attempts = max_attempts;
        job_row.loop_.on_exhausted = exhausted_at(exhausted);
        job_row.gates = if review {
            vec![Gate::Review(ReviewGate { by: Role::Verifier })]
        } else {
            Vec::new()
        };
        let snapshot = job_row.clone();

        let first = transition(&job_row, &event);
        let second = transition(&job_row, &event);
        match (first, second) {
            (Ok(left), Ok(right)) => prop_assert_eq!(left, right),
            (Err(left), Err(right)) => prop_assert_eq!(left.to_string(), right.to_string()),
            (left, right) => prop_assert!(false, "non-deterministic results: {left:?} vs {right:?}"),
        }
        prop_assert_eq!(job_row, snapshot, "transition must not mutate the job");
    }
}
