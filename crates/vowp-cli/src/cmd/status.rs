//! `vowp status`: one screen of supervisor, jobs, slots, limits, cost and inbox.
//!
//! Spec: `docs/design/cli.md` §2 and `docs/design/loops.md` §1 step 13. `--check` exits 1
//! when the heartbeat file is older than three minutes; the watchdog uses that contract.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use vowp_core::fleet;
use vowp_core::ledger::BoardSnapshot;
use vowp_protocol::{Limit, LimitRowKind, ProjectState, Snapshot, Timestamp, VowpConfig};
use vowp_store::NodePaths;

use crate::cli::StatusArgs;
use crate::signals;
use crate::{CliError, Ctx, IoStreams};

/// Age after which the heartbeat is stale (`cli.md`: 3 minutes).
pub const STALE_AFTER_SECS: i64 = 180;

/// What `status --json` prints.
#[derive(Debug, Serialize)]
struct StatusOutput {
    supervisor: SupervisorStatus,
    jobs: BTreeMap<String, u32>,
    slots: Slots,
    limits: Vec<LimitOutput>,
    cost_today_usd: Option<f64>,
    inbox_open: u32,
    home: String,
    projects: Vec<ProjectStatus>,
}

/// One project row in the status document.
#[derive(Debug, Serialize)]
struct ProjectStatus {
    name: String,
    state: &'static str,
    running: u32,
    ready: u32,
    blocked: u32,
    paused_reason: String,
}

/// The supervisor block of [`StatusOutput`].
#[derive(Debug, Serialize)]
struct SupervisorStatus {
    running: bool,
    pid: Option<u32>,
    last_tick: Option<String>,
    heartbeat_age_secs: Option<i64>,
    paused: bool,
}

/// Slot occupancy.
#[derive(Debug, Serialize)]
struct Slots {
    busy: u32,
    total: u32,
}

/// One active limit in the documented JSON shape.
#[derive(Debug, Serialize)]
pub(crate) struct LimitOutput {
    pub(crate) provider: String,
    pub(crate) kind: &'static str,
    pub(crate) resets_at: Option<String>,
}

/// The supervisor facts the human and JSON status views share.
struct SupervisorState {
    /// Live supervisor pid, when one holds the lock.
    pid: Option<u32>,
    /// Start time from the second lock line.
    started_at: Option<Timestamp>,
    /// Timestamp from the heartbeat file.
    heartbeat: Option<Timestamp>,
    /// Age of that heartbeat in seconds.
    heartbeat_age_secs: Option<i64>,
    /// Whether a clean stop marker makes the home stopped.
    stopped: bool,
}

/// Runs `vowp status`.
///
/// `status` is a fleet read-model command: it uses [`Ctx::fleet_store`] so a home with zero
/// or several projects works without `--project`. A selected project still supplies the
/// byte-compatible v0.1 board; a fleet context aggregates the same keys across the snapshot.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &StatusArgs) -> Result<i32, CliError> {
    let now = ctx.clock().now();
    let store = ctx.fleet_store()?;
    let fleet = fleet::build_snapshot(ctx.deps(), &store, now)?;
    let board = if ctx.project_selected() {
        let project_store = ctx.store()?;
        Some(BoardSnapshot::collect(&project_store, ctx.config(), now)?)
    } else {
        None
    };
    let (pid, started_at) = read_lock(ctx.paths());
    // A clean shutdown removes the heartbeat and leaves a stopped marker. It is not a live
    // supervisor even if a heartbeat file survived the stop.
    let stopped = pid.is_none() && signals::stopped(ctx.paths());
    let heartbeat = if stopped { None } else { read_heartbeat(ctx.paths()) };
    let supervisor = SupervisorState {
        pid,
        started_at,
        heartbeat,
        heartbeat_age_secs: heartbeat.map(|at| now.unix_secs() - at.unix_secs()),
        stopped,
    };
    let stale = supervisor.stopped || supervisor.heartbeat_age_secs.is_none_or(|age| age > STALE_AFTER_SECS);
    let paused = store.kv_get("paused")?.as_deref() == Some("1");

    if ctx.json() {
        let slots = match &board {
            Some(board) => Slots {
                busy: board.slots_busy,
                total: board.slots_total,
            },
            None => Slots {
                busy: fleet_slots_busy(&fleet),
                total: ctx.config().node.max_workers + ctx.config().node.max_verifiers,
            },
        };
        let output = StatusOutput {
            supervisor: SupervisorStatus {
                running: supervisor.pid.is_some(),
                pid: supervisor.pid,
                last_tick: supervisor.heartbeat.map(|at| at.as_rfc3339()),
                heartbeat_age_secs: supervisor.heartbeat_age_secs,
                paused,
            },
            jobs: match &board {
                Some(board) => board.jobs_by_state.clone(),
                None => fleet_jobs_by_state(&fleet),
            },
            slots,
            limits: match &board {
                Some(board) => board.limits.iter().map(LimitOutput::from).collect(),
                None => fleet.limits.iter().map(LimitOutput::from).collect(),
            },
            cost_today_usd: match &board {
                Some(board) => board.cost_today.usd,
                None => fleet_cost_today(&fleet),
            },
            inbox_open: match &board {
                Some(board) => board.inbox_open,
                None => fleet_inbox_open(&fleet),
            },
            home: fleet.home.to_string(),
            projects: fleet.projects.iter().map(project_status).collect(),
        };
        crate::output::print_json(io, &output)?;
    } else {
        print_human(io, &supervisor, paused, ctx.config(), board.as_ref(), &fleet)?;
    }

    if args.check && stale {
        return Ok(1);
    }
    Ok(0)
}

