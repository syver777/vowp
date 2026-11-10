//! Repository methods for `runs`, `gate_results` and `workers`.
//! Spec: `docs/design/crates/vowp-store.md` §3.4 and `docs/design/types.md` §8–9.

use rusqlite::{OptionalExtension, Row, params};
use vowp_protocol::{
    Cost, GateResult, JobId, LimitKind, NodeName, Run, RunId, RunOutcome, RunPurpose, RunRole, SessionId, Tokens,
    Worker, WorkerKind, WorkerState,
};

use crate::convert::{
    bool_to_int, enum_from_text, enum_to_text, executor_from_parts, executor_to_parts, json_from_text, json_to_text,
    opt_ts_from_text, ts_from_text, ts_to_text, u32_from_i64,
};
use crate::db::{RunFinish as Finish, Store};
use crate::error::StoreError;

/// Column list shared by every `runs` query.
const RUN_COLUMNS: &str = "id, job_id, session_id, role, purpose, profile, rung, harness, model, effort, provider, \
     node, native_session_id, pid, window, cwd, started_at, ended_at, exit_code, outcome, limit_kind, cost_usd, \
     cost_source, tokens_in, tokens_out, tokens_reasoning, tokens_cache_read, tokens_cache_write, detail";

/// Raw values of one `runs` row before protocol conversion.
struct RunRow {
    id: String,
    job_id: Option<String>,
    session_id: Option<String>,
    role: String,
    purpose: String,
    profile: Option<String>,
    rung: i64,
    harness: String,
    model: Option<String>,
    effort: Option<String>,
    provider: Option<String>,
    node: String,
    native_session_id: Option<String>,
    pid: Option<i64>,
    window: Option<String>,
    cwd: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    exit_code: Option<i32>,
    outcome: Option<String>,
    limit_kind: Option<String>,
    cost_usd: Option<f64>,
    cost_source: String,
    tokens_in: i64,
    tokens_out: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    detail: String,
}

/// Reads the textual columns of a `runs` row.
fn read_run_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: row.get(0)?,
        job_id: row.get(1)?,
        session_id: row.get(2)?,
        role: row.get(3)?,
        purpose: row.get(4)?,
        profile: row.get(5)?,
        rung: row.get(6)?,
        harness: row.get(7)?,
        model: row.get(8)?,
        effort: row.get(9)?,
        provider: row.get(10)?,
        node: row.get(11)?,
        native_session_id: row.get(12)?,
        pid: row.get(13)?,
        window: row.get(14)?,
        cwd: row.get(15)?,
        started_at: row.get(16)?,
        ended_at: row.get(17)?,
        exit_code: row.get(18)?,
        outcome: row.get(19)?,
        limit_kind: row.get(20)?,
        cost_usd: row.get(21)?,
        cost_source: row.get(22)?,
        tokens_in: row.get(23)?,
        tokens_out: row.get(24)?,
        tokens_reasoning: row.get(25)?,
        tokens_cache_read: row.get(26)?,
        tokens_cache_write: row.get(27)?,
        detail: row.get(28)?,
    })
}

