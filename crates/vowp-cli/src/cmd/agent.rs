//! `agent`: enroll, run and inspect the Mac-side v0.4 agent, and serve the hidden home gateway.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.2, §8.2–§8.5, §12.2–§12.3 and
//! `docs/design/cli.md` §12. The Mac commands use `~/.vowp-agent` (or `VOWP_AGENT_HOME`) and never
//! open a project store or `vowp-core`; the hidden `agent gateway` command is the home-side
//! exception and opens the home store through the node context.

use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::Serialize;
use vowp_agent::service::{
    self, InstallOptions, ServiceError, ServicePlatform, ServiceSpec, SystemManager, run_marker_path,
};
use vowp_agent::{
    AgentClient, AgentConfig, AgentError, AgentStatus, ConnectionState, IsolationError, NodeKey, OrchestratorHost,
    PairedSurface, RunOptions, RunOutcome, ServiceRunOutcome, SurfacePairRequest, SurfaceStatus, SystemAgentClock,
    TransportConfig, WssIngress, WssListener, WssTls, WssTlsMode, WssTransport, check_state_dir, current_uid,
    ensure_not_root, ensure_state_dir, fingerprint_of, is_fingerprint, load_public, pending_spool_bytes,
    surface_statuses, surface_token_expiry, transport_for_config, validate_enrollment_code,
};
use vowp_core::Deps;
use vowp_core::agent::gateway;
use vowp_protocol::{
    AGENT_PROTOCOL, AgentPathMap, AgentRole, ErrorKind, OrchestratorEndpoint, OrchestratorError, OrchestratorLocation,
    Project, ProjectName, SessionId, SurfaceId, Timestamp,
};
use vowp_store::{NodePaths, Store};

use crate::agent_exec::CoreRunHandler;
use crate::cli::{
    AgentCommand, AgentDoctorArgs, AgentEnrollArgs, AgentGatewayArgs, AgentOrchestratorArgs, AgentOrchestratorCommand,
    AgentOrchestratorEndpointArgs, AgentPairArgs, AgentPathsAddArgs, AgentPathsArgs, AgentPathsCommand,
    AgentPathsRmArgs, AgentRunArgs, AgentServiceArgs, AgentServiceCommand, AgentServiceInstallArgs,
    AgentServiceStatusArgs, AgentServiceUninstallArgs, AgentStatusArgs, AgentTrustArgs,
};
use crate::node_ctx::{self, NodeRole};
use crate::output::print_json;
use crate::{CliError, Ctx, Global, IoStreams};

/// Warning window for a local surface token before it expires.
const SURFACE_EXPIRY_WARNING: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Runs one agent subcommand.
pub fn run(global: &Global, io: &mut dyn IoStreams, command: &AgentCommand) -> Result<i32, CliError> {
    match command {
        AgentCommand::Enroll(args) => enroll(global, io, args),
        AgentCommand::Paths(args) => paths(global, io, args),
        AgentCommand::Pair(args) => pair(global, io, args),
        AgentCommand::Run(args) => run_agent(global, io, args),
        AgentCommand::Status(args) => status(global, io, args),
        AgentCommand::Service(args) => service(global, io, args),
        AgentCommand::Doctor(args) => doctor(global, io, args),
        AgentCommand::Trust(args) => trust(global, io, args),
        AgentCommand::Orchestrator(args) => orchestrator(global, io, args),
        AgentCommand::Gateway(args) => gateway(global, io, args),
    }
}

/// Runs `vowp agent orchestrator`, local inspection only.
fn orchestrator(global: &Global, io: &mut dyn IoStreams, args: &AgentOrchestratorArgs) -> Result<i32, CliError> {
    match &args.command {
        AgentOrchestratorCommand::Endpoint(args) => orchestrator_endpoint(global, io, args),
    }
}

/// Runs `vowp agent orchestrator endpoint`.
///
/// The command reads only this machine's `~/.vowp-agent`: the orchestrator's private
/// `config.json` and, for a live opencode host, its 0600 `service.json`. It never contacts
/// the home, opens a socket or prints a path, port, password, token or pair link in an
/// error. The password appears only in `--json` for a live local host.
fn orchestrator_endpoint(
    global: &Global,
    io: &mut dyn IoStreams,
    args: &AgentOrchestratorEndpointArgs,
) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let host = OrchestratorHost::new(state_dir);
    let endpoint = host.endpoint(&args.name).map_err(map_host_error)?;
    if global.json {
        print_json(io, &endpoint)?;
        return Ok(0);
    }
    print_agent_endpoint(io, &endpoint)?;
    Ok(0)
}

/// Prints one agent endpoint as a compact key/value block without its password.
fn print_agent_endpoint(io: &mut dyn IoStreams, endpoint: &OrchestratorEndpoint) -> Result<(), CliError> {
    let node = endpoint
        .node
        .as_ref()
        .map(|node| node.as_str().to_owned())
        .unwrap_or_else(|| "-".to_owned());
    writeln!(io.out(), "name: {}", endpoint.name)?;
    writeln!(io.out(), "app: {}", vowp_agent::app_name(endpoint.app))?;
    writeln!(io.out(), "session: {}", endpoint.session_id)?;
    writeln!(
        io.out(),
        "location: {}",
        match endpoint.location {
            OrchestratorLocation::Home => "home",
            OrchestratorLocation::Node => "node",
        }
    )?;
    writeln!(io.out(), "node: {node}")?;
    writeln!(io.out(), "state: {}", endpoint.state)?;
    writeln!(io.out(), "url: {}", endpoint.url.as_deref().unwrap_or("-"))?;
    writeln!(
        io.out(),
        "native session: {}",
        endpoint.native_session_id.as_deref().unwrap_or("-")
    )?;
    match &endpoint.auth {
        Some(auth) => writeln!(
            io.out(),
            "auth: {} {} via {} (password in --json)",
            auth.scheme,
            auth.username,
            auth.header
        )?,
        None => writeln!(io.out(), "auth: -")?,
    }
    Ok(())
}

/// Maps a typed local endpoint failure onto the CLI exit classes.
///
/// Only a missing local record (`not_found`) exits 3. A corrupt, too-wide or unreadable
/// private file is invalid input and exits 2, the same class as the home command's invalid
/// error for that corruption; the agent library keeps its typed `config_invalid` code.
fn map_host_error(error: OrchestratorError) -> CliError {
    match error.code.as_str() {
        "not_found" => CliError::NotFound(error.message),
        _ => CliError::Invalid(error.message),
    }
}

/// Serves one Phase 0 agent connection on stdin/stdout or a loopback WebSocket listener.
fn gateway(global: &Global, io: &mut dyn IoStreams, args: &AgentGatewayArgs) -> Result<i32, CliError> {
    match (args.stdio, args.listen.as_deref()) {
        (true, None) => gateway_stdio(global, io),
        (false, Some(listen)) => gateway_listen(global, io, listen),
        (true, Some(_)) => Err(CliError::Invalid(
            "`vowp agent gateway` accepts either --stdio or --listen, not both".to_owned(),
        )),
        (false, None) => Err(CliError::Invalid(
            "`vowp agent gateway` requires --stdio or --listen <addr>".to_owned(),
        )),
    }
}

