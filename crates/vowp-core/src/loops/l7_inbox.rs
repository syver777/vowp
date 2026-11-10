//! L7 inbox: items, their Markdown copies, and turning an answer into job progress.
//!
//! Spec: `docs/design/loops.md` §9 and `docs/design/crates/vowp-core.md` §7. An inbox item
//! is the only place a question, approval, decision or notice waits for a human. Answering
//! a question writes the answer to the job's worktree, appends it to the job note and lets
//! the parked job become `ready` again.

use std::path::{Path, PathBuf};

use serde_json::json;
use vowp_protocol::{
    InboxId, InboxItem, InboxKind, InboxState, Job, JobId, JobState, LedgerEntry, LedgerKind, ParkedReason, ProjectName,
};
use vowp_store::{StateChange, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::requests::{Request, push};

/// Input for [`open_inbox`].
#[derive(Debug, Clone)]
pub struct NewInboxItem {
    /// Project the item belongs to, when any.
    pub project: Option<ProjectName>,
    /// Job that raised it, when any.
    pub job_id: Option<JobId>,
    /// Item kind.
    pub kind: InboxKind,
    /// Short title, also used for deduplication.
    pub title: String,
    /// Full body, clipped to 4000 characters.
    pub body: String,
    /// Suggested answers.
    pub options: Vec<String>,
}

/// What answering one item means for the job graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerEffect {
    /// Jobs that should receive `Resolved`.
    pub jobs: Vec<JobId>,
    /// Provider whose limit row is cleared by this answer (`credits`/`auth`).
    pub clear_limit: Option<String>,
}

/// Opens one inbox item and writes its Markdown copy under `<node>/inbox/`.
///
/// An open item with the same kind and title is returned instead of creating a duplicate.
/// A question or approval attached to a job in a state without a live run also parks that
/// job with [`ParkedReason::Question`]; loop effects park their job themselves before
/// calling this, so the second call is a no-op.
pub fn open_inbox(store: &Store, deps: &Deps, item: NewInboxItem) -> Result<InboxId, CoreError> {
    if let Some(existing) = store
        .list_inbox(Some(InboxState::Open))?
        .into_iter()
        .find(|open| open.kind == item.kind && open.title == item.title)
    {
        return Ok(existing.id);
    }

    let id = InboxId::new();
    let now = store.clock().now();
    let body = super::clip_chars(&item.body, 4000);
    store.insert_inbox(&InboxItem {
        id: id.clone(),
        project: item.project.clone(),
        job_id: item.job_id.clone(),
        kind: item.kind,
        title: item.title.clone(),
        body: body.clone(),
        options: item.options,
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: now,
        answered_at: None,
    })?;
    record_inbox(store, &id, "opened", &item.title, &item.project, item.job_id.as_ref())?;

    let dir = deps.paths.root().join("inbox");
    std::fs::create_dir_all(&dir).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let path = dir.join(format!("{id}.md"));
    let text = format!("# {}\n\n{}\n", item.title, body);
    std::fs::write(&path, text).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;

    if matches!(item.kind, InboxKind::Question | InboxKind::Approval)
        && let Some(job_id) = &item.job_id
        && let Some(job) = store.job(job_id)?
        && !job.state.is_terminal()
        && job.state != JobState::Parked
        && matches!(
            job.state,
            JobState::Pending | JobState::Ready | JobState::Stalled | JobState::Failed
        )
    {
        park_job(store, &job, ParkedReason::Question)?;
    }
    Ok(id)
}

