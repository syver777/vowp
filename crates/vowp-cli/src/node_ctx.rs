//! Node role discovery: home, control machine or uninitialised.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §8.1–§8.2 and §13.6.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vowp_core::deps::SystemRng;
use vowp_core::{AdapterRegistry, Deps, SystemGit, SystemRunner};
use vowp_protocol::{Clock, NodeFile, NullLog, ProjectName, Snapshot, SystemClock, Timestamp, VowpConfig};
use vowp_store::{NodePaths, Store};
use vowp_tmux::Tmux;

use crate::CliError;
use crate::cli::Global;

/// Which side of a home/control pair this machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeRole {
    /// The machine that holds the database and runs the supervisor.
    Home,
    /// A machine that reaches a home over SSH.
    Control,
    /// No role has been configured yet.
    Uninitialised,
}

/// The `control.json` document of a control machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlConfig {
    /// Always `control`.
    pub role: String,
    /// Node name of the home.
    pub home: String,
    /// SSH destination of the home.
    pub ssh_target: String,
    /// Absolute home path on the remote machine.
    pub remote_home: String,
    /// When the control machine was configured.
    pub created_at: Timestamp,
}

/// The control-machine directory and its config.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlHome {
    /// `~/.vowp-control` (or `$VOWP_CONTROL_HOME`).
    pub root: PathBuf,
    /// Parsed `control.json`.
    pub config: ControlConfig,
}

/// A home reached over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHome {
    /// Home node name.
    pub name: String,
    /// SSH destination.
    pub ssh_target: String,
    /// Absolute home path on the remote machine.
    pub remote_home: String,
}

/// A snapshot read from the control cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedSnapshot {
    /// The snapshot.
    pub snapshot: Snapshot,
    /// When it was fetched.
    pub fetched_at: Timestamp,
    /// `remote` or `cache`.
    pub source: String,
}

/// The resolved role of this machine and its node paths.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCtx {
    role: NodeRole,
    paths: NodePaths,
    control: Option<ControlHome>,
    target: Option<RemoteHome>,
}

impl NodeCtx {
    /// Resolves the role from flags, `VOWP_CONTROL_HOME`, `VOWP_HOME` and the filesystem.
    pub fn discover(global: &Global) -> Result<NodeCtx, CliError> {
        if let Some(home) = &global.home {
            return Ok(Self {
                role: NodeRole::Home,
                paths: NodePaths::new(home),
                control: None,
                target: None,
            });
        }
        if let Some(control) = read_control_home()? {
            let target = RemoteHome {
                name: control.config.home.clone(),
                ssh_target: control.config.ssh_target.clone(),
                remote_home: control.config.remote_home.clone(),
            };
            return Ok(Self {
                role: NodeRole::Control,
                paths: NodePaths::new(&control.root),
                control: Some(control),
                target: Some(target),
            });
        }
        let paths = crate::ctx::node_paths(global)?;
        let vowp_home = std::env::var_os("VOWP_HOME").is_some_and(|value| !value.is_empty());
        let role = if vowp_home || paths.db_path().is_file() {
            NodeRole::Home
        } else {
            NodeRole::Uninitialised
        };
        Ok(Self {
            role,
            paths,
            control: None,
            target: None,
        })
    }

    /// Returns the node paths, requiring the home role.
    pub fn require_home(&self) -> Result<&NodePaths, CliError> {
        match self.role {
            NodeRole::Home => Ok(&self.paths),
            NodeRole::Control => Err(CliError::Guard(
                "this command needs the home; run it on the machine that holds the node".to_owned(),
            )),
            NodeRole::Uninitialised => Err(CliError::NotFound(
                "vowp home is not initialised; run `vowp setup` or `vowp init`".to_owned(),
            )),
        }
    }

    /// Returns the control configuration, requiring the control role.
    pub fn require_control(&self) -> Result<&ControlHome, CliError> {
        self.control
            .as_ref()
            .ok_or_else(|| CliError::Guard("this machine is not a control machine".to_owned()))
    }

