//! The opencode app host: the managed v2 service, its pair link and its transcript.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.5. The host runs opencode v2 as a managed
//! user service under a **private** `XDG_STATE_HOME`/`XDG_DATA_HOME` below the orchestrator
//! directory, binds `127.0.0.1` only, and talks to the server with HTTP Basic
//! `opencode:<token>`. The pair link is printed on demand by `opencode pair`; this module
//! never stores or logs it.
//!
//! The HTTP client is deliberately small and blocking: opencode is a loopback service and
//! the workspace already prefers plain threads over an async runtime.
//!
//! The live reader holds one persistent `/api/event` subscription from host start and appends
//! every frame to a private spool. The 2.0.28 stream is subscription-only and volatile (its
//! own route description says events during a disconnection are missed), so a per-pass read
//! would lose events; the spool is replayed to the mirror and filtered to the target session.
//! Assistant text parts are coalesced by `assistantMessageID`; `session.usage.updated` carries
//! a cumulative snapshot, so the mirror writes only each update's delta as cost.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vowp_protocol::{JoinInstructions, OrchestratorApp, Timestamp};

use crate::CoreError;

use super::mirror::MirrorCursor;
use super::native::{
    AppHandle, AppHost, HostHealth, HostRequest, TranscriptBatch, TranscriptEntry, TranscriptKind, process_alive,
    remove_private, shell_quote_text, stop_pid, write_private_json,
};

/// How long `start` waits for the service and its first authenticated answer.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one HTTP request may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// How many times an idempotent request is sent before a stalled response is reported.
const HTTP_ATTEMPTS: usize = 2;

/// How long one `/api/event` read may take after the request is sent.
const SSE_WINDOW: Duration = Duration::from_millis(750);

/// The opencode app host.
///
/// Construct it with [`OpencodeHost::new`] in tests to point at a fake binary; the default
/// runs `opencode` from `PATH`.
#[derive(Debug, Clone)]
pub struct OpencodeHost {
    binary: PathBuf,
    startup: Duration,
    sse_window: Duration,
}

impl Default for OpencodeHost {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("opencode"),
            startup: STARTUP_TIMEOUT,
            sse_window: SSE_WINDOW,
        }
    }
}

impl OpencodeHost {
    /// Creates a host that runs `binary`.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            startup: STARTUP_TIMEOUT,
            sse_window: SSE_WINDOW,
        }
    }

    /// Overrides how long `start` waits for the service.
    pub fn with_startup(mut self, timeout: Duration) -> Self {
        self.startup = timeout;
        self
    }

    /// Overrides how long one live-event read waits.
    pub fn with_sse_window(mut self, window: Duration) -> Self {
        self.sse_window = window;
        self
    }

    /// Returns the shell command that prints a fresh one-time pair link.
    ///
    /// The command is safe to show; the link it prints is a secret with a five-minute life.
    pub fn pair_command(&self, handle: &AppHandle) -> Result<String, CoreError> {
        let layout = Layout::for_workdir(&handle.workdir);
        if read_host_state(&layout)?.is_none() {
            return Err(CoreError::NotFound("opencode host".to_owned()));
        }
        Ok(format!(
            "cd {} && XDG_STATE_HOME={} {} pair",
            shell_quote(&layout.workdir),
            shell_quote(&layout.state),
            shell_quote(&self.binary)
        ))
    }

    /// Reads the newest transcript entries after `cursor`.
    ///
    /// The first pass backfills from the session export. Every pass then maps the listener
    /// spool, so an event that happened between two passes is not lost; the spool is replayed
    /// in full and the mirror's `native_events` map keeps the pass idempotent.
    fn read_transcript(&self, handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
        let layout = Layout::for_workdir(&handle.workdir);
        let Some(state) = read_host_state(&layout)? else {
            return Ok(TranscriptBatch {
                entries: Vec::new(),
                cursor: cursor.clone(),
            });
        };
        let native = state
            .native_session_id
            .clone()
            .ok_or_else(|| CoreError::Invalid("opencode host has no app session".to_owned()))?;
        let client = self.client(&layout)?;
        let mut entries = Vec::new();
        if cursor.last_native_id.is_none() {
            entries.extend(self.backfill(&client, &native)?);
        }
        entries.extend(self.live_events(&layout, &native)?);
        for entry in &mut entries {
            if entry.native_session.is_none() {
                entry.native_session = Some(native.clone());
            }
        }
        // The spool is replayed in full so a turn start that the export does not contain
        // cannot be skipped by a content-id cursor. `native_events` makes the pass
        // idempotent; the cursor still records the last entry for persistence.
        let last = entries
            .last()
            .map(|entry| entry.native_id.clone())
            .or_else(|| cursor.last_native_id.clone());
        Ok(TranscriptBatch {
            entries,
            cursor: MirrorCursor {
                last_native_id: last,
                updated_at: Timestamp::now(),
            },
        })
    }

    /// Backfills from the export endpoint, falling back to the message list.
    fn backfill(&self, client: &HttpClient, native: &str) -> Result<Vec<TranscriptEntry>, CoreError> {
        if super::native::safe_segment(native) {
            let export = format!("/api/experimental/session/{native}/export");
            if let Ok((status, value)) = client.get_json(&export) {
                if (200..300).contains(&status) {
                    let entries = parse_message_list(&value);
                    if !entries.is_empty() {
                        return Ok(entries);
                    }
                }
            }
            let messages = format!("/api/session/{native}/message");
            if let Ok((status, value)) = client.get_json(&messages) {
                if (200..300).contains(&status) {
                    return Ok(parse_message_list(&value));
                }
            }
        }
        Ok(Vec::new())
    }

    /// Reads the target session's events from the listener spool.
    ///
    /// The global `/api/event` stream covers every location and is subscription-only (the
    /// 2.0.28 route description: events during a disconnection are missed). A
    /// background listener keeps the subscription open from host start and appends every
    /// event to a private spool, so a pass cannot lose an event that happened between passes.
    /// The spool is filtered to this app session before mapping, so another session's
    /// transcript can never be written into this one.
    fn live_events(&self, layout: &Layout, native: &str) -> Result<Vec<TranscriptEntry>, CoreError> {
        // A missing service state is not fatal; the spool may already hold events.
        let _ = ensure_event_listener(layout);
        let events = read_event_spool(&layout.event_spool)?;
        let target: Vec<Value> = events
            .into_iter()
            .filter(|event| session_of(event).as_deref() == Some(native))
            .collect();
        Ok(map_events(&target, false))
    }

    /// Builds a client from the service state in `layout`.
    fn client(&self, layout: &Layout) -> Result<HttpClient, CoreError> {
        let service = read_service_state(layout)?;
        let url = service
            .url
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", service.port.unwrap_or_default()));
        HttpClient::new(&url, &service.password)
    }

    /// Runs one `opencode service …` command in the private state tree.
    fn run_service(&self, layout: &Layout, args: &[&str]) -> Result<(), CoreError> {
        let status = self
            .service_command(layout, args)?
            .status()
            .map_err(|source| CoreError::Io {
                path: self.binary.display().to_string(),
                source,
            })?;
        if !status.success() {
            // The log is private; never echo it, in case the client logs the pair link.
            return Err(CoreError::Command(format!(
                "opencode {} failed: exit {:?}; see {}",
                args.join(" "),
                status.code(),
                layout.service_log.display()
            )));
        }
        Ok(())
    }

    /// Runs one `opencode service …` command, ignoring a non-zero exit.
    fn run_service_best_effort(&self, layout: &Layout, args: &[&str]) {
        if let Ok(mut command) = self.service_command(layout, args) {
            let _ = command.status();
        }
    }

    /// Builds the isolated service command.
    ///
    /// Output goes to the private `service.log`; a background child that inherits the
    /// write end must never keep `Command::output` blocked.
    fn service_command(&self, layout: &Layout, args: &[&str]) -> Result<Command, CoreError> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&layout.service_log)
            .map_err(|source| CoreError::Io {
                path: layout.service_log.display().to_string(),
                source,
            })?;
        let _ = fs::set_permissions(&layout.service_log, fs::Permissions::from_mode(0o600));
        let errors = log.try_clone().map_err(|source| CoreError::Io {
            path: layout.service_log.display().to_string(),
            source,
        })?;
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .current_dir(&layout.workdir)
            .env("XDG_CONFIG_HOME", &layout.config)
            .env("XDG_DATA_HOME", &layout.data)
            .env("XDG_STATE_HOME", &layout.state)
            .env("XDG_CACHE_HOME", &layout.cache)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors));
        Ok(command)
    }
}

