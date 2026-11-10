//! `cmd` and `script` gates: run one shell command in the worktree and check its result.
//! Spec: `docs/design/loops.md` §4.

use std::path::Path;
use std::time::{Duration, Instant};

use regex::Regex;
use vowp_protocol::{CmdGate, Evidence};
use vowp_store::blobs::BlobStore;

use crate::CoreError;
use crate::deps::{CommandOutput, CommandSpec, Deps};

use super::{GateCtx, Outcome};

/// Output larger than this is also stored as a content-addressed blob.
const SPILL_BYTES: usize = 8192;

/// Evaluates a `cmd` gate (or its `script` twin).
pub(crate) fn evaluate(ctx: &GateCtx<'_>, gate: &CmdGate, script: bool) -> Outcome {
    evaluate_at(ctx.deps, gate, ctx.worktree, script)
}

/// Evaluates a command gate with `base` as the working directory.
pub(crate) fn evaluate_at(deps: &Deps, gate: &CmdGate, base: &Path, script: bool) -> Outcome {
    let start = Instant::now();
    let cwd = resolve_cwd(base, gate.cwd.as_deref());
    let output = run_shell(deps, &gate.run, &cwd, gate.timeout_secs);

    match output {
        Ok(output) => {
            let duration_ms = u64::try_from(output.duration.as_millis()).unwrap_or(u64::MAX);
            let text = output.text();
            let timed_out = output.timed_out;
            let exit_ok = !timed_out && output.exit_code == Some(gate.expect_exit);
            let mut tail = output.tail_lines(200);
            let mut pattern_ok = true;
            if let Some(pattern) = &gate.expect_pattern {
                match Regex::new(pattern) {
                    Ok(regex) => pattern_ok = regex.is_match(&text),
                    Err(err) => {
                        pattern_ok = false;
                        if !tail.is_empty() {
                            tail.push('\n');
                        }
                        tail.push_str(&format!("invalid expect_pattern {pattern:?}: {err}"));
                    }
                }
            }
            let passed = exit_ok && pattern_ok;
            let output_blob = spill(deps, &output.output);
            Outcome {
                passed,
                evidence: evidence(script, &gate.run, output.exit_code, timed_out, tail, duration_ms),
                duration_ms: if duration_ms == 0 {
                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
                } else {
                    duration_ms
                },
                output_blob,
            }
        }
        Err(err) => {
            let tail = format!("command could not run: {err}");
            Outcome {
                passed: false,
                evidence: evidence(
                    script,
                    &gate.run,
                    None,
                    false,
                    tail,
                    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                output_blob: None,
            }
        }
    }
}

/// Runs one shell command with the gate timeout.
pub(crate) fn run_shell(deps: &Deps, run: &str, cwd: &Path, timeout_secs: u64) -> Result<CommandOutput, CoreError> {
    deps.cmd.run(&CommandSpec {
        shell: Some(run.to_owned()),
        argv: Vec::new(),
        cwd: Some(cwd.to_path_buf()),
        env: Vec::new(),
        timeout: Duration::from_secs(timeout_secs.max(1)),
    })
}

/// Resolves a gate's optional relative working directory against the worktree.
pub(crate) fn resolve_cwd(worktree: &Path, cwd: Option<&str>) -> std::path::PathBuf {
    match cwd {
        Some(cwd) => worktree.join(cwd),
        None => worktree.to_path_buf(),
    }
}

/// Spills output larger than [`SPILL_BYTES`] into the node's gate blob directory.
fn spill(deps: &Deps, output: &[u8]) -> Option<String> {
    if output.len() <= SPILL_BYTES {
        return None;
    }
    let blobs = BlobStore::new(deps.paths.root().join("gates").join("blobs"));
    match blobs.put(output) {
        Ok(blob) => Some(blob.sha256),
        Err(err) => {
            deps.log.log(
                vowp_protocol::Level::Warn,
                "gates",
                &format!("could not spill gate output to a blob: {err}"),
            );
            None
        }
    }
}

/// Builds command or script evidence.
pub(crate) fn evidence(
    script: bool,
    command: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    tail: String,
    duration_ms: u64,
) -> Evidence {
    if script {
        Evidence::Script {
            command: command.to_owned(),
            exit_code,
            timed_out,
            tail,
            duration_ms,
        }
    } else {
        Evidence::Cmd {
            command: command.to_owned(),
            exit_code,
            timed_out,
            tail,
            duration_ms,
        }
    }
}
