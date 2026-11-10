//! Stall judging, the repeated-action guard and the fix-round counter helpers.
//! Spec: `docs/design/crates/vowp-graph.md` §8 and §10.

use std::time::Duration;

use vowp_graph::guards::{StallInput, StallJudge, StallThresholds, StallVerdict, attempts_left, is_exhausted};
use vowp_graph::{RepeatGuard, RepeatVerdict};
use vowp_protocol::{Job, JobId, JobKind, JobLoop, JobState, ProjectName, Role, Timestamp};

/// Builds a fixed timestamp.
fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix_secs(secs)
}

/// Returns `now` minus whole seconds.
fn ago(now: Timestamp, secs: u64) -> Timestamp {
    now.minus(Duration::from_secs(secs))
}

/// Builds a job row with explicit fix-round counters.
fn job(max_attempts: u32, attempts: u32) -> Job {
    let id = JobId::parse("attempts").expect("test job id is valid");
    let project = ProjectName::parse("vowp").expect("test project name is valid");
    let loop_ = JobLoop {
        max_attempts,
        ..JobLoop::default()
    };
    Job {
        id: id.clone(),
        project,
        title: "attempts".to_owned(),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: "worker".to_owned(),
        state: JobState::Running,
        priority: 50,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        brief: format!(".vowp/jobs/{id}/brief.md"),
        report: format!(".vowp/jobs/{id}/report.md"),
        gates: Vec::new(),
        loop_,
        branch: Some(format!("job/{id}")),
        worktree: Some(format!("/tmp/wt/{id}")),
        attempts,
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

#[test]
fn stall_judge_matrix() {
    type StallCase = (u64, Option<u64>, Option<u64>, u64, StallVerdict);

    let now = at(1_700_000_000);
    let thresholds = StallThresholds::default();
    assert_eq!(thresholds.stall_minutes, 20);
    assert_eq!(thresholds.silent_minutes, 8);

    let minute = 60;
    // (output bytes, event age, commit age, run age) -> verdict
    let cases: &[StallCase] = &[
        (1, Some(19 * minute), None, 120 * minute, StallVerdict::Healthy),
        (1, Some(19 * minute + 59), None, 120 * minute, StallVerdict::Healthy),
        (
            1,
            Some(20 * minute),
            None,
            120 * minute,
            StallVerdict::Stalled {
                idle_minutes: 20,
                silent: false,
            },
        ),
        (0, Some(7 * minute), None, 120 * minute, StallVerdict::Healthy),
        (0, Some(7 * minute + 59), None, 120 * minute, StallVerdict::Healthy),
        (
            0,
            Some(8 * minute),
            None,
            120 * minute,
            StallVerdict::Stalled {
                idle_minutes: 8,
                silent: true,
            },
        ),
        (
            1,
            None,
            None,
            30 * minute,
            StallVerdict::Stalled {
                idle_minutes: 30,
                silent: false,
            },
        ),
        (1, None, Some(5 * minute), 30 * minute, StallVerdict::Healthy),
        (
            1,
            Some(30 * minute),
            Some(2 * minute),
            30 * minute,
            StallVerdict::Healthy,
        ),
        (
            1,
            Some(30 * minute),
            None,
            30 * minute,
            StallVerdict::Stalled {
                idle_minutes: 30,
                silent: false,
            },
        ),
        (1, Some(0), None, 120 * minute, StallVerdict::Healthy),
        (0, Some(0), Some(0), 120 * minute, StallVerdict::Healthy),
    ];

    for &(output_bytes, event_age, commit_age, run_age, ref expected) in cases {
        let input = StallInput {
            now,
            run_started_at: ago(now, run_age),
            last_event_at: event_age.map(|secs| ago(now, secs)),
            last_commit_at: commit_age.map(|secs| ago(now, secs)),
            output_bytes,
        };
        assert_eq!(
            StallJudge::judge(&input, &thresholds),
            *expected,
            "output={output_bytes} event={event_age:?} commit={commit_age:?} run={run_age}"
        );
    }

    let custom = StallThresholds {
        stall_minutes: 2,
        silent_minutes: 1,
    };
    let input = StallInput {
        now,
        run_started_at: ago(now, 600),
        last_event_at: Some(ago(now, 90)),
        last_commit_at: None,
        output_bytes: 1,
    };
    assert_eq!(StallJudge::judge(&input, &custom), StallVerdict::Healthy);
    let input = StallInput {
        output_bytes: 0,
        ..input
    };
    assert_eq!(
        StallJudge::judge(&input, &custom),
        StallVerdict::Stalled {
            idle_minutes: 1,
            silent: true,
        }
    );
}

#[test]
fn stall_judge_commit_resets_idle() {
    let now = at(1_700_000_000);
    let thresholds = StallThresholds::default();
    let no_commit = StallInput {
        now,
        run_started_at: ago(now, 40 * 60),
        last_event_at: Some(ago(now, 30 * 60)),
        last_commit_at: None,
        output_bytes: 100,
    };
    assert_eq!(
        StallJudge::judge(&no_commit, &thresholds),
        StallVerdict::Stalled {
            idle_minutes: 30,
            silent: false,
        }
    );

    let with_commit = StallInput {
        last_commit_at: Some(ago(now, 2 * 60)),
        ..no_commit
    };
    assert_eq!(StallJudge::judge(&with_commit, &thresholds), StallVerdict::Healthy);
}

#[test]
fn repeat_guard_trips_on_third_identical() {
    let mut guard = RepeatGuard::new(3);
    assert_eq!(guard.observe("job:continue", "tail-hash-a"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:continue", "tail-hash-a"), RepeatVerdict::Ok);
    assert_eq!(
        guard.observe("job:continue", "tail-hash-a"),
        RepeatVerdict::Tripped { count: 3 }
    );
    assert_eq!(
        guard.observe("job:continue", "tail-hash-a"),
        RepeatVerdict::Tripped { count: 4 },
        "further identical observations keep tripping"
    );

    let mut default_guard = RepeatGuard::default();
    assert_eq!(default_guard.observe("k", "f"), RepeatVerdict::Ok);
    assert_eq!(default_guard.observe("k", "f"), RepeatVerdict::Ok);
    assert_eq!(default_guard.observe("k", "f"), RepeatVerdict::Tripped { count: 3 });
}

#[test]
fn repeat_guard_resets_on_new_fingerprint() {
    let mut guard = RepeatGuard::new(3);
    assert_eq!(guard.observe("job:gate", "hash-a"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:gate", "hash-a"), RepeatVerdict::Ok);
    assert_eq!(
        guard.observe("job:gate", "hash-b"),
        RepeatVerdict::Ok,
        "a different fingerprint starts a fresh run"
    );
    assert_eq!(guard.observe("job:gate", "hash-b"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:gate", "hash-b"), RepeatVerdict::Tripped { count: 3 });

    guard.reset();
    assert_eq!(guard.observe("job:gate", "hash-b"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:gate", "hash-b"), RepeatVerdict::Ok);
}

#[test]
fn repeat_guard_resets_on_new_key() {
    let mut guard = RepeatGuard::new(3);
    assert_eq!(guard.observe("job:a", "same"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:a", "same"), RepeatVerdict::Ok);
    assert_eq!(
        guard.observe("job:b", "same"),
        RepeatVerdict::Ok,
        "a different key starts a fresh run"
    );
    assert_eq!(guard.observe("job:a", "same"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:a", "same"), RepeatVerdict::Ok);
    assert_eq!(guard.observe("job:a", "same"), RepeatVerdict::Tripped { count: 3 });
}

#[test]
fn attempts_left_saturates() {
    assert_eq!(attempts_left(&job(3, 0)), 3);
    assert_eq!(attempts_left(&job(3, 2)), 1);
    assert_eq!(attempts_left(&job(3, 3)), 0);
    assert_eq!(attempts_left(&job(3, 4)), 0, "attempts past the bound stay at zero");
    assert_eq!(attempts_left(&job(0, 0)), 0);

    assert!(!is_exhausted(&job(3, 2)));
    assert!(is_exhausted(&job(3, 3)));
    assert!(is_exhausted(&job(3, 9)));
    assert!(is_exhausted(&job(0, 0)));
}
