//! The local agent socket: newline-delimited JSON between a local client and the home.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.4, §8.3 and §10.4–§10.5, and
//! `docs/design/crates/vowp-agent.md`. `vowp agent run` binds `<state_dir>/agent.sock` mode
//! 0600. A local client such as `vowp mcp --agent-socket … --surface …` writes one JSON
//! request per line. The server never opens the store, never reads a home secret and never
//! logs a request or response payload; tool calls travel over the one outbound agent
//! connection and the home stays authoritative for token scopes and leases.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vowp_protocol::SessionId;
use vowp_protocol::SurfaceId;
use vowp_protocol::SurfaceScope;
use vowp_protocol::Timestamp;
use vowp_protocol::agent::surface::{ToolCall, ToolError};

use crate::cache::{Cache, CachedRead, debug_failure, is_cacheable_tool};
use crate::client::validate_surface_id;
use crate::config::ensure_private_dir;
use crate::error::AgentError;

/// File name of the local agent socket below the state directory.
pub const AGENT_SOCKET_FILE: &str = "agent.sock";

/// Largest accepted request line, including its trailing newline.
pub const MAX_SOCKET_REQUEST_BYTES: usize = 1024 * 1024;

/// How long a tool call waits for the home's `ToolResult` before it expires.
pub const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the frame loop wakes to service local socket requests.
pub(crate) const SOCKET_POLL: Duration = Duration::from_millis(50);

/// Poll interval of the accept thread; it also lets the thread observe the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Read and write timeout of one accepted local client.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_millis(250);

/// Upper bound on an attach or detach reply; a tool call has its own timeout.
const REPLY_TIMEOUT: Duration = Duration::from_secs(65);

/// One request written by a local client, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SocketRequest {
    /// Local liveness probe; answered without touching the home.
    Ping,
    /// Attach a surface to a session; forwarded as `SurfaceAttach`.
    Attach {
        /// Surface being attached.
        surface: SurfaceId,
        /// Session the surface will mirror.
        session: SessionId,
    },
    /// Detach a surface; forwarded as `SurfaceDetach`.
    Detach {
        /// Surface being detached.
        surface: SurfaceId,
        /// Session the surface was mirroring.
        session: SessionId,
    },
    /// Forward one MCP tool call as a `ToolCall` frame.
    ToolCall {
        /// Client-chosen request id, echoed by the matching `ToolResult`.
        request_id: String,
        /// Surface that made the call.
        surface: SurfaceId,
        /// Session the call is scoped to.
        session: SessionId,
        /// MCP tool name.
        tool: String,
        /// MCP arguments as a JSON object; never a local path or secret value.
        arguments: Value,
    },
    /// Redeem a one-time pairing code over the live `vowp agent run` connection.
    Pair {
        /// One-time pairing code; never logged or echoed.
        code: String,
        /// Surface the owner created on the home.
        surface: SurfaceId,
        /// Session the surface will mirror.
        session: SessionId,
        /// Application name, for example `opencode`.
        app: String,
    },
}

/// One response written by the server, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SocketResponse {
    /// The attach or detach frame was written to the home connection.
    Ok,
    /// Reply to a local [`SocketRequest::Ping`].
    Pong,
    /// The home's answer to one tool call; exactly one of `result` and `error` is set.
    ToolResult {
        /// Request id from the matching tool call.
        request_id: String,
        /// Tool result as normal MCP JSON, when the call succeeded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// Typed failure, passed through unchanged when the home supplied it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolError>,
    },
    /// A local protocol failure that is not a tool result.
    Error {
        /// Machine-readable code, for example `unknown_kind` or `home_offline`.
        code: String,
        /// Human-readable message; never a payload, token or absolute local path.
        message: String,
    },
}

/// The answer to one [`SocketRequest::Pair`].
///
/// The running agent writes the token to disk itself and returns only its path. The
/// plaintext token never crosses this local socket or appears in this value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairReply {
    /// The home accepted the pairing request and the files are on disk.
    Paired {
        /// Surface the token belongs to.
        surface_id: SurfaceId,
        /// Scopes granted to the token.
        scopes: Vec<SurfaceScope>,
        /// When the token expires.
        expires_at: Timestamp,
        /// Mode-0600 file that holds the token; never the token itself.
        token_path: PathBuf,
    },
    /// The pairing request failed locally or at the home.
    Error {
        /// Machine-readable code, for example `home_offline` or `pair_failed`.
        code: String,
        /// Human-readable message; never a pairing code or surface token.
        message: String,
    },
}

