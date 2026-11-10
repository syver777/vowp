//! `vowp attach`: exec tmux on a job window, or tail its session when tmux is unusable.
//!
//! Spec: `docs/design/cli.md` §3. With no job the command attaches to the project's
//! `board` window. tmux inherits the terminal; without one (or when tmux is absent or the
//! window is gone) the command falls back to the job's session log and exits 3 when the
//! job has no session.

use std::process::Command;

use vowp_protocol::JobId;
use vowp_tmux::{Tmux, session_name_for_project, window_name_for_job};

use crate::cli::AttachArgs;
use crate::cmd::window;
use crate::{CliError, Ctx, IoStreams};

/// Runs `vowp attach`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &AttachArgs) -> Result<i32, CliError> {
    let job = match args.job.as_deref() {
        Some(value) => Some(JobId::parse(value)?),
        None => None,
    };
    let session = session_name_for_project(ctx.project().as_str());
    let window_name = match &job {
        Some(job) => window_name_for_job(job.as_str()),
        None => "board".to_owned(),
    };

    let window_there = io.is_tty()
        && match ctx.deps().windows.window_exists(&session, &window_name) {
            Ok(exists) => exists,
            Err(vowp_tmux::TmuxError::NotInstalled) => false,
            Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
        };
    if window_there {
        let target = format!("{session}:{window_name}");
        let argv = Tmux::new(None).attach_argv(&target);
        match Command::new(&argv[0]).args(&argv[1..]).status() {
            Ok(status) if status.success() => return Ok(0),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CliError::Io(error)),
        }
    }

    match job {
        Some(job) => window::tail_latest_job(ctx, io, &job),
        None => Err(CliError::NotFound(
            "no job given and tmux is not available; pass a job id".to_owned(),
        )),
    }
}
