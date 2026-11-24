//! clap definitions for every v0.1 and v0.2 command.
//!
//! Spec: `docs/design/cli.md` and `docs/design/v0.2/home-and-surfaces.md` §12–§13.5.
//! This file fixes every v0.2 top-level command, subcommand and flag; each command's
//! handler and its `Args` struct live in `cmd/<name>.rs`.

use std::cell::Cell;
use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use crate::cmd::capacity::CapacityArgs;
use crate::cmd::control::ControlArgs;
use crate::cmd::exec::ExecArgs;
use crate::cmd::fleet::FleetArgs;
use crate::cmd::gc::GcArgs;
use crate::cmd::lease::{LeaseArgs, TakeoverArgs};
use crate::cmd::mcp::McpArgs;
use crate::cmd::migrate::MigrateArgs;
use crate::cmd::orchestrator::OrchestratorArgs;
use crate::cmd::project::ProjectArgs;
use crate::cmd::secret::{SecretArgs, VaultAdminArgs, VaultExecArgs};
use crate::cmd::setup::SetupArgs;
use crate::cmd::tunnel::TunnelArgs;

/// Every top-level command the v0.1 CLI exposes, in `cli.md` order.
pub const V0_1_COMMANDS: &[&str] = &[
    "init", "doctor", "up", "down", "status", "board", "plan", "job", "graph", "attach", "session", "chat", "inbox",
    "cost", "node", "loops", "schedule", "promote", "rollback", "releases", "reindex", "backup", "schema", "version",
    "__window",
];

/// Every top-level command v0.2 adds, including the hidden implementation interfaces.
///
/// `Cli::command().get_subcommands()` enumerates hidden subcommands too, which is why they
/// are part of the help-test expectation.
pub const V0_2_COMMANDS: &[&str] = &[
    "project",
    "fleet",
    "capacity",
    "secret",
    "gc",
    "tunnel",
    "setup",
    "control",
    "orchestrator",
    "migrate",
    "mcp",
    "agent",
    "surface",
    "offload",
    "__exec",
    "__vault-exec",
    "__vault-admin",
];

/// The parsed command line.
#[derive(Debug)]
pub struct Cli {
    /// Global flags accepted before or after the command.
    pub global: Global,
    /// Command to run.
    pub command: Command,
}

// The clap-generated parser behind `Cli`. The public type carries the same two fields.
// This mirror lets `Cli` override `Parser::try_parse_from` for the agent chat spelling
// while every other parser method keeps clap's generated behaviour.
#[derive(Debug, Parser)]
#[command(
    name = "vowp",
    version,
    about = "VOWP: local-first supervisor for AI coding agents",
    disable_help_subcommand = true
)]
struct ParsedCli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

impl CommandFactory for Cli {
    fn command() -> clap::Command {
        ParsedCli::command()
    }

    fn command_for_update() -> clap::Command {
        ParsedCli::command_for_update()
    }
}

impl FromArgMatches for Cli {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let parsed = ParsedCli::from_arg_matches(matches)?;
        Ok(Self {
            global: parsed.global,
            command: parsed.command,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        let parsed = ParsedCli::from_arg_matches(matches)?;
        self.global = parsed.global;
        self.command = parsed.command;
        Ok(())
    }
}

impl Parser for Cli {
    /// Parses a command line, filling an agent chat's positional id from `--session`.
    ///
    /// `vowp chat --agent --session <id>` spells the session as a flag while the home
    /// `vowp chat` keeps a required positional `<ID>`; inserting the value before clap
    /// parses lets both spellings share one definition, so the home usage, help tree and
    /// missing-id error stay unchanged. Every other command line is untouched.
    fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut args: Vec<OsString> = itr.into_iter().map(Into::into).collect();
        record_agent_chat(false);
        inject_agent_chat_id(&mut args);
        rewrite_move_home(&mut args);
        ParsedCli::try_parse_from(args).map(|mut parsed| {
            // `orchestrator new --node <name>` and `orchestrator move --node <name>`
            // name the node that runs the orchestrator, not an SSH redirect. Move the
            // global value into the subcommand before `remote::maybe_run` sees it; every
            // other command keeps the global meaning.
            let node = parsed.global.node.take();
            if let Command::Orchestrator(args) = &mut parsed.command {
                parsed.global.node = args.take_orchestrator_node(node);
            } else {
                parsed.global.node = node;
            }
            Self {
                global: parsed.global,
                command: parsed.command,
            }
        })
    }
}

/// Rewrites `--home` after `orchestrator move` to its hidden switch.
///
/// `--home <DIR>` is a global flag, and clap rejects a second argument with the same long
/// name on a subcommand. For `orchestrator move` the owner spelling is the target switch,
/// so every `--home` after the subcommand is rewritten; a global home directory still goes
/// before the command. The user-facing spelling remains `--home`.
fn rewrite_move_home(args: &mut [OsString]) {
    let Some(orchestrator) = args.iter().position(|arg| arg == "orchestrator") else {
        return;
    };
    if args.get(orchestrator + 1).and_then(|arg| arg.to_str()) != Some("move") {
        return;
    }
    for arg in args.iter_mut().skip(orchestrator + 2) {
        if arg == "--home" {
            *arg = OsString::from("--move-home");
        }
    }
}

/// Inserts an agent chat's `--session` value as the required positional id.
fn inject_agent_chat_id(args: &mut Vec<OsString>) {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return;
    }
    let Some(chat) = chat_position(args) else {
        return;
    };
    let mut agent = false;
    let mut session = None;
    let mut has_id = false;
    let rest = args[chat + 1..].to_vec();
    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].to_string_lossy();
        match arg.as_ref() {
            "--agent" => agent = true,
            "--session" => {
                if let Some(value) = rest.get(index + 1) {
                    session = Some(value.clone());
                    index += 1;
                }
            }
            "--executor" | "--node" => index += 1,
            "--" => {
                has_id = index + 1 < rest.len();
                break;
            }
            value if value.starts_with("--session=") => {
                session = Some(OsString::from(&value["--session=".len()..]));
            }
            value if value.starts_with('-') => {}
            _ => has_id = true,
        }
        index += 1;
    }
    if agent
        && !has_id
        && let Some(session) = session
    {
        args.push(session);
    }
}

/// Returns the index of the `chat` command token, including `session chat`.
fn chat_position(args: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if matches!(arg.as_ref(), "--home" | "--project" | "--node") {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg == "chat" {
            return Some(index);
        }
        if arg == "session" {
            let mut next = index + 1;
            while next < args.len() {
                let value = args[next].to_string_lossy();
                if value.starts_with('-') {
                    next += 1;
                    continue;
                }
                return (value == "chat").then_some(next);
            }
            return None;
        }
        return None;
    }
    None
}

