//! `loops`: list the supervisor loops and pause or resume one.
//!
//! Spec: `docs/design/cli.md` §5 and `docs/design/loops.md` §1. L1 itself cannot be paused;
//! stopping the supervisor is `vowp down`.

use vowp_protocol::{ActiveState, LoopKind};

use crate::cli::{LoopNameArgs, LoopsArgs, LoopsCommand};
use crate::output::{Table, local_time_at, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Lists loops or pauses/resumes one.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &LoopsArgs) -> Result<i32, CliError> {
    match &args.command {
        None => list(ctx, io),
        Some(LoopsCommand::Pause(args)) => set_state(ctx, io, args, ActiveState::Paused),
        Some(LoopsCommand::Resume(args)) => set_state(ctx, io, args, ActiveState::Active),
    }
}

/// Lists every loop row.
fn list(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let store = ctx.store()?;
    let loops = store.list_loops()?;
    if ctx.json() {
        print_json(io, &loops)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut table = Table::new(&["NAME", "KIND", "STATE", "INTERVAL", "LAST", "NEXT", "STATUS", "FAILS"]);
    for loop_ in &loops {
        table.row(vec![
            loop_.name.clone(),
            loop_kind(loop_.kind).to_owned(),
            active_state(loop_.state).to_owned(),
            interval(loop_.interval_secs),
            loop_
                .last_run_at
                .map(|ts| local_time_at(ts, now, ctx.tz_offset_minutes()))
                .unwrap_or_else(|| "-".to_owned()),
            loop_
                .next_run_at
                .map(|ts| local_time_at(ts, now, ctx.tz_offset_minutes()))
                .unwrap_or_else(|| "-".to_owned()),
            if loop_.last_status.is_empty() {
                "-".to_owned()
            } else {
                loop_.last_status.clone()
            },
            loop_.fail_streak.to_string(),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Pauses or resumes one loop.
fn set_state(ctx: &Ctx, io: &mut dyn IoStreams, args: &LoopNameArgs, state: ActiveState) -> Result<i32, CliError> {
    let store = ctx.store()?;
    if args.loop_name == "l1-supervisor" {
        return Err(CliError::Invalid(
            "the l1-supervisor loop cannot be paused; use `vowp down`".to_owned(),
        ));
    }
    let loop_ = store
        .loop_(&args.loop_name)?
        .ok_or_else(|| CliError::NotFound(format!("loop {}", args.loop_name)))?;
    store.set_loop_state(&loop_.name, state)?;
    let updated = store
        .loop_(&loop_.name)?
        .ok_or_else(|| CliError::NotFound(format!("loop {}", loop_.name)))?;
    if ctx.json() {
        print_json(io, &updated)?;
    } else {
        let verb = if state == ActiveState::Paused {
            "paused"
        } else {
            "resumed"
        };
        writeln!(io.out(), "{verb} loop {}", updated.name)?;
    }
    Ok(0)
}

/// Returns the display name of a loop kind.
fn loop_kind(kind: LoopKind) -> &'static str {
    match kind {
        LoopKind::L1 => "L1",
        LoopKind::L2 => "L2",
        LoopKind::L3 => "L3",
        LoopKind::L4 => "L4",
        LoopKind::L5 => "L5",
        LoopKind::L6 => "L6",
        LoopKind::L7 => "L7",
        LoopKind::L8 => "L8",
        LoopKind::L9 => "L9",
    }
}

/// Returns the wire name of an active state.
fn active_state(state: ActiveState) -> &'static str {
    match state {
        ActiveState::Active => "active",
        ActiveState::Paused => "paused",
    }
}

/// Formats an interval in seconds.
fn interval(seconds: u64) -> String {
    if seconds == 0 {
        "-".to_owned()
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}
