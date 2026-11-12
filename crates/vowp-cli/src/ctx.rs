//! The per-command context: node paths, parsed config, dependencies and rendering flags.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §1–§2. [`Ctx::build`] never creates the node home:
//! a missing explicit home fails with `not found` and the hint to run `vowp init`, while a
//! control machine builds a context over `~/.vowp-control` and an uninitialised machine
//! builds an empty one so `vowp control init` can create the role.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use vowp_core::deps::SystemRng;
use vowp_core::{AdapterRegistry, Deps, SystemGit, SystemRunner};
use vowp_protocol::{Clock, NodeFile, NullLog, ProjectName, SystemClock, VowpConfig};
use vowp_store::{NodePaths, Store};
use vowp_tmux::{Tmux, WindowManager};

use crate::cli::Global;
use crate::node_ctx::{ControlHome, NodeCtx, NodeRole};
use crate::{CliError, IoStreams};

/// The keys a `[loops.<name>]` table may contain, matching `vowp_protocol::config`.
const LOOP_KEYS: [&str; 11] = [
    "interval_secs",
    "state",
    "max_continues",
    "stall_minutes",
    "silent_minutes",
    "max_hang_restarts",
    "same_action_max",
    "probe_interval_minutes",
    "planner_min_gap_minutes",
    "idle_gap_minutes",
    "checkpoint_minutes",
];

/// Everything a command handler needs.
pub struct Ctx {
    paths: NodePaths,
    deps: Deps,
    project: ProjectName,
    project_root: PathBuf,
    json: bool,
    color: bool,
    role: NodeRole,
    control: Option<ControlHome>,
    project_selected: bool,
}

impl Ctx {
    /// Builds the context from the global flags without creating the node home.
    ///
    /// A home keeps the v0.1 behaviour. A control machine gets a context bound to its
    /// `~/.vowp-control` directory so local commands (`control`, `tunnel`, `node`) work
    /// without a database; every other command is routed to the home by
    /// [`crate::remote::maybe_run`] before this function runs. An uninitialised machine
    /// gets an empty context so `vowp control init` can create the control role.
    ///
    /// Errors: `NotFound` when an explicit home has no node database or a selected
    /// checkout has no `vowp.toml`, and the config errors of `vowp.toml`. A home with
    /// zero or several projects and no `--project` builds a fleet context instead of
    /// failing; [`Ctx::store`] then refuses project-scoped work.
    pub fn build(global: &Global, io: &dyn IoStreams) -> Result<Ctx, CliError> {
        let node = NodeCtx::discover(global)?;
        let control = node.require_control().ok().cloned();
        let (role, paths) = match &control {
            Some(control) => (NodeRole::Control, NodePaths::new(&control.root)),
            None => {
                let paths = node_paths(global)?;
                let role = if paths.db_path().is_file() {
                    NodeRole::Home
                } else {
                    NodeRole::Uninitialised
                };
                (role, paths)
            }
        };
        let (config, project, project_root, project_selected) = match role {
            NodeRole::Home => match resolve_project_root(global, &paths) {
                Ok(project_root) => {
                    let config_path = project_root.join("vowp.toml");
                    if !config_path.is_file() {
                        return Err(CliError::NotFound(format!(
                            "project config {}; run `vowp init`",
                            config_path.display()
                        )));
                    }
                    let config = load_project_config(&config_path, &paths)?;
                    let project = config.project.name.clone();
                    (config, project, project_root, true)
                }
                Err(error) if global.project.is_none() && is_missing_project(&error) => {
                    // Fleet commands (status, board, fleet, capacity, gc, backup) do not
                    // need a project. A home with zero or several projects still gets a
                    // context; `Ctx::store` refuses project-scoped work until one is chosen.
                    let config = fleet_config(&paths)?;
                    let project = ProjectName::parse("_node")?;
                    (config, project, paths.root().to_path_buf(), false)
                }
                Err(error) => return Err(error),
            },
            NodeRole::Control => {
                let control = control
                    .as_ref()
                    .ok_or_else(|| CliError::Guard("this machine is not a control machine".to_owned()))?;
                let config = placeholder_config(&control.config.home)?;
                let project = ProjectName::parse("_node")?;
                (config, project, control.root.clone(), false)
            }
            NodeRole::Uninitialised => {
                if global.home.is_some() {
                    return Err(CliError::NotFound(
                        "vowp home is not initialised; run `vowp init`".to_owned(),
                    ));
                }
                let config = placeholder_config("home")?;
                let project = ProjectName::parse("_node")?;
                (config, project, paths.root().to_path_buf(), false)
            }
        };
        let config = Arc::new(config);
        let node_name = config.node.name.clone();

        let cmd: Arc<dyn vowp_core::CommandRunner> = Arc::new(SystemRunner);
        let git = Arc::new(SystemGit::new(Arc::clone(&cmd)));
        let windows: Arc<dyn WindowManager> = Arc::new(Tmux::for_home(paths.root()));
        let adapters = Arc::new(AdapterRegistry::from_config(&config)?);
        let deps = Deps {
            paths: paths.clone(),
            config,
            node: node_name,
            clock: Arc::new(SystemClock),
            log: Arc::new(NullLog),
            adapters,
            windows,
            git,
            cmd,
            rng: Arc::new(SystemRng::new()),
        };

        let color = color_enabled(global, io);
        Ok(Ctx {
            paths,
            deps,
            project,
            project_root,
            json: global.json,
            color,
            role,
            control,
            project_selected,
        })
    }

