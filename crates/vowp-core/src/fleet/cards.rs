//! Project card builders for [`super::build_snapshot`].
//!
//! The card is derived from one bound project handle. Fleet reads open each project
//! database read-only in turn (never two at once), as the split-store rule requires.

use std::collections::BTreeMap;

use vowp_protocol::{
    BlockedJobCard, Evidence, GateFailureCard, GateKind, InboxItem, InboxState, Job, JobState, LedgerKind, MergeCard,
    ProjectEntry, ProjectName, ProjectSnapshot, ProjectState, QueueJobCard, RunningJobCard, Timestamp,
};
use vowp_store::{CostGroup, CostQuery, JobFilter, LedgerFilter, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::vault::Redactor;

/// One project's card plus the inbox rows that belong to it.
///
/// The inbox rows stay raw here; the snapshot writer redacts and truncates them when it
/// builds [`vowp_protocol::InboxSummary`] values.
pub(crate) struct ProjectRead {
    /// The snapshot card.
    pub card: ProjectSnapshot,
    /// Open project and matching node inbox rows.
    pub inbox: Vec<InboxItem>,
    /// Consecutive failed L1 ticks recorded in the project's `l1-supervisor` loop row.
    pub fail_streak: u32,
}

/// Builds one project's snapshot card from a bound store.
///
/// `deps` supplies the node name and the redactor; `store` must be bound to `project`.
/// `project` is the registry entry, so state, weight and pause reason come from the real
/// registry row rather than a schema default.
pub(crate) fn project_read(
    deps: &Deps,
    redactor: &Redactor,
    store: &Store,
    project: &ProjectEntry,
    now: Timestamp,
) -> Result<ProjectRead, CoreError> {
    let jobs_by_state = store.count_jobs_by_state(Some(&project.name))?;
    let jobs = store.list_jobs(&JobFilter {
        project: Some(project.name.clone()),
        ..JobFilter::default()
    })?;
    let open_runs = store.open_runs()?;

    let mut running = Vec::new();
    for job in jobs.iter().filter(|job| job.state.holds_lock()) {
        let run = open_runs.iter().rev().find(|run| run.job_id.as_ref() == Some(&job.id));
        let (executor, node, started_at, last_event_age_secs, session_id) = match run {
            Some(run) => {
                let last_event_age = match &run.session_id {
                    Some(session_id) => store
                        .session(session_id)?
                        .map(|session| now.since(session.updated_at).as_secs()),
                    None => None,
                };
                (
                    run.executor.as_ref().map(ToString::to_string),
                    run.node.clone(),
                    run.started_at,
                    last_event_age,
                    run.session_id.clone(),
                )
            }
            None => (None, deps.node.clone(), job.state_changed_at, None, None),
        };
        running.push(RunningJobCard {
            job: job.id.clone(),
            kind: job.kind,
            role: job.role,
            executor,
            node,
            age_secs: now.since(started_at).as_secs(),
            last_event_age_secs,
            session_id,
        });
    }
    running.sort_by(|left, right| left.job.cmp(&right.job));

    let mut blocked = Vec::new();
    for job in jobs
        .iter()
        .filter(|job| matches!(job.state, JobState::Parked | JobState::Failed))
    {
        blocked.push(BlockedJobCard {
            job: job.id.clone(),
            state: job.state,
            reason: blocked_reason(job),
            detail: summarize(redactor, &job.note),
            since: job.state_changed_at,
        });
    }
    blocked.sort_by(|left, right| left.job.cmp(&right.job));

    let mut queued: Vec<&Job> = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.state,
                JobState::Pending | JobState::Ready | JobState::Parked | JobState::Stalled
            )
        })
        .collect();
    queued.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    queued.truncate(200);
    let queue = queued
        .into_iter()
        .map(|job| QueueJobCard {
            job: job.id.clone(),
            title: full_title(redactor, &job.title),
            kind: job.kind,
            role: job.role,
            priority: job.priority,
            state: job.state,
            age_secs: now.since(job.created_at).as_secs(),
            waiting_reason: waiting_reason(redactor, job),
        })
        .collect();

    // `latest_gate_results` only sees the newest attempt. The card is specified as the last
    // 10 failures across the project, so walk every attempt the job has recorded. The newest
    // attempt is added as well, which keeps the card honest even if `job.attempts` is stale.
    let mut gate_failures = Vec::new();
    for job in &jobs {
        let mut results = Vec::new();
        for attempt in 0..=job.attempts {
            results.extend(store.gate_results_for(&job.id, attempt)?);
        }
        for result in store.latest_gate_results(&job.id)? {
            if results.iter().all(|seen| seen.id != result.id) {
                results.push(result);
            }
        }
        for result in results {
            if result.passed {
                continue;
            }
            gate_failures.push(GateFailureCard {
                job: result.job_id.clone(),
                gate: gate_kind_name(result.gate.kind()).to_owned(),
                at: result.ts,
                evidence: summarize(redactor, &evidence_line(&result.evidence)),
            });
        }
    }
    gate_failures.sort_by_key(|failure| std::cmp::Reverse(failure.at));
    gate_failures.truncate(10);

    let last_merge = last_merge(store, &project.name, &jobs, redactor)?;
    let cost_today_usd = cost_today(store, &project.name, now)?;
    let inbox = store.list_inbox(Some(InboxState::Open))?;
    let fail_streak = store
        .loop_("l1-supervisor")?
        .map(|loop_| loop_.fail_streak)
        .unwrap_or(0);

    Ok(ProjectRead {
        card: ProjectSnapshot {
            name: project.name.clone(),
            path: project.path.clone(),
            state: project.state,
            weight: project.weight,
            paused_reason: project.paused_reason.clone(),
            jobs_by_state,
            running,
            blocked,
            queue,
            last_merge,
            gate_failures,
            cost_today_usd,
        },
        inbox,
        fail_streak,
    })
}