/// Global flags accepted by every command.
#[derive(Debug, Clone, Default, Args)]
pub struct Global {
    /// Print one machine-readable JSON document on stdout.
    #[arg(long, global = true)]
    pub json: bool,
    /// Run the command on that node over ssh.
    #[arg(long, global = true, value_name = "NAME")]
    pub node: Option<String>,
    /// Project name or path; defaults to the nearest ancestor with `vowp.toml`.
    #[arg(long, global = true, value_name = "NAME|PATH")]
    pub project: Option<String>,
    /// Node home (default `$VOWP_HOME`, else `~/.vowp`).
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Disable colour even when stdout is a terminal.
    #[arg(long, global = true)]
    pub no_color: bool,
    /// Suppress non-essential human output.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,
    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// One top-level command.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialise this git repository as a VOWP project.
    Init(InitArgs),
    /// Check the node, database, harnesses and providers.
    Doctor(DoctorArgs),
    /// Create the tmux layout and start the supervisor.
    Up(UpArgs),
    /// Stop the supervisor.
    Down(DownArgs),
    /// Show the supervisor, jobs, slots, limits, cost and inbox.
    Status(StatusArgs),
    /// Print or refresh the board.
    Board(BoardArgs),
    /// Start a planner turn for a goal.
    Plan(PlanArgs),
    /// Add, inspect or change jobs.
    Job {
        /// The job subcommand.
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Print, export or import the job graph.
    Graph(GraphArgs),
    /// Attach to a job's tmux window.
    Attach(AttachArgs),
    /// List and operate universal sessions.
    Session {
        /// The session subcommand.
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Alias of `session chat`.
    Chat(ChatArgs),
    /// List and answer inbox items.
    Inbox {
        /// The inbox subcommand.
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// Show cost totals.
    Cost(CostArgs),
    /// Add, list or remove nodes.
    Node {
        /// The node subcommand.
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Show the effective offload policy or the offload read model.
    Offload(OffloadArgs),
    /// List or pause supervisor loops.
    Loops(LoopsArgs),
    /// Manage scheduled actions.
    Schedule(ScheduleArgs),
    /// Promote a change to an environment.
    Promote(PromoteArgs),
    /// Roll a release back.
    Rollback(RollbackArgs),
    /// List releases.
    Releases(ReleasesArgs),
    /// Rebuild the session index from JSONL logs.
    Reindex(ReindexArgs),
    /// Back up the node database.
    Backup(BackupArgs),
    /// Print one exported JSON Schema.
    Schema(SchemaArgs),
    /// Register, list, pause or archive projects.
    Project(ProjectArgs),
    /// Read the node snapshot, queue and inbox.
    Fleet(FleetArgs),
    /// Show, change or reload measured capacity.
    Capacity(CapacityArgs),
    /// Set, list, remove or request secrets.
    Secret(SecretArgs),
    /// List lifecycle classes and collect garbage.
    Gc(GcArgs),
    /// Open, list or close control-machine SSH tunnels.
    Tunnel(TunnelArgs),
    /// Plan or apply the AI-first setup.
    Setup(SetupArgs),
    /// Configure this machine as the control machine.
    Control(ControlArgs),
    /// Create and manage orchestrator sessions.
    Orchestrator(OrchestratorArgs),
    /// Migrate a v0.1 home to the v0.2 layout.
    Migrate(MigrateArgs),
    /// Run the stdio MCP server or the Mac-side proxy.
    Mcp(McpArgs),
    /// Enroll, run or inspect the Mac-side agent.
    Agent {
        /// The agent subcommand.
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Create, list or revoke a paired local surface; `--node` names the surface's node.
    #[command(after_help = "For `surface`, --node <name> names the node the surface runs on, not an SSH redirect.")]
    Surface {
        /// The surface subcommand.
        #[command(subcommand)]
        command: SurfaceCommand,
    },
    /// Print the binary version.
    Version,
    /// Hidden: what the tmux windows run.
    #[command(name = "__window", hide = true)]
    Window(WindowArgs),
    /// Hidden: remote executor client.
    #[command(name = "__exec", hide = true)]
    Exec(ExecArgs),
    /// Hidden: vault read/inject helper.
    #[command(name = "__vault-exec", hide = true)]
    VaultExec(VaultExecArgs),
    /// Hidden: vault write helper.
    #[command(name = "__vault-admin", hide = true)]
    VaultAdmin(VaultAdminArgs),
}

/// Arguments for `vowp init`.
#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// Project name slug; defaults to the repository directory name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Overwrite an existing project and existing framework files.
    #[arg(long)]
    pub force: bool,
    /// Do not install the embedded framework files.
    #[arg(long)]
    pub no_framework: bool,
}

/// Arguments for `vowp doctor`.
#[derive(Debug, Clone, Default, Args)]
pub struct DoctorArgs {}

/// Arguments for `vowp up`.
#[derive(Debug, Clone, Args)]
pub struct UpArgs {
    /// Run the supervisor loop in this process.
    #[arg(long, conflicts_with = "detach")]
    pub foreground: bool,
    /// Explicitly detach (the default).
    #[arg(long)]
    pub detach: bool,
    /// Do not create the tmux windows.
    #[arg(long)]
    pub no_tmux: bool,
    /// Do not migrate the home before starting the supervisor.
    #[arg(long)]
    pub no_migrate: bool,
    /// Run exactly one supervisor tick and exit.
    #[arg(long)]
    pub once: bool,
    /// Drain a running supervisor, apply pending migrations and start this binary.
    #[arg(long)]
    pub replace: bool,
}

/// Arguments for `vowp down`.
#[derive(Debug, Clone, Args)]
pub struct DownArgs {
    /// Let active runs and gate tasks finish; admit no new work.
    #[arg(long)]
    pub drain: bool,
    /// Send SIGTERM if the supervisor does not stop by itself.
    #[arg(long)]
    pub force: bool,
    /// Kill the tmux windows too.
    #[arg(long)]
    pub kill_windows: bool,
}

/// Arguments for `vowp status`.
#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Exit 1 when the supervisor heartbeat is stale.
    #[arg(long)]
    pub check: bool,
}

/// Arguments for `vowp board`.
#[derive(Debug, Clone, Args)]
pub struct BoardArgs {
    /// Print once instead of refreshing.
    #[arg(long)]
    pub once: bool,
    /// Refresh interval in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 2)]
    pub interval: u64,
}

/// Arguments for `vowp plan`.
#[derive(Debug, Clone, Args)]
pub struct PlanArgs {
    /// Goal the planner must turn into jobs.
    pub goal: String,
    /// Profile the planner runs with.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Ask the planner to print YAML instead of writing jobs.
    #[arg(long)]
    pub dry_run: bool,
}

/// `vowp job` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum JobCommand {
    /// Add one job and its brief.
    Add(JobAddArgs),
    /// Change a job's editable fields.
    Update(JobUpdateArgs),
    /// List jobs.
    Ls(JobLsArgs),
    /// Show one job.
    Show(JobShowArgs),
    /// Retry a failed, stalled or parked job.
    Retry(JobIdArgs),
    /// Park a non-terminal job.
    Park(JobParkArgs),
    /// Ask a stalled job to continue.
    Nudge(JobIdArgs),
    /// Restart a stalled job with a fresh process.
    Restart(JobIdArgs),
    /// Replace a job through a planner turn.
    Replan(JobIdArgs),
    /// Give up on a stalled job.
    Fail(JobFailArgs),
    /// Cancel a non-terminal job.
    Cancel(JobIdArgs),
    /// Pin a job to an executor, or switch a running job's session onto it.
    SetExecutor(JobSetExecutorArgs),
}

/// Arguments for `vowp job set-executor`.
#[derive(Debug, Clone, Args)]
pub struct JobSetExecutorArgs {
    /// Job id.
    pub id: String,
    /// Executor as `<harness>:<provider>/<model>[@effort|#variant]`.
    pub executor: String,
}

/// Arguments for `vowp job add`.
#[derive(Debug, Clone, Args)]
pub struct JobAddArgs {
    /// Job id slug.
    #[arg(long, value_name = "SLUG")]
    pub id: String,
    /// Human-readable title.
    #[arg(long, value_name = "TEXT")]
    pub title: String,
    /// Job kind (`build`, `verify`, `plan`, ...).
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Role; defaults from the kind.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,
    /// Profile; defaults to the role name.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Jobs that must finish first.
    #[arg(long, value_name = "A,B", value_delimiter = ',')]
    pub needs: Vec<String>,
    /// Ownership globs.
    #[arg(long, value_name = "GLOB,...", value_delimiter = ',')]
    pub owns: Vec<String>,
    /// Read globs.
    #[arg(long, value_name = "GLOB,...", value_delimiter = ',')]
    pub reads: Vec<String>,
    /// Gate set names or short gate forms.
    #[arg(long, value_name = "SET|SHORT,...", value_delimiter = ',')]
    pub gates: Vec<String>,
    /// Dispatch priority (higher first).
    #[arg(long, value_name = "N")]
    pub priority: Option<i32>,
    /// Maximum fix rounds.
    #[arg(long, value_name = "N")]
    pub max_attempts: Option<u32>,
    /// Read the brief body from this file.
    #[arg(long, value_name = "PATH", conflicts_with = "brief")]
    pub brief_file: Option<PathBuf>,
    /// Read the brief body from stdin (`-`).
    #[arg(long, value_name = "PATH|-", conflicts_with = "brief_file")]
    pub brief: Option<String>,
    /// Offload placement: `home`, `any`, `node:<name>` or `node:<name>+home`.
    #[arg(long, value_name = "PLACEMENT")]
    pub placement: Option<String>,
}

/// Arguments for `vowp job update`.
#[derive(Debug, Clone, Args)]
pub struct JobUpdateArgs {
    /// Job id.
    pub id: String,
    /// New title.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
    /// New priority.
    #[arg(long, value_name = "N")]
    pub priority: Option<i32>,
    /// New profile.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Replacement ownership globs.
    #[arg(long, value_name = "GLOB,...", value_delimiter = ',')]
    pub owns: Option<Vec<String>>,
    /// Replacement read globs.
    #[arg(long, value_name = "GLOB,...", value_delimiter = ',')]
    pub reads: Option<Vec<String>>,
    /// Replacement gates.
    #[arg(long, value_name = "SET|SHORT,...", value_delimiter = ',')]
    pub gates: Option<Vec<String>>,
    /// Needs to add.
    #[arg(long, value_name = "A,B", value_delimiter = ',')]
    pub add_needs: Vec<String>,
    /// Needs to remove.
    #[arg(long, value_name = "A", value_delimiter = ',')]
    pub remove_needs: Vec<String>,
    /// Free-form note.
    #[arg(long, value_name = "TEXT")]
    pub note: Option<String>,
}

/// Arguments for `vowp job ls`.
#[derive(Debug, Clone, Args)]
pub struct JobLsArgs {
    /// Show only these states.
    #[arg(long, value_name = "STATE,...", value_delimiter = ',')]
    pub state: Vec<String>,
    /// Show only this kind.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
    /// Show only this role.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,
    /// Include done, cancelled and replanned jobs.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `vowp job show`.
#[derive(Debug, Clone, Args)]
pub struct JobShowArgs {
    /// Job id.
    pub id: String,
    /// Print the brief file.
    #[arg(long)]
    pub brief: bool,
    /// Print the report file.
    #[arg(long)]
    pub report: bool,
    /// Include the job's runs.
    #[arg(long)]
    pub runs: bool,
    /// Include the job's latest gate results.
    #[arg(long)]
    pub gates: bool,
}

/// Arguments carrying only a job id.
#[derive(Debug, Clone, Args)]
pub struct JobIdArgs {
    /// Job id.
    pub id: String,
}

/// Arguments for `vowp job park`.
#[derive(Debug, Clone, Args)]
pub struct JobParkArgs {
    /// Job id.
    pub id: String,
    /// Why the job is parked.
    #[arg(long, value_name = "TEXT")]
    pub reason: Option<String>,
}

/// Arguments for `vowp job fail`.
#[derive(Debug, Clone, Args)]
pub struct JobFailArgs {
    /// Job id.
    pub id: String,
    /// Why the job failed.
    #[arg(long, value_name = "TEXT")]
    pub reason: String,
}

/// Arguments for `vowp graph`.
#[derive(Debug, Clone, Args)]
pub struct GraphArgs {
    /// Render Mermaid instead of ASCII.
    #[arg(long, conflicts_with_all = ["export", "import"])]
    pub mermaid: bool,
    /// Write the graph YAML to this file (`-` for stdout).
    #[arg(long, value_name = "FILE", conflicts_with = "import")]
    pub export: Option<String>,
    /// Insert the jobs and edges from this YAML file.
    #[arg(long, value_name = "FILE")]
    pub import: Option<PathBuf>,
    /// Include every project on this node.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `vowp attach`.
#[derive(Debug, Clone, Args)]
pub struct AttachArgs {
    /// Job id; without one, attach to the board window.
    pub job: Option<String>,
}

/// `vowp session` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum SessionCommand {
    /// List sessions.
    Ls(SessionLsArgs),
    /// Show one session.
    Show(SessionShowArgs),
    /// Create an interactive session.
    New(SessionNewArgs),
    /// Chat with a session.
    Chat(ChatArgs),
    /// Switch a session to another executor.
    Switch(SessionSwitchArgs),
    /// Write a capsule now.
    Capsule(SessionCapsuleArgs),
    /// Export a session bundle or ASIF package.
    Export(SessionExportArgs),
    /// Import a session bundle or ASIF package.
    Import(SessionImportArgs),
    /// Pull a session from another node.
    Pull(SessionPullArgs),
    /// Push a session to another node.
    Push(SessionPushArgs),
    /// Hidden: stream a full session directory snapshot.
    #[command(name = "dir-export", hide = true)]
    DirExport(SessionDirExportArgs),
    /// Record interactive Claude Code transcripts.
    Watch(SessionWatchArgs),
    /// Print a session's event log.
    Tail(SessionTailArgs),
    /// Hidden: begin a remote turn and print its grant.
    #[command(name = "turn-begin", hide = true)]
    TurnBegin(TurnBeginArgs),
    /// Hidden: ingest remote events for one run.
    #[command(name = "ingest", hide = true)]
    Ingest(IngestArgs),
    /// Hidden: show or acquire a session lease.
    #[command(name = "lease", hide = true)]
    Lease(LeaseArgs),
    /// Hidden: take a session over from another surface.
    #[command(name = "takeover", hide = true)]
    Takeover(TakeoverArgs),
}

/// Arguments for `vowp session ls`.
#[derive(Debug, Clone, Args)]
pub struct SessionLsArgs {
    /// Restrict to one project.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// Restrict to one job.
    #[arg(long, value_name = "ID")]
    pub job: Option<String>,
    /// Restrict to one state.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    /// Maximum rows.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
}

/// Arguments for `vowp session show`.
#[derive(Debug, Clone, Args)]
pub struct SessionShowArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Include the last capsule.
    #[arg(long)]
    pub capsule: bool,
    /// Print the last N events.
    #[arg(long, value_name = "N")]
    pub events: Option<usize>,
    /// Include the switch lineage.
    #[arg(long)]
    pub lineage: bool,
}

/// Arguments for `vowp session new`.
#[derive(Debug, Clone, Args)]
pub struct SessionNewArgs {
    /// Session title.
    #[arg(long, value_name = "TEXT")]
    pub title: Option<String>,
    /// Executor string.
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: Option<String>,
    /// Working directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Node to run on.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,
}

/// Arguments for `vowp session chat` and `vowp chat`.
///
/// `--session` is the agent spelling and the positional id the home spelling. Both name
/// the same session: [`Cli::try_parse_from`] fills the required positional from
/// `--session` for an agent command, so the home usage, help tree and missing-id error
/// stay unchanged. [`agent_chat_requested`] lets the owned `chat` module dispatch the
/// agent bridge before the home lease path.
#[derive(Debug, Clone)]
pub struct ChatArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Switch to this executor before the first turn.
    pub executor: Option<String>,
    /// Node that runs the turn; defaults to this surface's machine.
    pub node: Option<String>,
    /// Follow the log read-only, without taking the lease.
    pub follow: bool,
    /// Take the lease from a live surface before the first turn.
    pub takeover: bool,
    /// Run the local agent chat bridge instead of the home surface.
    pub agent: bool,
    /// Session id for `--agent`; equivalent to the positional id.
    pub session: Option<String>,
}

/// The clap field set behind [`ChatArgs`].
#[derive(Debug, Clone, Args)]
struct ChatArgsFields {
    /// Session id or unique prefix.
    #[arg(value_name = "ID")]
    id: String,
    /// Switch to this executor before the first turn.
    #[arg(long, value_name = "EXECUTOR")]
    executor: Option<String>,
    /// Node that runs the turn; defaults to this surface's machine.
    #[arg(long, value_name = "NAME")]
    node: Option<String>,
    /// Follow the log read-only, without taking the lease.
    #[arg(long)]
    follow: bool,
    /// Take the lease from a live surface before the first turn.
    #[arg(long)]
    takeover: bool,
    /// Run the local agent chat bridge instead of the home surface.
    #[arg(long)]
    agent: bool,
    /// Session id for `--agent`; equivalent to the positional id.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
}

thread_local! {
    /// Whether the last chat command parsed on this thread asked for `--agent`.
    static AGENT_CHAT: Cell<bool> = const { Cell::new(false) };
}

/// Records whether one parsed chat command asked for `--agent`.
fn record_agent_chat(agent: bool) {
    AGENT_CHAT.with(|flag| flag.set(agent));
}

/// Returns whether this thread last parsed a `chat --agent` command.
pub fn agent_chat_requested() -> bool {
    AGENT_CHAT.with(Cell::get)
}

impl Args for ChatArgs {
    fn augment_args(command: clap::Command) -> clap::Command {
        ChatArgsFields::augment_args(command)
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        ChatArgsFields::augment_args_for_update(command)
    }
}

impl FromArgMatches for ChatArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let fields = ChatArgsFields::from_arg_matches(matches)?;
        let id = fields.session.clone().unwrap_or_else(|| fields.id.clone());
        record_agent_chat(fields.agent);
        Ok(Self {
            id,
            executor: fields.executor,
            node: fields.node,
            follow: fields.follow,
            takeover: fields.takeover,
            agent: fields.agent,
            session: fields.session,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        let fields = ChatArgsFields::from_arg_matches(matches)?;
        self.id = fields.session.clone().unwrap_or_else(|| fields.id.clone());
        record_agent_chat(fields.agent);
        self.executor = fields.executor;
        self.node = fields.node;
        self.follow = fields.follow;
        self.takeover = fields.takeover;
        self.agent = fields.agent;
        self.session = fields.session;
        Ok(())
    }
}

/// Arguments for `vowp session switch`.
#[derive(Debug, Clone, Args)]
pub struct SessionSwitchArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Target executor.
    #[arg(long, value_name = "EXECUTOR")]
    pub to: String,
    /// Node to run the target on.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,
    /// Why the switch happens.
    #[arg(long, value_name = "REASON")]
    pub reason: Option<String>,
    /// Switch even while a turn is running.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `vowp session capsule`.
#[derive(Debug, Clone, Args)]
pub struct SessionCapsuleArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Print the capsule markdown instead of its id.
    #[arg(long)]
    pub print: bool,
}