impl AppHost for OpencodeHost {
    fn kind(&self) -> OrchestratorApp {
        OrchestratorApp::Opencode
    }

    fn start(&self, req: &HostRequest) -> Result<AppHandle, CoreError> {
        let layout = Layout::for_workdir(&req.workdir);
        fs::create_dir_all(&layout.root).map_err(|source| CoreError::Io {
            path: layout.root.display().to_string(),
            source,
        })?;
        let _ = fs::set_permissions(&layout.root, fs::Permissions::from_mode(0o700));

        let previous = read_host_state(&layout)?;
        if let Some(state) = &previous {
            if process_alive(state.pid) {
                return Err(CoreError::Invalid(format!(
                    "opencode host for {} is already running (pid {})",
                    layout.workdir.display(),
                    state.pid
                )));
            }
            remove_private(&layout.host_json);
        }
        let reuse_native = previous.and_then(|state| state.native_session_id);

        let port = match req.port {
            Some(port) if port > 0 => port,
            Some(_) => return Err(CoreError::Invalid("port 0 is not a valid opencode port".to_owned())),
            None => free_port()?,
        };
        self.run_service_best_effort(&layout, &["service", "stop"]);
        remove_private(&layout.service_json);
        self.run_service(&layout, &["service", "set", "port", &port.to_string()])?;
        self.run_service(&layout, &["service", "start"])?;

        let started = (|| -> Result<AppHandle, CoreError> {
            let started_at = Timestamp::now();
            let service = self.wait_for_service(&layout, port)?;
            let client = self.client(&layout)?;
            let native = match reuse_native {
                Some(native) if self.session_exists(&client, &native) => native,
                _ => self.create_native_session(&client, req)?,
            };
            let pid = service
                .pid
                .ok_or_else(|| CoreError::Invalid("opencode service.json has no pid".to_owned()))?;
            let state = HostState {
                kind: "opencode".to_owned(),
                name: req.name.clone(),
                pid,
                port: Some(port),
                url: service
                    .url
                    .clone()
                    .unwrap_or_else(|| format!("http://127.0.0.1:{port}")),
                started_at,
                workdir: layout.workdir.display().to_string(),
                service_json: layout.service_json.display().to_string(),
                native_session_id: Some(native),
                vowp_session: req.session.to_string(),
            };
            write_private_json(&layout.host_json, &state)?;
            start_event_listener(&layout)?;
            Ok(AppHandle {
                kind: OrchestratorApp::Opencode,
                pid,
                port: Some(port),
                started_at,
                workdir: req.workdir.clone(),
            })
        })();
        if started.is_err() {
            self.run_service_best_effort(&layout, &["service", "stop"]);
            remove_private(&layout.host_json);
            remove_private(&layout.service_json);
        }
        started
    }

    fn stop(&self, handle: &AppHandle) -> Result<(), CoreError> {
        let layout = Layout::for_workdir(&handle.workdir);
        stop_event_listener(&layout.workdir);
        self.run_service_best_effort(&layout, &["service", "stop"]);
        let state = read_host_state(&layout)?;
        if let Some(state) = &state {
            stop_pid(state.pid);
        } else if process_alive(handle.pid) {
            stop_pid(handle.pid);
        }
        remove_private(&layout.host_json);
        remove_private(&layout.service_json);
        Ok(())
    }

    fn health(&self, handle: &AppHandle) -> Result<HostHealth, CoreError> {
        let layout = Layout::for_workdir(&handle.workdir);
        if read_host_state(&layout)?.is_none() {
            return Ok(HostHealth {
                ok: false,
                detail: "opencode host is not running".to_owned(),
            });
        }
        let client = match self.client(&layout) {
            Ok(client) => client,
            Err(error) => {
                return Ok(HostHealth {
                    ok: false,
                    detail: error.to_string(),
                });
            }
        };
        match client.get_json("/api/session") {
            Ok((status, _)) if (200..300).contains(&status) => Ok(HostHealth {
                ok: true,
                detail: "authenticated GET /api/session succeeded".to_owned(),
            }),
            Ok((status, _)) => Ok(HostHealth {
                ok: false,
                detail: format!("opencode answered HTTP {status} on /api/session"),
            }),
            Err(error) => Ok(HostHealth {
                ok: false,
                detail: error.to_string(),
            }),
        }
    }

