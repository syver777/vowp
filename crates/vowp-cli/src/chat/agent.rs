//! `vowp chat --agent`: the local chat surface on the Mac.
//!
//! The home log stays authoritative. This handler loads `~/.vowp-agent/agent.json`, the node
//! key and the paired surface token, then runs the adapter on this machine while the agent
//! client acquires the lease, spools events and streams them to the home gateway. It never
//! takes the home lease and never writes a second session log.
//!
//! [`crate::execute`] dispatches `chat --agent` here before the control-machine route and
//! before [`Ctx::build`], so the Mac needs neither a local home database nor a registered
//! project. The checkout comes from `--project <dir>` or the nearest `vowp.toml`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vowp_agent::{AgentConfig, AgentError, ChatClient, ChatTurnRequest, ChatTurnStatus, SystemAgentClock};
use vowp_core::AdapterRegistry;
use vowp_protocol::{
    Adapter, EventBody, Executor, ExecutorRef, HarnessEvent, NodeName, PathRef, ProjectName, RunPurpose, SessionId,
    VowpConfig,
};
use vowp_store::NodePaths;

use crate::chat::{ChatOptions, write_wrapped};
use crate::cli::{ChatArgs, Global};
use crate::{CliError, Ctx, IoStreams};

/// The local checkout an agent chat invocation runs in.
struct LocalProject {
    root: PathBuf,
    config: VowpConfig,
}

/// Runs the local agent bridge from a parsed command, without building a [`Ctx`].
///
/// `vowp chat --agent` must work on a Mac with no VOWP home database and must not be
/// routed to the home from a control machine. [`crate::execute`] calls this before both
/// routes; the checkout comes from `--project <dir>` or the nearest `vowp.toml`.
pub fn run_local(global: &Global, io: &mut dyn IoStreams, args: &ChatArgs) -> Result<i32, CliError> {
    let opts = options_from_args(global, args)?;
    let local = local_project(global)?;
    run_bridge(io, &opts, &local)
}

/// Runs the local agent chat bridge for a chat that already has a [`Ctx`].
///
/// The home log stays authoritative. This handler loads `~/.vowp-agent/agent.json`, the
/// node key and the paired surface token, then runs the adapter on this machine while
/// the agent client spools and streams events to the home gateway. It never takes the
/// home lease and never writes a second session log.
pub fn run_shared(ctx: &Ctx, io: &mut dyn IoStreams, opts: &ChatOptions) -> Result<i32, CliError> {
    let local = LocalProject {
        root: ctx.project_root().to_path_buf(),
        config: ctx.config().clone(),
    };
    run_bridge(io, opts, &local)
}

/// Builds the bridge options from the agent command spelling.
///
/// `--node` names the executor node for agent chat; a global `--node` before the
/// subcommand has the same meaning and is never a redirect target.
fn options_from_args(global: &Global, args: &ChatArgs) -> Result<ChatOptions, CliError> {
    let node = args
        .node
        .as_deref()
        .or(global.node.as_deref())
        .map(NodeName::parse)
        .transpose()?;
    let executor = args
        .executor
        .as_deref()
        .map(Executor::parse)
        .transpose()?
        .map(|executor| ExecutorRef {
            executor,
            node: node.clone(),
        });
    Ok(ChatOptions {
        session: SessionId::parse(&args.id)?,
        executor,
        node,
        follow: args.follow,
        takeover: args.takeover,
    })
}