/// Builds the card for a project whose database could not be read.
///
/// One broken project must never stop the rest of the fleet, so the card is marked paused
/// with the error as its reason and every derived list stays empty. The registry weight is
/// kept as recorded, so the card still describes the right project.
pub(crate) fn broken_card(project: &ProjectEntry, error: &CoreError) -> ProjectSnapshot {
    ProjectSnapshot {
        name: project.name.clone(),
        path: project.path.clone(),
        state: ProjectState::Paused,
        weight: project.weight,
        paused_reason: format!("broken: {error}"),
        jobs_by_state: BTreeMap::new(),
        running: Vec::new(),
        blocked: Vec::new(),
        queue: Vec::new(),
        last_merge: None,
        gate_failures: Vec::new(),
        cost_today_usd: None,
    }
}

/// Returns the human label for a parked reason.
fn blocked_reason(job: &Job) -> String {
    let parked = match job.parked_reason {
        Some(vowp_protocol::ParkedReason::Credits) => "credits",
        Some(vowp_protocol::ParkedReason::Auth) => "auth",
        Some(vowp_protocol::ParkedReason::Question) => "question",
        Some(vowp_protocol::ParkedReason::ProviderBlocked) => "provider_blocked",
        Some(vowp_protocol::ParkedReason::Manual) => "manual",
        Some(vowp_protocol::ParkedReason::Exhausted) => "exhausted",
        None => "",
    };
    if !parked.is_empty() {
        return parked.to_owned();
    }
    match job.state {
        JobState::Failed => "failed".to_owned(),
        _ => "parked".to_owned(),
    }
}

/// Returns a job's admission/brake reason when its note carries one.
///
/// Admission writes the whole `waiting: RAM 1.2 GB short` line to `jobs.note`; the card
/// carries that line verbatim (redacted) so the orchestrator sees the same text.
fn waiting_reason(redactor: &Redactor, job: &Job) -> Option<String> {
    let reason = job.note.trim();
    if !reason.starts_with("waiting:") || reason == "waiting:" {
        return None;
    }
    Some(summarize(redactor, reason))
}

/// Reads the newest successful merge row for a project.
///
/// Merge refusal rows are recorded under the same ledger kind with `detail.failure` and no
/// commit; they are skipped so the card names a real merge, never a refused attempt.
fn last_merge(
    store: &Store,
    project: &ProjectName,
    jobs: &[Job],
    redactor: &Redactor,
) -> Result<Option<MergeCard>, CoreError> {
    let rows = store.ledger_since(&LedgerFilter {
        kind: Some(LedgerKind::Merge),
        ..LedgerFilter::default()
    })?;
    let entry = rows
        .into_iter()
        .filter(|entry| entry.project.as_ref() == Some(project))
        .filter(|entry| {
            let commit = entry
                .detail
                .get("main_after")
                .or_else(|| entry.detail.get("commit"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty());
            entry.detail.get("failure").is_none() || commit
        })
        .max_by_key(|entry| entry.ts);
    let Some(entry) = entry else {
        return Ok(None);
    };
    let Some(job_id) = entry.job_id.clone() else {
        return Ok(None);
    };
    let commit = entry
        .detail
        .get("main_after")
        .or_else(|| entry.detail.get("commit"))
        .and_then(serde_json::Value::as_str)
        .map(|value| summarize(redactor, value))
        .unwrap_or_default();
    let branch = entry
        .detail
        .get("branch")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            jobs.iter()
                .find(|job| job.id == job_id)
                .and_then(|job| job.branch.clone())
        })
        .unwrap_or_default();
    Ok(Some(MergeCard {
        job: job_id,
        at: entry.ts,
        commit,
        branch,
    }))
}