    fn join_instructions(&self, handle: &AppHandle) -> Result<JoinInstructions, CoreError> {
        let layout = Layout::for_workdir(&handle.workdir);
        let state = read_host_state(&layout)?.ok_or_else(|| CoreError::NotFound("opencode host".to_owned()))?;
        let port = state.port.unwrap_or_default();
        let native = state
            .native_session_id
            .clone()
            .unwrap_or_else(|| "<opencode-session>".to_owned());
        let pair = self.pair_command(handle)?;
        let remote = format!(
            "cd {} && {} --server http://127.0.0.1:{port} --session {}",
            shell_quote(&layout.workdir),
            shell_quote(&self.binary),
            shell_quote_text(&native)
        );
        Ok(JoinInstructions {
            desktop: format!(
                "1. On the control machine: vowp tunnel open {} --remote-port {port}\n\
                 2. On the home: {pair}\n\
                 3. Open the one-time link it prints (5 minutes, single use) in the opencode desktop app, replacing its port with the tunnel's local port.",
                shell_quote_text(&state.name)
            ),
            phone: format!("ssh -t <home-ssh-target> {}", shell_quote_text(&remote)),
            chat: format!("vowp chat {}", state.vowp_session),
        })
    }

    fn transcript_since(&self, handle: &AppHandle, cursor: &MirrorCursor) -> Result<TranscriptBatch, CoreError> {
        self.read_transcript(handle, cursor)
    }
}

impl OpencodeHost {
    /// Polls the service state until an authenticated request answers.
    fn wait_for_service(&self, layout: &Layout, port: u16) -> Result<ServiceState, CoreError> {
        let deadline = Instant::now() + self.startup;
        let mut last = "service.json was not written".to_owned();
        while Instant::now() < deadline {
            match read_service_state(layout) {
                Ok(service) => {
                    let url = service
                        .url
                        .clone()
                        .unwrap_or_else(|| format!("http://127.0.0.1:{port}"));
                    match HttpClient::new(&url, &service.password) {
                        Ok(client) => match client.get_json("/api/session") {
                            Ok((status, _)) if (200..300).contains(&status) => {
                                if let Some(version) = &service.version {
                                    if !version.starts_with("2.") {
                                        self.run_service_best_effort(layout, &["service", "stop"]);
                                        return Err(CoreError::Refused(format!(
                                            "opencode 2.x is required; the service reports {version}"
                                        )));
                                    }
                                }
                                return Ok(service);
                            }
                            Ok((status, _)) => last = format!("opencode answered HTTP {status} on /api/session"),
                            Err(error) => last = error.to_string(),
                        },
                        Err(error) => last = error.to_string(),
                    }
                }
                Err(error) => last = error.to_string(),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.run_service_best_effort(layout, &["service", "stop"]);
        Err(CoreError::Command(format!(
            "opencode service did not become healthy within {:?}: {last}",
            self.startup
        )))
    }

    /// Creates the app-side session in the orchestrator directory.
    fn create_native_session(&self, client: &HttpClient, req: &HostRequest) -> Result<String, CoreError> {
        let body = serde_json::json!({
            "title": req.name,
            "location": {"directory": req.workdir.display().to_string()},
        });
        let (status, value) = client.post_json("/api/session", &body)?;
        if !(200..300).contains(&status) {
            return Err(CoreError::Command(format!(
                "opencode refused POST /api/session with HTTP {status}"
            )));
        }
        value
            .pointer("/data/id")
            .and_then(Value::as_str)
            .or_else(|| value.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Invalid("opencode POST /api/session returned no session id".to_owned()))
    }

    /// Returns true when the named app session still exists.
    fn session_exists(&self, client: &HttpClient, native: &str) -> bool {
        if !super::native::safe_segment(native) {
            return false;
        }
        let path = format!("/api/session/{native}/message");
        matches!(client.get_json(&path), Ok((status, _)) if (200..300).contains(&status))
    }
}

/// The private directory layout of one opencode orchestrator.
#[derive(Debug, Clone)]
struct Layout {
    workdir: PathBuf,
    root: PathBuf,
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    service_json: PathBuf,
    service_log: PathBuf,
    event_spool: PathBuf,
    host_json: PathBuf,
}

impl Layout {
    /// Builds the layout below `workdir`.
    fn for_workdir(workdir: &Path) -> Self {
        let root = workdir.join(".opencode");
        let state = root.join("state");
        Self {
            workdir: workdir.to_path_buf(),
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            service_json: state.join("opencode").join("service.json"),
            service_log: root.join("service.log"),
            event_spool: root.join("event-spool.jsonl"),
            host_json: workdir.join("host.json"),
            root,
            state,
        }
    }
}

/// What `host.json` records while the host runs (mode 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostState {
    kind: String,
    name: String,
    pid: u32,
    port: Option<u16>,
    url: String,
    started_at: Timestamp,
    workdir: String,
    service_json: String,
    native_session_id: Option<String>,
    vowp_session: String,
}

/// The subset of opencode's own `service.json` this host needs.
///
/// Deliberately not `Debug`: `password` is a secret.
#[derive(Clone, Deserialize)]
struct ServiceState {
    version: Option<String>,
    url: Option<String>,
    pid: Option<u32>,
    port: Option<u16>,
    password: String,
}

/// Reads `host.json`, returning `None` when the file is absent.
fn read_host_state(layout: &Layout) -> Result<Option<HostState>, CoreError> {
    match fs::read(&layout.host_json) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CoreError::Invalid(format!("host.json: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: layout.host_json.display().to_string(),
            source,
        }),
    }
}

/// Reads opencode's `service.json`.
fn read_service_state(layout: &Layout) -> Result<ServiceState, CoreError> {
    read_service_state_or_none(layout)?
        .ok_or_else(|| CoreError::NotFound(format!("opencode service state {}", layout.service_json.display())))
}

/// Reads opencode's `service.json`, returning `None` when the file is absent.
fn read_service_state_or_none(layout: &Layout) -> Result<Option<ServiceState>, CoreError> {
    match fs::read(&layout.service_json) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| CoreError::Invalid(format!("opencode service.json: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: layout.service_json.display().to_string(),
            source,
        }),
    }
}

/// One persistent `/api/event` subscription.
struct EventListener {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Live listeners keyed by orchestrator workdir. The host object is cheap and stateless;
/// this registry keeps one subscription per running orchestrator.
static EVENT_LISTENERS: OnceLock<Mutex<BTreeMap<PathBuf, EventListener>>> = OnceLock::new();

/// Returns the listener registry.
fn event_listeners() -> &'static Mutex<BTreeMap<PathBuf, EventListener>> {
    EVENT_LISTENERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Ensures a listener is running for the orchestrator in `layout`.
fn ensure_event_listener(layout: &Layout) -> Result<(), CoreError> {
    let running = event_listeners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&layout.workdir);
    if running {
        return Ok(());
    }
    start_event_listener(layout)
}

