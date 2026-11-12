//! CLI-to-supervisor requests stored as `kv` rows and applied by the supervisor or CLI.
//! Spec: `docs/design/crates/vowp-core.md` §2 and `docs/design/loops.md` §1 step 0.
//!
//! A request is the JSON value `{"kind": "<snake_case>", "payload": {…}}` stored under the
//! `kv` key `req:<ULID>` by [`push`]. [`drain`] consumes the queue in id order; rows that no
//! longer decode are dropped and recorded as a ledger `note` row. [`apply_direct`] is the
//! no-supervisor path used by the CLI: it applies the same state-machine event directly.

use serde::{Deserialize, Serialize};
use vowp_graph::{Effect, JobEvent, transition};
use vowp_protocol::{ActiveState, InboxId, InboxState, JobId, LedgerEntry, LedgerKind, Level};
use vowp_store::{RequestId, StateChange, Store};

use crate::{CoreError, Deps};

/// One command from the CLI to the supervisor.
///
/// The wire form is the `kv` value: `{"kind":"retry_job","payload":{"job":"example-job"}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Request {
    /// Move a parked job back to `ready` (`Resolved`, T31).
    ResolveJob {
        /// Job to resolve.
        job: JobId,
    },
    /// Run one loop's step on the next tick.
    RunLoop {
        /// Loop name, for example `l4-planner`.
        name: String,
    },
    /// Retry a failed or stalled job.
    RetryJob {
        /// Job to retry.
        job: JobId,
    },
    /// Park a job until a human resolves it.
    ParkJob {
        /// Job to park.
        job: JobId,
        /// Why the job was parked; stored as the job note.
        reason: String,
    },
    /// Cancel a job.
    CancelJob {
        /// Job to cancel.
        job: JobId,
    },
    /// Ask a stalled job to continue without a restart.
    NudgeJob {
        /// Job to nudge.
        job: JobId,
    },
    /// Restart a stalled job with a fresh process.
    RestartJob {
        /// Job to restart.
        job: JobId,
    },
    /// Have the planner replace a job.
    ReplanJob {
        /// Job to replan.
        job: JobId,
    },
    /// Give up on a stalled job and mark it failed.
    FailJob {
        /// Job to fail.
        job: JobId,
        /// Why the job failed; stored as the job note.
        reason: String,
    },
    /// Deliver an inbox answer to the parked job it belongs to.
    AnswerInbox {
        /// Answered inbox item.
        id: InboxId,
    },
    /// Pause a loop (never `l1-supervisor`).
    PauseLoop {
        /// Loop name.
        name: String,
    },
    /// Resume a paused loop.
    ResumeLoop {
        /// Loop name.
        name: String,
    },
    /// End the supervisor's idle wait without any other effect.
    Wake,
}

/// Serialises `req` and enqueues it as one `req:<ULID>` row.
pub fn push(store: &Store, req: &Request) -> Result<RequestId, CoreError> {
    let value = serde_json::to_value(req).map_err(vowp_protocol::ProtocolError::from)?;
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CoreError::Invalid("serialised request has no kind".to_owned()))?
        .to_owned();
    let payload = value.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    Ok(store.push_request(&kind, &payload)?)
}

/// Drains every queued request in id order.
///
/// Undecodable rows are removed from the queue by the store and recorded as a ledger `note`
/// row so an operator can see that something was dropped.
pub fn drain(store: &Store) -> Result<Vec<(RequestId, Request)>, CoreError> {
    let mut requests = Vec::new();
    for stored in store.take_requests()? {
        match decode(&stored.kind, &stored.payload) {
            Ok(request) => requests.push((stored.id, request)),
            Err(error) => log_dropped(store, &stored.id, &stored.kind, &error)?,
        }
    }
    Ok(requests)
}

