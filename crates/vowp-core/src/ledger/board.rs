//! `vowp board` snapshot: job counts, running runs, slots, limits, today's cost and inbox.
//! Spec: `docs/design/crates/vowp-core.md` §5 and `docs/design/loops.md` §1 step 12.
//!
//! The snapshot is recomputed from the database; `~/.vowp/board.json` written by L1 is only a
//! convenience copy. Rendering is deterministic so tests can pin it as a golden string.
//!
//! Note: `types.md` spells `jobs_by_state` as `BTreeMap<JobState, u32>`, but `JobState` does
//! not implement `Ord` in `vowp-protocol`, so the map is keyed by the canonical snake_case
//! state name.

use std::collections::BTreeMap;

use serde::Serialize;
use vowp_protocol::{
    Cost, CostSource, Executor, InboxState, JobId, Limit, LimitRowKind, NodeName, ProjectName, ProjectSnapshot,
    ProjectState, Timestamp, Tokens, VowpConfig, WorkerState,
};
use vowp_store::{CostGroup, CostQuery, JobFilter, Store};

use crate::CoreError;
use crate::deps::Deps;

/// One running job shown on the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRun {
    /// Job id.
    pub job: JobId,
    /// Executor of the run, when the job has a live run.
    pub executor: Option<Executor>,
    /// Seconds since the run (or the state change) started.
    pub age_secs: u64,
    /// Seconds since the session's last recorded event, when a session exists.
    pub last_event_age_secs: Option<u64>,
}

/// One point-in-time view of the project board.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardSnapshot {
    /// Project the snapshot covers.
    pub project: ProjectName,
    /// Instant the snapshot was taken.
    pub at: Timestamp,
    /// Job counts keyed by canonical state name (see the module note).
    pub jobs_by_state: BTreeMap<String, u32>,
    /// Jobs currently holding a run or worktree lock.
    pub running: Vec<BoardRun>,
    /// Worker and verifier slots currently busy.
    pub slots_busy: u32,
    /// Worker and verifier slots configured.
    pub slots_total: u32,
    /// First rung of the `planner` profile, when configured.
    pub planner_executor: Option<Executor>,
    /// First rung of the `keeper` profile, when configured.
    pub keeper_executor: Option<Executor>,
    /// Provider limits still in force.
    pub limits: Vec<Limit>,
    /// Cost recorded today (UTC).
    pub cost_today: Cost,
    /// Open inbox items.
    pub inbox_open: u32,
    /// Newest checkpoint file, when one exists.
    pub last_checkpoint: Option<Timestamp>,
}

