//! `cost`: aggregate the cost ledger by job, role, provider, model or day.
//!
//! Spec: `docs/design/cli.md` §5. Unknown costs are rendered as `?` and counted separately;
//! they are never treated as a zero-dollar run.

use serde::Serialize;
use time::macros::format_description;
use vowp_protocol::{JobId, Tokens};
use vowp_store::{CostGroup, CostQuery, CostRow};

use crate::cli::CostArgs;
use crate::output::{Table, print_json};
use crate::{CliError, Ctx, IoStreams};

/// One rendered cost row.
#[derive(Debug, Serialize)]
struct CostRowOut {
    key: String,
    runs: u64,
    usd: Option<f64>,
    tokens: Tokens,
    source: &'static str,
    unknown_entries: u64,
}

/// The totals row.
#[derive(Debug, Serialize)]
struct CostTotalOut {
    runs: u64,
    usd: Option<f64>,
    tokens: Tokens,
    source: &'static str,
    unknown_entries: u64,
}

/// The JSON document printed by `vowp cost --json`.
#[derive(Debug, Serialize)]
struct CostReport {
    by: &'static str,
    rows: Vec<CostRowOut>,
    total: CostTotalOut,
}

/// Prints cost totals.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &CostArgs) -> Result<i32, CliError> {
    let (group, by_name) = parse_group(args.by.as_deref())?;
    let since = match &args.since {
        Some(value) => Some(parse_since(value, ctx.clock().now())?),
        None => None,
    };
    let job = args.job.as_deref().map(JobId::parse).transpose()?;
    let store = ctx.store()?;
    let query = CostQuery {
        since,
        until: None,
        project: Some(ctx.project().clone()),
        job_id: job,
    };
    let rows = store.cost_totals(&query, group)?;
    let report = build_report(by_name, rows);

    if ctx.json() {
        print_json(io, &report)?;
        return Ok(0);
    }

    let mut table = Table::new(&["KEY", "RUNS", "USD", "INPUT", "OUTPUT", "CACHE_R", "SOURCE"]);
    for row in &report.rows {
        table.row(vec![
            row.key.clone(),
            row.runs.to_string(),
            usd_cell(row.usd, row.runs, row.unknown_entries),
            row.tokens.input.to_string(),
            row.tokens.output.to_string(),
            row.tokens.cache_read.to_string(),
            row.source.to_owned(),
        ]);
    }
    table.row(vec![
        "TOTAL".to_owned(),
        report.total.runs.to_string(),
        usd_cell(report.total.usd, report.total.runs, report.total.unknown_entries),
        report.total.tokens.input.to_string(),
        report.total.tokens.output.to_string(),
        report.total.tokens.cache_read.to_string(),
        report.total.source.to_owned(),
    ]);
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    if report.total.unknown_entries > 0 {
        writeln!(
            io.out(),
            "unknown costs: {} entries (shown as ?, never counted as zero)",
            report.total.unknown_entries
        )?;
    }
    Ok(0)
}

/// Aggregates rows into the documented JSON shape.
fn build_report(by: &'static str, rows: Vec<CostRow>) -> CostReport {
    let mut total_tokens = Tokens::default();
    let mut total_usd = 0.0_f64;
    let mut total_runs = 0_u64;
    let mut receipt = 0_u64;
    let mut price_table = 0_u64;
    let mut unknown = 0_u64;

    let rows = rows
        .into_iter()
        .map(|row| {
            total_tokens.add(&row.tokens);
            total_usd += row.usd;
            total_runs += row.entries;
            receipt += row.receipt_entries;
            price_table += row.price_table_entries;
            unknown += row.unknown_entries;
            CostRowOut {
                key: row.key,
                runs: row.entries,
                usd: known_usd(row.usd, row.entries, row.unknown_entries),
                tokens: row.tokens,
                source: source_name(row.receipt_entries, row.price_table_entries, row.unknown_entries),
                unknown_entries: row.unknown_entries,
            }
        })
        .collect();

    CostReport {
        by,
        rows,
        total: CostTotalOut {
            runs: total_runs,
            usd: known_usd(total_usd, total_runs, unknown),
            tokens: total_tokens,
            source: source_name(receipt, price_table, unknown),
            unknown_entries: unknown,
        },
    }
}

/// Returns the known USD amount, or `None` when every entry is unknown.
fn known_usd(usd: f64, entries: u64, unknown: u64) -> Option<f64> {
    if entries > 0 && unknown >= entries {
        None
    } else {
        Some(usd)
    }
}

/// Derives the SOURCE column from the receipt/price-table/unknown entry counts.
fn source_name(receipt: u64, price_table: u64, unknown: u64) -> &'static str {
    let present = [receipt > 0, price_table > 0, unknown > 0]
        .into_iter()
        .filter(|present| *present)
        .count();
    match (present, receipt > 0, price_table > 0, unknown > 0) {
        (0, _, _, _) => "unknown",
        (1, true, false, false) => "receipt",
        (1, false, true, false) => "price-table",
        (1, false, false, true) => "unknown",
        _ => "mixed",
    }
}

/// Renders one USD cell, showing unknown amounts as `?`.
fn usd_cell(usd: Option<f64>, entries: u64, unknown: u64) -> String {
    match usd {
        None => "?".to_owned(),
        Some(value) if unknown > 0 && unknown < entries => format!("{value:.4}+?"),
        Some(value) => format!("{value:.4}"),
    }
}

/// Parses `--by`.
fn parse_group(value: Option<&str>) -> Result<(CostGroup, &'static str), CliError> {
    match value.unwrap_or("job") {
        "job" => Ok((CostGroup::Job, "job")),
        "role" => Ok((CostGroup::Role, "role")),
        "provider" => Ok((CostGroup::Provider, "provider")),
        "model" => Ok((CostGroup::Model, "model")),
        "day" => Ok((CostGroup::Day, "day")),
        other => Err(CliError::Invalid(format!(
            "unknown cost dimension: {other} (job, role, provider, model or day)"
        ))),
    }
}

/// Parses `--since` as a duration (`30m`, `7d`) or a `YYYY-MM-DD` date.
fn parse_since(value: &str, now: vowp_protocol::Timestamp) -> Result<vowp_protocol::Timestamp, CliError> {
    let text = value.trim();
    if let Some(timestamp) = parse_duration(text, now) {
        return Ok(timestamp);
    }
    if let Ok(date) = time::Date::parse(text, format_description!("[year]-[month]-[day]")) {
        return Ok(vowp_protocol::Timestamp::from_unix_secs(
            date.midnight().assume_utc().unix_timestamp(),
        ));
    }
    if let Ok(timestamp) = vowp_protocol::Timestamp::parse(text) {
        return Ok(timestamp);
    }
    Err(CliError::Invalid(format!(
        "invalid --since {value:?}; use a duration (30m, 24h, 7d) or YYYY-MM-DD"
    )))
}

/// Parses a compact duration suffix into an instant before `now`.
fn parse_duration(value: &str, now: vowp_protocol::Timestamp) -> Option<vowp_protocol::Timestamp> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_u64)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_u64)
    } else if let Some(digits) = value.strip_suffix('h') {
        (digits, 3_600_u64)
    } else if let Some(digits) = value.strip_suffix('d') {
        (digits, 86_400_u64)
    } else {
        let digits = value.strip_suffix('w')?;
        (digits, 604_800_u64)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let count: u64 = digits.parse().ok()?;
    let seconds = i64::try_from(count.checked_mul(multiplier)?).ok()?;
    Some(vowp_protocol::Timestamp::from_unix_secs(
        now.unix_secs().saturating_sub(seconds),
    ))
}
