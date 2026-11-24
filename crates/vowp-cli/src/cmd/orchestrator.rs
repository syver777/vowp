//! `vowp orchestrator` — orchestrator sessions and app hosts.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.6 and §12.3. The core manager owns
//! the files and hosts; this handler validates the app, project and executor names and
//! renders the card or the join instructions.

use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use vowp_adapters::proc::{Signal, process_alive, signal_group};
use vowp_core::CoreError;
use vowp_core::surface::orchestrator::{KEEPER_PID_FILE, NewOrchestrator, Orchestrator, OrchestratorInfo};
use vowp_protocol::{
    Executor, ExecutorRef, OrchestratorApp, OrchestratorAttach, OrchestratorCard, OrchestratorEndpoint,
    OrchestratorLocation, ProjectName, SurfaceId, VowpConfig,
};
use vowp_store::NodePaths;

use crate::output::print_json;
use crate::{CliError, Ctx, IoStreams};

/// The keeper's private log file.
const KEEPER_LOG: &str = "keeper.log";

/// How long a new keeper has to publish its pid file.
const KEEPER_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Arguments for `vowp orchestrator`.
#[derive(Debug, Clone, Args)]
pub struct OrchestratorArgs {
    /// The orchestrator subcommand.
    #[command(subcommand)]
    pub command: OrchestratorCommand,
}

/// The `vowp orchestrator` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum OrchestratorCommand {
    /// Create and start an orchestrator.
    New(OrchestratorNewArgs),
    /// List orchestrators.
    Ls,
    /// Move an orchestrator between the home and an agent node, keeping its session.
    #[command(
        override_usage = "vowp orchestrator move <NAME> (--node <NAME> | --home)",
        after_help = "For `orchestrator move`, --node <name> names the target node that runs the orchestrator, not an SSH redirect; use --home to move it back to this home. Exactly one of --node or --home is required."
    )]
    Move(OrchestratorMoveArgs),
    /// Stop an orchestrator's host and release its lease.
    Stop(OrchestratorNameArgs),
    /// Start an orchestrator's host again.
    Resume(OrchestratorNameArgs),
    /// Show the typed discovery document a local UI client uses to connect.
    #[command(
        after_help = "For `orchestrator endpoint`, --node is not an SSH redirect: the command inspects this home and never opens SSH or a node's files."
    )]
    Endpoint(OrchestratorEndpointArgs),
    /// Attach a desktop UI as the session owner, then hold the lease until stdin closes.
    #[command(
        after_help = "Prints the one-time grant, then heartbeats the lease until stdin reaches end-of-file and releases it. Keep this process (as a child of the UI) alive while the UI writes. Recorded ownership is cooperative: it does not stop a UI that calls the app's HTTP API directly. --node is not an SSH redirect; attach works only on a home-hosted orchestrator."
    )]
    Attach(OrchestratorAttachArgs),
    /// Hidden: keep one orchestrator's lease alive after the CLI exits.
    #[command(name = "__keepalive", hide = true)]
    KeepAlive(OrchestratorNameArgs),
}

/// Arguments for `vowp orchestrator endpoint`.
#[derive(Debug, Clone, Args)]
pub struct OrchestratorEndpointArgs {
    /// Orchestrator name.
    pub name: String,
    /// The caller's surface id; `access: owner` is reported only for the live owner surface.
    #[arg(long = "surface", value_name = "ID")]
    pub surface: Option<String>,
}

/// Arguments for `vowp orchestrator attach`.
#[derive(Debug, Clone, Args)]
pub struct OrchestratorAttachArgs {
    /// Orchestrator name.
    pub name: String,
    /// Holder name shown to other surfaces, for example `opencode-desktop`.
    #[arg(long, value_name = "NAME")]
    pub holder: String,
    /// Friendly surface label, for example `StackSpirit · macbook`.
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,
    /// Take the session over from a live owner.
    #[arg(long)]
    pub takeover: bool,
}

