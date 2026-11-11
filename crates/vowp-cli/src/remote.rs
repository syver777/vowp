//! `--node <name>` redirection over ssh and the control-machine route to the home.
//!
//! Spec: `docs/design/cli.md` §7, `docs/design/crates/vowp-cli.md` §2 and
//! `docs/design/v0.2/home-and-surfaces.md` §8.1–§8.3. The argv builder is pure and tested
//! without executing anything; [`run_remote`] returns the remote exit code unchanged and
//! maps ssh's own failure (255) to [`CliError::Remote`].
//!
//! On a control machine every command that needs the home is routed here, before
//! [`crate::Ctx::build`] would look for a local database. `fleet status` refreshes the
//! snapshot cache from the home's `snapshot.json`; `status` and `fleet status` serve the
//! cache with an "as of" time when the home is unreachable. `status --check` keeps the
//! v0.1 watchdog contract: a cached snapshot whose heartbeat is stale exits 1. Writes
//! never use the cache. `secret request` is wrapped in a local tunnel so the owner's
//! browser only sees a control-machine loopback URL (§9.2–§9.3).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_adapters::proc::{Signal, process_alive, signal_group};
use vowp_protocol::{FleetStatus, Node, NodeName, SecretName, SecretRequest, Snapshot, Timestamp, Tunnel};
use vowp_store::Store;
use vowp_tmux::quote::shell_quote;

use crate::CliError;
use crate::cli::{Command as CliCommand, Global, SessionCommand};
use crate::cmd::secret::SecretCommand;
use crate::node_ctx::{CachedSnapshot, ControlConfig, ControlHome, NodeCtx};
use crate::output::{self, Table};

/// Redirects the command when `--node` names a remote node or this is a control machine.
///
/// Returns `Ok(None)` when the command must run locally. On a control machine that means
/// the control/tunnel/node/setup group and every command that only reads local files; a
/// command that needs local truth either routes to the home or is refused by
/// [`control_refusal`].
pub fn maybe_run(global: &Global, command: &CliCommand, original_args: &[String]) -> Result<Option<i32>, CliError> {
    let node = NodeCtx::discover(global)?;
    let control = node.require_control().ok().cloned();

    if let Some(name) = &global.node {
        let name = NodeName::parse(name)?;
        match resolve_target(global, control.as_ref(), &name)? {
            Some(target) => {
                let args = match control.as_ref() {
                    Some(control) if control.config.home == name.as_str() => {
                        control_remote_args(&control.config.remote_home, original_args)
                    }
                    _ => strip_node_flag(original_args),
                };
                let interactive = is_interactive(command);
                return run_remote(&target, &args, interactive).map(Some);
            }
            None if control.is_none() => return Ok(None),
            None => {}
        }
    }

    let Some(control) = control else {
        return Ok(None);
    };
    match control_action(command) {
        ControlAction::Local => Ok(None),
        ControlAction::Refuse => Err(control_refusal()),
        ControlAction::Route(RouteKind::Plain) => {
            let args = control_remote_args(&control.config.remote_home, original_args);
            run_remote(&control.config.ssh_target, &args, is_interactive(command)).map(Some)
        }
        ControlAction::Route(RouteKind::Status) => {
            route_status(global, &control, command, original_args, false).map(Some)
        }
        ControlAction::Route(RouteKind::FleetStatus) => {
            let cached = fleet_status_cached(command);
            let refresh = fleet_status_refresh(command);
            route_status(global, &control, command, original_args, cached || refresh).map(Some)
        }
        ControlAction::Route(RouteKind::SecretRequest) => {
            route_secret_request(global, &control, command, original_args).map(Some)
        }
    }
}

/// How a command behaves on a control machine.
enum ControlAction {
    /// Run in this process, against the control directory.
    Local,
    /// The command needs local truth and is refused.
    Refuse,
    /// Send it to the home.
    Route(RouteKind),
}

