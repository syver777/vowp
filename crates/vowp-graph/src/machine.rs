//! The job state machine: [`JobEvent`], [`Effect`] and [`transition`].
//! Spec: `docs/design/loops.md` §2 (rows T1–T37) and `docs/design/crates/vowp-graph.md` §3.
//!
//! [`transition`] is a pure function of the job row and the event. It never mutates the job,
//! never reads a clock and never performs I/O: the caller applies the returned effects and
//! the new state. Every `(state, event)` pair that the table does not list is
//! [`GraphError::InvalidTransition`]; terminal states accept nothing.

use vowp_protocol::{Gate, InboxKind, Job, JobKind, JobState, OnExhausted, ParkedReason, RunPurpose, Timestamp};

use crate::error::GraphError;

/// Maximum automatic continues per attempt, unless [`TransitionBounds`] overrides it.
pub const MAX_CONTINUES: u32 = 2;

/// Maximum restarts of a run that hung before producing output.
pub const MAX_HANG_RESTARTS: u32 = 3;

/// Events the supervisor feeds into [`transition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    /// Every `needs` target is done (T1; emitted by the tick's readiness check).
    NeedsSatisfied,
    /// A worker slot and executor were assigned (T2).
    Dispatched,
    /// The worker wrote a valid report (T3).
    ReportReady,
    /// A run ended without a valid report (T4/T5).
    RunEndedNoReport,
    /// The provider rate-limited a running job; the continue run waits until `until` (T35).
    RunRateLimited {
        /// First instant the continue run may start.
        until: Timestamp,
    },
    /// Stall detection fired; `silent` means the run produced no output at all (T6–T9).
    Stalled {
        /// True when the run has produced zero output bytes.
        silent: bool,
    },
    /// The keeper asked a stalled job to continue with a nudge (T10).
    KeeperNudge {
        /// Optional keeper text; v0.1 continues run without extra text.
        text: Option<String>,
    },
    /// The keeper asked a stalled job to restart with a fresh process (T11).
    KeeperRestart,
    /// The keeper asked for a replan (T12/T34).
    KeeperReplan,
    /// The keeper gave up on a stalled job (T13).
    KeeperFail,
    /// Every command gate passed (T14–T16, T36).
    GatesPassed,
    /// At least one command gate failed (T17/T18/T36).
    GatesFailed,
    /// The verifier passed the work (T19).
    VerifyPassed,
    /// The verifier failed the work (T20/T21).
    VerifyFailed,
    /// A fix or merge job finished; `kind` is the finished child's kind (T22/T23).
    FixDone {
        /// Kind of the finished fix job: [`JobKind::Fix`] or [`JobKind::Merge`].
        kind: JobKind,
    },
    /// The fix or merge job ended failed (T24).
    FixFailed,
    /// The verify job failed or produced no usable verdict (T37).
    VerifierFailed,
    /// The merge queue merged the job (T25).
    MergeSucceeded,
    /// The merge queue hit a rebase conflict (T26/T28).
    MergeConflict,
    /// The repo-wide checks failed in the merge queue (T27/T28).
    MergeChecksFailed,
    /// A limit or auth problem parked the job (T29).
    Blocked {
        /// Why the job is parked.
        reason: ParkedReason,
    },
    /// A human parked the job (T30).
    HumanPark,
    /// A parked job was resolved and may run again (T31).
    Resolved,
    /// A failed job was retried (T32).
    Retry,
    /// A human cancelled the job (T33).
    Cancel,
}

