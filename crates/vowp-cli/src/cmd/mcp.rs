//! `vowp mcp` — the stdio MCP server and the local MCP proxy.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7 and
//! `docs/design/v0.4/local-orchestrator.md` §5.4. The handler only parses the frozen
//! flags; the home server lives in [`crate::mcp`] and the Mac proxy in
//! [`crate::mcp::proxy`]. The proxy is dispatched before `Ctx::build`, because a Mac has no
//! local VOWP database.

use std::path::PathBuf;

use clap::Args;
use vowp_protocol::{SessionId, SurfaceId};

use crate::mcp::McpOptions;
use crate::mcp::proxy::ProxyOptions;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp mcp`.
#[derive(Debug, Clone, Args)]
pub struct McpArgs {
    /// Session the server writes to (home mode; not with --agent-socket).
    #[arg(
        long,
        value_name = "ID",
        required_unless_present = "agent_socket",
        conflicts_with = "agent_socket"
    )]
    pub session: Option<String>,
    /// Surface id this server represents.
    #[arg(long, value_name = "ID")]
    pub surface: String,
    /// File that holds the lease token (home mode; not with --agent-socket).
    #[arg(
        long,
        value_name = "FILE",
        required_unless_present = "agent_socket",
        conflicts_with = "agent_socket"
    )]
    pub token_file: Option<PathBuf>,
    /// Unix socket served by `vowp agent run`; selects the local MCP proxy.
    #[arg(long, value_name = "PATH")]
    pub agent_socket: Option<PathBuf>,
}

/// Runs the home `vowp mcp` server.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &McpArgs) -> Result<i32, CliError> {
    let session_text = args
        .session
        .as_deref()
        .ok_or_else(|| CliError::Invalid("--session is required without --agent-socket".to_owned()))?;
    let token_file = args
        .token_file
        .as_ref()
        .ok_or_else(|| CliError::Invalid("--token-file is required without --agent-socket".to_owned()))?;
    let session = SessionId::parse(session_text)?;
    if args.surface.trim().is_empty() {
        return Err(CliError::Invalid("--surface must not be empty".to_owned()));
    }
    let opts = McpOptions {
        session,
        surface: SurfaceId(args.surface.clone()),
        token_file: token_file.clone(),
    };
    crate::mcp::serve_with_ctx(ctx, io, &opts)
}

/// Runs the Mac-side proxy; it must be dispatched before [`Ctx`] exists.
pub fn run_proxy(io: &mut dyn IoStreams, args: &McpArgs) -> Result<i32, CliError> {
    let Some(agent_socket) = args.agent_socket.as_ref() else {
        return Err(CliError::Invalid(
            "--agent-socket selects the local MCP proxy".to_owned(),
        ));
    };
    if args.session.is_some() || args.token_file.is_some() {
        return Err(CliError::Invalid(
            "--session and --token-file are not used with --agent-socket".to_owned(),
        ));
    }
    if args.surface.trim().is_empty() {
        return Err(CliError::Invalid("--surface must not be empty".to_owned()));
    }
    let surface = SurfaceId(args.surface.clone());
    vowp_agent::validate_surface_id(&surface)
        .map_err(|_| CliError::Invalid("--surface must be a 26-character ULID".to_owned()))?;
    let opts = ProxyOptions {
        agent_socket: agent_socket.clone(),
        surface,
    };
    crate::mcp::proxy::serve(io, &opts)
}