impl PairReply {
    /// Builds one typed pairing failure.
    pub(crate) fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl SocketResponse {
    pub(crate) fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn tool_error(request_id: &str, code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self::ToolResult {
            request_id: request_id.to_owned(),
            result: None,
            error: Some(ToolError {
                code: code.to_owned(),
                message: message.into(),
                retryable,
            }),
        }
    }
}

/// One local client request queued for the frame loop.
pub(crate) enum SocketEvent {
    Attach {
        surface: SurfaceId,
        session: SessionId,
        reply: Sender<SocketResponse>,
    },
    Detach {
        surface: SurfaceId,
        session: SessionId,
        reply: Sender<SocketResponse>,
    },
    ToolCall {
        call: ToolCall,
        reply: Sender<SocketResponse>,
    },
    Pair {
        request: crate::client::SurfacePairRequest,
        reply: Sender<PairReply>,
    },
}

struct SocketShared {
    stop: AtomicBool,
    connected: AtomicBool,
    clients: AtomicUsize,
    inbox: Sender<SocketEvent>,
    cache: Cache,
}

/// The mode-0600 Unix socket served by `vowp agent run`.
///
/// The listener accepts several local clients, one thread each, and queues their requests
/// for the single frame loop that owns the outbound home connection. Dropping the value
/// stops accepting, wakes the handler threads and removes the socket file only when it is
/// still the inode this server bound.
pub struct AgentSocket {
    path: PathBuf,
    bound: Option<(u64, u64)>,
    shared: Arc<SocketShared>,
    events: Receiver<SocketEvent>,
    accept: Option<JoinHandle<()>>,
}

impl AgentSocket {
    /// Binds `<state_dir>/agent.sock` mode 0600 and starts the accept thread.
    ///
    /// A live server is refused, so a second `vowp agent run` cannot take over the path. A
    /// stale socket file left by a crashed process is removed and rebound.
    pub fn bind(state_dir: &Path) -> Result<Self, AgentError> {
        ensure_private_dir(state_dir)?;
        let path = state_dir.join(AGENT_SOCKET_FILE);
        let listener = bind_listener(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let metadata = fs::metadata(&path)?;
        let bound = (metadata.dev(), metadata.ino());
        listener.set_nonblocking(true)?;

        let (sender, events) = mpsc::channel();
        let shared = Arc::new(SocketShared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            inbox: sender,
            cache: Cache::new(state_dir),
        });
        let accept_shared = Arc::clone(&shared);
        let accept = match thread::Builder::new()
            .name("vowp-agent-socket".to_owned())
            .spawn(move || accept_loop(listener, accept_shared))
        {
            Ok(handle) => handle,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(AgentError::Io(error));
            }
        };
        Ok(Self {
            path,
            bound: Some(bound),
            shared,
            events,
            accept: Some(accept),
        })
    }

    /// Returns the bound socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Updates whether the frame loop currently has a usable home connection.
    pub(crate) fn set_connected(&self, connected: bool) {
        self.shared.connected.store(connected, Ordering::SeqCst);
    }

    /// Drains requests queued by local client threads without blocking.
    pub(crate) fn drain_events(&self) -> Vec<SocketEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }
}

impl Drop for AgentSocket {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.connected.store(false, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let owned = fs::metadata(&self.path)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()))
            .is_some_and(|identity| Some(identity) == self.bound);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn bind_listener(path: &Path) -> Result<UnixListener, AgentError> {
    match fs::symlink_metadata(path) {
        Ok(_) => match probe_socket(path) {
            SocketProbe::Live => {
                return Err(AgentError::Config("the agent socket is already in use".to_owned()));
            }
            SocketProbe::Stale => remove_socket_file(path)?,
            SocketProbe::Unknown => {
                return Err(AgentError::Config(
                    "the agent socket exists and could not be probed".to_owned(),
                ));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(AgentError::Io(error)),
    }
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        // The path can appear between the probe and the bind. Probe once more so only a
        // genuinely stale file is removed.
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => match probe_socket(path) {
            SocketProbe::Live => Err(AgentError::Config("the agent socket is already in use".to_owned())),
            SocketProbe::Stale => {
                remove_socket_file(path)?;
                UnixListener::bind(path).map_err(AgentError::Io)
            }
            SocketProbe::Unknown => Err(AgentError::Config("the agent socket is already in use".to_owned())),
        },
        Err(error) => Err(AgentError::Io(error)),
    }
}

fn remove_socket_file(path: &Path) -> Result<(), AgentError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AgentError::Io(error)),
    }
}

