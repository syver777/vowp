//! `cmd` and `script` gates: run one shell command in the worktree and check its result.
//! Spec: `docs/design/loops.md` §4.
//!
//! Stored evidence is redacted with the vault's exact-value redactor: the tail and any
//! spilled output blob carry `[REDACTED:<NAME>]` instead of a value the command printed.
//! The raw output still feeds `expect_pattern`, and exit codes and timeouts are untouched.

use std::path::Path;
use std::time::Instant;

use regex::Regex;
use vowp_protocol::{CmdGate, Evidence, Job, ProjectName, RunId, RunRole, RunUsage};
use vowp_store::Store;
use vowp_store::blobs::BlobStore;

use crate::CoreError;
use crate::capacity::{scope, usage};
use crate::deps::{CommandOutput, Deps};

use super::{GateCtx, Outcome};

/// Output larger than this is also stored as a content-addressed blob.
const SPILL_BYTES: usize = 8192;

/// Evaluates a `cmd` gate (or its `script` twin).
pub(crate) fn evaluate(ctx: &GateCtx<'_>, gate: &CmdGate, script: bool) -> Outcome {
    evaluate_inner(
        ctx.deps,
        gate,
        ctx.worktree,
        script,
        Some((ctx.store, ctx.job, ctx.run_id.as_ref())),
    )
}

/// Evaluates a command gate with `base` as the working directory.
pub(crate) fn evaluate_at(deps: &Deps, gate: &CmdGate, base: &Path, script: bool) -> Outcome {
    evaluate_inner(deps, gate, base, script, None)
}

/// Evaluates one command gate, optionally recording its measured usage.
fn evaluate_inner(
    deps: &Deps,
    gate: &CmdGate,
    base: &Path,
    script: bool,
    record: Option<(&Store, &Job, Option<&RunId>)>,
) -> Outcome {
    let start = Instant::now();
    let cwd = resolve_cwd(base, gate.cwd.as_deref());
    let scoped = scope::run_scoped_shell(deps, &gate.run, &cwd, gate.timeout_secs, "gate");
    let output = scoped.as_ref().map(|scoped| &scoped.output);
    if let (Some(record), Ok(scoped)) = (record, &scoped)
        && let Err(error) = record_gate_usage(deps, record.0, record.1, scoped, &cwd)
    {
        deps.log.log(
            vowp_protocol::Level::Warn,
            "capacity",
            &format!("could not record gate usage: {error}"),
        );
    }

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
            // The pattern above matched the command's real output; only the stored tail is
            // redacted. The command text, exit code and timeout stay as measured.
            let tail = crate::vault::redact_text(deps, &tail);
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

/// Records one gate command's measured usage under a synthetic run id.
///
/// A gate is not a `runs` row of its own; the synthetic id keeps several gates of one job from
/// overwriting the adapter run's usage row while still tying the sample to the job and kind.
fn record_gate_usage(
    deps: &Deps,
    store: &Store,
    job: &Job,
    scoped: &scope::ScopedShell,
    cwd: &Path,
) -> Result<(), CoreError> {
    let project = match store.bound_project() {
        Some(project) => project.clone(),
        None => ProjectName::parse("_node")?,
    };
    let duration_ms = scoped
        .ended_at
        .since(scoped.started_at)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let (sample, source) = scope::effective_sample(scoped.sample.clone().unwrap_or_default(), scoped.source);
    let build_mode = scope::infer_build_mode(cwd);
    let usage = RunUsage {
        run_id: RunId::new(),
        project,
        job_id: Some(job.id.clone()),
        job_kind: Some(job.kind),
        role: RunRole::Gate,
        harness: None,
        model: None,
        provider: None,
        node: deps.node.clone(),
        started_at: scoped.started_at,
        ended_at: scoped.ended_at,
        duration_ms,
        peak_rss_bytes: sample.peak_rss_bytes.max(1),
        anon_peak_bytes: sample.anon_peak_bytes,
        file_peak_bytes: sample.file_peak_bytes,
        kernel_peak_bytes: sample.kernel_peak_bytes,
        cpu_usec: sample.cpu_usec,
        mean_cpu_cores: sample.mean_cpu_cores,
        max_cpu_cores: sample.max_cpu_cores,
        build_mode,
        source,
        oom_killed: sample.oom_killed,
        exit_code: scoped.output.exit_code,
    };
    usage::record(store, &usage)
}

/// Runs one shell command with the gate timeout.
pub(crate) fn run_shell(deps: &Deps, run: &str, cwd: &Path, timeout_secs: u64) -> Result<CommandOutput, CoreError> {
    Ok(scope::run_scoped_shell(deps, run, cwd, timeout_secs, "gate")?.output)
}

/// Resolves a gate's optional relative working directory against the worktree.
pub(crate) fn resolve_cwd(worktree: &Path, cwd: Option<&str>) -> std::path::PathBuf {
    match cwd {
        Some(cwd) => worktree.join(cwd),
        None => worktree.to_path_buf(),
    }
}

/// Spills output larger than [`SPILL_BYTES`] into the node's gate blob directory.
///
/// The bytes are redacted before they are written, so a spilled blob cannot keep a value
/// the command printed even when the short evidence tail was redacted.
fn spill(deps: &Deps, output: &[u8]) -> Option<String> {
    if output.len() <= SPILL_BYTES {
        return None;
    }
    let redacted = crate::vault::redact_bytes(deps, output);
    let blobs = BlobStore::new(deps.paths.root().join("gates").join("blobs"));
    match blobs.put(&redacted) {
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
