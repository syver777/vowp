//! Planner/keeper digest: the compact state snapshot fed to one L4/L5 turn.
//!
//! Spec: `docs/design/loops.md` §7 "Digest". The digest is at most `max_chars` characters
//! (default 24 000) and every item is clipped to 600 characters. It lists what changed since
//! the previous turn, gate evidence and report open items, inbox state, limits, counts, cost,
//! slots, recent ledger errors and — for the planner — the last checkpoint. The keeper digest
//! also includes the last 60 indexed events of each stalled job's session.

use std::fmt::Write as _;

use vowp_protocol::{JobState, LedgerKind, Limit, Role, RunRole, Timestamp, VowpConfig};
use vowp_store::{CostGroup, CostQuery, JobFilter, LedgerFilter, SessionFilter, Store};

use crate::CoreError;
use crate::ledger::BoardSnapshot;

/// Maximum characters in one digest item.
const ITEM_MAX_CHARS: usize = 600;

/// Builds planner and keeper digests.
pub struct Digest;

impl Digest {
    /// Builds the digest for `role` covering events since `since`, capped at `max_chars`.
    ///
    /// A `role` other than Planner or Keeper is treated like a keeper digest without the
    /// stalled-job tails; the loops only call the two real roles.
    pub fn build(
        store: &Store,
        cfg: &VowpConfig,
        role: Role,
        since: Timestamp,
        max_chars: usize,
    ) -> Result<String, CoreError> {
        let now = store.clock().now();
        let project = &cfg.project.name;
        let mut text = String::new();
        let role_name = match role {
            Role::Planner => "planner",
            Role::Keeper => "keeper",
            _ => "operations",
        };
        let _ = writeln!(
            text,
            "# {role_name} digest for project {project} at {}",
            now.as_rfc3339()
        );
        let _ = writeln!(text, "Events since {}", since.as_rfc3339());

        let snapshot = BoardSnapshot::collect(store, cfg, now)?;
        let jobs = store.list_jobs(&JobFilter {
            project: Some(project.clone()),
            ..JobFilter::default()
        })?;

        text.push_str("\n## Jobs changed\n");
        let mut changed = 0;
        for job in &jobs {
            if job.state_changed_at < since {
                continue;
            }
            changed += 1;
            let _ = writeln!(
                text,
                "- {} {} ({}) at {}",
                job.id,
                job.state.as_str(),
                job.title,
                job.state_changed_at.as_rfc3339()
            );
            if matches!(
                job.state,
                JobState::Done | JobState::Failed | JobState::Stalled | JobState::Parked
            ) {
                let results = store.gate_results_for(&job.id, job.attempts).unwrap_or_default();
                for result in results.iter().filter(|result| !result.passed) {
                    let _ = writeln!(
                        text,
                        "  - gate {} failed: {}",
                        result.gate_index,
                        clip(&format!("{:?}", result.evidence), ITEM_MAX_CHARS)
                    );
                }
                if let Some(report) = read_report(store, cfg, job) {
                    for item in report.front_matter.open_items.iter().take(8) {
                        let _ = writeln!(text, "  - open item: {}", clip(item, ITEM_MAX_CHARS));
                    }
                }
            }
        }
        if changed == 0 {
            text.push_str("- (none)\n");
        }

        text.push_str("\n## Inbox\n");
        let inbox = store.list_inbox(None)?;
        if inbox.is_empty() {
            text.push_str("- (empty)\n");
        }
        for item in &inbox {
            let state = match item.state {
                vowp_protocol::InboxState::Open => "open",
                vowp_protocol::InboxState::Answered => "answered",
                vowp_protocol::InboxState::Dismissed => "dismissed",
            };
            let _ = writeln!(
                text,
                "- [{state}] {:?} {}: {}",
                item.kind,
                item.title,
                clip(item.answer.as_deref().unwrap_or(&item.body), ITEM_MAX_CHARS)
            );
        }

        text.push_str("\n## Limits\n");
        if snapshot.limits.is_empty() {
            text.push_str("- (none)\n");
        }
        for limit in &snapshot.limits {
            let _ = writeln!(
                text,
                "- {} {:?}{}: {}",
                limit.provider,
                limit.kind,
                limit
                    .resets_at
                    .map(|at| format!(" until {}", at.as_rfc3339()))
                    .unwrap_or_default(),
                clip(&limit.detail, ITEM_MAX_CHARS)
            );
        }

        text.push_str("\n## Counts\n");
        if snapshot.jobs_by_state.is_empty() {
            text.push_str("- (no jobs)\n");
        }
        for (state, count) in &snapshot.jobs_by_state {
            let _ = writeln!(text, "- {state}: {count}");
        }

        text.push_str("\n## Cost\n");
        let cost_rows = store.cost_totals(
            &CostQuery {
                project: Some(project.clone()),
                ..CostQuery::default()
            },
            CostGroup::Role,
        )?;
        if cost_rows.is_empty() {
            text.push_str("- (no cost rows)\n");
        }
        for row in &cost_rows {
            let _ = writeln!(
                text,
                "- {}: ${:.4} (receipts {}, price-table {}, unknown {})",
                row.key, row.usd, row.receipt_entries, row.price_table_entries, row.unknown_entries
            );
        }

        text.push_str("\n## Slots\n");
        let _ = writeln!(
            text,
            "- {}/{} busy; planner {:?}, keeper {:?}",
            snapshot.slots_busy,
            snapshot.slots_total,
            snapshot.planner_executor.as_ref().map(ToString::to_string),
            snapshot.keeper_executor.as_ref().map(ToString::to_string)
        );

        text.push_str("\n## Recent errors\n");
        let ledger = store.ledger_since(&LedgerFilter {
            since: Some(since),
            ..LedgerFilter::default()
        })?;
        let errors = ledger
            .iter()
            .filter(|entry| {
                entry.kind == LedgerKind::Note
                    || entry.summary.to_ascii_lowercase().contains("error")
                    || entry.summary.to_ascii_lowercase().contains("failed")
            })
            .rev()
            .take(5)
            .collect::<Vec<_>>();
        if errors.is_empty() {
            text.push_str("- (none)\n");
        }
        for entry in errors {
            let _ = writeln!(
                text,
                "- {} {}: {}",
                entry.ts.as_rfc3339(),
                entry.kind_name(),
                clip(&entry.summary, ITEM_MAX_CHARS)
            );
        }

        if role == Role::Planner {
            text.push_str("\n## Checkpoint\n");
            match crate::ledger::report::checkpoint(store, cfg, now) {
                Ok(checkpoint) => text.push_str(&clip(&checkpoint, 4000)),
                Err(error) => {
                    let _ = write!(text, "(checkpoint unavailable: {error})");
                }
            }
            text.push('\n');
        }

        if role == Role::Keeper {
            append_stalled_tails(store, project, &jobs, &mut text)?;
        }

        Ok(clip(&text, max_chars))
    }
}