/// What kind of answer a routed command needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    /// Plain pass-through of the home's human or JSON output.
    Plain,
    /// `vowp status`, which has a cached fallback.
    Status,
    /// `vowp fleet status`, which also has a cached fallback and `--cached`/`--refresh`.
    FleetStatus,
    /// `vowp secret request`, wrapped in a local tunnel to the home's paste page.
    SecretRequest,
}

/// Returns whether the command needs the home on a control machine.
fn control_action(command: &CliCommand) -> ControlAction {
    match command {
        // These read or write the control machine itself.
        CliCommand::Control(_)
        | CliCommand::Tunnel(_)
        | CliCommand::Node { .. }
        | CliCommand::Setup(_)
        | CliCommand::Doctor(_)
        | CliCommand::Version
        | CliCommand::Schema(_)
        | CliCommand::Mcp(_)
        | CliCommand::Window(_)
        | CliCommand::Exec(_)
        | CliCommand::VaultExec(_)
        | CliCommand::VaultAdmin(_) => ControlAction::Local,
        // These need a local supervisor, graph or worktrees.
        CliCommand::Up(_) | CliCommand::Down(_) | CliCommand::Board(_) => ControlAction::Refuse,
        CliCommand::Status(_) => ControlAction::Route(RouteKind::Status),
        CliCommand::Fleet(args) => match &args.command {
            crate::cmd::fleet::FleetCommand::Status(_) => ControlAction::Route(RouteKind::FleetStatus),
            _ => ControlAction::Route(RouteKind::Plain),
        },
        CliCommand::Secret(args) => match &args.command {
            SecretCommand::Request(_) => ControlAction::Route(RouteKind::SecretRequest),
            _ => ControlAction::Route(RouteKind::Plain),
        },
        _ => ControlAction::Route(RouteKind::Plain),
    }
}

/// Returns the guard error for a command that cannot run on a control machine.
fn control_refusal() -> CliError {
    CliError::Guard(
        "this is a control machine; the home runs the supervisor (use `vowp control status` and `vowp fleet status`)"
            .to_owned(),
    )
}

/// Resolves the ssh target named by `--node`, or `None` when it is this machine.
fn resolve_target(global: &Global, control: Option<&ControlHome>, name: &NodeName) -> Result<Option<String>, CliError> {
    if let Some(control) = control {
        if control.config.home == name.as_str() {
            return Ok(Some(control.config.ssh_target.clone()));
        }
        let nodes = read_node_registry(&control.root)?;
        let node = nodes
            .into_iter()
            .find(|node| node.name == *name)
            .ok_or_else(|| CliError::NotFound(format!("node {name}")))?;
        if node.is_local {
            return Ok(None);
        }
        return node
            .ssh_target
            .map(Some)
            .ok_or_else(|| CliError::Invalid(format!("node {name} has no ssh target")));
    }

    let paths = crate::ctx::node_paths(global)?;
    if !paths.db_path().is_file() {
        return Err(CliError::NotFound(
            "vowp home is not initialised; run `vowp init`".to_owned(),
        ));
    }
    let store = Store::open(&paths)?;
    let node = store
        .node(name)?
        .ok_or_else(|| CliError::NotFound(format!("node {name}")))?;
    if node.is_local {
        return Ok(None);
    }
    node.ssh_target
        .map(Some)
        .ok_or_else(|| CliError::Invalid(format!("node {name} has no ssh target")))
}

/// Returns true for commands that attach to a terminal on the remote node.
pub fn is_interactive(command: &CliCommand) -> bool {
    match command {
        CliCommand::Attach(_) | CliCommand::Chat(_) => true,
        CliCommand::Session {
            command: SessionCommand::Chat(_),
        } => true,
        CliCommand::Board(args) => !args.once,
        // §9.3: the home's hidden prompt needs a TTY, so the routed write uses `ssh -t`.
        CliCommand::Secret(args) => matches!(args.command, SecretCommand::Set(_)),
        _ => false,
    }
}