/// Sums job counts across every project card.
fn fleet_jobs_by_state(snapshot: &Snapshot) -> BTreeMap<String, u32> {
    let mut jobs = BTreeMap::new();
    for project in &snapshot.projects {
        for (state, count) in &project.jobs_by_state {
            let total = jobs.entry(state.clone()).or_insert(0_u32);
            *total = total.saturating_add(*count);
        }
    }
    jobs
}

/// Counts running jobs across every project card.
fn fleet_slots_busy(snapshot: &Snapshot) -> u32 {
    snapshot.projects.iter().fold(0_u32, |total, project| {
        total.saturating_add(u32::try_from(project.running.len()).unwrap_or(u32::MAX))
    })
}

/// Sums the known cost of every project; unknown costs stay out of the total.
fn fleet_cost_today(snapshot: &Snapshot) -> Option<f64> {
    snapshot
        .projects
        .iter()
        .filter_map(|project| project.cost_today_usd)
        .reduce(|left, right| left + right)
}

/// Counts the open inbox items in the snapshot.
fn fleet_inbox_open(snapshot: &Snapshot) -> u32 {
    u32::try_from(snapshot.inbox.len()).unwrap_or(u32::MAX)
}

/// Prints the human supervisor line followed by the board screen.
fn print_human(
    io: &mut dyn IoStreams,
    supervisor: &SupervisorState,
    paused: bool,
    config: &VowpConfig,
    board: Option<&BoardSnapshot>,
    fleet: &Snapshot,
) -> Result<(), CliError> {
    let now = fleet.generated_at;
    let paused = if paused { ", paused" } else { "" };
    match supervisor.pid {
        Some(pid) => {
            let uptime = supervisor
                .started_at
                .map(|started| format_duration(now.since(started).as_secs()))
                .unwrap_or_else(|| "unknown".to_owned());
            match supervisor.heartbeat_age_secs {
                Some(age) => writeln!(
                    io.out(),
                    "supervisor: running (pid {pid}, up {uptime}, last tick {} ago{paused})",
                    format_duration(age.max(0).unsigned_abs())
                )?,
                None => writeln!(
                    io.out(),
                    "supervisor: running (pid {pid}, up {uptime}, no heartbeat yet{paused})"
                )?,
            }
        }
        None if supervisor.stopped => writeln!(io.out(), "supervisor: stopped")?,
        None => writeln!(io.out(), "supervisor: not running")?,
    }
    writeln!(io.out(), "home: {}", fleet.home)?;
    writeln!(io.out(), "projects:")?;
    for project in &fleet.projects {
        let ready = project.jobs_by_state.get("ready").copied().unwrap_or(0);
        writeln!(
            io.out(),
            "  {}  {}  running {}  ready {ready}  blocked {}",
            project.name,
            project_state_name(project.state),
            project.running.len(),
            project.blocked.len()
        )?;
    }
    match board {
        Some(board) => write!(io.out(), "{}", board.render_text(false))?,
        None => write!(io.out(), "{}", render_fleet_text(fleet, config))?,
    }
    Ok(())
}

