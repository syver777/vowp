//! Repository methods for the append-only `ledger` table and its cost aggregation.
//! Spec: `docs/design/crates/vowp-store.md` §3.4 and `docs/design/types.md` §10.
//!
//! Rows are only ever inserted: schema triggers reject `UPDATE` and `DELETE`. Cost totals are
//! computed directly from the ledger columns (role, provider, harness and model are copied onto
//! cost rows), so no join to `runs` is needed.

use rusqlite::types::ToSql;
use rusqlite::{Row, params, params_from_iter};
use vowp_protocol::{
    Cost, CostSource, HarnessKind, JobId, LedgerEntry, LedgerKind, ProjectName, RunId, RunRole, SessionId, Tokens,
};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, ts_from_text, ts_to_text, u64_from_i64,
};
use crate::db::{CostGroup, CostQuery, CostRow, LedgerFilter, Store};
use crate::error::StoreError;

/// Column list shared by every `ledger` query.
const LEDGER_COLUMNS: &str = "id, ts, kind, project, job_id, run_id, session_id, role, provider, harness, model, \
     usd, tokens_in, tokens_out, tokens_reasoning, tokens_cache_read, tokens_cache_write, cost_source, summary, \
     detail_json";

/// Raw values of one `ledger` row before protocol conversion.
struct LedgerRow {
    id: i64,
    ts: String,
    kind: String,
    project: Option<String>,
    job_id: Option<String>,
    run_id: Option<String>,
    session_id: Option<String>,
    role: Option<String>,
    provider: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    cost_source: Option<String>,
    summary: String,
    detail_json: String,
}

/// Reads the textual columns of a `ledger` row.
fn read_ledger_row(row: &Row<'_>) -> rusqlite::Result<LedgerRow> {
    Ok(LedgerRow {
        id: row.get(0)?,
        ts: row.get(1)?,
        kind: row.get(2)?,
        project: row.get(3)?,
        job_id: row.get(4)?,
        run_id: row.get(5)?,
        session_id: row.get(6)?,
        role: row.get(7)?,
        provider: row.get(8)?,
        harness: row.get(9)?,
        model: row.get(10)?,
        usd: row.get(11)?,
        tokens_in: row.get(12)?,
        tokens_out: row.get(13)?,
        tokens_reasoning: row.get(14)?,
        tokens_cache_read: row.get(15)?,
        tokens_cache_write: row.get(16)?,
        cost_source: row.get(17)?,
        summary: row.get(18)?,
        detail_json: row.get(19)?,
    })
}

/// Converts a raw `ledger` row into a protocol [`LedgerEntry`].
fn ledger_from_row(raw: LedgerRow) -> Result<LedgerEntry, StoreError> {
    let tokens = Tokens {
        input: u64_from_i64(raw.tokens_in, "ledger.tokens_in")?,
        output: u64_from_i64(raw.tokens_out, "ledger.tokens_out")?,
        reasoning: u64_from_i64(raw.tokens_reasoning, "ledger.tokens_reasoning")?,
        cache_read: u64_from_i64(raw.tokens_cache_read, "ledger.tokens_cache_read")?,
        cache_write: u64_from_i64(raw.tokens_cache_write, "ledger.tokens_cache_write")?,
    };
    let cost = match raw.cost_source {
        Some(source) => Some(Cost {
            usd: raw.usd,
            tokens,
            source: enum_from_text::<CostSource>(&source, "ledger.cost_source")?,
        }),
        None => None,
    };
    Ok(LedgerEntry {
        id: Some(raw.id),
        ts: ts_from_text(&raw.ts, "ledger.ts")?,
        kind: enum_from_text::<LedgerKind>(&raw.kind, "ledger.kind")?,
        project: raw.project.as_deref().map(ProjectName::parse).transpose()?,
        job_id: raw.job_id.as_deref().map(JobId::parse).transpose()?,
        run_id: raw.run_id.as_deref().map(RunId::parse).transpose()?,
        session_id: raw.session_id.as_deref().map(SessionId::parse).transpose()?,
        role: raw
            .role
            .as_deref()
            .map(|role| enum_from_text::<RunRole>(role, "ledger.role"))
            .transpose()?,
        provider: raw.provider,
        harness: raw
            .harness
            .as_deref()
            .map(|harness| enum_from_text::<HarnessKind>(harness, "ledger.harness"))
            .transpose()?,
        model: raw.model,
        cost,
        summary: raw.summary,
        detail: json_from_text(&raw.detail_json, "ledger.detail_json")?,
    })
}

