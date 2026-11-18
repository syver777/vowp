//! The stdio MCP servers: newline-delimited JSON-RPC 2.0, plain threads, no async runtime.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.7 and
//! `docs/design/v0.4/local-orchestrator.md` §5.4. The home server is started by an app
//! (opencode, Claude Code or any MCP-capable client) with the session and surface it
//! represents; read tools work in follow mode and every write re-reads the lease token.
//! [`proxy`] is the Mac-side variant: it answers the local methods and forwards
//! `tools/call` over the agent socket to the home.

mod backend;
mod jsonrpc;
pub(crate) mod proxy;
mod tools;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use serde_json::{Value, json};
use vowp_protocol::{SessionId, SurfaceId};

use crate::node_ctx::NodeRole;
use crate::{CliError, Ctx, Global, IoStreams};

use backend::Backend;

/// Options for the home `vowp mcp` server.
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
    let definitions = tools::definitions();
    serve_jsonrpc(io, |request| handle_request(&backend, &definitions, request))
}

/// Reads newline-delimited requests from stdin and writes one response per request.
///
/// Both the home server and the local proxy share this loop: framing, notifications and
/// parse errors are identical, only [`local_method`] and `tools/call` differ.
pub(crate) fn serve_jsonrpc(
    io: &mut dyn IoStreams,
    mut handle: impl FnMut(&jsonrpc::Request) -> Option<Value>,
) -> Result<i32, CliError> {
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
            Ok(request) => handle(&request),
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

/// Answers the methods every MCP server serves without the home; `None` for any other.
///
/// `tools` is the caller's `tools/list` result, so the proxy can advertise only the tools
/// it may forward.
pub(crate) fn local_method(request: &jsonrpc::Request, tools: &Value) -> Option<Value> {
    match request.method.as_str() {
        "initialize" => {
            let version = request
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            Some(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "vowp", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        // Claude Code sends this probe before initialize. A valid empty capability is
        // friendlier than -32601 and is accepted by the MCP clients we tested.
        "server/discover" => Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "vowp", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Some(json!({})),
        "tools/list" => Some(tools.clone()),
        _ => None,
    }
}

/// Reads one `tools/call` request, or returns the ready invalid-params response.
pub(crate) fn tools_call(request: &jsonrpc::Request) -> Result<(String, Value), Value> {
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return Err(jsonrpc::error_response(
            request.id_value(),
            jsonrpc::INVALID_PARAMS,
            "tools/call requires a tool name",
        ));
    };
    let arguments = request.params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    Ok((name.to_owned(), arguments))
}

/// Handles one parsed request; notifications return `None`.
fn handle_request(backend: &Backend<'_>, tools: &Value, request: &jsonrpc::Request) -> Option<Value> {
    if request.is_notification() {
        return None;
    }
    let id = request.id_value();
    let result = if let Some(result) = local_method(request, tools) {
        result
    } else if request.method == "tools/call" {
        let (name, arguments) = match tools_call(request) {
            Ok(call) => call,
            Err(response) => return Some(response),
        };
        tools::call(backend, &name, &arguments)
    } else {
        return Some(jsonrpc::error_response(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            &format!("method not found: {}", request.method),
        ));
    };
    Some(jsonrpc::success(id, result))
}
