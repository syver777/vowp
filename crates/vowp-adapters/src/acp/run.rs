//! Process and JSON-RPC transport for one ACP prompt turn.
//! Spec: `docs/design/adapters.md` §10 and `docs/design/v0.2/acp-adapter.md`.
//!
//! ACP is JSON-RPC 2.0 over the child's stdin/stdout, so this run cannot use
//! [`crate::proc::RunHandle`] (which writes stdin once and closes it). The driver thread owns
//! the child, writes `initialize`, `session/new` and `session/prompt`, dispatches notifications
//! and permission requests until the prompt response arrives, then reaps the child. The process
//! stays in its own process group; stop, interrupt and timeout signal the whole group.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vowp_protocol::{AdapterError, AdapterRun, Executor, HarnessEvent, RunSummary, Timestamp};

use super::mapping::{AcpTurn, EndKind};
use crate::proc::{Signal, SpawnSpec, signal_group};

/// A stdout line longer than this is truncated before parsing.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// How long [`AcpRun::stop`] waits after SIGTERM before sending SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// How long [`AcpRun::interrupt`] waits after SIGINT before sending SIGKILL.
const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// How long the driver waits for the server to exit after stdin closes.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// How long the driver waits after SIGTERM before sending SIGKILL while reaping.
const REAP_KILL_GRACE: Duration = Duration::from_secs(1);

/// Everything the driver needs from one [`crate::acp::AcpAdapter::start`] call.
pub(crate) struct RunOptions {
    /// The user/continue/seed prompt for this turn.
    pub prompt: String,
    /// Working directory sent in `session/new`.
    pub cwd: PathBuf,
    /// Executor that owns the run.
    pub executor: Executor,
    /// Fixed offset used to interpret clock-time reset texts.
    pub tz_offset_minutes: i32,
    /// ACP protocol version sent in `initialize`.
    pub protocol_version: u32,
}

/// One running ACP server and the thread driving it.
pub(crate) struct AcpRun {
    pid: u32,
    rx: Receiver<HarnessEvent>,
    driver: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    stopped: bool,
    grace: Duration,
    interrupt_grace: Duration,
}

struct Shared {
    killed: AtomicBool,
    done: AtomicBool,
    summary: Mutex<Option<RunSummary>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            killed: AtomicBool::new(false),
            done: AtomicBool::new(false),
            summary: Mutex::new(None),
        }
    }
}

impl AcpRun {
    /// Spawns the ACP server described by `spec` and starts the driver thread.
    pub(crate) fn start(spec: SpawnSpec, options: RunOptions) -> Result<AcpRun, AdapterError> {
        let Some(binary) = spec.argv.first().cloned() else {
            return Err(AdapterError::Unavailable("empty argv".to_owned()));
        };

        let mut command = Command::new(&binary);
        command
            .args(&spec.argv[1..])
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = command
            .spawn()
            .map_err(|source| AdapterError::Spawn { binary, source })?;
        let pid = child.id();

        let Some(stdin) = child.stdin.take() else {
            let _ = signal_group(pid, Signal::Kill);
            return Err(AdapterError::Protocol("child stdin was not piped".to_owned()));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = signal_group(pid, Signal::Kill);
            return Err(AdapterError::Protocol("child stdout was not piped".to_owned()));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = signal_group(pid, Signal::Kill);
            return Err(AdapterError::Protocol("child stderr was not piped".to_owned()));
        };

        let stdout_tee = open_tee(spec.tee_stdout.as_deref());
        let stderr_tee_path = spec.tee_stderr.as_deref().map(append_err_suffix);
        let stderr_tee = open_tee(stderr_tee_path.as_deref());
        let stderr_thread = spawn_stderr_reader(stderr, stderr_tee);

        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        if let Some(timeout) = spec.timeout {
            spawn_timeout_watchdog(pid, timeout, STOP_GRACE, shared.clone(), cancel_rx);
        } else {
            drop(cancel_rx);
        }

        let driver_shared = shared.clone();
        let driver = thread::Builder::new()
            .name(format!("vowp-acp-{pid}"))
            .spawn(move || {
                let mut transport = Transport::new(stdin, stdout, stdout_tee);
                let mut turn = AcpTurn::new(options.executor.clone(), Timestamp::now(), options.tz_offset_minutes);
                let end = conversation(&mut transport, &mut turn, &tx, &options);
                transport.close_stdin();
                let exit = reap(&mut child, pid);
                let stderr_tail = stderr_thread.join().unwrap_or_default();
                let killed = driver_shared.killed.load(Ordering::SeqCst);
                let (events, summary) = turn.finish(end, killed, exit, &String::from_utf8_lossy(&stderr_tail));
                for event in events {
                    let _ = tx.send(event);
                }
                *driver_shared.summary.lock().unwrap_or_else(PoisonError::into_inner) = Some(summary);
                driver_shared.done.store(true, Ordering::SeqCst);
                drop(cancel_tx);
            })
            .map_err(AdapterError::Io)?;

        Ok(AcpRun {
            pid,
            rx,
            driver: Some(driver),
            shared,
            stopped: false,
            grace: STOP_GRACE,
            interrupt_grace: INTERRUPT_GRACE,
        })
    }