impl Store {
    /// Appends one ledger row and returns its autoincrement id.
    ///
    /// Cost columns come from `entry.cost`; an absent or unknown cost stores `usd` as `NULL`
    /// and `cost_source` as `unknown` (or `NULL` when the entry has no cost at all).
    pub fn append_ledger(&self, entry: &LedgerEntry) -> Result<i64, StoreError> {
        let tokens = entry.cost.as_ref().map(|cost| cost.tokens).unwrap_or_default();
        let (usd, cost_source) = match &entry.cost {
            Some(cost) => (cost.usd, Some(enum_to_text(&cost.source)?)),
            None => (None, None),
        };
        self.conn.execute(
            "INSERT INTO ledger (
                ts, kind, project, job_id, run_id, session_id, role, provider, harness, model, usd,
                tokens_in, tokens_out, tokens_reasoning, tokens_cache_read, tokens_cache_write,
                cost_source, summary, detail_json
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
             )",
            params![
                ts_to_text(entry.ts),
                enum_to_text(&entry.kind)?,
                entry.project.as_ref().map(ProjectName::as_str),
                entry.job_id.as_ref().map(JobId::as_str),
                entry.run_id.as_ref().map(RunId::as_str),
                entry.session_id.as_ref().map(SessionId::as_str),
                entry.role.as_ref().map(enum_to_text).transpose()?,
                entry.provider,
                entry.harness.as_ref().map(enum_to_text).transpose()?,
                entry.model,
                usd,
                token_i64(tokens.input, "ledger.tokens_in")?,
                token_i64(tokens.output, "ledger.tokens_out")?,
                token_i64(tokens.reasoning, "ledger.tokens_reasoning")?,
                token_i64(tokens.cache_read, "ledger.tokens_cache_read")?,
                token_i64(tokens.cache_write, "ledger.tokens_cache_write")?,
                cost_source,
                entry.summary,
                json_to_text(&entry.detail)?,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Lists ledger rows matching `filter`, ascending by id.
    pub fn ledger_since(&self, filter: &LedgerFilter) -> Result<Vec<LedgerEntry>, StoreError> {
        let mut query = format!("SELECT {LEDGER_COLUMNS} FROM ledger WHERE 1 = 1");
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(kind) = &filter.kind {
            query.push_str(" AND kind = ?");
            values.push(Box::new(enum_to_text(kind)?));
        }
        if let Some(job_id) = &filter.job_id {
            query.push_str(" AND job_id = ?");
            values.push(Box::new(job_id.as_str().to_owned()));
        }
        if let Some(since) = filter.since {
            query.push_str(" AND ts >= ?");
            values.push(Box::new(ts_to_text(since)));
        }
        query.push_str(" ORDER BY id");
        if let Some(limit) = filter.limit {
            query.push_str(" LIMIT ?");
            values.push(Box::new(i64::from(limit)));
        }

        let mut statement = self.conn.prepare(&query)?;
        let rows = statement.query_map(params_from_iter(values), read_ledger_row)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(ledger_from_row(row?)?);
        }
        Ok(entries)
    }

    /// Sums `kind = 'cost'` rows grouped by one dimension.
    ///
    /// Rows whose key column is `NULL` fall into the key `"(none)"`; `Day` groups by the UTC
    /// date (`YYYY-MM-DD`) of `ts`. Ordering is known USD descending, then key ascending.
    pub fn cost_totals(&self, query: &CostQuery, group: CostGroup) -> Result<Vec<CostRow>, StoreError> {
        let key_expr = match group {
            CostGroup::Job => "COALESCE(job_id, '(none)')",
            CostGroup::Role => "COALESCE(role, '(none)')",
            CostGroup::Provider => "COALESCE(provider, '(none)')",
            CostGroup::Model => "COALESCE(model, '(none)')",
            CostGroup::Day => "substr(ts, 1, 10)",
        };
        let mut sql = format!(
            "SELECT {key_expr} AS group_key,
                COALESCE(SUM(usd), 0.0) AS group_usd,
                SUM(CASE WHEN cost_source = 'receipt' THEN 1 ELSE 0 END) AS receipt_entries,
                SUM(CASE WHEN cost_source = 'price-table' THEN 1 ELSE 0 END) AS price_table_entries,
                SUM(CASE WHEN usd IS NULL THEN 1 ELSE 0 END) AS unknown_entries,
                COALESCE(SUM(tokens_in), 0) AS tokens_in,
                COALESCE(SUM(tokens_out), 0) AS tokens_out,
                COALESCE(SUM(tokens_reasoning), 0) AS tokens_reasoning,
                COALESCE(SUM(tokens_cache_read), 0) AS tokens_cache_read,
                COALESCE(SUM(tokens_cache_write), 0) AS tokens_cache_write,
                COUNT(*) AS entries
             FROM ledger WHERE kind = 'cost'"
        );
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(since) = query.since {
            sql.push_str(" AND ts >= ?");
            values.push(Box::new(ts_to_text(since)));
        }
        if let Some(until) = query.until {
            sql.push_str(" AND ts <= ?");
            values.push(Box::new(ts_to_text(until)));
        }
        if let Some(project) = &query.project {
            sql.push_str(" AND project = ?");
            values.push(Box::new(project.as_str().to_owned()));
        }
        if let Some(job_id) = &query.job_id {
            sql.push_str(" AND job_id = ?");
            values.push(Box::new(job_id.as_str().to_owned()));
        }
        sql.push_str(&format!(" GROUP BY {key_expr} ORDER BY group_usd DESC, group_key ASC"));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        let mut totals = Vec::new();
        for row in rows {
            let (
                key,
                usd,
                receipt_entries,
                price_table_entries,
                unknown_entries,
                tokens_in,
                tokens_out,
                tokens_reasoning,
                tokens_cache_read,
                tokens_cache_write,
                entries,
            ) = row?;
            totals.push(CostRow {
                key,
                usd,
                receipt_entries: u64_from_i64(receipt_entries, "ledger.receipt_entries")?,
                price_table_entries: u64_from_i64(price_table_entries, "ledger.price_table_entries")?,
                unknown_entries: u64_from_i64(unknown_entries, "ledger.unknown_entries")?,
                tokens: Tokens {
                    input: u64_from_i64(tokens_in, "ledger.tokens_in")?,
                    output: u64_from_i64(tokens_out, "ledger.tokens_out")?,
                    reasoning: u64_from_i64(tokens_reasoning, "ledger.tokens_reasoning")?,
                    cache_read: u64_from_i64(tokens_cache_read, "ledger.tokens_cache_read")?,
                    cache_write: u64_from_i64(tokens_cache_write, "ledger.tokens_cache_write")?,
                },
                entries: u64_from_i64(entries, "ledger.entries")?,
            });
        }
        Ok(totals)
    }
}

/// Converts one token count into SQLite's signed integer form.
fn token_i64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|err| crate::convert::corrupt(what, err))
}