/// Session interchange format for `vowp session export` and `vowp session import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SessionFormat {
    /// Native `vowp-bundle-v1` JSON. The default for both commands.
    Bundle,
    /// ASIF 0.4 (review draft) package. Semantic interchange with declared losses.
    Asif,
}

/// Arguments for `vowp session export`.
#[derive(Debug, Clone, Args)]
pub struct SessionExportArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Interchange format.
    #[arg(long, value_enum, default_value = "bundle", value_name = "FORMAT")]
    pub format: SessionFormat,
    /// Output file (default `./<id>.vowp-bundle.json`, or `./<id>.asif.zip` for ASIF);
    /// `-` writes to stdout.
    #[arg(long, short = 'o', value_name = "FILE|-")]
    pub out: Option<PathBuf>,
    /// Number of tail events to include (bundle only).
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,
    /// Replace this exact literal with `<redacted>` (ASIF only, repeatable).
    #[arg(long, value_name = "LITERAL")]
    pub redact: Vec<String>,
    /// Read exact redaction literals from a local file, one per line (ASIF only).
    #[arg(long, value_name = "FILE")]
    pub redact_file: Vec<PathBuf>,
    /// Keep host-path-looking strings in a trusted local export (ASIF only).
    #[arg(long)]
    pub keep_host_paths: bool,
    /// Keep the full `vowp-asif-report-v1` document at this file (ASIF only).
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Refuse `Blocking` and `Integrity` ASIF losses (ASIF only).
    #[arg(long, conflicts_with = "allow_loss")]
    pub strict: bool,
    /// Let recoverable ASIF integrity/ambiguity problems become declared losses (ASIF only).
    #[arg(long)]
    pub allow_loss: bool,
    /// Hidden: write one stream envelope line and the package bytes (ASIF only).
    #[arg(long, hide = true)]
    pub asif_stream: bool,
}

