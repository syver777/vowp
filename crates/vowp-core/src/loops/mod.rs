//! Supervisor loops that keep the job graph moving.
//!
//! Spec: `docs/design/loops.md` §1–3, §5–6 and §12, and
//! `docs/design/crates/vowp-core.md` §7.
//!
//! This module owns the L1 tick, the L2 worker start/continue/fix prompts, L3 verify
//! scheduling, the merge queue, dispatch, reconcile and the runner-thread pool. The shared
//! helpers here apply one job event exactly like `requests::apply_direct`: counters, parked
//! reason and the ledger `state` row (`{"from","to","why"}`) are written in one transaction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;
use vowp_graph::{Effect, JobEvent, TransitionBounds, transition_with};
use vowp_protocol::{
    InboxId, Job, JobId, JobState, LedgerEntry, LedgerKind, ParkedReason, ProjectName, SessionId, Timestamp,
};
use vowp_store::{SessionFilter, StateChange, Store};

use crate::CoreError;
use crate::deps::Deps;

pub mod digest;
pub mod dispatch;
pub mod l2_worker;
pub mod l3_verify;
pub mod l4_planner;
pub mod l5_keeper;
pub mod l6_failover;
pub mod l7_inbox;
pub mod l8_watchdog;
pub mod merge_queue;
pub mod prompts;
pub mod reconcile;
pub mod runner;
pub mod supervisor;

pub use runner::{RunnerPool, SessionRunPool, WakeHandle};
pub use supervisor::{ReconcileReport, StepReport, Supervisor, TickReport};

/// L6 failover helpers (spec: `loops.md` §8).
pub mod l6 {
    pub use super::l6_failover::{Failover, FailoverAction, ProbeVerdict, is_limit, provider_of, row_kind_for};
}

/// L7 inbox helpers (spec: `loops.md` §9).
pub mod l7 {
    pub use super::l7_inbox::{AnswerEffect, NewInboxItem, on_answer, open_inbox, record_answer};
}

/// L8 watchdog helpers (spec: `loops.md` §10).
pub mod l8 {
    pub use super::l8_watchdog::{CRON_CHECK_SH, STALE_AFTER_SECS, SYSTEMD_UNIT, heartbeat_age, write_heartbeat};
}

/// L2 prompt templates (spec: `loops.md` §3).
pub mod l2 {
    pub use super::l2_worker::{continue_prompt, fix_prompt, start_prompt};
}

/// L3 verify helpers (spec: `loops.md` §5).
pub mod l3 {
    pub use super::l3_verify::{on_verify_done, on_verify_done_with_findings, parse_verdict, verify_prompt};
}

/// One job event that was applied to a row.
#[derive(Debug, Clone)]
pub(crate) struct Applied {
    /// State before the transition.
    pub from: JobState,
    /// State after the transition.
    pub to: JobState,
    /// Effects the caller still has to perform.
    pub effects: Vec<Effect>,
}

/// Applies one job event and writes the state row, counters and ledger entry atomically.
///
/// This is the supervisor's copy of `requests::apply_direct`'s job path; both produce the
/// same ledger `state` detail shape `{"from","to","why"}`.
pub(crate) fn apply_event(
    store: &Store,
    job_id: &JobId,
    event: &JobEvent,
    bounds: TransitionBounds,
    note: Option<String>,
    parked_override: Option<ParkedReason>,
) -> Result<Applied, CoreError> {
    let job = store.require_job(job_id)?;
    let outcome = transition_with(&job, event, &bounds)?;
    let why = event_name(event);

    let mut attempts = job.attempts;
    let mut continues = job.continues;
    let mut hang_restarts = job.hang_restarts;
    let mut counters_changed = false;
    let mut parked_reason = parked_override;
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

    let from = store.transaction(|tx| {
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
            summary: format!("{job_id}: {} -> {} ({why})", from.as_str(), outcome.to.as_str()),
            detail: json!({
                "from": from.as_str(),
                "to": outcome.to.as_str(),
                "why": why,
            }),
        };
        tx.append_ledger(&entry)?;
        Ok(from)
    })?;

    Ok(Applied {
        from,
        to: outcome.to,
        effects: outcome.effects,
    })
}

