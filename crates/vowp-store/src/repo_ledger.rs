//! Repository methods for the two append-only ledger tables and their cost aggregation.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.4, §3.6 and §3.7.
//!
//! Project rows live in `project.ledger`; node-wide rows live in `main.node_ledger`. The
//! tables are disjoint by name, and every statement names the file it touches. Rows are only
//! ever inserted: schema triggers reject `UPDATE` and `DELETE`.

use std::collections::BTreeMap;

use rusqlite::types::ToSql;
use rusqlite::{Row, params, params_from_iter};
use vowp_protocol::{
    CostSource, HarnessKind, JobId, LedgerEntry, LedgerKind, ProjectName, RunId, RunRole, SessionId, Tokens,
};

use crate::convert::{
    enum_from_text, enum_to_text, json_from_text, json_to_text, ts_from_text, ts_to_text, u64_from_i64,
};
use crate::db::{CostGroup, CostQuery, CostRow, LedgerFilter, Store};
use crate::error::StoreError;

/// Column list shared by both ledger queries.
const LEDGER_COLUMNS: &str = "id, ts, kind, project, job_id, run_id, session_id, role, provider, harness, model, \
     usd, tokens_in, tokens_out, tokens_reasoning, tokens_cache_read, tokens_cache_write, cost_source, summary, \
     detail_json";

/// Raw values of one ledger row before protocol conversion.
pub(crate) struct LedgerRow {
    pub(crate) id: i64,
    pub(crate) ts: String,
    pub(crate) kind: String,
    pub(crate) project: Option<String>,
    pub(crate) job_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) harness: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) usd: Option<f64>,
    pub(crate) tokens_in: i64,
    pub(crate) tokens_out: i64,
    pub(crate) tokens_reasoning: i64,
    pub(crate) tokens_cache_read: i64,
    pub(crate) tokens_cache_write: i64,
    pub(crate) cost_source: Option<String>,
    pub(crate) summary: String,
    pub(crate) detail_json: String,
}