/// Arguments for `vowp orchestrator move`.
#[derive(Debug, Clone, Args)]
pub struct OrchestratorMoveArgs {
    /// Orchestrator name.
    pub name: String,
    /// Target node that runs the orchestrator; taken from the global `--node` flag, not an SSH redirect.
    #[arg(skip)]
    pub node: Option<String>,
    /// Move the orchestrator back to this home.
    ///
    /// `cli.rs` rewrites the bare `--home` spelling to this hidden switch because the
    /// global `--home <DIR>` flag already owns that long name in clap.
    #[arg(long = "move-home", hide = true)]
    pub to_home: bool,
}

/// Arguments for `vowp orchestrator new`.
#[derive(Debug, Clone, Args)]
#[command(after_help = "For `orchestrator new`, --node <name> names the node that runs the app, not an SSH redirect.")]
pub struct OrchestratorNewArgs {
    /// Orchestrator name; defaults to the project name, or `orchestrator`.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Project to bind, when any.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// App to run: `opencode`, `claude-code` or `chat`.
    #[arg(long, value_name = "APP")]
    pub app: String,
    /// Executor to use.
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: Option<String>,
    /// Node that runs the app; taken from the global `--node` flag, not an SSH redirect.
    #[arg(skip)]
    pub node: Option<String>,
    /// Replace an orchestrator with the same name.
    #[arg(long)]
    pub force: bool,
}

impl OrchestratorArgs {
    /// Moves the global `--node` value into `orchestrator new` or `orchestrator move`,
    /// where it names the orchestrator's node instead of an SSH redirect.
    ///
    /// Every other `vowp orchestrator` subcommand keeps the global meaning except
    /// `endpoint`: it is a home-side inspection command, so the value is consumed and the
    /// command never opens SSH. `cli.rs` calls this immediately after parsing and returns
    /// the value only when the command takes a target node.
    pub fn take_orchestrator_node(&mut self, node: Option<String>) -> Option<String> {
        match &mut self.command {
            OrchestratorCommand::New(args) => {
                args.node = node;
                None
            }
            OrchestratorCommand::Move(args) => {
                args.node = node;
                None
            }
            OrchestratorCommand::Endpoint(_) | OrchestratorCommand::Attach(_) => None,
            _ => node,
        }
    }
}

/// Arguments carrying only an orchestrator name.
#[derive(Debug, Clone, Args)]
pub struct OrchestratorNameArgs {
    /// Orchestrator name.
    pub name: String,
}

/// Runs `vowp orchestrator`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &OrchestratorArgs) -> Result<i32, CliError> {
    let orchestrator = Orchestrator::new(ctx.deps().clone());
    match &args.command {
        OrchestratorCommand::New(args) => create(ctx, io, &orchestrator, args),
        OrchestratorCommand::Ls => list(ctx, io, &orchestrator),
        OrchestratorCommand::Move(args) => move_orchestrator(ctx, io, &orchestrator, args),
        OrchestratorCommand::Stop(args) => stop(ctx, io, &orchestrator, &args.name),
        OrchestratorCommand::Resume(args) => resume(ctx, io, &orchestrator, &args.name),
        OrchestratorCommand::Endpoint(args) => endpoint(ctx, io, &orchestrator, args),
        OrchestratorCommand::Attach(args) => attach(ctx, io, &orchestrator, args),
        OrchestratorCommand::KeepAlive(args) => {
            orchestrator.keep_alive(&args.name)?;
            Ok(0)
        }
    }
}