impl BoardSnapshot {
    /// Recomputes the snapshot from `store`, `cfg` and `now`.
    pub fn collect(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<Self, CoreError> {
        let project = cfg.project.name.clone();
        let jobs_by_state = store.count_jobs_by_state(Some(&project))?;
        let jobs = store.list_jobs(&JobFilter {
            project: Some(project.clone()),
            ..JobFilter::default()
        })?;
        let open_runs = store.open_runs()?;

        let mut running = Vec::new();
        for job in jobs.iter().filter(|job| job.state.holds_lock()) {
            let run = open_runs.iter().rev().find(|run| run.job_id.as_ref() == Some(&job.id));
            let (executor, started_at, last_event_age_secs) = match run {
                Some(run) => {
                    let last_event_age = match &run.session_id {
                        Some(session_id) => store
                            .session(session_id)?
                            .map(|session| now.since(session.updated_at).as_secs()),
                        None => None,
                    };
                    (run.executor.clone(), run.started_at, last_event_age)
                }
                None => (None, job.state_changed_at, None),
            };
            running.push(BoardRun {
                job: job.id.clone(),
                executor,
                age_secs: now.since(started_at).as_secs(),
                last_event_age_secs,
            });
        }
        running.sort_by(|left, right| left.job.cmp(&right.job));

        let workers = store.list_workers(&cfg.node.name)?;
        let slots_busy = workers
            .iter()
            .filter(|worker| worker.state == WorkerState::Busy)
            .count()
            .try_into()
            .unwrap_or(u32::MAX);

        Ok(Self {
            project,
            at: now,
            jobs_by_state,
            running,
            slots_busy,
            slots_total: cfg.node.max_workers + cfg.node.max_verifiers,
            planner_executor: profile_head(cfg, "planner"),
            keeper_executor: profile_head(cfg, "keeper"),
            limits: store.active_limits(now)?,
            cost_today: cost_today(store, cfg, now)?,
            inbox_open: store
                .list_inbox(Some(InboxState::Open))?
                .len()
                .try_into()
                .unwrap_or(u32::MAX),
            last_checkpoint: last_checkpoint(cfg),
        })
    }

    /// Renders the snapshot for the `vowp board` window.
    ///
    /// With `color`, the header is bold and running job ids are green; without it the output
    /// contains no ANSI escapes.
    pub fn render_text(&self, color: bool) -> String {
        let mut out = String::new();
        if color {
            out.push_str("\x1b[1m");
        }
        out.push_str(&format!("{} @ {}", self.project, self.at.as_rfc3339()));
        if color {
            out.push_str("\x1b[0m");
        }
        out.push('\n');

        if self.jobs_by_state.is_empty() {
            out.push_str("jobs: none\n");
        } else {
            let counts: Vec<String> = self
                .jobs_by_state
                .iter()
                .map(|(state, count)| format!("{state} {count}"))
                .collect();
            out.push_str(&format!("jobs: {}\n", counts.join(", ")));
        }

        if self.running.is_empty() {
            out.push_str("running: none\n");
        } else {
            out.push_str("running:\n");
            for run in &self.running {
                let job = if color {
                    format!("\x1b[32m{}\x1b[0m", run.job)
                } else {
                    run.job.to_string()
                };
                let executor = executor_label(run.executor.as_ref());
                let last_event = run
                    .last_event_age_secs
                    .map(|secs| format!("{secs}s"))
                    .unwrap_or_else(|| "-".to_owned());
                out.push_str(&format!("  {job}  {executor}  {}s  {last_event}\n", run.age_secs));
            }
        }

        out.push_str(&format!("slots: {}/{}\n", self.slots_busy, self.slots_total));
        out.push_str(&format!(
            "planner: {}\n",
            executor_label(self.planner_executor.as_ref())
        ));
        out.push_str(&format!("keeper: {}\n", executor_label(self.keeper_executor.as_ref())));

        if self.limits.is_empty() {
            out.push_str("limits: none\n");
        } else {
            out.push_str("limits:\n");
            for limit in &self.limits {
                let resets = limit
                    .resets_at
                    .map(|at| format!("resets {}", at.as_rfc3339()))
                    .unwrap_or_else(|| "until resolved".to_owned());
                out.push_str(&format!(
                    "  {} {} {resets}\n",
                    limit.provider,
                    limit_kind_label(limit.kind)
                ));
            }
        }

        out.push_str(&format!("cost today: {}\n", cost_amount(&self.cost_today)));
        out.push_str(&format!("inbox: {} open\n", self.inbox_open));
        match self.last_checkpoint {
            Some(at) => out.push_str(&format!("checkpoint: {}\n", at.as_rfc3339())),
            None => out.push_str("checkpoint: -\n"),
        }
        out
    }
}

/// One node-wide board frame, built from the snapshot's project cards.
///
/// With one project the CLI keeps using [`BoardSnapshot`] so its output stays byte-compatible
/// with v0.1; this view is the multi-project board and adds a project column.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FleetBoard {
    /// Home the board describes.
    pub home: NodeName,
    /// Instant the frame was taken.
    pub at: Timestamp,
    /// Project cards in registry order.
    pub projects: Vec<ProjectSnapshot>,
    /// Provider limits still in force.
    pub limits: Vec<Limit>,
    /// Open inbox items.
    pub inbox_open: u32,
}

impl FleetBoard {
    /// Builds one board frame from the derived snapshot.
    pub fn collect(deps: &Deps, store: &Store, now: Timestamp) -> Result<Self, CoreError> {
        let snapshot = crate::fleet::build_snapshot(deps, store, now)?;
        Ok(Self {
            home: snapshot.home,
            at: snapshot.generated_at,
            projects: snapshot.projects,
            limits: snapshot.limits,
            inbox_open: snapshot.inbox.len().try_into().unwrap_or(u32::MAX),
        })
    }