    /// Returns true once the driver thread has finished.
    fn finished(&self) -> bool {
        self.driver.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Polls until the driver finishes or `grace` elapses.
    fn wait_for_grace(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while !self.finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl AdapterRun for AcpRun {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.rx
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("live_input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        if self.stopped {
            return self.wait().map(|_| ());
        }
        self.shared.killed.store(true, Ordering::SeqCst);
        signal_group(self.pid, Signal::Int)?;
        self.wait_for_grace(self.interrupt_grace);
        if !self.finished() {
            return self.stop();
        }
        self.stopped = true;
        self.wait().map(|_| ())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        if self.stopped {
            return self.wait().map(|_| ());
        }
        self.stopped = true;
        self.shared.killed.store(true, Ordering::SeqCst);
        signal_group(self.pid, Signal::Term)?;
        self.wait_for_grace(self.grace);
        if !self.finished() {
            signal_group(self.pid, Signal::Kill)?;
        }
        self.wait().map(|_| ())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        if let Some(driver) = self.driver.take() {
            match driver.join() {
                Ok(()) => {}
                Err(_) => return Err(AdapterError::Protocol("run thread panicked".to_owned())),
            }
        }
        let summary = self
            .shared
            .summary
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        summary.ok_or_else(|| AdapterError::Protocol("run finished without a summary".to_owned()))
    }
}

/// Writes one JSON-RPC line and reads the child's stdout.
struct Transport {
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    tee: Option<File>,
    next_id: u64,
}

impl Transport {
    /// Wraps the child's pipes.
    fn new(stdin: ChildStdin, stdout: ChildStdout, tee: Option<File>) -> Self {
        Self {
            stdin: Some(BufWriter::new(stdin)),
            stdout: BufReader::new(stdout),
            tee,
            next_id: 0,
        }
    }

    /// Sends one request and returns its id.
    fn send_request(&mut self, method: &str, params: Value) -> io::Result<u64> {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        Ok(id)
    }

    /// Sends one JSON-RPC message.
    fn send(&mut self, message: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "ACP stdin is closed"));
        };
        stdin.write_all(&bytes)?;
        stdin.flush()
    }

    /// Reads one stdout line, teeing it raw before parsing.
    fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut raw = Vec::new();
        match self.stdout.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => return Ok(None),
            Ok(_) => {}
        }
        if let Some(tee) = self.tee.as_mut() {
            let _ = tee.write_all(&raw);
            let _ = tee.flush();
        }
        let truncated = raw.len() > MAX_LINE_BYTES;
        let bytes = if truncated { &raw[..MAX_LINE_BYTES] } else { &raw[..] };
        let mut line = String::from_utf8_lossy(bytes).into_owned();
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        Ok(Some(line))
    }

    /// Closes the child's stdin; ACP servers normally exit on EOF after one prompt.
    fn close_stdin(&mut self) {
        self.stdin.take();
    }
}

/// Runs the pinned ACP subset up to the prompt result.
fn conversation(
    transport: &mut Transport,
    turn: &mut AcpTurn,
    tx: &Sender<HarnessEvent>,
    options: &RunOptions,
) -> EndKind {
    let initialize = json!({
        "protocolVersion": options.protocol_version,
        "clientCapabilities": {},
        "clientInfo": {
            "name": "vowp",
            "version": env!("CARGO_PKG_VERSION"),
        },
    });
    if let Err(error) = call(transport, turn, tx, "initialize", initialize) {
        return end_for(error, turn);
    }

    let session_new = json!({
        "cwd": options.cwd.to_string_lossy(),
        "mcpServers": [],
    });
    let result = match call(transport, turn, tx, "session/new", session_new) {
        Ok(result) => result,
        Err(error) => return end_for(error, turn),
    };
    let Some(session_id) = result.get("sessionId").and_then(Value::as_str) else {
        send_events(
            tx,
            turn.on_protocol("ACP session/new result is missing sessionId", Some(result)),
        );
        return end_for(CallError::Protocol, turn);
    };
    turn.set_session_id(session_id.to_owned());

    let prompt = json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": options.prompt}],
    });
    match call(transport, turn, tx, "session/prompt", prompt) {
        Ok(result) => {
            send_events(tx, turn.on_prompt_result(&result));
            if turn.permission_refused() {
                EndKind::Permission
            } else if turn.has_result() {
                EndKind::Result
            } else {
                EndKind::Protocol
            }
        }
        Err(error) => end_for(error, turn),
    }
}

/// Why a request/response exchange stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallError {
    Eof,
    Rpc,
    Protocol,
    /// The server asked for permission; the turn ends without waiting for its result.
    Permission,
}

