//! The stdio MCP server: newline-delimited JSON-RPC 2.0, plain threads, no async runtime.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7. The server is started by an app
//! (opencode, Claude Code or any MCP-capable client) with the session and surface it
//! represents. Read tools work in follow mode; every write re-reads the lease token from
//! the file and refuses with the read-only notice after a takeover.

mod backend;
mod jsonrpc;
mod tools;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use serde_json::{Value, json};
use vowp_protocol::{SessionId, SurfaceId};

use crate::node_ctx::NodeRole;
use crate::{CliError, Ctx, Global, IoStreams};

use backend::Backend;

/// Options for `vowp mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOptions {
    /// Session the server writes to.
    pub session: SessionId,
    /// Surface id this server represents.
    pub surface: SurfaceId,
    /// File that holds the lease token.
    pub token_file: PathBuf,
}

/// Serves MCP JSON-RPC until stdin closes, discovering the node like every other command.
pub fn serve(io: &mut dyn IoStreams, opts: &McpOptions) -> Result<i32, CliError> {
    let global = Global::default();
    let ctx = Ctx::build(&global, io)?;
    serve_with_ctx(&ctx, io, opts)
}

/// Serves MCP JSON-RPC with an already built context.
pub(crate) fn serve_with_ctx(ctx: &Ctx, io: &mut dyn IoStreams, opts: &McpOptions) -> Result<i32, CliError> {
    if ctx.role() == NodeRole::Home {
        let store = ctx.fleet_store()?;
        store.require_session(&opts.session)?;
    }
    let backend = Backend::new(ctx, opts);
    loop {
        let mut line = String::new();
        let read = io.input().read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let response = match jsonrpc::parse(&line) {
            Ok(request) => handle_request(&backend, &request),
            Err(response) => Some(response),
        };
        if let Some(response) = response {
            writeln!(
                io.out(),
                "{}",
                serde_json::to_string(&response).map_err(|error| CliError::Invalid(error.to_string()))?
            )?;
            io.out().flush()?;
        }
    }
    Ok(0)
}

/// Handles one parsed request; notifications return `None`.
fn handle_request(backend: &Backend<'_>, request: &jsonrpc::Request) -> Option<Value> {
    if request.is_notification() {
        return None;
    }
    let id = request.id_value();
    let result = match request.method.as_str() {
        "initialize" => {
            let version = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "vowp", "version": env!("CARGO_PKG_VERSION")},
            })
        }
        // Claude Code sends this probe before initialize. A valid empty capability is
        // friendlier than -32601 and is accepted by the MCP clients we tested.
        "server/discover" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "vowp", "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => tools::definitions(),
        "tools/call" => {
            let name = request.params.get("name").and_then(Value::as_str);
            let Some(name) = name else {
                return Some(jsonrpc::error_response(
                    id,
                    jsonrpc::INVALID_PARAMS,
                    "tools/call requires a tool name",
                ));
            };
            let arguments = request.params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            tools::call(backend, name, &arguments)
        }
        other => {
            return Some(jsonrpc::error_response(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            ));
        }
    };
    Some(jsonrpc::success(id, result))
}