/// Result of probing an existing path that may be a live or a stale agent socket.
enum SocketProbe {
    /// A server answered the liveness ping.
    Live,
    /// The path is a socket file that no server is serving.
    Stale,
    /// The path is not a socket, or the probe could not tell whether it is live.
    Unknown,
}

/// One liveness request written to an existing socket before the bind replaces it.
const SOCKET_PROBE_REQUEST: &[u8] = b"{\"kind\":\"ping\"}\n";

/// How long the bind-time liveness probe waits for the server's reply.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Probes an existing `agent.sock` path without ever removing a non-socket file.
///
/// A successful `connect` is not proof of a live server: Darwin can report a refused
/// connection only on the first read or write (through `SO_ERROR`). Only an answer to
/// a `Ping` proves the server is live; a reset, EOF or broken pipe proves the file is a
/// leftover socket that can be replaced.
fn probe_socket(path: &Path) -> SocketProbe {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {}
        // A regular file, directory or symlink is never replaced.
        Ok(_) => return SocketProbe::Unknown,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SocketProbe::Stale,
        Err(_) => return SocketProbe::Unknown,
    }
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) => return classify_probe_error(&error),
    };
    // BSD sockets can inherit O_NONBLOCK from the nonblocking listener; a read timeout
    // does not make a nonblocking stream block. The probe needs blocking I/O.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(SOCKET_PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_PROBE_TIMEOUT));
    if let Err(error) = stream.write_all(SOCKET_PROBE_REQUEST) {
        return classify_probe_error(&error);
    }
    if let Err(error) = stream.flush() {
        return classify_probe_error(&error);
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => SocketProbe::Stale,
        Ok(_) => SocketProbe::Live,
        Err(error) => classify_probe_error(&error),
    }
}

fn classify_probe_error(error: &io::Error) -> SocketProbe {
    if is_stale_socket_error(error) {
        SocketProbe::Stale
    } else {
        SocketProbe::Unknown
    }
}

/// Returns whether a probe failure means the socket file has no live server behind it.
///
/// Anything other than a permission, would-block, timeout or interrupted failure is a
/// dead-server failure on a path that the caller already confirmed is a socket. That
/// deliberately covers the wider Darwin error set (`ConnectionReset`, `AddrNotAvailable`,
/// `InvalidInput`, `ENOTSOCK`, `EPROTOTYPE`, ...) that a stale socket can produce.
fn is_stale_socket_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    )
}