    /// Returns the node paths.
    pub fn paths(&self) -> &NodePaths {
        &self.paths
    }

    /// Returns the role discovered for this machine.
    pub fn role(&self) -> NodeRole {
        self.role
    }

    /// Returns the control configuration, when this is a control machine.
    pub fn control(&self) -> Option<&ControlHome> {
        self.control.as_ref()
    }

    /// Returns the injected dependencies.
    pub fn deps(&self) -> &Deps {
        &self.deps
    }

    /// Returns the parsed project config.
    pub fn config(&self) -> &VowpConfig {
        &self.deps.config
    }

    /// Returns the project name.
    pub fn project(&self) -> &ProjectName {
        &self.project
    }

    /// Returns the directory containing `vowp.toml`.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Returns the clock used by the supervisor.
    pub fn clock(&self) -> &dyn Clock {
        self.deps.clock.as_ref()
    }

    /// Returns whether a project was selected for this command.
    ///
    /// A home with zero or several projects and no `--project` gets a fleet context where
    /// this is false; project-scoped handlers must then ask for a project instead of
    /// writing to whichever database happens to be open.
    pub fn project_selected(&self) -> bool {
        self.project_selected
    }

    /// Opens a fresh project store connection.
    ///
    /// Only a home with a selected project holds project-scoped data. A control machine, an
    /// uninitialised machine and a fleet context return an error instead of opening the
    /// wrong database. Fleet read-model commands use [`Ctx::fleet_store`] instead.
    pub fn store(&self) -> Result<Store, CliError> {
        match self.role {
            NodeRole::Home if self.project_selected => Ok(self.deps.open_store()?),
            NodeRole::Home => Err(CliError::Invalid(
                "no project selected; pass --project <name> or run `vowp init`".to_owned(),
            )),
            NodeRole::Control => Err(CliError::NotFound(
                "this is a control machine; the home holds the database".to_owned(),
            )),
            NodeRole::Uninitialised => Err(CliError::NotFound(
                "vowp home is not initialised; run `vowp setup` or `vowp init`".to_owned(),
            )),
        }
    }

    /// Opens the global store used by the fleet read model.
    ///
    /// `status`, `board` and `fleet ...` accept every project (spec §4.5, §5.5 and §8.2), so
    /// they must work on a home with zero or several projects and without `--project`. The
    /// core read model opens each project database itself; this handle is never bound to a
    /// single project and must not be used for project writes.
    pub fn fleet_store(&self) -> Result<Store, CliError> {
        match self.role {
            NodeRole::Home => Ok(Store::open_global(&self.paths)?),
            NodeRole::Control => Err(CliError::NotFound(
                "this is a control machine; the home holds the database".to_owned(),
            )),
            NodeRole::Uninitialised => Err(CliError::NotFound(
                "vowp home is not initialised; run `vowp setup` or `vowp init`".to_owned(),
            )),
        }
    }