/// Arguments for `vowp session import`.
#[derive(Debug, Clone, Args)]
pub struct SessionImportArgs {
    /// Bundle/package file, document, directory, or `-` for stdin.
    pub file: String,
    /// Interchange format.
    #[arg(long, value_enum, default_value = "bundle", value_name = "FORMAT")]
    pub format: SessionFormat,
    /// Node to import on.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,
    /// Executor for the child session (bundle only).
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: Option<String>,
    /// Working directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Keep the bundle's session id; refuse when it already exists locally (bundle only).
    #[arg(long)]
    pub same_id: bool,
    /// Select one ASIF branch when the document has more than one (ASIF only).
    #[arg(long, value_name = "ID")]
    pub branch: Option<String>,
    /// Keep the full `vowp-asif-report-v1` document at this file (ASIF only).
    #[arg(long, value_name = "FILE")]
    pub report: Option<PathBuf>,
    /// Refuse `Blocking` and `Integrity` ASIF losses (ASIF only).
    #[arg(long, conflicts_with = "allow_loss")]
    pub strict: bool,
    /// Let recoverable ASIF integrity/ambiguity problems become declared losses (ASIF only).
    #[arg(long)]
    pub allow_loss: bool,
}

/// Arguments for `vowp session pull`.
#[derive(Debug, Clone, Args)]
pub struct SessionPullArgs {
    /// Node that owns the source session.
    pub node: String,
    /// Source session id or unique prefix.
    pub session: String,
    /// Keep the source session id; refuse when it already exists locally.
    #[arg(long)]
    pub same_id: bool,
    /// Copy the full session directory (log, blobs, capsules, adapters.json).
    #[arg(long, conflicts_with = "same_id")]
    pub full: bool,
}