/// Removes `--node <name>` and `--node=<name>` from an argument list.
pub fn strip_node_flag(args: &[String]) -> Vec<String> {
    strip_value_flag(args, "--node")
}

/// Removes `--home <dir>` and `--home=<dir>` from an argument list.
pub fn strip_home_flag(args: &[String]) -> Vec<String> {
    strip_value_flag(args, "--home")
}

/// Removes one global flag that takes a separate or `=` value.
fn strip_value_flag(args: &[String], flag: &str) -> Vec<String> {
    let prefix = format!("{flag}=");
    let mut stripped = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == flag {
            skip_next = true;
        } else if !arg.starts_with(&prefix) {
            stripped.push(arg.clone());
        }
    }
    stripped
}

/// Removes every occurrence of a bare boolean switch from an argument list.
fn strip_switch_flag(args: &[String], flag: &str) -> Vec<String> {
    args.iter().filter(|arg| arg.as_str() != flag).cloned().collect()
}

/// Builds the arguments for a control-machine call to the home.
///
/// The local arguments are forwarded unchanged except that the local `--home` (if one
/// slipped through) is replaced by the home's absolute path. The quoted ssh argv does not
/// expand `~`, so the home path is always absolute.
fn control_remote_args(remote_home: &str, args: &[String]) -> Vec<String> {
    let mut routed = vec!["--home".to_owned(), remote_home.to_owned()];
    routed.extend(strip_home_flag(&strip_node_flag(args)));
    routed
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
    remote_status(status, target)
}

/// Routes `status` or `fleet status`, with the snapshot cache as the offline fallback.
///
/// Only a successful `fleet status` refreshes the cache from the home's `snapshot.json`;
/// `status` is a pass-through and adds no second ssh call. When ssh itself fails (255) and
/// the command did not ask for a refresh, the last cached snapshot is printed with its
/// "as of" time. `status --check` returns 1 when that cache has no fresh heartbeat, so the
/// watchdog contract survives an unreachable home. A write never reaches this function.
fn route_status(
    global: &Global,
    control: &ControlHome,
    command: &CliCommand,
    original_args: &[String],
    cache_only: bool,
) -> Result<i32, CliError> {
    let fleet_status = matches!(control_action(command), ControlAction::Route(RouteKind::FleetStatus));
    let refresh = fleet_status && fleet_status_refresh(command);
    let check = status_check(command);
    let kind = if fleet_status {
        RouteKind::FleetStatus
    } else {
        RouteKind::Status
    };

    if cache_only && !refresh {
        return print_cached(control, kind, global.json);
    }

    let args = control_remote_args(&control.config.remote_home, original_args);
    match run_remote(&control.config.ssh_target, &args, false) {
        Ok(code) => {
            // `fleet status` is the read-model command that refreshes the snapshot cache.
            // `status` is a pass-through; it must not add a second ssh call behind the
            // user's command.
            if code == 0 && fleet_status {
                let _ = refresh_cache(control);
            }
            Ok(code)
        }
        Err(CliError::Remote(message)) if !refresh => match cached_snapshot(&control.root)? {
            Some(cached) => {
                let now = Timestamp::now();
                let stale = cached_is_stale_at(&cached, now);
                print_cached_value(kind, &cached, global.json, &message, stale)?;
                // A stale heartbeat is the v0.1 watchdog's failure signal, even when the
                // only available document came from the cache.
                if check && heartbeat_is_stale(&cached.snapshot, now) {
                    Ok(1)
                } else {
                    Ok(0)
                }
            }
            None => Err(CliError::Remote(message)),
        },
        Err(error) => Err(error),
    }
}

