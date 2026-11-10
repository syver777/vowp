//! The gate runner: evaluate one gate, a job's whole gate list, or the merge queue checks.
//! Spec: `docs/design/loops.md` §4 and `docs/design/crates/vowp-core.md` §4.

use std::time::Instant;

use vowp_protocol::{CmdGate, Evidence, Gate, GateResult};

use crate::CoreError;
use crate::deps::Deps;

use super::{GateCtx, Outcome, cmd, deploy, owns, pushed, report, review, secret_scan};

/// Evaluates gates for one job.
pub struct GateRunner {
    deps: Deps,
}

/// The result of evaluating every gate of one job.
#[derive(Debug, Clone, PartialEq)]
pub struct GatesOutcome {
    /// Results that were evaluated (review gates are not evaluated here).
    pub results: Vec<GateResult>,
    /// True when every evaluated gate passed.
    pub all_passed_before_review: bool,
    /// Index in the job's gate list of the first failed gate.
    pub first_failure: Option<usize>,
    /// True when the job has a `review` gate that L3 must satisfy later.
    pub needs_review: bool,
}

impl GateRunner {
    /// Creates a runner over the injected dependencies.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Evaluates one gate at `index`.
    ///
    /// Bad definitions and environment failures become `passed = false` with evidence; this
    /// method does not persist the result.
    pub fn evaluate(&self, ctx: &GateCtx<'_>, index: i32, gate: &Gate) -> Result<GateResult, CoreError> {
        let start = Instant::now();
        let outcome = self.evaluate_outcome(ctx, gate);
        Ok(self.build_result(ctx, index, gate, outcome, start))
    }

    /// Evaluates every gate of the job in order and persists each result.
    ///
    /// Evaluation continues after a failure so the fix prompt lists every finding, except
    /// that a failed `report` gate stops the rest. `review` gates are left for L3 and are not
    /// evaluated or persisted here.
    pub fn evaluate_all(&self, ctx: &GateCtx<'_>) -> Result<GatesOutcome, CoreError> {
        let needs_review = ctx.job.gates.iter().any(|gate| matches!(gate, Gate::Review(_)));
        let mut results = Vec::new();
        for (index, gate) in ctx.job.gates.iter().enumerate() {
            if matches!(gate, Gate::Review(_)) || is_disabled(gate) {
                continue;
            }
            let gate_index = i32::try_from(index).unwrap_or(i32::MAX);
            let mut result = self.evaluate(ctx, gate_index, gate)?;
            let id = ctx.store.insert_gate_result(&result)?;
            result.id = Some(id);
            let report_failed = matches!(gate, Gate::Report(_)) && !result.passed;
            results.push(result);
            if report_failed {
                break;
            }
        }
        let first_failure = results
            .iter()
            .find(|result| !result.passed)
            .map(|result| usize::try_from(result.gate_index).unwrap_or(usize::MAX));
        let all_passed_before_review = results.iter().all(|result| result.passed);
        Ok(GatesOutcome {
            results,
            all_passed_before_review,
            first_failure,
            needs_review,
        })
    }

    /// Runs the merge queue's repo-wide checks with `gate_index = -1`.
    ///
    /// Commands come from `[merge].check_cmds`, falling back to `[project].check_cmds`; the
    /// secret scan of the rebased branch runs when `[merge].secret_scan` is on.
    pub fn evaluate_merge_checks(&self, ctx: &GateCtx<'_>) -> Result<Vec<GateResult>, CoreError> {
        let config = &self.deps.config;
        let commands = if config.merge.check_cmds.is_empty() {
            &config.project.check_cmds
        } else {
            &config.merge.check_cmds
        };
        let mut results = Vec::new();
        for command in commands {
            let gate = Gate::Cmd(CmdGate {
                run: command.clone(),
                expect_exit: 0,
                expect_pattern: None,
                timeout_secs: config.merge.timeout_secs.max(1),
                cwd: None,
            });
            let mut result = self.evaluate(ctx, -1, &gate)?;
            let id = ctx.store.insert_gate_result(&result)?;
            result.id = Some(id);
            results.push(result);
        }
        if config.merge.secret_scan {
            let gate = Gate::SecretScan(true);
            let start = Instant::now();
            let outcome = secret_scan::evaluate_merge(ctx);
            let mut result = self.build_result(ctx, -1, &gate, outcome, start);
            let id = ctx.store.insert_gate_result(&result)?;
            result.id = Some(id);
            results.push(result);
        }
        Ok(results)
    }