/// Serves one agent connection over the home's stdin/stdout (the SSH forced command).
///
/// The client hard-codes `vowp agent gateway --stdio`, so the remote cwd is `$HOME`. A home
/// with zero or several projects has no single project to bind there; the global store and
/// node-level deps then serve every project, and the gateway resolves each session's or
/// assignment's project from its durable binding.
fn gateway_stdio(global: &Global, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let node = node_ctx::NodeCtx::discover(global)?;
    match node.role() {
        NodeRole::Home => {}
        NodeRole::Control => {
            return Err(CliError::Guard(
                "the gateway needs the home store; run it on the home machine".to_owned(),
            ));
        }
        NodeRole::Uninitialised => {
            return Err(CliError::NotFound(
                "vowp home is not initialised; run `vowp init`".to_owned(),
            ));
        }
    }
    let paths = node.require_home()?.clone();
    crate::ctx::require_initialised_home(&paths)?;
    let (store, deps) = gateway_context(global, io, &paths)?;
    // Frames are the only stdout output; the CLI prints any error to stderr.
    gateway::serve(&store, &deps, BufReader::new(std::io::stdin()), std::io::stdout())?;
    Ok(0)
}

/// Opens the store and dependencies one gateway invocation serves with.
///
/// An explicit `--project` or a cwd inside a registered checkout keeps the project-scoped
/// context. Everywhere else — notably `$HOME` on a home with zero or several projects —
/// the home's global store and node-level deps are used instead of failing with "no
/// project selected".
fn gateway_context(global: &Global, io: &mut dyn IoStreams, paths: &NodePaths) -> Result<(Store, Deps), CliError> {
    match Ctx::build(global, io) {
        Ok(ctx) if ctx.project_selected() => return Ok((ctx.store()?, ctx.deps().clone())),
        Ok(_) => {}
        // Without an explicit project the gateway does not need a selected checkout. A
        // broken or missing project config, or "several projects", falls back to the
        // home's global store; the per-session project store is resolved later.
        Err(_) if global.project.is_none() => {}
        Err(error) => return Err(error),
    }
    ensure_node_project(paths)?;
    Ok((Store::open_global(paths)?, node_ctx::home_deps(paths)?))
}

/// Ensures the reserved `_node` project row and database exist.
///
/// An unbound session (for example an orchestrator chat turn) writes its runs there. A
/// home created by `vowp init` already has both; a migrated or hand-built home may not.
fn ensure_node_project(paths: &NodePaths) -> Result<(), CliError> {
    let node_project = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    let store = Store::open_global(paths)?;
    if store.project(&node_project)?.is_none() {
        let now = store.clock().now();
        store.upsert_project(&Project {
            name: node_project.clone(),
            path: paths.root().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        })?;
    }
    Store::create_project_db(paths, &node_project)?;
    Ok(())
}

/// Serves one gateway connection per WebSocket on a loopback address.
///
/// The owner's TLS reverse proxy owns the public port and terminates TLS; this listener is a
/// plain `ws://127.0.0.1:<port>/agent/v1`. A non-loopback address is refused before any
/// socket is bound. Every accepted WebSocket gets its own thread and its own store handle, so
/// the handshake, enrollment, heartbeat, surface pairing and dispatch paths are exactly the
/// SSH stdio paths.
fn gateway_listen(global: &Global, io: &mut dyn IoStreams, listen: &str) -> Result<i32, CliError> {
    let address: SocketAddr = listen
        .parse()
        .map_err(|_| CliError::Invalid(format!("invalid --listen address {listen:?}; use IP:PORT")))?;
    if !address.ip().is_loopback() {
        return Err(CliError::Guard(format!(
            "the wss listener must bind a loopback address, not {}",
            address.ip()
        )));
    }
    let node = node_ctx::NodeCtx::discover(global)?;
    let paths = node.require_home()?.clone();
    crate::ctx::require_initialised_home(&paths)?;
    ensure_node_project(&paths)?;
    // Honour the node's `[ingress.wss]` limits when it has them; `--listen` overrides only
    // the address, so a one-off test or an explicit `up` flag cannot widen the deployment.
    let mut ingress_config = vowp_protocol::WssIngressConfig::default();
    let node_config = paths.root().join("config.toml");
    if node_config.is_file() {
        let text = std::fs::read_to_string(&node_config)?;
        ingress_config = vowp_protocol::NodeFile::from_toml(&text)?.ingress.wss;
    }
    ingress_config.enabled = true;
    ingress_config.listen = address.to_string();
    let ingress = WssIngress::from_config(&ingress_config).map_err(map_agent_error)?;

    let listener = WssListener::bind(&ingress).map_err(map_agent_error)?;
    let bound = listener.local_addr().map_err(map_agent_error)?;
    writeln!(io.out(), "gateway: listening on ws://{bound}/agent/v1")?;
    io.out().flush()?;

    loop {
        let connection = match listener.accept() {
            Ok(connection) => connection,
            Err(error) => {
                // A wrong path, subprotocol or upgrade is refused inside `accept`; the
                // listener keeps serving the next connection.
                writeln!(io.err(), "warning: {error}")?;
                continue;
            }
        };
        let paths = paths.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("vowp-wss-gateway-{}", connection.connection_id()))
            .spawn(move || {
                let Ok(store) = Store::open_global(&paths) else {
                    return;
                };
                let Ok(deps) = node_ctx::home_deps(&paths) else {
                    return;
                };
                let Ok((reader, writer)) = connection.split() else {
                    return;
                };
                let _ = gateway::serve(&store, &deps, reader, writer);
            });
        if let Err(error) = spawned {
            writeln!(io.err(), "warning: the wss connection thread could not start: {error}")?;
        }
    }
}

fn enroll(global: &Global, io: &mut dyn IoStreams, args: &AgentEnrollArgs) -> Result<i32, CliError> {
    if args.wss.is_none() && args.target.as_deref().is_none_or(|target| target.trim().is_empty()) {
        return Err(CliError::Invalid(
            "`vowp agent enroll` needs --target <ssh-target> or --wss <url>".to_owned(),
        ));
    }
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let path_maps = enrollment_path_maps(&state_dir, args)?;
    let code = read_code(io, &args.code_file)?;
    validate_enrollment_code(&code).map_err(map_agent_error)?;
    let key = NodeKey::load_or_generate(&state_dir).map_err(map_agent_error)?;
    let enroll_transport = enrollment_transport_config(args.wss.as_deref(), args.tls_fingerprint.as_deref())?;
    let transport_config = match &enroll_transport {
        Some(config) => config.clone(),
        None => TransportConfig::Ssh {
            target: args.target.clone().unwrap_or_default(),
        },
    };
    let transport = transport_for_config(&transport_config).map_err(map_agent_error)?;
    let mut client =
        AgentClient::unenrolled(state_dir, key, transport, Arc::new(SystemAgentClock)).with_path_maps(path_maps);
    if enroll_transport.is_some() {
        client = client.with_enroll_transport(transport_config);
    }
    let target = if enroll_transport.is_some() {
        ""
    } else {
        args.target.as_deref().unwrap_or_default()
    };
    let enrollment = client.enroll(target, &code).map_err(map_enroll_error)?;

    if global.json {
        print_json(io, &enrollment)?;
    } else {
        writeln!(
            io.out(),
            "enrolled node {} in home {} ({})",
            enrollment.node,
            enrollment.home,
            enrollment.protocol
        )?;
    }
    Ok(0)
}

