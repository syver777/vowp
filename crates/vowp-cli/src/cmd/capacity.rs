//! `vowp capacity` — measured, admitted capacity. Spec: §6.5 of the v0.2 spec.
//!
//! `show` and `report` read the node report; `set` writes one target history row and refuses
//! an owner target above the advice unless `--override` is given (exit 5). The same report
//! renderer feeds the `capacity` tmux window.

use clap::{Args, Subcommand};
use time::{Date, macros::format_description};
use vowp_core::CapacityManager;
use vowp_protocol::{CapacityReport, CapacityTarget, TargetMode, Timestamp};

use crate::output::Table;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp capacity`.
#[derive(Debug, Clone, Args)]
pub struct CapacityArgs {
    /// The capacity subcommand.
    #[command(subcommand)]
    pub command: CapacityCommand,
}

/// The `vowp capacity` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum CapacityCommand {
    /// Show the current capacity report.
    Show(CapacityShowArgs),
    /// Show the report used by the advisor.
    Report(CapacityReportArgs),
    /// Write one capacity target row.
    Set(CapacitySetArgs),
}

/// Arguments for `vowp capacity show`.
#[derive(Debug, Clone, Default, Args)]
pub struct CapacityShowArgs {}

/// Arguments for `vowp capacity report`.
#[derive(Debug, Clone, Default, Args)]
pub struct CapacityReportArgs {
    /// Only include samples since this duration or date.
    #[arg(long, value_name = "DUR|DATE")]
    pub since: Option<String>,
    /// Maximum number of estimates.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
}

/// Arguments for `vowp capacity set`.
#[derive(Debug, Clone, Default, Args)]
pub struct CapacitySetArgs {
    /// Number of workers.
    #[arg(long, value_name = "N")]
    pub target: Option<u32>,
    /// `auto` or `fixed`.
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,
    /// Allow a target above the supervisor's advice.
    #[arg(long = "override")]
    pub is_override: bool,
    /// Why the target changed.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
    /// Who set the target.
    #[arg(long, value_name = "ACTOR")]
    pub by: Option<String>,
}

/// Runs `vowp capacity`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &CapacityArgs) -> Result<i32, CliError> {
    match &args.command {
        CapacityCommand::Show(_) => show(ctx, io)?,
        CapacityCommand::Report(args) => {
            let now = ctx.clock().now();
            let since = args.since.as_deref().map(|value| parse_since(value, now)).transpose()?;
            let store = ctx.fleet_store()?;
            let manager = CapacityManager::new(ctx.deps().clone());
            let report = manager.report_filtered(&store, now, since, args.limit)?;
            print_report(ctx, io, &report)?;
        }
        CapacityCommand::Set(args) => set(ctx, io, args)?,
    }
    Ok(0)
}

/// Renders `capacity show`; also the body of the `capacity` tmux window.
pub(crate) fn show(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<(), CliError> {
    let store = ctx.fleet_store()?;
    let manager = CapacityManager::new(ctx.deps().clone());
    let report = manager.report(&store, ctx.clock().now())?;
    print_report(ctx, io, &report)
}

/// Prints one report as JSON or as the human capacity card.
pub(crate) fn print_report(ctx: &Ctx, io: &mut dyn IoStreams, report: &CapacityReport) -> Result<(), CliError> {
    if ctx.json() {
        crate::output::print_json(io, report)?;
        return Ok(());
    }
    let target = &report.target;
    if let Some(host) = &report.host {
        if host.source == vowp_protocol::SampleSource::Fallback {
            writeln!(
                io.out(),
                "capacity: fallback (no /proc), target {}",
                target.target_workers
            )?;
        }
        writeln!(
            io.out(),
            "host: {} {:?}, mem available {} of {}, load {:.2} {:.2} {:.2}, {} CPU(s), disk home {}",
            host.at.as_rfc3339(),
            host.source,
            format_bytes(host.mem_available_bytes),
            format_bytes(host.mem_total_bytes),
            host.load1,
            host.load5,
            host.load15,
            host.cpu_count,
            format_bytes(host.disk_free_home_bytes)
        )?;
        if let Some(worktrees) = host.disk_free_worktrees_bytes {
            writeln!(io.out(), "disk worktrees: {}", format_bytes(worktrees))?;
        }
    } else {
        writeln!(io.out(), "host: no samples yet")?;
    }
    writeln!(
        io.out(),
        "target: {} ({}) advised {}",
        target.target_workers,
        mode_name(target.mode),
        target
            .advised_workers
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned())
    )?;
    if target.is_override {
        writeln!(
            io.out(),
            "owner override: {} (advised {})",
            target.target_workers,
            target
                .advised_workers
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned())
        )?;
    }
    writeln!(
        io.out(),
        "running: {} run(s), RAM p90 {}, CPU p90 {:.2}",
        report.running.runs,
        format_bytes(report.running.ram_p90_bytes),
        report.running.cpu_p90_cores
    )?;
    if report.brakes.active {
        let reasons = report
            .brakes
            .reasons
            .iter()
            .map(|reason| reason.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        writeln!(
            io.out(),
            "brake: {reasons}{}",
            report
                .brakes
                .since
                .map(|since| format!(" since {}", since.as_rfc3339()))
                .unwrap_or_default()
        )?;
    } else {
        writeln!(io.out(), "brake: off")?;
    }
    writeln!(
        io.out(),
        "reserve: {} RAM, {} disk",
        format_bytes(report.reserve_ram_bytes),
        format_bytes(report.min_disk_bytes)
    )?;

    if report.estimates.is_empty() {
        writeln!(io.out(), "estimates: none yet")?;
    } else {
        writeln!(io.out(), "estimates:")?;
        let mut table = Table::new(&[
            "PROJECT", "KIND", "HARNESS", "MODEL", "N", "RAM P50", "RAM P90", "CPU P90",
        ]);
        for estimate in &report.estimates {
            table.row(vec![
                estimate
                    .project
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "*".to_owned()),
                estimate
                    .job_kind
                    .map(|kind| format!("{kind:?}").to_lowercase())
                    .unwrap_or_else(|| "*".to_owned()),
                estimate
                    .harness
                    .map(|harness| harness.as_str().to_owned())
                    .unwrap_or_else(|| "*".to_owned()),
                estimate.model.clone().unwrap_or_else(|| "*".to_owned()),
                estimate.samples.to_string(),
                format_bytes(estimate.ram_p50_bytes),
                format_bytes(estimate.ram_p90_bytes),
                format!("{:.2}", estimate.cpu_p90_cores),
            ]);
        }
        write!(io.out(), "{}", table.render(ctx.color()))?;
    }

    if !report.waiting.is_empty() {
        writeln!(io.out(), "waiting:")?;
        let mut table = Table::new(&["PROJECT", "JOB", "SINCE", "REASON"]);
        for waiting in &report.waiting {
            table.row(vec![
                waiting.project.to_string(),
                waiting.job.to_string(),
                waiting.since.as_rfc3339(),
                format!("waiting: {}", waiting.reason.detail),
            ]);
        }
        write!(io.out(), "{}", table.render(ctx.color()))?;
    }
    if !report.limits.is_empty() {
        writeln!(io.out(), "limits:")?;
        let mut table = Table::new(&["PROVIDER", "KIND", "RESETS", "DETAIL"]);
        for limit in &report.limits {
            table.row(vec![
                limit.provider.clone(),
                format!("{:?}", limit.kind).to_lowercase(),
                limit
                    .resets_at
                    .map(|at| at.as_rfc3339())
                    .unwrap_or_else(|| "-".to_owned()),
                limit.detail.clone(),
            ]);
        }
        write!(io.out(), "{}", table.render(ctx.color()))?;
    }
    Ok(())
}

/// Runs `capacity set`.
fn set(ctx: &Ctx, io: &mut dyn IoStreams, args: &CapacitySetArgs) -> Result<(), CliError> {
    let mode = match args.mode.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("auto") => TargetMode::Auto,
        Some("fixed") => TargetMode::Fixed,
        Some(other) => {
            return Err(CliError::Invalid(format!(
                "unknown --mode {other:?}; use auto or fixed"
            )));
        }
        None if args.target.is_some() => TargetMode::Fixed,
        None => {
            return Err(CliError::Invalid(
                "capacity set requires --target <n> or --mode auto".to_owned(),
            ));
        }
    };
    if mode == TargetMode::Fixed && args.target.is_none() {
        return Err(CliError::Invalid(
            "capacity set --mode fixed requires --target <n>".to_owned(),
        ));
    }
    let by = args.by.clone().unwrap_or_else(|| "owner".to_owned());
    let reason = args.reason.clone().unwrap_or_else(|| match mode {
        TargetMode::Auto => "owner recomputed the auto target".to_owned(),
        TargetMode::Fixed => "owner set the worker target".to_owned(),
    });
    let store = ctx.fleet_store()?;
    let manager = CapacityManager::new(ctx.deps().clone());
    let target = manager.set_target(&store, mode, args.target.unwrap_or(0), args.is_override, &by, &reason)?;
    if ctx.json() {
        crate::output::print_json(io, &target)?;
    } else {
        print_target(io, &target)?;
    }
    Ok(())
}

/// Prints one target row.
fn print_target(io: &mut dyn IoStreams, target: &CapacityTarget) -> Result<(), CliError> {
    writeln!(
        io.out(),
        "target: {} ({}) advised {}",
        target.target_workers,
        mode_name(target.mode),
        target
            .advised_workers
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned())
    )?;
    if target.is_override {
        writeln!(
            io.out(),
            "owner override: {} (advised {})",
            target.target_workers,
            target
                .advised_workers
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned())
        )?;
    }
    if !target.reason.is_empty() {
        writeln!(io.out(), "reason: {}", target.reason)?;
    }
    Ok(())
}

/// Returns the lowercase target-mode name.
fn mode_name(mode: TargetMode) -> &'static str {
    match mode {
        TargetMode::Auto => "auto",
        TargetMode::Fixed => "fixed",
    }
}

/// Formats bytes as GiB with one decimal, or MiB for small values.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

/// Parses `--since` as a duration (`30m`, `7d`) or a `YYYY-MM-DD` date.
fn parse_since(value: &str, now: Timestamp) -> Result<Timestamp, CliError> {
    let text = value.trim();
    if let Some(timestamp) = parse_duration(text, now) {
        return Ok(timestamp);
    }
    if let Ok(date) = Date::parse(text, format_description!("[year]-[month]-[day]")) {
        return Ok(Timestamp::from_unix_secs(date.midnight().assume_utc().unix_timestamp()));
    }
    if let Ok(timestamp) = Timestamp::parse(text) {
        return Ok(timestamp);
    }
    Err(CliError::Invalid(format!(
        "invalid --since {value:?}; use a duration (30m, 24h, 7d) or YYYY-MM-DD"
    )))
}

/// Parses a compact duration suffix into an instant before `now`.
fn parse_duration(value: &str, now: Timestamp) -> Option<Timestamp> {
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
    Some(Timestamp::from_unix_secs(now.unix_secs().saturating_sub(seconds)))
}