/// Appends the last 60 indexed events of every stalled job's newest session.
fn append_stalled_tails(
    store: &Store,
    project: &vowp_protocol::ProjectName,
    jobs: &[vowp_protocol::Job],
    text: &mut String,
) -> Result<(), CoreError> {
    text.push_str("\n## Stalled session tails\n");
    let mut any = false;
    for job in jobs.iter().filter(|job| job.state == JobState::Stalled) {
        let sessions = store.list_sessions(&SessionFilter {
            project: Some(project.clone()),
            job_id: Some(job.id.clone()),
            limit: Some(1),
            ..SessionFilter::default()
        })?;
        let Some(session) = sessions.first() else {
            continue;
        };
        any = true;
        let _ = writeln!(text, "### {}", job.id);
        let after = session.last_seq.saturating_sub(60);
        let rows = store.event_rows(&session.id, after, 60)?;
        if rows.is_empty() {
            text.push_str("(no indexed events)\n");
        }
        for row in rows {
            let _ = writeln!(text, "{}: {}", row.seq, clip(&row.summary, ITEM_MAX_CHARS));
        }
    }
    if !any {
        text.push_str("(none)\n");
    }
    Ok(())
}

/// Reads a job's report from its worktree, when it exists.
fn read_report(store: &Store, cfg: &VowpConfig, job: &vowp_protocol::Job) -> Option<vowp_protocol::Report> {
    let worktree = job
        .worktree
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| cfg.project.path.as_ref().map(std::path::PathBuf::from))?;
    let path = worktree.join(&job.report);
    let text = std::fs::read_to_string(path).ok()?;
    let _ = store;
    vowp_protocol::Report::parse(&text).ok()
}

/// Clips one text block to `max` characters.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// Extra name used by the digest for ledger kinds.
trait LedgerKindName {
    fn kind_name(&self) -> &'static str;
}

impl LedgerKindName for vowp_protocol::LedgerEntry {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            LedgerKind::Dispatch => "dispatch",
            LedgerKind::Gate => "gate",
            LedgerKind::Merge => "merge",
            LedgerKind::Cost => "cost",
            LedgerKind::Switch => "switch",
            LedgerKind::Limit => "limit",
            LedgerKind::Promote => "promote",
            LedgerKind::Rollback => "rollback",
            LedgerKind::Inbox => "inbox",
            LedgerKind::Schedule => "schedule",
            LedgerKind::State => "state",
            LedgerKind::Note => "note",
        }
    }
}

/// Returns the run role name used in digest text (kept for callers/tests).
pub fn run_role_name(role: RunRole) -> &'static str {
    match role {
        RunRole::Planner => "planner",
        RunRole::Keeper => "keeper",
        RunRole::Worker => "worker",
        RunRole::Verifier => "verifier",
        RunRole::Capsule => "capsule",
        RunRole::Probe => "probe",
        RunRole::Merge => "merge",
        RunRole::Gate => "gate",
    }
}

/// Returns the reset text for one limit row.
pub fn limit_until(limit: &Limit) -> String {
    limit
        .resets_at
        .map(|at| at.as_rfc3339())
        .unwrap_or_else(|| "manual".to_owned())
}
