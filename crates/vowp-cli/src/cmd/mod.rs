//! Command dispatch. Spec: `docs/design/crates/vowp-cli.md` §1.
//!
//! `init` and `doctor` run before a [`crate::Ctx`] exists; every other command receives one.
//! Every command is dispatched to its handler in `cmd/<name>.rs`; none is a silent no-op.

use crate::cli::{Command, MaintCommand, PromoteCommand, SessionCommand, UpCommand};
use crate::{CliError, Ctx, IoStreams};

pub mod attach;
pub mod board;
pub mod capacity;
pub mod control;
pub mod cost;
pub mod doctor;
pub mod exec;
pub mod fleet;
pub mod gc;
pub mod graph;
pub mod inbox;
pub mod init;
pub mod job;
pub mod lease;
pub mod loops;
pub mod maint;
pub mod mcp;
pub mod migrate;
pub mod node;
pub mod orchestrator;
pub mod plan;
pub mod project;
pub mod promote;
pub mod schedule;
pub mod schema;
pub mod secret;
pub mod session;
pub mod setup;
pub mod status;
pub mod tunnel;
pub mod up;
pub mod window;

/// Returns the shared not-implemented error.
pub(crate) fn not_implemented() -> CliError {
    CliError::NotImplemented("not implemented in this build")
}

/// Returns whether `command` still needs the v0.2 pre-dispatch guard.
///
/// Chat's v0.2 flags are no longer refused here. The remaining frozen flags
/// (`up --no-migrate`, `loops --all`, `schedule --all`) are handled by their own handlers;
/// this hook stays so the pre-dispatch check in `lib.rs` keeps a single documented entry
/// point.
pub(crate) fn command_needs_v0_2_handler(_command: &Command) -> bool {
    false
}

/// Runs one command with a built context.
pub fn dispatch(ctx: &Ctx, io: &mut dyn IoStreams, command: Command) -> Result<i32, CliError> {
    match command {
        Command::Init(_) | Command::Doctor(_) => Err(CliError::Invalid(
            "init and doctor run without a project context".to_owned(),
        )),
        Command::Up(args) => up::run(ctx, io, &UpCommand::Up(args)),
        Command::Down(args) => up::run(ctx, io, &UpCommand::Down(args)),
        Command::Status(args) => status::run(ctx, io, &args),
        Command::Board(args) => board::run(ctx, io, &args),
        Command::Plan(args) => plan::run(ctx, io, &args),
        Command::Job { command } => job::run(ctx, io, &command),
        Command::Graph(args) => graph::run(ctx, io, &args),
        Command::Attach(args) => attach::run(ctx, io, &args),
        Command::Session { command } => session::run(ctx, io, &command),
        Command::Chat(args) => session::run(ctx, io, &SessionCommand::Chat(args)),
        Command::Inbox { command } => inbox::run(ctx, io, &command),
        Command::Cost(args) => cost::run(ctx, io, &args),
        Command::Node { command } => node::run(ctx, io, &command),
        Command::Loops(args) => loops::run(ctx, io, &args),
        Command::Schedule(args) => schedule::run(ctx, io, &args),
        Command::Promote(args) => promote::run(ctx, io, &PromoteCommand::Promote(args)),
        Command::Rollback(args) => promote::run(ctx, io, &PromoteCommand::Rollback(args)),
        Command::Releases(args) => promote::run(ctx, io, &PromoteCommand::Releases(args)),
        Command::Reindex(args) => maint::run(ctx, io, &MaintCommand::Reindex(args)),
        Command::Backup(args) => maint::run(ctx, io, &MaintCommand::Backup(args)),
        Command::Schema(args) => schema::run(ctx.json(), io, &args),
        Command::Project(args) => project::run(ctx, io, &args),
        Command::Fleet(args) => fleet::run(ctx, io, &args),
        Command::Capacity(args) => capacity::run(ctx, io, &args),
        Command::Secret(args) => secret::run(ctx, io, &args),
        Command::Gc(args) => gc::run(ctx, io, &args),
        Command::Tunnel(args) => tunnel::run(ctx, io, &args),
        Command::Setup(args) => setup::run(ctx, io, &args),
        Command::Control(args) => control::run(ctx, io, &args),
        Command::Orchestrator(args) => orchestrator::run(ctx, io, &args),
        Command::Migrate(args) => migrate::run(ctx, io, &args),
        Command::Mcp(args) => mcp::run(ctx, io, &args),
        Command::Version => maint::run(ctx, io, &MaintCommand::Version),
        Command::Window(args) => window::run(ctx, io, &args),
        Command::Exec(args) => exec::run(ctx, io, &args),
        Command::VaultExec(args) => secret::vault_exec(ctx, io, &args),
        Command::VaultAdmin(args) => secret::vault_admin(ctx, io, &args),
    }
}