/// Side effects the caller must perform; the machine never does I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Create the job's worktree and branch; verify/fix/merge children reuse the parent's.
    CreateWorktree,
    /// Start a harness run with the given purpose.
    StartRun {
        /// Why the run starts (start, continue or fix).
        purpose: RunPurpose,
    },
    /// Start a run only once `now >= until` (T35).
    DelayRun {
        /// Purpose of the delayed run.
        purpose: RunPurpose,
        /// First instant the run may start.
        until: Timestamp,
    },
    /// Stop the job's live run.
    StopRun,
    /// Increment the job's `continues` counter.
    IncrementContinues,
    /// Reset the job's `continues` counter to zero.
    ResetContinues,
    /// Increment the job's `hang_restarts` counter.
    IncrementHangRestarts,
    /// Increment the job's `attempts` counter.
    IncrementAttempts,
    /// Reset `attempts`, `continues` and `hang_restarts` to zero.
    ResetCounters,
    /// Run the job's command gates.
    RunGates,
    /// Create and dispatch the verify job for this attempt.
    StartVerify,
    /// Create a fix or merge child job.
    CreateFixJob {
        /// Child kind to create.
        kind: JobKind,
    },
    /// Put the job on the merge queue.
    EnqueueMerge,
    /// Remove the job's worktree after a successful merge.
    CleanupWorktree,
    /// Let dependent jobs re-evaluate their readiness.
    UnblockDependents,
    /// Create a decision job that escalates this job to a human.
    CreateDecisionJob,
    /// Add an item to the human inbox.
    NotifyInbox {
        /// Inbox item kind.
        kind: InboxKind,
    },
    /// Deliver the child's completion event to the job it fixes or verifies.
    NotifyParent,
    /// Wake the keeper.
    WakeKeeper,
    /// Wake the planner.
    WakePlanner,
    /// Store the reason a job is parked.
    SetParkedReason(ParkedReason),
    /// Clear the stored parked reason.
    ClearParkedReason,
}

/// The state a transition moves to plus the effects the caller must apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// New job state.
    pub to: JobState,
    /// Ordered side effects.
    pub effects: Vec<Effect>,
}

/// Bounds the guards use. `Default` is `MAX_CONTINUES` and `MAX_HANG_RESTARTS`.
///
/// `max_attempts` is not here: it comes from `job.loop_`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionBounds {
    /// Automatic continues allowed per attempt.
    pub max_continues: u32,
    /// Silent-run restarts allowed.
    pub max_hang_restarts: u32,
}

impl Default for TransitionBounds {
    fn default() -> Self {
        Self {
            max_continues: MAX_CONTINUES,
            max_hang_restarts: MAX_HANG_RESTARTS,
        }
    }
}

/// Applies the state table with the default [`TransitionBounds`].
pub fn transition(job: &Job, ev: &JobEvent) -> Result<Transition, GraphError> {
    transition_with(job, ev, &TransitionBounds::default())
}

