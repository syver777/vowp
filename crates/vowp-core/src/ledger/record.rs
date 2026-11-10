//! Append-only ledger writes for dispatches, gates, costs and job state changes.
//! Spec: `docs/design/crates/vowp-core.md` §5 and `docs/design/loops.md`.
//!
//! Every method appends one row through [`Store::append_ledger`]; the table trigger rejects
//! updates and deletes, so this module never tries to correct a row. Timestamps come from the
//! store's injected clock, never from the wall clock.

use serde::Serialize;
use serde_json::json;
use vowp_protocol::{
    Cost, CostSource, Executor, GateResult, HarnessKind, Job, JobId, JobState, LedgerEntry, LedgerKind, Run, RunSummary,
};
use vowp_store::{LedgerFilter, Store};

use crate::{CoreError, Deps};

use super::price;

/// Writes the append-only ledger rows the loops and the session service produce.
#[derive(Clone)]
pub struct Ledger {
    deps: Deps,
}

impl Ledger {
    /// Creates a ledger writer over `deps`.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Appends one arbitrary ledger entry and returns its row id.
    pub fn record(&self, store: &Store, entry: LedgerEntry) -> Result<i64, CoreError> {
        Ok(store.append_ledger(&entry)?)
    }

    /// Records the dispatch of `job` to the run `run` is about to start.
    pub fn record_dispatch(&self, store: &Store, job: &Job, run: &Run) -> Result<(), CoreError> {
        let executor = run
            .executor
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "shell".to_owned());
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Dispatch,
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            run_id: Some(run.id.clone()),
            session_id: run.session_id.clone(),
            role: Some(run.role),
            provider: run.provider.clone(),
            harness: run.executor.as_ref().map(|exec| exec.harness),
            model: run.executor.as_ref().map(|exec| exec.model.clone()),
            cost: None,
            summary: format!("dispatch {} to {executor}", job.id),
            detail: json!({
                "run": run.id.as_str(),
                "executor": run.executor.as_ref().map(ToString::to_string),
                "purpose": wire_name(&run.purpose)?,
                "profile": run.profile,
                "rung": run.rung,
            }),
        };
        self.record(store, entry)?;
        Ok(())
    }

    /// Records one cost row for a finished run and returns the cost that was stored.
    ///
    /// A receipt from `summary` wins; otherwise the configured price table is used; otherwise
    /// the cost is unknown (never assumed free). Calling this twice for the same run returns
    /// the first row's cost instead of writing a second row.
    pub fn record_run_cost(&self, store: &Store, run: &Run, summary: &RunSummary) -> Result<Cost, CoreError> {
        if let Some(existing) = self.existing_run_cost(store, run)? {
            return Ok(existing);
        }

        let cost = self.cost_for(run, summary);
        let project = match &run.job_id {
            Some(job_id) => store.job(job_id)?.map(|job| job.project),
            None => None,
        };
        let provider = run
            .provider
            .clone()
            .or_else(|| run.executor.as_ref().map(Executor::provider));
        let detail = match run.executor.as_ref().map(|exec| exec.harness) {
            // Claude Code on a subscription reports a list-price equivalent, not a bill.
            Some(HarnessKind::ClaudeCode) => json!({"billed": false, "basis": "list"}),
            _ => json!({
                "source": wire_name(&cost.source)?,
                "outcome": wire_name(&summary.outcome)?,
            }),
        };
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Cost,
            project,
            job_id: run.job_id.clone(),
            run_id: Some(run.id.clone()),
            session_id: run.session_id.clone(),
            role: Some(run.role),
            provider,
            harness: run.executor.as_ref().map(|exec| exec.harness),
            model: run.executor.as_ref().map(|exec| exec.model.clone()),
            cost: Some(cost.clone()),
            summary: format!("cost {} {}", run.id, cost_label(&cost)),
            detail,
        };
        self.record(store, entry)?;
        Ok(cost)
    }

    /// Records one evaluated gate result (pass or fail) with its evidence.
    pub fn record_gate(&self, store: &Store, result: &GateResult) -> Result<(), CoreError> {
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Gate,
            project: None,
            job_id: Some(result.job_id.clone()),
            run_id: result.run_id.clone(),
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!(
                "gate {} {} {}",
                result.gate_index,
                wire_name(&result.gate.kind())?,
                if result.passed { "passed" } else { "failed" }
            ),
            detail: json!({
                "gate_index": result.gate_index,
                "attempt": result.attempt,
                "passed": result.passed,
                "duration_ms": result.duration_ms,
                "evidence": result.evidence,
            }),
        };
        self.record(store, entry)?;
        Ok(())
    }

    /// Records one job state-machine transition.
    pub fn record_state(
        &self,
        store: &Store,
        job: &JobId,
        from: JobState,
        to: JobState,
        why: &str,
    ) -> Result<(), CoreError> {
        let entry = LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::State,
            project: None,
            job_id: Some(job.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("{job}: {} -> {}", from.as_str(), to.as_str()),
            detail: json!({"from": from.as_str(), "to": to.as_str(), "why": why}),
        };
        self.record(store, entry)?;
        Ok(())
    }

    /// Chooses the stored cost: a receipt, then a price-table estimate, then unknown.
    fn cost_for(&self, run: &Run, summary: &RunSummary) -> Cost {
        if summary.cost.usd.is_some() && summary.cost.source != CostSource::Unknown {
            return summary.cost.clone();
        }
        if let Some(executor) = &run.executor {
            let estimate = price::estimate(&self.deps.config, executor, &summary.cost.tokens);
            if estimate.usd.is_some() {
                return estimate;
            }
        }
        Cost {
            usd: None,
            tokens: summary.cost.tokens,
            source: CostSource::Unknown,
        }
    }

    /// Returns the cost row already written for `run`, when there is one.
    fn existing_run_cost(&self, store: &Store, run: &Run) -> Result<Option<Cost>, CoreError> {
        let filter = LedgerFilter {
            kind: Some(LedgerKind::Cost),
            job_id: run.job_id.clone(),
            since: None,
            limit: None,
        };
        for entry in store.ledger_since(&filter)? {
            if entry.run_id.as_ref() == Some(&run.id) {
                return Ok(entry.cost);
            }
        }
        Ok(None)
    }
}

/// Formats the source and amount of a cost for a ledger summary.
fn cost_label(cost: &Cost) -> String {
    match cost.usd {
        Some(usd) => format!("{usd:.6} USD {}", source_label(cost.source)),
        None => "unknown".to_owned(),
    }
}

/// Returns the canonical label of a cost source.
fn source_label(source: CostSource) -> &'static str {
    match source {
        CostSource::Receipt => "receipt",
        CostSource::PriceTable => "price-table",
        CostSource::Unknown => "unknown",
    }
}

/// Returns the serde/wire name of an enum value (snake_case for every protocol enum).
fn wire_name<T: Serialize>(value: &T) -> Result<String, CoreError> {
    let json = serde_json::to_value(value).map_err(vowp_protocol::ProtocolError::from)?;
    json.as_str()
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Invalid("enum did not serialise as a string".to_owned()))
}