/// Maps an enrollment failure onto the CLI; a WSS TLS/handshake refusal is exit 5.
///
/// Spec: `docs/design/v0.4/hardening.md` §2.3. A certificate mismatch, expired certificate
/// or wrong hostname refuses the connection before the first frame, and the agent reports it
/// as a guard (exit 5) rather than a generic remote failure.
fn map_enroll_error(error: AgentError) -> CliError {
    match &error {
        AgentError::Transport(message) if message.starts_with("wss tls:") || message.starts_with("wss handshake:") => {
            CliError::Guard(error.to_string())
        }
        _ => map_agent_error(error),
    }
}

/// Builds the transport an `vowp agent enroll` invocation stores for `--wss`.
///
/// The URL must be `wss://host[:port]/agent/v1`; a fingerprint must be
/// `sha256:<64 lowercase hex>` and selects the hard `pin` TLS mode. Both values are carried
/// into `agent.json`; no token, code or key is stored here.
pub fn enrollment_transport_config(
    wss: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<Option<TransportConfig>, CliError> {
    let Some(url) = wss else {
        if fingerprint.is_some() {
            return Err(CliError::Invalid("--tls-fingerprint needs --wss <url>".to_owned()));
        }
        return Ok(None);
    };
    let tls = match fingerprint {
        Some(fingerprint) => {
            if !is_fingerprint(fingerprint) {
                return Err(CliError::Invalid(
                    "--tls-fingerprint must be sha256:<64 lowercase hex>".to_owned(),
                ));
            }
            WssTls::Pin {
                fingerprint: fingerprint.to_owned(),
            }
        }
        None => WssTls::System { expected: None },
    };
    WssTransport::new(url, tls).map_err(map_agent_error)?;
    Ok(Some(TransportConfig::Wss {
        url: url.to_owned(),
        tls: if fingerprint.is_some() {
            WssTlsMode::Pin
        } else {
            WssTlsMode::System
        },
        fingerprint: fingerprint.map(str::to_owned),
    }))
}

/// Builds the map sent by enrollment: explicit `--project` flags win, else the saved map.
fn enrollment_path_maps(state_dir: &Path, args: &AgentEnrollArgs) -> Result<Vec<AgentPathMap>, CliError> {
    let path_maps = if args.project.is_empty() {
        if args.worktree_root.is_some() {
            return Err(CliError::Invalid(
                "--worktree-root needs at least one --project".to_owned(),
            ));
        }
        AgentConfig::load(state_dir)
            .map(|config| config.path_maps)
            .unwrap_or_default()
    } else {
        let mut path_maps = Vec::with_capacity(args.project.len());
        for project in &args.project {
            path_maps.push(path_map_from_checkout(
                Path::new(project),
                None,
                args.worktree_root.as_deref(),
                &[],
            )?);
        }
        path_maps
    };
    AgentConfig::validate_path_maps(state_dir, &path_maps).map_err(map_agent_error)?;
    Ok(path_maps)
}

/// Runs `vowp agent trust`: records a verified system-mode WSS leaf fingerprint.
///
/// The command is local-only. It never opens a connection, runs the agent or reads `node.key`,
/// a surface token or any other secret; it only loads and rewrites the private `agent.json`.
/// Spec: `docs/design/v0.4/hardening.md` §2.2.
fn trust(global: &Global, io: &mut dyn IoStreams, args: &AgentTrustArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let mut config = load_trust_config(&state_dir)?;
    let TransportConfig::Wss { tls, fingerprint, .. } = &mut config.transport else {
        return Err(CliError::Guard(
            "agent transport is ssh; trust applies to wss".to_owned(),
        ));
    };
    if *tls == WssTlsMode::Pin {
        return Err(CliError::Guard(
            "agent transport is pinned (tls = \"pin\"); a changed pin is a hard pin, so re-enroll with the new --tls-fingerprint"
                .to_owned(),
        ));
    }
    if !vowp_agent::wss::is_fingerprint(&args.fingerprint) {
        return Err(CliError::Guard(
            "--fingerprint must be sha256:<64 lowercase hex>".to_owned(),
        ));
    }
    let old = fingerprint.clone();
    if old.as_deref() == Some(args.fingerprint.as_str()) {
        if global.json {
            print_json(
                io,
                &TrustDocument {
                    trusted: true,
                    changed: false,
                    old: old.as_deref(),
                    new: &args.fingerprint,
                    tls: "system",
                },
            )?;
        } else {
            writeln!(io.out(), "fingerprint unchanged")?;
        }
        return Ok(0);
    }
    *fingerprint = Some(args.fingerprint.clone());
    config.write().map_err(map_agent_error)?;
    if global.json {
        print_json(
            io,
            &TrustDocument {
                trusted: true,
                changed: true,
                old: old.as_deref(),
                new: &args.fingerprint,
                tls: "system",
            },
        )?;
    } else {
        writeln!(io.out(), "old fingerprint: {}", old.as_deref().unwrap_or("none"))?;
        writeln!(io.out(), "new fingerprint: {}", args.fingerprint)?;
        writeln!(
            io.out(),
            "warning: verify the new fingerprint through a trusted channel (out of band, not from the same connection)"
        )?;
        if old.is_none() {
            writeln!(
                io.out(),
                "no fingerprint was recorded; this is an explicit first record, not a silent trust reset"
            )?;
        }
        writeln!(io.out(), "recorded")?;
    }
    Ok(0)
}

/// Loads `agent.json` for `trust`; a missing file is the not-enrolled guard and creates nothing.
fn load_trust_config(state_dir: &Path) -> Result<AgentConfig, CliError> {
    match load_agent_config(state_dir) {
        Ok(config) => Ok(config),
        Err(CliError::NotFound(message)) => Err(CliError::Guard(message)),
        Err(error) => Err(error),
    }
}

/// The `vowp agent trust --json` document; path-free and secret-free.
#[derive(Debug, Serialize)]
struct TrustDocument<'a> {
    trusted: bool,
    changed: bool,
    old: Option<&'a str>,
    new: &'a str,
    tls: &'static str,
}

fn paths(global: &Global, io: &mut dyn IoStreams, args: &AgentPathsArgs) -> Result<i32, CliError> {
    match &args.command {
        AgentPathsCommand::Add(args) => paths_add(global, io, args),
        AgentPathsCommand::Rm(args) => paths_rm(global, io, args),
        AgentPathsCommand::Ls(_) => paths_ls(global, io),
    }
}

fn paths_add(global: &Global, io: &mut dyn IoStreams, args: &AgentPathsAddArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let mut config = load_agent_config(&state_dir)?;
    let map = path_map_from_checkout(
        Path::new(&args.project),
        args.name.as_deref(),
        args.worktree_root.as_deref(),
        &args.allow,
    )?;
    config.set_path_map(map.clone()).map_err(map_agent_error)?;
    config.write().map_err(map_agent_error)?;
    if global.json {
        print_json(io, &map)?;
    } else {
        print_path_map(io, &map)?;
    }
    Ok(0)
}

