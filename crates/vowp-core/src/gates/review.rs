//! The `review` gate: a verify job's report must carry `verdict: pass`.
//! Spec: `docs/design/loops.md` §4–§5.

use std::path::PathBuf;
use std::time::Instant;

use vowp_protocol::{EdgeKind, Evidence, Job, JobKind, Report, ReviewGate, Verdict};
use vowp_store::JobFilter;

use super::report::elapsed_ms;
use super::{GateCtx, Outcome};

/// Evaluates `review` by reading the latest verify job for this attempt.
pub(crate) fn evaluate(ctx: &GateCtx<'_>, _gate: &ReviewGate) -> Outcome {
    let start = Instant::now();
    let verify_job = match latest_verify_job(ctx) {
        Ok(Some(job)) => job,
        Ok(None) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Review {
                    verify_job: None,
                    verdict: None,
                    findings: Vec::new(),
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
        Err(err) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Review {
                    verify_job: None,
                    verdict: None,
                    findings: vec![format!("cannot list verify jobs: {err}")],
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    };

    let report_path = report_path(ctx, &verify_job);
    let report = match read_report(&report_path) {
        Ok(report) => report,
        Err(problem) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Review {
                    verify_job: Some(verify_job.id),
                    verdict: None,
                    findings: vec![problem],
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    };

    let verdict = report.front_matter.verdict;
    let findings = if verdict == Some(Verdict::Fail) {
        report.front_matter.findings.clone()
    } else if verdict.is_none() {
        vec!["verifier report has no verdict".to_owned()]
    } else {
        Vec::new()
    };
    Outcome {
        passed: verdict == Some(Verdict::Pass),
        evidence: Evidence::Review {
            verify_job: Some(verify_job.id),
            verdict,
            findings,
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Returns the newest verify job that verifies this job for the current attempt.
fn latest_verify_job(ctx: &GateCtx<'_>) -> Result<Option<Job>, crate::CoreError> {
    let filter = JobFilter {
        project: Some(ctx.job.project.clone()),
        kind: Some(JobKind::Verify),
        ..JobFilter::default()
    };
    let jobs = ctx.store.list_jobs(&filter)?;
    let mut candidates = Vec::new();
    for job in jobs {
        let verifies = ctx
            .store
            .edges_of(&job.id)?
            .into_iter()
            .any(|edge| edge.kind == EdgeKind::Verifies && edge.dst == ctx.job.id);
        if verifies {
            candidates.push(job);
        }
    }
    let attempt_suffix = format!("-verify-{}", ctx.attempt);
    let mut matching: Vec<Job> = candidates
        .into_iter()
        .filter(|job| job.id.as_str().ends_with(&attempt_suffix))
        .collect();
    matching.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    Ok(matching.pop())
}

/// Resolves the verifier's report path, preferring its recorded worktree.
fn report_path(ctx: &GateCtx<'_>, verify_job: &Job) -> PathBuf {
    let path = PathBuf::from(&verify_job.report);
    if path.is_absolute() {
        return path;
    }
    match &verify_job.worktree {
        Some(worktree) => PathBuf::from(worktree).join(path),
        None => ctx.worktree.join(path),
    }
}

/// Reads a verifier report, turning I/O and parse failures into finding text.
fn read_report(path: &std::path::Path) -> Result<Report, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read verifier report {}: {err}", path.display()))?;
    Report::parse(&text).map_err(|err| format!("cannot parse verifier report {}: {err}", path.display()))
}