/// Converts a raw `runs` row into a protocol [`Run`].
fn run_from_row(raw: RunRow) -> Result<Run, StoreError> {
    let executor = executor_from_parts(
        &raw.harness,
        raw.model.as_deref(),
        raw.effort.as_deref(),
        "runs.executor",
    )?;
    let outcome = match raw.outcome {
        Some(outcome) => Some(enum_from_text::<RunOutcome>(&outcome, "runs.outcome")?),
        None => None,
    };
    let limit_kind = match raw.limit_kind {
        Some(kind) => Some(enum_from_text::<LimitKind>(&kind, "runs.limit_kind")?),
        None => None,
    };
    let job_id = match raw.job_id {
        Some(id) => Some(JobId::parse(&id)?),
        None => None,
    };
    let session_id = match raw.session_id {
        Some(id) => Some(SessionId::parse(&id)?),
        None => None,
    };
    let pid = match raw.pid {
        Some(pid) => Some(u32::try_from(pid).map_err(|err| crate::convert::corrupt("runs.pid", err))?),
        None => None,
    };
    let tokens = Tokens {
        input: u64::try_from(raw.tokens_in).map_err(|err| crate::convert::corrupt("runs.tokens_in", err))?,
        output: u64::try_from(raw.tokens_out).map_err(|err| crate::convert::corrupt("runs.tokens_out", err))?,
        reasoning: u64::try_from(raw.tokens_reasoning)
            .map_err(|err| crate::convert::corrupt("runs.tokens_reasoning", err))?,
        cache_read: u64::try_from(raw.tokens_cache_read)
            .map_err(|err| crate::convert::corrupt("runs.tokens_cache_read", err))?,
        cache_write: u64::try_from(raw.tokens_cache_write)
            .map_err(|err| crate::convert::corrupt("runs.tokens_cache_write", err))?,
    };
    Ok(Run {
        id: RunId::parse(&raw.id)?,
        job_id,
        session_id,
        role: enum_from_text::<RunRole>(&raw.role, "runs.role")?,
        purpose: enum_from_text::<RunPurpose>(&raw.purpose, "runs.purpose")?,
        profile: raw.profile,
        rung: u32_from_i64(raw.rung, "runs.rung")?,
        executor,
        provider: raw.provider,
        node: NodeName::parse(&raw.node)?,
        native_session_id: raw.native_session_id,
        pid,
        window: raw.window,
        cwd: raw.cwd,
        started_at: ts_from_text(&raw.started_at, "runs.started_at")?,
        ended_at: opt_ts_from_text(raw.ended_at.as_deref(), "runs.ended_at")?,
        exit_code: raw.exit_code,
        outcome,
        limit_kind,
        cost: Cost {
            usd: raw.cost_usd,
            tokens,
            source: enum_from_text(&raw.cost_source, "runs.cost_source")?,
        },
        detail: raw.detail,
    })
}

/// Raw values of one `gate_results` row before protocol conversion.
struct GateResultRow {
    id: i64,
    job_id: String,
    run_id: Option<String>,
    attempt: i64,
    gate_index: i64,
    gate_json: String,
    passed: i64,
    evidence_json: String,
    output_blob: Option<String>,
    duration_ms: i64,
    ts: String,
}

/// Column list shared by every `gate_results` query.
const GATE_RESULT_COLUMNS: &str =
    "id, job_id, run_id, attempt, gate_index, gate_json, passed, evidence_json, output_blob, duration_ms, ts";

/// Reads the textual columns of a `gate_results` row.
fn read_gate_result_row(row: &Row<'_>) -> rusqlite::Result<GateResultRow> {
    Ok(GateResultRow {
        id: row.get(0)?,
        job_id: row.get(1)?,
        run_id: row.get(2)?,
        attempt: row.get(3)?,
        gate_index: row.get(4)?,
        gate_json: row.get(5)?,
        passed: row.get(6)?,
        evidence_json: row.get(7)?,
        output_blob: row.get(8)?,
        duration_ms: row.get(9)?,
        ts: row.get(10)?,
    })
}

/// Converts a raw `gate_results` row into a protocol [`GateResult`].
fn gate_result_from_row(raw: GateResultRow) -> Result<GateResult, StoreError> {
    let run_id = match raw.run_id {
        Some(id) => Some(RunId::parse(&id)?),
        None => None,
    };
    Ok(GateResult {
        id: Some(raw.id),
        job_id: JobId::parse(&raw.job_id)?,
        run_id,
        attempt: u32_from_i64(raw.attempt, "gate_results.attempt")?,
        gate_index: i32::try_from(raw.gate_index)
            .map_err(|err| crate::convert::corrupt("gate_results.gate_index", err))?,
        gate: json_from_text(&raw.gate_json, "gate_results.gate_json")?,
        passed: raw.passed != 0,
        evidence: json_from_text(&raw.evidence_json, "gate_results.evidence_json")?,
        output_blob: raw.output_blob,
        duration_ms: u64::try_from(raw.duration_ms)
            .map_err(|err| crate::convert::corrupt("gate_results.duration_ms", err))?,
        ts: ts_from_text(&raw.ts, "gate_results.ts")?,
    })
}

