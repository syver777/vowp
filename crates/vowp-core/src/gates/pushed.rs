//! The `pushed` gate: the job branch exists on the remote and matches the local head.
//! Spec: `docs/design/loops.md` §4.

use std::time::Instant;

use vowp_protocol::{Evidence, PushedGate};

use super::report::elapsed_ms;
use super::{GateCtx, Outcome};

/// Evaluates `pushed`.
pub(crate) fn evaluate(ctx: &GateCtx<'_>, gate: &PushedGate) -> Outcome {
    let start = Instant::now();
    let branch = gate
        .branch
        .clone()
        .or_else(|| ctx.job.branch.clone())
        .unwrap_or_else(|| format!("job/{}", ctx.job.id));
    let remote = &gate.remote;

    let ls_remote = match ctx.deps.git.ls_remote(ctx.worktree, remote, &branch) {
        Ok(output) => output,
        Err(err) => {
            return Outcome {
                passed: false,
                evidence: Evidence::Pushed {
                    remote: remote.clone(),
                    branch,
                    ls_remote: format!("ls-remote failed: {err}"),
                    local_sha: None,
                },
                duration_ms: elapsed_ms(start),
                output_blob: None,
            };
        }
    };
    let remote_sha = remote_sha(&ls_remote);
    let local_sha = ctx.deps.git.head_sha(ctx.worktree, &branch).ok();
    let passed = match (&remote_sha, &local_sha) {
        (Some(remote), Some(local)) => remote.eq_ignore_ascii_case(local),
        _ => false,
    };
    Outcome {
        passed,
        evidence: Evidence::Pushed {
            remote: remote.clone(),
            branch,
            ls_remote,
            local_sha,
        },
        duration_ms: elapsed_ms(start),
        output_blob: None,
    }
}

/// Returns the first sha in `git ls-remote` output.
fn remote_sha(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
}