/// Applies an answered item: writes the answer file, appends the job note and records what
/// the caller should resolve. The caller performs the transitions (the supervisor directly,
/// the CLI through request rows), so this function stays a pure store/filesystem step.
pub fn record_answer(store: &Store, item: &InboxItem) -> Result<AnswerEffect, CoreError> {
    if item.state != InboxState::Answered {
        return Err(CoreError::Invalid(format!(
            "inbox item {} has not been answered",
            item.id
        )));
    }
    let answer = item.answer.clone().unwrap_or_default();
    let mut effect = AnswerEffect::default();
    let mut jobs = Vec::new();
    if let Some(job_id) = &item.job_id {
        jobs.push(store.require_job(job_id)?);
    } else if let Some((reason, provider)) = provider_title(&item.title) {
        effect.clear_limit = Some(provider.clone());
        let wanted = match reason {
            "credits" => Some(ParkedReason::Credits),
            "auth" => Some(ParkedReason::Auth),
            "provider_blocked" => Some(ParkedReason::ProviderBlocked),
            _ => None,
        };
        if let Some(wanted) = wanted {
            for job in store.list_jobs(&vowp_store::JobFilter {
                project: item.project.clone(),
                states: vec![JobState::Parked],
                ..vowp_store::JobFilter::default()
            })? {
                if job.parked_reason == Some(wanted) && job_on_provider(store, &job, &provider)? {
                    jobs.push(job);
                }
            }
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    for job in jobs {
        if !seen.insert(job.id.clone()) {
            continue;
        }
        write_answer_file(store, &job, &answer)?;
        append_note(store, &job, &answer)?;
        effect.jobs.push(job.id);
    }
    record_inbox(
        store,
        &item.id,
        "answered",
        &item.title,
        &item.project,
        item.job_id.as_ref(),
    )?;
    Ok(effect)
}

/// Public L7 entry point: apply an answer and queue `Resolved` for every affected job.
pub fn on_answer(store: &Store, id: &InboxId) -> Result<(), CoreError> {
    let item = store
        .inbox_item(id)?
        .ok_or_else(|| CoreError::NotFound(format!("inbox item {id}")))?;
    let effect = record_answer(store, &item)?;
    if let Some(provider) = &effect.clear_limit {
        store.clear_limit(provider)?;
    }
    for job in effect.jobs {
        push(store, &Request::ResolveJob { job })?;
    }
    Ok(())
}

/// Returns true when a parked job's current rung belongs to `provider`.
///
/// A job parked before its first run has no run row; the supervisor's park note names the
/// provider in that case (`credits exhausted for deepseek`, `provider deepseek is blocked`).
fn job_on_provider(store: &Store, job: &Job, provider: &str) -> Result<bool, CoreError> {
    if let Some(run) = store.last_run_of_job(&job.id)?
        && let Some(run_provider) = super::l6_failover::provider_of(&run)
    {
        return Ok(run_provider == provider);
    }
    Ok(job.note.contains(provider))
}

/// Returns `(reason, provider)` encoded in a provider-wide inbox title.
fn provider_title(title: &str) -> Option<(&'static str, String)> {
    for (prefix, reason) in [
        ("credits exhausted: ", "credits"),
        ("auth required: ", "auth"),
        ("provider blocked: ", "provider_blocked"),
    ] {
        if let Some(provider) = title.strip_prefix(prefix) {
            return Some((reason, provider.trim().to_owned()));
        }
    }
    None
}

/// Writes `<worktree>/.vowp/jobs/<id>/answer-<n>.md` and returns its relative path.
fn write_answer_file(store: &Store, job: &Job, answer: &str) -> Result<Option<String>, CoreError> {
    let Some(worktree) = &job.worktree else {
        return Ok(None);
    };
    let dir = PathBuf::from(worktree).join(".vowp/jobs").join(job.id.as_str());
    std::fs::create_dir_all(&dir).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let index = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("answer-"))
                .count()
                + 1
        })
        .unwrap_or(1);
    let path = dir.join(format!("answer-{index}.md"));
    std::fs::write(&path, format!("{answer}\n")).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let _ = store;
    Ok(Some(format!(".vowp/jobs/{}/answer-{index}.md", job.id)))
}

/// Appends the owner's answer to the job note, keeping any earlier note.
fn append_note(store: &Store, job: &Job, answer: &str) -> Result<(), CoreError> {
    if answer.trim().is_empty() {
        return Ok(());
    }
    let note = if job.note.trim().is_empty() {
        format!("owner answer: {answer}")
    } else {
        format!("{}\nowner answer: {answer}", job.note)
    };
    store.set_job_note(&job.id, &note)?;
    Ok(())
}

/// Parks one job with `reason`, writing a state ledger row.
fn park_job(store: &Store, job: &Job, reason: ParkedReason) -> Result<(), CoreError> {
    let from = store.set_job_state(
        &job.id,
        JobState::Parked,
        &StateChange {
            note: None,
            parked_reason: Some(reason),
        },
    )?;
    append_state(store, job, from, JobState::Parked, "blocked on an inbox item")?;
    Ok(())
}

/// Writes one ledger `inbox` row.
fn record_inbox(
    store: &Store,
    id: &InboxId,
    action: &str,
    title: &str,
    project: &Option<ProjectName>,
    job_id: Option<&JobId>,
) -> Result<(), CoreError> {
    store.append_ledger(&LedgerEntry {
        id: None,
        ts: store.clock().now(),
        kind: LedgerKind::Inbox,
        project: project.clone(),
        job_id: job_id.cloned(),
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("inbox {id} {action}: {title}"),
        detail: json!({"action": action}),
    })?;
    Ok(())
}

/// Writes one ledger `state` row for a direct L7 park.
fn append_state(store: &Store, job: &Job, from: JobState, to: JobState, why: &str) -> Result<(), CoreError> {
    store.append_ledger(&LedgerEntry {
        id: None,
        ts: store.clock().now(),
        kind: LedgerKind::State,
        project: Some(job.project.clone()),
        job_id: Some(job.id.clone()),
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: format!("{}: {} -> {} ({why})", job.id, from.as_str(), to.as_str()),
        detail: json!({"from": from.as_str(), "to": to.as_str(), "why": why}),
    })?;
    Ok(())
}

/// Returns the path of the inbox Markdown copy, for tests and the CLI.
pub fn item_path(deps: &Deps, id: &InboxId) -> PathBuf {
    Path::new(&deps.paths.root()).join("inbox").join(format!("{id}.md"))
}