/// Resolves the checkout without opening the node database.
///
/// The Mac has no project registry to consult, so `--project` must name a checkout
/// directory; without it the nearest ancestor with `vowp.toml` wins.
fn local_project(global: &Global) -> Result<LocalProject, CliError> {
    let paths = crate::ctx::node_paths(global)?;
    let root = match &global.project {
        Some(value) => {
            let candidate = PathBuf::from(value);
            if candidate.is_dir() && candidate.join("vowp.toml").is_file() {
                candidate
            } else if candidate.is_dir() {
                return Err(CliError::NotFound(format!(
                    "no vowp.toml in {}; run `vowp init`",
                    candidate.display()
                )));
            } else {
                return Err(CliError::Invalid(format!(
                    "--project {value} is not a checkout; on the Mac pass the directory containing vowp.toml"
                )));
            }
        }
        None => {
            let cwd = std::env::current_dir()?;
            crate::ctx::find_project_root(&cwd).ok_or_else(|| {
                CliError::NotFound(
                    "no vowp.toml in this directory or its parents; run `vowp chat --agent` from a checkout or pass --project <dir>"
                        .to_owned(),
                )
            })?
        }
    };
    let config = crate::ctx::load_project_config(&root.join("vowp.toml"), &paths)?;
    Ok(LocalProject { root, config })
}

/// Runs the bridge against one already-resolved checkout.
fn run_bridge(io: &mut dyn IoStreams, opts: &ChatOptions, local: &LocalProject) -> Result<i32, CliError> {
    if opts.follow {
        return Err(CliError::Invalid(
            "`chat --agent --follow` is not supported; the agent bridge owns the live turn".to_owned(),
        ));
    }
    if opts.takeover {
        return Err(CliError::Invalid(
            "`chat --agent --takeover` is not supported; ask the owner to take the session over".to_owned(),
        ));
    }
    let session = opts.session.clone();
    let state_dir = AgentConfig::default_state_dir().map_err(map_agent_error)?;
    let config = AgentConfig::load(&state_dir).map_err(map_agent_error)?;
    if let Some(node) = opts.node.as_ref()
        && node != &config.node
    {
        return Err(CliError::Invalid(format!(
            "executor node {node} is not this agent ({})",
            config.node
        )));
    }
    let executor = match &opts.executor {
        Some(executor) => executor.executor.clone(),
        None => local
            .config
            .profiles
            .get("worker")
            .and_then(|profile| profile.ladder.first().cloned())
            .ok_or_else(|| {
                CliError::Invalid(
                    "no executor: pass --executor or define a non-empty `worker` profile in vowp.toml".to_owned(),
                )
            })?,
    };
    let cwd = local_working_dir(local)?;
    let cwd_ref = path_ref_for(&local.config.project.name, &local.root, &cwd);
    let adapter = adapter_for(&local.config, &executor)?;
    let mut chat = ChatClient::from_state_dir(state_dir.clone(), session.clone(), Arc::new(SystemAgentClock))
        .map_err(map_agent_error)?;
    let watcher = if io.is_tty() {
        crate::signals::SignalWatcher::install(&NodePaths::new(&state_dir)).ok()
    } else {
        None
    };
    let cancel = watcher.as_ref().map(crate::signals::SignalWatcher::cancel_token);
    writeln!(
        io.out(),
        "attached session {} on {} through the agent; press Ctrl-C to stop",
        session,
        config.node
    )?;
    loop {
        if cancel.as_ref().is_some_and(vowp_protocol::CancelToken::is_cancelled) {
            break;
        }
        let mut line = String::new();
        let read = io.input().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "/quit" | "/q") {
            break;
        }
        if line.starts_with('/') {
            writeln!(io.err(), "slash commands are not available in --agent mode")?;
            continue;
        }
        let request = ChatTurnRequest {
            executor: executor.clone(),
            prompt: line.to_owned(),
            cwd: cwd.clone(),
            cwd_ref: cwd_ref.clone(),
            // The home owns the session's job binding and purpose; the local checkout
            // has no store from which to read them. A continuation is always valid.
            job: None,
            purpose: RunPurpose::Continue,
        };
        let mut print_error: Option<CliError> = None;
        let status = chat.turn(
            &request,
            adapter.as_ref(),
            &mut |event| {
                if print_error.is_none()
                    && let Err(error) = print_event(io.out(), event)
                {
                    print_error = Some(error);
                }
            },
            cancel.as_ref(),
        );
        if let Some(error) = print_error {
            return Err(error);
        }
        match status.map_err(map_agent_error)? {
            ChatTurnStatus::Done => {}
            ChatTurnStatus::ReadOnly { reason } => {
                writeln!(io.err(), "This surface is read-only: {reason}")?;
                break;
            }
            ChatTurnStatus::SpoolFull { bytes, cap } => {
                return Err(CliError::Remote(format!(
                    "spool is full ({bytes} bytes would exceed the {cap}-byte cap); the turn stopped"
                )));
            }
            ChatTurnStatus::Interrupted => break,
        }
        if cancel.as_ref().is_some_and(vowp_protocol::CancelToken::is_cancelled) {
            break;
        }
    }
    let _ = chat.release();
    Ok(0)
}

