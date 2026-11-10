//! clap definitions for every v0.1 command.
//!
//! Spec: `docs/design/cli.md`. The structs are shared by the command handlers; their
//! argument names are stable.
//!
//! `Command` contains every command the v0.1 binary exposes, including commands whose
//! handlers are still explicit [`crate::CliError::NotImplemented`] stubs.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Every top-level command the v0.1 CLI exposes, in `cli.md` order.
pub const V0_1_COMMANDS: &[&str] = &[
    "init", "doctor", "up", "down", "status", "board", "plan", "job", "graph", "attach", "session", "chat", "inbox",
    "cost", "node", "loops", "schedule", "promote", "rollback", "releases", "reindex", "backup", "schema", "version",
    "__window",
];

/// The parsed command line.
#[derive(Debug, Parser)]
#[command(
    name = "vowp",
    version,
    about = "VOWP: VPS-first supervisor for AI coding agents",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Global flags accepted before or after the command.
    #[command(flatten)]
    pub global: Global,
    /// Command to run.
    #[command(subcommand)]
    pub command: Command,
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
    /// List or pause supervisor loops.
    Loops(LoopsArgs),
    /// Manage scheduled actions.
    Schedule {
        /// The schedule subcommand.
        #[command(subcommand)]
        command: ScheduleCommand,
    },
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
    /// Print the binary version.
    Version,
    /// Hidden: what the tmux windows run.
    #[command(name = "__window", hide = true)]
    Window(WindowArgs),
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
    /// Run exactly one supervisor tick and exit.
    #[arg(long)]
    pub once: bool,
}

/// Arguments for `vowp down`.
#[derive(Debug, Clone, Args)]
pub struct DownArgs {
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
    /// Export a session bundle.
    Export(SessionExportArgs),
    /// Import a session bundle.
    Import(SessionImportArgs),
    /// Record interactive Claude Code transcripts.
    Watch(SessionWatchArgs),
    /// Print a session's event log.
    Tail(SessionTailArgs),
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
#[derive(Debug, Clone, Args)]
pub struct ChatArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Switch to this executor before the first turn.
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: Option<String>,
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

/// Arguments for `vowp session export`.
#[derive(Debug, Clone, Args)]
pub struct SessionExportArgs {
    /// Session id or unique prefix.
    pub id: String,
    /// Output file (default `./<id>.vowp-bundle.json`).
    #[arg(long, short = 'o', value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Number of tail events to include.
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,
}

/// Arguments for `vowp session import`.
#[derive(Debug, Clone, Args)]
pub struct SessionImportArgs {
    /// Bundle file, or `-` for stdin.
    pub file: String,
    /// Node to import on.
    #[arg(long, value_name = "NAME")]
    pub node: Option<String>,
    /// Executor for the child session.
    #[arg(long, value_name = "EXECUTOR")]
    pub executor: Option<String>,
    /// Working directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
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
}

/// Arguments for `vowp loops`.
#[derive(Debug, Clone, Args)]
pub struct LoopsArgs {
    /// Optional pause/resume action.
    #[command(subcommand)]
    pub command: Option<LoopsCommand>,
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
#[derive(Debug, Clone, Args)]
pub struct BackupArgs {
    /// Destination file.
    #[arg(long, value_name = "FILE")]
    pub to: Option<PathBuf>,
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
    /// Window kind (`board`, `supervisor`, `planner`, `keeper`, `inbox`, `job`).
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