fn accept_loop(listener: UnixListener, shared: Arc<SocketShared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                shared.clients.fetch_add(1, Ordering::SeqCst);
                let client_shared = Arc::clone(&shared);
                let spawned = thread::Builder::new()
                    .name("vowp-agent-socket-client".to_owned())
                    .spawn(move || handle_client(stream, client_shared));
                if spawned.is_err() {
                    shared.clients.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

fn handle_client(stream: UnixStream, shared: Arc<SocketShared>) {
    // macOS/BSD accept() inherits O_NONBLOCK from the nonblocking listener, and setting
    // a read timeout does not make a nonblocking stream block. Restore blocking mode
    // before the timeouts and before cloning the stream for the reader.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => {
            shared.clients.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line = Vec::new();
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        match read_request_line(&mut reader, &mut line) {
            Ok(false) => break,
            Ok(true) => {
                let (response, close) = handle_request(&shared, &line, &mut writer);
                line.clear();
                let wrote = match response {
                    Some(response) => write_response(&mut writer, &response).is_ok(),
                    None => true,
                };
                if !wrote || close {
                    break;
                }
            }
            Err(ReadLineError::Timeout) => {}
            Err(ReadLineError::TooLarge) => {
                let response = SocketResponse::error(
                    "frame_too_large",
                    format!("the request exceeds {MAX_SOCKET_REQUEST_BYTES} bytes"),
                );
                let _ = write_response(&mut writer, &response);
                break;
            }
            Err(ReadLineError::Io) => break,
        }
    }
    shared.clients.fetch_sub(1, Ordering::SeqCst);
}

fn handle_request(shared: &SocketShared, line: &[u8], writer: &mut UnixStream) -> (Option<SocketResponse>, bool) {
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(failure) => return (Some(SocketResponse::error(failure.code, failure.message)), false),
    };
    match request {
        SocketRequest::Ping => (Some(SocketResponse::Pong), false),
        SocketRequest::Attach { surface, session } => {
            let (reply, receiver) = mpsc::channel();
            if !shared.connected.load(Ordering::SeqCst) {
                return (Some(offline_response()), false);
            }
            if shared
                .inbox
                .send(SocketEvent::Attach {
                    surface,
                    session,
                    reply,
                })
                .is_err()
            {
                return (Some(offline_response()), false);
            }
            (Some(wait_for_reply(&receiver, shared, None)), false)
        }
        SocketRequest::Detach { surface, session } => {
            let (reply, receiver) = mpsc::channel();
            if !shared.connected.load(Ordering::SeqCst) {
                return (Some(offline_response()), false);
            }
            if shared
                .inbox
                .send(SocketEvent::Detach {
                    surface,
                    session,
                    reply,
                })
                .is_err()
            {
                return (Some(offline_response()), false);
            }
            (Some(wait_for_reply(&receiver, shared, None)), false)
        }
        SocketRequest::ToolCall {
            request_id,
            surface,
            session,
            tool,
            arguments,
        } => {
            // Only a cacheable read keeps the values needed for an offline answer. They
            // are cloned because the original values move into the outbound frame.
            let cache_key = is_cacheable_tool(&tool).then(|| (surface.clone(), tool.clone(), arguments.clone()));
            let (reply, receiver) = mpsc::channel();
            if !shared.connected.load(Ordering::SeqCst) {
                let response = offline_tool_result(&request_id);
                return (
                    Some(with_cached_fallback(shared, &request_id, cache_key.as_ref(), response)),
                    false,
                );
            }
            let call = ToolCall {
                request_id: request_id.clone(),
                surface,
                session,
                tool,
                arguments,
            };
            if shared.inbox.send(SocketEvent::ToolCall { call, reply }).is_err() {
                let response = offline_tool_result(&request_id);
                return (
                    Some(with_cached_fallback(shared, &request_id, cache_key.as_ref(), response)),
                    false,
                );
            }
            let response = wait_for_reply(&receiver, shared, Some(&request_id));
            (
                Some(with_cached_fallback(shared, &request_id, cache_key.as_ref(), response)),
                false,
            )
        }
        SocketRequest::Pair {
            code,
            surface,
            session,
            app,
        } => {
            let (reply, receiver) = mpsc::channel();
            if !shared.connected.load(Ordering::SeqCst) {
                return (Some(offline_response()), false);
            }
            let request = crate::client::SurfacePairRequest::new(code, surface, session, app);
            if shared.inbox.send(SocketEvent::Pair { request, reply }).is_err() {
                return (Some(offline_response()), false);
            }
            let reply = wait_for_pair_reply(&receiver, shared);
            let _ = write_pair_reply(writer, &reply);
            (None, true)
        }
    }
}

fn wait_for_reply(
    receiver: &Receiver<SocketResponse>,
    shared: &SocketShared,
    tool_request: Option<&str>,
) -> SocketResponse {
    let started = Instant::now();
    loop {
        match receiver.recv_timeout(SOCKET_POLL) {
            Ok(response) => return response,
            Err(RecvTimeoutError::Disconnected) => return offline_for(tool_request),
            Err(RecvTimeoutError::Timeout) => {
                if !shared.connected.load(Ordering::SeqCst) {
                    return offline_for(tool_request);
                }
                if started.elapsed() >= REPLY_TIMEOUT {
                    return match tool_request {
                        Some(request_id) => SocketResponse::tool_error(
                            request_id,
                            "tool_timeout",
                            "the agent did not answer the tool call",
                            true,
                        ),
                        None => SocketResponse::error("agent_timeout", "the agent did not answer the local request"),
                    };
                }
            }
        }
    }
}

fn offline_for(tool_request: Option<&str>) -> SocketResponse {
    match tool_request {
        Some(request_id) => offline_tool_result(request_id),
        None => offline_response(),
    }
}