/// Runs `orchestrator new`.
fn create(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    orchestrator: &Orchestrator,
    args: &OrchestratorNewArgs,
) -> Result<i32, CliError> {
    let app = parse_app(&args.app)?;
    let project = match args.project.as_deref() {
        Some(value) => Some(resolve_project(value)?),
        None => None,
    };
    let name = match &args.name {
        Some(name) => name.clone(),
        None => project
            .as_ref()
            .map(|project| project.as_str().to_owned())
            .unwrap_or_else(|| "orchestrator".to_owned()),
    };
    let executor = match &args.executor {
        Some(text) => Some(ExecutorRef {
            executor: Executor::parse(text)?,
            node: None,
        }),
        None => None,
    };
    let node = match args.node.as_deref() {
        Some(value) => Some(vowp_protocol::NodeName::parse(value)?),
        None => None,
    };
    let info = orchestrator
        .create_on_node(
            &NewOrchestrator {
                name,
                project,
                app,
                executor,
                force: args.force,
            },
            node,
        )
        .map_err(map_orchestrator_error)?;
    ensure_keeper(ctx, orchestrator, &info)?;
    print_info(ctx, io, &info)
}

/// Runs `orchestrator move`.
fn move_orchestrator(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    orchestrator: &Orchestrator,
    args: &OrchestratorMoveArgs,
) -> Result<i32, CliError> {
    let node = match (args.node.as_deref(), args.to_home) {
        (Some(_), true) => {
            return Err(CliError::Invalid(
                "orchestrator move takes exactly one of --node <NAME> or --home, not both".to_owned(),
            ));
        }
        (None, false) => {
            return Err(CliError::Invalid(
                "orchestrator move needs exactly one of --node <NAME> or --home".to_owned(),
            ));
        }
        (Some(value), false) => Some(vowp_protocol::NodeName::parse(value)?),
        (None, true) => None,
    };
    let info = orchestrator
        .move_to_node(&args.name, node)
        .map_err(map_orchestrator_error)?;
    ensure_keeper(ctx, orchestrator, &info)?;
    print_info(ctx, io, &info)
}

/// Runs `orchestrator ls`.
fn list(ctx: &Ctx, io: &mut dyn IoStreams, orchestrator: &Orchestrator) -> Result<i32, CliError> {
    let infos = orchestrator.list()?;
    if ctx.json() {
        let cards: Vec<OrchestratorCard> = infos.into_iter().map(|info| info.card).collect();
        print_json(io, &cards)?;
        return Ok(0);
    }
    if infos.is_empty() {
        writeln!(io.out(), "orchestrators: none")?;
        return Ok(0);
    }
    writeln!(
        io.out(),
        "{:<24} {:<12} {:<10} {:<26} NODE",
        "NAME",
        "APP",
        "STATE",
        "SESSION"
    )?;
    for info in &infos {
        let node = info
            .card
            .node
            .as_ref()
            .map(|node| node.as_str().to_owned())
            .unwrap_or_else(|| "home".to_owned());
        writeln!(
            io.out(),
            "{:<24} {:<12} {:<10} {:<26} {}",
            info.card.name,
            app_name(info.card.app),
            info.card.state,
            info.card.session_id,
            node
        )?;
    }
    Ok(0)
}

/// Runs `orchestrator stop`.
fn stop(ctx: &Ctx, io: &mut dyn IoStreams, orchestrator: &Orchestrator, name: &str) -> Result<i32, CliError> {
    orchestrator.stop(name).map_err(map_orchestrator_error)?;
    let info = orchestrator.info(name)?;
    print_info(ctx, io, &info)
}

/// Runs `orchestrator resume`.
fn resume(ctx: &Ctx, io: &mut dyn IoStreams, orchestrator: &Orchestrator, name: &str) -> Result<i32, CliError> {
    let info = orchestrator.resume(name).map_err(map_orchestrator_error)?;
    ensure_keeper(ctx, orchestrator, &info)?;
    print_info(ctx, io, &info)
}