/// Arguments for `vowp session push`.
#[derive(Debug, Clone, Args)]
pub struct SessionPushArgs {
    /// Node that receives the session.
    pub node: String,
    /// Local session id or unique prefix.
    pub session: String,
    /// Keep the source session id; refuse when it already exists on the node.
    #[arg(long)]
    pub same_id: bool,
}

/// Arguments for the hidden `vowp session dir-export` stream.
#[derive(Debug, Clone, Args)]
pub struct SessionDirExportArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Refuse before hashing when the session directory exceeds this many bytes.
    #[arg(long, value_name = "N")]
    pub max_bytes: Option<u64>,
}

/// Arguments for `vowp session watch`.
#[derive(Debug, Clone, Args)]
pub struct SessionWatchArgs {
    /// Claude Code project directory to scan.
    #[arg(long, value_name = "DIR")]
    pub claude_dir: Option<PathBuf>,
    /// Scan once instead of following.
    #[arg(long)]
    pub once: bool,
    /// Follow new transcript lines until interrupted.
    #[arg(long)]
    pub follow: bool,
}

/// Arguments for `vowp session tail`.
#[derive(Debug, Clone, Args)]
pub struct SessionTailArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Follow the log as it grows.
    #[arg(long)]
    pub follow: bool,
    /// One pretty line per event.
    #[arg(long)]
    pub pretty: bool,
    /// Start after this sequence number.
    #[arg(long, value_name = "N")]
    pub from: Option<u64>,
}

/// Arguments for the hidden `vowp session turn-begin`.
#[derive(Debug, Clone, Args)]
pub struct TurnBeginArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Executor to run on the surface's machine.
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: String,
    /// Purpose of the run (`start`, `continue`, `fix`, ...).
    #[arg(long, value_name = "PURPOSE")]
    pub purpose: String,
    /// Node that will execute the run.
    #[arg(long, value_name = "NAME")]
    pub node: String,
    /// Request document file, or `-` for stdin.
    #[arg(long, value_name = "FILE|-")]
    pub request: String,
}

/// Arguments for the hidden `vowp session ingest`.
#[derive(Debug, Clone, Args)]
pub struct IngestArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Run id to ingest.
    #[arg(long, value_name = "ID")]
    pub run: String,
    /// Hint that the client is replaying its spool.
    #[arg(long)]
    pub replay: bool,
}

/// `vowp inbox` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum InboxCommand {
    /// List inbox items.
    Ls(InboxLsArgs),
    /// Show one item.
    Show(InboxShowArgs),
    /// Ask a question or request an approval.
    Ask(InboxAskArgs),
    /// Answer an item.
    Answer(InboxAnswerArgs),
    /// Dismiss an item.
    Dismiss(InboxDismissArgs),
}

/// Arguments for `vowp inbox ls`.
#[derive(Debug, Clone, Args)]
pub struct InboxLsArgs {
    /// Include answered items.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `vowp inbox show`.
#[derive(Debug, Clone, Args)]
pub struct InboxShowArgs {
    /// Inbox item id.
    pub id: String,
}

/// Arguments for `vowp inbox ask`.
#[derive(Debug, Clone, Args)]
pub struct InboxAskArgs {
    /// Short title.
    pub title: String,
    /// Full body.
    #[arg(long, value_name = "TEXT")]
    pub body: Option<String>,
    /// Job to park while the item is open.
    #[arg(long, value_name = "ID")]
    pub job: Option<String>,
    /// Item kind (`question`, `approval`, `notice`).
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,
    /// Suggested answer.
    #[arg(long = "option", value_name = "TEXT")]
    pub options: Vec<String>,
}

/// Arguments for `vowp inbox answer`.
#[derive(Debug, Clone, Args)]
pub struct InboxAnswerArgs {
    /// Inbox item id.
    pub id: String,
    /// Answer text.
    pub text: Option<String>,
    /// Shorthand approval.
    #[arg(long)]
    pub approve: bool,
    /// Shorthand rejection.
    #[arg(long)]
    pub reject: bool,
    /// Who answered.
    #[arg(long, value_name = "NAME")]
    pub by: Option<String>,
}

/// Arguments for `vowp inbox dismiss`.
#[derive(Debug, Clone, Args)]
pub struct InboxDismissArgs {
    /// Inbox item id.
    pub id: String,
}

/// Arguments for `vowp cost`.
#[derive(Debug, Clone, Args)]
pub struct CostArgs {
    /// Group by `job`, `role`, `provider`, `model` or `day`.
    #[arg(long, value_name = "DIM")]
    pub by: Option<String>,
    /// Start of the window (duration or date).
    #[arg(long, value_name = "DUR|DATE")]
    pub since: Option<String>,
    /// Restrict to one job.
    #[arg(long, value_name = "ID")]
    pub job: Option<String>,
}

/// `vowp node` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum NodeCommand {
    /// Add a node and probe it.
    Add(NodeAddArgs),
    /// List nodes.
    Ls,
    /// Remove a node.
    Rm(NodeRmArgs),
    /// Create a pending local-agent node and a one-time enrollment code.
    Enroll(NodeEnrollArgs),
    /// Revoke a node's credentials.
    Revoke(NodeRevokeArgs),
    /// Show the redacted node card.
    Show(NodeShowArgs),
}