fn paths_rm(global: &Global, io: &mut dyn IoStreams, args: &AgentPathsRmArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let mut config = load_agent_config(&state_dir)?;
    let project = ProjectName::parse(&args.project_name)
        .map_err(|_| CliError::Invalid(format!("invalid project name {:?}", args.project_name)))?;
    if !config.remove_path_map(&project) {
        return Err(CliError::NotFound(format!("project {project} is not mapped")));
    }
    config.write().map_err(map_agent_error)?;
    if global.json {
        print_json(io, &serde_json::json!({ "project": project.as_str(), "removed": true }))?;
    } else {
        writeln!(io.out(), "unmapped project {project}")?;
    }
    Ok(0)
}

fn paths_ls(global: &Global, io: &mut dyn IoStreams) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let config = load_agent_config(&state_dir)?;
    if global.json {
        print_json(io, &config.path_maps)?;
    } else if config.path_maps.is_empty() {
        writeln!(io.out(), "no projects mapped")?;
    } else {
        for map in &config.path_maps {
            print_path_map(io, map)?;
        }
    }
    Ok(0)
}

fn load_agent_config(state_dir: &Path) -> Result<AgentConfig, CliError> {
    AgentConfig::load(state_dir).map_err(|error| match error {
        AgentError::NotEnrolled(_) => CliError::NotFound(format!(
            "agent is not enrolled under {}; run `vowp agent enroll --target <ssh-target> --code-file -`",
            state_dir.display()
        )),
        other => map_agent_error(other),
    })
}

/// Builds one validated map from a checkout and the CLI flags.
fn path_map_from_checkout(
    project: &Path,
    name: Option<&str>,
    worktree_root: Option<&Path>,
    allow: &[String],
) -> Result<AgentPathMap, CliError> {
    let local_root = std::fs::canonicalize(project).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::NotFound(format!("project checkout {}", project.display()))
        } else {
            CliError::Io(error)
        }
    })?;
    if !local_root.is_dir() {
        return Err(CliError::Invalid(format!(
            "project checkout {} is not a directory",
            local_root.display()
        )));
    }
    let project_name = match name {
        Some(name) => {
            ProjectName::parse(name).map_err(|_| CliError::Invalid(format!("invalid project name {name:?}")))?
        }
        None => project_name_for(&local_root)?,
    };
    let worktree_root = match worktree_root {
        Some(root) if root.is_absolute() => root.to_path_buf(),
        Some(root) => std::env::current_dir().map_err(CliError::Io)?.join(root),
        None => local_root.join(vowp_agent::DEFAULT_WORKTREE_DIR),
    };
    std::fs::create_dir_all(&worktree_root).map_err(CliError::Io)?;
    let worktree_root = std::fs::canonicalize(&worktree_root).map_err(CliError::Io)?;
    let local_root = local_root
        .to_str()
        .ok_or_else(|| CliError::Invalid("project checkout must be a UTF-8 path".to_owned()))?
        .to_owned();
    let worktree_root = worktree_root
        .to_str()
        .ok_or_else(|| CliError::Invalid("worktree root must be a UTF-8 path".to_owned()))?
        .to_owned();
    Ok(AgentPathMap {
        project: project_name,
        local_root,
        worktree_root: Some(worktree_root),
        allow: allow.to_vec(),
    })
}

/// Reads `[project].name` from a checkout's `vowp.toml`, else uses the directory name.
fn project_name_for(local_root: &Path) -> Result<ProjectName, CliError> {
    let config_path = local_root.join("vowp.toml");
    if config_path.is_file() {
        let text = std::fs::read_to_string(&config_path).map_err(CliError::Io)?;
        let value: toml::Value = toml::from_str(&text)
            .map_err(|error| CliError::Invalid(format!("invalid {}: {error}", config_path.display())))?;
        if let Some(name) = value
            .get("project")
            .and_then(|project| project.get("name"))
            .and_then(toml::Value::as_str)
        {
            return ProjectName::parse(name)
                .map_err(|_| CliError::Invalid(format!("invalid project name {name:?} in {}", config_path.display())));
        }
    }
    let name = local_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::Invalid(format!("cannot derive a project name from {}", local_root.display())))?;
    ProjectName::parse(name).map_err(|_| {
        CliError::Invalid(format!(
            "cannot derive a project name from {}; pass --name",
            local_root.display()
        ))
    })
}

fn print_path_map(io: &mut dyn IoStreams, map: &AgentPathMap) -> Result<(), CliError> {
    write!(io.out(), "{} {}", map.project, map.local_root)?;
    if let Some(root) = &map.worktree_root {
        write!(io.out(), " worktrees {root}")?;
    }
    if !map.allow.is_empty() {
        write!(io.out(), " allow {}", map.allow.join(","))?;
    }
    writeln!(io.out())?;
    Ok(())
}

fn pair(global: &Global, io: &mut dyn IoStreams, args: &AgentPairArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let code = read_code(io, &args.code_file)?;
    let session = SessionId::parse(&args.session)
        .map_err(|_| CliError::Invalid(format!("invalid session id {:?}", args.session)))?;
    let surface_id = SurfaceId(args.surface.clone());
    let app = args.app.clone().unwrap_or_else(|| "agent".to_owned());
    let request = SurfacePairRequest::new(code, surface_id, session, app);

    // A running `vowp agent run` owns the one node connection. Opening a second direct
    // connection with the same node key would supersede it, so prefer its local socket.
    let paired = if let Some(paired) = pair_through_agent_socket(&state_dir, &request)? {
        paired
    } else {
        let mut client = AgentClient::from_state_dir(state_dir, Arc::new(SystemAgentClock)).map_err(map_agent_error)?;
        client.pair(&request).map_err(map_agent_error)?
    };
    if global.json {
        print_json(io, &paired)?;
    } else {
        writeln!(
            io.out(),
            "paired surface {}; token stored at {}; expires {}",
            paired.surface_id,
            paired.token_path.display(),
            paired.expires_at.as_rfc3339()
        )?;
    }
    Ok(0)
}

/// Tries the pairing over `<state_dir>/agent.sock`.
///
/// `Ok(None)` means the caller should fall back to one direct agent connection: the socket
/// is missing or not accepting connections, or the running agent reports `home_offline`.
/// A pairing refusal from a live agent is returned as the CLI error instead of retrying
/// through a second connection that would supersede it.
fn pair_through_agent_socket(
    state_dir: &Path,
    request: &SurfacePairRequest,
) -> Result<Option<PairedSurface>, CliError> {
    let response = match vowp_agent::socket::pair_over_socket(state_dir, request).map_err(map_agent_error)? {
        Some(response) => response,
        None => return Ok(None),
    };
    match response {
        vowp_agent::socket::PairReply::Paired {
            surface_id,
            scopes,
            expires_at,
            token_path,
        } => Ok(Some(PairedSurface {
            surface_id,
            scopes,
            expires_at,
            token_path,
        })),
        vowp_agent::socket::PairReply::Error { code, message: _ } if code == "home_offline" => Ok(None),
        vowp_agent::socket::PairReply::Error { code, message } => Err(match code.as_str() {
            "invalid_request" => CliError::Invalid(message),
            "pair_timeout" => CliError::Remote(message),
            "surface_revoked" | "pair_failed" => CliError::Guard(message),
            _ => CliError::Remote(message),
        }),
    }
}