/// Runs `orchestrator endpoint`.
///
/// This command is a home-side inspection: an `Orchestrator::endpoint` call never opens
/// SSH, a socket or a node's files. A node-hosted card is metadata only, and the human
/// output points the owner at the agent machine's own command. JSON carries the local auth
/// material only when the home itself hosts a live endpoint.
fn endpoint(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    orchestrator: &Orchestrator,
    args: &OrchestratorEndpointArgs,
) -> Result<i32, CliError> {
    let surface = match args.surface.as_deref() {
        Some(value) => Some(parse_surface_id(value)?),
        None => None,
    };
    let endpoint = orchestrator
        .endpoint(&args.name, surface.as_ref())
        .map_err(map_orchestrator_error)?;
    if ctx.json() {
        print_json(io, &endpoint)?;
        return Ok(0);
    }
    print_endpoint(io, &endpoint)?;
    Ok(0)
}

/// Parses `--surface`, which is a 26-character ULID.
fn parse_surface_id(value: &str) -> Result<SurfaceId, CliError> {
    let value = value.trim();
    if value.len() != 26 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(CliError::Invalid(
            "--surface takes a 26-character surface id".to_owned(),
        ));
    }
    Ok(SurfaceId(value.to_owned()))
}

/// Runs `orchestrator attach`.
///
/// Prints the grant on stdout, the hold notice on stderr (so `--json` stays parseable), then
/// blocks until stdin reaches end-of-file and releases the lease. There is no detached
/// keeper: the lease lives exactly as long as this process. The token is never logged and
/// never part of an error.
fn attach(
    ctx: &Ctx,
    io: &mut dyn IoStreams,
    orchestrator: &Orchestrator,
    args: &OrchestratorAttachArgs,
) -> Result<i32, CliError> {
    let guard = orchestrator
        .attach(&args.name, &args.holder, args.label.as_deref(), args.takeover)
        .map_err(map_orchestrator_error)?;
    if ctx.json() {
        print_json(io, guard.grant())?;
    } else {
        print_attach(io, guard.grant())?;
    }
    io.out().flush()?;
    writeln!(io.err(), "attach: holding ownership; close stdin to release")?;
    writeln!(
        io.err(),
        "warning: closing stdin releases the session; the UI loses write access"
    )?;
    io.err().flush()?;
    // Block until the parent closes stdin. Read errors end the hold the same way.
    let _ = std::io::copy(io.input(), &mut std::io::sink());
    guard.release().map_err(map_orchestrator_error)?;
    writeln!(io.err(), "attach: released")?;
    Ok(0)
}

/// Prints one attach grant as a compact block. The token is the one-time lease token; the
/// engine password stays in `--json`.
fn print_attach(io: &mut dyn IoStreams, grant: &OrchestratorAttach) -> Result<(), CliError> {
    writeln!(io.out(), "surface: {}", grant.surface_id)?;
    writeln!(io.out(), "holder: {}", grant.owner.holder)?;
    writeln!(io.out(), "token: {}", grant.token)?;
    writeln!(io.out(), "access: {}", grant.endpoint.access)?;
    writeln!(io.out(), "url: {}", grant.endpoint.url.as_deref().unwrap_or("-"))?;
    writeln!(io.out(), "expires: {}", grant.expires_at)?;
    Ok(())
}

/// Prints one endpoint as a compact key/value block without its password.
fn print_endpoint(io: &mut dyn IoStreams, endpoint: &OrchestratorEndpoint) -> Result<(), CliError> {
    let node = endpoint
        .node
        .as_ref()
        .map(|node| node.as_str().to_owned())
        .unwrap_or_else(|| "-".to_owned());
    writeln!(io.out(), "name: {}", endpoint.name)?;
    writeln!(io.out(), "app: {}", app_name(endpoint.app))?;
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
    match &endpoint.owner {
        Some(owner) => writeln!(
            io.out(),
            "owner: {} ({}) holder {} node {} mode {} state {} activity {} epoch {}",
            owner.label.as_deref().unwrap_or("-"),
            owner.kind.store_name(),
            owner.holder,
            owner.node,
            owner.mode,
            owner.state,
            owner.activity,
            owner.epoch
        )?,
        None => writeln!(io.out(), "owner: -")?,
    }
    writeln!(io.out(), "access: {}", endpoint.access)?;
    if endpoint.location == OrchestratorLocation::Node {
        writeln!(
            io.out(),
            "run on that machine: vowp agent orchestrator endpoint {} --json",
            endpoint.name
        )?;
    }
    Ok(())
}