/// Builds the project's adapter registry and returns the adapter for one executor.
///
/// Both the local chat surface and the assignment executor start adapters through this
/// helper, so the `vowp.toml` harness configuration is read in exactly one place.
pub(crate) fn adapter_for(config: &VowpConfig, executor: &Executor) -> Result<Arc<dyn Adapter>, CliError> {
    let adapters = AdapterRegistry::from_config(config)?;
    adapters
        .get(executor)
        .ok_or_else(|| CliError::Invalid(format!("harness for {executor} is not installed on this machine")))
}

/// Returns the directory the adapter starts in: the current directory when it is below
/// the project root, otherwise the project root itself.
fn local_working_dir(project: &LocalProject) -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir()?;
    let root = std::fs::canonicalize(&project.root).unwrap_or_else(|_| project.root.clone());
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    Ok(if cwd.starts_with(&root) { cwd } else { root })
}

/// Builds the project-relative path for a local working directory.
fn path_ref_for(project: &ProjectName, root: &Path, cwd: &Path) -> PathRef {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if cwd == root {
        return PathRef::Project {
            project: project.clone(),
        };
    }
    let Ok(relative) = cwd.strip_prefix(&root) else {
        return PathRef::Project {
            project: project.clone(),
        };
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        return PathRef::Project {
            project: project.clone(),
        };
    }
    PathRef::parse(&format!("subdir:{project}/{relative}")).unwrap_or_else(|_| PathRef::Project {
        project: project.clone(),
    })
}

/// Prints one adapter event in the home chat's narrow format.
fn print_event(out: &mut dyn std::io::Write, event: &HarnessEvent) -> Result<(), CliError> {
    match &event.body {
        EventBody::AssistantMessage(payload) => write_wrapped(out, &payload.text)?,
        EventBody::ToolCall(payload) => write_wrapped(out, &format!("· tool: {}", payload.name))?,
        EventBody::Error(payload) => write_wrapped(out, &format!("! {}", payload.message))?,
        EventBody::Note(payload) => write_wrapped(out, &format!("— {}", payload.text))?,
        _ => {}
    }
    Ok(())
}

/// Maps an agent failure onto the CLI's stable errors.
fn map_agent_error(error: AgentError) -> CliError {
    match error {
        AgentError::NotEnrolled(message) => CliError::NotFound(message),
        AgentError::Revoked(reason) => CliError::Guard(format!("agent revoked: {reason}")),
        AgentError::Superseded(connection_id) => CliError::Conflict(format!("agent superseded by {connection_id}")),
        AgentError::Auth(message) => CliError::Remote(format!("authentication rejected: {message}")),
        AgentError::Remote { kind, message, .. } => match kind {
            vowp_protocol::ErrorKind::Refused => CliError::Guard(message),
            vowp_protocol::ErrorKind::InvalidInput => CliError::Invalid(message),
            vowp_protocol::ErrorKind::Conflict => CliError::Conflict(message),
            vowp_protocol::ErrorKind::NotFound => CliError::NotFound(message),
            vowp_protocol::ErrorKind::Limit => CliError::Limited(message),
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