/// Returns the canonical event name used in ledger `state` summaries.
pub(crate) fn event_name(event: &JobEvent) -> String {
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

/// Writes a ledger `state` row for a loop request (`pause_loop`, `resume_loop`, `run_loop`,
/// `wake`) as required by loops.md §1 step 0.
pub(crate) fn record_loop_request(
    store: &Store,
    project: &ProjectName,
    loop_name: &str,
    from: &str,
    to: &str,
    why: &str,
) -> Result<(), CoreError> {
    let entry = LedgerEntry {
        id: None,
        ts: store.clock().now(),
        kind: LedgerKind::State,
        project: Some(project.clone()),
        job_id: None,
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("loop {loop_name}: {from} -> {to} ({why})"),
        detail: json!({"from": from, "to": to, "why": why}),
    };
    store.append_ledger(&entry)?;
    Ok(())
}

/// Transport-neutral inbox item input (the L7 public type).
pub use l7_inbox::NewInboxItem as NewInbox;

/// Opens one inbox item and writes its Markdown copy under `<node>/inbox/`.
pub(crate) fn open_inbox(store: &Store, deps: &Deps, item: NewInbox) -> Result<InboxId, CoreError> {
    l7_inbox::open_inbox(store, deps, item)
}

/// Returns the newest session for `job`, if one exists.
pub(crate) fn latest_session(store: &Store, job: &JobId) -> Result<Option<SessionId>, CoreError> {
    let sessions = store.list_sessions(&SessionFilter {
        job_id: Some(job.clone()),
        limit: Some(1),
        ..SessionFilter::default()
    })?;
    Ok(sessions.into_iter().next().map(|session| session.id))
}

/// Returns the project's main checkout.
pub(crate) fn main_repo(deps: &Deps, store: &Store, project: &ProjectName) -> Result<PathBuf, CoreError> {
    if let Some(path) = &deps.config.project.path {
        return Ok(PathBuf::from(path));
    }
    let row = store
        .project(project)?
        .ok_or_else(|| CoreError::NotFound(format!("project {project}")))?;
    Ok(PathBuf::from(row.path))
}

/// Returns the worktree root, defaulting to `<repo>/../<repo-name>-wt`.
pub(crate) fn worktree_root(deps: &Deps, repo: &Path) -> PathBuf {
    if let Some(root) = &deps.config.project.worktree_root {
        return PathBuf::from(root);
    }
    let name = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_owned());
    repo.parent().unwrap_or(repo).join(format!("{name}-wt"))
}

/// Returns the path of `job`'s worktree.
pub(crate) fn job_worktree(job: &Job) -> Result<PathBuf, CoreError> {
    job.worktree
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| CoreError::Invalid(format!("job {} has no worktree", job.id)))
}

/// Returns the absolute path of the job brief.
pub(crate) fn brief_path(job: &Job) -> Result<PathBuf, CoreError> {
    Ok(job_worktree(job)?.join(&job.brief))
}

/// Returns the tmux window name for a job.
pub(crate) fn window_name(job: &JobId) -> String {
    vowp_tmux::window_name_for_job(job.as_str())
}

/// Returns the project's default branch.
pub(crate) fn default_branch(deps: &Deps) -> &str {
    &deps.config.project.default_branch
}

/// Returns the provider limits that are in force at `now`, keyed by provider.
///
/// A quota/rate row whose reset time has passed stays blocking while its probe-back is
/// still owed: the preferred rung returns only after a successful probe (loops.md §8.5).
pub(crate) fn limited_map(store: &Store, now: Timestamp) -> Result<BTreeMap<String, Option<Timestamp>>, CoreError> {
    let mut limited = BTreeMap::new();
    for limit in store.active_limits(now)? {
        limited.insert(limit.provider, limit.resets_at);
    }
    for (key, value) in store.kv_prefix("probe_back:")? {
        if value != "1" {
            continue;
        }
        let provider = key.trim_start_matches("probe_back:");
        if store.limit_for(provider)?.is_some() {
            limited.insert(provider.to_owned(), None);
        }
    }
    for (key, _) in store.kv_prefix("probe_pending:")? {
        limited.insert(key.trim_start_matches("probe_pending:").to_owned(), None);
    }
    Ok(limited)
}

/// Clips `text` to at most `max` characters.
pub(crate) fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Returns the number of bytes in `path`, or zero when it is absent.
pub(crate) fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Parses an RFC 3339 timestamp written by a loop, ignoring malformed values.
pub(crate) fn parse_ts(value: &str) -> Option<Timestamp> {
    Timestamp::parse(value).ok()
}

/// Converts a filesystem modification time to a protocol timestamp.
pub(crate) fn modified_at(path: &Path) -> Option<Timestamp> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let millis = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    Some(Timestamp::from_unix_millis(i64::try_from(millis).ok()?))
}
