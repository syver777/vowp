//! `vowp control` — the control-machine role.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §8.1–§8.2. `control init` writes
//! `~/.vowp-control/control.json` (0600), the spool, the node registry mirror and the
//! tunnel table. `control status` prints the config and the snapshot cache metadata
//! without requiring SSH.
//!
//! JSON: `control init` prints one [`ControlConfig`]; `control status` prints
//! `{"config": ControlConfig, "cache": FleetStatus | null}`. The members are the exact
//! spec types from §8.1 and §5.5; the wrapper keeps the config (§8.1 says `control status`
//! prints it) next to the cache document.

use std::path::Path;
use std::process::Command;

use clap::{Args, Subcommand};
use serde_json::json;
use vowp_protocol::{FleetStatus, Node, NodeName, Timestamp};

use crate::node_ctx::{ControlConfig, NodeRole};
use crate::output;
use crate::remote;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp control`.
#[derive(Debug, Clone, Args)]
pub struct ControlArgs {
    /// The control subcommand.
    #[command(subcommand)]
    pub command: ControlCommand,
}

/// The `vowp control` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ControlCommand {
    /// Record the home this machine controls.
    Init(ControlInitArgs),
    /// Print the control configuration and cache metadata.
    Status,
}

/// Arguments for `vowp control init`.
#[derive(Debug, Clone, Args)]
pub struct ControlInitArgs {
    /// Name of the home node.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// SSH destination of the home.
    pub ssh_target: String,
    /// Fail when the SSH probe fails.
    #[arg(long)]
    pub strict: bool,
    /// Overwrite an existing control configuration.
    #[arg(long)]
    pub force: bool,
}

/// The default home node name when `--name` is omitted.
const DEFAULT_HOME_NAME: &str = "home";

/// Runs `vowp control`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &ControlArgs) -> Result<i32, CliError> {
    match &args.command {
        ControlCommand::Init(init) => run_init(ctx, io, init),
        ControlCommand::Status => run_status(ctx, io),
    }
}

/// Writes the control configuration and its companion files.
fn run_init(ctx: &Ctx, io: &mut dyn IoStreams, args: &ControlInitArgs) -> Result<i32, CliError> {
    if ctx.role() == NodeRole::Home {
        return Err(CliError::Guard(
            "this is a home; run `vowp control init` on the control machine".to_owned(),
        ));
    }
    if args.ssh_target.trim().is_empty() {
        return Err(CliError::Invalid("ssh target must not be empty".to_owned()));
    }
    let name = match &args.name {
        Some(name) => NodeName::parse(name)?,
        None => NodeName::parse(DEFAULT_HOME_NAME)?,
    };
    let root = match ctx.control() {
        Some(control) => control.root.clone(),
        None => remote::control_root()?,
    };
    let existing = remote::read_control_config(&root)?;
    if let Some(existing) = &existing {
        if !args.force {
            return Err(CliError::Conflict(format!(
                "control machine already configured for home {}; pass --force to replace {}",
                existing.home,
                root.join("control.json").display()
            )));
        }
    }

    let home = probe_remote_home(&args.ssh_target)?;
    let version = probe_version(&args.ssh_target);
    if args.strict && !version.ok {
        return Err(CliError::Remote(format!(
            "home {} probe failed: {}",
            args.ssh_target, version.detail
        )));
    }
    if !version.ok {
        writeln!(
            io.err(),
            "warning: home {} probe failed: {}",
            args.ssh_target,
            version.detail
        )?;
    }

    let now = ctx.clock().now();
    let config = ControlConfig {
        role: "control".to_owned(),
        home: name.to_string(),
        ssh_target: args.ssh_target.clone(),
        remote_home: join_remote(&home, ".vowp"),
        created_at: now,
    };
    remote::ensure_private_dir(&root)?;
    remote::ensure_private_dir(&root.join("spool"))?;
    remote::write_private(
        &root.join("control.json"),
        &serde_json::to_vec_pretty(&config).map_err(vowp_protocol::ProtocolError::from)?,
    )?;
    write_if_missing(&root.join("surfaces.json"), b"[]\n")?;
    write_if_missing(&root.join("tunnels.json"), b"[]\n")?;

    let node = Node {
        name: name.clone(),
        ssh_target: Some(args.ssh_target.clone()),
        is_local: false,
        capabilities: json!({
            "vowp": version.version,
            "probe_ok": version.ok,
            "probe": version.detail,
        }),
        last_seen_at: version.ok.then_some(now),
        created_at: now,
    };
    // The registry mirrors the configured home's fleet. Re-pointing the control machine
    // at a different home must not keep the old home's node row.
    let home_changed = existing.as_ref().is_some_and(|old| old.home != config.home);
    let mut nodes = if home_changed {
        Vec::new()
    } else {
        remote::read_node_registry(&root)?
    };
    nodes.retain(|existing| existing.name != node.name);
    nodes.push(node);
    nodes.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    remote::write_node_registry(&root, &nodes)?;

    if ctx.json() {
        output::print_json(io, &config)?;
    } else {
        writeln!(io.out(), "control machine configured")?;
        writeln!(io.out(), "home: {}", config.home)?;
        writeln!(io.out(), "ssh target: {}", config.ssh_target)?;
        writeln!(io.out(), "remote home: {}", config.remote_home)?;
        writeln!(io.out(), "next: vowp setup control")?;
    }
    Ok(0)
}

/// Prints the control configuration and cache metadata without SSH.
fn run_status(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let root = match ctx.role() {
        NodeRole::Control => ctx.paths().root().to_path_buf(),
        NodeRole::Home => {
            return Err(CliError::Guard(
                "this is a home; the control machine is configured with `vowp control init`".to_owned(),
            ));
        }
        NodeRole::Uninitialised => {
            return Err(CliError::NotFound(
                "no control configuration; run `vowp control init --name <home> <ssh-target>`".to_owned(),
            ));
        }
    };
    let config = remote::read_control_config(&root)?.ok_or_else(|| {
        CliError::NotFound(format!(
            "{}; run `vowp control init`",
            root.join("control.json").display()
        ))
    })?;
    let cache = remote::cached_snapshot(&root)?;
    let stale = cache.as_ref().is_some_and(remote::cached_is_stale);

    if ctx.json() {
        let cache_json = match &cache {
            Some(cached) => serde_json::to_value(FleetStatus {
                snapshot: cached.snapshot.clone(),
                source: "cache".to_owned(),
                fetched_at: cached.fetched_at,
                stale,
            })
            .map_err(vowp_protocol::ProtocolError::from)?,
            None => serde_json::Value::Null,
        };
        output::print_json(io, &json!({ "config": config, "cache": cache_json }))?;
        return Ok(0);
    }

    writeln!(io.out(), "home: {} ({})", config.home, config.ssh_target)?;
    writeln!(io.out(), "remote home: {}", config.remote_home)?;
    match &cache {
        Some(cached) => {
            let as_of = output::local_time(cached.fetched_at, Timestamp::now());
            writeln!(
                io.out(),
                "cache: as of {as_of} ({}, {})",
                cached.source,
                if stale { "stale" } else { "fresh" }
            )?;
        }
        None => writeln!(io.out(), "cache: empty")?,
    }
    Ok(0)
}

/// Writes a file only when it is absent, preserving owner edits.
fn write_if_missing(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if path.exists() {
        return Ok(());
    }
    remote::write_private(path, bytes)
}

/// Joins an absolute remote directory and a relative name without using local path rules.
fn join_remote(home: &str, name: &str) -> String {
    format!("{}/{}", home.trim_end_matches('/'), name)
}

/// The version probe result.
struct Probe {
    ok: bool,
    version: Option<String>,
    detail: String,
}

/// Probes `$HOME` on the home and returns its absolute path.
fn probe_remote_home(target: &str) -> Result<String, CliError> {
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            target,
            "printf %s \"$HOME\"",
        ])
        .output()
        .map_err(|error| CliError::Remote(format!("ssh {target}: {error}")))?;
    let home = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && home.starts_with('/') {
        return Ok(home.trim_end_matches('/').to_owned());
    }
    let detail = if output.status.success() {
        format!("unexpected $HOME value {home:?}")
    } else {
        format!(
            "exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(300)
                .collect::<String>()
        )
    };
    Err(CliError::Remote(format!("could not read $HOME on {target}: {detail}")))
}

/// Runs the documented `vowp --version` probe.
fn probe_version(target: &str) -> Probe {
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            target,
            "vowp --version",
        ])
        .output();
    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let version = stdout
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(str::trim)
                .map(str::to_owned);
            Probe {
                ok: output.status.success() && version.is_some(),
                version,
                detail: if output.status.success() {
                    if stderr.trim().is_empty() {
                        "probe ok".to_owned()
                    } else {
                        stderr.trim().chars().take(300).collect()
                    }
                } else {
                    format!(
                        "exit {:?}: {}",
                        output.status.code(),
                        stderr.trim().chars().take(300).collect::<String>()
                    )
                },
            }
        }
        Err(error) => Probe {
            ok: false,
            version: None,
            detail: format!("ssh could not run: {error}"),
        },
    }
}
