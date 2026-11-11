//! `vowp mcp` — the stdio MCP server.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7. The handler only parses the frozen
//! flags; the server lives in [`crate::mcp`].

use std::path::PathBuf;

use clap::Args;
use vowp_protocol::{SessionId, SurfaceId};

use crate::mcp::McpOptions;
use crate::{CliError, Ctx, IoStreams};

/// Arguments for `vowp mcp`.
#[derive(Debug, Clone, Args)]
pub struct McpArgs {
    /// Session the server writes to.
    #[arg(long, value_name = "ID")]
    pub session: String,
    /// Surface id this server represents.
    #[arg(long, value_name = "ID")]
    pub surface: String,
    /// File that holds the lease token (read on every write).
    #[arg(long, value_name = "FILE")]
    pub token_file: PathBuf,
}

/// Runs `vowp mcp`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &McpArgs) -> Result<i32, CliError> {
    let session = SessionId::parse(&args.session)?;
    if args.surface.trim().is_empty() {
        return Err(CliError::Invalid("--surface must not be empty".to_owned()));
    }
    let opts = McpOptions {
        session,
        surface: SurfaceId(args.surface.clone()),
        token_file: args.token_file.clone(),
    };
    crate::mcp::serve_with_ctx(ctx, io, &opts)
}