/// Waits for the frame loop to answer one pairing request.
fn wait_for_pair_reply(receiver: &Receiver<PairReply>, shared: &SocketShared) -> PairReply {
    let started = Instant::now();
    loop {
        match receiver.recv_timeout(SOCKET_POLL) {
            Ok(reply) => return reply,
            Err(RecvTimeoutError::Disconnected) => {
                return PairReply::error(
                    "home_offline",
                    "the agent is not connected to the home; start `vowp agent run`",
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                if !shared.connected.load(Ordering::SeqCst) {
                    return PairReply::error(
                        "home_offline",
                        "the agent is not connected to the home; start `vowp agent run`",
                    );
                }
                if started.elapsed() >= REPLY_TIMEOUT {
                    return PairReply::error("pair_timeout", "the agent did not answer the pairing request");
                }
            }
        }
    }
}

fn offline_response() -> SocketResponse {
    SocketResponse::error(
        "home_offline",
        "the agent is not connected to the home; start `vowp agent run`",
    )
}

fn offline_tool_result(request_id: &str) -> SocketResponse {
    SocketResponse::tool_error(
        request_id,
        "home_offline",
        "the agent is not connected to the home; start `vowp agent run`",
        true,
    )
}

/// Replaces an offline or timed-out read with a fresh cached result when there is one.
///
/// The cache key is present only for the six cacheable read tools, so a write tool keeps
/// its exact `home_offline` or `tool_timeout` error and never reads the cache.
fn with_cached_fallback(
    shared: &SocketShared,
    request_id: &str,
    cache_key: Option<&(SurfaceId, String, Value)>,
    response: SocketResponse,
) -> SocketResponse {
    if !is_cache_fallback(&response) {
        return response;
    }
    let Some((surface, tool, arguments)) = cache_key else {
        return response;
    };
    match shared.cache.lookup(tool, surface, arguments, Timestamp::now()) {
        Ok(Some(cached)) => SocketResponse::ToolResult {
            request_id: request_id.to_owned(),
            result: Some(cached_result(&cached)),
            error: None,
        },
        Ok(None) => response,
        Err(error) => {
            debug_failure("cache lookup failed", &error);
            response
        }
    }
}

fn is_cache_fallback(response: &SocketResponse) -> bool {
    match response {
        SocketResponse::ToolResult { error: Some(error), .. } => {
            matches!(error.code.as_str(), "home_offline" | "tool_timeout")
        }
        _ => false,
    }
}

/// Returns the cached MCP envelope with one extra text line naming when it was stored.
fn cached_result(cached: &CachedRead) -> Value {
    let marker = format!(
        "as of {} (the home is unreachable; this is a cached read)",
        cached.stored_at.as_rfc3339()
    );
    let mut envelope = match &cached.result {
        Value::Object(map) => Value::Object(map.clone()),
        other => serde_json::json!({
            "content": [{"type": "text", "text": other.to_string()}],
            "isError": false,
        }),
    };
    if let Some(object) = envelope.as_object_mut() {
        let content = object.entry("content").or_insert_with(|| Value::Array(Vec::new()));
        if !content.is_array() {
            *content = Value::Array(Vec::new());
        }
        if let Some(items) = content.as_array_mut() {
            items.push(serde_json::json!({"type": "text", "text": marker}));
        }
        object.insert("isError".to_owned(), Value::Bool(false));
    }
    envelope
}

struct SocketFailure {
    code: &'static str,
    message: String,
}

fn parse_request(line: &[u8]) -> Result<SocketRequest, SocketFailure> {
    let value: Value = serde_json::from_slice(line).map_err(|_| invalid_request())?;
    let kind = value.get("kind").and_then(Value::as_str).ok_or_else(invalid_request)?;
    if !matches!(kind, "ping" | "attach" | "detach" | "tool_call" | "pair") {
        return Err(SocketFailure {
            code: "unknown_kind",
            message: "unknown request kind".to_owned(),
        });
    }
    let request: SocketRequest = serde_json::from_value(value).map_err(|_| invalid_request())?;
    validate_request(&request)?;
    Ok(request)
}

fn invalid_request() -> SocketFailure {
    SocketFailure {
        code: "invalid_request",
        message: "the request is not a valid socket request".to_owned(),
    }
}

fn validate_request(request: &SocketRequest) -> Result<(), SocketFailure> {
    match request {
        SocketRequest::Ping => Ok(()),
        SocketRequest::Attach { surface, .. } | SocketRequest::Detach { surface, .. } => validate_surface_id(surface)
            .map_err(|_| SocketFailure {
                code: "invalid_request",
                message: "surface must be a 26-character ULID".to_owned(),
            }),
        SocketRequest::ToolCall {
            request_id,
            surface,
            tool,
            arguments,
            ..
        } => {
            if request_id.is_empty() || request_id.len() > 128 || request_id.chars().any(char::is_control) {
                return Err(SocketFailure {
                    code: "invalid_request",
                    message: "request_id must be a printable string of at most 128 bytes".to_owned(),
                });
            }
            if tool.is_empty() || tool.len() > 128 || tool.chars().any(char::is_control) {
                return Err(SocketFailure {
                    code: "invalid_request",
                    message: "tool must be a printable name of at most 128 bytes".to_owned(),
                });
            }
            if !arguments.is_object() {
                return Err(SocketFailure {
                    code: "invalid_request",
                    message: "arguments must be a JSON object".to_owned(),
                });
            }
            validate_surface_id(surface).map_err(|_| SocketFailure {
                code: "invalid_request",
                message: "surface must be a 26-character ULID".to_owned(),
            })
        }
        SocketRequest::Pair { code, surface, app, .. } => {
            crate::client::validate_pairing_code(code).map_err(|_| SocketFailure {
                code: "invalid_request",
                message: "pairing code must be 52 characters".to_owned(),
            })?;
            validate_surface_id(surface).map_err(|_| SocketFailure {
                code: "invalid_request",
                message: "surface must be a 26-character ULID".to_owned(),
            })?;
            if app.is_empty() || app.chars().count() > 64 || app.chars().any(char::is_control) {
                return Err(SocketFailure {
                    code: "invalid_request",
                    message: "app must be a printable name of at most 64 characters".to_owned(),
                });
            }
            Ok(())
        }
    }
}

fn read_request_line(reader: &mut BufReader<UnixStream>, line: &mut Vec<u8>) -> Result<bool, ReadLineError> {
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if is_io_timeout(&error) => return Err(ReadLineError::Timeout),
            Err(_) => return Err(ReadLineError::Io),
        };
        if buffer.is_empty() {
            return Ok(false);
        }
        if let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
            let total = line.len().saturating_add(end).saturating_add(1);
            if total > MAX_SOCKET_REQUEST_BYTES {
                return Err(ReadLineError::TooLarge);
            }
            line.extend_from_slice(&buffer[..=end]);
            reader.consume(end + 1);
            return Ok(true);
        }
        let consumed = buffer.len();
        let total = line.len().saturating_add(consumed);
        if total > MAX_SOCKET_REQUEST_BYTES {
            return Err(ReadLineError::TooLarge);
        }
        line.extend_from_slice(buffer);
        reader.consume(consumed);
    }
}