/// Routes `secret request` through a local tunnel to the home's one-time paste page.
///
/// §9.2–§9.3: the page listens on the home's loopback, so a control machine forwards it
/// and prints its own local URL. The home's raw URL is parsed but never shown to the
/// owner. The remote command prints the human header and then waits for one POST; the
/// wrapper forwards its completion line and returns the home's exit code.
fn route_secret_request(
    global: &Global,
    control: &ControlHome,
    command: &CliCommand,
    original_args: &[String],
) -> Result<i32, CliError> {
    let name = match command {
        CliCommand::Secret(args) => match &args.command {
            SecretCommand::Request(request) => SecretName::parse(&request.name)?,
            _ => return Err(CliError::Invalid("not a secret request".to_owned())),
        },
        _ => return Err(CliError::Invalid("not a secret request".to_owned())),
    };

    // Force the remote command's two-line human header so the wrapper can read the port
    // and expiry. The local `--json` document is rendered below with the local URL.
    let args = strip_switch_flag(
        &control_remote_args(&control.config.remote_home, original_args),
        "--json",
    );
    let argv = build_ssh_argv(&control.config.ssh_target, &args, false);
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| CliError::Remote("empty ssh argv".to_owned()))?;
    let mut child = ProcessCommand::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| CliError::Remote(format!("ssh {}: {error}", control.config.ssh_target)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::Remote("ssh secret request produced no stdout".to_owned()))?;
    let mut reader = BufReader::new(stdout);

    let mut first = String::new();
    reader.read_line(&mut first)?;
    if first.trim().is_empty() {
        let status = child.wait()?;
        return remote_status(status, &control.config.ssh_target);
    }
    let first = first.trim_end();
    let Some(url) = first.strip_prefix("open ") else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::Remote(format!(
            "unexpected secret request output from the home: {first:?}"
        )));
    };
    let Some((remote_port, path)) = split_request_url(url) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::Remote(format!(
            "unexpected secret request URL from the home: {url:?}"
        )));
    };

    let mut expiry = String::new();
    reader.read_line(&mut expiry)?;
    let expiry = expiry.trim_end();
    let expires_at = expiry
        .strip_prefix("expires ")
        .and_then(|text| text.split_whitespace().next())
        .and_then(|text| Timestamp::parse(text).ok());
    let Some(expires_at) = expires_at else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CliError::Remote(format!(
            "unexpected secret request expiry from the home: {expiry:?}"
        )));
    };

    let tunnel = match open_request_tunnel(control, &name, remote_port) {
        Ok(tunnel) => tunnel,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let local_url = format!("http://127.0.0.1:{}/{path}", tunnel.local_port);
    if global.json {
        let request = SecretRequest {
            name: name.clone(),
            url: local_url,
            expires_at,
            single_use: true,
        };
        let text = serde_json::to_string_pretty(&request).map_err(vowp_protocol::ProtocolError::from)?;
        write_stdout(&format!("{text}\n"));
    } else {
        let mut text = format!("open {local_url}\n");
        text.push_str(expiry);
        text.push('\n');
        write_stdout(&text);
    }

    // Forward the home's completion line while it waits for the one-time POST.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !global.json {
            write_stdout(&line);
        }
    }
    let status = child.wait()?;
    drop(tunnel);
    remote_status(status, &control.config.ssh_target)
}

/// Splits `http://127.0.0.1:<port>/<path>` into the home port and the path.
fn split_request_url(url: &str) -> Option<(u16, &str)> {
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let (port, path) = rest.split_once('/')?;
    Some((port.parse().ok()?, path))
}

/// Maps an ssh exit status to the routed exit code, with 255 as a remote failure.
fn remote_status(status: std::process::ExitStatus, target: &str) -> Result<i32, CliError> {
    match status.code() {
        Some(255) => Err(CliError::Remote(format!("ssh {target} failed with exit 255"))),
        Some(code) => Ok(code),
        None => Err(CliError::Remote(format!("ssh {target} terminated by a signal"))),
    }
}

/// A tunnel opened for one secret request, closed when the request finishes.
struct RequestTunnel {
    root: PathBuf,
    name: String,
    local_port: u16,
    owned: bool,
}

impl Drop for RequestTunnel {
    fn drop(&mut self) {
        if self.owned {
            let _ = close_request_tunnel(&self.root, &self.name);
        }
    }
}