fn run_agent(global: &Global, io: &mut dyn IoStreams, args: &AgentRunArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    check_run_isolation(&state_dir)?;
    let config = AgentConfig::load(&state_dir).map_err(map_agent_error)?;
    let key = NodeKey::load(&state_dir).map_err(map_agent_error)?;
    let handler = Arc::new(CoreRunHandler::new(config.clone()));
    let transport = transport_for_config(config.transport()).map_err(map_agent_error)?;
    let mut client = AgentClient::new(state_dir, key, config, transport, Arc::new(SystemAgentClock));
    let (role, role_name) = parse_role(args.role.as_deref())?;
    if !global.quiet {
        let mode = if args.service {
            "as a service"
        } else {
            "in the terminal"
        };
        writeln!(
            io.err(),
            "agent running {mode} as {role_name}; press Ctrl-C or send SIGTERM to stop"
        )?;
    }
    let options = RunOptions {
        role,
        foreground: args.foreground || args.service,
    };
    if args.service {
        if ServicePlatform::current().is_none() {
            return Err(CliError::Guard(
                "agent service is supported on Linux and macOS only".to_owned(),
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        install_stop_flag(&stop)?;
        match client
            .run_as_service(&options, handler, stop)
            .map_err(map_agent_error)?
        {
            ServiceRunOutcome::Drained => {
                if !global.quiet {
                    writeln!(io.err(), "agent drained; the service may start it again")?;
                }
                Ok(0)
            }
            ServiceRunOutcome::Terminal(RunOutcome::Revoked { reason }) => {
                Err(CliError::Guard(format!("agent revoked: {reason}")))
            }
            ServiceRunOutcome::Terminal(RunOutcome::Superseded { connection_id }) => {
                Err(CliError::Conflict(format!("agent superseded by {connection_id}")))
            }
        }
    } else {
        match client.run_with_handler(&options, handler).map_err(map_agent_error)? {
            RunOutcome::Revoked { reason } => Err(CliError::Guard(format!("agent revoked: {reason}"))),
            RunOutcome::Superseded { connection_id } => {
                Err(CliError::Conflict(format!("agent superseded by {connection_id}")))
            }
        }
    }
}

/// Checks the state directory before the library's run loop repeats the check.
///
/// The library reports an isolation refusal as [`AgentError::Config`], which the generic
/// mapping turns into exit 2. Checking here first lets the CLI use
/// [`IsolationError::exit_code`], so a loose or foreign state directory and a root process are
/// refusals with exit 5, as `hardening.md` §6.2 requires.
fn check_run_isolation(state_dir: &Path) -> Result<(), CliError> {
    let uid = current_uid().map_err(map_isolation_error)?;
    ensure_not_root(uid).map_err(map_isolation_error)?;
    ensure_state_dir(state_dir, uid).map_err(map_isolation_error)?;
    Ok(())
}

/// Maps an isolation refusal onto the CLI error whose exit code is the documented one.
fn map_isolation_error(error: IsolationError) -> CliError {
    let message = error.to_string();
    match error.exit_code() {
        2 => CliError::Invalid(message),
        5 => CliError::Guard(message),
        _ => CliError::Io(std::io::Error::other(message)),
    }
}

/// Registers SIGTERM and SIGINT to set the service run's stop flag.
///
/// The library installs no signal handler itself; a service manager sends SIGTERM and the
/// agent drains the spool and connected surfaces before exiting 0.
fn install_stop_flag(stop: &Arc<AtomicBool>) -> Result<(), CliError> {
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(stop)).map_err(CliError::Io)?;
    }
    Ok(())
}

fn status(global: &Global, io: &mut dyn IoStreams, _args: &AgentStatusArgs) -> Result<i32, CliError> {
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let config = AgentConfig::load(&state_dir).map_err(|error| match error {
        AgentError::NotEnrolled(_) => CliError::NotFound(format!(
            "agent is not enrolled under {}; run `vowp agent enroll --target <ssh-target> --code-file -`",
            state_dir.display()
        )),
        other => map_agent_error(other),
    })?;

    let mut status = AgentStatus::load(&state_dir)
        .map_err(map_agent_error)?
        .unwrap_or_default();
    if status.connection == ConnectionState::Unknown {
        status.connection = ConnectionState::Stopped;
    }
    if status.node.is_none() {
        status.node = Some(config.node.clone());
    }
    if status.home.is_none() {
        status.home = Some(config.home.clone());
    }
    if status.protocol.is_none() {
        status.protocol = Some(AGENT_PROTOCOL.to_owned());
    }
    if status.key_fingerprint.is_none() {
        status.key_fingerprint = load_public(&state_dir).ok().map(|key| fingerprint_of(&key));
    }
    status.pending_spool_bytes = pending_spool_bytes(&state_dir);
    status.refresh_cache(&state_dir);
    status.surface_token_expires_at = surface_token_expiry(&state_dir);
    let surfaces = status.surfaces(&state_dir);

    if global.json {
        print_json(
            io,
            &StatusDocument {
                status: &status,
                surfaces: &surfaces,
                paths: &config.path_maps,
            },
        )?;
    } else {
        print_status(io, &status)?;
        print_surfaces(io, &surfaces)?;
        if !config.path_maps.is_empty() {
            writeln!(io.out(), "paths:")?;
            for map in &config.path_maps {
                write!(io.out(), "  ")?;
                print_path_map(io, map)?;
            }
        }
    }
    warn_expiring_surfaces(io, &surfaces, Timestamp::now())?;
    Ok(0)
}

/// Prints the local surfaces array.
fn print_surfaces(io: &mut dyn IoStreams, surfaces: &[SurfaceStatus]) -> Result<(), CliError> {
    if surfaces.is_empty() {
        return Ok(());
    }
    writeln!(io.out(), "surfaces:")?;
    for surface in surfaces {
        writeln!(
            io.out(),
            "  {} {} expires {} {}",
            surface.surface,
            surface.prefix.as_deref().unwrap_or("-"),
            surface
                .expires_at
                .map(|expiry| expiry.as_rfc3339())
                .unwrap_or_else(|| "-".to_owned()),
            surface_state_name(surface)
        )?;
    }
    Ok(())
}

/// Returns the local surface state's wire name.
fn surface_state_name(surface: &SurfaceStatus) -> &'static str {
    match surface.state {
        vowp_agent::SurfaceState::Paired => "paired",
        vowp_agent::SurfaceState::NeedsPair => "needs_pair",
        vowp_agent::SurfaceState::Revoked => "revoked",
    }
}

/// Returns whether a surface token expires within the seven-day warning window.
fn expires_within_warning(surface: &SurfaceStatus, now: Timestamp) -> bool {
    surface
        .expires_at
        .is_some_and(|expiry| expiry > now && expiry.since(now) <= SURFACE_EXPIRY_WARNING)
}

