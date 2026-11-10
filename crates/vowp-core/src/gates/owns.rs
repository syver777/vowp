//! The `owns_respected` gate: every changed path must fall inside the job's ownership globs.
//! Spec: `docs/design/loops.md` §4 and `docs/design/graph.md` §4.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use vowp_protocol::Evidence;

use crate::deps::CommandSpec;

use super::report::elapsed_ms;
use super::{GateCtx, Outcome};

/// `git status --porcelain` timeout for the uncommitted-files half of the check.
const STATUS_TIMEOUT: Duration = Duration::from_secs(60);

/// Evaluates `owns_respected`.
pub(crate) fn evaluate(ctx: &GateCtx<'_>) -> Outcome {
    let start = Instant::now();
    let branch = ctx.job.branch.clone().unwrap_or_else(|| format!("job/{}", ctx.job.id));
    let base = &ctx.deps.config.project.default_branch;

    let mut changed = BTreeSet::new();
    match ctx.deps.git.diff_names(ctx.worktree, base, &branch) {
        Ok(paths) => changed.extend(paths),
        Err(err) => {
            return Outcome {
                passed: false,
                evidence: Evidence::OwnsRespected {
                    changed: Vec::new(),
                    violations: vec![format!("cannot list changed files: {err}")],
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    }
    let mut violations = Vec::new();
    match uncommitted_paths(ctx) {
        Ok(paths) => changed.extend(paths),
        Err(problem) => violations.push(problem),
    }

    let exact = ctx.deps.config.project.always_allowed.clone();
    violations.extend(
        changed
            .iter()
            .filter(|path| !vowp_graph::path_allowed(&ctx.job.owns, &exact, path))
            .cloned(),
    );
    Outcome {
        passed: violations.is_empty(),
        evidence: Evidence::OwnsRespected {
            changed: changed.into_iter().collect(),
            violations,
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Lists uncommitted paths from `git status --porcelain`.
fn uncommitted_paths(ctx: &GateCtx<'_>) -> Result<Vec<String>, String> {
    let output = ctx
        .deps
        .cmd
        .run(&CommandSpec {
            shell: None,
            argv: vec!["git".to_owned(), "status".to_owned(), "--porcelain".to_owned()],
            cwd: Some(ctx.worktree.to_path_buf()),
            env: Vec::new(),
            timeout: STATUS_TIMEOUT,
        })
        .map_err(|err| format!("cannot list uncommitted files: {err}"))?;
    if output.exit_code != Some(0) {
        return Err(format!(
            "cannot list uncommitted files: git status exited {:?}: {}",
            output.exit_code,
            output.tail_lines(20)
        ));
    }
    Ok(output.text().lines().filter_map(parse_status_path).collect())
}

/// Extracts the new path from one porcelain status line.
fn parse_status_path(line: &str) -> Option<String> {
    if line.len() < 4 {
        return None;
    }
    let path = line[3..].trim();
    if path.is_empty() {
        return None;
    }
    let path = match path.rsplit_once(" -> ") {
        Some((_, new_path)) => new_path,
        None => path,
    };
    let path = path.trim_matches('"');
    Some(path.to_owned())
}
