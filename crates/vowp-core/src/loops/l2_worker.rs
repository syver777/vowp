//! L2 worker loop: start, continue and fix prompts, and the report check after a run.
//!
//! Spec: `docs/design/loops.md` §3. The prompt text is part of the contract (golden-tested
//! through the acceptance tests) and `continue_prompt` deliberately has no nudge parameter
//! (decisions.md errata 3).

use std::path::Path;

use vowp_protocol::{Job, JobKind, Report, ReportStatus, RunPurpose, Timestamp};

use crate::CoreError;

/// What a finished worker run left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    /// A report exists and parses with `done` or `partial` status.
    Ready,
    /// The report is `blocked`; the questions go to the inbox.
    Blocked(Vec<String>),
    /// No report, an unparsable report, or one older than the run.
    NoReport,
}

/// Returns the run purpose for a fresh dispatch of `job`.
///
/// T2 only sees `job.continues`; the store-aware Start-vs-Resume decision is made by
/// `dispatch::purpose_for_dispatch`, which also looks for a prior session/run.
pub fn purpose_for(job: &Job) -> RunPurpose {
    if matches!(job.kind, JobKind::Fix | JobKind::Merge) {
        RunPurpose::Fix
    } else if job.continues > 0 {
        RunPurpose::Continue
    } else {
        RunPurpose::Start
    }
}

/// The L2 start prompt (loops.md §3.1).
pub fn start_prompt(job: &Job, brief: &str) -> String {
    let worktree = job.worktree.as_deref().unwrap_or(".");
    let branch = job.branch.as_deref().unwrap_or("job/<unknown>");
    let report = report_text(job);
    format!(
        "You are the worker for job {job_id} of project {project}. Your working directory is the job worktree {worktree} on branch {branch}.\n\
         Read AGENTS.md and your brief at {brief_path} first, and do exactly what the brief says. Change only the paths in `owns`.\n\
         When finished, commit on this branch (do not push), run the gates listed in the brief, and write your report to {report_path}\n\
         with the front-matter required by the brief. Do not stop before the report exists. If you need the owner, write the question under\n\
         questions_for_owner and set status: blocked.",
        job_id = job.id,
        project = job.project,
        worktree = worktree,
        branch = branch,
        brief_path = brief,
        report_path = report,
    )
}

/// The L2 continue prompt (loops.md §3.2).
pub fn continue_prompt(job: &Job) -> String {
    let report = report_text(job);
    format!(
        "You stopped before writing your report ({report_path}). Continue the job from where you left off: finish the work, run the gates,\n\
         commit, and write the report. Do not announce next steps — do them. ({used} of 2 automatic continues used; after that a supervisor will diagnose.)",
        report_path = report,
        used = job.continues,
    )
}

/// The L2 fix prompt (loops.md §3.3).
pub fn fix_prompt(job: &Job, findings: &[String]) -> String {
    let parent = parent_id(job);
    let attempt = job.attempts.max(1);
    let max = job.loop_.max_attempts.max(1);
    let mut list = String::new();
    if findings.is_empty() {
        list.push_str("1. (no findings recorded)");
    } else {
        for (index, finding) in findings.iter().enumerate() {
            list.push_str(&format!("{}. {}\n", index + 1, finding));
        }
        list.truncate(list.trim_end_matches('\n').len());
    }
    format!(
        "Job {parent} failed its checks (round {attempt} of {max}). Findings:\n{list}\n\
         Fix exactly these problems in the same worktree, re-run the failing commands, commit, and update {report_path}. Do not widen scope.",
        report_path = report_text(job),
    )
}

/// Checks the report after a worker run.
pub fn classify_report(worktree: &Path, job: &Job, run_started_at: Timestamp) -> ReportOutcome {
    let path = worktree.join(&job.report);
    let Some(modified) = super::modified_at(&path) else {
        return ReportOutcome::NoReport;
    };
    if modified < run_started_at {
        return ReportOutcome::NoReport;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ReportOutcome::NoReport;
    };
    let Ok(report) = Report::parse(&text) else {
        return ReportOutcome::NoReport;
    };
    match report.front_matter.status {
        ReportStatus::Done | ReportStatus::Partial => ReportOutcome::Ready,
        ReportStatus::Blocked => {
            let mut questions = report.front_matter.questions_for_owner;
            if questions.is_empty() {
                questions.push(format!("job {} reported blocked", job.id));
            }
            ReportOutcome::Blocked(questions)
        }
    }
}

/// Builds a fix job id from its parent and the current attempt number.
pub(crate) fn child_id(parent: &Job, kind: JobKind) -> Result<String, CoreError> {
    let suffix = match kind {
        JobKind::Fix => "fix",
        JobKind::Merge => "merge",
        JobKind::Verify => "verify",
        _ => {
            return Err(CoreError::Invalid(format!("job kind {kind:?} is not a child job")));
        }
    };
    Ok(format!("{}-{suffix}-{}", parent.id, parent.attempts.max(1)))
}

/// Returns the parent id encoded in a fix/merge/verify child id.
fn parent_id(job: &Job) -> String {
    let id = job.id.as_str();
    for marker in ["-fix-", "-merge-", "-verify-"] {
        if let Some((parent, _)) = id.rsplit_once(marker) {
            return parent.to_owned();
        }
    }
    id.to_owned()
}

/// Formats a job's report path for a prompt.
fn report_text(job: &Job) -> String {
    match &job.worktree {
        Some(worktree) => format!("{worktree}/{}", job.report),
        None => job.report.clone(),
    }
}