/// Prints one warning line when any surface token expires within seven days.
fn warn_expiring_surfaces(io: &mut dyn IoStreams, surfaces: &[SurfaceStatus], now: Timestamp) -> Result<(), CliError> {
    let expiring: Vec<&SurfaceStatus> = surfaces
        .iter()
        .filter(|surface| expires_within_warning(surface, now))
        .collect();
    if expiring.is_empty() {
        return Ok(());
    }
    let expiry = expiring
        .iter()
        .filter_map(|surface| surface.expires_at)
        .min()
        .map(|expiry| expiry.as_rfc3339())
        .unwrap_or_else(|| "-".to_owned());
    writeln!(
        io.err(),
        "warning: {} surface token(s) expire within 7 days (earliest {expiry}); run `vowp surface rotate <id>`",
        expiring.len()
    )?;
    Ok(())
}

#[derive(Serialize)]
struct StatusDocument<'a> {
    #[serde(flatten)]
    status: &'a AgentStatus,
    /// Local surface ids, display prefixes, expiries and states; never a token.
    surfaces: &'a [SurfaceStatus],
    /// Absolute roots, printed only by this owner-only command (never by `status.json`).
    paths: &'a [AgentPathMap],
}

fn print_status(io: &mut dyn IoStreams, status: &AgentStatus) -> Result<(), CliError> {
    writeln!(io.out(), "connection: {}", connection_name(status.connection))?;
    writeln!(
        io.out(),
        "node: {}",
        status
            .node
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(io.out(), "home: {}", status.home.as_deref().unwrap_or("-"))?;
    writeln!(io.out(), "protocol: {}", status.protocol.as_deref().unwrap_or("-"))?;
    writeln!(
        io.out(),
        "last heartbeat: {}",
        status
            .last_heartbeat
            .map(|at| at.as_rfc3339())
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(io.out(), "pending spool: {} bytes", status.pending_spool_bytes)?;
    writeln!(
        io.out(),
        "cache: {} entries, {} bytes",
        status.cache_entries,
        status.cache_bytes
    )?;
    writeln!(
        io.out(),
        "cache oldest: {}",
        status
            .cache_oldest_at
            .map(|at| at.as_rfc3339())
            .unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(io.out(), "key: {}", status.key_fingerprint.as_deref().unwrap_or("-"))?;
    if let Some(expiry) = status.surface_token_expires_at {
        writeln!(io.out(), "surface token expires: {}", expiry.as_rfc3339())?;
    }
    if let Some(error) = &status.last_error {
        writeln!(io.out(), "last error: {error}")?;
    }
    Ok(())
}

/// Runs one `vowp agent service` subcommand.
fn service(global: &Global, io: &mut dyn IoStreams, args: &AgentServiceArgs) -> Result<i32, CliError> {
    match &args.command {
        AgentServiceCommand::Install(args) => service_install(global, io, args),
        AgentServiceCommand::Uninstall(args) => service_uninstall(global, io, args),
        AgentServiceCommand::Status(args) => service_status(global, io, args),
    }
}

/// Builds the current user's service specification.
fn current_user_service_spec() -> Result<ServiceSpec, CliError> {
    let binary = std::env::current_exe()?;
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    ServiceSpec::for_current_user(binary, state_dir).map_err(map_service_error)
}

/// Runs `agent service install [--print] [--force] [--now]`.
fn service_install(global: &Global, io: &mut dyn IoStreams, args: &AgentServiceInstallArgs) -> Result<i32, CliError> {
    let spec = current_user_service_spec()?;
    let report = service::install(
        &spec,
        &InstallOptions {
            print_only: args.print,
            force: args.force,
            now: args.now,
        },
        &SystemManager,
    )
    .map_err(map_service_error)?;
    if global.json {
        print_json(
            io,
            &ServiceInstallOutput {
                installed: report.installed,
                unit: spec.platform.unit_name(),
                run_marker: report.run_marker.is_some(),
                linger: report.linger_hint.clone(),
            },
        )?;
        return Ok(0);
    }
    if args.print {
        write!(io.out(), "{}", report.text)?;
    }
    writeln!(io.out(), "unit: {}", report.unit_path.display())?;
    if report.installed {
        writeln!(
            io.out(),
            "installed: yes{}",
            if args.now { " (loaded now)" } else { "" }
        )?;
    } else {
        writeln!(io.out(), "installed: no (--print writes nothing)")?;
    }
    if let Some(marker) = &report.run_marker {
        writeln!(io.out(), "run marker: {}", marker.display())?;
    }
    if !report.commands.is_empty() {
        writeln!(io.out(), "commands:")?;
        for command in &report.commands {
            writeln!(io.out(), "  {}", command.join(" "))?;
        }
    }
    if let Some(hint) = &report.linger_hint {
        writeln!(io.out(), "{hint}")?;
    }
    Ok(0)
}

/// The JSON shape of `vowp agent service install`.
#[derive(Debug, Serialize)]
struct ServiceInstallOutput {
    installed: bool,
    unit: &'static str,
    run_marker: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    linger: Option<String>,
}

/// Runs `agent service uninstall [--yes]`.
fn service_uninstall(
    global: &Global,
    io: &mut dyn IoStreams,
    args: &AgentServiceUninstallArgs,
) -> Result<i32, CliError> {
    if !args.yes {
        return Err(CliError::Guard(
            "refusing to uninstall the agent service without --yes".to_owned(),
        ));
    }
    let spec = current_user_service_spec()?;
    let report = service::uninstall(&spec, &SystemManager).map_err(map_service_error)?;
    if global.json {
        print_json(
            io,
            &ServiceUninstallOutput {
                removed: report.removed,
                unit: spec.platform.unit_name(),
            },
        )?;
    } else if report.removed {
        writeln!(
            io.out(),
            "removed unit {}; the agent state and every token are kept",
            spec.platform.unit_name()
        )?;
    } else {
        writeln!(
            io.out(),
            "no VOWP-generated unit {} at {}; nothing removed",
            spec.platform.unit_name(),
            report.unit_path.display()
        )?;
    }
    Ok(0)
}

/// The JSON shape of `vowp agent service uninstall`.
#[derive(Debug, Serialize)]
struct ServiceUninstallOutput {
    removed: bool,
    unit: &'static str,
}

/// Runs `agent service status [--json]`.
fn service_status(global: &Global, io: &mut dyn IoStreams, _args: &AgentServiceStatusArgs) -> Result<i32, CliError> {
    let spec = current_user_service_spec()?;
    let status = service::status(&spec, &SystemManager).map_err(map_service_error)?;
    let state = if !status.available {
        "unavailable"
    } else if status.loaded == Some(true) {
        "loaded"
    } else {
        "not_loaded"
    };
    if global.json {
        print_json(
            io,
            &ServiceStatusOutput {
                available: status.available,
                unit: spec.platform.unit_name(),
                state,
                loaded: status.loaded,
                pid: status.pid,
                last_exit: status.last_exit,
            },
        )?;
        return Ok(0);
    }
    writeln!(io.out(), "unit: {}", status.unit_path.display())?;
    if !status.available {
        writeln!(io.out(), "status: unavailable")?;
        return Ok(0);
    }
    writeln!(
        io.out(),
        "loaded: {}",
        match status.loaded {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    )?;
    writeln!(
        io.out(),
        "pid: {}",
        status.pid.map(|pid| pid.to_string()).unwrap_or_else(|| "-".to_owned())
    )?;
    writeln!(
        io.out(),
        "last exit: {}",
        status
            .last_exit
            .map(|code| code.to_string())
            .unwrap_or_else(|| "-".to_owned())
    )?;
    Ok(0)
}

/// The JSON shape of `vowp agent service status`; path-free and token-free.
#[derive(Debug, Serialize)]
struct ServiceStatusOutput {
    available: bool,
    unit: &'static str,
    state: &'static str,
    loaded: Option<bool>,
    pid: Option<u32>,
    last_exit: Option<i32>,
}

/// Maps a service refusal onto the CLI error whose exit code is the documented one.
///
/// The library owns the exit-code table; this keeps the CLI's error kind in step with it, so
/// an unsupported platform is exit 5, a foreign unit is exit 4, a missing `HOME` is exit 2
/// and a failed manager command is exit 7.
pub fn map_service_error(error: ServiceError) -> CliError {
    let message = error.to_string();
    match error.exit_code() {
        2 => CliError::Invalid(message),
        4 => CliError::Conflict(message),
        5 => CliError::Guard(message),
        7 => CliError::Remote(message),
        _ => CliError::Io(std::io::Error::other(message)),
    }
}

/// Returns the documented exit code for one service refusal.
///
/// Exposed so the integration tests can prove the mapping the CLI uses for a platform this
/// build cannot select at compile time.
pub fn service_error_exit_code(error: &ServiceError) -> i32 {
    error.exit_code()
}

/// Runs `vowp agent doctor`: local warnings, never failures.
fn doctor(global: &Global, io: &mut dyn IoStreams, _args: &AgentDoctorArgs) -> Result<i32, CliError> {
    let mut checks = AgentDoctorChecks::default();
    match AgentConfig::default_state_dir() {
        Ok(state_dir) => {
            check_agent_state_dir(&state_dir, &mut checks);
            check_agent_service(&state_dir, &mut checks);
            check_agent_surfaces(&state_dir, &mut checks);
            check_copied_logins(&state_dir, &mut checks);
        }
        Err(AgentError::Config(_)) => checks.warn(
            "state_dir",
            "HOME is not set; the agent state and service cannot be checked",
        ),
        Err(error) => checks.warn("state_dir", error.to_string()),
    }
    let report = AgentDoctorReport {
        ok: checks.healthy(),
        checks: checks.items,
    };
    if global.json {
        print_json(io, &report)?;
    } else {
        for check in &report.checks {
            writeln!(io.out(), "{:<4} {:<12} {}", check.status, check.name, check.detail)?;
        }
        let warned = report.checks.iter().any(|check| check.status == "warn");
        writeln!(
            io.out(),
            "agent doctor: {}",
            if !report.ok {
                "failures found"
            } else if warned {
                "ok (warnings)"
            } else {
                "ok"
            }
        )?;
    }
    Ok(if report.ok { 0 } else { 1 })
}

/// One `vowp agent doctor` check.
#[derive(Debug, Serialize)]
struct AgentDoctorCheck {
    name: String,
    status: &'static str,
    detail: String,
}

/// The `vowp agent doctor --json` document.
#[derive(Debug, Serialize)]
struct AgentDoctorReport {
    ok: bool,
    checks: Vec<AgentDoctorCheck>,
}

/// Accumulates agent doctor warnings.
#[derive(Default)]
struct AgentDoctorChecks {
    items: Vec<AgentDoctorCheck>,
}

impl AgentDoctorChecks {
    /// Records one warning.
    fn warn(&mut self, name: &str, detail: impl Into<String>) {
        self.items.push(AgentDoctorCheck {
            name: name.to_owned(),
            status: "warn",
            detail: detail.into(),
        });
    }

    /// Records one healthy check.
    fn ok(&mut self, name: &str, detail: impl Into<String>) {
        self.items.push(AgentDoctorCheck {
            name: name.to_owned(),
            status: "ok",
            detail: detail.into(),
        });
    }

    /// Returns true when no check failed; agent doctor records warnings only.
    fn healthy(&self) -> bool {
        !self.items.iter().any(|check| check.status == "fail")
    }
}

/// Checks the state directory's ownership and mode without printing its path.
fn check_agent_state_dir(state_dir: &Path, checks: &mut AgentDoctorChecks) {
    if !state_dir.is_dir() {
        checks.warn(
            "state_dir",
            "the agent state directory is missing; run `vowp agent enroll`",
        );
        return;
    }
    match current_uid().and_then(|uid| check_state_dir(state_dir, uid)) {
        Ok(()) => checks.ok("state_dir", "the state directory is private and owned by this user"),
        Err(error) => checks.warn("state_dir", isolation_detail(&error)),
    }
}

/// Checks the service unit, the manager's loaded state and the run marker.
fn check_agent_service(state_dir: &Path, checks: &mut AgentDoctorChecks) {
    let binary = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vowp"));
    let spec = match ServiceSpec::for_current_user(binary, state_dir.to_path_buf()) {
        Ok(spec) => spec,
        Err(ServiceError::UnsupportedPlatform) => {
            checks.warn("service", "agent service is supported on Linux and macOS only");
            return;
        }
        Err(ServiceError::MissingHome) => {
            checks.warn("service", "HOME is not set; the service unit cannot be checked");
            return;
        }
        Err(error) => {
            checks.warn("service", service_error_detail(&error));
            return;
        }
    };
    let unit_name = spec.platform.unit_name();
    let unit_present = spec.unit_path().map(|path| path.is_file()).unwrap_or(false);
    let marker_present = run_marker_path(state_dir).is_file();
    let mut problems = Vec::new();
    let status = if unit_present {
        match service::status(&spec, &SystemManager) {
            Ok(status) => Some(status),
            Err(error) => {
                problems.push(service_error_detail(&error));
                None
            }
        }
    } else {
        None
    };
    if !unit_present {
        problems.push(format!(
            "the service unit {unit_name} is not installed; run `vowp agent service install`"
        ));
    } else if let Some(status) = &status {
        if !status.available {
            problems.push(format!(
                "the service manager is unavailable; {unit_name} is installed but its state cannot be read"
            ));
        } else if status.loaded != Some(true) {
            problems.push(format!(
                "the {unit_name} unit is not loaded; run `vowp agent service status`"
            ));
        }
    }
    match spec.platform {
        ServicePlatform::Launchd => {
            if unit_present && !marker_present {
                problems.push("the launchd run marker is missing; reinstall the service".to_owned());
            }
            if !unit_present && marker_present {
                problems.push("a leftover run marker exists without a service unit".to_owned());
            }
        }
        ServicePlatform::SystemdUser => {
            let running = status
                .as_ref()
                .is_some_and(|status| status.loaded == Some(true) && status.pid.is_some());
            if marker_present && !running {
                problems.push("a leftover run marker exists; the systemd user unit does not use one".to_owned());
            }
        }
    }
    if problems.is_empty() {
        checks.ok("service", format!("the {unit_name} unit is loaded"));
    } else {
        checks.warn("service", problems.join("; "));
    }
}

/// Checks local surfaces for `needs_pair` or an expired token.
fn check_agent_surfaces(state_dir: &Path, checks: &mut AgentDoctorChecks) {
    let surfaces = surface_statuses(state_dir);
    if surfaces.is_empty() {
        checks.ok("surfaces", "no local surfaces are paired");
        return;
    }
    let now = Timestamp::now();
    let mut problems = Vec::new();
    for surface in &surfaces {
        match surface.state {
            vowp_agent::SurfaceState::NeedsPair => problems.push(format!(
                "surface {} needs pairing; run `vowp surface revoke {}` then `vowp surface pair` and `vowp agent pair`",
                surface.surface, surface.surface
            )),
            vowp_agent::SurfaceState::Paired => {
                if surface.expires_at.is_some_and(|expiry| expiry <= now) {
                    let expiry = surface
                        .expires_at
                        .map(|expiry| expiry.as_rfc3339())
                        .unwrap_or_else(|| "-".to_owned());
                    problems.push(format!(
                        "surface {} token expired at {expiry}; rotate or re-pair",
                        surface.surface
                    ));
                } else if expires_within_warning(surface, now) {
                    let expiry = surface
                        .expires_at
                        .map(|expiry| expiry.as_rfc3339())
                        .unwrap_or_else(|| "-".to_owned());
                    problems.push(format!(
                        "surface {} token expires within 7 days at {expiry}; run `vowp surface rotate {}`",
                        surface.surface, surface.surface
                    ));
                }
            }
            vowp_agent::SurfaceState::Revoked => {}
        }
    }
    if problems.is_empty() {
        checks.ok("surfaces", format!("{} local surface(s) are paired", surfaces.len()));
    } else {
        checks.warn("surfaces", problems.join("; "));
    }
}

/// Reports copied login material by name only; file contents are never read.
fn check_copied_logins(state_dir: &Path, checks: &mut AgentDoctorChecks) {
    let mut found = Vec::new();
    for name in [".ssh", ".claude", ".env", ".git-credentials", "auth.json"] {
        if state_dir.join(name).exists() {
            found.push(name.to_owned());
        }
    }
    for relative in [".config/opencode/auth.json", ".local/share/opencode/auth.json"] {
        if state_dir.join(relative).is_file() {
            found.push(relative.to_owned());
        }
    }
    if found.is_empty() {
        checks.ok("logins", "no copied login files under the state directory");
    } else {
        checks.warn(
            "logins",
            format!(
                "found {} under the state directory; the agent user must use its own login",
                found.join(", ")
            ),
        );
    }
}

/// Returns a path-free isolation message for the doctor.
fn isolation_detail(error: &vowp_agent::IsolationError) -> String {
    use vowp_agent::IsolationError;
    match error {
        IsolationError::ForeignOwner { expected, found, .. } => {
            format!("the state directory is owned by uid {found}; expected uid {expected}")
        }
        IsolationError::LooseMode { expected, mode, .. } => {
            format!("the state directory is mode {mode:03o}; it must be {expected:03o} with no group or world access")
        }
        IsolationError::LooseFile { expected, mode, .. } => {
            format!("a private state file is mode {mode:03o}; it must be {expected:03o}")
        }
        IsolationError::Symlink { .. } => {
            "the state directory is a symbolic link; it must be a real directory".to_owned()
        }
        IsolationError::NotADirectory { .. } => "the agent state path is not a directory".to_owned(),
        IsolationError::Missing(_) => "the agent state directory is missing".to_owned(),
        IsolationError::Root => "the agent must not run as root; use a standard user account".to_owned(),
        IsolationError::Io(error) => format!("io: {error}"),
        IsolationError::Config(message) => message.clone(),
    }
}

/// Returns a path-free service message for the doctor.
fn service_error_detail(error: &ServiceError) -> String {
    match error {
        ServiceError::UnsupportedPlatform | ServiceError::MissingHome => error.to_string(),
        ServiceError::ForeignUnit { .. } => {
            "the service unit path holds a file VOWP did not generate; run `vowp agent service install --force`"
                .to_owned()
        }
        ServiceError::ManagerMissing { command } => {
            format!("the service manager command is not available: {command}")
        }
        ServiceError::ManagerFailed { command, .. } => format!("{command} failed"),
        ServiceError::Isolation(error) => isolation_detail(error),
        ServiceError::Config(message) => message.clone(),
        ServiceError::Io(_) => "the service unit could not be read".to_owned(),
    }
}

fn read_code(io: &mut dyn IoStreams, code_file: &str) -> Result<String, CliError> {
    let text = if code_file == "-" {
        crate::read_stdin(io)?
    } else {
        std::fs::read_to_string(Path::new(code_file)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CliError::NotFound(format!("code file {code_file}"))
            } else {
                CliError::Io(error)
            }
        })?
    };
    Ok(text.trim().to_owned())
}

fn parse_role(value: Option<&str>) -> Result<(AgentRole, &'static str), CliError> {
    match value.unwrap_or("both") {
        "surface" => Ok((AgentRole::Surface, "surface")),
        "executor" => Ok((AgentRole::Executor, "executor")),
        "both" => Ok((AgentRole::Both, "both")),
        other => Err(CliError::Invalid(format!(
            "invalid role {other:?}; use surface, executor or both"
        ))),
    }
}

fn connection_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Unknown => "unknown",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Revoked => "revoked",
        ConnectionState::Superseded => "superseded",
        ConnectionState::Stopped => "stopped",
    }
}

