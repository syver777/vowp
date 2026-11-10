//! Command dispatch. Spec: `docs/design/crates/vowp-cli.md` §1.
//!
//! `init` and `doctor` run before a [`crate::Ctx`] exists; every other command receives one.
//! A command whose handler is not implemented yet is an explicit `NotImplemented` stub here,
//! never a silent no-op.

use crate::cli::{Command, MaintCommand, PromoteCommand, SessionCommand, UpCommand};
use crate::{CliError, Ctx, IoStreams};

pub mod attach;
pub mod board;
pub mod cost;
pub mod doctor;
pub mod graph;
pub mod inbox;
pub mod init;
pub mod job;
pub mod loops;
pub mod maint;
pub mod node;
pub mod plan;
pub mod promote;
pub mod schedule;
pub mod session;
pub mod status;
pub mod up;
pub mod window;

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
        Command::Schedule { command } => schedule::run(ctx, io, &command),
        Command::Promote(args) => promote::run(ctx, io, &PromoteCommand::Promote(args)),
        Command::Rollback(args) => promote::run(ctx, io, &PromoteCommand::Rollback(args)),
        Command::Releases(args) => promote::run(ctx, io, &PromoteCommand::Releases(args)),
        Command::Reindex(args) => maint::run(ctx, io, &MaintCommand::Reindex(args)),
        Command::Backup(args) => maint::run(ctx, io, &MaintCommand::Backup(args)),
        Command::Schema(args) => maint::run(ctx, io, &MaintCommand::Schema(args)),
        Command::Version => maint::run(ctx, io, &MaintCommand::Version),
        Command::Window(args) => window::run(ctx, io, &args),
    }
}