    /// Returns whether `--json` was passed.
    pub fn json(&self) -> bool {
        self.json
    }

    /// Returns whether human output may use ANSI colour.
    pub fn color(&self) -> bool {
        self.color
    }

    /// Returns the configured UTC offset in minutes.
    pub fn tz_offset_minutes(&self) -> i32 {
        self.deps.config.node.tz_offset_minutes
    }
}

/// Returns the node home from `--home`, `$VOWP_HOME`, or `~/.vowp`.
pub fn node_paths(global: &Global) -> Result<NodePaths, CliError> {
    match &global.home {
        Some(home) => Ok(NodePaths::new(home)),
        None => Ok(NodePaths::from_env()?),
    }
}

/// Returns whether human output may use ANSI colour.
///
/// Commands that run before [`Ctx`] exists (and therefore cannot ask the context) use
/// this instead, so `--no-color`, `NO_COLOR` and a non-TTY behave the same everywhere.
pub fn color_enabled(global: &Global, io: &dyn IoStreams) -> bool {
    !global.no_color && io.is_tty() && std::env::var_os("NO_COLOR").is_none()
}

/// Refuses a node-scoped command on a machine with no database.
///
/// `up`, `down`, `migrate` and `project` dispatch before [`Ctx::build`], because a v0.1
/// home cannot open its database until it has been migrated. They still keep the v0.1
/// "run `vowp init` first" error for an empty home.
pub fn require_initialised_home(paths: &NodePaths) -> Result<(), CliError> {
    if paths.db_path().is_file() {
        return Ok(());
    }
    Err(CliError::NotFound(
        "vowp home is not initialised; run `vowp init`".to_owned(),
    ))
}

/// Resolves the checkout a node-scoped command should register, if any.
///
/// `--project <path>` names a checkout directly; `--project <name>` is looked up in the
/// registry. Without `--project`, the nearest ancestor with a `vowp.toml` wins. The
/// reserved `_node` home file is not a project checkout, so it resolves to `None`.
///
/// Unlike [`resolve_project_root`] this never falls back to "the only registered
/// project": `up` runs the node supervisor for every project, so no project selection is
/// needed and a checkout is only registered when the caller actually stands in one.
pub fn selected_checkout_root(global: &Global, paths: &NodePaths) -> Result<Option<PathBuf>, CliError> {
    let candidate = if let Some(value) = &global.project {
        let candidate = PathBuf::from(value);
        if candidate.is_dir() {
            if !candidate.join("vowp.toml").is_file() {
                return Err(CliError::NotFound(format!(
                    "no vowp.toml in {}; run `vowp init`",
                    candidate.display()
                )));
            }
            Some(candidate)
        } else {
            let name = ProjectName::parse(value)?;
            let store = Store::open_global(paths)?;
            let project = store
                .project(&name)?
                .ok_or_else(|| CliError::NotFound(format!("project {name}; run `vowp init`")))?;
            let root = PathBuf::from(project.path);
            if !root.join("vowp.toml").is_file() {
                return Err(CliError::NotFound(format!(
                    "no vowp.toml in {}; run `vowp init`",
                    root.display()
                )));
            }
            Some(root)
        }
    } else {
        let cwd = std::env::current_dir()?;
        find_project_root(&cwd)
    };

    let Some(root) = candidate else {
        return Ok(None);
    };
    let config = load_project_config(&root.join("vowp.toml"), paths)?;
    if config.project.name.as_str() == vowp_store::RESERVED_NODE_PROJECT {
        return Ok(None);
    }
    Ok(Some(root))
}

