//! Human-readable night report and the short checkpoint text L1 writes every 120 minutes.
//! Spec: `docs/design/crates/vowp-core.md` §5 and `docs/design/loops.md` §1 step 12.
//!
//! Both reports are Markdown, recomputed from the database and deterministic: rows are sorted,
//! timestamps use the canonical RFC 3339 form, and unknown costs are shown as `?` rather than
//! zero.

use std::fmt::Write as _;

use vowp_protocol::{InboxState, Job, JobState, LedgerKind, Timestamp, Tokens, VowpConfig};
use vowp_store::{CostGroup, CostQuery, JobFilter, LedgerFilter, Store};

use crate::CoreError;

use super::board::limit_kind_label;

/// Builds the night report: built, failed+why, cost, limits/failover, switches and inbox.
///
/// The window starts at midnight UTC of `now`.
pub fn night_report(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<String, CoreError> {
    let since = start_of_day(now);
    let project = cfg.project.name.clone();
    let jobs = store.list_jobs(&JobFilter {
        project: Some(project.clone()),
        ..JobFilter::default()
    })?;

    let mut built: Vec<&Job> = jobs
        .iter()
        .filter(|job| job.state == JobState::Done && job.state_changed_at >= since)
        .collect();
    built.sort_by(|left, right| left.id.cmp(&right.id));
    let mut failed: Vec<&Job> = jobs
        .iter()
        .filter(|job| job.state == JobState::Failed && job.state_changed_at >= since)
        .collect();
    failed.sort_by(|left, right| left.id.cmp(&right.id));

    let cost_rows = store.cost_totals(
        &CostQuery {
            since: Some(since),
            project: Some(project.clone()),
            ..CostQuery::default()
        },
        CostGroup::Provider,
    )?;
    let limits = store.active_limits(now)?;
    let limit_events = ledger_of(store, LedgerKind::Limit, since)?;
    let switches = ledger_of(store, LedgerKind::Switch, since)?;
    let inbox = store.list_inbox(None)?;
    let open: Vec<_> = inbox.iter().filter(|item| item.state == InboxState::Open).collect();
    let answered = inbox
        .iter()
        .filter(|item| item.state == InboxState::Answered && item.answered_at.is_some_and(|at| at >= since))
        .count();

    let mut out = String::new();
    let _ = writeln!(out, "# Night report — {project}");
    let _ = writeln!(out);
    let _ = writeln!(out, "Generated {} (since {})", now.as_rfc3339(), since.as_rfc3339());
    let _ = writeln!(out);

    let _ = writeln!(out, "## Built");
    if built.is_empty() {
        let _ = writeln!(out, "- none");
    } else {
        for job in &built {
            let _ = writeln!(out, "- {}: {}", job.id, job.title);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Failed");
    if failed.is_empty() {
        let _ = writeln!(out, "- none");
    } else {
        for job in &failed {
            let why = if job.note.is_empty() {
                "no note recorded"
            } else {
                job.note.as_str()
            };
            let _ = writeln!(out, "- {}: {} — {why}", job.id, job.title);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Cost");
    if cost_rows.is_empty() {
        let _ = writeln!(out, "- none");
    } else {
        for row in &cost_rows {
            let amount = if row.receipt_entries + row.price_table_entries == 0 {
                "?".to_owned()
            } else {
                format!("{:.2} USD", row.usd)
            };
            let _ = writeln!(
                out,
                "- {}: {amount} receipt={} price-table={} unknown={}",
                row.key, row.receipt_entries, row.price_table_entries, row.unknown_entries
            );
        }
    }
    let total = CostSummary::from_rows(&cost_rows);
    let _ = writeln!(out, "Total: {}", total.render());
    let _ = writeln!(out);

    let _ = writeln!(out, "## Limits and failover");
    let _ = writeln!(out, "- limit events: {}", limit_events.len());
    let _ = writeln!(out, "- failover switches: {}", switches.len());
    if limits.is_empty() {
        let _ = writeln!(out, "- no active limits");
    } else {
        for limit in &limits {
            let resets = limit
                .resets_at
                .map(|at| format!("until {}", at.as_rfc3339()))
                .unwrap_or_else(|| "until resolved by a human".to_owned());
            let detail = if limit.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", limit.detail)
            };
            let _ = writeln!(
                out,
                "- {} {} {resets}{detail}",
                limit.provider,
                limit_kind_label(limit.kind)
            );
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Switches");
    if switches.is_empty() {
        let _ = writeln!(out, "- none");
    } else {
        for entry in &switches {
            let _ = writeln!(out, "- {}: {}", entry.ts.as_rfc3339(), entry.summary);
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Inbox");
    let _ = writeln!(out, "Open: {}", open.len());
    for item in &open {
        let job = item
            .job_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned());
        let kind = inbox_kind_label(item.kind);
        let _ = writeln!(out, "- [{kind}] {job}: {}", item.title);
    }
    let _ = writeln!(out, "Answered: {answered}");

    Ok(out)
}

/// Builds the short checkpoint text L1 stores under `<project>/.vowp/checkpoints/<ts>.md`.
pub fn checkpoint(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<String, CoreError> {
    let project = cfg.project.name.clone();
    let counts = store.count_jobs_by_state(Some(&project))?;
    let since = start_of_day(now);
    let cost_rows = store.cost_totals(
        &CostQuery {
            since: Some(since),
            project: Some(project.clone()),
            ..CostQuery::default()
        },
        CostGroup::Provider,
    )?;
    let total = CostSummary::from_rows(&cost_rows);
    let limits = store.active_limits(now)?;
    let open_inbox = store.list_inbox(Some(InboxState::Open))?.len();
    let count = |state: &str| counts.get(state).copied().unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(out, "# Checkpoint — {project}");
    let _ = writeln!(out, "At {}", now.as_rfc3339());
    let _ = writeln!(
        out,
        "done {} · failed {} · running {} · limits {} · inbox {} open · cost {}",
        count("done"),
        count("failed"),
        count("running"),
        limits.len(),
        open_inbox,
        total.render()
    );
    Ok(out)
}

/// Sums one cost-total result set for the report line.
struct CostSummary {
    usd: f64,
    tokens: Tokens,
    receipt: u64,
    price_table: u64,
    unknown: u64,
}

impl CostSummary {
    /// Builds the summary from `store.cost_totals` rows.
    fn from_rows(rows: &[vowp_store::CostRow]) -> Self {
        let mut summary = Self {
            usd: 0.0,
            tokens: Tokens::default(),
            receipt: 0,
            price_table: 0,
            unknown: 0,
        };
        for row in rows {
            summary.usd += row.usd;
            summary.tokens.add(&row.tokens);
            summary.receipt += row.receipt_entries;
            summary.price_table += row.price_table_entries;
            summary.unknown += row.unknown_entries;
        }
        summary
    }

    /// Formats the amount and its provenance counts; `?` means no known amount.
    fn render(&self) -> String {
        let amount = if self.receipt + self.price_table == 0 {
            "?".to_owned()
        } else {
            format!("{:.2} USD", self.usd)
        };
        format!(
            "{amount} receipt={} price-table={} unknown={}",
            self.receipt, self.price_table, self.unknown
        )
    }
}

/// Lists one ledger kind since `since`, ascending by id.
fn ledger_of(store: &Store, kind: LedgerKind, since: Timestamp) -> Result<Vec<vowp_protocol::LedgerEntry>, CoreError> {
    Ok(store.ledger_since(&LedgerFilter {
        kind: Some(kind),
        job_id: None,
        since: Some(since),
        limit: None,
    })?)
}

/// Returns the canonical label of an inbox kind.
fn inbox_kind_label(kind: vowp_protocol::InboxKind) -> &'static str {
    match kind {
        vowp_protocol::InboxKind::Question => "question",
        vowp_protocol::InboxKind::Approval => "approval",
        vowp_protocol::InboxKind::Decision => "decision",
        vowp_protocol::InboxKind::Notice => "notice",
    }
}

/// Returns midnight UTC of `now`.
fn start_of_day(now: Timestamp) -> Timestamp {
    let secs = now.unix_secs();
    Timestamp::from_unix_secs(secs - secs.rem_euclid(86_400))
}
