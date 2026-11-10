//! `vowp status`: one screen of supervisor, jobs, slots, limits, cost and inbox.
//!
//! Spec: `docs/design/cli.md` §2 and `docs/design/loops.md` §1 step 13. `--check` exits 1
//! when the heartbeat file is older than three minutes; the watchdog uses that contract.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use vowp_core::ledger::BoardSnapshot;
use vowp_protocol::{Limit, LimitRowKind, Timestamp};
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

/// Runs `vowp status`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &StatusArgs) -> Result<i32, CliError> {
    let now = ctx.clock().now();
    let store = ctx.store()?;
    let snapshot = BoardSnapshot::collect(&store, ctx.config(), now)?;
    let (pid, started_at) = read_lock(ctx.paths());
    // A clean shutdown removes the heartbeat and leaves a stopped marker. It is not a live
    // supervisor even if a heartbeat file survived the stop.
    let stopped = pid.is_none() && signals::stopped(ctx.paths());
    let heartbeat = if stopped { None } else { read_heartbeat(ctx.paths()) };
    let heartbeat_age_secs = heartbeat.map(|at| now.unix_secs() - at.unix_secs());
    let stale = stopped || heartbeat_age_secs.is_none_or(|age| age > STALE_AFTER_SECS);
    let paused = store.kv_get("paused")?.as_deref() == Some("1");

    if ctx.json() {
        let output = StatusOutput {
            supervisor: SupervisorStatus {
                running: pid.is_some(),
                pid,
                last_tick: heartbeat.map(|at| at.as_rfc3339()),
                heartbeat_age_secs,
                paused,
            },
            jobs: snapshot.jobs_by_state.clone(),
            slots: Slots {
                busy: snapshot.slots_busy,
                total: snapshot.slots_total,
            },
            limits: snapshot.limits.iter().map(LimitOutput::from).collect(),
            cost_today_usd: snapshot.cost_today.usd,
            inbox_open: snapshot.inbox_open,
        };
        crate::output::print_json(io, &output)?;
    } else {
        print_human(io, pid, started_at, stopped, heartbeat_age_secs, &snapshot)?;
    }

    if args.check && stale {
        return Ok(1);
    }
    Ok(0)
}

/// Prints the human supervisor line followed by the board screen.
fn print_human(
    io: &mut dyn IoStreams,
    pid: Option<u32>,
    started_at: Option<Timestamp>,
    stopped: bool,
    heartbeat_age_secs: Option<i64>,
    snapshot: &BoardSnapshot,
) -> Result<(), CliError> {
    let now = snapshot.at;
    match pid {
        Some(pid) => {
            let uptime = started_at
                .map(|started| format_duration(now.since(started).as_secs()))
                .unwrap_or_else(|| "unknown".to_owned());
            match heartbeat_age_secs {
                Some(age) => writeln!(
                    io.out(),
                    "supervisor: running (pid {pid}, up {uptime}, last tick {} ago)",
                    format_duration(age.max(0).unsigned_abs())
                )?,
                None => writeln!(
                    io.out(),
                    "supervisor: running (pid {pid}, up {uptime}, no heartbeat yet)"
                )?,
            }
        }
        None if stopped => writeln!(io.out(), "supervisor: stopped")?,
        None => writeln!(io.out(), "supervisor: not running")?,
    }
    write!(io.out(), "{}", snapshot.render_text(false))?;
    Ok(())
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
