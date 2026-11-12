//! Command-line options and project selection for the daemon.
//!
//! Reads open the home with [`vowp_store::Store::open_read_only`]. The two writes (inbox
//! answer and session switch) use [`vowp_store::Store::open_writable`]; neither path creates
//! or migrates a home.

use std::ffi::OsString;
use std::path::PathBuf;

use vowp_protocol::{NodeName, Project, ProjectName};
use vowp_store::{NodePaths, Store};

use crate::error::DaemonError;

/// Usage text printed for `--help`.
pub const USAGE: &str = "\
Usage: vowp-daemon [--home <dir>] [--project <name>] [--port <n>]

Serves the VOWP loopback HTTP API for one project. Reads are token-free. The two
writes are POST /api/v1/inbox/{id}/answer and POST /api/v1/sessions/{id}/switch,
enabled by VOWP_API_TOKEN (missing or empty disables them). The listener binds
127.0.0.1 only.

Options:
  --home <dir>      VOWP home (default: $VOWP_HOME, else $HOME/.vowp)
  --project <name>  registered project to serve (default: the only registered project)
  --port <n>        loopback port (default: 0, an ephemeral port)
  -h, --help        print this help
";

/// Options parsed from `vowp-daemon` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Node home; `None` means `VOWP_HOME` or `$HOME/.vowp`.
    pub home: Option<PathBuf>,
    /// Registered project to serve; `None` means the only registered project.
    pub project: Option<String>,
    /// Loopback port; zero asks the operating system for an ephemeral port.
    pub port: u16,
}

/// What the binary should do after parsing its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Run the server with these options.
    Serve(Options),
    /// Print [`USAGE`] and exit successfully.
    Help,
}

/// Parses arguments, excluding the program name.
pub fn parse_invocation(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, DaemonError> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "--home" => {
                options.home = Some(PathBuf::from(next_value(&mut args, "--home")?));
            }
            "--project" => {
                options.project = Some(next_value(&mut args, "--project")?.to_string_lossy().into_owned());
            }
            "--port" => {
                options.port = parse_port(&next_value(&mut args, "--port")?.to_string_lossy())?;
            }
            _ if text.starts_with("--home=") => {
                options.home = Some(PathBuf::from(&text["--home=".len()..]));
            }
            _ if text.starts_with("--project=") => {
                options.project = Some(text["--project=".len()..].to_owned());
            }
            _ if text.starts_with("--port=") => {
                options.port = parse_port(&text["--port=".len()..])?;
            }
            other => {
                return Err(DaemonError::Config(format!(
                    "unknown argument {other:?}; run `vowp-daemon --help`"
                )));
            }
        }
    }
    Ok(Invocation::Serve(options))
}

/// Returns the value after a flag that takes one.
fn next_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, DaemonError> {
    args.next()
        .ok_or_else(|| DaemonError::Config(format!("{flag} needs a value; run `vowp-daemon --help`")))
}

/// Parses a TCP port number.
fn parse_port(text: &str) -> Result<u16, DaemonError> {
    text.parse::<u16>()
        .map_err(|_| DaemonError::Config(format!("invalid port {text:?}; expected 0-65535")))
}

impl Options {
    /// Resolves the node home without touching the filesystem.
    pub fn paths(&self) -> Result<NodePaths, DaemonError> {
        match &self.home {
            Some(home) => Ok(NodePaths::new(home)),
            None => NodePaths::from_env().map_err(DaemonError::from_store),
        }
    }
}

/// The project and node identity a daemon serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSelection {
    /// Registered project to serve.
    pub project: ProjectName,
    /// Node name from the registered project's parsed `vowp.toml` config.
    pub node: NodeName,
    /// Store schema version of the project database.
    pub schema_version: i64,
}

/// Selects the project to serve from an existing read-only home.
///
/// With `requested`, that registered project is selected. Without it, exactly one
/// non-reserved project must be registered; zero or several is a clear configuration
/// error naming `--project`.
pub fn select_project(paths: &NodePaths, requested: Option<&str>) -> Result<ProjectSelection, DaemonError> {
    let store = Store::open_read_only(paths, None).map_err(DaemonError::from_store)?;
    let projects = store.list_projects().map_err(DaemonError::from_store)?;
    let real: Vec<&Project> = projects
        .iter()
        .filter(|project| project.name.as_str() != vowp_store::RESERVED_NODE_PROJECT)
        .collect();

    let chosen = match requested {
        Some(requested) => {
            if requested == vowp_store::RESERVED_NODE_PROJECT {
                return Err(DaemonError::Config(format!(
                    "project {requested:?} is reserved; pass a real project name"
                )));
            }
            let name = ProjectName::parse(requested)
                .map_err(|_| DaemonError::Config(format!("invalid project name {requested:?}")))?;
            real.iter()
                .find(|project| project.name == name)
                .map(|project| (*project).clone())
                .ok_or_else(|| {
                    DaemonError::Config(format!(
                        "project {requested:?} is not registered; run `vowp project ls`"
                    ))
                })?
        }
        None => match real.as_slice() {
            [] => {
                return Err(DaemonError::Config(
                    "no project is registered in this VOWP home; run `vowp init` or pass --project".to_owned(),
                ));
            }
            [only] => (*only).clone(),
            several => {
                let names = several
                    .iter()
                    .map(|project| project.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DaemonError::Config(format!(
                    "several projects are registered ({names}); pass --project <name>"
                )));
            }
        },
    };

    let node = node_for_project(&chosen);
    let bound = Store::open_read_only(paths, Some(&chosen.name)).map_err(DaemonError::from_store)?;
    let schema_version = bound.project_schema_version().map_err(DaemonError::from_store)?;
    Ok(ProjectSelection {
        project: chosen.name,
        node,
        schema_version,
    })
}