/// Reads the textual columns of a ledger row.
pub(crate) fn read_ledger_row(row: &Row<'_>) -> rusqlite::Result<LedgerRow> {
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

/// Converts a raw ledger row into a protocol [`LedgerEntry`].
pub(crate) fn ledger_from_row(raw: LedgerRow) -> Result<LedgerEntry, StoreError> {
    let tokens = Tokens {
        input: u64_from_i64(raw.tokens_in, "ledger.tokens_in")?,
        output: u64_from_i64(raw.tokens_out, "ledger.tokens_out")?,
        reasoning: u64_from_i64(raw.tokens_reasoning, "ledger.tokens_reasoning")?,
        cache_read: u64_from_i64(raw.tokens_cache_read, "ledger.tokens_cache_read")?,
        cache_write: u64_from_i64(raw.tokens_cache_write, "ledger.tokens_cache_write")?,
    };
    let cost = match raw.cost_source {
        Some(source) => Some(vowp_protocol::Cost {
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
    /// `entry.project = None` writes `node_ledger`; `Some(p)` writes the bound project's
    /// `ledger` and returns [`StoreError::WrongProject`] when `p` is not the bound project.
    pub fn append_ledger(&self, entry: &LedgerEntry) -> Result<i64, StoreError> {
        let table = match &entry.project {
            Some(project) => {
                self.check_bound(project)?;
                "project.ledger"
            }
            None => "main.node_ledger",
        };
        let tokens = entry.cost.as_ref().map(|cost| cost.tokens).unwrap_or_default();
        let (usd, cost_source) = match &entry.cost {
            Some(cost) => (cost.usd, Some(enum_to_text(&cost.source)?)),
            None => (None, None),
        };
        self.conn.execute(
            &format!(
                "INSERT INTO {table} (
                    ts, kind, project, job_id, run_id, session_id, role, provider, harness, model, usd,
                    tokens_in, tokens_out, tokens_reasoning, tokens_cache_read, tokens_cache_write,
                    cost_source, summary, detail_json
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )"
            ),
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

    /// Lists ledger rows matching `filter`, ascending by timestamp then id.
    ///
    /// The bound project's rows and node-wide rows (whose `project` is `NULL` or names the
    /// bound project) are merged.
    pub fn ledger_since(&self, filter: &LedgerFilter) -> Result<Vec<LedgerEntry>, StoreError> {
        let bound = self.bound_project().cloned();
        let mut entries = Vec::new();
        if bound.is_some() {
            let (project_sql, project_values) = ledger_filter_sql("project.ledger", filter, None, true)?;
            entries.extend(collect_ledger_entries(&self.conn, &project_sql, project_values)?);
        }
        let (node_sql, node_values) = ledger_filter_sql("main.node_ledger", filter, bound.as_ref(), true)?;
        entries.extend(collect_ledger_entries(&self.conn, &node_sql, node_values)?);
        entries.sort_by(|left, right| {
            left.ts
                .cmp(&right.ts)
                .then_with(|| left.id.unwrap_or_default().cmp(&right.id.unwrap_or_default()))
        });
        if let Some(limit) = filter.limit {
            entries.truncate(limit as usize);
        }
        Ok(entries)
    }

    /// Sums `kind = 'cost'` rows grouped by one dimension across both files.
    ///
    /// Rows whose key column is `NULL` fall into the key `"(none)"`; `Day` groups by the UTC
    /// date (`YYYY-MM-DD`) of `ts`. Ordering is known USD descending, then key ascending.
    pub fn cost_totals(&self, query: &CostQuery, group: CostGroup) -> Result<Vec<CostRow>, StoreError> {
        let bound = match &query.project {
            Some(project) => Some(self.check_bound(project)?),
            None => self.bound_project().cloned(),
        };
        let filter = LedgerFilter {
            kind: Some(LedgerKind::Cost),
            job_id: query.job_id.clone(),
            since: query.since,
            limit: None,
        };
        let mut entries = Vec::new();
        if bound.is_some() {
            let (project_sql, project_values) = ledger_filter_sql("project.ledger", &filter, None, true)?;
            entries.extend(collect_ledger_entries(&self.conn, &project_sql, project_values)?);
        }
        let (node_sql, node_values) = ledger_filter_sql("main.node_ledger", &filter, bound.as_ref(), false)?;
        entries.extend(collect_ledger_entries(&self.conn, &node_sql, node_values)?);
        entries.retain(|entry| {
            if let Some(until) = query.until {
                entry.ts <= until
            } else {
                true
            }
        });

        let mut totals: BTreeMap<String, CostRow> = BTreeMap::new();
        for entry in entries {
            let key = match group {
                CostGroup::Job => entry.job_id.as_ref().map(JobId::as_str).unwrap_or("(none)").to_owned(),
                CostGroup::Role => entry
                    .role
                    .as_ref()
                    .map(|role| enum_to_text(role).unwrap_or_else(|_| "(none)".to_owned()))
                    .unwrap_or_else(|| "(none)".to_owned()),
                CostGroup::Provider => entry.provider.clone().unwrap_or_else(|| "(none)".to_owned()),
                CostGroup::Model => entry.model.clone().unwrap_or_else(|| "(none)".to_owned()),
                CostGroup::Day => entry.ts.as_rfc3339()[..10].to_owned(),
            };
            let row = totals.entry(key.clone()).or_insert_with(|| CostRow {
                key,
                usd: 0.0,
                receipt_entries: 0,
                price_table_entries: 0,
                unknown_entries: 0,
                tokens: Tokens::default(),
                entries: 0,
            });
            if let Some(cost) = &entry.cost {
                if let Some(usd) = cost.usd {
                    row.usd += usd;
                }
                match cost.source {
                    CostSource::Receipt => row.receipt_entries += 1,
                    CostSource::PriceTable => row.price_table_entries += 1,
                    CostSource::Unknown => row.unknown_entries += 1,
                }
                row.tokens.input += cost.tokens.input;
                row.tokens.output += cost.tokens.output;
                row.tokens.reasoning += cost.tokens.reasoning;
                row.tokens.cache_read += cost.tokens.cache_read;
                row.tokens.cache_write += cost.tokens.cache_write;
            } else {
                row.unknown_entries += 1;
            }
            row.entries += 1;
        }
        let mut rows: Vec<CostRow> = totals.into_values().collect();
        rows.sort_by(|left, right| {
            right
                .usd
                .partial_cmp(&left.usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.key.cmp(&right.key))
        });
        Ok(rows)
    }
}

/// Builds one filtered ledger query and its bound values.
fn ledger_filter_sql(
    table: &str,
    filter: &LedgerFilter,
    node_project: Option<&ProjectName>,
    include_null_node: bool,
) -> Result<(String, Vec<Box<dyn ToSql>>), StoreError> {
    let mut query = format!("SELECT {LEDGER_COLUMNS} FROM {table} WHERE 1 = 1");
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
    if let Some(project) = node_project {
        if include_null_node {
            query.push_str(" AND (project IS NULL OR project = ?)");
        } else {
            query.push_str(" AND project = ?");
        }
        values.push(Box::new(project.as_str().to_owned()));
    }
    query.push_str(" ORDER BY ts, id");
    Ok((query, values))
}

/// Runs one ledger query and converts its rows.
fn collect_ledger_entries(
    conn: &rusqlite::Connection,
    sql: &str,
    values: Vec<Box<dyn ToSql>>,
) -> Result<Vec<LedgerEntry>, StoreError> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(params_from_iter(values), read_ledger_row)?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(ledger_from_row(row?)?);
    }
    Ok(entries)
}

/// Converts one token count into SQLite's signed integer form.
fn token_i64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|err| crate::convert::corrupt(what, err))
}
