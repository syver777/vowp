//! `vowp tunnel` — managed SSH local forwards opened by a control machine.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §8.3. A tunnel is `ssh -N ... -L
//! 127.0.0.1:<local>:<host>:<remote>` in its own process group, recorded in
//! `~/.vowp-control/tunnels.json` (0600). The home refuses the command: a tunnel is opened
//! from the machine that will use it.

use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde_json::json;
use vowp_adapters::proc::{Signal, process_alive, signal_group};
use vowp_protocol::Tunnel;

use crate::node_ctx::NodeRole;
use crate::output::{self, Table};
use crate::remote;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp tunnel`.
#[derive(Debug, Clone, Args)]
pub struct TunnelArgs {
    /// The tunnel subcommand.
    #[command(subcommand)]
    pub command: TunnelCommand,
}

/// The `vowp tunnel` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum TunnelCommand {
    /// Open a local port forward to the home.
    Open(TunnelOpenArgs),
    /// List open tunnels.
    Ls,
    /// Close one tunnel.
    Close(TunnelCloseArgs),
}

/// Arguments for `vowp tunnel open`.
#[derive(Debug, Clone, Args)]
pub struct TunnelOpenArgs {
    /// Tunnel name.
    pub name: String,
    /// Remote loopback port on the home.
    #[arg(long, value_name = "N")]
    pub remote_port: u16,
    /// Local loopback port; a free port is chosen when omitted.
    #[arg(long, value_name = "N")]
    pub local_port: Option<u16>,
    /// Remote host the tunnel forwards to.
    #[arg(long, value_name = "HOST", default_value = "127.0.0.1")]
    pub host: String,
    /// Allow a local port below 1024.
    #[arg(long)]
    pub allow_privileged: bool,
}

/// Arguments for `vowp tunnel close`.
#[derive(Debug, Clone, Args)]
pub struct TunnelCloseArgs {
    /// Tunnel name.
    pub name: String,
}

/// How long `close` waits for the ssh process group to exit after SIGTERM.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// Runs `vowp tunnel`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &TunnelArgs) -> Result<i32, CliError> {
    if ctx.role() != NodeRole::Control {
        return Err(CliError::Guard(
            "tunnels are opened from the machine that will use them; run `vowp tunnel` on the control machine"
                .to_owned(),
        ));
    }
    let root = ctx.paths().root().to_path_buf();
    match &args.command {
        TunnelCommand::Open(open) => run_open(ctx, io, &root, open),
        TunnelCommand::Ls => run_list(ctx, io, &root),
        TunnelCommand::Close(close) => run_close(ctx, io, &root, close),
    }
}

/// Opens one managed forward.
fn run_open(ctx: &Ctx, io: &mut dyn IoStreams, root: &Path, args: &TunnelOpenArgs) -> Result<i32, CliError> {
    let name = args.name.trim();
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        return Err(CliError::Invalid(
            "tunnel name must be non-empty and contain no whitespace".to_owned(),
        ));
    }
    if args.remote_port == 0 {
        return Err(CliError::Invalid("remote port must not be 0".to_owned()));
    }
    if args.local_port == Some(0) {
        return Err(CliError::Invalid("local port must not be 0".to_owned()));
    }
    if args.host.trim().is_empty() || args.host.chars().any(char::is_whitespace) {
        return Err(CliError::Invalid(
            "forward host must be non-empty and contain no whitespace".to_owned(),
        ));
    }
    let local_port = match args.local_port {
        Some(port) => port,
        None => free_local_port()?,
    };
    if local_port < 1024 && !args.allow_privileged {
        return Err(CliError::Invalid(
            "refusing a local port below 1024; pass --allow-privileged to open it anyway".to_owned(),
        ));
    }
    let config = remote::read_control_config(root)?.ok_or_else(|| {
        CliError::NotFound(format!(
            "{}; run `vowp control init`",
            root.join("control.json").display()
        ))
    })?;

    let mut tunnels = load_tunnels(root)?;
    if clean_dead(&mut tunnels) {
        save_tunnels(root, &tunnels)?;
    }
    if tunnels.iter().any(|tunnel| tunnel.name == name) {
        return Err(CliError::Conflict(format!("tunnel {name} is already open")));
    }

    let forward = format!("127.0.0.1:{local_port}:{}:{}", args.host.trim(), args.remote_port);
    let mut child = Command::new("ssh")
        .args([
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ServerAliveInterval=15",
            "-L",
            &forward,
            &config.ssh_target,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| CliError::Remote(format!("ssh {}: {error}", config.ssh_target)))?;
    let pid = child.id();

    // `ExitOnForwardFailure` makes ssh fail quickly for an unusable port; catch that here
    // instead of leaving a dead record behind. A slow network failure is cleaned by `ls`.
    std::thread::sleep(Duration::from_millis(80));
    if let Some(status) = child
        .try_wait()
        .map_err(|error| CliError::Remote(format!("ssh {pid}: {error}")))?
    {
        return Err(CliError::Remote(format!(
            "ssh tunnel for {name} exited before it was ready (exit {:?})",
            status.code()
        )));
    }

    let tunnel = Tunnel {
        name: name.to_owned(),
        local_port,
        remote_port: args.remote_port,
        host: args.host.trim().to_owned(),
        pid,
        started_at: ctx.clock().now(),
    };
    tunnels.push(tunnel.clone());
    save_tunnels(root, &tunnels)?;

    if ctx.json() {
        output::print_json(io, &tunnel)?;
    } else {
        writeln!(
            io.out(),
            "tunnel {name} open: 127.0.0.1:{local_port} -> {}:{} (pid {pid})",
            tunnel.host,
            tunnel.remote_port
        )?;
    }
    Ok(0)
}

/// Lists the live tunnels and removes dead records.
fn run_list(ctx: &Ctx, io: &mut dyn IoStreams, root: &Path) -> Result<i32, CliError> {
    let mut tunnels = load_tunnels(root)?;
    if clean_dead(&mut tunnels) {
        save_tunnels(root, &tunnels)?;
    }
    if ctx.json() {
        output::print_json(io, &tunnels)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut table = Table::new(&["NAME", "LOCAL", "REMOTE", "HOST", "PID", "AGE"]);
    for tunnel in &tunnels {
        table.row(vec![
            tunnel.name.clone(),
            tunnel.local_port.to_string(),
            tunnel.remote_port.to_string(),
            tunnel.host.clone(),
            tunnel.pid.to_string(),
            format_age(now.since(tunnel.started_at).as_secs()),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Closes one tunnel by signalling its process group.
fn run_close(ctx: &Ctx, io: &mut dyn IoStreams, root: &Path, args: &TunnelCloseArgs) -> Result<i32, CliError> {
    let mut tunnels = load_tunnels(root)?;
    let index = tunnels
        .iter()
        .position(|tunnel| tunnel.name == args.name)
        .ok_or_else(|| CliError::NotFound(format!("tunnel {}", args.name)))?;
    let tunnel = tunnels.remove(index);
    if process_alive(tunnel.pid) {
        signal_group(tunnel.pid, Signal::Term)?;
        wait_for_exit(tunnel.pid, CLOSE_WAIT);
    }
    save_tunnels(root, &tunnels)?;

    if ctx.json() {
        output::print_json(io, &json!({ "name": tunnel.name, "closed": true }))?;
    } else {
        writeln!(io.out(), "closed tunnel {}", tunnel.name)?;
    }
    Ok(0)
}

/// Returns a free loopback port chosen by the operating system.
fn free_local_port() -> Result<u16, CliError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Reads `tunnels.json`; a missing file is an empty table.
fn load_tunnels(root: &Path) -> Result<Vec<Tunnel>, CliError> {
    let path = root.join("tunnels.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))
}

/// Replaces `tunnels.json` atomically.
fn save_tunnels(root: &Path, tunnels: &[Tunnel]) -> Result<(), CliError> {
    let text = serde_json::to_vec_pretty(tunnels).map_err(vowp_protocol::ProtocolError::from)?;
    remote::write_private(&root.join("tunnels.json"), &text)
}

/// Removes records whose pid no longer exists; returns whether anything changed.
fn clean_dead(tunnels: &mut Vec<Tunnel>) -> bool {
    let before = tunnels.len();
    tunnels.retain(|tunnel| process_alive(tunnel.pid));
    tunnels.len() != before
}

/// Waits briefly for a signalled process group to disappear.
fn wait_for_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Formats an age as `12s`, `3m 04s` or `1h 02m`.
fn format_age(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