    /// Returns the remote home, when this is a control machine.
    pub fn remote(&self) -> Option<&RemoteHome> {
        self.target.as_ref()
    }

    /// Writes the control snapshot cache.
    ///
    /// A home has no cache: the snapshot it reads is always fresh, so this is a no-op
    /// there and refuses only on an uninitialised machine.
    pub fn cache_snapshot(&self, snapshot: &Snapshot) -> Result<(), CliError> {
        let Some(control) = &self.control else {
            if self.role == NodeRole::Home {
                return Ok(());
            }
            return Err(CliError::Guard(
                "no control home is configured; run `vowp control init`".to_owned(),
            ));
        };
        let cached = CachedSnapshot {
            snapshot: snapshot.clone(),
            fetched_at: SystemClock.now(),
            source: "cache".to_owned(),
        };
        let text = serde_json::to_string_pretty(&cached)
            .map_err(|error| CliError::Invalid(format!("snapshot cache: {error}")))?;
        std::fs::create_dir_all(&control.root)?;
        let path = control.root.join("snapshot-cache.json");
        let temp = control.root.join(".snapshot-cache.json.tmp");
        std::fs::write(&temp, format!("{text}\n"))?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Reads the control snapshot cache.
    pub fn cached_snapshot(&self) -> Result<Option<CachedSnapshot>, CliError> {
        let Some(control) = &self.control else {
            return Ok(None);
        };
        let path = control.root.join("snapshot-cache.json");
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let mut cached: CachedSnapshot =
            serde_json::from_str(&text).map_err(|error| CliError::Invalid(format!("{}: {error}", path.display())))?;
        cached.source = "cache".to_owned();
        Ok(Some(cached))
    }

    /// Routes one command to the home and returns its JSON stdout.
    ///
    /// The control-home SSH transport is not implemented in this build; the signature stays
    /// here so the control commands can be written against it without touching this file.
    pub fn route_json(&self, _args: &[String]) -> Result<String, CliError> {
        Err(CliError::NotImplemented("not implemented in this build"))
    }
}

/// Returns the configured control home, when this machine has one.
pub fn control_home() -> Result<Option<ControlHome>, CliError> {
    read_control_home()
}

/// Returns whether this machine is configured as a control machine.
pub fn is_control_machine() -> Result<bool, CliError> {
    Ok(read_control_home()?.is_some())
}

/// Returns the project a command should act on.
///
/// A command explicitly bound to a project uses it. A command running from the node's
/// home uses the only real project when there is exactly one; with several projects it
/// refuses and names them, so the caller must pass `--project`. With no real project the
/// reserved `_node` project is used.
pub fn selected_project(ctx: &crate::Ctx) -> Result<ProjectName, CliError> {
    if ctx.project().as_str() != vowp_store::RESERVED_NODE_PROJECT {
        return Ok(ctx.project().clone());
    }
    let global = Store::open_global(ctx.paths())?;
    let mut projects: Vec<ProjectName> = vowp_core::loops::registry::project_entries(&global)?
        .into_iter()
        .filter(|entry| entry.name.as_str() != vowp_store::RESERVED_NODE_PROJECT)
        .map(|entry| entry.name)
        .collect();
    match projects.len() {
        0 => Ok(ctx.project().clone()),
        1 => Ok(projects.remove(0)),
        _ => {
            let names = projects.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            Err(CliError::Invalid(format!(
                "several projects are registered ({names}); pass --project <name|path>"
            )))
        }
    }
}

/// Opens the store bound to [`selected_project`].
pub fn selected_store(ctx: &crate::Ctx) -> Result<Store, CliError> {
    let project = selected_project(ctx)?;
    if &project == ctx.project() {
        return ctx.store();
    }
    Ok(Store::open_with_project(ctx.paths(), &project)?)
}

/// Writes the node-level `vowp.toml` used by the systemd unit and home-scoped commands.
///
/// The file describes the reserved `_node` project and lives directly in the home. Its
/// presence makes `vowp up`, `vowp migrate`, `vowp project` and `vowp status` work from
/// `~/.vowp` with no checkout and no `--project`. It is created by `init`, after a
/// migration and before an `up` starts the supervisor; an existing file is never
/// overwritten.
pub fn ensure_home_project_file(paths: &NodePaths) -> Result<(), CliError> {
    let path = paths.root().join("vowp.toml");
    if path.is_file() {
        return Ok(());
    }
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    config.project.path = Some(paths.root().display().to_string());
    config.project.push = false;
    config.validate()?;
    std::fs::create_dir_all(paths.root())?;
    let text =
        toml::to_string_pretty(&config).map_err(|error| vowp_protocol::ProtocolError::Config(error.to_string()))?;
    let temp = paths.root().join(".vowp.toml.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// Builds the node-level dependencies `vowp up` and `down` need without a project context.
///
/// The base config is a valid default; [`NodeSupervisor`](vowp_core::loops::NodeSupervisor)
/// replaces it with each registered project's own `vowp.toml` before ticking that project.
pub fn home_deps(paths: &NodePaths) -> Result<Deps, CliError> {
    let config = home_config(paths)?;
    let adapters = AdapterRegistry::from_config(&config)?;
    Ok(build_home_deps(paths, config, Arc::new(adapters)))
}

/// Builds the node-level dependencies `vowp down` needs.
///
/// `down` only needs the clock and tmux; it must still work when a harness mapping
/// referenced by `config.toml` is broken, so it uses an empty registry.
pub fn home_shell_deps(paths: &NodePaths) -> Result<Deps, CliError> {
    let config = home_config(paths)?;
    Ok(build_home_deps(paths, config, Arc::new(AdapterRegistry::new())))
}

/// Loads the base node config with `<home>/config.toml` defaults applied.
fn home_config(paths: &NodePaths) -> Result<VowpConfig, CliError> {
    let mut config = VowpConfig::with_defaults_for_tests();
    let node_file_path = paths.root().join("config.toml");
    if node_file_path.is_file() {
        let text = std::fs::read_to_string(&node_file_path)?;
        let node_file = NodeFile::from_toml(&text)?;
        config = config.with_node_defaults(&node_file);
    }
    config.project.name = ProjectName::parse(vowp_store::RESERVED_NODE_PROJECT)?;
    config.project.path = Some(paths.root().display().to_string());
    config.validate()?;
    Ok(config)
}

/// Assembles [`Deps`] for a node-level command.
fn build_home_deps(paths: &NodePaths, config: VowpConfig, adapters: Arc<AdapterRegistry>) -> Deps {
    let command: Arc<dyn vowp_core::CommandRunner> = Arc::new(SystemRunner);
    Deps {
        paths: paths.clone(),
        node: config.node.name.clone(),
        config: Arc::new(config),
        clock: Arc::new(SystemClock),
        log: Arc::new(NullLog),
        adapters,
        windows: Arc::new(Tmux::for_home(paths.root())),
        git: Arc::new(SystemGit::new(Arc::clone(&command))),
        cmd: command,
        rng: Arc::new(SystemRng::new()),
    }
}

/// Returns the control home, when one is configured.
fn read_control_home() -> Result<Option<ControlHome>, CliError> {
    let root = match std::env::var_os("VOWP_CONTROL_HOME").filter(|value| !value.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => {
            let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
                return Ok(None);
            };
            PathBuf::from(home).join(".vowp-control")
        }
    };
    let config_path = root.join("control.json");
    if !config_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&config_path)?;
    let config: ControlConfig = serde_json::from_str(&text)
        .map_err(|error| CliError::Invalid(format!("{}: {error}", config_path.display())))?;
    Ok(Some(ControlHome { root, config }))
}

/// Returns true when `path` is a usable control directory.
pub fn is_control_dir(path: &Path) -> bool {
    path.join("control.json").is_file()
}