/// Opens or reuses the per-secret local forward to the home's paste page.
fn open_request_tunnel(
    control: &ControlHome,
    secret: &SecretName,
    remote_port: u16,
) -> Result<RequestTunnel, CliError> {
    let name = format!("secret-{secret}");
    let mut tunnels = load_request_tunnels(&control.root)?;
    let before = tunnels.len();
    tunnels.retain(|tunnel| process_alive(tunnel.pid));
    if tunnels.len() != before {
        save_request_tunnels(&control.root, &tunnels)?;
    }
    if let Some(index) = tunnels.iter().position(|tunnel| tunnel.name == name) {
        let existing = &tunnels[index];
        if existing.remote_port == remote_port && existing.host == "127.0.0.1" {
            return Ok(RequestTunnel {
                root: control.root.clone(),
                name,
                local_port: existing.local_port,
                owned: false,
            });
        }
        let stale = tunnels.remove(index);
        if process_alive(stale.pid) {
            signal_group(stale.pid, Signal::Term)?;
            wait_for_exit(stale.pid, Duration::from_secs(2));
        }
        save_request_tunnels(&control.root, &tunnels)?;
    }

    let local_port = free_local_port()?;
    let forward = format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}");
    let mut child = ProcessCommand::new("ssh")
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
            &control.config.ssh_target,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| CliError::Remote(format!("ssh {}: {error}", control.config.ssh_target)))?;
    let pid = child.id();

    // `ExitOnForwardFailure` makes ssh fail quickly for an unusable port.
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

    tunnels.push(Tunnel {
        name: name.clone(),
        local_port,
        remote_port,
        host: "127.0.0.1".to_owned(),
        pid,
        started_at: Timestamp::now(),
    });
    if let Err(error) = save_request_tunnels(&control.root, &tunnels) {
        let _ = signal_group(pid, Signal::Term);
        return Err(error);
    }
    Ok(RequestTunnel {
        root: control.root.clone(),
        name,
        local_port,
        owned: true,
    })
}

/// Closes a request tunnel and removes its record.
fn close_request_tunnel(root: &Path, name: &str) -> Result<(), CliError> {
    let mut tunnels = load_request_tunnels(root)?;
    let Some(index) = tunnels.iter().position(|tunnel| tunnel.name == name) else {
        return Ok(());
    };
    let tunnel = tunnels.remove(index);
    if process_alive(tunnel.pid) {
        signal_group(tunnel.pid, Signal::Term)?;
        wait_for_exit(tunnel.pid, Duration::from_secs(2));
    }
    save_request_tunnels(root, &tunnels)
}

