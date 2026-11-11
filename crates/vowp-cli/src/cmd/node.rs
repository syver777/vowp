//! `node`: register, list and remove the nodes of the VOWP network.
//!
//! Spec: `docs/design/cli.md` §5 and `docs/design/v0.2/home-and-surfaces.md` §8.2. On a
//! home the registry is the `nodes` table; on a control machine it is the `nodes.json`
//! mirror written by `vowp control init`. `node add` probes `ssh -o BatchMode=yes <target>
//! vowp --version` and stores the result in `capabilities`; a failed probe is a warning
//! unless `--strict` is passed. A target of `-` registers a surface-only executor node.

use std::process::Command;

use vowp_protocol::{Node, NodeName};

use crate::cli::{NodeAddArgs, NodeCommand, NodeRmArgs};
use crate::node_ctx::NodeRole;
use crate::output::{Table, local_time_at, print_json};
use crate::remote;
use crate::{CliError, Ctx, IoStreams};

/// Runs one node subcommand.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, command: &NodeCommand) -> Result<i32, CliError> {
    match command {
        NodeCommand::Add(args) => add(ctx, io, args),
        NodeCommand::Ls => list(ctx, io),
        NodeCommand::Rm(args) => remove(ctx, io, args),
    }
}

/// Adds or updates a node and probes it over ssh.
fn add(ctx: &Ctx, io: &mut dyn IoStreams, args: &NodeAddArgs) -> Result<i32, CliError> {
    require_registry(ctx)?;
    let name = NodeName::parse(&args.name)?;
    let surface_only = args.ssh_target.trim() == "-";
    if !surface_only && args.ssh_target.trim().is_empty() {
        return Err(CliError::Invalid("ssh target must not be empty".to_owned()));
    }
    let probe = if surface_only {
        Probe {
            ok: true,
            version: None,
            detail: "surface-only executor (dials the home)".to_owned(),
        }
    } else {
        probe(&args.ssh_target)
    };
    if args.strict && !probe.ok {
        return Err(CliError::Remote(format!("node {name} probe failed: {}", probe.detail)));
    }
    let now = ctx.clock().now();
    let created_at = existing_created_at(ctx, &name)?.unwrap_or(now);
    let capabilities = if surface_only {
        serde_json::json!({ "surface_only": true })
    } else {
        serde_json::json!({
            "vowp": probe.version,
            "probe_ok": probe.ok,
            "probe": probe.detail,
        })
    };
    let node = Node {
        name: name.clone(),
        ssh_target: (!surface_only).then(|| args.ssh_target.clone()),
        is_local: false,
        capabilities,
        last_seen_at: (probe.ok && !surface_only).then_some(now),
        created_at,
    };
    if ctx.role() == NodeRole::Home {
        ctx.store()?.upsert_node(&node)?;
    } else {
        let root = ctx.paths().root();
        let mut nodes = remote::read_node_registry(root)?;
        nodes.retain(|existing| existing.name != node.name);
        nodes.push(node.clone());
        nodes.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
        remote::write_node_registry(root, &nodes)?;
    }
    if !probe.ok {
        writeln!(io.err(), "warning: node {name} probe failed: {}", probe.detail)?;
    }
    if ctx.json() {
        print_json(io, &node)?;
    } else {
        match &node.ssh_target {
            Some(target) => writeln!(io.out(), "added node {} ({})", node.name, target)?,
            None => writeln!(io.out(), "added surface-only node {}", node.name)?,
        }
    }
    Ok(0)
}

/// Lists every node.
fn list(ctx: &Ctx, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    require_registry(ctx)?;
    let nodes = if ctx.role() == NodeRole::Home {
        ctx.store()?.list_nodes()?
    } else {
        remote::read_node_registry(ctx.paths().root())?
    };
    if ctx.json() {
        print_json(io, &nodes)?;
        return Ok(0);
    }
    let now = ctx.clock().now();
    let mut table = Table::new(&["NAME", "TARGET", "LOCAL", "LAST_SEEN", "VERSION"]);
    for node in &nodes {
        table.row(vec![
            node.name.to_string(),
            node.ssh_target.clone().unwrap_or_else(|| "-".to_owned()),
            if node.is_local { "yes" } else { "no" }.to_owned(),
            node.last_seen_at
                .map(|ts| local_time_at(ts, now, ctx.tz_offset_minutes()))
                .unwrap_or_else(|| "-".to_owned()),
            node.capabilities
                .get("vowp")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_owned(),
        ]);
    }
    writeln!(io.out(), "{}", table.render(ctx.color()))?;
    Ok(0)
}

/// Removes a remote node.
fn remove(ctx: &Ctx, io: &mut dyn IoStreams, args: &NodeRmArgs) -> Result<i32, CliError> {
    require_registry(ctx)?;
    let name = NodeName::parse(&args.name)?;
    let node = if ctx.role() == NodeRole::Home {
        let store = ctx.store()?;
        let node = store.require_node(&name)?;
        store.remove_node(&name)?;
        node
    } else {
        let root = ctx.paths().root();
        let mut nodes = remote::read_node_registry(root)?;
        let index = nodes
            .iter()
            .position(|node| node.name == name)
            .ok_or_else(|| CliError::NotFound(format!("node {name}")))?;
        let node = nodes.remove(index);
        if node.is_local {
            return Err(CliError::Conflict("cannot remove the local node".to_owned()));
        }
        remote::write_node_registry(root, &nodes)?;
        node
    };
    if ctx.json() {
        print_json(io, &node)?;
    } else {
        writeln!(io.out(), "removed node {}", node.name)?;
    }
    Ok(0)
}

/// Refuses node commands on a machine that has neither registry.
fn require_registry(ctx: &Ctx) -> Result<(), CliError> {
    match ctx.role() {
        NodeRole::Home | NodeRole::Control => Ok(()),
        NodeRole::Uninitialised => Err(CliError::NotFound(
            "vowp home is not initialised; run `vowp init` or `vowp control init`".to_owned(),
        )),
    }
}

/// Returns an existing node's creation time from the active registry.
fn existing_created_at(ctx: &Ctx, name: &NodeName) -> Result<Option<vowp_protocol::Timestamp>, CliError> {
    if ctx.role() == NodeRole::Home {
        return Ok(ctx.store()?.node(name)?.map(|node| node.created_at));
    }
    Ok(remote::read_node_registry(ctx.paths().root())?
        .into_iter()
        .find(|node| node.name == *name)
        .map(|node| node.created_at))
}

/// One ssh probe result.
struct Probe {
    ok: bool,
    version: Option<String>,
    detail: String,
}

/// Runs the documented version probe; never reads or prints environment values.
fn probe(target: &str) -> Probe {
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