/// Applies the state table with explicit bounds.
///
/// Unlisted `(state, event)` pairs, including every event on a terminal state, return
/// [`GraphError::InvalidTransition`].
pub fn transition_with(job: &Job, ev: &JobEvent, bounds: &TransitionBounds) -> Result<Transition, GraphError> {
    if job.state.is_terminal() {
        return Err(invalid(job, ev));
    }

    match (job.state, ev) {
        // T1
        (JobState::Pending, JobEvent::NeedsSatisfied) => done(JobState::Ready, Vec::new()),

        // T2
        (JobState::Ready, JobEvent::Dispatched) => {
            // T2's "already has a session / prior run" fact is not part of the pure-function
            // inputs listed in crates/vowp-graph.md §3 rule 1; only `continues` is observable
            // here, so a caller resuming a prior run overrides the purpose itself.
            let purpose = if matches!(job.kind, JobKind::Fix | JobKind::Merge) {
                RunPurpose::Fix
            } else if job.continues > 0 {
                RunPurpose::Continue
            } else {
                RunPurpose::Start
            };
            done(
                JobState::Running,
                vec![Effect::CreateWorktree, Effect::StartRun { purpose }],
            )
        }

        // T3
        (JobState::Running, JobEvent::ReportReady) => done(JobState::Gating, vec![Effect::RunGates]),

        // T4/T5
        (JobState::Running, JobEvent::RunEndedNoReport) => {
            if job.continues < bounds.max_continues {
                done(
                    JobState::Running,
                    vec![
                        Effect::IncrementContinues,
                        Effect::StartRun {
                            purpose: RunPurpose::Continue,
                        },
                    ],
                )
            } else {
                done(JobState::Stalled, vec![Effect::WakeKeeper])
            }
        }

        // T6/T7
        (JobState::Running, JobEvent::Stalled { silent: false }) => {
            if job.continues < bounds.max_continues {
                done(
                    JobState::Running,
                    vec![
                        Effect::StopRun,
                        Effect::IncrementContinues,
                        Effect::StartRun {
                            purpose: RunPurpose::Continue,
                        },
                    ],
                )
            } else {
                done(JobState::Stalled, vec![Effect::StopRun, Effect::WakeKeeper])
            }
        }

        // T8/T9
        (JobState::Running, JobEvent::Stalled { silent: true }) => {
            if job.hang_restarts < bounds.max_hang_restarts {
                done(
                    JobState::Running,
                    vec![
                        Effect::StopRun,
                        Effect::IncrementHangRestarts,
                        Effect::StartRun {
                            purpose: RunPurpose::Start,
                        },
                    ],
                )
            } else {
                done(JobState::Stalled, vec![Effect::StopRun, Effect::WakeKeeper])
            }
        }

        // T35
        (JobState::Running, JobEvent::RunRateLimited { until }) => done(
            JobState::Running,
            vec![Effect::DelayRun {
                purpose: RunPurpose::Continue,
                until: *until,
            }],
        ),

        // T10
        (JobState::Stalled, JobEvent::KeeperNudge { .. }) => done(
            JobState::Running,
            vec![
                Effect::ResetContinues,
                Effect::StartRun {
                    purpose: RunPurpose::Continue,
                },
            ],
        ),

        // T11
        (JobState::Stalled, JobEvent::KeeperRestart) => done(
            JobState::Running,
            vec![
                Effect::ResetContinues,
                Effect::StartRun {
                    purpose: RunPurpose::Start,
                },
            ],
        ),

        // T12
        (JobState::Stalled, JobEvent::KeeperReplan) => done(JobState::Replanned, vec![Effect::WakePlanner]),

        // T13
        (JobState::Stalled, JobEvent::KeeperFail) => {
            let mut effects = vec![Effect::NotifyInbox {
                kind: InboxKind::Notice,
            }];
            if is_child_kind(job.kind) {
                effects.push(Effect::NotifyParent);
            }
            done(JobState::Failed, effects)
        }

        // T14/T15/T16
        (JobState::Gating, JobEvent::GatesPassed) => {
            if is_child_kind(job.kind) {
                done(JobState::Done, vec![Effect::NotifyParent])
            } else if has_review_gate(job) {
                done(JobState::Reviewing, vec![Effect::StartVerify])
            } else {
                done(JobState::Merging, vec![Effect::EnqueueMerge])
            }
        }

        // T36 (before T17/T18: a child job never creates a fix-of-fix)
        (JobState::Gating, JobEvent::GatesFailed) if is_child_kind(job.kind) => done(
            JobState::Failed,
            vec![
                Effect::NotifyParent,
                Effect::NotifyInbox {
                    kind: InboxKind::Notice,
                },
            ],
        ),

        // T17/T18
        (JobState::Gating, JobEvent::GatesFailed) => {
            if attempts_left(job) > 0 {
                done(
                    JobState::Fixing,
                    vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix }],
                )
            } else {
                exhausted(job)
            }
        }

        // T19
        (JobState::Reviewing, JobEvent::VerifyPassed) => done(JobState::Merging, vec![Effect::EnqueueMerge]),

        // T20/T21
        (JobState::Reviewing, JobEvent::VerifyFailed) => {
            if attempts_left(job) > 0 {
                done(
                    JobState::Fixing,
                    vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix }],
                )
            } else {
                exhausted(job)
            }
        }

        // T37
        (JobState::Reviewing, JobEvent::VerifierFailed) => done(
            JobState::Parked,
            vec![
                Effect::SetParkedReason(ParkedReason::Question),
                Effect::NotifyInbox {
                    kind: InboxKind::Question,
                },
            ],
        ),

        // T22
        (JobState::Fixing, JobEvent::FixDone { kind: JobKind::Fix }) => {
            done(JobState::Gating, vec![Effect::ResetContinues, Effect::RunGates])
        }

        // T23
        (JobState::Fixing, JobEvent::FixDone { kind: JobKind::Merge }) => {
            done(JobState::Merging, vec![Effect::EnqueueMerge])
        }

        // T24
        (JobState::Fixing, JobEvent::FixFailed) => done(
            JobState::Failed,
            vec![
                Effect::NotifyInbox {
                    kind: InboxKind::Notice,
                },
                Effect::WakeKeeper,
            ],
        ),

        // T25
        (JobState::Merging, JobEvent::MergeSucceeded) => {
            done(JobState::Done, vec![Effect::CleanupWorktree, Effect::UnblockDependents])
        }

        // T26/T27/T28
        (JobState::Merging, JobEvent::MergeConflict) => {
            if attempts_left(job) > 0 {
                done(
                    JobState::Fixing,
                    vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Merge }],
                )
            } else {
                exhausted(job)
            }
        }
        (JobState::Merging, JobEvent::MergeChecksFailed) => {
            if attempts_left(job) > 0 {
                done(
                    JobState::Fixing,
                    vec![Effect::IncrementAttempts, Effect::CreateFixJob { kind: JobKind::Fix }],
                )
            } else {
                exhausted(job)
            }
        }

        // T29
        (
            JobState::Pending | JobState::Ready | JobState::Running | JobState::Stalled | JobState::Failed,
            JobEvent::Blocked { reason },
        ) => {
            let mut effects = Vec::new();
            if job.state == JobState::Running {
                effects.push(Effect::StopRun);
            }
            if matches!(
                reason,
                ParkedReason::Question | ParkedReason::Auth | ParkedReason::Credits | ParkedReason::ProviderBlocked
            ) {
                effects.push(Effect::NotifyInbox {
                    kind: InboxKind::Question,
                });
            }
            effects.push(Effect::SetParkedReason(*reason));
            done(JobState::Parked, effects)
        }

        // T30
        (_, JobEvent::HumanPark) => {
            let mut effects = Vec::new();
            if job.state == JobState::Running {
                effects.push(Effect::StopRun);
            }
            effects.push(Effect::SetParkedReason(ParkedReason::Manual));
            done(JobState::Parked, effects)
        }

        // T31
        (JobState::Parked, JobEvent::Resolved) => {
            done(JobState::Ready, vec![Effect::ClearParkedReason, Effect::ResetContinues])
        }

        // T32
        (JobState::Failed, JobEvent::Retry) => done(JobState::Ready, vec![Effect::ResetCounters]),

        // T34
        (JobState::Parked | JobState::Failed, JobEvent::KeeperReplan) => {
            done(JobState::Replanned, vec![Effect::WakePlanner])
        }

        // T33
        (_, JobEvent::Cancel) => {
            let effects = if job.state == JobState::Running {
                vec![Effect::StopRun]
            } else {
                Vec::new()
            };
            done(JobState::Cancelled, effects)
        }

        _ => Err(invalid(job, ev)),
    }
}