/// Reads the control tunnel table; a missing file is empty.
fn load_request_tunnels(root: &Path) -> Result<Vec<Tunnel>, CliError> {
    let path = root.join("tunnels.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))
}

/// Replaces the control tunnel table atomically.
fn save_request_tunnels(root: &Path, tunnels: &[Tunnel]) -> Result<(), CliError> {
    let text = serde_json::to_vec_pretty(tunnels).map_err(vowp_protocol::ProtocolError::from)?;
    write_private(&root.join("tunnels.json"), &text)
}

/// Returns a free loopback port chosen by the operating system.
fn free_local_port() -> Result<u16, CliError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Waits briefly for a signalled tunnel process group to exit.
fn wait_for_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Returns the `--check` flag of `status`.
fn status_check(command: &CliCommand) -> bool {
    match command {
        CliCommand::Status(args) => args.check,
        _ => false,
    }
}

/// Returns the `--cached` flag of `fleet status`.
fn fleet_status_cached(command: &CliCommand) -> bool {
    match command {
        CliCommand::Fleet(args) => match &args.command {
            crate::cmd::fleet::FleetCommand::Status(status) => status.cached,
            _ => false,
        },
        _ => false,
    }
}

/// Returns the `--refresh` flag of `fleet status`.
fn fleet_status_refresh(command: &CliCommand) -> bool {
    match command {
        CliCommand::Fleet(args) => match &args.command {
            crate::cmd::fleet::FleetCommand::Status(status) => status.refresh,
            _ => false,
        },
        _ => false,
    }
}

/// Prints the cached snapshot, requiring one to exist.
fn print_cached(control: &ControlHome, kind: RouteKind, json: bool) -> Result<i32, CliError> {
    let Some(cached) = cached_snapshot(&control.root)? else {
        return Err(CliError::NotFound(format!(
            "no cached snapshot in {}; run `vowp fleet status --refresh`",
            control.root.join("snapshot.json").display()
        )));
    };
    let stale = cached_is_stale_at(&cached, Timestamp::now());
    print_cached_value(kind, &cached, json, "", stale)?;
    Ok(0)
}

/// Prints `status` or `fleet status` from a cached snapshot.
fn print_cached_value(
    kind: RouteKind,
    cached: &CachedSnapshot,
    json: bool,
    detail: &str,
    stale: bool,
) -> Result<(), CliError> {
    let now = Timestamp::now();
    if json {
        let text = match kind {
            RouteKind::FleetStatus => {
                let status = FleetStatus {
                    snapshot: cached.snapshot.clone(),
                    source: "cache".to_owned(),
                    fetched_at: cached.fetched_at,
                    stale,
                };
                serde_json::to_string_pretty(&status).map_err(vowp_protocol::ProtocolError::from)?
            }
            RouteKind::Status => {
                let value = status_json(&cached.snapshot, cached.fetched_at, stale);
                serde_json::to_string_pretty(&value).map_err(vowp_protocol::ProtocolError::from)?
            }
            RouteKind::Plain | RouteKind::SecretRequest => return Ok(()),
        };
        write_stdout(&format!("{text}\n"));
        return Ok(());
    }

    let as_of = output::local_time(cached.fetched_at, now);
    let mut text = format!(
        "home: {} (cache, as of {as_of}, {})\n",
        cached.snapshot.home,
        if stale { "stale" } else { "fresh" }
    );
    if !detail.is_empty() {
        text.push_str(&format!("connection: {detail}\n"));
    }
    text.push_str(&render_snapshot_summary(&cached.snapshot));
    write_stdout(&text);
    Ok(())
}

/// Builds the v0.1-compatible `status --json` document from a snapshot.
fn status_json(snapshot: &Snapshot, fetched_at: Timestamp, stale: bool) -> serde_json::Value {
    let mut jobs = std::collections::BTreeMap::new();
    let mut running = 0_u32;
    for project in &snapshot.projects {
        for (state, count) in &project.jobs_by_state {
            *jobs.entry(state.clone()).or_insert(0_u32) += count;
        }
        running += project.running.len() as u32;
    }
    serde_json::json!({
        "home": &snapshot.home,
        "source": "cache",
        "as_of": fetched_at.as_rfc3339(),
        "stale": stale,
        "supervisor": {
            "running": snapshot.supervisor.running,
            "pid": snapshot.supervisor.pid,
            "last_tick": snapshot.supervisor.last_tick.map(|at| at.as_rfc3339()),
            "heartbeat_age_secs": serde_json::Value::Null,
            "paused": snapshot.supervisor.paused,
        },
        "jobs": jobs,
        "slots": {
            "busy": running,
            "total": snapshot.capacity.target.target_workers,
        },
        "limits": &snapshot.limits,
        "cost_today_usd": snapshot.projects.iter().filter_map(|p| p.cost_today_usd).reduce(|a, b| a + b),
        "inbox_open": snapshot.inbox.len(),
        "projects": snapshot.projects.iter().map(|project| serde_json::json!({
            "name": &project.name,
            "state": project.state,
            "running": project.running.len(),
            "ready": project.jobs_by_state.get("ready").copied().unwrap_or(0),
            "blocked": project.blocked.len(),
            "paused_reason": &project.paused_reason,
        })).collect::<Vec<_>>(),
    })
}

/// Renders the short human summary shared by cached `status` and `fleet status`.
fn render_snapshot_summary(snapshot: &Snapshot) -> String {
    let mut text = String::new();
    if snapshot.supervisor.running {
        match snapshot.supervisor.pid {
            Some(pid) => text.push_str(&format!("supervisor: running (pid {pid})\n")),
            None => text.push_str("supervisor: running\n"),
        }
    } else {
        text.push_str("supervisor: not running\n");
    }

    let mut table = Table::new(&["PROJECT", "STATE", "RUNNING", "QUEUED", "BLOCKED"]);
    for project in &snapshot.projects {
        let queued: u32 = project
            .jobs_by_state
            .iter()
            .filter(|(state, _)| {
                matches!(
                    state.as_str(),
                    "ready" | "waiting" | "admitted" | "queued" | "needs_attention"
                )
            })
            .map(|(_, count)| *count)
            .sum();
        table.row(vec![
            project.name.to_string(),
            format!("{:?}", project.state).to_lowercase(),
            project.running.len().to_string(),
            queued.to_string(),
            project.blocked.len().to_string(),
        ]);
    }
    if snapshot.projects.is_empty() {
        text.push_str("projects: none\n");
    } else {
        text.push_str(&table.render(false));
    }
    text.push_str(&format!("inbox: {} open\n", snapshot.inbox.len()));
    text
}

/// Returns whether a cached snapshot is stale.
///
/// A cache is stale when the fetch is older than three supervisor ticks, or when the
/// heartbeat recorded inside it is older than the v0.1 three-minute watchdog limit. The
/// second check matters when a fresh fetch carried an already stalled heartbeat.
fn cached_is_stale_at(cached: &CachedSnapshot, now: Timestamp) -> bool {
    snapshot_is_stale(&cached.snapshot, cached.fetched_at, now) || heartbeat_is_stale(&cached.snapshot, now)
}

/// Returns whether a cached snapshot is stale, measured at the current time.
pub(crate) fn cached_is_stale(cached: &CachedSnapshot) -> bool {
    cached_is_stale_at(cached, Timestamp::now())
}

/// Returns whether a snapshot is older than three supervisor ticks.
fn snapshot_is_stale(snapshot: &Snapshot, fetched_at: Timestamp, now: Timestamp) -> bool {
    let tick = if snapshot.supervisor.tick_secs == 0 {
        60
    } else {
        snapshot.supervisor.tick_secs
    };
    let limit = i64::try_from(tick.saturating_mul(3)).unwrap_or(i64::MAX);
    now.unix_secs().saturating_sub(fetched_at.unix_secs()) > limit
}

/// Returns whether the supervisor heartbeat in `snapshot` is stale.
///
/// This is the v0.1 watchdog contract from `cli.md` §2: a missing heartbeat or one older
/// than [`crate::cmd::status::STALE_AFTER_SECS`] means `status --check` exits 1.
fn heartbeat_is_stale(snapshot: &Snapshot, now: Timestamp) -> bool {
    snapshot.supervisor.last_tick.is_none_or(|last_tick| {
        now.unix_secs().saturating_sub(last_tick.unix_secs()) > crate::cmd::status::STALE_AFTER_SECS
    })
}

/// Fetches `snapshot.json` from the home and writes the local cache.
fn refresh_cache(control: &ControlHome) -> Result<CachedSnapshot, CliError> {
    let remote_path = format!("{}/snapshot.json", control.config.remote_home.trim_end_matches('/'));
    let command = format!("cat {}", shell_quote(&remote_path));
    let output = ProcessCommand::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &control.config.ssh_target,
            &command,
        ])
        .output()
        .map_err(|error| CliError::Remote(format!("ssh {}: {error}", control.config.ssh_target)))?;
    if !output.status.success() {
        return Err(CliError::Remote(format!(
            "ssh {} could not read {remote_path}",
            control.config.ssh_target
        )));
    }
    let snapshot: Snapshot = serde_json::from_slice(&output.stdout)
        .map_err(|error| CliError::Remote(format!("invalid snapshot from the home: {error}")))?;
    let fetched_at = Timestamp::now();
    write_private(
        &control.root.join("snapshot.json"),
        &serde_json::to_vec_pretty(&snapshot).map_err(vowp_protocol::ProtocolError::from)?,
    )?;
    let meta = SnapshotMeta {
        fetched_at: Some(fetched_at),
        source: Some("remote".to_owned()),
    };
    write_private(
        &control.root.join("snapshot.meta.json"),
        &serde_json::to_vec_pretty(&meta).map_err(vowp_protocol::ProtocolError::from)?,
    )?;
    Ok(CachedSnapshot {
        snapshot,
        fetched_at,
        source: "remote".to_owned(),
    })
}

