//! `vowp board`: the refreshing dashboard, computed from the database.
//!
//! Spec: `docs/design/cli.md` §2 and `docs/design/crates/vowp-core.md` §5. The snapshot is
//! recomputed on every redraw (`BoardSnapshot::collect`); `~/.vowp/board.json` is only a
//! convenience copy written by L1. `--once` prints a single frame and is what tests pin.

use std::collections::BTreeMap;

use serde::Serialize;
use vowp_core::ledger::BoardSnapshot;

use crate::cli::BoardArgs;
use crate::cmd::status::LimitOutput;
use crate::{CliError, Ctx, IoStreams};

/// One running job in the `board --json` document.
#[derive(Debug, Serialize)]
struct RunningOutput {
    job: String,
    executor: Option<String>,
    age_secs: u64,
    last_event_age_secs: Option<u64>,
}

/// What `board --json` prints.
#[derive(Debug, Serialize)]
struct BoardOutput {
    at: String,
    project: String,
    jobs: BTreeMap<String, u32>,
    running: Vec<RunningOutput>,
    slots_busy: u32,
    slots_total: u32,
    planner: Option<String>,
    keeper: Option<String>,
    limits: Vec<LimitOutput>,
    cost_today_usd: Option<f64>,
    inbox_open: u32,
    last_checkpoint: Option<String>,
}

/// Runs `vowp board`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &BoardArgs) -> Result<i32, CliError> {
    if args.once || !io.is_tty() {
        render(ctx, io)?;
        return Ok(0);
    }

    let interval = std::time::Duration::from_secs(args.interval.max(1));
    loop {
        write!(io.out(), "\x1b[2J\x1b[H")?;
        render(ctx, io)?;
        std::thread::sleep(interval);
    }
}

/// Prints one board frame in the human or JSON shape.
pub(crate) fn render(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<(), CliError> {
    let snapshot = collect(ctx)?;
    if ctx.json() {
        crate::output::print_json(io, &json_view(&snapshot))?;
    } else {
        write!(io.out(), "{}", snapshot.render_text(ctx.color()))?;
    }
    Ok(())
}

/// Returns the current snapshot for this project.
pub(crate) fn collect(ctx: &Ctx) -> Result<BoardSnapshot, CliError> {
    let store = ctx.store()?;
    Ok(BoardSnapshot::collect(&store, ctx.config(), ctx.clock().now())?)
}

/// Converts a snapshot to the CLI's JSON shape.
fn json_view(snapshot: &BoardSnapshot) -> BoardOutput {
    BoardOutput {
        at: snapshot.at.as_rfc3339(),
        project: snapshot.project.to_string(),
        jobs: snapshot.jobs_by_state.clone(),
        running: snapshot
            .running
            .iter()
            .map(|run| RunningOutput {
                job: run.job.to_string(),
                executor: run.executor.as_ref().map(ToString::to_string),
                age_secs: run.age_secs,
                last_event_age_secs: run.last_event_age_secs,
            })
            .collect(),
        slots_busy: snapshot.slots_busy,
        slots_total: snapshot.slots_total,
        planner: snapshot.planner_executor.as_ref().map(ToString::to_string),
        keeper: snapshot.keeper_executor.as_ref().map(ToString::to_string),
        limits: snapshot.limits.iter().map(LimitOutput::from).collect(),
        cost_today_usd: snapshot.cost_today.usd,
        inbox_open: snapshot.inbox_open,
        last_checkpoint: snapshot.last_checkpoint.map(|at| at.as_rfc3339()),
    }
}