/// Sums today's known cost rows for one project.
///
/// Rows whose amount is unknown are never counted as zero: when no row carries a known USD
/// amount, the card says `None`.
fn cost_today(store: &Store, project: &ProjectName, now: Timestamp) -> Result<Option<f64>, CoreError> {
    let rows = store.cost_totals(
        &CostQuery {
            since: Some(start_of_day(now)),
            project: Some(project.clone()),
            ..CostQuery::default()
        },
        CostGroup::Day,
    )?;
    let known: u64 = rows
        .iter()
        .map(|row| row.receipt_entries + row.price_table_entries)
        .sum();
    if known == 0 {
        return Ok(None);
    }
    Ok(Some(rows.iter().map(|row| row.usd).sum()))
}

/// Returns midnight UTC of `now`.
fn start_of_day(now: Timestamp) -> Timestamp {
    let secs = now.unix_secs();
    Timestamp::from_unix_secs(secs - secs.rem_euclid(86_400))
}

/// Redacts one title or question, keeping the full text.
///
/// The snapshot's 200-character cap applies to bodies and detail lines, not to titles.
fn full_title(redactor: &Redactor, text: &str) -> String {
    redactor.redact(text)
}

/// Redacts and truncates one free-text body or detail line to the snapshot's 200-character cap.
fn summarize(redactor: &Redactor, text: &str) -> String {
    redactor.redact(text).chars().take(200).collect()
}

/// Returns one readable line from a gate's evidence.
fn evidence_line(evidence: &Evidence) -> String {
    match evidence {
        Evidence::Report {
            missing_fields,
            problems,
            front_matter,
        } => {
            if !missing_fields.is_empty() {
                format!("missing fields: {}", missing_fields.join(", "))
            } else if !problems.is_empty() {
                problems.join("; ")
            } else if front_matter.is_some() {
                "front matter parsed".to_owned()
            } else {
                "report has no front matter".to_owned()
            }
        }
        Evidence::Cmd {
            command,
            exit_code,
            timed_out,
            tail,
            ..
        }
        | Evidence::Script {
            command,
            exit_code,
            timed_out,
            tail,
            ..
        } => {
            let outcome = if *timed_out {
                "timed out".to_owned()
            } else {
                format!("exit {exit_code:?}")
            };
            let last = tail.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
            format!("{command}: {outcome}; {last}")
        }
        Evidence::OwnsRespected { violations, changed } => {
            if violations.is_empty() {
                format!("{} file(s) inside owns", changed.len())
            } else {
                format!("outside owns: {}", violations.join(", "))
            }
        }
        Evidence::Pushed { remote, branch, .. } => format!("{remote} {branch} not found"),
        Evidence::Review { findings, verdict, .. } => {
            if findings.is_empty() {
                verdict
                    .as_ref()
                    .map(|verdict| format!("verdict {verdict:?}"))
                    .unwrap_or_else(|| "review failed".to_owned())
            } else {
                findings.join("; ")
            }
        }
        Evidence::Deploy {
            command,
            exit_code,
            probe,
            ..
        } => format!("{command}: exit {exit_code:?}; probe: {}", evidence_line(probe)),
        Evidence::SecretScan { files_scanned, hits } => {
            if hits.is_empty() {
                format!("{files_scanned} file(s) scanned")
            } else {
                hits.iter()
                    .map(|hit| format!("{}:{} {}", hit.path, hit.line, hit.rule))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
    }
}

/// Returns the stable snake_case name of a gate kind.
fn gate_kind_name(kind: GateKind) -> &'static str {
    match kind {
        GateKind::Report => "report",
        GateKind::Cmd => "cmd",
        GateKind::OwnsRespected => "owns_respected",
        GateKind::Pushed => "pushed",
        GateKind::Review => "review",
        GateKind::Script => "script",
        GateKind::Deploy => "deploy",
        GateKind::SecretScan => "secret_scan",
    }
}