/// Raw values of one `workers` row before protocol conversion.
struct WorkerRow {
    node: String,
    slot: i64,
    kind: String,
    state: String,
    run_id: Option<String>,
    job_id: Option<String>,
    window: Option<String>,
    updated_at: String,
}

/// Column list shared by every `workers` query.
const WORKER_COLUMNS: &str = "node, slot, kind, state, run_id, job_id, window, updated_at";

/// Reads the textual columns of a `workers` row.
fn read_worker_row(row: &Row<'_>) -> rusqlite::Result<WorkerRow> {
    Ok(WorkerRow {
        node: row.get(0)?,
        slot: row.get(1)?,
        kind: row.get(2)?,
        state: row.get(3)?,
        run_id: row.get(4)?,
        job_id: row.get(5)?,
        window: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// Converts a raw `workers` row into a protocol [`Worker`].
fn worker_from_row(raw: WorkerRow) -> Result<Worker, StoreError> {
    let run_id = match raw.run_id {
        Some(id) => Some(RunId::parse(&id)?),
        None => None,
    };
    let job_id = match raw.job_id {
        Some(id) => Some(JobId::parse(&id)?),
        None => None,
    };
    Ok(Worker {
        node: NodeName::parse(&raw.node)?,
        slot: u32_from_i64(raw.slot, "workers.slot")?,
        kind: enum_from_text::<WorkerKind>(&raw.kind, "workers.kind")?,
        state: enum_from_text::<WorkerState>(&raw.state, "workers.state")?,
        run_id,
        job_id,
        window: raw.window,
        updated_at: ts_from_text(&raw.updated_at, "workers.updated_at")?,
    })
}

impl Store {
    /// Inserts a run that has not ended yet.
    ///
    /// A run carrying `ended_at` or `outcome` is rejected with [`StoreError::InvalidInput`].
    pub fn insert_run(&self, run: &Run) -> Result<(), StoreError> {
        if run.ended_at.is_some() || run.outcome.is_some() {
            return Err(StoreError::InvalidInput(
                "insert_run requires a run that has not ended".to_owned(),
            ));
        }
        let (harness, model, effort) = executor_to_parts(run.executor.as_ref());
        self.conn.execute(
            "INSERT INTO runs (
                id, job_id, session_id, role, purpose, profile, rung, harness, model, effort, provider,
                node, native_session_id, pid, window, cwd, started_at, ended_at, exit_code, outcome,
                limit_kind, cost_usd, cost_source, tokens_in, tokens_out, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, detail
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                NULL, NULL, NULL, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
             )",
            params![
                run.id.as_str(),
                run.job_id.as_ref().map(JobId::as_str),
                run.session_id.as_ref().map(SessionId::as_str),
                enum_to_text(&run.role)?,
                enum_to_text(&run.purpose)?,
                run.profile,
                i64::from(run.rung),
                harness,
                model,
                effort,
                run.provider,
                run.node.as_str(),
                run.native_session_id,
                run.pid.map(i64::from),
                run.window,
                run.cwd,
                ts_to_text(run.started_at),
                run.limit_kind.as_ref().map(enum_to_text).transpose()?,
                run.cost.usd,
                enum_to_text(&run.cost.source)?,
                i64::try_from(run.cost.tokens.input).map_err(|err| crate::convert::corrupt("runs.tokens_in", err))?,
                i64::try_from(run.cost.tokens.output).map_err(|err| crate::convert::corrupt("runs.tokens_out", err))?,
                i64::try_from(run.cost.tokens.reasoning)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_reasoning", err))?,
                i64::try_from(run.cost.tokens.cache_read)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_cache_read", err))?,
                i64::try_from(run.cost.tokens.cache_write)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_cache_write", err))?,
                run.detail,
            ],
        )?;
        Ok(())
    }

    /// Returns the run called `id`, when it exists.
    pub fn run(&self, id: &RunId) -> Result<Option<Run>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                [id.as_str()],
                read_run_row,
            )
            .optional()?;
        raw.map(run_from_row).transpose()
    }

    /// Returns the run called `id` or [`StoreError::NotFound`].
    pub fn require_run(&self, id: &RunId) -> Result<Run, StoreError> {
        self.run(id)?.ok_or_else(|| StoreError::not_found("run", id.as_str()))
    }

    /// Records the process attachment of a run; fields given as `None` are left unchanged.
    pub fn update_run_attach(
        &self,
        id: &RunId,
        pid: Option<u32>,
        native_session_id: Option<&str>,
        window: Option<&str>,
        session_id: Option<&SessionId>,
    ) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE runs SET pid = COALESCE(?1, pid),
                native_session_id = COALESCE(?2, native_session_id),
                window = COALESCE(?3, window),
                session_id = COALESCE(?4, session_id)
             WHERE id = ?5",
            params![
                pid.map(i64::from),
                native_session_id,
                window,
                session_id.map(SessionId::as_str),
                id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::not_found("run", id.as_str()));
        }
        Ok(())
    }

    /// Finishes a run once, writing its outcome, cost and the last 1500 characters of detail.
    pub fn finish_run(&self, id: &RunId, fin: &Finish) -> Result<(), StoreError> {
        let run = self.require_run(id)?;
        if run.ended_at.is_some() {
            return Err(StoreError::Conflict(format!("run {id} has already ended")));
        }
        let detail = tail_chars(&fin.detail, 1500);
        let changed = self.conn.execute(
            "UPDATE runs SET ended_at = ?1, outcome = ?2, exit_code = ?3, limit_kind = ?4,
                cost_usd = ?5, cost_source = ?6, tokens_in = ?7, tokens_out = ?8,
                tokens_reasoning = ?9, tokens_cache_read = ?10, tokens_cache_write = ?11,
                native_session_id = COALESCE(?12, native_session_id), detail = ?13
             WHERE id = ?14 AND ended_at IS NULL",
            params![
                ts_to_text(self.clock().now()),
                enum_to_text(&fin.outcome)?,
                fin.exit_code,
                fin.limit_kind.as_ref().map(enum_to_text).transpose()?,
                fin.cost.usd,
                enum_to_text(&fin.cost.source)?,
                i64::try_from(fin.cost.tokens.input).map_err(|err| crate::convert::corrupt("runs.tokens_in", err))?,
                i64::try_from(fin.cost.tokens.output).map_err(|err| crate::convert::corrupt("runs.tokens_out", err))?,
                i64::try_from(fin.cost.tokens.reasoning)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_reasoning", err))?,
                i64::try_from(fin.cost.tokens.cache_read)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_cache_read", err))?,
                i64::try_from(fin.cost.tokens.cache_write)
                    .map_err(|err| crate::convert::corrupt("runs.tokens_cache_write", err))?,
                fin.native_session_id,
                detail,
                id.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Conflict(format!("run {id} has already ended")));
        }
        Ok(())
    }

    /// Lists runs that have not ended, oldest first.
    pub fn open_runs(&self) -> Result<Vec<Run>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE ended_at IS NULL ORDER BY started_at, id"
        ))?;
        let rows = statement.query_map([], read_run_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(run_from_row(row?)?);
        }
        Ok(runs)
    }

    /// Lists a job's runs, oldest first.
    pub fn runs_of_job(&self, id: &JobId) -> Result<Vec<Run>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE job_id = ?1 ORDER BY started_at, id"
        ))?;
        let rows = statement.query_map([id.as_str()], read_run_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(run_from_row(row?)?);
        }
        Ok(runs)
    }

    /// Returns a job's most recent run, when it has one.
    pub fn last_run_of_job(&self, id: &JobId) -> Result<Option<Run>, StoreError> {
        let raw = self
            .conn
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE job_id = ?1 ORDER BY started_at DESC, id DESC LIMIT 1"),
                [id.as_str()],
                read_run_row,
            )
            .optional()?;
        raw.map(run_from_row).transpose()
    }

    /// Inserts a gate result and returns its rowid; `result.id` is ignored.
    pub fn insert_gate_result(&self, result: &GateResult) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO gate_results (
                job_id, run_id, attempt, gate_index, gate_kind, gate_json, passed, evidence_json,
                output_blob, duration_ms, ts
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                result.job_id.as_str(),
                result.run_id.as_ref().map(RunId::as_str),
                i64::from(result.attempt),
                i64::from(result.gate_index),
                enum_to_text(&result.gate.kind())?,
                json_to_text(&result.gate)?,
                bool_to_int(result.passed),
                json_to_text(&result.evidence)?,
                result.output_blob,
                i64::try_from(result.duration_ms)
                    .map_err(|err| crate::convert::corrupt("gate_results.duration_ms", err))?,
                ts_to_text(result.ts),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Lists one attempt's gate results, ordered by gate index then row id.
    pub fn gate_results_for(&self, job: &JobId, attempt: u32) -> Result<Vec<GateResult>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {GATE_RESULT_COLUMNS} FROM gate_results
             WHERE job_id = ?1 AND attempt = ?2 ORDER BY gate_index, id"
        ))?;
        let rows = statement.query_map(params![job.as_str(), i64::from(attempt)], read_gate_result_row)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(gate_result_from_row(row?)?);
        }
        Ok(results)
    }

    /// Lists the newest attempt's gate results, ordered by gate index then row id.
    pub fn latest_gate_results(&self, job: &JobId) -> Result<Vec<GateResult>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {GATE_RESULT_COLUMNS} FROM gate_results
             WHERE job_id = ?1
               AND attempt = (SELECT MAX(attempt) FROM gate_results WHERE job_id = ?1)
             ORDER BY gate_index, id"
        ))?;
        let rows = statement.query_map([job.as_str()], read_gate_result_row)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(gate_result_from_row(row?)?);
        }
        Ok(results)
    }

    /// Inserts or updates one worker slot.
    pub fn upsert_worker(&self, worker: &Worker) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO workers (node, slot, kind, state, run_id, job_id, window, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(node, kind, slot) DO UPDATE SET
               state = excluded.state,
               run_id = excluded.run_id,
               job_id = excluded.job_id,
               window = excluded.window,
               updated_at = excluded.updated_at",
            params![
                worker.node.as_str(),
                i64::from(worker.slot),
                enum_to_text(&worker.kind)?,
                enum_to_text(&worker.state)?,
                worker.run_id.as_ref().map(RunId::as_str),
                worker.job_id.as_ref().map(JobId::as_str),
                worker.window,
                ts_to_text(worker.updated_at),
            ],
        )?;
        Ok(())
    }

    /// Lists one node's worker slots, ordered by kind then slot.
    pub fn list_workers(&self, node: &NodeName) -> Result<Vec<Worker>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {WORKER_COLUMNS} FROM workers WHERE node = ?1 ORDER BY kind, slot"
        ))?;
        let rows = statement.query_map([node.as_str()], read_worker_row)?;
        let mut workers = Vec::new();
        for row in rows {
            workers.push(worker_from_row(row?)?);
        }
        Ok(workers)
    }

    /// Creates the idle slots `1..=count` and removes idle slots above `count`.
    ///
    /// Busy and draining slots are never removed, even above the new bound.
    pub fn ensure_worker_slots(&self, node: &NodeName, kind: WorkerKind, count: u32) -> Result<(), StoreError> {
        self.transaction(|tx| {
            let kind_text = enum_to_text(&kind)?;
            tx.conn.execute(
                "DELETE FROM workers WHERE node = ?1 AND kind = ?2 AND slot > ?3 AND state = 'idle'",
                params![node.as_str(), kind_text, i64::from(count)],
            )?;
            let now = ts_to_text(tx.clock().now());
            for slot in 1..=count {
                tx.conn.execute(
                    "INSERT OR IGNORE INTO workers (node, slot, kind, state, run_id, job_id, window, updated_at)
                     VALUES (?1, ?2, ?3, 'idle', NULL, NULL, NULL, ?4)",
                    params![node.as_str(), i64::from(slot), kind_text, now],
                )?;
            }
            Ok(())
        })
    }
}

/// Keeps the last `limit` characters of `text`.
fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    text.chars().skip(count - limit).collect()
}
