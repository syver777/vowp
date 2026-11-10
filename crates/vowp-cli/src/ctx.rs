//! The per-command context: node paths, parsed config, dependencies and rendering flags.
//!
//! Spec: `docs/design/crates/vowp-cli.md` §1–§2. [`Ctx::build`] never creates the node home:
//! commands other than `init` fail with `not found` and the hint to run `vowp init`.

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
}

impl Ctx {
    /// Builds the context from the global flags without creating the node home.
    ///
    /// Errors: `NotFound` when the node database or project config is missing,
    /// `InvalidInput` when no project can be selected.
    pub fn build(global: &Global, io: &dyn IoStreams) -> Result<Ctx, CliError> {
        let paths = node_paths(global)?;
        if !paths.db_path().is_file() {
            return Err(CliError::NotFound(
                "vowp home is not initialised; run `vowp init`".to_owned(),
            ));
        }
        let project_root = resolve_project_root(global, &paths)?;
        let config_path = project_root.join("vowp.toml");
        if !config_path.is_file() {
            return Err(CliError::NotFound(format!(
                "project config {}; run `vowp init`",
                config_path.display()
            )));
        }
        let config = Arc::new(load_project_config(&config_path, &paths)?);
        let project = config.project.name.clone();
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

        let color = !global.no_color && io.is_tty() && std::env::var_os("NO_COLOR").is_none();
        Ok(Ctx {
            paths,
            deps,
            project,
            project_root,
            json: global.json,
            color,
        })
    }

    /// Returns the node paths.
    pub fn paths(&self) -> &NodePaths {
        &self.paths
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

    /// Opens a fresh store connection.
    pub fn store(&self) -> Result<Store, CliError> {
        Ok(self.deps.open_store()?)
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
    if let Some(root) = find_project_root(&cwd) {
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

/// Reads and validates `vowp.toml`, merging `~/.vowp/config.toml` defaults first.
///
/// `VowpConfig::from_toml` validates before callers can merge node defaults, so the CLI does
/// the documented order itself: parse, merge, validate. Unknown keys inside `[loops.*]`
/// are rejected here because the flattened `LoopBounds` cannot use `deny_unknown_fields`.
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

    /// Writes a lock file whose first line is `contents`.
    fn write_lock(home: &TempDir, contents: &str) {
        std::fs::write(home.path().join("supervisor.lock"), contents).expect("write supervisor.lock");
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