/// Starts a detached lease keeper unless one is already running.
///
/// The CLI exits after `new`/`resume`; this child is the long-lived holder whose
/// `LeaseGuard` heartbeat keeps the session from becoming stale. It refuses to steal a
/// lease that another surface has taken over (see `Orchestrator::keep_alive`).
///
/// A remote node owns its own app host and lease, so the home starts no keeper for it. A
/// node orchestrator bound to the home (the `local` row or `deps.node`) still runs the
/// home host and home lease, so it needs the keeper exactly like a v0.2 record.
fn ensure_keeper(ctx: &Ctx, orchestrator: &Orchestrator, info: &OrchestratorInfo) -> Result<(), CliError> {
    let project_root = ctx.project_selected().then(|| ctx.project_root());
    ensure_keeper_at(ctx.paths(), project_root, orchestrator, info)
}

/// Starts a detached lease keeper for `info` unless it already has one.
///
/// This is the [`ensure_keeper`] body without a [`Ctx`], so `project add` can use it
/// after registering a checkout. `project_root` is the checkout passed to the keeper's
/// hidden `__keepalive` child as `--project`; `None` leaves the child to the home's own
/// project selection.
pub(crate) fn ensure_keeper_at(
    paths: &NodePaths,
    project_root: Option<&Path>,
    orchestrator: &Orchestrator,
    info: &OrchestratorInfo,
) -> Result<(), CliError> {
    if info.card.app == OrchestratorApp::Chat {
        return Ok(());
    }
    if let Some(node) = info.card.node.as_ref()
        && !orchestrator.is_home_node(node).map_err(map_orchestrator_error)?
    {
        return Ok(());
    }
    let dir = info.dir.clone();
    if keeper_pid(&dir).is_some() {
        return Ok(());
    }
    let _ = fs::remove_file(dir.join(KEEPER_PID_FILE));
    let exe = std::env::current_exe()?;
    start_keeper_with(paths, project_root, &info.card.name, &exe, KEEPER_START_TIMEOUT)
}

/// Starts a keeper from `exe` and waits until it publishes its pid.
///
/// Tests pass the built `vowp` binary explicitly because the test harness's
/// `current_exe` is not the CLI.
fn start_keeper_with(
    paths: &NodePaths,
    project_root: Option<&Path>,
    name: &str,
    exe: &Path,
    wait: Duration,
) -> Result<(), CliError> {
    let _child = spawn_keeper(paths, project_root, name, exe, wait)?;
    Ok(())
}

/// Spawns the keeper child and waits until it publishes its pid.
fn spawn_keeper(
    paths: &NodePaths,
    project_root: Option<&Path>,
    name: &str,
    exe: &Path,
    wait: Duration,
) -> Result<std::process::Child, CliError> {
    let dir = paths.root().join("orchestrators").join(name);
    if keeper_pid(&dir).is_some() {
        return Err(CliError::Conflict(format!(
            "an orchestrator keeper for {name} is already running"
        )));
    }
    let log_path = dir.join(KEEPER_LOG);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|source| CoreError::Io {
            path: log_path.display().to_string(),
            source,
        })?;
    let stderr = log.try_clone().map_err(|source| CoreError::Io {
        path: log_path.display().to_string(),
        source,
    })?;
    let mut command = Command::new(exe);
    command.arg("--home").arg(paths.root());
    if let Some(project_root) = project_root {
        command.arg("--project").arg(project_root);
    }
    let mut child = command
        .arg("orchestrator")
        .arg("__keepalive")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .map_err(|source| CoreError::Io {
            path: exe.display().to_string(),
            source,
        })?;
    let pid = child.id();
    let deadline = Instant::now() + wait;
    loop {
        if keeper_pid(&dir) == Some(pid) {
            return Ok(child);
        }
        if !process_alive(pid) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = signal_group(pid, Signal::Term);
    let _ = child.wait();
    let _ = fs::remove_file(dir.join(KEEPER_PID_FILE));
    let tail = fs::read_to_string(&log_path).unwrap_or_default();
    Err(CliError::Core(CoreError::Command(format!(
        "orchestrator keeper for {name} did not start; {}: {}",
        log_path.display(),
        tail.trim()
    ))))
}

