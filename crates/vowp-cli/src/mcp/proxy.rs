//! The Mac-side MCP proxy: stdio JSON-RPC forwarded over the local agent socket.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.4 and §10.5. `initialize`,
//! `tools/list` and `ping` are answered locally. A `tools/call` is sent as a typed
//! `ToolCall` to the socket served by `vowp agent run`, which forwards it to the home;
//! the home checks the surface token scope and the live lease and executes the same core
//! path as its own MCP server. The proxy never opens a store, reads a secret or prints an
//! argument value, a token or an absolute local path.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use vowp_agent::{AgentConfig, SocketRequest, SocketResponse};
use vowp_protocol::{SessionId, SurfaceId};

use crate::mcp::{jsonrpc, tools};
use crate::{CliError, IoStreams};

/// How long one forwarded call may wait for the agent's reply.
///
/// The socket server itself waits up to 65 seconds for the home, then answers with a typed
/// `tool_timeout`; this leaves margin without hanging the app forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(75);

/// Largest accepted socket reply line.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// Options for the local MCP proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyOptions {
    /// Unix socket served by `vowp agent run`.
    pub agent_socket: PathBuf,
    /// Paired surface this proxy represents.
    pub surface: SurfaceId,
}

/// Serves the stdio MCP proxy until stdin closes.
pub(crate) fn serve(io: &mut dyn IoStreams, opts: &ProxyOptions) -> Result<i32, CliError> {
    let definitions = local_tools();
    let mut state = ProxyState { next_request_id: 0 };
    super::serve_jsonrpc(io, |request| handle_request(opts, &definitions, &mut state, request))
}

/// Per-process counters; a request id is never the client's id, so it cannot collide with
/// an id another local client happens to send to the same agent.
struct ProxyState {
    next_request_id: u64,
}

impl ProxyState {
    fn next_request_id(&mut self) -> String {
        self.next_request_id += 1;
        format!("mcp-{}", self.next_request_id)
    }
}

/// Returns the tool catalogue without the tools a local surface always refuses.
fn local_tools() -> Value {
    let mut definitions = tools::definitions();
    if let Some(entries) = definitions.get_mut("tools").and_then(Value::as_array_mut) {
        entries.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_none_or(|name| refusal(name).is_none())
        });
    }
    definitions
}

/// Returns why a control-machine-only tool is refused on a local surface.
fn refusal(name: &str) -> Option<&'static str> {
    match name {
        "tunnel_open" | "tunnel_close" => Some("managed SSH tunnels are opened on the control machine"),
        "secret_request" => {
            Some("home secrets are never requested from a local surface; set the value on this Mac instead")
        }
        "node_enroll" | "node_revoke" => Some("node enrollment and revocation are CLI-only on the home"),
        "surface_pair" => Some("surface pairing is CLI-only on the home"),
        "offload_policy_set" => Some("offload-policy writes are CLI-only on the home"),
        _ => None,
    }
}

/// Handles one parsed request; notifications return `None`.
fn handle_request(
    opts: &ProxyOptions,
    definitions: &Value,
    state: &mut ProxyState,
    request: &jsonrpc::Request,
) -> Option<Value> {
    if request.is_notification() {
        return None;
    }
    let id = request.id_value();
    if let Some(result) = super::local_method(request, definitions) {
        return Some(jsonrpc::success(id, result));
    }
    if request.method == "tools/call" {
        let (name, arguments) = match super::tools_call(request) {
            Ok(call) => call,
            Err(response) => return Some(response),
        };
        let result = call_tool(opts, state, &name, &arguments);
        return Some(jsonrpc::success(id, result));
    }
    Some(jsonrpc::error_response(
        id,
        jsonrpc::METHOD_NOT_FOUND,
        &format!("method not found: {}", request.method),
    ))
}

/// Forwards one tool call or returns a typed MCP failure; it never retries.
fn call_tool(opts: &ProxyOptions, state: &mut ProxyState, name: &str, arguments: &Value) -> Value {
    if let Some(reason) = refusal(name) {
        return failure("refused", &format!("{name}: {reason}"), false);
    }
    let request_id = state.next_request_id();
    match forward(opts, &request_id, name, arguments) {
        Ok(result) => result,
        Err(error) => failure(&error.code, &error.message, error.retryable),
    }
}

/// Wraps a typed failure in the MCP content envelope, preserving its code.
///
/// The home returns the same text shape for its own tool failures; the extra `code` and
/// `retryable` members let the app distinguish a refused control tool from a lease loss.
fn failure(code: &str, message: &str, retryable: bool) -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": format!("{code}: {message}")}],
        "isError": true,
        "code": code,
        "retryable": retryable,
    })
}

