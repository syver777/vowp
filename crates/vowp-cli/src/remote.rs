//! `--node <name>` redirection over ssh.
//!
//! Spec: `docs/design/cli.md` §7 and `docs/design/crates/vowp-cli.md` §2. The argv builder is
//! pure and tested without executing anything; [`run_remote`] returns the remote exit code
//! unchanged and maps ssh's own failure (255) to [`CliError::Remote`].

use std::process::Command as ProcessCommand;

use vowp_protocol::NodeName;
use vowp_store::Store;
use vowp_tmux::quote::shell_quote;

use crate::CliError;
use crate::cli::{Command as CliCommand, Global, SessionCommand};
use crate::ctx;

/// Redirects the command when `--node` names a remote node.
///
/// Returns `Ok(None)` when the flag is absent or names the local node, in which case the
/// caller continues locally.
pub fn maybe_run(global: &Global, command: &CliCommand, original_args: &[String]) -> Result<Option<i32>, CliError> {
    let Some(name) = &global.node else {
        return Ok(None);
    };
    let name = NodeName::parse(name)?;
    let paths = ctx::node_paths(global)?;
    if !paths.db_path().is_file() {
        return Err(CliError::NotFound(
            "vowp home is not initialised; run `vowp init`".to_owned(),
        ));
    }
    let store = Store::open(&paths)?;
    let node = store
        .node(&name)?
        .ok_or_else(|| CliError::NotFound(format!("node {name}")))?;
    if node.is_local {
        return Ok(None);
    }
    let Some(target) = node.ssh_target.as_deref() else {
        return Err(CliError::Invalid(format!("node {name} has no ssh target")));
    };
    let args = strip_node_flag(original_args);
    let interactive = is_interactive(command);
    run_remote(target, &args, interactive).map(Some)
}

/// Returns true for commands that attach to a terminal on the remote node.
pub fn is_interactive(command: &CliCommand) -> bool {
    match command {
        CliCommand::Attach(_) | CliCommand::Chat(_) => true,
        CliCommand::Session {
            command: SessionCommand::Chat(_),
        } => true,
        CliCommand::Board(args) => !args.once,
        _ => false,
    }
}

/// Removes `--node <name>` and `--node=<name>` from an argument list.
pub fn strip_node_flag(args: &[String]) -> Vec<String> {
    let mut stripped = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--node" {
            skip_next = true;
        } else if !arg.starts_with("--node=") {
            stripped.push(arg.clone());
        }
    }
    stripped
}

/// Builds `ssh -o BatchMode=yes -o ConnectTimeout=10 [-t] <target> vowp <quoted args>`.
///
/// Every remote argument is POSIX single-quoted with [`shell_quote`]; `ssh` joins the final
/// words into one command string and the remote shell must see the exact original arguments.
pub fn build_ssh_argv(target: &str, args: &[String], interactive: bool) -> Vec<String> {
    let mut argv = vec![
        "ssh".to_owned(),
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        "ConnectTimeout=10".to_owned(),
    ];
    if interactive {
        argv.push("-t".to_owned());
    }
    argv.push(target.to_owned());

    let quoted: Vec<String> = args.iter().map(|arg| shell_quote(arg)).collect();
    let mut remote = String::from("vowp");
    if !quoted.is_empty() {
        remote.push(' ');
        remote.push_str(&quoted.join(" "));
    }
    argv.push(remote);
    argv
}

/// Runs `ssh` with inherited stdio and returns the remote exit code.
///
/// Exit 255 means ssh itself failed (connection, authentication, missing remote `vowp`);
/// that becomes [`CliError::Remote`] and exit code 7.
pub fn run_remote(target: &str, args: &[String], interactive: bool) -> Result<i32, CliError> {
    let argv = build_ssh_argv(target, args, interactive);
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| CliError::Remote("empty ssh argv".to_owned()))?;
    let status = ProcessCommand::new(program)
        .args(rest)
        .status()
        .map_err(|error| CliError::Remote(format!("ssh {target}: {error}")))?;
    match status.code() {
        Some(255) => Err(CliError::Remote(format!("ssh {target} failed with exit 255"))),
        Some(code) => Ok(code),
        None => Err(CliError::Remote(format!("ssh {target} terminated by a signal"))),
    }
}