/// Returns the node name from a registered project's parsed `vowp.toml` config.
///
/// `vowp init` stores the whole [`vowp_protocol::VowpConfig`] as JSON, so `node.name` is
/// present; a fixture with an empty config falls back to the documented node default.
fn node_for_project(project: &Project) -> NodeName {
    project
        .config
        .pointer("/node/name")
        .and_then(serde_json::Value::as_str)
        .and_then(|name| NodeName::parse(name).ok())
        .unwrap_or_else(|| vowp_protocol::NodeConfig::default().name)
}

#[cfg(test)]
mod tests {
    use vowp_protocol::{Node, Project, ProjectName, Timestamp};
    use vowp_store::NodePaths;

    use super::*;

    fn now() -> Timestamp {
        Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
    }

    fn project(name: &str, node: Option<&str>) -> Project {
        let config = match node {
            Some(node) => serde_json::json!({ "node": { "name": node } }),
            None => serde_json::json!({}),
        };
        Project {
            name: ProjectName::parse(name).expect("project name"),
            path: format!("/tmp/{name}"),
            remote_url: None,
            default_branch: "main".to_owned(),
            config,
            created_at: now(),
            updated_at: now(),
        }
    }

    fn home() -> (tempfile::TempDir, NodePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let store = Store::open(&paths).expect("store");
        store
            .upsert_node(&Node {
                name: NodeName::parse("local").expect("node"),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: None,
                created_at: now(),
            })
            .expect("node row");
        (tmp, paths)
    }

    fn register(paths: &NodePaths, project: Project) {
        let store = Store::open(paths).expect("store");
        store.upsert_project(&project).expect("project row");
    }

    #[test]
    fn select_project_errors_when_no_project_is_registered() {
        let (_tmp, paths) = home();
        let error = select_project(&paths, None).expect_err("no project");
        assert!(error.to_string().contains("--project"), "{error}");
    }

    #[test]
    fn select_project_errors_when_several_projects_are_registered() {
        let (_tmp, paths) = home();
        register(&paths, project("one", None));
        register(&paths, project("two", None));
        let error = select_project(&paths, None).expect_err("several projects");
        let text = error.to_string();
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(text.contains("--project"), "{text}");
    }

    #[test]
    fn select_project_uses_the_only_project_and_its_configured_node() {
        let (_tmp, paths) = home();
        register(&paths, project("only", Some("build-node")));
        let selection = select_project(&paths, None).expect("selection");
        assert_eq!(selection.project.as_str(), "only");
        assert_eq!(selection.node.as_str(), "build-node");
        assert_eq!(selection.schema_version, vowp_store::PROJECT_SCHEMA_VERSION);
    }

    #[test]
    fn select_project_honours_an_explicit_registered_project() {
        let (_tmp, paths) = home();
        register(&paths, project("one", None));
        register(&paths, project("two", None));
        let selection = select_project(&paths, Some("two")).expect("selection");
        assert_eq!(selection.project.as_str(), "two");
    }

    #[test]
    fn select_project_rejects_unknown_reserved_and_invalid_names() {
        let (_tmp, paths) = home();
        register(&paths, project("only", None));

        let unknown = select_project(&paths, Some("missing")).expect_err("unknown");
        assert!(unknown.to_string().contains("not registered"), "{unknown}");

        let reserved = select_project(&paths, Some("_node")).expect_err("reserved");
        assert!(reserved.to_string().contains("reserved"), "{reserved}");

        let invalid = select_project(&paths, Some("Not A Name")).expect_err("invalid");
        assert!(invalid.to_string().contains("invalid project name"), "{invalid}");
    }

    #[test]
    fn parse_invocation_reads_flags_and_rejects_bad_values() {
        let invocation = parse_invocation([
            OsString::from("--home"),
            OsString::from("/tmp/home"),
            OsString::from("--project=proj"),
            OsString::from("--port=8080"),
        ])
        .expect("options");
        let Invocation::Serve(options) = invocation else {
            panic!("expected serve");
        };
        assert_eq!(options.home, Some(PathBuf::from("/tmp/home")));
        assert_eq!(options.project.as_deref(), Some("proj"));
        assert_eq!(options.port, 8080);

        assert!(matches!(
            parse_invocation([OsString::from("--help")]).expect("help"),
            Invocation::Help
        ));
        assert!(parse_invocation([OsString::from("--port"), OsString::from("70000")]).is_err());
        assert!(parse_invocation([OsString::from("--bogus")]).is_err());
    }
}