/// Arguments for `vowp node add`.
#[derive(Debug, Clone, Args)]
pub struct NodeAddArgs {
    /// Node name slug.
    pub name: String,
    /// ssh destination.
    pub ssh_target: String,
    /// Fail when the probe fails.
    #[arg(long)]
    pub strict: bool,
}

/// Arguments for `vowp node rm`.
#[derive(Debug, Clone, Args)]
pub struct NodeRmArgs {
    /// Node name.
    pub name: String,
    /// Remove without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp node enroll`.
#[derive(Debug, Clone, Args)]
pub struct NodeEnrollArgs {
    /// Node name slug.
    pub name: String,
    /// Owner-assigned label; repeat for several labels.
    #[arg(long, value_name = "LABEL")]
    pub label: Vec<String>,
    /// Rotate a pending node's one-time code in one transaction.
    #[arg(long)]
    pub replace: bool,
}

/// Arguments for `vowp node revoke`.
#[derive(Debug, Clone, Args)]
pub struct NodeRevokeArgs {
    /// Node name.
    pub name: String,
    /// Revoke without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp node show`.
#[derive(Debug, Clone, Args)]
pub struct NodeShowArgs {
    /// Node name.
    pub name: String,
}

/// Arguments for `vowp offload`.
#[derive(Debug, Clone, Args)]
pub struct OffloadArgs {
    /// The offload subcommand.
    #[command(subcommand)]
    pub command: OffloadCommand,
}

/// The `vowp offload` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum OffloadCommand {
    /// Show the effective offload policy.
    Policy(OffloadPolicyArgs),
    /// Show nodes, live assignments and waiting jobs.
    Status(OffloadStatusArgs),
}

/// Arguments for `vowp offload policy`.
///
/// The project selector is the global `--project <name|path>` flag; the selected
/// `vowp.toml` supplies the policy document. [`OffloadPolicyArgs::project_explicit`]
/// records whether that flag was passed, because clap does not expose global-flag
/// presence through [`crate::Ctx`].
#[derive(Debug, Clone, Default)]
pub struct OffloadPolicyArgs {
    /// Whether the global `--project` flag was passed.
    pub project_explicit: bool,
}

/// The clap field set behind [`OffloadPolicyArgs`]; it defines no arguments of its own.
#[derive(Debug, Clone, Args)]
struct OffloadPolicyArgsFields {}

impl Args for OffloadPolicyArgs {
    fn augment_args(command: clap::Command) -> clap::Command {
        OffloadPolicyArgsFields::augment_args(command)
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        OffloadPolicyArgsFields::augment_args_for_update(command)
    }
}

impl FromArgMatches for OffloadPolicyArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        OffloadPolicyArgsFields::from_arg_matches(matches)?;
        Ok(Self {
            project_explicit: matches.get_one::<String>("project").is_some(),
        })
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        OffloadPolicyArgsFields::from_arg_matches(matches)?;
        self.project_explicit = matches.get_one::<String>("project").is_some();
        Ok(())
    }
}

/// Arguments for `vowp offload status`.
#[derive(Debug, Clone, Default, Args)]
pub struct OffloadStatusArgs {}

/// `vowp agent` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    /// Enroll this Mac with a one-time code.
    Enroll(AgentEnrollArgs),
    /// Add, remove or list the local project path map.
    Paths(AgentPathsArgs),
    /// Redeem a home surface pairing code and store the token.
    Pair(AgentPairArgs),
    /// Run the agent client until revoked or superseded.
    Run(AgentRunArgs),
    /// Show the last local agent status.
    Status(AgentStatusArgs),
    /// Install, remove or inspect the launchd or systemd-user service.
    Service(AgentServiceArgs),
    /// Report local agent warnings without failing on them.
    Doctor(AgentDoctorArgs),
    /// Confirm a changed system-mode WSS leaf fingerprint after verifying it out of band.
    Trust(AgentTrustArgs),
    /// Inspect one local orchestrator on this agent machine.
    Orchestrator(AgentOrchestratorArgs),
    /// Hidden: serve the home side of one agent stdio or WebSocket connection.
    #[command(hide = true)]
    Gateway(AgentGatewayArgs),
}

/// Arguments for `vowp agent enroll`.
///
/// `--target` names the SSH home. `--wss` instead stores a WSS transport; the target is then
/// optional because the proxy URL carries the route.
#[derive(Debug, Clone, Args)]
pub struct AgentEnrollArgs {
    /// ssh destination of the home.
    #[arg(long, value_name = "SSH-TARGET")]
    pub target: Option<String>,
    /// File holding the one-time code, or `-` for stdin.
    #[arg(long, value_name = "PATH|-")]
    pub code_file: String,
    /// Map a local project checkout; repeat for more projects.
    #[arg(long, value_name = "PATH")]
    pub project: Vec<String>,
    /// Worktree root for every --project; defaults to `<checkout>/.vowp/worktrees`.
    #[arg(long, value_name = "PATH")]
    pub worktree_root: Option<PathBuf>,
    /// Reach the home through a WSS proxy URL instead of SSH.
    #[arg(long, value_name = "URL")]
    pub wss: Option<String>,
    /// Hard-pin the proxy certificate (`sha256:<hex>`); only with --wss.
    #[arg(long, value_name = "SHA256:HEX", requires = "wss")]
    pub tls_fingerprint: Option<String>,
}

/// Arguments for `vowp agent paths`.
#[derive(Debug, Clone, Args)]
pub struct AgentPathsArgs {
    /// The path-map subcommand.
    #[command(subcommand)]
    pub command: AgentPathsCommand,
}

/// `vowp agent paths` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentPathsCommand {
    /// Add or update one project checkout.
    Add(AgentPathsAddArgs),
    /// Remove one project from the map.
    Rm(AgentPathsRmArgs),
    /// List the local project path map.
    Ls(AgentPathsLsArgs),
}

/// Arguments for `vowp agent paths add`.
#[derive(Debug, Clone, Args)]
pub struct AgentPathsAddArgs {
    /// Local project checkout to map.
    #[arg(long, value_name = "PATH")]
    pub project: String,
    /// Project name override; defaults to `[project].name` in its `vowp.toml`.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Worktree root; defaults to `<checkout>/.vowp/worktrees`.
    #[arg(long, value_name = "PATH")]
    pub worktree_root: Option<PathBuf>,
    /// Allowed relative path below the checkout; repeat for more.
    #[arg(long, value_name = "REL")]
    pub allow: Vec<String>,
}

/// Arguments for `vowp agent paths rm`.
#[derive(Debug, Clone, Args)]
pub struct AgentPathsRmArgs {
    /// Project name slug to remove.
    #[arg(value_name = "PROJECT")]
    pub project_name: String,
}

