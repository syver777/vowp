//! The `deploy` gate: run the deploy command in the environment checkout, then probe it.
//! Spec: `docs/design/loops.md` §4.

use std::path::{Path, PathBuf};
use std::time::Instant;

use vowp_protocol::{DeployGate, EnvConfig, Evidence};

use crate::deps::Deps;

use super::cmd;
use super::report::elapsed_ms;
use super::{GateCtx, Outcome};

/// Timeout used for the optional environment version probe.
const VERSION_PROBE_TIMEOUT_SECS: u64 = 300;

/// Timeout used for the deploy command itself.
const DEPLOY_TIMEOUT_SECS: u64 = 1800;

/// Evaluates a deploy gate.
pub(crate) fn evaluate(ctx: &GateCtx<'_>, gate: &DeployGate) -> Outcome {
    let start = Instant::now();
    let env = match gate.env.as_deref() {
        Some(name) => match ctx.deps.config.env.get(name) {
            Some(env) => Some(env),
            None => return missing_environment(gate, name, start),
        },
        None => None,
    };
    let cwd = checkout(ctx, env);

    let version_before = probe_version(ctx.deps, env, &cwd);
    let deploy = cmd::run_shell(ctx.deps, &gate.run, &cwd, DEPLOY_TIMEOUT_SECS);
    let (exit_code, deploy_ok) = match &deploy {
        Ok(output) => (output.exit_code, output.exit_code == Some(0) && !output.timed_out),
        Err(_) => (None, false),
    };
    let version_after = probe_version(ctx.deps, env, &cwd);

    let probe_base = cwd;
    let probe = cmd::evaluate_at(ctx.deps, &gate.probe, &probe_base, false);
    let probe_evidence = probe.evidence.clone();
    let passed = deploy_ok && probe.passed;
    Outcome {
        passed,
        evidence: Evidence::Deploy {
            command: gate.run.clone(),
            exit_code,
            probe: Box::new(probe_evidence),
            version_before,
            version_after,
            rollback_cmd: env.and_then(|env| env.rollback.clone()),
        },
        duration_ms: elapsed_ms(start),
        output_blob: probe.output_blob,
    }
}

/// Builds a failed outcome for a deploy gate whose named environment is not configured.
///
/// Nothing runs in this case: deploying an unknown environment in the job worktree would
/// report success while the real environment was never touched.
fn missing_environment(gate: &DeployGate, name: &str, start: Instant) -> Outcome {
    let duration_ms = elapsed_ms(start);
    let message = format!("environment {name:?} is not configured; refusing to deploy in the job worktree");
    Outcome {
        passed: false,
        evidence: Evidence::Deploy {
            command: gate.run.clone(),
            exit_code: None,
            probe: Box::new(cmd::evidence(false, &gate.probe.run, None, false, message, duration_ms)),
            version_before: None,
            version_after: None,
            rollback_cmd: None,
        },
        duration_ms,
        output_blob: None,
    }
}

/// Returns the environment checkout, or the job worktree when no environment is configured.
fn checkout(ctx: &GateCtx<'_>, env: Option<&EnvConfig>) -> PathBuf {
    env.map(|env| PathBuf::from(&env.checkout))
        .unwrap_or_else(|| ctx.worktree.to_path_buf())
}

/// Runs the environment's own version probe, when it has one.
fn probe_version(deps: &Deps, env: Option<&EnvConfig>, cwd: &Path) -> Option<String> {
    let env = env?;
    let command = env.probe.as_ref()?;
    let output = cmd::run_shell(deps, command, cwd, VERSION_PROBE_TIMEOUT_SECS).ok()?;
    if output.exit_code != Some(0) {
        return None;
    }
    let text = output.text().trim().to_owned();
    if text.is_empty() { None } else { Some(text) }
}