fn map_agent_error(error: AgentError) -> CliError {
    match error {
        AgentError::NotEnrolled(message) => CliError::NotFound(message),
        AgentError::Revoked(reason) => CliError::Guard(format!("agent revoked: {reason}")),
        AgentError::Superseded(connection_id) => CliError::Conflict(format!("agent superseded by {connection_id}")),
        AgentError::Auth(message) => CliError::Remote(format!("authentication rejected: {message}")),
        AgentError::Remote { kind, message, .. } => match kind {
            ErrorKind::Refused => CliError::Guard(message),
            ErrorKind::InvalidInput => CliError::Invalid(message),
            ErrorKind::Conflict => CliError::Conflict(message),
            ErrorKind::NotFound => CliError::NotFound(message),
            ErrorKind::Limit => CliError::Limited(message),
            other => CliError::Remote(format!("home error ({other:?}): {message}")),
        },
        AgentError::Transport(message) => CliError::Remote(message),
        AgentError::Io(error) => CliError::Io(error),
        AgentError::ReplayNonce => CliError::Guard("the home replayed a handshake nonce".to_owned()),
        AgentError::FrameTooLarge { .. } | AgentError::Protocol(_) | AgentError::Json(_) => {
            CliError::Remote(error.to_string())
        }
        AgentError::Config(_) | AgentError::Key(_) => CliError::Invalid(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wss_tls_refusals_are_guards_and_transport_failures_stay_remote() {
        let tls = map_enroll_error(AgentError::Transport(
            "wss tls: the pinned certificate fingerprint changed".to_owned(),
        ));
        assert_eq!(tls.exit_code(), 5);

        let handshake = map_enroll_error(AgentError::Transport(
            "wss handshake: the wss server did not select vowp-agent/1".to_owned(),
        ));
        assert_eq!(handshake.exit_code(), 5);

        let refused = map_enroll_error(AgentError::Transport(
            "wss connect home.example:443: connection refused".to_owned(),
        ));
        assert_eq!(refused.exit_code(), 7);
    }
}