/// The on-disk `snapshot.meta.json` document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SnapshotMeta {
    /// When the snapshot was fetched.
    #[serde(default)]
    pub fetched_at: Option<Timestamp>,
    /// Where it came from (`remote`, `local` or `cache`).
    #[serde(default)]
    pub source: Option<String>,
}

/// Reads the control snapshot cache; a missing or malformed cache is `None`.
pub(crate) fn cached_snapshot(root: &Path) -> Result<Option<CachedSnapshot>, CliError> {
    let snapshot_path = root.join("snapshot.json");
    let Ok(text) = std::fs::read_to_string(&snapshot_path) else {
        return Ok(None);
    };
    let Ok(snapshot) = serde_json::from_str::<Snapshot>(&text) else {
        return Ok(None);
    };
    let meta = std::fs::read_to_string(root.join("snapshot.meta.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<SnapshotMeta>(&text).ok())
        .unwrap_or_default();
    Ok(Some(CachedSnapshot {
        fetched_at: meta.fetched_at.unwrap_or(snapshot.generated_at),
        source: meta.source.unwrap_or_else(|| "cache".to_owned()),
        snapshot,
    }))
}

/// Returns the control directory: `$VOWP_CONTROL_HOME`, else `$HOME/.vowp-control`.
pub(crate) fn control_root() -> Result<PathBuf, CliError> {
    if let Some(root) = std::env::var_os("VOWP_CONTROL_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CliError::NotFound("HOME is not set; set VOWP_CONTROL_HOME to a control directory".to_owned())
        })?;
    Ok(PathBuf::from(home).join(".vowp-control"))
}

/// Reads `control.json` from `root`, when it exists.
pub(crate) fn read_control_config(root: &Path) -> Result<Option<ControlConfig>, CliError> {
    let path = root.join("control.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let config: ControlConfig =
        serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))?;
    if config.role != "control" {
        return Err(CliError::Invalid(format!(
            "{} has role {:?}; expected \"control\"",
            path.display(),
            config.role
        )));
    }
    Ok(Some(config))
}

/// Reads the control machine's `nodes.json` registry mirror.
pub(crate) fn read_node_registry(root: &Path) -> Result<Vec<Node>, CliError> {
    let path = root.join("nodes.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))
}

/// Replaces the control machine's `nodes.json` registry mirror.
pub(crate) fn write_node_registry(root: &Path, nodes: &[Node]) -> Result<(), CliError> {
    let text = serde_json::to_vec_pretty(nodes).map_err(vowp_protocol::ProtocolError::from)?;
    write_private(&root.join("nodes.json"), &text)
}

/// Writes `bytes` to `path` atomically with mode 0600, creating private parents.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    set_private_mode(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Creates `dir` and its parents, then makes the directory private (0700).
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<(), CliError> {
    std::fs::create_dir_all(dir)?;
    set_private_mode(dir)
}

/// Sets mode 0600 on a file or 0700 on a directory.
fn set_private_mode(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Writes routing output that has no [`IoStreams`] (the control fallback).
///
/// This mirrors [`run_remote`], which already passes the remote process's stdio straight
/// through: the routing layer runs before [`crate::Ctx`] and therefore has no buffer.
fn write_stdout(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}