/// Arguments for `vowp agent paths ls`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentPathsLsArgs {}

/// Arguments for `vowp agent pair`.
#[derive(Debug, Clone, Args)]
pub struct AgentPairArgs {
    /// File holding the one-time pairing code, or `-` for stdin.
    #[arg(long, value_name = "PATH|-")]
    pub code_file: String,
    /// Surface id printed by `vowp surface pair`.
    #[arg(long, value_name = "ID")]
    pub surface: String,
    /// Session id printed by `vowp surface pair`.
    #[arg(long, value_name = "ID")]
    pub session: String,
    /// Application name; defaults to `agent` when the home did not bind one.
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
}

/// Arguments for `vowp agent run`.
#[derive(Debug, Clone, Args)]
pub struct AgentRunArgs {
    /// Keep the process in the terminal (the Phase 0 default).
    #[arg(long)]
    pub foreground: bool,
    /// Agent role: `surface`, `executor` or `both`.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,
    /// Run under the service manager: write the `run` marker and drain on SIGTERM.
    #[arg(long)]
    pub service: bool,
}

/// Arguments for `vowp agent status`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentStatusArgs {}

/// Arguments for `vowp agent orchestrator`.
#[derive(Debug, Clone, Args)]
pub struct AgentOrchestratorArgs {
    /// The local orchestrator inspection subcommand.
    #[command(subcommand)]
    pub command: AgentOrchestratorCommand,
}

/// `vowp agent orchestrator` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentOrchestratorCommand {
    /// Show the local endpoint document for one orchestrator.
    Endpoint(AgentOrchestratorEndpointArgs),
}

/// Arguments for `vowp agent orchestrator endpoint`.
#[derive(Debug, Clone, Args)]
pub struct AgentOrchestratorEndpointArgs {
    /// Orchestrator name.
    pub name: String,
}

/// Arguments for the hidden `vowp agent gateway`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentGatewayArgs {
    /// Serve one JSON frame per line on stdin/stdout.
    #[arg(long, conflicts_with = "listen")]
    pub stdio: bool,
    /// Serve one gateway connection per WebSocket on this loopback address.
    #[arg(long, value_name = "ADDR", conflicts_with = "stdio")]
    pub listen: Option<String>,
}

/// Arguments for `vowp agent service`.
#[derive(Debug, Clone, Args)]
pub struct AgentServiceArgs {
    /// The service subcommand.
    #[command(subcommand)]
    pub command: AgentServiceCommand,
}

/// `vowp agent service` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum AgentServiceCommand {
    /// Write exactly one launchd plist or systemd user unit.
    Install(AgentServiceInstallArgs),
    /// Stop the unit and remove only a VOWP-generated file.
    Uninstall(AgentServiceUninstallArgs),
    /// Show the unit path and the manager's reported state.
    Status(AgentServiceStatusArgs),
}

/// Arguments for `vowp agent service install`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentServiceInstallArgs {
    /// Print the unit text and commands without writing or invoking the manager.
    #[arg(long)]
    pub print: bool,
    /// Replace a file at the unit path that VOWP did not generate.
    #[arg(long)]
    pub force: bool,
    /// Run the current-user manager commands after writing the unit.
    #[arg(long)]
    pub now: bool,
}

/// Arguments for `vowp agent service uninstall`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentServiceUninstallArgs {
    /// Remove the unit without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp agent service status`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentServiceStatusArgs {}

/// Arguments for `vowp agent doctor`.
#[derive(Debug, Clone, Default, Args)]
pub struct AgentDoctorArgs {}

/// Arguments for `vowp agent trust`.
#[derive(Debug, Clone, Args)]
pub struct AgentTrustArgs {
    /// New leaf fingerprint (`sha256:<64 lowercase hex>`), verified through a trusted channel.
    #[arg(long, value_name = "SHA256:HEX")]
    pub fingerprint: String,
}

/// `vowp surface` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum SurfaceCommand {
    /// Create a pending surface and print its one-time pairing code.
    Pair(SurfacePairArgs),
    /// List surfaces, their scopes and token expiry.
    Ls(SurfaceLsArgs),
    /// Revoke a surface and every token it holds.
    Revoke(SurfaceRevokeArgs),
    /// Rotate a surface token, keeping the old token for a ten-minute overlap.
    Rotate(SurfaceRotateArgs),
    /// Save a friendly name for a surface.
    Rename(SurfaceRenameArgs),
}

/// Arguments for `vowp surface rename`.
#[derive(Debug, Clone, Args)]
pub struct SurfaceRenameArgs {
    /// Surface id.
    pub id: String,
    /// New friendly name, for example `Kitchen laptop`.
    pub label: Option<String>,
    /// Restore the default name (`<kind name> · <node>`).
    #[arg(long)]
    pub clear: bool,
}

/// Arguments for `vowp surface pair`.
///
/// The target node comes from the global `--node <name>` flag: for this command it names
/// the node the surface runs on, not an SSH redirect.
#[derive(Debug, Clone, Args)]
#[command(after_help = "For this command --node <name> names the node the surface runs on, not an SSH redirect.")]
pub struct SurfacePairArgs {
    /// Session the surface will mirror.
    #[arg(long, value_name = "ID")]
    pub session: String,
    /// Surface kind: `opencode` or `chat`.
    #[arg(long, value_name = "KIND")]
    pub kind: String,
    /// Application name carried into the token scopes.
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,
    /// Revoke this surface and create the replacement pending surface in one transaction.
    #[arg(long, value_name = "SURFACE-ID")]
    pub replace: Option<String>,
    /// Friendly saved name for the surface (default: `<kind name> · <node>`).
    #[arg(long, value_name = "LABEL")]
    pub label: Option<String>,
}

/// Arguments for `vowp surface ls`.
///
/// The optional node filter comes from the global `--node <name>` flag.
#[derive(Debug, Clone, Default, Args)]
#[command(after_help = "For this command --node <name> filters by the surface's node, not an SSH redirect.")]
pub struct SurfaceLsArgs {}