    /// Dispatches one gate to its module.
    fn evaluate_outcome(&self, ctx: &GateCtx<'_>, gate: &Gate) -> Outcome {
        if let Err(err) = gate.validate() {
            return invalid_outcome(gate, &err);
        }
        match gate {
            Gate::Report(gate) => report::evaluate(ctx, gate),
            Gate::Cmd(gate) => cmd::evaluate(ctx, gate, false),
            Gate::Script(gate) => cmd::evaluate(ctx, gate, true),
            Gate::OwnsRespected(true) => owns::evaluate(ctx),
            Gate::OwnsRespected(false) => Outcome::new(
                true,
                Evidence::OwnsRespected {
                    changed: Vec::new(),
                    violations: Vec::new(),
                },
            ),
            Gate::Pushed(gate) => pushed::evaluate(ctx, gate),
            Gate::Review(gate) => review::evaluate(ctx, gate),
            Gate::Deploy(gate) => deploy::evaluate(ctx, gate),
            Gate::SecretScan(true) => secret_scan::evaluate(ctx),
            Gate::SecretScan(false) => Outcome::new(
                true,
                Evidence::SecretScan {
                    files_scanned: 0,
                    hits: Vec::new(),
                },
            ),
        }
    }

    /// Stamps an outcome into a [`GateResult`].
    fn build_result(&self, ctx: &GateCtx<'_>, index: i32, gate: &Gate, outcome: Outcome, start: Instant) -> GateResult {
        GateResult {
            id: None,
            job_id: ctx.job.id.clone(),
            run_id: ctx.run_id.clone(),
            attempt: ctx.attempt,
            gate_index: index,
            gate: gate.clone(),
            passed: outcome.passed,
            evidence: outcome.evidence,
            output_blob: outcome.output_blob,
            duration_ms: if outcome.duration_ms == 0 {
                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
            } else {
                outcome.duration_ms
            },
            ts: self.deps.clock.now(),
        }
    }
}

/// Returns true for gates explicitly disabled with `false`.
fn is_disabled(gate: &Gate) -> bool {
    matches!(gate, Gate::OwnsRespected(false) | Gate::SecretScan(false))
}

/// Builds a failed result for a gate that cannot run as written.
fn invalid_outcome(gate: &Gate, err: &vowp_protocol::ProtocolError) -> Outcome {
    let message = format!("invalid gate: {err}");
    let evidence = match gate {
        Gate::Cmd(command) | Gate::Script(command) => {
            if matches!(gate, Gate::Script(_)) {
                Evidence::Script {
                    command: command.run.clone(),
                    exit_code: None,
                    timed_out: false,
                    tail: message,
                    duration_ms: 0,
                }
            } else {
                Evidence::Cmd {
                    command: command.run.clone(),
                    exit_code: None,
                    timed_out: false,
                    tail: message,
                    duration_ms: 0,
                }
            }
        }
        Gate::Deploy(deploy_gate) => Evidence::Deploy {
            command: deploy_gate.run.clone(),
            exit_code: None,
            probe: Box::new(Evidence::Cmd {
                command: deploy_gate.probe.run.clone(),
                exit_code: None,
                timed_out: false,
                tail: message,
                duration_ms: 0,
            }),
            version_before: None,
            version_after: None,
            rollback_cmd: None,
        },
        _ => Evidence::Report {
            front_matter: None,
            missing_fields: Vec::new(),
            problems: vec![message],
        },
    };
    Outcome::new(false, evidence)
}