/// Starts the persistent `/api/event` subscription for `layout`.
fn start_event_listener(layout: &Layout) -> Result<(), CoreError> {
    let service = read_service_state(layout)?;
    let url = service
        .url
        .clone()
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", service.port.unwrap_or_default()));
    let client = HttpClient::new(&url, &service.password)?;
    stop_event_listener(&layout.workdir);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spool = layout.event_spool.clone();
    let thread = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            let _ = client.subscribe_events("/api/event", &thread_stop, |event| {
                let _ = append_event_spool(&spool, &event);
            });
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    if let Ok(mut listeners) = event_listeners().lock() {
        listeners.insert(layout.workdir.clone(), EventListener { stop, thread });
    }
    Ok(())
}

/// Stops the listener for `workdir`, if one is running.
fn stop_event_listener(workdir: &Path) {
    let listener = event_listeners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(workdir);
    if let Some(listener) = listener {
        listener.stop.store(true, Ordering::Relaxed);
        let _ = listener.thread.join();
    }
}

/// Appends one raw event to the private spool, one JSON object per line.
fn append_event_spool(path: &Path, event: &Value) -> Result<(), CoreError> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
    let line = serde_json::to_vec(event).map_err(|error| CoreError::Invalid(error.to_string()))?;
    file.write_all(&line).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.write_all(b"\n").map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;
    file.flush().map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Reads every complete event from the spool; a torn final line is ignored.
fn read_event_spool(path: &Path) -> Result<Vec<Value>, CoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(Value::is_object)
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(CoreError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Returns a free loopback port.
fn free_port() -> Result<u16, CoreError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|source| CoreError::Io {
        path: "127.0.0.1:0".to_owned(),
        source,
    })?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|source| CoreError::Io {
            path: "127.0.0.1:0".to_owned(),
            source,
        })
}

/// Quotes one shell argument with single quotes.
fn shell_quote(value: &Path) -> String {
    shell_quote_text(&value.to_string_lossy())
}

/// A minimal blocking HTTP client for the loopback opencode API.
///
/// Deliberately not `Debug`: `auth` carries the service password.
#[derive(Clone)]
struct HttpClient {
    host: String,
    port: u16,
    auth: String,
    timeout: Duration,
}

impl HttpClient {
    /// Builds a client for `url` and an `opencode:<password>` Basic header.
    fn new(url: &str, password: &str) -> Result<Self, CoreError> {
        let (host, port) = loopback_address(url)?;
        Ok(Self {
            host,
            port,
            auth: format!("Basic {}", base64_encode(format!("opencode:{password}").as_bytes())),
            timeout: HTTP_TIMEOUT,
        })
    }

    /// Sends a request and reads the whole response.
    ///
    /// A GET is retried once: a loopback read can time out before the first byte arrives on
    /// a loaded machine, and a health probe must not report that as a protocol failure. A
    /// POST is never retried because the server may already have created the session its
    /// response names.
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, String), CoreError> {
        let payload = match body {
            Some(body) => serde_json::to_vec(body).map_err(|error| CoreError::Invalid(error.to_string()))?,
            None => Vec::new(),
        };
        let attempts = if method.eq_ignore_ascii_case("GET") {
            HTTP_ATTEMPTS
        } else {
            1
        };
        let mut last = CoreError::Invalid("opencode sent no HTTP response".to_owned());
        for _ in 0..attempts {
            match self.exchange(method, path, body.is_some(), &payload) {
                Ok(response) => return Ok(response),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    /// Performs one request attempt and parses its response.
    fn exchange(&self, method: &str, path: &str, with_body: bool, payload: &[u8]) -> Result<(u16, String), CoreError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).map_err(|source| CoreError::Io {
            path: format!("{}:{}", self.host, self.port),
            source,
        })?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|source| CoreError::Io {
                path: format!("{}:{}", self.host, self.port),
                source,
            })?;
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: {}\r\nAccept: application/json\r\nConnection: close\r\n",
            self.host, self.port, self.auth
        );
        if with_body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).map_err(|source| CoreError::Io {
            path: format!("{}:{}", self.host, self.port),
            source,
        })?;
        if !payload.is_empty() {
            stream.write_all(payload).map_err(|source| CoreError::Io {
                path: format!("{}:{}", self.host, self.port),
                source,
            })?;
        }
        let bytes = read_to_end(&mut stream, self.timeout)?;
        parse_response(&bytes)
    }

    /// Sends a GET and parses a JSON body.
    fn get_json(&self, path: &str) -> Result<(u16, Value), CoreError> {
        let (status, body) = self.request("GET", path, None)?;
        let value = serde_json::from_str(&body).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// Sends a POST with a JSON body and parses a JSON response.
    fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), CoreError> {
        let (status, text) = self.request("POST", path, Some(body))?;
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// Keeps one `/api/event` subscription open and calls `on_event` for each frame.
    ///
    /// The stream is volatile by contract, so the listener must stay connected between
    /// mirror passes. A read timeout lets the stop flag be checked without depending on
    /// the server closing the stream.
    fn subscribe_events(
        &self,
        path: &str,
        stop: &AtomicBool,
        mut on_event: impl FnMut(Value),
    ) -> Result<(), CoreError> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).map_err(|source| CoreError::Io {
            path: format!("{}:{}", self.host, self.port),
            source,
        })?;
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|source| CoreError::Io {
                path: format!("{}:{}", self.host, self.port),
                source,
            })?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: {}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
            self.host, self.port, self.auth
        );
        stream.write_all(request.as_bytes()).map_err(|source| CoreError::Io {
            path: format!("{}:{}", self.host, self.port),
            source,
        })?;

        let mut decoder = SseDecoder::default();
        let mut raw = Vec::new();
        let mut headers_done = false;
        let mut chunk = [0u8; 8192];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    raw.extend_from_slice(&chunk[..read]);
                    if !headers_done {
                        let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
                            continue;
                        };
                        let headers = String::from_utf8_lossy(&raw[..position]);
                        decoder.chunked = headers.lines().any(|line| {
                            line.split_once(':').is_some_and(|(name, value)| {
                                name.trim().eq_ignore_ascii_case("transfer-encoding")
                                    && value.trim().eq_ignore_ascii_case("chunked")
                            })
                        });
                        raw.drain(..position + 4);
                        headers_done = true;
                    }
                    decoder.feed(&raw, &mut on_event);
                    raw.clear();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(source) => {
                    return Err(CoreError::Io {
                        path: format!("{}:{}", self.host, self.port),
                        source,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Reads until EOF, a read timeout or `timeout` overall.
///
/// A signal can interrupt a loopback read under parallel tests; that is retried inside the
/// deadline instead of surfacing as a failed health check.
fn read_to_end(stream: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>, CoreError> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !bytes.is_empty() {
                    break;
                }
            }
            Err(source) => {
                return Err(CoreError::Io {
                    path: "http".to_owned(),
                    source,
                });
            }
        }
    }
    Ok(bytes)
}