/// Maps a call error to the conversation end, letting a refusal win.
fn end_for(error: CallError, turn: &AcpTurn) -> EndKind {
    if turn.permission_refused() {
        return EndKind::Permission;
    }
    match error {
        CallError::Eof => EndKind::Eof,
        CallError::Rpc => EndKind::RpcError,
        CallError::Protocol => EndKind::Protocol,
        CallError::Permission => EndKind::Permission,
    }
}

/// Sends one request and dispatches notifications and agent requests until its response.
fn call(
    transport: &mut Transport,
    turn: &mut AcpTurn,
    tx: &Sender<HarnessEvent>,
    method: &str,
    params: Value,
) -> Result<Value, CallError> {
    let id = transport.send_request(method, params).map_err(|_| CallError::Eof)?;
    loop {
        let Some(line) = transport.read_line().map_err(|_| CallError::Eof)? else {
            return Err(CallError::Eof);
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                send_events(tx, turn.on_malformed(&line, &error.to_string()));
                return Err(CallError::Protocol);
            }
        };

        let message_id = message.get("id");
        match message.get("method").and_then(Value::as_str) {
            Some(agent_method) => {
                if agent_method == "session/update" {
                    let events = turn.on_update(message.get("params").unwrap_or(&Value::Null));
                    send_events(tx, events);
                } else if agent_method == "session/request_permission" {
                    let events = turn.on_permission(message.get("params").unwrap_or(&Value::Null));
                    send_events(tx, events);
                    if let Some(request_id) = message_id {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": {"outcome": {"outcome": "cancelled"}},
                        });
                        transport.send(&response).map_err(|_| CallError::Eof)?;
                    }
                    // The turn is over once VOWP refuses; waiting for a server that may
                    // ignore the cancellation would trade a clean failure for a hang.
                    return Err(CallError::Permission);
                } else {
                    // An unknown method is a protocol failure; a request also gets a
                    // JSON-RPC -32601 so the server is not left waiting for an answer.
                    let events = turn.on_unknown_method(agent_method, &message);
                    send_events(tx, events);
                    if let Some(request_id) = message_id {
                        let response = json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": format!("Method not found: {agent_method}"),
                            },
                        });
                        let _ = transport.send(&response);
                    }
                    return Err(CallError::Protocol);
                }
            }
            None => {
                let Some(response_id) = message_id else {
                    send_events(tx, turn.on_malformed(&line, "message has no method or id"));
                    return Err(CallError::Protocol);
                };
                if response_id.as_u64() != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let events = turn.on_rpc_error(error);
                    send_events(tx, events);
                    return Err(CallError::Rpc);
                }
                return message.get("result").cloned().ok_or(CallError::Protocol);
            }
        }
    }
}

/// Sends mapper events to the run channel, ignoring a dropped receiver.
fn send_events(tx: &Sender<HarnessEvent>, events: Vec<HarnessEvent>) {
    for event in events {
        let _ = tx.send(event);
    }
}

/// Waits for the server to exit, then terminates and kills the group after the grace periods.
fn reap(child: &mut Child, pid: u32) -> Option<i32> {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    let _ = signal_group(pid, Signal::Term);
    let deadline = Instant::now() + REAP_KILL_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    let _ = signal_group(pid, Signal::Kill);
    child.wait().ok().and_then(|status| status.code())
}

/// Spawns the stderr reader and returns its 4 KiB tail through the join handle.
fn spawn_stderr_reader(stderr: std::process::ChildStderr, mut tee: Option<File>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut raw = Vec::new();
        let mut tail = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Some(file) = tee.as_mut() {
                let _ = file.write_all(&raw);
                let _ = file.flush();
            }
            let mut line = String::from_utf8_lossy(&raw).into_owned();
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            tail.extend_from_slice(line.as_bytes());
            tail.push(b'\n');
            if tail.len() > 4 * 1024 {
                let excess = tail.len() - 4 * 1024;
                tail.drain(..excess);
            }
        }
        tail
    })
}

/// Kills the group after `timeout`, escalating after the grace period.
fn spawn_timeout_watchdog(pid: u32, timeout: Duration, grace: Duration, shared: Arc<Shared>, cancel_rx: Receiver<()>) {
    let _ = thread::Builder::new()
        .name(format!("vowp-acp-timeout-{pid}"))
        .spawn(move || match cancel_rx.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => {
                shared.killed.store(true, Ordering::SeqCst);
                let _ = signal_group(pid, Signal::Term);
                let deadline = Instant::now() + grace;
                while !shared.done.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if !shared.done.load(Ordering::SeqCst) {
                    let _ = signal_group(pid, Signal::Kill);
                }
            }
            Err(RecvTimeoutError::Disconnected) | Ok(()) => {}
        });
}

/// Appends `.err` to a tee path.
fn append_err_suffix(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".err");
    PathBuf::from(name)
}

/// Opens a tee file for appending, creating it when absent.
fn open_tee(path: Option<&Path>) -> Option<File> {
    let path = path?;
    OpenOptions::new().create(true).append(true).open(path).ok()
}
