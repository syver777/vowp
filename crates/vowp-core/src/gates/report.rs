//! The `report` gate: the worker's report must exist, parse, match the job and be `done`.
//! Spec: `docs/design/loops.md` §4 and `docs/design/types.md` §8.1.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use vowp_protocol::{Evidence, Report, ReportGate, ReportStatus};

use crate::deps::CommandSpec;

use super::{GateCtx, Outcome};

/// Evaluates the `report` gate.
pub(crate) fn evaluate(ctx: &GateCtx<'_>, gate: &ReportGate) -> Outcome {
    let start = Instant::now();
    let path = report_path(ctx);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Report {
                    front_matter: None,
                    missing_fields: gate.fields.clone(),
                    problems: vec![format!("cannot read report {}: {err}", path.display())],
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    };
    let report = match Report::parse(&text) {
        Ok(report) => report,
        Err(err) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Report {
                    front_matter: None,
                    missing_fields: gate.fields.clone(),
                    problems: vec![format!("cannot parse report {}: {err}", path.display())],
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    };

    let missing_fields = report.missing_fields(&gate.fields);
    let mut problems = Vec::new();
    if let Some(branch) = &ctx.job.branch {
        if report.front_matter.branch != *branch {
            problems.push(format!(
                "report branch {:?} does not match job branch {branch:?}",
                report.front_matter.branch
            ));
        }
    } else {
        problems.push("job has no branch to compare with the report".to_owned());
    }
    if report.front_matter.job != ctx.job.id {
        problems.push(format!(
            "report job {:?} does not match job {:?}",
            report.front_matter.job, ctx.job.id
        ));
    }
    if report.front_matter.status != ReportStatus::Done {
        problems.push(format!(
            "status {} is not done; a job only advances with status: done",
            status_text(report.front_matter.status)
        ));
    }
    for commit in &report.front_matter.commits {
        if !commit_exists(ctx, commit) {
            problems.push(format!("commit {commit:?} does not exist in the worktree"));
        }
    }

    let passed = missing_fields.is_empty() && problems.is_empty();
    Outcome {
        passed,
        evidence: Evidence::Report {
            front_matter: Some(report.raw_front_matter.clone()),
            missing_fields,
            problems,
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Returns the absolute report path for the job.
pub(crate) fn report_path(ctx: &GateCtx<'_>) -> PathBuf {
    let path = PathBuf::from(&ctx.job.report);
    if path.is_absolute() {
        path
    } else {
        ctx.worktree.join(path)
    }
}

/// Returns true when `commit` resolves to an object in the worktree repository.
fn commit_exists(ctx: &GateCtx<'_>, commit: &str) -> bool {
    let output = ctx.deps.cmd.run(&CommandSpec {
        shell: None,
        argv: vec![
            "git".to_owned(),
            "cat-file".to_owned(),
            "-e".to_owned(),
            commit.to_owned(),
        ],
        cwd: Some(ctx.worktree.to_path_buf()),
        env: Vec::new(),
        timeout: Duration::from_secs(60),
    });
    matches!(output, Ok(output) if output.exit_code == Some(0))
}

/// Returns whole milliseconds since `start`, saturating at `u64::MAX`.
pub(crate) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Returns the canonical snake_case status text.
fn status_text(status: ReportStatus) -> &'static str {
    match status {
        ReportStatus::Done => "done",
        ReportStatus::Partial => "partial",
        ReportStatus::Blocked => "blocked",
    }
}
