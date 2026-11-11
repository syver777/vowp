//! `vowp project` — the project registry.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §3.9 and §4.6. One home runs one
//! supervisor for every active project; these commands manage the registry it reads.
//! `project add` validates a checkout with a `vowp.toml`, registers it, creates its
//! database, loop rows and worker slots, and never starts a supervisor itself.
//!
//! The commands read and write the global registry only. They therefore work from any
//! directory, including a home with several projects and no `vowp.toml` in the cwd.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use vowp_core::Deps;
use vowp_core::loops::prepare_project;
use vowp_core::loops::registry;
use vowp_protocol::{Project, ProjectEntry, ProjectName, ProjectState};
use vowp_store::{NodePaths, Store};
use vowp_tmux::session_name_for_project;

use crate::cli::Global;
use crate::output::{Table, print_json};
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp project`.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectArgs {
    /// The project subcommand.
    #[command(subcommand)]
    pub command: ProjectCommand,
}

/// The `vowp project` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum ProjectCommand {
    /// Register a git checkout as a project.
    Add(ProjectAddArgs),
    /// List registered projects.
    Ls(ProjectLsArgs),
    /// Stop new work in a project.
    Pause(ProjectNameArgs),
    /// Resume a paused project.
    Resume(ProjectNameArgs),
    /// Archive a project.
    Archive(ProjectNameArgs),
    /// Archive a project's database and remove its registry row.
    Rm(ProjectRmArgs),
    /// Set a project's fairness weight.
    Weight(ProjectWeightArgs),
}

/// Arguments for `vowp project add`.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectAddArgs {
    /// Path of the checkout on the home.
    pub path: PathBuf,
    /// Project name; defaults to the directory name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Fairness weight, `1..=100`.
    #[arg(long, value_name = "N")]
    pub weight: Option<u8>,
}

/// Arguments for `vowp project ls`.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectLsArgs {
    /// Include the reserved `_node` project.
    #[arg(long)]
    pub all: bool,
}

/// Arguments carrying only a project name.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectNameArgs {
    /// Project name.
    pub name: String,
}

/// Arguments for `vowp project rm`.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectRmArgs {
    /// Project name.
    pub name: String,
    /// Confirm the archive.
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for `vowp project weight`.
#[derive(Debug, Clone, clap::Args)]
pub struct ProjectWeightArgs {
    /// Project name.
    pub name: String,
    /// Fairness weight, `1..=100`.
    pub weight: u8,
}

/// The node-level context a registry command needs.
struct Registry<'a> {
    paths: &'a NodePaths,
    deps: &'a Deps,
    json: bool,
    color: bool,
}

/// Runs `vowp project` with a project context.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &ProjectArgs) -> Result<i32, CliError> {
    let registry = Registry {
        paths: ctx.paths(),
        deps: ctx.deps(),
        json: ctx.json(),
        color: ctx.color(),
    };
    dispatch(&registry, io, args)
}

/// Runs `vowp project` before a project context exists.
///
/// Registry commands act on the node, not on the selected checkout: they must work from
/// any directory, with several registered projects and on a home whose only context is
/// the node-level `vowp.toml`. Building the node dependencies directly also keeps
/// `project add` independent of the checkout the caller happens to stand in.
pub fn run_node(global: &Global, io: &mut dyn IoStreams, args: &ProjectArgs) -> Result<i32, CliError> {
    let paths = crate::ctx::node_paths(global)?;
    crate::ctx::require_initialised_home(&paths)?;
    let deps = crate::node_ctx::home_shell_deps(&paths)?;
    let registry = Registry {
        paths: &paths,
        deps: &deps,
        json: global.json,
        color: crate::ctx::color_enabled(global, io),
    };
    dispatch(&registry, io, args)
}

/// Runs one registry subcommand.
fn dispatch(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectArgs) -> Result<i32, CliError> {
    match &args.command {
        ProjectCommand::Add(args) => add(registry, io, args),
        ProjectCommand::Ls(args) => list(registry, io, args),
        ProjectCommand::Pause(args) => pause(registry, io, args),
        ProjectCommand::Resume(args) => resume(registry, io, args),
        ProjectCommand::Archive(args) => archive(registry, io, args),
        ProjectCommand::Rm(args) => remove(registry, io, args),
        ProjectCommand::Weight(args) => weight(registry, io, args),
    }
}

/// Result of `project rm` (the public `ProjectRemoved` shape).
#[derive(Debug, Serialize)]
struct ProjectRemoved {
    name: String,
    archive_path: String,
    removed: bool,
}

/// Registers one checkout.
fn add(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectAddArgs) -> Result<i32, CliError> {
    let (entry, created) = register_checkout(
        registry.paths,
        registry.deps,
        &args.path,
        args.name.as_deref(),
        args.weight,
    )?;
    print_entry(
        registry,
        io,
        &entry,
        if created { "registered" } else { "already registered" },
    )?;
    Ok(0)
}

/// Registers `path` as a project, idempotently.
///
/// Returns the registry entry and whether the row was new. `vowp project add` and the
/// v0.1-compatible `vowp up` path share this function, so a checkout with a `vowp.toml`
/// becomes a project before the node supervisor reads the registry.
pub fn register_checkout(
    paths: &NodePaths,
    base: &Deps,
    path: &Path,
    name: Option<&str>,
    weight: Option<u8>,
) -> Result<(ProjectEntry, bool), CliError> {
    let root = validate_checkout(path)?;
    let config = crate::ctx::load_project_config(&root.join("vowp.toml"), paths)?;
    if let Some(name) = name
        && ProjectName::parse(name)? != config.project.name
    {
        return Err(CliError::Invalid(format!(
            "{} names project {}; --name {name} would make the supervisor reject it",
            root.join("vowp.toml").display(),
            config.project.name
        )));
    }
    let project = config.project.name.clone();
    if project.as_str() == vowp_store::RESERVED_NODE_PROJECT {
        return Err(CliError::Invalid(
            "`_node` is reserved and cannot be registered as a user project".to_owned(),
        ));
    }
    if let Some(weight) = weight
        && !(1..=100).contains(&weight)
    {
        return Err(CliError::Invalid(format!(
            "project weight must be 1..=100, got {weight}"
        )));
    }

    let store = Store::open_global(paths)?;
    let now = store.clock().now();
    let config_json = serde_json::to_value(&config).map_err(vowp_protocol::ProtocolError::from)?;
    let created = match store.project(&project)? {
        Some(existing) => {
            if !same_path(&existing.path, &root) {
                return Err(CliError::Conflict(format!(
                    "project {project} is already registered at {}",
                    existing.path
                )));
            }
            // `upsert_project` refreshes the checkout columns but leaves the registry
            // columns (state, weight, pause reason, archive time) untouched, so re-adding
            // the same checkout keeps a pause, archive or owner-chosen weight.
            store.upsert_project(&Project {
                name: project.clone(),
                path: root.display().to_string(),
                remote_url: remote_url(&root),
                default_branch: config.project.default_branch.clone(),
                config: config_json,
                created_at: existing.created_at,
                updated_at: now,
            })?;
            false
        }
        None => {
            if let Some(existing) = registry::project_by_path(&store, &root)? {
                return Err(CliError::Conflict(format!(
                    "{} is already registered as project {}",
                    root.display(),
                    existing.name
                )));
            }
            store.upsert_project(&Project {
                name: project.clone(),
                path: root.display().to_string(),
                remote_url: remote_url(&root),
                default_branch: config.project.default_branch.clone(),
                config: config_json,
                created_at: now,
                updated_at: now,
            })?;
            true
        }
    };
    if let Some(weight) = weight {
        registry::set_project_weight(&store, &project, weight)?;
    }
    Store::create_project_db(paths, &project)?;
    prepare(base, project.clone(), config)?;
    Ok((require_entry(&store, &project)?, created))
}

/// Lists the registry.
fn list(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectLsArgs) -> Result<i32, CliError> {
    let store = Store::open_global(registry.paths)?;
    let entries: Vec<ProjectEntry> = registry::project_entries(&store)?
        .into_iter()
        .filter(|entry| args.all || entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT)
        .collect();
    if registry.json {
        print_json(io, &entries)?;
        return Ok(0);
    }
    let mut table = Table::new(&["NAME", "STATE", "WEIGHT", "PATH", "UPDATED"]);
    for entry in &entries {
        table.row(vec![
            entry.name.to_string(),
            state_name(entry.state).to_owned(),
            entry.weight.to_string(),
            entry.path.clone(),
            entry.updated_at.as_rfc3339(),
        ]);
    }
    writeln!(io.out(), "{}", table.render(registry.color))?;
    Ok(0)
}

/// Pauses a project: running work finishes, new work does not start.
fn pause(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectNameArgs) -> Result<i32, CliError> {
    let store = Store::open_global(registry.paths)?;
    let name = user_project(&args.name)?;
    let entry = require_entry(&store, &name)?;
    if entry.state == ProjectState::Archived {
        return Err(CliError::Conflict(format!("project {name} is archived")));
    }
    let entry = registry::set_project_state(&store, &name, ProjectState::Paused, "owner")?;
    print_entry(registry, io, &entry, "paused")?;
    Ok(0)
}

/// Resumes a paused project. Archived projects stay archived.
fn resume(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectNameArgs) -> Result<i32, CliError> {
    let store = Store::open_global(registry.paths)?;
    let name = user_project(&args.name)?;
    let entry = require_entry(&store, &name)?;
    if entry.state == ProjectState::Archived {
        return Err(CliError::Conflict(format!(
            "project {name} is archived and cannot be resumed"
        )));
    }
    let entry = registry::set_project_state(&store, &name, ProjectState::Active, "")?;
    print_entry(registry, io, &entry, "resumed")?;
    Ok(0)
}

/// Archives a project: it keeps its database and gets no more ticks.
fn archive(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectNameArgs) -> Result<i32, CliError> {
    let store = Store::open_global(registry.paths)?;
    let name = user_project(&args.name)?;
    require_entry(&store, &name)?;
    let entry = registry::set_project_state(&store, &name, ProjectState::Archived, "owner")?;
    kill_project_session(registry, &name)?;
    print_entry(registry, io, &entry, "archived")?;
    Ok(0)
}

/// Archives a project's database directory and removes its registry row.
fn remove(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectRmArgs) -> Result<i32, CliError> {
    let store = Store::open_global(registry.paths)?;
    let name = user_project(&args.name)?;
    require_entry(&store, &name)?;
    if !args.yes {
        return Err(CliError::Guard(format!(
            "refusing to remove project {name} without --yes"
        )));
    }
    if project_is_running(registry, &name)? {
        return Err(CliError::Conflict(format!(
            "project {name} has running work; pause it and wait for the work to finish"
        )));
    }

    let source = registry.paths.project_dir(&name);
    let archive_path = if source.exists() {
        let stamp = store.clock().now().unix_millis();
        let archive_root = registry.paths.root().join("archive/projects");
        std::fs::create_dir_all(&archive_root)?;
        let target = archive_root.join(format!("{name}-{stamp}"));
        std::fs::rename(&source, &target)?;
        target.display().to_string()
    } else {
        String::new()
    };
    registry::remove_project_record(&store, &name)?;
    kill_project_session(registry, &name)?;
    if registry.json {
        print_json(
            io,
            &ProjectRemoved {
                name: name.to_string(),
                archive_path: archive_path.clone(),
                removed: true,
            },
        )?;
    } else if archive_path.is_empty() {
        writeln!(io.out(), "removed project {name}")?;
    } else {
        writeln!(io.out(), "removed project {name}; database archived to {archive_path}")?;
    }
    Ok(0)
}

/// Sets one project's fairness weight.
fn weight(registry: &Registry<'_>, io: &mut dyn IoStreams, args: &ProjectWeightArgs) -> Result<i32, CliError> {
    if !(1..=100).contains(&args.weight) {
        return Err(CliError::Invalid(format!(
            "project weight must be 1..=100, got {}",
            args.weight
        )));
    }
    let store = Store::open_global(registry.paths)?;
    let name = user_project(&args.name)?;
    require_entry(&store, &name)?;
    registry::set_project_weight(&store, &name, args.weight)?;
    let entry = require_entry(&store, &name)?;
    print_entry(registry, io, &entry, "weight set")?;
    Ok(0)
}

/// Prints one registry entry as JSON or as a one-line human message.
fn print_entry(
    registry: &Registry<'_>,
    io: &mut dyn IoStreams,
    entry: &ProjectEntry,
    verb: &str,
) -> Result<(), CliError> {
    if registry.json {
        print_json(io, entry)?;
    } else {
        writeln!(
            io.out(),
            "{verb}: {} ({}, weight {}, {})",
            entry.name,
            state_name(entry.state),
            entry.weight,
            entry.path
        )?;
    }
    Ok(())
}

/// Returns the entry or a not-found error.
fn require_entry(store: &Store, name: &ProjectName) -> Result<ProjectEntry, CliError> {
    registry::project_entry(store, name)?.ok_or_else(|| CliError::NotFound(format!("project {name}")))
}

/// Parses a user-supplied project name and refuses the reserved `_node`.
fn user_project(value: &str) -> Result<ProjectName, CliError> {
    let name = ProjectName::parse(value)?;
    if name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
        return Err(CliError::Invalid(
            "`_node` is reserved and cannot be changed with `vowp project`".to_owned(),
        ));
    }
    Ok(name)
}

/// Validates that `path` is a git checkout with a `vowp.toml` and returns its absolute root.
fn validate_checkout(path: &Path) -> Result<PathBuf, CliError> {
    if !path.is_dir() {
        return Err(CliError::Invalid(format!("{} is not a directory", path.display())));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .map_err(CliError::Io)?;
    if !output.status.success() {
        return Err(CliError::Invalid(format!(
            "{} is not a git checkout: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join("vowp.toml").is_file() {
        return Err(CliError::Invalid(format!(
            "no vowp.toml in {}; run `vowp init` there",
            root.display()
        )));
    }
    Ok(root)
}

/// Returns `remote.origin.url` when the checkout has one.
fn remote_url(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "remote.origin.url"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Builds project-scoped dependencies and creates the project's loops and worker slots.
fn prepare(base: &Deps, project: ProjectName, config: vowp_protocol::VowpConfig) -> Result<(), CliError> {
    let mut deps = base.clone();
    deps.adapters = Arc::new(vowp_core::AdapterRegistry::from_config(&config)?);
    deps.config = Arc::new(config);
    prepare_project(&deps, &project)?;
    Ok(())
}

/// Returns whether the project has an open run or a busy worker slot.
fn project_is_running(registry: &Registry<'_>, name: &ProjectName) -> Result<bool, CliError> {
    let store = match Store::open_with_project(registry.paths, name) {
        Ok(store) => store,
        Err(vowp_store::StoreError::NotFound { .. }) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !store.open_runs()?.is_empty() {
        return Ok(true);
    }
    let node = store
        .local_node()?
        .map(|node| node.name)
        .unwrap_or_else(|| registry.deps.node.clone());
    Ok(store
        .list_workers(&node)?
        .iter()
        .any(|worker| worker.state == vowp_protocol::WorkerState::Busy))
}

/// Removes the project's tmux session.
///
/// `Tmux::kill_session` refuses a session with a foreign untagged window (and a server
/// `vowp up` did not record). In those cases only the tagged views are removed, exactly
/// as the old window-by-window cleanup did, and the session is left to its owner.
fn kill_project_session(registry: &Registry<'_>, name: &ProjectName) -> Result<(), CliError> {
    let session = session_name_for_project(name.as_str());
    match registry.deps.windows.kill_session(&session) {
        Ok(()) | Err(vowp_tmux::TmuxError::NotInstalled) => return Ok(()),
        Err(vowp_tmux::TmuxError::NotTagged { .. } | vowp_tmux::TmuxError::Refused(_)) => {}
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    }
    let windows = match registry.deps.windows.list_windows(&session) {
        Ok(windows) => windows,
        Err(vowp_tmux::TmuxError::NotInstalled) => return Ok(()),
        Err(error) => return Err(CliError::Core(vowp_core::CoreError::Tmux(error))),
    };
    for window in windows {
        if window.tagged {
            let _ = registry.deps.windows.kill_window(&session, &window.name);
        }
    }
    Ok(())
}

/// Returns whether two checkouts name the same path, resolving symlinks when possible.
fn same_path(left: &str, right: &Path) -> bool {
    let left = Path::new(left);
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Returns the wire/display name of a registry state.
fn state_name(state: ProjectState) -> &'static str {
    match state {
        ProjectState::Active => "active",
        ProjectState::Paused => "paused",
        ProjectState::Archived => "archived",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vowp_core::SystemGit;
    use vowp_core::deps::{AdapterRegistry, FixedRng, SystemRunner};
    use vowp_protocol::{FakeClock, MemoryLog, NodeName, Timestamp, VowpConfig};
    use vowp_tmux::fake::FakeWindows;
    use vowp_tmux::{LayoutSpec, WindowManager};

    use super::*;

    fn layout() -> LayoutSpec {
        let argv = || vec!["sh".to_owned(), "-c".to_owned(), "exec cat".to_owned()];
        LayoutSpec {
            board: argv(),
            supervisor: argv(),
            planner: argv(),
            keeper: argv(),
            inbox: argv(),
            cwd: None,
        }
    }

    #[test]
    fn kill_project_session_removes_the_session_and_keeps_foreign_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path().join("node"));
        let windows = Arc::new(FakeWindows::new());
        let clock = Arc::new(FakeClock::at(
            Timestamp::parse("2026-11-10T10:00:00Z").expect("timestamp"),
        ));
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: NodeName::parse("local").expect("node"),
            clock,
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: windows.clone(),
            git: Arc::new(SystemGit::new(Arc::new(SystemRunner))),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        let registry = Registry {
            paths: &paths,
            deps: &deps,
            json: false,
            color: false,
        };
        let project = ProjectName::parse("demo").expect("project");

        // A normal project session disappears as a whole.
        windows.ensure_layout("demo", &layout()).expect("layout");
        assert!(windows.has_session("vowp-demo"));
        kill_project_session(&registry, &project).expect("kill");
        assert!(
            !windows.has_session("vowp-demo"),
            "archive/rm must call kill_session, not only kill each window"
        );

        // A foreign untagged window belongs to someone else: the session stays, but the
        // tagged views are removed.
        windows.ensure_layout("demo", &layout()).expect("layout again");
        windows.untagged_window("vowp-demo", "foreign");
        kill_project_session(&registry, &project).expect("fallback kill");
        assert!(windows.has_session("vowp-demo"), "foreign session is not destroyed");
        assert!(windows.window_exists("vowp-demo", "foreign").expect("foreign"));
        assert!(!windows.window_exists("vowp-demo", "board").expect("board"));
    }
}