    /// Renders the multi-project board.
    ///
    /// Each project gets its own section with the project name as the heading; this is the
    /// spec's "project column" (§12.2 wording) without restructuring the v0.1 line shape.
    /// With `color`, the header is bold; without it the output contains no ANSI escapes.
    pub fn render_text(&self, color: bool) -> String {
        let mut out = String::new();
        if color {
            out.push_str("\x1b[1m");
        }
        out.push_str(&format!("{} @ {}", self.home, self.at.as_rfc3339()));
        if color {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
        out.push_str(&format!("projects: {}\n", self.projects.len()));
        for project in &self.projects {
            out.push_str(&format!("  {}  {}\n", project.name, project_state_label(project.state)));
            if project.jobs_by_state.is_empty() {
                out.push_str("    jobs: none\n");
            } else {
                let counts: Vec<String> = project
                    .jobs_by_state
                    .iter()
                    .map(|(state, count)| format!("{state} {count}"))
                    .collect();
                out.push_str(&format!("    jobs: {}\n", counts.join(", ")));
            }
            if !project.running.is_empty() {
                out.push_str("    running:\n");
                for run in &project.running {
                    let job = if color {
                        format!("\x1b[32m{}\x1b[0m", run.job)
                    } else {
                        run.job.to_string()
                    };
                    let executor = run.executor.as_deref().unwrap_or("-");
                    let last_event = run
                        .last_event_age_secs
                        .map(|secs| format!("{secs}s"))
                        .unwrap_or_else(|| "-".to_owned());
                    out.push_str(&format!("      {job}  {executor}  {}s  {last_event}\n", run.age_secs));
                }
            }
            if !project.queue.is_empty() {
                out.push_str("    queue:\n");
                for job in &project.queue {
                    out.push_str(&format!("      {}  {}  {}s", job.job, job.state.as_str(), job.age_secs));
                    if let Some(reason) = &job.waiting_reason {
                        out.push_str(&format!("  {reason}"));
                    }
                    out.push('\n');
                }
            }
            if !project.blocked.is_empty() {
                out.push_str("    blocked:\n");
                for job in &project.blocked {
                    out.push_str(&format!("      {}  {}  {}\n", job.job, job.reason, job.detail));
                }
            }
            if let Some(merge) = &project.last_merge {
                out.push_str(&format!("    last merge: {} {}\n", merge.job, merge.commit));
            }
            if !project.gate_failures.is_empty() {
                out.push_str(&format!("    gate failures: {}\n", project.gate_failures.len()));
            }
            out.push_str(&format!(
                "    cost today: {}\n",
                optional_cost_amount(project.cost_today_usd)
            ));
        }

        if self.limits.is_empty() {
            out.push_str("limits: none\n");
        } else {
            out.push_str("limits:\n");
            for limit in &self.limits {
                let resets = limit
                    .resets_at
                    .map(|at| format!("resets {}", at.as_rfc3339()))
                    .unwrap_or_else(|| "until resolved".to_owned());
                out.push_str(&format!(
                    "  {} {} {resets}\n",
                    limit.provider,
                    limit_kind_label(limit.kind)
                ));
            }
        }
        out.push_str(&format!("inbox: {} open\n", self.inbox_open));
        out
    }
}

/// Returns the lowercase name of a registry state.
fn project_state_label(state: ProjectState) -> &'static str {
    match state {
        ProjectState::Active => "active",
        ProjectState::Paused => "paused",
        ProjectState::Archived => "archived",
    }
}

/// Formats an optional USD amount for one line.
fn optional_cost_amount(usd: Option<f64>) -> String {
    match usd {
        Some(usd) => format!("{usd:.2} USD"),
        None => "? (unknown)".to_owned(),
    }
}

/// Returns the first rung of a named profile.
fn profile_head(cfg: &VowpConfig, name: &str) -> Option<Executor> {
    cfg.profiles
        .get(name)
        .and_then(|profile| profile.ladder.first().cloned())
}

/// Formats an optional executor for the board.
fn executor_label(executor: Option<&Executor>) -> String {
    executor.map(ToString::to_string).unwrap_or_else(|| "-".to_owned())
}

/// Returns the canonical label of a limit row kind.
pub(crate) fn limit_kind_label(kind: LimitRowKind) -> &'static str {
    match kind {
        LimitRowKind::Quota => "quota",
        LimitRowKind::Credits => "credits",
        LimitRowKind::Rate => "rate",
        LimitRowKind::Context => "context",
        LimitRowKind::Auth => "auth",
        LimitRowKind::Unavailable => "unavailable",
    }
}

/// Formats a cost amount for one line, showing `?` when no USD amount is known.
fn cost_amount(cost: &Cost) -> String {
    match cost.usd {
        Some(usd) => format!("{usd:.2} USD"),
        None => "? (unknown)".to_owned(),
    }
}

/// Sums today's cost rows, keeping unknown entries out of the USD total.
fn cost_today(store: &Store, cfg: &VowpConfig, now: Timestamp) -> Result<Cost, CoreError> {
    let since = start_of_day(now);
    let rows = store.cost_totals(
        &CostQuery {
            since: Some(since),
            project: Some(cfg.project.name.clone()),
            ..CostQuery::default()
        },
        CostGroup::Day,
    )?;
    let mut usd = 0.0;
    let mut tokens = Tokens::default();
    let mut receipt_entries = 0_u64;
    let mut price_table_entries = 0_u64;
    for row in &rows {
        usd += row.usd;
        tokens.add(&row.tokens);
        receipt_entries += row.receipt_entries;
        price_table_entries += row.price_table_entries;
    }
    let known_entries = receipt_entries + price_table_entries;
    let source = if known_entries == 0 {
        CostSource::Unknown
    } else if price_table_entries == 0 {
        CostSource::Receipt
    } else {
        CostSource::PriceTable
    };
    Ok(Cost {
        usd: (known_entries > 0).then_some(usd),
        tokens,
        source,
    })
}

/// Returns midnight UTC of `now`.
fn start_of_day(now: Timestamp) -> Timestamp {
    let secs = now.unix_secs();
    Timestamp::from_unix_secs(secs - secs.rem_euclid(86_400))
}

/// Returns the newest `<project>/.vowp/checkpoints/*.md` timestamp.
fn last_checkpoint(cfg: &VowpConfig) -> Option<Timestamp> {
    let root = cfg.project.path.as_ref()?;
    let dir = std::path::Path::new(root).join(".vowp").join("checkpoints");
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".md")) else {
            continue;
        };
        let Ok(at) = Timestamp::parse(stem) else {
            continue;
        };
        if newest.is_none_or(|current: Timestamp| at > current) {
            newest = Some(at);
        }
    }
    newest
}
