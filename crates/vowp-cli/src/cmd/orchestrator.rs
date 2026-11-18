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
use vowp_protocol::{Executor, ExecutorRef, OrchestratorApp, OrchestratorCard, ProjectName, VowpConfig};

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
    /// Stop an orchestrator's host and release its lease.
    Stop(OrchestratorNameArgs),
    /// Start an orchestrator's host again.
    Resume(OrchestratorNameArgs),
    /// Hidden: keep one orchestrator's lease alive after the CLI exits.
    #[command(name = "__keepalive", hide = true)]
    KeepAlive(OrchestratorNameArgs),
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
    /// Moves the global `--node` value into `orchestrator new`, where it names the
    /// orchestrator's node instead of an SSH redirect.
    ///
    /// Every other `vowp orchestrator` subcommand keeps the global meaning. `cli.rs`
    /// calls this immediately after parsing and returns the value only when the command
    /// is `new`.
    pub fn take_new_node(&mut self, node: Option<String>) -> Option<String> {
        match &mut self.command {
            OrchestratorCommand::New(args) => {
                args.node = node;
                None
            }
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
        OrchestratorCommand::Stop(args) => stop(ctx, io, &orchestrator, &args.name),
        OrchestratorCommand::Resume(args) => resume(ctx, io, &orchestrator, &args.name),
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
    start_keeper_with(ctx, &info.card.name, &exe, KEEPER_START_TIMEOUT)
}

/// Starts a keeper from `exe` and waits until it publishes its pid.
///
/// Tests pass the built `vowp` binary explicitly because the test harness's
/// `current_exe` is not the CLI.
fn start_keeper_with(ctx: &Ctx, name: &str, exe: &Path, wait: Duration) -> Result<(), CliError> {
    let _child = spawn_keeper(ctx, name, exe, wait)?;
    Ok(())
}

/// Spawns the keeper child and waits until it publishes its pid.
fn spawn_keeper(ctx: &Ctx, name: &str, exe: &Path, wait: Duration) -> Result<std::process::Child, CliError> {
    let dir = ctx.paths().root().join("orchestrators").join(name);
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
    command.arg("--home").arg(ctx.paths().root());
    if ctx.project_selected() {
        command.arg("--project").arg(ctx.project_root());
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
fn parse_app(value: &str) -> Result<OrchestratorApp, CliError> {
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
fn app_name(app: OrchestratorApp) -> &'static str {
    match app {
        OrchestratorApp::Opencode => "opencode",
        OrchestratorApp::ClaudeCode => "claude-code",
        OrchestratorApp::Chat => "chat",
    }
}