/// Resolves the project root from `--project`, the cwd, or the only registered project.
fn resolve_project_root(global: &Global, paths: &NodePaths) -> Result<PathBuf, CliError> {
    if let Some(value) = &global.project {
        let candidate = PathBuf::from(value);
        if candidate.is_dir() {
            if candidate.join("vowp.toml").is_file() {
                return Ok(candidate);
            }
            return Err(CliError::NotFound(format!(
                "no vowp.toml in {}; run `vowp init`",
                candidate.display()
            )));
        }
        let name = ProjectName::parse(value)?;
        let store = Store::open(paths)?;
        let project = store
            .project(&name)?
            .ok_or_else(|| CliError::NotFound(format!("project {name}; run `vowp init`")))?;
        let root = PathBuf::from(project.path);
        if !root.join("vowp.toml").is_file() {
            return Err(CliError::NotFound(format!(
                "no vowp.toml in {}; run `vowp init`",
                root.display()
            )));
        }
        return Ok(root);
    }

    let cwd = std::env::current_dir()?;
    if let Some(root) = cwd_project_root(&cwd, paths)? {
        return Ok(root);
    }

    let store = Store::open(paths)?;
    let projects = store.list_projects()?;
    match projects.as_slice() {
        [project] => {
            let root = PathBuf::from(&project.path);
            if !root.join("vowp.toml").is_file() {
                return Err(CliError::NotFound(format!(
                    "no vowp.toml in {}; run `vowp init`",
                    root.display()
                )));
            }
            Ok(root)
        }
        [] => Err(CliError::Invalid("no project: run `vowp init`".to_owned())),
        _ => Err(CliError::Invalid(
            "several projects are registered; pass --project <name|path>".to_owned(),
        )),
    }
}

/// Returns the nearest checkout from `start` when it is registered in `paths`.
///
/// A `vowp.toml` found from the cwd must not override an explicit `--home`: the caller may
/// stand in a checkout registered in a different home (for example the repository while a
/// temporary home is selected). The checkout only selects its project when the selected
/// home has that project name at the same path; otherwise [`resolve_project_root`] falls
/// back to the home's own projects, so fleet commands keep their fleet context.
fn cwd_project_root(start: &Path, paths: &NodePaths) -> Result<Option<PathBuf>, CliError> {
    let Some(root) = find_project_root(start) else {
        return Ok(None);
    };
    let config = load_project_config(&root.join("vowp.toml"), paths)?;
    let store = Store::open(paths)?;
    let Some(project) = store.project(&config.project.name)? else {
        return Ok(None);
    };
    if same_checkout(&project.path, &root) {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

/// Returns whether two paths name the same checkout, resolving symlinks when possible.
fn same_checkout(registered: &str, root: &Path) -> bool {
    let registered = Path::new(registered);
    if registered == root {
        return true;
    }
    match (std::fs::canonicalize(registered), std::fs::canonicalize(root)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Walks from `start` to the filesystem root looking for `vowp.toml`.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        if directory.join("vowp.toml").is_file() {
            return Some(directory.to_path_buf());
        }
    }
    None
}

/// Walks from `start` to the filesystem root looking for `.git`.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        if directory.join(".git").exists() {
            return Some(directory.to_path_buf());
        }
    }
    None
}

/// Returns the live supervisor pid from `<home>/supervisor.lock`, if one holds the lock.
///
/// The lock file contains the pid written by `vowp up`; a stale file (no such process) is
/// treated as "not running". Liveness uses `kill -0`, which works on Linux and macOS: exit 0
/// or `EPERM` means the process exists.
pub fn supervisor_pid(paths: &NodePaths) -> Option<u32> {
    let text = std::fs::read_to_string(paths.root().join("supervisor.lock")).ok()?;
    let pid: u32 = text.lines().next()?.trim().parse().ok()?;
    (valid_pid(pid) && process_alive(pid)).then_some(pid)
}