fn is_io_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

enum ReadLineError {
    Timeout,
    TooLarge,
    Io,
}

fn write_response(writer: &mut UnixStream, response: &SocketResponse) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Writes one pairing answer on its own connection.
fn write_pair_reply(writer: &mut UnixStream, reply: &PairReply) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(reply).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

/// Tries one pairing request over a live `vowp agent run` socket.
///
/// `Ok(None)` means the socket is missing, is not a private Unix socket, or is no longer
/// accepting connections; the caller may then open one direct agent connection. A response
/// is returned unchanged, including `home_offline`. The pairing code is written once and
/// never logged; the token only reaches the disk file the running client writes.
pub fn pair_over_socket(
    state_dir: &Path,
    request: &crate::client::SurfacePairRequest,
) -> Result<Option<PairReply>, AgentError> {
    let path = state_dir.join(AGENT_SOCKET_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Ok(None);
    }
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    // macOS/BSD may inherit O_NONBLOCK; a read timeout does not make a nonblocking stream
    // block. Pairing is one request and one reply, so blocking I/O is correct here.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(crate::client::PAIR_TIMEOUT + Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(crate::client::PAIR_TIMEOUT + Duration::from_secs(5)));
    let request = SocketRequest::Pair {
        code: request.code.clone(),
        surface: request.surface_id.clone(),
        session: request.session.clone(),
        app: request.app.clone(),
    };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
        return Ok(None);
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => Ok(None),
        Ok(_) => {
            let reply: PairReply = serde_json::from_str(&line)
                .map_err(|_| AgentError::Protocol("the agent returned an unreadable pairing response".to_owned()))?;
            Ok(Some(reply))
        }
    }
}
