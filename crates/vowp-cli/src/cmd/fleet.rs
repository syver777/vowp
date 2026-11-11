//! `vowp fleet` — the node read model: status, queue, inbox and capacity.
//!
//! Reads `snapshot.json` when it is fresh, recomputes from the databases otherwise, and never
//! opens SQLite over the network. Spec: `docs/design/v0.2/home-and-surfaces.md` §5.5.

use clap::{Args, Subcommand};
use vowp_core::CoreError;
use vowp_core::fleet;
use vowp_protocol::{FleetQueueRow, FleetStatus, InboxItem, JobState, ProjectName, Snapshot, Timestamp};

use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp fleet`.
#[derive(Debug, Clone, Args)]
pub struct FleetArgs {
    /// The fleet subcommand.
    #[command(subcommand)]
    pub command: FleetCommand,
}

/// The `vowp fleet` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum FleetCommand {
    /// Show the node summary from `snapshot.json`.
    Status(FleetStatusArgs),
    /// List queued jobs across projects.
    Queue(FleetQueueArgs),
    /// List open inbox items across the node and its projects.
    Inbox(FleetInboxArgs),
    /// Alias of `vowp capacity show`.
    Capacity(FleetCapacityArgs),
}

/// Arguments for `vowp fleet status`.
#[derive(Debug, Clone, Args)]
pub struct FleetStatusArgs {
    /// Recompute the snapshot instead of reading the file.
    #[arg(long, conflicts_with = "cached")]
    pub refresh: bool,
    /// Read only the local cache, never SSH.
    #[arg(long)]
    pub cached: bool,
}

/// Arguments for `vowp fleet queue`.
#[derive(Debug, Clone, Args)]
pub struct FleetQueueArgs {
    /// Restrict to one project.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// Restrict to one job state.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
}

/// Arguments for `vowp fleet inbox`.
#[derive(Debug, Clone, Args)]
pub struct FleetInboxArgs {
    /// Restrict to one project.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// Include answered items.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `vowp fleet capacity`.
#[derive(Debug, Clone, Default, Args)]
pub struct FleetCapacityArgs {}

/// Runs `vowp fleet`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &FleetArgs) -> Result<i32, CliError> {
    match &args.command {
        FleetCommand::Status(args) => status(ctx, io, args),
        FleetCommand::Queue(args) => queue(ctx, io, args),
        FleetCommand::Inbox(args) => inbox(ctx, io, args),
        FleetCommand::Capacity(_) => capacity(ctx, io),
    }
}

/// Runs `fleet status`.
fn status(ctx: &Ctx, io: &mut dyn IoStreams, args: &FleetStatusArgs) -> Result<i32, CliError> {
    let now = ctx.clock().now();
    let (snapshot, source, stale) = load_snapshot(ctx, args.refresh, args.cached, now)?;
    let status = FleetStatus {
        snapshot,
        source,
        fetched_at: now,
        stale,
    };
    if ctx.json() {
        crate::output::print_json(io, &status)?;
    } else {
        print_status(io, &status)?;
    }
    Ok(0)
}

/// Prints the human node summary.
fn print_status(io: &mut dyn IoStreams, status: &FleetStatus) -> Result<(), CliError> {
    let snapshot = &status.snapshot;
    let supervisor = if snapshot.supervisor.running {
        match snapshot.supervisor.pid {
            Some(pid) => format!("running (pid {pid})"),
            None => "running".to_owned(),
        }
    } else if snapshot.supervisor.paused {
        "paused".to_owned()
    } else {
        "not running".to_owned()
    };
    writeln!(io.out(), "home: {}", snapshot.home)?;
    writeln!(
        io.out(),
        "snapshot: {} (source {}, stale {})",
        snapshot.generated_at.as_rfc3339(),
        status.source,
        status.stale
    )?;
    writeln!(io.out(), "supervisor: {supervisor}")?;
    writeln!(io.out(), "projects:")?;
    for project in &snapshot.projects {
        let ready = project.jobs_by_state.get("ready").copied().unwrap_or(0);
        writeln!(
            io.out(),
            "  {}  {}  running {}  ready {ready}  blocked {}",
            project.name,
            state_name(project.state),
            project.running.len(),
            project.blocked.len()
        )?;
    }
    let brake = if snapshot.capacity.brake.active {
        "active"
    } else {
        "off"
    };
    writeln!(
        io.out(),
        "capacity: target {} ({}), brake {brake}",
        snapshot.capacity.target.target_workers,
        target_mode_name(snapshot.capacity.target.mode)
    )?;
    if snapshot.limits.is_empty() {
        writeln!(io.out(), "limits: none")?;
    } else {
        writeln!(io.out(), "limits: {}", snapshot.limits.len())?;
    }
    writeln!(io.out(), "inbox: {} open", snapshot.inbox.len())?;
    Ok(())
}

/// Runs `fleet queue`.
fn queue(ctx: &Ctx, io: &mut dyn IoStreams, args: &FleetQueueArgs) -> Result<i32, CliError> {
    let now = ctx.clock().now();
    let (snapshot, _, _) = load_snapshot(ctx, false, false, now)?;
    let mut rows = fleet::queue_rows(&snapshot);
    if let Some(project) = &args.project
        && let Some(project) = project_filter(project)?
    {
        rows.retain(|row| row.project == project);
    }
    if let Some(state) = &args.state {
        let state: JobState = serde_json::from_value(serde_json::Value::String(state.clone()))
            .map_err(|_| CliError::Invalid(format!("unknown job state {state:?}")))?;
        rows.retain(|row| row.state == state);
    }
    if ctx.json() {
        crate::output::print_json(io, &rows)?;
    } else {
        print_queue(io, &rows)?;
    }
    Ok(0)
}

/// Prints the queue table.
fn print_queue(io: &mut dyn IoStreams, rows: &[FleetQueueRow]) -> Result<(), CliError> {
    if rows.is_empty() {
        writeln!(io.out(), "queue: empty")?;
        return Ok(());
    }
    writeln!(
        io.out(),
        "{:<12} {:<24} {:<9} {:>4} {:>7}  WAITING",
        "PROJECT",
        "JOB",
        "STATE",
        "PRIO",
        "AGE"
    )?;
    for row in rows {
        writeln!(
            io.out(),
            "{:<12} {:<24} {:<9} {:>4} {:>6}s  {}",
            row.project,
            row.job,
            row.state.as_str(),
            row.priority,
            row.age_secs,
            row.waiting_reason.as_deref().unwrap_or("-")
        )?;
    }
    Ok(())
}

/// Runs `fleet inbox`.
fn inbox(ctx: &Ctx, io: &mut dyn IoStreams, args: &FleetInboxArgs) -> Result<i32, CliError> {
    let store = ctx.fleet_store()?;
    let mut items = fleet::collect_inbox(&store, args.all)?;
    if let Some(project) = &args.project
        && let Some(project) = project_filter(project)?
    {
        items.retain(|item| item.project.as_ref() == Some(&project));
    }
    if ctx.json() {
        crate::output::print_json(io, &items)?;
    } else {
        print_inbox(io, &items)?;
    }
    Ok(0)
}

/// Prints the inbox rows, oldest first.
fn print_inbox(io: &mut dyn IoStreams, items: &[InboxItem]) -> Result<(), CliError> {
    if items.is_empty() {
        writeln!(io.out(), "inbox: empty")?;
        return Ok(());
    }
    for item in items {
        let project = item
            .project
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned());
        let job = item
            .job_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned());
        writeln!(
            io.out(),
            "{}  {}  {}  {}  {}",
            inbox_state_name(item.state),
            project,
            job,
            item.title,
            item.id
        )?;
    }
    Ok(())
}

/// Runs `fleet capacity`, an alias of `capacity show`.
fn capacity(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let now = ctx.clock().now();
    let (snapshot, _, _) = load_snapshot(ctx, false, false, now)?;
    if ctx.json() {
        crate::output::print_json(io, &snapshot.capacity)?;
    } else {
        let capacity = &snapshot.capacity;
        writeln!(
            io.out(),
            "target: {} ({})",
            capacity.target.target_workers,
            target_mode_name(capacity.target.mode)
        )?;
        writeln!(
            io.out(),
            "advised: {}",
            capacity
                .advised_target
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned())
        )?;
        writeln!(io.out(), "running: {} run(s)", capacity.running.runs)?;
        writeln!(
            io.out(),
            "brake: {}",
            if capacity.brake.active { "active" } else { "off" }
        )?;
        match &capacity.measured {
            Some(sample) => writeln!(io.out(), "measured: {}", sample.at.as_rfc3339())?,
            None => writeln!(io.out(), "measured: none")?,
        }
    }
    Ok(0)
}

/// Reads a fresh snapshot, or recomputes one from the databases.
///
/// The global store is enough: the read model opens each registered project database in
/// turn, so `fleet status` works on a home with several projects and no `--project`.
fn load_snapshot(ctx: &Ctx, refresh: bool, cached: bool, now: Timestamp) -> Result<(Snapshot, String, bool), CliError> {
    let tick_secs = ctx.config().node.tick_secs;
    if cached {
        let snapshot = fleet::read_snapshot(ctx.paths())?;
        let stale = !fleet::snapshot_is_fresh(&snapshot, now, tick_secs);
        return Ok((snapshot, "cache".to_owned(), stale));
    }
    if refresh {
        let store = ctx.fleet_store()?;
        fleet::write_snapshot(ctx.deps(), &store, now)?;
        let snapshot = fleet::read_snapshot(ctx.paths())?;
        return Ok((snapshot, "local".to_owned(), false));
    }
    match fleet::read_snapshot(ctx.paths()) {
        Ok(snapshot) if fleet::snapshot_is_fresh(&snapshot, now, tick_secs) => {
            return Ok((snapshot, "local".to_owned(), false));
        }
        Ok(_) => {}
        Err(CoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    let store = ctx.fleet_store()?;
    let snapshot = fleet::build_snapshot(ctx.deps(), &store, now)?;
    Ok((snapshot, "local".to_owned(), false))
}

/// Resolves a `--project` filter.
///
/// The global `--project` flag accepts a name or a path and clap folds that value into this
/// field too. A directory is the global path form and means "no extra filter"; a name
/// restricts the output to that project.
fn project_filter(value: &str) -> Result<Option<ProjectName>, CliError> {
    if std::path::Path::new(value).is_dir() {
        return Ok(None);
    }
    Ok(Some(ProjectName::parse(value)?))
}

/// Returns the lowercase registry state name.
fn state_name(state: vowp_protocol::ProjectState) -> &'static str {
    match state {
        vowp_protocol::ProjectState::Active => "active",
        vowp_protocol::ProjectState::Paused => "paused",
        vowp_protocol::ProjectState::Archived => "archived",
    }
}

/// Returns the lowercase target mode name.
fn target_mode_name(mode: vowp_protocol::TargetMode) -> &'static str {
    match mode {
        vowp_protocol::TargetMode::Auto => "auto",
        vowp_protocol::TargetMode::Fixed => "fixed",
    }
}

/// Returns the lowercase inbox state name.
fn inbox_state_name(state: vowp_protocol::InboxState) -> &'static str {
    match state {
        vowp_protocol::InboxState::Open => "open",
        vowp_protocol::InboxState::Answered => "answered",
        vowp_protocol::InboxState::Dismissed => "dismissed",
    }
}