/// Arguments for `vowp surface revoke`.
#[derive(Debug, Clone, Args)]
pub struct SurfaceRevokeArgs {
    /// Surface id.
    pub id: String,
    /// Revoke without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp surface rotate`.
///
/// The optional node check comes from the global `--node <name>` flag: for this command it
/// names the node the surface runs on, not an SSH redirect.
#[derive(Debug, Clone, Args)]
#[command(after_help = "For this command --node <name> names the node the surface runs on, not an SSH redirect.")]
pub struct SurfaceRotateArgs {
    /// Surface id printed by `vowp surface pair`.
    pub id: String,
    /// Poll the outbox for this long and exit 7 with `rotation_pending` if still queued.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
    /// Shorten the old tokens to `now + grace` instead of the ten-minute default (max 24 h).
    #[arg(long, value_name = "DURATION")]
    pub grace: Option<String>,
}

/// Arguments for `vowp loops`.
#[derive(Debug, Clone, Args)]
pub struct LoopsArgs {
    /// Optional pause/resume action.
    #[command(subcommand)]
    pub command: Option<LoopsCommand>,
    /// List every project's loop rows.
    #[arg(long)]
    pub all: bool,
}

/// `vowp loops` actions.
#[derive(Debug, Clone, Subcommand)]
pub enum LoopsCommand {
    /// Pause a loop.
    Pause(LoopNameArgs),
    /// Resume a loop.
    Resume(LoopNameArgs),
}

/// Arguments carrying only a loop name.
#[derive(Debug, Clone, Args)]
pub struct LoopNameArgs {
    /// Loop name, for example `l4-planner`.
    pub loop_name: String,
}

/// `vowp schedule` arguments.
#[derive(Debug, Clone, Args)]
pub struct ScheduleArgs {
    /// The schedule subcommand.
    #[command(subcommand)]
    pub command: ScheduleCommand,
    /// Show every project's schedules.
    #[arg(long)]
    pub all: bool,
}

impl std::ops::Deref for ScheduleArgs {
    type Target = ScheduleCommand;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

/// `vowp schedule` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleCommand {
    /// Add a schedule.
    Add(ScheduleAddArgs),
    /// List schedules.
    Ls,
    /// Pause a schedule.
    Pause(ScheduleNameArgs),
    /// Resume a schedule.
    Resume(ScheduleNameArgs),
    /// Remove a schedule.
    Rm(ScheduleNameArgs),
}

/// Arguments for `vowp schedule add`.
#[derive(Debug, Clone, Args)]
pub struct ScheduleAddArgs {
    /// Five-field cron expression or `every <duration>`.
    pub spec: String,
    /// What the schedule does.
    #[command(subcommand)]
    pub action: ScheduleActionArg,
}

/// Options shared by every `vowp schedule add` action, accepted after the action keyword.
#[derive(Debug, Clone, Args)]
pub struct ScheduleCommonArgs {
    /// Unique schedule name.
    #[arg(long, value_name = "NAME")]
    pub name: String,
    /// Fixed offset or `UTC`.
    #[arg(long, value_name = "TZ")]
    pub tz: Option<String>,
    /// `run_once` or `skip`.
    #[arg(long, value_name = "POLICY")]
    pub missed: Option<String>,
}

/// The action part of `vowp schedule add`.
#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleActionArg {
    /// Trigger one run of a loop.
    Loop(ScheduleLoopArgs),
    /// Run a command.
    Command(ScheduleCommandArgs),
    /// Create a job from a template file.
    JobTemplate(ScheduleJobTemplateArgs),
}

/// Arguments for a scheduled loop action.
#[derive(Debug, Clone, Args)]
pub struct ScheduleLoopArgs {
    /// Loop name, for example `l4-planner`.
    pub loop_name: String,
    /// Name, timezone and missed-run policy.
    #[command(flatten)]
    pub common: ScheduleCommonArgs,
}

/// Arguments for a scheduled command action.
#[derive(Debug, Clone, Args)]
pub struct ScheduleCommandArgs {
    /// Name, timezone and missed-run policy.
    #[command(flatten)]
    pub common: ScheduleCommonArgs,
    /// Program and arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true)]
    pub argv: Vec<String>,
}

/// Arguments for a scheduled job-template action.
#[derive(Debug, Clone, Args)]
pub struct ScheduleJobTemplateArgs {
    /// Name, timezone and missed-run policy.
    #[command(flatten)]
    pub common: ScheduleCommonArgs,
    /// JSON file containing a `NewJob`.
    #[arg(long, value_name = "FILE")]
    pub template_file: PathBuf,
}

/// Arguments carrying only a schedule name.
#[derive(Debug, Clone, Args)]
pub struct ScheduleNameArgs {
    /// Schedule name.
    pub name: String,
}

/// Arguments for `vowp promote`.
#[derive(Debug, Clone, Args)]
pub struct PromoteArgs {
    /// Commit, ref or job id to promote.
    pub change: String,
    /// Target environment.
    #[arg(long, value_name = "ENV")]
    pub to: String,
    /// Skip the inbox approval only when policy permits it.
    #[arg(long)]
    pub yes: bool,
    /// Run guards and the diff only.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `vowp rollback`.
#[derive(Debug, Clone, Args)]
pub struct RollbackArgs {
    /// Release id to roll back.
    pub release: String,
    /// Confirm the rollback.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp releases`.
#[derive(Debug, Clone, Args)]
pub struct ReleasesArgs {
    /// Restrict to one environment.
    #[arg(long, value_name = "ENV")]
    pub env: Option<String>,
    /// Maximum rows.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
}

/// Arguments for `vowp reindex`.
#[derive(Debug, Clone, Args)]
pub struct ReindexArgs {
    /// Reindex one session only.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}

/// Arguments for `vowp backup`.
#[derive(Debug, Clone, Default, Args)]
pub struct BackupArgs {
    /// Destination file/directory or offsite target; a `.db` path is a global-only copy.
    #[arg(long, value_name = "PATH|TARGET")]
    pub to: Option<PathBuf>,
    /// Offsite rsync or git target.
    #[arg(long, value_name = "TARGET")]
    pub offsite: Option<String>,
    /// Encrypt with `age` to this recipient.
    #[arg(long, value_name = "RECIPIENT")]
    pub encrypt: Option<String>,
    /// Restore this backup directory into `--to-home`.
    #[arg(long, value_name = "DIR")]
    pub restore: Option<PathBuf>,
    /// Fresh home directory to restore into.
    #[arg(long, value_name = "DIR")]
    pub to_home: Option<PathBuf>,
    /// Overwrite an existing restore target.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `vowp schema`.
#[derive(Debug, Clone, Args)]
pub struct SchemaArgs {
    /// Schema type name; without one, list the names.
    pub type_name: Option<String>,
}

/// Arguments for the hidden `vowp __window`.
#[derive(Debug, Clone, Args)]
pub struct WindowArgs {
    /// Window kind (`board`, `supervisor`, `planner`, `keeper`, `inbox`, `capacity`, `job`).
    pub kind: String,
    /// Job id for `job`.
    pub id: Option<String>,
}

/// The up/down group handed to `cmd::up`.
#[derive(Debug, Clone)]
pub enum UpCommand {
    /// `vowp up`.
    Up(UpArgs),
    /// `vowp down`.
    Down(DownArgs),
}

/// The reindex/backup/schema/version group handed to `cmd::maint`.
#[derive(Debug, Clone)]
pub enum MaintCommand {
    /// `vowp reindex`.
    Reindex(ReindexArgs),
    /// `vowp backup`.
    Backup(BackupArgs),
    /// `vowp schema`.
    Schema(SchemaArgs),
    /// `vowp version`.
    Version,
}

/// The promote/rollback/releases group handed to `cmd::promote`.
#[derive(Debug, Clone)]
pub enum PromoteCommand {
    /// `vowp promote`.
    Promote(PromoteArgs),
    /// `vowp rollback`.
    Rollback(RollbackArgs),
    /// `vowp releases`.
    Releases(ReleasesArgs),
}