/// Applies one request directly, as the CLI does when no supervisor is running.
///
/// Job requests go through the same [`vowp_graph::transition`] table as the supervisor. Only
/// effects that change the job row (state, counters, parked reason) are applied; effects that
/// need a live supervisor (runs, gates, worktrees, inbox items) wait for the next tick.
pub fn apply_direct(deps: &Deps, store: &Store, req: &Request) -> Result<(), CoreError> {
    match req {
        Request::ResolveJob { job } => apply_job_event(store, job, &JobEvent::Resolved, None),
        Request::RetryJob { job } => {
            let current = store.require_job(job)?;
            let event = match current.state {
                vowp_protocol::JobState::Failed => JobEvent::Retry,
                vowp_protocol::JobState::Stalled => JobEvent::KeeperRestart,
                other => {
                    return Err(CoreError::Invalid(format!(
                        "retry is not allowed in state {}",
                        other.as_str()
                    )));
                }
            };
            apply_job_event(store, job, &event, None)
        }
        Request::ParkJob { job, reason } => apply_job_event(store, job, &JobEvent::HumanPark, Some(reason.clone())),
        Request::CancelJob { job } => apply_job_event(store, job, &JobEvent::Cancel, None),
        Request::NudgeJob { job } => apply_job_event(store, job, &JobEvent::KeeperNudge { text: None }, None),
        Request::RestartJob { job } => apply_job_event(store, job, &JobEvent::KeeperRestart, None),
        Request::ReplanJob { job } => apply_job_event(store, job, &JobEvent::KeeperReplan, None),
        Request::FailJob { job, reason } => apply_job_event(store, job, &JobEvent::KeeperFail, Some(reason.clone())),
        Request::AnswerInbox { id } => apply_inbox_answer(store, id),
        Request::PauseLoop { name } => set_loop_state(store, name, ActiveState::Paused),
        Request::ResumeLoop { name } => set_loop_state(store, name, ActiveState::Active),
        Request::RunLoop { name } => run_loop_now(store, name),
        Request::Wake => Ok(()),
    }?;
    deps.log
        .log(Level::Info, "requests", &format!("applied {}", kind_name(req)));
    Ok(())
}

/// Decodes one stored `(kind, payload)` pair.
fn decode(kind: &str, payload: &serde_json::Value) -> Result<Request, CoreError> {
    let value = serde_json::json!({ "kind": kind, "payload": payload });
    serde_json::from_value(value).map_err(|error| CoreError::Invalid(format!("{kind}: {error}")))
}

/// Records one dropped request as an append-only ledger note.
fn log_dropped(store: &Store, id: &RequestId, kind: &str, error: &CoreError) -> Result<(), CoreError> {
    let entry = LedgerEntry {
        id: None,
        ts: store.clock().now(),
        kind: LedgerKind::Note,
        project: None,
        job_id: None,
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("dropped undecodable request {kind}"),
        detail: serde_json::json!({
            "request": id.0,
            "kind": kind,
            "error": error.to_string(),
        }),
    };
    store.append_ledger(&entry)?;
    Ok(())
}