/// Returns whether `pid` names a process this program may safely signal.
///
/// On platforms whose `pid_t` is signed 32-bit, `kill` parses large `u32` values as negative
/// ids. In particular, `u32::MAX` becomes `-1`, which addresses every process the caller may
/// signal. VOWP therefore refuses pid 0, pid 1 and every value above [`i32::MAX`].
fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Returns whether a process with `pid` exists.
///
/// `kill -0` sends no signal: exit 0 means the process exists, and `EPERM` means it exists
/// but belongs to another user. Invalid ids always count as not alive.
fn process_alive(pid: u32) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    match Command::new("kill")
        .env("LC_ALL", "C")
        .arg("-0")
        .arg(pid.to_string())
        .output()
    {
        Ok(output) if output.status.success() => true,
        Ok(output) => String::from_utf8_lossy(&output.stderr).contains("not permitted"),
        Err(_) => false,
    }
}

/// Returns whether a project-resolution error means "no project can be selected".
fn is_missing_project(error: &CliError) -> bool {
    match error {
        CliError::Invalid(message) => message.starts_with("no project") || message.starts_with("several projects"),
        _ => false,
    }
}

/// Builds a fleet context config from the node file only.
///
/// Fleet commands (`status`, `board`, `fleet`, `capacity`, `gc`, `backup`) must work on a
/// home from any directory. They load the node-wide `config.toml` and use the reserved
/// `_node` project name; project-scoped work is refused by [`Ctx::store`] until a project
/// is selected.
fn fleet_config(paths: &NodePaths) -> Result<VowpConfig, CliError> {
    let mut config: VowpConfig = toml::from_str("[project]\nname = \"_node\"\n")
        .map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;
    let node_file_path = paths.root().join("config.toml");
    if node_file_path.is_file() {
        let node_text = std::fs::read_to_string(&node_file_path)?;
        let node_file = NodeFile::from_toml(&node_text)?;
        config = config.with_node_defaults(&node_file);
    }
    Ok(config)
}

/// Builds the minimal config a non-home context needs.
///
/// Control and uninitialised machines have no `vowp.toml` to load. The placeholder only
/// supplies the node name and a valid project identity; no command that uses it may open
/// a store, which [`Ctx::store`] enforces.
fn placeholder_config(node_name: &str) -> Result<VowpConfig, CliError> {
    let name = match vowp_protocol::NodeName::parse(node_name) {
        Ok(name) => name,
        Err(_) => vowp_protocol::NodeName::parse("home")?,
    };
    let text = format!("[project]\nname = \"_node\"\n\n[node]\nname = \"{}\"\n", name.as_str());
    toml::from_str(&text).map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()).into())
}

/// Reads and validates `vowp.toml`, merging `~/.vowp/config.toml` defaults first.
///
/// `VowpConfig::from_toml` validates before callers can merge node defaults, so the CLI does
/// the documented order itself: parse, merge, default, validate. An omitted
/// `[project].path` defaults to the directory containing `vowp.toml`, so relative harness
/// mappings resolve under the project checkout instead of the caller's cwd. Unknown keys
/// inside `[loops.*]` are rejected here because the flattened `LoopBounds` cannot use
/// `deny_unknown_fields`.
pub fn load_project_config(path: &Path, paths: &NodePaths) -> Result<VowpConfig, CliError> {
    let text = std::fs::read_to_string(path)?;
    reject_unknown_loop_keys(&text)?;
    let mut config: VowpConfig =
        toml::from_str(&text).map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;

    let node_file_path = paths.root().join("config.toml");
    if node_file_path.is_file() {
        let node_text = std::fs::read_to_string(&node_file_path)?;
        let node_file = NodeFile::from_toml(&node_text)?;
        config = config.with_node_defaults(&node_file);
    }
    if config.project.path.is_none() {
        config.project.path = path.parent().map(|parent| parent.display().to_string());
    }
    config.validate()?;
    Ok(config)
}