/// Returns the live keeper pid from `KEEPER_PID_FILE`.
fn keeper_pid(dir: &Path) -> Option<u32> {
    let text = fs::read_to_string(dir.join(KEEPER_PID_FILE)).ok()?;
    let pid = text.trim().parse::<u32>().ok()?;
    process_alive(pid).then_some(pid)
}

/// Prints one card as JSON or as the join instructions.
fn print_info(ctx: &Ctx, io: &mut dyn IoStreams, info: &OrchestratorInfo) -> Result<i32, CliError> {
    if ctx.json() {
        print_json(io, &info.card)?;
        return Ok(0);
    }
    let node = info
        .card
        .node
        .as_ref()
        .map(|node| format!(", node {node}"))
        .unwrap_or_default();
    writeln!(
        io.out(),
        "orchestrator {} ({}{node}, {}) session {}",
        info.card.name,
        app_name(info.card.app),
        info.card.state,
        info.card.session_id
    )?;
    writeln!(io.out(), "  directory: {}", info.dir.display())?;
    if let Some(detail) = &info.detail
        && !detail.is_empty()
    {
        writeln!(io.out(), "  detail: {detail}")?;
    }
    writeln!(io.out(), "  desktop: {}", info.card.join.desktop)?;
    writeln!(io.out(), "  phone: {}", info.card.join.phone)?;
    writeln!(io.out(), "  chat: {}", info.card.join.chat)?;
    Ok(0)
}

/// Maps a typed core error onto the CLI's exit classes.
///
/// `node_offline` is a transport failure (`cli.md` §1.1 exit 7), not a refusal. Every
/// other core error keeps its own class.
fn map_orchestrator_error(error: CoreError) -> CliError {
    match &error {
        CoreError::Refused(message) if message.to_ascii_lowercase().contains("node_offline") => {
            CliError::Remote(message.clone())
        }
        _ => CliError::Core(error),
    }
}

/// Resolves `--project`, accepting a registry name or a checkout path.
///
/// The global `--project` flag has the same spelling and may reach this handler when the
/// caller selected a checkout. A path is read for its `[project].name`; a name is passed
/// through and checked against the registry by the core manager.
fn resolve_project(value: &str) -> Result<ProjectName, CliError> {
    if let Ok(name) = ProjectName::parse(value) {
        return Ok(name);
    }
    let path = Path::new(value);
    let config_path = path.join("vowp.toml");
    if path.is_dir() && config_path.is_file() {
        let text = std::fs::read_to_string(&config_path)?;
        let config = VowpConfig::from_toml(&text)?;
        return Ok(config.project.name);
    }
    Err(CliError::NotFound(format!("project {value}")))
}

/// Parses the `--app` value.
pub(crate) fn parse_app(value: &str) -> Result<OrchestratorApp, CliError> {
    match value {
        "opencode" => Ok(OrchestratorApp::Opencode),
        "claude-code" | "claude_code" | "claude" => Ok(OrchestratorApp::ClaudeCode),
        "chat" => Ok(OrchestratorApp::Chat),
        other => Err(CliError::Invalid(format!(
            "unknown app {other:?}; use opencode, claude-code or chat"
        ))),
    }
}

/// Returns the CLI name of an app.
pub(crate) fn app_name(app: OrchestratorApp) -> &'static str {
    match app {
        OrchestratorApp::Opencode => "opencode",
        OrchestratorApp::ClaudeCode => "claude-code",
        OrchestratorApp::Chat => "chat",
    }
}