/// Parses an HTTP/1.1 response into status and body.
fn parse_response(bytes: &[u8]) -> Result<(u16, String), CoreError> {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(CoreError::Invalid("opencode answered without HTTP headers".to_owned()));
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let body_bytes = &bytes[header_end + 4..];
    let mut lines = headers.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| CoreError::Invalid("opencode answered without a status code".to_owned()))?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            if name == "content-length" {
                length = value.parse().ok();
            }
        }
    }
    let body = if chunked {
        decode_chunked(body_bytes)
    } else {
        match length {
            Some(length) if length <= body_bytes.len() => body_bytes[..length].to_vec(),
            _ => body_bytes.to_vec(),
        }
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// Decodes a chunked body; malformed framing falls back to the raw bytes.
fn decode_chunked(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut position = 0usize;
    while position < bytes.len() {
        let Some(relative) = bytes[position..].windows(2).position(|window| window == b"\r\n") else {
            break;
        };
        let end = position + relative;
        let size_text = String::from_utf8_lossy(&bytes[position..end]);
        let Ok(size) = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16) else {
            return bytes.to_vec();
        };
        if size == 0 {
            break;
        }
        let start = end + 2;
        let stop = start.saturating_add(size).min(bytes.len());
        output.extend_from_slice(&bytes[start..stop]);
        position = stop + 2;
    }
    output
}

/// Incremental parser for an SSE body, including chunked transfer framing.
#[derive(Default)]
struct SseDecoder {
    chunked: bool,
    raw: Vec<u8>,
    line: Vec<u8>,
    remaining: usize,
    crlf: usize,
}

impl SseDecoder {
    /// Feeds response bytes and emits each complete `data:` JSON object.
    fn feed(&mut self, bytes: &[u8], on_event: &mut impl FnMut(Value)) {
        if !self.chunked {
            self.line.extend_from_slice(bytes);
            self.drain_lines(on_event);
            return;
        }
        self.raw.extend_from_slice(bytes);
        loop {
            if self.remaining > 0 {
                if self.raw.is_empty() {
                    break;
                }
                let take = self.remaining.min(self.raw.len());
                self.line.extend_from_slice(&self.raw[..take]);
                self.raw.drain(..take);
                self.remaining -= take;
                if self.remaining == 0 {
                    self.crlf = 2;
                }
            } else if self.crlf > 0 {
                if self.raw.is_empty() {
                    break;
                }
                let take = self.crlf.min(self.raw.len());
                self.raw.drain(..take);
                self.crlf -= take;
            } else {
                let Some(position) = self.raw.windows(2).position(|window| window == b"\r\n") else {
                    break;
                };
                let size_text = String::from_utf8_lossy(&self.raw[..position]);
                let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                    .unwrap_or_default();
                self.raw.drain(..position + 2);
                if size == 0 {
                    break;
                }
                self.remaining = size;
            }
        }
        self.drain_lines(on_event);
    }

    /// Emits complete lines that start with `data:`.
    fn drain_lines(&mut self, on_event: &mut impl FnMut(Value)) {
        while let Some(position) = self.line.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.line.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line);
            let line = text.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
                if value.is_object() {
                    on_event(value);
                }
            }
        }
    }
}

/// Parses `http://host:port[/…]` and refuses anything but loopback.
fn loopback_address(url: &str) -> Result<(String, u16), CoreError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| CoreError::Refused("opencode must use a loopback http URL".to_owned()))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| CoreError::Refused("opencode service URL has no port".to_owned()))?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return Err(CoreError::Refused(format!(
            "opencode must bind 127.0.0.1; refusing {host}"
        )));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| CoreError::Refused(format!("invalid opencode port {port:?}")))?;
    Ok(("127.0.0.1".to_owned(), port))
}

/// Standard padded base64, used only for the HTTP Basic header.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    output
}

/// Parses an opencode message list or export into transcript entries.
///
/// The real v2.0.28 export (`session-export.json`) and message list
/// (`/api/session/<id>/message`) carry a user message's text at the top level and an
/// assistant message's text in `content`, with `cost` and `tokens` at the message level.
/// The message list is newest-first, so entries are sorted by the app's timestamp.
pub fn parse_message_list(value: &Value) -> Vec<TranscriptEntry> {
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| value.get("data").and_then(Value::as_array))
        .or_else(|| value.as_array());
    let Some(messages) = messages else {
        return Vec::new();
    };
    let mut ordered: Vec<(i64, usize, &Value)> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message_created(message), index, message))
        .collect();
    ordered.sort_by_key(|(created, index, _)| (*created, *index));
    let mut entries = Vec::new();
    for (_, index, message) in ordered {
        push_message(message, index, &mut entries);
    }
    entries
}