/// Applies one state-machine event and the effects that touch the job row.
///
/// The state change, counter updates and the ledger `state` row are written in one transaction.
fn apply_job_event(store: &Store, job_id: &JobId, event: &JobEvent, note: Option<String>) -> Result<(), CoreError> {
    let job = store.require_job(job_id)?;
    let outcome = transition(&job, event)?;
    let event_name = event_name(event);

    let mut attempts = job.attempts;
    let mut continues = job.continues;
    let mut hang_restarts = job.hang_restarts;
    let mut counters_changed = false;
    let mut parked_reason = None;
    for effect in &outcome.effects {
        match effect {
            Effect::ResetCounters => {
                attempts = 0;
                continues = 0;
                hang_restarts = 0;
                counters_changed = true;
            }
            Effect::ResetContinues => {
                continues = 0;
                counters_changed = true;
            }
            Effect::IncrementAttempts => {
                attempts = attempts.saturating_add(1);
                counters_changed = true;
            }
            Effect::IncrementContinues => {
                continues = continues.saturating_add(1);
                counters_changed = true;
            }
            Effect::IncrementHangRestarts => {
                hang_restarts = hang_restarts.saturating_add(1);
                counters_changed = true;
            }
            Effect::SetParkedReason(reason) => parked_reason = Some(*reason),
            _ => {}
        }
    }

    store.transaction(|tx| {
        let from = tx.set_job_state(
            job_id,
            outcome.to,
            &StateChange {
                note: note.clone(),
                parked_reason,
            },
        )?;
        if counters_changed {
            tx.set_job_counters(job_id, Some(attempts), Some(continues), Some(hang_restarts))?;
        }
        let entry = LedgerEntry {
            id: None,
            ts: tx.clock().now(),
            kind: LedgerKind::State,
            project: Some(job.project.clone()),
            job_id: Some(job_id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("{job_id}: {} -> {} ({event_name})", from.as_str(), outcome.to.as_str()),
            detail: serde_json::json!({
                "from": from.as_str(),
                "to": outcome.to.as_str(),
                "why": event_name,
            }),
        };
        tx.append_ledger(&entry)?;
        Ok(())
    })?;
    Ok(())
}

/// Returns the canonical event name used in ledger `state` summaries.
fn event_name(event: &JobEvent) -> String {
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

/// Resolves the parked job of an answered inbox item.
fn apply_inbox_answer(store: &Store, id: &InboxId) -> Result<(), CoreError> {
    let item = store
        .inbox_item(id)?
        .ok_or_else(|| CoreError::NotFound(format!("inbox item {id}")))?;
    if item.state != InboxState::Answered {
        return Err(CoreError::Invalid(format!("inbox item {id} has not been answered")));
    }
    let Some(job_id) = item.job_id else {
        return Ok(());
    };
    let job = store.require_job(&job_id)?;
    if job.state == vowp_protocol::JobState::Parked {
        apply_job_event(store, &job_id, &JobEvent::Resolved, None)?;
    }
    Ok(())
}

/// Pauses or resumes a loop.
fn set_loop_state(store: &Store, name: &str, state: ActiveState) -> Result<(), CoreError> {
    if name == "l1-supervisor" {
        return Err(CoreError::Invalid(
            "the l1-supervisor loop cannot be paused or resumed".to_owned(),
        ));
    }
    store.set_loop_state(name, state).map_err(|error| match error {
        vowp_store::StoreError::NotFound { .. } => CoreError::NotFound(format!("loop {name}")),
        other => CoreError::Store(other),
    })
}

/// Makes one loop due immediately by stamping `next_run_at = now`.
fn run_loop_now(store: &Store, name: &str) -> Result<(), CoreError> {
    let mut loop_ = store
        .loop_(name)?
        .ok_or_else(|| CoreError::NotFound(format!("loop {name}")))?;
    let now = store.clock().now();
    loop_.next_run_at = Some(now);
    loop_.updated_at = now;
    store.upsert_loop(&loop_)?;
    Ok(())
}

/// Returns the snake-case kind name used in log lines.
fn kind_name(req: &Request) -> &'static str {
    match req {
        Request::ResolveJob { .. } => "resolve_job",
        Request::RunLoop { .. } => "run_loop",
        Request::RetryJob { .. } => "retry_job",
        Request::ParkJob { .. } => "park_job",
        Request::CancelJob { .. } => "cancel_job",
        Request::NudgeJob { .. } => "nudge_job",
        Request::RestartJob { .. } => "restart_job",
        Request::ReplanJob { .. } => "replan_job",
        Request::FailJob { .. } => "fail_job",
        Request::AnswerInbox { .. } => "answer_inbox",
        Request::PauseLoop { .. } => "pause_loop",
        Request::ResumeLoop { .. } => "resume_loop",
        Request::Wake => "wake",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vowp_protocol::InboxId;
    use vowp_store::Store;

    #[test]
    fn push_encodes_answer_inbox_with_the_inbox_id() {
        let store = Store::open_memory().expect("memory store");
        let id = InboxId::new();
        let request_id = push(&store, &Request::AnswerInbox { id: id.clone() }).expect("push request");
        let stored = store.take_requests().expect("take requests");
        assert_eq!(stored.len(), 1, "push must write exactly one request row");
        assert_eq!(stored[0].id, request_id);
        assert_eq!(stored[0].kind, "answer_inbox");
        assert_eq!(stored[0].payload, serde_json::json!({ "id": id.as_str() }));
        let decoded = decode(&stored[0].kind, &stored[0].payload).expect("decode request");
        assert_eq!(decoded, Request::AnswerInbox { id });
    }
}