/// Returns `loop_.max_attempts - attempts`, saturating at zero.
pub fn attempts_left(job: &Job) -> u32 {
    job.loop_.max_attempts.saturating_sub(job.attempts)
}

/// Returns true when the job has used every fix round.
pub fn is_exhausted(job: &Job) -> bool {
    attempts_left(job) == 0
}

/// Returns true for the kinds that report their result to a parent job.
fn is_child_kind(kind: JobKind) -> bool {
    matches!(kind, JobKind::Fix | JobKind::Merge | JobKind::Verify)
}

/// Returns true when the job carries any [`Gate::Review`] gate.
fn has_review_gate(job: &Job) -> bool {
    job.gates.iter().any(|gate| matches!(gate, Gate::Review(_)))
}

/// Builds the `on_exhausted` outcome shared by T18, T21 and T28.
fn exhausted(job: &Job) -> Result<Transition, GraphError> {
    match job.loop_.on_exhausted {
        OnExhausted::Escalate => done(
            JobState::Failed,
            vec![
                Effect::CreateDecisionJob,
                Effect::NotifyInbox {
                    kind: InboxKind::Decision,
                },
                Effect::WakeKeeper,
            ],
        ),
        OnExhausted::Park => done(
            JobState::Parked,
            vec![
                Effect::SetParkedReason(ParkedReason::Exhausted),
                Effect::NotifyInbox {
                    kind: InboxKind::Question,
                },
            ],
        ),
        OnExhausted::Fail => done(JobState::Failed, vec![Effect::WakeKeeper]),
    }
}

/// Builds one transition.
fn done(to: JobState, effects: Vec<Effect>) -> Result<Transition, GraphError> {
    Ok(Transition { to, effects })
}

/// Builds the error for an unlisted `(state, event)` pair.
fn invalid(job: &Job, ev: &JobEvent) -> GraphError {
    GraphError::InvalidTransition {
        state: job.state,
        event: format!("{ev:?}"),
    }
}