/// Sends one `ToolCall` and returns the home's result or typed error unchanged.
fn forward(opts: &ProxyOptions, request_id: &str, tool: &str, arguments: &Value) -> Result<Value, ProxyError> {
    socket_ready(&opts.agent_socket)?;
    let session = bound_session(&opts.agent_socket, &opts.surface)?;
    let request = SocketRequest::ToolCall {
        request_id: request_id.to_owned(),
        surface: opts.surface.clone(),
        session,
        tool: tool.to_owned(),
        arguments: arguments.clone(),
    };
    let mut bytes = serde_json::to_vec(&request)
        .map_err(|_| ProxyError::new("invalid_input", "the tool call could not be encoded", false))?;
    bytes.push(b'\n');

    let mut stream = UnixStream::connect(&opts.agent_socket).map_err(|_| {
        ProxyError::home_offline("the agent socket is not accepting connections; start `vowp agent run`")
    })?;
    stream
        .set_write_timeout(Some(REPLY_TIMEOUT))
        .map_err(|_| ProxyError::home_offline("the agent socket could not be configured"))?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .map_err(|_| ProxyError::home_offline("the agent socket could not be configured"))?;
    stream
        .write_all(&bytes)
        .map_err(|_| ProxyError::home_offline("the agent socket closed before the tool call was sent"))?;

    let mut reader = BufReader::new(stream.take(MAX_REPLY_BYTES));
    let mut line = String::new();
    let read = reader.read_line(&mut line).map_err(|error| match error.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            ProxyError::new("tool_timeout", "the agent did not answer the tool call", true)
        }
        _ => ProxyError::home_offline("the agent closed the connection before answering the tool call"),
    })?;
    if read == 0 {
        return Err(ProxyError::home_offline(
            "the agent closed the connection before answering the tool call",
        ));
    }
    if line.len() as u64 > MAX_REPLY_BYTES {
        return Err(ProxyError::invalid_response());
    }
    let response: SocketResponse = serde_json::from_str(&line).map_err(|_| ProxyError::invalid_response())?;
    match response {
        SocketResponse::ToolResult {
            request_id: echoed,
            result,
            error,
        } => {
            if echoed != request_id {
                return Err(ProxyError::invalid_response());
            }
            match (result, error) {
                (Some(result), None) => Ok(result),
                (None, Some(error)) => Err(ProxyError {
                    code: error.code,
                    message: error.message,
                    retryable: error.retryable,
                }),
                _ => Err(ProxyError::invalid_response()),
            }
        }
        SocketResponse::Error { code, message } => {
            let retryable = matches!(code.as_str(), "home_offline" | "agent_timeout");
            Err(ProxyError {
                code,
                message,
                retryable,
            })
        }
        SocketResponse::Ok | SocketResponse::Pong => Err(ProxyError::invalid_response()),
    }
}

/// Checks that the path is a private Unix socket before connecting.
fn socket_ready(path: &Path) -> Result<(), ProxyError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            ProxyError::home_offline("the agent socket is not present; start `vowp agent run`")
        }
        _ => ProxyError::home_offline("the agent socket could not be inspected; start `vowp agent run`"),
    })?;
    if !metadata.file_type().is_socket() {
        return Err(ProxyError::new(
            "refused",
            "the agent socket path is not a Unix socket",
            false,
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ProxyError::new(
            "refused",
            "the agent socket is accessible to other users; restart `vowp agent run`",
            false,
        ));
    }
    Ok(())
}

/// Reads the session bound to the paired surface next to the agent socket.
fn bound_session(socket: &Path, surface: &SurfaceId) -> Result<SessionId, ProxyError> {
    let state_dir = match socket.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let path = AgentConfig::surface_session_path(state_dir, surface);
    let text = fs::read_to_string(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ProxyError::new(
            "not_found",
            format!("surface {surface} has no session binding; pair it with `vowp agent pair`"),
            false,
        ),
        _ => ProxyError::new("io", "the surface session binding could not be read", false),
    })?;
    SessionId::parse(text.trim()).map_err(|_| {
        ProxyError::new(
            "invalid_input",
            "the surface session binding is not a valid session id",
            false,
        )
    })
}

/// One typed failure returned to the MCP client.
struct ProxyError {
    code: String,
    message: String,
    retryable: bool,
}

impl ProxyError {
    fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    fn home_offline(message: impl Into<String>) -> Self {
        Self::new("home_offline", message, true)
    }

    fn invalid_response() -> Self {
        Self::new(
            "invalid_response",
            "the agent returned an unreadable tool response",
            false,
        )
    }
}