/// Rejects unknown keys inside any `[loops.<name>]` table.
fn reject_unknown_loop_keys(text: &str) -> Result<(), CliError> {
    let raw: toml::Value =
        toml::from_str(text).map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;
    let Some(loops) = raw.get("loops").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let allowed: BTreeSet<&str> = LOOP_KEYS.into_iter().collect();
    for (name, value) in loops {
        let Some(entry) = value.as_table() else {
            continue;
        };
        for key in entry.keys() {
            if !allowed.contains(key.as_str()) {
                return Err(CliError::Protocol(vowp_protocol::ProtocolError::Config(format!(
                    "unknown key loops.{name}.{key}"
                ))));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use vowp_protocol::{Project, Timestamp, VowpConfig};

    /// Writes a lock file whose first line is `contents`.
    fn write_lock(home: &TempDir, contents: &str) {
        std::fs::write(home.path().join("supervisor.lock"), contents).expect("write supervisor.lock");
    }

    /// Creates a checkout whose `vowp.toml` names `name`.
    fn checkout(name: &str) -> TempDir {
        let dir = tempfile::tempdir().expect("temp checkout");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = ProjectName::parse(name).expect("project name");
        config.project.path = Some(dir.path().display().to_string());
        std::fs::write(
            dir.path().join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config serialises"),
        )
        .expect("write vowp.toml");
        dir
    }

    /// Creates a home registry with one project row for `checkout`.
    fn home_with(checkout: &TempDir) -> (TempDir, NodePaths) {
        let home = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(home.path());
        let store = Store::open(&paths).expect("store opens");
        let text = std::fs::read_to_string(checkout.path().join("vowp.toml")).expect("read config");
        let config = VowpConfig::from_toml(&text).expect("valid config");
        let now = Timestamp::now();
        store
            .upsert_project(&Project {
                name: config.project.name.clone(),
                path: checkout.path().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        (home, paths)
    }

    #[test]
    fn cwd_checkout_must_match_the_selected_homes_name_and_path() {
        let registered = checkout("alpha");
        let (_home, paths) = home_with(&registered);
        assert_eq!(
            cwd_project_root(registered.path(), &paths).expect("resolve registered"),
            Some(registered.path().to_path_buf()),
            "a checkout registered in this home is selected"
        );

        let same_name = checkout("alpha");
        assert_eq!(
            cwd_project_root(same_name.path(), &paths).expect("resolve same name"),
            None,
            "the same project name at another path belongs to a different home"
        );

        let unregistered = checkout("gamma");
        assert_eq!(
            cwd_project_root(unregistered.path(), &paths).expect("resolve unregistered"),
            None,
            "an unregistered checkout must not be selected"
        );
    }

    #[test]
    fn supervisor_pid_none_for_dead_pid() {
        let home = tempfile::tempdir().expect("temp home");
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().expect("spawn child");
        let pid = child.id();
        let status = child.wait().expect("reap child");
        assert!(status.success(), "child exits 0");

        write_lock(&home, &format!("{pid}\n"));
        assert_eq!(
            supervisor_pid(&NodePaths::new(home.path())),
            None,
            "the reaped pid {pid} must count as not running"
        );
    }

    #[test]
    fn supervisor_pid_some_for_live_pid() {
        let home = tempfile::tempdir().expect("temp home");
        let pid = std::process::id();
        write_lock(&home, &format!("{pid}\n"));

        assert_eq!(supervisor_pid(&NodePaths::new(home.path())), Some(pid));
    }

    #[test]
    fn supervisor_pid_none_for_invalid_pid() {
        let home = tempfile::tempdir().expect("temp home");
        for pid in [0, 1, u32::MAX] {
            write_lock(&home, &format!("{pid}\n"));
            assert_eq!(
                supervisor_pid(&NodePaths::new(home.path())),
                None,
                "pid {pid} must count as not running"
            );
        }
    }

    #[test]
    fn supervisor_pid_none_when_lock_is_missing_or_malformed() {
        let home = tempfile::tempdir().expect("temp home");
        let paths = NodePaths::new(home.path());
        assert_eq!(supervisor_pid(&paths), None, "a missing lock is not running");

        for text in ["", "not-a-pid\n", "-1\n", "4294967296\n"] {
            write_lock(&home, text);
            assert_eq!(supervisor_pid(&paths), None, "malformed lock {text:?}");
        }
    }
}