/// Maps one opencode message (export or message list) into entries.
fn push_message(message: &Value, index: usize, entries: &mut Vec<TranscriptEntry>) {
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| message.get("messageID").and_then(Value::as_str))
        .or_else(|| {
            message
                .get("info")
                .and_then(|info| info.get("id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("message-{index}"));
    let role = message
        .get("type")
        .and_then(Value::as_str)
        .filter(|role| matches!(*role, "user" | "assistant"))
        .or_else(|| message.get("role").and_then(Value::as_str))
        .or_else(|| {
            message
                .get("info")
                .and_then(|info| info.get("role"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default();
    let ts = message_timestamp(message);
    let model = message_model(message);
    let session = message_session(message);
    let content = message
        .get("content")
        .or_else(|| message.get("parts"))
        .or_else(|| message.get("info").and_then(|info| info.get("content")));
    match role {
        "user" => {
            // Real v2.0.28 user messages use a top-level `text`; older shapes use content.
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| content.and_then(collect_text))
                .unwrap_or_default();
            if !text.is_empty() {
                entries.push(simple_entry(
                    format!("user:{id}"),
                    TranscriptKind::UserMessage,
                    ts,
                    text,
                    &model,
                    &session,
                ));
            }
            if let Some(content) = content {
                push_tool_parts(content, ts, &model, &session, entries);
            }
        }
        "assistant" => {
            if let Some(content) = content {
                if let Some(text) = collect_text(content) {
                    entries.push(simple_entry(
                        format!("assistant:{id}"),
                        TranscriptKind::AssistantMessage,
                        ts,
                        text,
                        &model,
                        &session,
                    ));
                }
                push_tool_parts(content, ts, &model, &session, entries);
            }
            let tokens_value = message.get("tokens").or_else(|| message.get("usage"));
            if tokens_value.is_some() || message.get("cost").is_some() {
                let tokens = tokens_value.map(tokens_from_value).unwrap_or_default();
                let usd = message
                    .get("cost")
                    .and_then(Value::as_f64)
                    .or_else(|| tokens_value.and_then(|value| value.get("cost")).and_then(Value::as_f64));
                entries.push(cost_entry(format!("cost:{id}"), ts, &model, &session, tokens, usd));
            }
        }
        _ => {}
    }
}

/// Maps a live event list, coalescing all `session.text.ended` parts by assistant message.
///
/// `flush_end` is true for one-shot callers: pending text is emitted even when the slice
/// does not reach an execution end. The live host passes false, so a mid-turn window never
/// writes a partial assistant message.
pub fn entries_from_api_events(events: &[Value]) -> Vec<TranscriptEntry> {
    map_events(events, true)
}

/// Maps one live `/api/event` payload to canonical transcript entries.
pub fn entries_from_api_event(event: &Value) -> Vec<TranscriptEntry> {
    map_events(std::slice::from_ref(event), true)
}

/// One assistant message whose text parts are still being collected.
#[derive(Debug)]
struct PendingAssistant {
    ts: Timestamp,
    parts: Vec<(u64, String)>,
    model: Option<String>,
    session: Option<String>,
}

/// Maps live events in order, coalescing assistant text and computing usage deltas.
fn map_events(events: &[Value], flush_end: bool) -> Vec<TranscriptEntry> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut out = Vec::new();
    let mut pending: BTreeMap<String, PendingAssistant> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut completed: BTreeSet<String> = BTreeSet::new();
    let mut last_assistant: Option<String> = None;
    let mut last_model: Option<String> = None;
    let mut step_models: BTreeMap<String, String> = BTreeMap::new();
    let mut previous_tokens = vowp_protocol::Tokens::default();
    let mut previous_usd = 0.0;
    let mut usage_seen = false;

    for event in events {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        let data = event.get("data").unwrap_or(&Value::Null);
        let ts = event_timestamp(event);
        let marker = event_marker(event, data);
        let session = session_of(event);
        let model = data
            .get("model")
            .and_then(model_name)
            .or_else(|| data.get("modelID").and_then(Value::as_str).map(str::to_owned));
        match kind {
            "session.inbox.enqueued" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                let text = item
                    .pointer("/payload/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let id = data
                    .get("inboxID")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .map(str::to_owned)
                    .unwrap_or_else(|| marker.clone());
                out.push(simple_entry(
                    format!("user:{id}"),
                    TranscriptKind::UserMessage,
                    ts,
                    text.to_owned(),
                    &model,
                    &session,
                ));
            }
            "session.execution.started" => {
                last_assistant = None;
                last_model = None;
                out.push(TranscriptEntry {
                    native_id: format!("turn-start:{marker}"),
                    kind: TranscriptKind::TurnStarted,
                    ts,
                    text: None,
                    tool: None,
                    input: None,
                    output: None,
                    path: None,
                    call_id: None,
                    model,
                    native_session: session,
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "session.text.ended" => {
                let Some(id) = data.get("assistantMessageID").and_then(Value::as_str) else {
                    continue;
                };
                let text = data.get("text").and_then(Value::as_str).unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
                let entry_model = model
                    .clone()
                    .or_else(|| step_models.get(id).cloned())
                    .or_else(|| last_model.clone());
                let pending_message = pending.entry(id.to_owned()).or_insert_with(|| {
                    order.push(id.to_owned());
                    PendingAssistant {
                        ts,
                        parts: Vec::new(),
                        model: entry_model.clone(),
                        session: session.clone(),
                    }
                });
                pending_message.parts.push((ordinal, text.to_owned()));
                if pending_message.model.is_none() {
                    pending_message.model.clone_from(&entry_model);
                }
                if pending_message.model.is_some() {
                    last_model.clone_from(&pending_message.model);
                }
                if pending_message.session.is_none() {
                    pending_message.session.clone_from(&session);
                }
                last_assistant = Some(id.to_owned());
            }
            "session.step.started" => {
                let Some(id) = data.get("assistantMessageID").and_then(Value::as_str) else {
                    continue;
                };
                last_assistant = Some(id.to_owned());
                if let Some(step_model) = &model {
                    step_models.insert(id.to_owned(), step_model.clone());
                    last_model = Some(step_model.clone());
                }
                let entry_model = model
                    .clone()
                    .or_else(|| step_models.get(id).cloned())
                    .or_else(|| last_model.clone());
                let pending_message = pending.entry(id.to_owned()).or_insert_with(|| {
                    order.push(id.to_owned());
                    PendingAssistant {
                        ts,
                        parts: Vec::new(),
                        model: entry_model.clone(),
                        session: session.clone(),
                    }
                });
                if pending_message.model.is_none() {
                    pending_message.model.clone_from(&entry_model);
                }
                if pending_message.model.is_some() {
                    last_model.clone_from(&pending_message.model);
                }
                if pending_message.session.is_none() {
                    pending_message.session.clone_from(&session);
                }
            }
            "session.step.ended" => {
                let Some(id) = data.get("assistantMessageID").and_then(Value::as_str) else {
                    continue;
                };
                last_assistant = Some(id.to_owned());
                if let Some(step_model) = step_models.get(id) {
                    last_model = Some(step_model.clone());
                }
                completed.insert(id.to_owned());
                flush_ready(&mut out, &mut pending, &mut order, &mut completed, false);
            }
            "session.tool.called" => {
                let call_id = data.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                out.push(TranscriptEntry {
                    native_id: format!("tool-call:{call_id}"),
                    kind: TranscriptKind::ToolCall,
                    ts,
                    text: None,
                    tool: data
                        .get("name")
                        .or_else(|| data.get("tool"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    input: Some(data.get("input").cloned().unwrap_or(Value::Null)),
                    output: None,
                    path: None,
                    call_id: Some(call_id),
                    model: model.clone().or_else(|| last_model.clone()),
                    native_session: session,
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "session.tool.success" | "session.tool.failed" => {
                let call_id = data.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let ok = kind == "session.tool.success";
                let output = if ok {
                    text_value(data.get("content"))
                } else {
                    data.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                out.push(TranscriptEntry {
                    native_id: format!("tool-result:{call_id}"),
                    kind: TranscriptKind::ToolResult,
                    ts,
                    text: None,
                    tool: None,
                    input: None,
                    output,
                    path: None,
                    call_id: Some(call_id),
                    model: model.clone().or_else(|| last_model.clone()),
                    native_session: session,
                    ok: Some(ok),
                    tokens: None,
                    usd: None,
                });
            }
            "session.usage.updated" => {
                let usage = data.get("tokens").or_else(|| data.get("usage")).unwrap_or(data);
                let tokens = tokens_from_value(usage);
                let usd = data
                    .get("cost")
                    .and_then(Value::as_f64)
                    .or_else(|| usage.get("cost").and_then(Value::as_f64));
                // `session.usage.updated` is a cumulative session snapshot, so write
                // the delta for each update and key it by the assistant message it follows.
                let delta = if usage_seen {
                    tokens_delta(&tokens, &previous_tokens)
                } else {
                    tokens
                };
                let delta_usd = if usage_seen {
                    usd.map(|usd| (usd - previous_usd).max(0.0))
                } else {
                    usd
                };
                let native_id = last_assistant
                    .clone()
                    .map(|id| format!("cost:{id}"))
                    .unwrap_or_else(|| format!("cost:usage:{marker}"));
                let entry_model = model.clone().or_else(|| last_model.clone());
                out.push(cost_entry(native_id, ts, &entry_model, &session, delta, delta_usd));
                previous_tokens = tokens;
                if let Some(usd) = usd {
                    previous_usd = usd;
                }
                usage_seen = true;
            }
            "session.execution.succeeded" | "session.execution.interrupted" | "session.execution.failed" => {
                flush_ready(&mut out, &mut pending, &mut order, &mut completed, true);
                if kind.ends_with("failed") {
                    let message = data
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("session execution failed")
                        .to_owned();
                    let entry_model = model.clone().or_else(|| last_model.clone());
                    out.push(error_entry(
                        format!("error:{marker}"),
                        ts,
                        message,
                        &entry_model,
                        &session,
                    ));
                }
                let outcome = if kind.ends_with("interrupted") {
                    "interrupted"
                } else if kind.ends_with("failed") {
                    "failed"
                } else {
                    "succeeded"
                };
                let entry_model = model.clone().or_else(|| last_model.clone());
                out.push(turn_end_entry(
                    format!("turn-end:{marker}"),
                    ts,
                    outcome,
                    &entry_model,
                    &session,
                ));
            }
            "session.step.failed" => {
                let message = data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("step failed")
                    .to_owned();
                let entry_model = model.clone().or_else(|| last_model.clone());
                out.push(error_entry(
                    format!("error:{marker}"),
                    ts,
                    message,
                    &entry_model,
                    &session,
                ));
            }
            "session.model.selected" => {
                let selected = data
                    .get("model")
                    .and_then(model_name)
                    .or_else(|| data.get("modelID").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned());
                last_model = Some(selected.clone());
                out.push(simple_entry(
                    format!("model:{marker}"),
                    TranscriptKind::Note,
                    ts,
                    format!("app selected model {selected}"),
                    &model,
                    &session,
                ));
            }
            _ => {}
        }
    }
    if flush_end {
        flush_ready(&mut out, &mut pending, &mut order, &mut completed, true);
    }
    out
}

/// Emits the pending assistant messages whose ids are complete (or all of them).
fn flush_ready(
    out: &mut Vec<TranscriptEntry>,
    pending: &mut std::collections::BTreeMap<String, PendingAssistant>,
    order: &mut Vec<String>,
    completed: &mut std::collections::BTreeSet<String>,
    all: bool,
) {
    let ready: Vec<String> = order
        .iter()
        .filter(|id| all || completed.contains(*id))
        .cloned()
        .collect();
    for id in ready {
        if let Some(message) = pending.remove(&id) {
            if let Some(entry) = pending_entry(&id, message) {
                out.push(entry);
            }
        }
        order.retain(|candidate| candidate != &id);
        completed.remove(&id);
    }
}

/// Builds the single coalesced assistant message for one app-side message id.
fn pending_entry(id: &str, mut message: PendingAssistant) -> Option<TranscriptEntry> {
    message.parts.sort_by_key(|(ordinal, _)| *ordinal);
    let text = message
        .parts
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    Some(simple_entry(
        format!("assistant:{id}"),
        TranscriptKind::AssistantMessage,
        message.ts,
        text,
        &message.model,
        &message.session,
    ))
}

/// Adds tool calls and tool results from a message content array.
fn push_tool_parts(
    content: &Value,
    ts: Timestamp,
    model: &Option<String>,
    session: &Option<String>,
    entries: &mut Vec<TranscriptEntry>,
) {
    let Some(parts) = content.as_array() else {
        return;
    };
    for (index, part) in parts.iter().enumerate() {
        let part_id = part
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("part-{index}"));
        match part.get("type").and_then(Value::as_str).unwrap_or_default() {
            "tool_use" | "tool" => {
                let call_id = part
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("callID").and_then(Value::as_str))
                    .unwrap_or(&part_id)
                    .to_owned();
                let input = part
                    .get("input")
                    .or_else(|| part.get("state").and_then(|state| state.get("input")))
                    .cloned()
                    .unwrap_or(Value::Null);
                entries.push(TranscriptEntry {
                    native_id: format!("tool-call:{call_id}"),
                    kind: TranscriptKind::ToolCall,
                    ts,
                    text: None,
                    tool: part
                        .get("name")
                        .or_else(|| part.get("tool"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    path: None,
                    input: Some(input),
                    output: None,
                    call_id: Some(call_id),
                    model: model.clone(),
                    native_session: session.clone(),
                    ok: None,
                    tokens: None,
                    usd: None,
                });
            }
            "tool_result" => {
                let call_id = part
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("callID").and_then(Value::as_str))
                    .unwrap_or(&part_id)
                    .to_owned();
                entries.push(TranscriptEntry {
                    native_id: format!("tool-result:{call_id}"),
                    kind: TranscriptKind::ToolResult,
                    ts,
                    text: None,
                    tool: None,
                    input: None,
                    output: text_value(part.get("content").or_else(|| part.get("output"))),
                    path: None,
                    call_id: Some(call_id),
                    model: model.clone(),
                    native_session: session.clone(),
                    ok: Some(!part.get("is_error").and_then(Value::as_bool).unwrap_or(false)),
                    tokens: None,
                    usd: None,
                });
            }
            _ => {}
        }
    }
}

/// Joins the text blocks of a content value into one message.
fn collect_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return if text.is_empty() { None } else { Some(text.to_owned()) };
    }
    let parts: Vec<String> = content
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() { None } else { Some(parts.join("\n")) }
}

/// Builds a turn-end entry.
fn turn_end_entry(
    native_id: String,
    ts: Timestamp,
    outcome: &str,
    model: &Option<String>,
    session: &Option<String>,
) -> TranscriptEntry {
    TranscriptEntry {
        native_id,
        kind: TranscriptKind::TurnEnded,
        ts,
        text: Some(outcome.to_owned()),
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: model.clone(),
        native_session: session.clone(),
        ok: None,
        tokens: None,
        usd: None,
    }
}

/// Builds an error entry.
fn error_entry(
    native_id: String,
    ts: Timestamp,
    message: String,
    model: &Option<String>,
    session: &Option<String>,
) -> TranscriptEntry {
    TranscriptEntry {
        native_id,
        kind: TranscriptKind::Error,
        ts,
        text: Some(message),
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: model.clone(),
        native_session: session.clone(),
        ok: None,
        tokens: None,
        usd: None,
    }
}

/// Builds a cost entry from explicit tokens and USD.
fn cost_entry(
    native_id: String,
    ts: Timestamp,
    model: &Option<String>,
    session: &Option<String>,
    tokens: vowp_protocol::Tokens,
    usd: Option<f64>,
) -> TranscriptEntry {
    TranscriptEntry {
        native_id,
        kind: TranscriptKind::Cost,
        ts,
        text: None,
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: model.clone(),
        native_session: session.clone(),
        ok: None,
        tokens: Some(tokens),
        usd,
    }
}

/// Builds a simple text entry.
fn simple_entry(
    native_id: String,
    kind: TranscriptKind,
    ts: Timestamp,
    text: String,
    model: &Option<String>,
    session: &Option<String>,
) -> TranscriptEntry {
    TranscriptEntry {
        native_id,
        kind,
        ts,
        text: Some(text),
        tool: None,
        input: None,
        output: None,
        path: None,
        call_id: None,
        model: model.clone(),
        native_session: session.clone(),
        ok: None,
        tokens: None,
        usd: None,
    }
}

/// Reads tokens from a usage object, including the nested `cache` shape.
fn tokens_from_value(value: &Value) -> vowp_protocol::Tokens {
    vowp_protocol::Tokens {
        input: u64_field(value, &["input", "input_tokens", "inputTokens"]),
        output: u64_field(value, &["output", "output_tokens", "outputTokens"]),
        reasoning: u64_field(value, &["reasoning", "reasoning_tokens", "reasoningTokens"]),
        cache_read: value
            .pointer("/cache/read")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| u64_field(value, &["cache_read", "cacheReadInputTokens"])),
        cache_write: value
            .pointer("/cache/write")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| u64_field(value, &["cache_write", "cacheCreationInputTokens"])),
    }
}

/// Subtracts one cumulative token snapshot from the next.
fn tokens_delta(current: &vowp_protocol::Tokens, previous: &vowp_protocol::Tokens) -> vowp_protocol::Tokens {
    vowp_protocol::Tokens {
        input: current.input.saturating_sub(previous.input),
        output: current.output.saturating_sub(previous.output),
        reasoning: current.reasoning.saturating_sub(previous.reasoning),
        cache_read: current.cache_read.saturating_sub(previous.cache_read),
        cache_write: current.cache_write.saturating_sub(previous.cache_write),
    }
}

/// Reads the first present unsigned field from a usage object.
fn u64_field(value: &Value, names: &[&str]) -> u64 {
    for name in names {
        if let Some(number) = value.get(*name).and_then(Value::as_u64) {
            return number;
        }
    }
    0
}

/// Returns a stable marker for one event: the durable sequence when present, else the id.
fn event_marker(event: &Value, data: &Value) -> String {
    event
        .pointer("/durable/seq")
        .and_then(Value::as_u64)
        .or_else(|| data.pointer("/durable/seq").and_then(Value::as_u64))
        .map(|seq| format!("seq:{seq}"))
        .or_else(|| event.get("id").and_then(Value::as_str).map(|id| format!("id:{id}")))
        .or_else(|| data.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| event_timestamp(event).to_string())
}

/// Returns an event's session id.
fn session_of(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    data.get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/data/sessionID").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Reads an event timestamp from `created` or `data.time.created`.
fn event_timestamp(event: &Value) -> Timestamp {
    event
        .get("created")
        .and_then(Value::as_i64)
        .or_else(|| event.pointer("/data/time/created").and_then(Value::as_i64))
        .map(Timestamp::from_unix_millis)
        .unwrap_or_else(Timestamp::now)
}

/// Reads a message timestamp.
fn message_timestamp(message: &Value) -> Timestamp {
    message
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .or_else(|| message.get("created").and_then(Value::as_i64))
        .or_else(|| message.pointer("/info/time/created").and_then(Value::as_i64))
        .map(Timestamp::from_unix_millis)
        .unwrap_or_else(Timestamp::now)
}

/// Reads a message timestamp as Unix milliseconds for stable sorting.
fn message_created(message: &Value) -> i64 {
    message
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .or_else(|| message.get("created").and_then(Value::as_i64))
        .or_else(|| message.pointer("/info/time/created").and_then(Value::as_i64))
        .unwrap_or(i64::MAX)
}

/// Reads the app-side session id from a message.
fn message_session(message: &Value) -> Option<String> {
    message
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| {
            message
                .get("info")
                .and_then(|info| info.get("sessionID"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
}

/// Reads a message model name.
fn message_model(message: &Value) -> Option<String> {
    message
        .get("model")
        .and_then(model_name)
        .or_else(|| {
            message
                .get("info")
                .and_then(|info| info.get("model"))
                .and_then(model_name)
        })
        .or_else(|| message.get("modelID").and_then(Value::as_str).map(str::to_owned))
}

/// Reads a model value that is either a string or an object with an id.
fn model_name(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("modelID").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| value.get("id").and_then(Value::as_str).map(str::to_owned))
}

/// Joins a string-or-block-array value into text.
fn text_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| block.as_str())
                        .map(str::to_owned)
                })
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n")) }
        }
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn sse_decoder_handles_chunked_split_frames() {
        let first = "data: {\"type\":\"one\"}\n\n";
        let second = "data: {\"type\":\"two\"}\n\n";
        let mut body = Vec::new();
        for chunk in [first, second] {
            body.extend_from_slice(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes());
        }
        body.extend_from_slice(b"0\r\n\r\n");
        let mut decoder = SseDecoder {
            chunked: true,
            ..SseDecoder::default()
        };
        let mut events = Vec::new();
        for byte in body {
            decoder.feed(&[byte], &mut |value| events.push(value));
        }
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"].as_str(), Some("one"));
        assert_eq!(events[1]["type"].as_str(), Some("two"));
    }

    #[test]
    fn sse_decoder_handles_plain_split_frames() {
        let payload = b"data: {\"type\":\"one\"}\n\ndata: {\"type\":\"two\"}\n\n";
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in payload.chunks(3) {
            decoder.feed(chunk, &mut |value| events.push(value));
        }
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"].as_str(), Some("one"));
        assert_eq!(events[1]["type"].as_str(), Some("two"));
    }
}