/// Renders the v0.1 status body for a fleet context.
///
/// The line shape matches [`BoardSnapshot::render_text`], with a project prefix on each
/// running job so the reader can tell which database supplied it.
fn render_fleet_text(snapshot: &Snapshot, config: &VowpConfig) -> String {
    let mut text = format!("fleet @ {}\n", snapshot.generated_at.as_rfc3339());
    let jobs = fleet_jobs_by_state(snapshot);
    if jobs.is_empty() {
        text.push_str("jobs: none\n");
    } else {
        let counts: Vec<String> = jobs.iter().map(|(state, count)| format!("{state} {count}")).collect();
        text.push_str(&format!("jobs: {}\n", counts.join(", ")));
    }

    let running: Vec<_> = snapshot
        .projects
        .iter()
        .flat_map(|project| project.running.iter().map(move |run| (project, run)))
        .collect();
    if running.is_empty() {
        text.push_str("running: none\n");
    } else {
        text.push_str("running:\n");
        for (project, run) in running {
            let executor = run.executor.as_deref().unwrap_or("-");
            let last_event = run
                .last_event_age_secs
                .map(|secs| format!("{secs}s"))
                .unwrap_or_else(|| "-".to_owned());
            text.push_str(&format!(
                "  {}/{job}  {executor}  {age}s  {last_event}\n",
                project.name,
                job = run.job,
                age = run.age_secs
            ));
        }
    }

    text.push_str(&format!(
        "slots: {}/{}\n",
        fleet_slots_busy(snapshot),
        config.node.max_workers + config.node.max_verifiers
    ));
    text.push_str(&format!("planner: {}\n", profile_head(config, "planner")));
    text.push_str(&format!("keeper: {}\n", profile_head(config, "keeper")));
    if snapshot.limits.is_empty() {
        text.push_str("limits: none\n");
    } else {
        text.push_str("limits:\n");
        for limit in &snapshot.limits {
            let resets = limit
                .resets_at
                .map(|at| format!("resets {}", at.as_rfc3339()))
                .unwrap_or_else(|| "until resolved".to_owned());
            text.push_str(&format!(
                "  {} {} {resets}\n",
                limit.provider,
                limit_kind_name(limit.kind)
            ));
        }
    }
    match fleet_cost_today(snapshot) {
        Some(usd) => text.push_str(&format!("cost today: {usd:.2} USD\n")),
        None => text.push_str("cost today: ? (unknown)\n"),
    }
    text.push_str(&format!("inbox: {} open\n", fleet_inbox_open(snapshot)));
    text
}

/// Returns the first executor of a named profile in the node config, or `-`.
fn profile_head(config: &VowpConfig, profile: &str) -> String {
    config
        .profiles
        .get(profile)
        .and_then(|profile| profile.ladder.first())
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_owned())
}

/// Converts one snapshot project card into the status row shape.
fn project_status(project: &vowp_protocol::ProjectSnapshot) -> ProjectStatus {
    ProjectStatus {
        name: project.name.to_string(),
        state: project_state_name(project.state),
        running: project.running.len().try_into().unwrap_or(u32::MAX),
        ready: project.jobs_by_state.get("ready").copied().unwrap_or(0),
        blocked: project.blocked.len().try_into().unwrap_or(u32::MAX),
        paused_reason: project.paused_reason.clone(),
    }
}

/// Returns the lowercase registry state name.
fn project_state_name(state: ProjectState) -> &'static str {
    match state {
        ProjectState::Active => "active",
        ProjectState::Paused => "paused",
        ProjectState::Archived => "archived",
    }
}

/// Formats a duration as `1h 02m`, `3m 04s` or `12s`.
fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Reads the pid and start time from `supervisor.lock`.
fn read_lock(paths: &NodePaths) -> (Option<u32>, Option<Timestamp>) {
    let pid = crate::ctx::supervisor_pid(paths);
    if pid.is_none() {
        return (None, None);
    }
    let started = std::fs::read_to_string(paths.root().join("supervisor.lock"))
        .ok()
        .and_then(|text| text.lines().nth(1).map(str::trim).map(Timestamp::parse))
        .and_then(Result::ok);
    (pid, started)
}

/// Reads the heartbeat timestamp from `<home>/heartbeat`.
fn read_heartbeat(paths: &NodePaths) -> Option<Timestamp> {
    read_heartbeat_text(&paths.root().join("heartbeat"))
}

/// Parses the first line of a heartbeat file as Unix seconds.
///
/// A legacy stopped heartbeat (`<seconds>\nstopped`) is not a live timestamp; the file
/// modification time is never used, because a fresh mtime would hide a stopped or
/// corrupted heartbeat from `status --check` and the watchdog.
fn read_heartbeat_text(path: &Path) -> Option<Timestamp> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    if lines.any(|line| line.trim() == "stopped") {
        return None;
    }
    first.parse::<i64>().ok().map(Timestamp::from_unix_secs)
}

impl From<&Limit> for LimitOutput {
    fn from(limit: &Limit) -> Self {
        Self {
            provider: limit.provider.clone(),
            kind: limit_kind_name(limit.kind),
            resets_at: limit.resets_at.map(|at| at.as_rfc3339()),
        }
    }
}

/// Returns the canonical lowercase name of a limit kind.
pub(crate) fn limit_kind_name(kind: LimitRowKind) -> &'static str {
    match kind {
        LimitRowKind::Quota => "quota",
        LimitRowKind::Credits => "credits",
        LimitRowKind::Rate => "rate",
        LimitRowKind::Context => "context",
        LimitRowKind::Auth => "auth",
        LimitRowKind::Unavailable => "unavailable",
    }
}
