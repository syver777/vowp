//! The Mac-side opencode bridge: the local app's SSE stream to normalized surface frames.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.3, §5.6, §5.7 and §8.3, and
//! `docs/design/crates/vowp-agent.md`. The bridge reads the mode-0600 local
//! `service.json` of an opencode app, follows `/api/event`, and maps completed events to
//! [`SurfaceEvent`]/[`SurfaceTurn`] frames. Mapping, path normalization, secret redaction
//! and attachment hashing all run on this machine; the home only receives native ids and
//! normalized payloads.
//!
//! The bridge owns its own private spool below `state_dir/bridge/<surface>/`. A frame is
//! written and fsynced there before it is handed to the client for sending. `mirror.json`
//! records the last applied raw event and native id so a reconnect or backfill can resume
//! from the cursor without appending a duplicate.
//!
//! The HTTP/SSE client is deliberately small and blocking. A read uses a short timeout so
//! the agent's frame loop can keep sending heartbeats and answering local tool calls.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vowp_protocol::agent::surface::{SurfaceEvent, SurfaceTurn, SurfaceTurnState};
use vowp_protocol::{
    AgentFrame, AgentPathMap, BlobRef, Cost, CostEvent, CostSource, ErrorEvent, ErrorKind, EventBody, JobId, Note,
    PathRef, ProjectName, SecretName, SessionId, SurfaceId, Timestamp, Tokens, ToolCall as EventToolCall,
    ToolResult as EventToolResult, UserMessage,
};

use crate::client::validate_surface_id;
use crate::config::{ensure_private_dir, write_private};
use crate::error::AgentError;

/// Directory below the agent state directory that holds every bridge.
pub const BRIDGE_DIR: &str = "bridge";
/// Cursor file inside one surface's bridge directory.
pub const MIRROR_FILE: &str = "mirror.json";
/// Append-only mirrored-frame spool inside one surface's bridge directory.
pub const BRIDGE_EVENTS_FILE: &str = "events.jsonl";
/// Sent-native-id index inside one surface's bridge spool.
pub const BRIDGE_SENT_FILE: &str = "sent.json";
/// Spool directory inside one surface's bridge directory.
pub const BRIDGE_SPOOL_DIR: &str = "spool";
/// Content-addressed attachment bytes inside one surface's bridge directory.
pub const BRIDGE_BLOBS_DIR: &str = "blobs";
/// Default attachment size cap: 8 MiB.
pub const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;
/// Path of the opencode event stream.
pub const SSE_PATH: &str = "/api/event";
/// How long one SSE read may wait before the frame loop gets a turn.
pub const SSE_READ_TIMEOUT: Duration = Duration::from_millis(50);
/// Loopback connect budget for the local app; the frame loop must not stall on it.
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
/// Response-header budget for the local app.
const SSE_HEADER_TIMEOUT: Duration = Duration::from_millis(250);
/// Longest one [`OpencodeBridge::drain`] call waits for a finite recorded stream.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest accepted SSE header block.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// How many recent raw events are remembered for duplicate suppression.
const RECENT_LIMIT: usize = 512;

/// Cursor persisted in `mirror.json`.
///
/// `last_seq` is the app's durable sequence when it has one. `last_event_id` is the raw
/// event marker, which is stable even when the app does not expose a durable sequence.
/// `last_native_id` is the last mirrored frame's native id and is the value a human sees
/// when inspecting the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorCursor {
    /// Last raw event marker applied by the bridge.
    #[serde(default)]
    pub last_event_id: Option<String>,
    /// Last app durable sequence applied, when the stream carries one.
    #[serde(default)]
    pub last_seq: Option<u64>,
    /// Last mirrored native id.
    #[serde(default)]
    pub last_native_id: Option<String>,
}

/// One project or worktree root and the path ref it maps to.
///
/// Roots are matched longest first, so a worktree below a project is not mistaken for a
/// project subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRoot {
    root: PathBuf,
    path_ref: PathRef,
}

impl PathRoot {
    /// Builds a root with an explicit path ref.
    pub fn new(root: impl Into<PathBuf>, path_ref: PathRef) -> Result<Self, AgentError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(AgentError::Config("a bridge path root must be absolute".to_owned()));
        }
        if root
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(AgentError::Config(
                "a bridge path root must not contain parent traversal".to_owned(),
            ));
        }
        Ok(Self { root, path_ref })
    }

    /// Builds a project root ref.
    pub fn project(root: impl Into<PathBuf>, project: &ProjectName) -> Result<Self, AgentError> {
        Self::new(
            root,
            PathRef::Project {
                project: project.clone(),
            },
        )
    }

    /// Builds a worktree root ref.
    pub fn worktree(root: impl Into<PathBuf>, project: &ProjectName, job: &JobId) -> Result<Self, AgentError> {
        Self::new(
            root,
            PathRef::Worktree {
                project: project.clone(),
                job: job.clone(),
            },
        )
    }

    /// Returns the local absolute root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the wire path ref.
    pub fn path_ref(&self) -> &PathRef {
        &self.path_ref
    }
}

/// One exact secret value the bridge must redact before spooling or sending.
///
/// The value is never printed by `Debug`; the name is the marker suffix.
#[derive(Clone, PartialEq, Eq)]
pub struct Redaction {
    name: SecretName,
    value: String,
}

impl Redaction {
    /// Builds a redaction from a secret name and its injected value.
    pub fn new(name: &str, value: impl Into<String>) -> Result<Self, AgentError> {
        let name = SecretName::parse(name)
            .map_err(|_| AgentError::Config("a redaction name must be an uppercase secret name".to_owned()))?;
        let value = value.into();
        if value.is_empty() {
            return Err(AgentError::Config("a redaction value must not be empty".to_owned()));
        }
        Ok(Self { name, value })
    }

    /// Returns the secret name.
    pub fn name(&self) -> &SecretName {
        &self.name
    }

    /// Returns the marker written in place of the value.
    pub fn marker(&self) -> String {
        format!("[REDACTED:{}]", self.name)
    }
}

impl fmt::Debug for Redaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redaction")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// One attachment read from the local disk, content-addressed but never path-addressed.
#[derive(Clone, PartialEq, Eq)]
pub struct BridgeAttachment {
    sha256: String,
    bytes: u64,
    preview: String,
    data: Vec<u8>,
}

impl BridgeAttachment {
    /// Returns the lowercase hex SHA-256.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Returns the number of bytes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns the first 512 display characters, already redacted.
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Returns the exact attachment bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Returns the wire blob reference carried by a `user.message`.
    pub fn blob_ref(&self) -> BlobRef {
        BlobRef {
            sha256: self.sha256.clone(),
            bytes: self.bytes,
            preview: self.preview.clone(),
        }
    }
}

impl fmt::Debug for BridgeAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeAttachment")
            .field("sha256", &self.sha256)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// One normalized frame the bridge produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeFrame {
    /// One normalized session event.
    Event {
        /// The event to send as `SurfaceEvent`.
        event: Box<SurfaceEvent>,
    },
    /// One turn boundary.
    Turn {
        /// The boundary to send as `SurfaceTurn`.
        turn: SurfaceTurn,
    },
}

impl BridgeFrame {
    /// Returns the app-side native id used for duplicate suppression.
    pub fn native_id(&self) -> &str {
        match self {
            Self::Event { event } => &event.native_id,
            Self::Turn { turn } => &turn.native_id,
        }
    }

    /// Returns the surface that observed the frame.
    pub fn surface(&self) -> &SurfaceId {
        match self {
            Self::Event { event } => &event.surface,
            Self::Turn { turn } => &turn.surface,
        }
    }

    /// Returns the session the frame belongs to.
    pub fn session(&self) -> &SessionId {
        match self {
            Self::Event { event } => &event.session,
            Self::Turn { turn } => &turn.session,
        }
    }

    /// Returns the outbound agent frame.
    pub fn as_agent_frame(&self) -> AgentFrame {
        match self {
            Self::Event { event } => AgentFrame::SurfaceEvent(event.as_ref().clone()),
            Self::Turn { turn } => AgentFrame::SurfaceTurn(turn.clone()),
        }
    }
}

/// A reported hole in the app's event sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeNotice {
    /// The stream resumed after a durable-sequence gap.
    Gap {
        /// Last applied sequence.
        after: u64,
        /// First sequence seen after the gap.
        next: u64,
    },
}

/// Frames and notices produced by one poll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BridgeSync {
    /// New frames, already fsynced to the bridge spool.
    pub frames: Vec<BridgeFrame>,
    /// Conditions worth surfacing to the operator; never silently dropped.
    pub notices: Vec<BridgeNotice>,
}

/// Observable bridge state for status and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeState {
    /// Whether the SSE stream is open.
    pub attached: bool,
    /// Why the bridge stopped writing, when a lease was lost or a surface was revoked.
    pub read_only: Option<String>,
    /// Frames spooled but not yet handed to the connection.
    pub pending: usize,
    /// Last reported sequence gap.
    pub last_gap: Option<(u64, u64)>,
}

/// The opencode bridge for one paired surface.
pub struct OpencodeBridge {
    surface: SurfaceId,
    session: SessionId,
    native_session: Option<String>,
    endpoint: Endpoint,
    bridge_dir: PathBuf,
    mirror_path: PathBuf,
    spool: BridgeSpool,
    roots: Vec<PathRoot>,
    redactions: Vec<Redaction>,
    max_attachment_bytes: u64,
    cursor: MirrorCursor,
    stream: Option<SseStream>,
    attached: bool,
    read_only: Option<String>,
    pending: Vec<PendingAssistant>,
    last_assistant: Option<String>,
    last_model: Option<String>,
    usage: UsageState,
    raw_seen: VecDeque<String>,
    recent_native: VecDeque<String>,
    last_gap: Option<(u64, u64)>,
}

struct PendingAssistant {
    id: String,
    parts: Vec<(u64, String)>,
    model: Option<String>,
}

impl Clone for PendingAssistant {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            parts: self.parts.clone(),
            model: self.model.clone(),
        }
    }
}

#[derive(Default)]
struct UsageState {
    seen: bool,
    tokens: Tokens,
    usd: f64,
}

impl OpencodeBridge {
    /// Loads the mode-0600 opencode `service.json` and opens the surface's private state.
    ///
    /// The path and port are private configuration: no error or `Debug` output includes
    /// the port, password or pair link. A missing, invalid or wider-than-0600 file is a
    /// typed [`AgentError::Config`].
    pub fn load(
        service_json: &Path,
        state_dir: &Path,
        surface: SurfaceId,
        session: SessionId,
    ) -> Result<Self, AgentError> {
        validate_surface_id(&surface)?;
        let endpoint = Endpoint::load(service_json)?;
        let bridge_dir = state_dir.join(BRIDGE_DIR).join(&surface.0);
        ensure_private_dir(&bridge_dir)?;
        let mirror_path = bridge_dir.join(MIRROR_FILE);
        let mirror = read_mirror(&mirror_path, &surface, &session)?;
        let cursor = mirror.as_ref().map(|file| file.cursor.clone()).unwrap_or_default();
        let mut recent_native: VecDeque<String> = mirror
            .as_ref()
            .map(|file| file.recent_native.iter().cloned().collect())
            .unwrap_or_default();
        if recent_native.is_empty()
            && let Some(last) = &cursor.last_native_id
        {
            recent_native.push_back(last.clone());
        }
        if mirror.is_some() {
            // The file is private by contract; tighten a file written by an older build.
            let _ = fs::set_permissions(&mirror_path, fs::Permissions::from_mode(0o600));
        }
        let spool = BridgeSpool::open(&bridge_dir.join(BRIDGE_SPOOL_DIR))?;
        Ok(Self {
            surface,
            session,
            native_session: None,
            endpoint,
            bridge_dir,
            mirror_path,
            spool,
            roots: Vec::new(),
            redactions: Vec::new(),
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            cursor,
            stream: None,
            attached: false,
            read_only: None,
            pending: Vec::new(),
            last_assistant: None,
            last_model: None,
            usage: UsageState::default(),
            raw_seen: VecDeque::new(),
            recent_native,
            last_gap: None,
        })
    }

    /// Restricts the bridge to one app-side session id.
    pub fn with_native_session(mut self, native_session: impl Into<String>) -> Self {
        self.native_session = Some(native_session.into());
        self
    }

    /// Replaces the root map used before spooling.
    pub fn with_path_roots(mut self, roots: Vec<PathRoot>) -> Self {
        self.roots = roots;
        self.sort_roots();
        self
    }

    /// Adds project roots from the agent's validated project maps.
    pub fn with_path_maps(mut self, maps: &[AgentPathMap]) -> Result<Self, AgentError> {
        for map in maps {
            let project = map.project.clone();
            self.roots
                .push(PathRoot::project(Path::new(&map.local_root), &project)?);
        }
        self.sort_roots();
        Ok(self)
    }

    /// Adds one path root.
    pub fn with_path_root(mut self, root: PathRoot) -> Self {
        self.roots.push(root);
        self.sort_roots();
        self
    }

    /// Replaces the exact values redacted before spooling.
    pub fn with_redactions(mut self, redactions: Vec<Redaction>) -> Self {
        self.redactions = redactions;
        self
    }

    /// Replaces the attachment size cap.
    pub fn with_attachment_cap(mut self, cap: u64) -> Self {
        self.max_attachment_bytes = cap;
        self
    }

    /// Returns the surface id.
    pub fn surface(&self) -> &SurfaceId {
        &self.surface
    }

    /// Returns the session id.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Returns the mode-0600 cursor path.
    pub fn mirror_path(&self) -> &Path {
        &self.mirror_path
    }

    /// Returns the bridge state directory.
    pub fn bridge_dir(&self) -> &Path {
        &self.bridge_dir
    }

    /// Returns the spool directory.
    pub fn spool_dir(&self) -> &Path {
        self.spool.dir()
    }

    /// Returns the current cursor.
    pub fn cursor(&self) -> &MirrorCursor {
        &self.cursor
    }

    /// Returns whether any redaction values are still in memory.
    pub fn redactions_loaded(&self) -> bool {
        !self.redactions.is_empty()
    }

    /// Returns the observable state.
    pub fn state(&self) -> BridgeState {
        BridgeState {
            attached: self.attached,
            read_only: self.read_only.clone(),
            pending: self.spool.pending_count().unwrap_or(0),
            last_gap: self.last_gap,
        }
    }

    /// Opens the event stream from the persisted cursor and marks the bridge attached.
    ///
    /// A caller that lost the lease must not call this; [`Self::note_lease_lost`] closes
    /// the stream and keeps the spool for inspection.
    pub fn attach(&mut self) -> Result<(), AgentError> {
        if self.read_only.is_some() {
            return Ok(());
        }
        if self.stream.is_none() {
            self.stream = Some(SseStream::open(&self.endpoint, &self.cursor)?);
        }
        self.attached = true;
        Ok(())
    }

    /// Closes the event stream without discarding the cursor or spool.
    pub fn close_stream(&mut self) {
        self.stream = None;
        self.attached = false;
    }

    /// Stops the bridge read-only after a lease loss or a revoked surface.
    ///
    /// The stream is closed and the spool is kept; no further frame is read, mapped or
    /// sent by this bridge.
    pub fn note_lease_lost(&mut self, reason: impl Into<String>) {
        self.read_only = Some(reason.into());
        self.close_stream();
    }

    /// Reads one short SSE window, maps complete events and returns the new frames.
    ///
    /// Every returned frame is already fsynced to the bridge spool. A broken local stream
    /// is a transport error; the next call reconnects from the cursor.
    pub fn poll(&mut self) -> Result<BridgeSync, AgentError> {
        let mut sync = BridgeSync::default();
        if self.read_only.is_some() {
            return Ok(sync);
        }
        if self.stream.is_none() {
            self.attach()?;
        }
        let read = match self.stream.as_mut() {
            Some(stream) => stream.read_available(),
            None => {
                return Ok(sync);
            }
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                self.stream = None;
                return Err(error);
            }
        };
        for event in read.events {
            self.apply_event(event, &mut sync)?;
        }
        if read.closed {
            self.stream = None;
            let mut flushed = Vec::new();
            self.flush_all(&mut flushed)?;
            for frame in flushed {
                self.spool.append(&frame)?;
                sync.frames.push(frame);
            }
            // Persist the newly emitted native ids even though the raw cursor did not
            // move: the spool already holds the frames and a restart must skip them.
            let _ = self.write_cursor();
        }
        Ok(sync)
    }

    /// Reads a finite recorded stream until it closes or is idle.
    ///
    /// This is the backfill/short-stream form of [`Self::poll`]; a live agent uses
    /// `poll` from its frame loop so the heartbeat never waits for a stream.
    pub fn drain(&mut self) -> Result<BridgeSync, AgentError> {
        let mut sync = BridgeSync::default();
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        loop {
            let piece = self.poll()?;
            let empty = piece.frames.is_empty() && piece.notices.is_empty();
            sync.frames.extend(piece.frames);
            sync.notices.extend(piece.notices);
            if self.stream.is_none() || empty || Instant::now() >= deadline {
                break;
            }
        }
        Ok(sync)
    }

    /// Returns the spooled frames the client has not sent yet, in original order.
    pub fn pending_frames(&self) -> Result<Vec<BridgeFrame>, AgentError> {
        self.spool.pending()
    }

    /// Records that the client wrote one frame to the one outbound connection.
    pub fn mark_sent(&mut self, native_id: &str) -> Result<(), AgentError> {
        self.spool.mark_sent(native_id)
    }

    /// Returns the content-addressed path of one stored attachment.
    pub fn blob_path(&self, sha256: &str) -> PathBuf {
        self.bridge_dir.join(BRIDGE_BLOBS_DIR).join(sha256)
    }

    /// Reads one local file into an attachment, enforcing the size cap.
    ///
    /// The path is used only for the read; it is never stored, spooled or sent. The
    /// returned bytes are written 0600 under `blobs/<sha256>` before the caller queues a
    /// `user.message` that names only the blob ref.
    pub fn attachment(&mut self, source: &Path) -> Result<BridgeAttachment, AgentError> {
        let metadata = fs::metadata(source)?;
        if !metadata.is_file() {
            return Err(AgentError::Config(
                "an opencode attachment must be a regular file".to_owned(),
            ));
        }
        if metadata.len() > self.max_attachment_bytes {
            return Err(AgentError::Config(format!(
                "an attachment is {} bytes; the cap is {}",
                metadata.len(),
                self.max_attachment_bytes
            )));
        }
        let data = fs::read(source)?;
        let bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if bytes > self.max_attachment_bytes {
            return Err(AgentError::Config(format!(
                "an attachment is {bytes} bytes; the cap is {}",
                self.max_attachment_bytes
            )));
        }
        let sha256 = sha256_hex(&data);
        let preview = self.sanitize_text(&preview_of(&data));
        let blob_dir = self.bridge_dir.join(BRIDGE_BLOBS_DIR);
        ensure_private_dir(&blob_dir)?;
        let blob_path = blob_dir.join(&sha256);
        if !blob_path.exists() {
            write_private(&blob_path, &data)?;
        }
        Ok(BridgeAttachment {
            sha256,
            bytes,
            preview,
            data,
        })
    }

    /// Queues one user message carrying the given local attachments.
    ///
    /// The message is spooled with only `BlobRef { sha256, bytes, preview }` values; the
    /// attachment bytes stay content-addressed in `blobs/<sha256>` and no local path ever
    /// reaches the spool or the wire.
    pub fn queue_attachment(&mut self, text: &str, sources: &[PathBuf]) -> Result<BridgeFrame, AgentError> {
        let mut refs = Vec::with_capacity(sources.len());
        let mut identity = String::new();
        let mut total = 0u64;
        for source in sources {
            let attachment = self.attachment(source)?;
            identity.push_str(&attachment.sha256);
            identity.push('\n');
            total = total.saturating_add(attachment.bytes);
            refs.push(attachment.blob_ref());
        }
        let native_id = format!(
            "attachment:{}",
            sha256_hex(format!("{text}\n{identity}\n{total}").as_bytes())
        );
        let event = SurfaceEvent {
            surface: self.surface.clone(),
            session: self.session.clone(),
            native_id: native_id.clone(),
            event: EventBody::UserMessage(UserMessage {
                text: self.sanitize_text(text),
                attachments: refs,
                note: None,
            }),
        };
        let frame = BridgeFrame::Event { event: Box::new(event) };
        if self.recent_native.iter().any(|seen| seen == &native_id) {
            return Ok(frame);
        }
        self.spool.append(&frame)?;
        self.push_recent_native(&native_id);
        Ok(frame)
    }

    /// Clears the injected secret values and closes the stream.
    ///
    /// The spool and cursor stay on disk for inspection and replay.
    pub fn finish(&mut self) {
        self.close_stream();
        self.redactions.clear();
        self.pending.clear();
        self.last_assistant = None;
        self.last_model = None;
        self.usage = UsageState::default();
    }

    fn sort_roots(&mut self) {
        self.roots
            .sort_by_key(|root| std::cmp::Reverse(root.root.as_os_str().len()));
    }

    fn apply_event(&mut self, event: Value, sync: &mut BridgeSync) -> Result<(), AgentError> {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return Ok(());
        };
        let data = event.get("data").unwrap_or(&Value::Null);
        if let (Some(expected), Some(actual)) = (&self.native_session, session_of(&event)) {
            if expected != actual {
                return Ok(());
            }
        }
        let raw_key = raw_event_key(&event, data);
        let seq = durable_seq(&event, data);
        if self.raw_seen.iter().any(|seen| seen == &raw_key) {
            return Ok(());
        }
        if let (Some(last), Some(seq)) = (self.cursor.last_seq, seq) {
            if seq <= last {
                self.remember_raw(raw_key);
                return Ok(());
            }
            if seq > last.saturating_add(1) {
                let notice = BridgeNotice::Gap { after: last, next: seq };
                self.last_gap = Some((last, seq));
                sync.notices.push(notice);
            }
        }
        let ts = event_timestamp(&event);
        let marker = event_marker(&event, data);
        let mut emitted: Vec<BridgeFrame> = Vec::new();
        match kind {
            "session.execution.started" => {
                self.flush_all(&mut emitted)?;
                self.last_model = model_of(data);
                let native_id = format!("turn-start:{marker}");
                if !self.recent_native.iter().any(|seen| seen == &native_id) {
                    emitted.push(self.turn_frame(native_id, SurfaceTurnState::Started, ts));
                }
            }
            "session.inbox.enqueued" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) != Some("user") {
                    self.skip_event(raw_key, seq);
                    return Ok(());
                }
                let text = item
                    .pointer("/payload/text")
                    .or_else(|| item.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    self.skip_event(raw_key, seq);
                    return Ok(());
                }
                let id = data
                    .get("inboxID")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .map(str::to_owned)
                    .unwrap_or_else(|| marker.clone());
                let native_id = format!("user:{id}");
                if self.recent_native.iter().any(|seen| seen == &native_id) {
                    self.skip_event(raw_key, seq);
                    return Ok(());
                }
                let event = SurfaceEvent {
                    surface: self.surface.clone(),
                    session: self.session.clone(),
                    native_id: native_id.clone(),
                    event: EventBody::UserMessage(UserMessage {
                        text: self.sanitize_text(text),
                        attachments: Vec::new(),
                        note: None,
                    }),
                };
                emitted.push(BridgeFrame::Event { event: Box::new(event) });
            }
            "session.text.ended" => {
                let Some(id) = data.get("assistantMessageID").and_then(Value::as_str) else {
                    self.skip_event(raw_key, seq);
                    return Ok(());
                };
                let text = data.get("text").and_then(Value::as_str).unwrap_or_default();
                if text.is_empty() {
                    self.skip_event(raw_key, seq);
                    return Ok(());
                }
                let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
                let model = model_of(data).or_else(|| self.last_model.clone());
                if self.pending.iter().any(|pending| pending.id != id) {
                    let old: Vec<PendingAssistant> = self
                        .pending
                        .iter()
                        .filter(|pending| pending.id != id)
                        .cloned()
                        .collect();
                    for pending in old {
                        self.flush_one(&pending.id, &mut emitted)?;
                    }
                }
                if let Some(pending) = self.pending.iter_mut().find(|pending| pending.id == id) {
                    pending.parts.push((ordinal, text.to_owned()));
                    if pending.model.is_none() {
                        pending.model = model;
                    }
                } else {
                    self.pending.push(PendingAssistant {
                        id: id.to_owned(),
                        parts: vec![(ordinal, text.to_owned())],
                        model,
                    });
                }
            }
            "session.step.ended" => {
                if let Some(id) = data.get("assistantMessageID").and_then(Value::as_str) {
                    self.flush_one(id, &mut emitted)?;
                }
            }
            "session.tool.called" => {
                let call_id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| marker.clone());
                let native_id = format!("tool-call:{call_id}");
                if !self.recent_native.iter().any(|seen| seen == &native_id) {
                    let name = data
                        .get("name")
                        .or_else(|| data.get("tool"))
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input = self.sanitize_json(data.get("input").cloned().unwrap_or(Value::Null));
                    let event = SurfaceEvent {
                        surface: self.surface.clone(),
                        session: self.session.clone(),
                        native_id,
                        event: EventBody::ToolCall(EventToolCall { call_id, name, input }),
                    };
                    emitted.push(BridgeFrame::Event { event: Box::new(event) });
                }
            }
            "session.tool.success" | "session.tool.failed" => {
                let call_id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| marker.clone());
                let native_id = format!("tool-result:{call_id}");
                if !self.recent_native.iter().any(|seen| seen == &native_id) {
                    let ok = kind == "session.tool.success";
                    let output = if ok {
                        text_value(data.get("content"))
                    } else {
                        data.pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    let event = SurfaceEvent {
                        surface: self.surface.clone(),
                        session: self.session.clone(),
                        native_id,
                        event: EventBody::ToolResult(EventToolResult {
                            call_id,
                            ok,
                            output: output.map(|text| self.sanitize_text(&text)),
                            output_ref: None,
                            exit_code: None,
                        }),
                    };
                    emitted.push(BridgeFrame::Event { event: Box::new(event) });
                }
            }
            "session.usage.updated" => {
                let native_id = self
                    .last_assistant
                    .clone()
                    .map(|id| format!("cost:{id}"))
                    .unwrap_or_else(|| format!("cost:usage:{marker}"));
                if !self.recent_native.iter().any(|seen| seen == &native_id) {
                    let usage = data.get("tokens").or_else(|| data.get("usage")).unwrap_or(data);
                    let tokens = tokens_from_value(usage);
                    let usd = data
                        .get("cost")
                        .and_then(Value::as_f64)
                        .or_else(|| usage.get("cost").and_then(Value::as_f64));
                    let (delta, delta_usd) = if self.usage.seen {
                        (
                            tokens_delta(&tokens, &self.usage.tokens),
                            usd.map(|usd| (usd - self.usage.usd).max(0.0)),
                        )
                    } else {
                        (tokens, usd)
                    };
                    let cost = match delta_usd {
                        Some(usd) => Cost::receipt(usd, delta),
                        None => Cost {
                            usd: None,
                            tokens: delta,
                            source: CostSource::Unknown,
                        },
                    };
                    let event = SurfaceEvent {
                        surface: self.surface.clone(),
                        session: self.session.clone(),
                        native_id,
                        event: EventBody::Cost(CostEvent { cost, run_id: None }),
                    };
                    emitted.push(BridgeFrame::Event { event: Box::new(event) });
                    self.usage.tokens = tokens;
                    if let Some(usd) = usd {
                        self.usage.usd = usd;
                    }
                    self.usage.seen = true;
                }
            }
            "session.execution.succeeded" | "session.execution.interrupted" | "session.execution.failed" => {
                self.flush_all(&mut emitted)?;
                if kind == "session.execution.failed" {
                    let message = data
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("session execution failed")
                        .to_owned();
                    let native_id = format!("error:{marker}");
                    if !self.recent_native.iter().any(|seen| seen == &native_id) {
                        let event = SurfaceEvent {
                            surface: self.surface.clone(),
                            session: self.session.clone(),
                            native_id,
                            event: EventBody::Error(ErrorEvent {
                                kind: ErrorKind::AdapterFailed,
                                message: self.sanitize_text(&message),
                                retryable: false,
                            }),
                        };
                        emitted.push(BridgeFrame::Event { event: Box::new(event) });
                    }
                }
                let native_id = format!("turn-end:{marker}");
                if !self.recent_native.iter().any(|seen| seen == &native_id) {
                    emitted.push(self.turn_frame(native_id, SurfaceTurnState::Ended, ts));
                }
                self.last_assistant = None;
                self.last_model = None;
                self.usage = UsageState::default();
            }
            "session.model.selected" => {
                let selected = data
                    .get("model")
                    .and_then(model_name)
                    .or_else(|| data.get("modelID").and_then(Value::as_str).map(str::to_owned));
                if let Some(selected) = selected {
                    self.last_model = Some(selected.clone());
                    let native_id = format!("model:{marker}");
                    if !self.recent_native.iter().any(|seen| seen == &native_id) {
                        let event = SurfaceEvent {
                            surface: self.surface.clone(),
                            session: self.session.clone(),
                            native_id,
                            event: EventBody::Note(Note {
                                text: self.sanitize_text(&format!("app selected model {selected}")),
                            }),
                        };
                        emitted.push(BridgeFrame::Event { event: Box::new(event) });
                    }
                }
            }
            // `session.text.started`, `session.text.delta`, `session.step.started`,
            // `session.step.failed`, `session.tool.input.*`, `server.connected`,
            // `project.updated`, `mcp.*` and unknown app events are UI-only.
            _ => {
                self.remember_raw(raw_key.clone());
                if self.pending.is_empty() {
                    self.advance_cursor(raw_key, seq, None);
                }
                return Ok(());
            }
        }

        for frame in &emitted {
            self.spool.append(frame)?;
            let native_id = frame.native_id();
            if !native_id.starts_with("assistant:") {
                self.push_recent_native(native_id);
            }
        }
        self.remember_raw(raw_key.clone());
        if self.pending.is_empty() {
            self.advance_cursor(raw_key, seq, emitted.last().map(BridgeFrame::native_id));
        }
        sync.frames.extend(emitted);
        Ok(())
    }

    fn turn_frame(&self, native_id: String, state: SurfaceTurnState, at: Timestamp) -> BridgeFrame {
        BridgeFrame::Turn {
            turn: SurfaceTurn {
                surface: self.surface.clone(),
                session: self.session.clone(),
                native_id,
                state,
                at,
            },
        }
    }

    fn flush_one(&mut self, id: &str, out: &mut Vec<BridgeFrame>) -> Result<(), AgentError> {
        let Some(position) = self.pending.iter().position(|pending| pending.id == id) else {
            return Ok(());
        };
        let mut pending = self.pending.remove(position);
        pending.parts.sort_by_key(|(ordinal, _)| *ordinal);
        let text = pending
            .parts
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return Ok(());
        }
        let native_id = format!("assistant:{}", pending.id);
        if self.recent_native.iter().any(|seen| seen == &native_id) {
            return Ok(());
        }
        let model = pending.model.clone().or_else(|| self.last_model.clone());
        if model.is_some() {
            self.last_model = model;
        }
        let event = SurfaceEvent {
            surface: self.surface.clone(),
            session: self.session.clone(),
            native_id: native_id.clone(),
            event: EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
                text: self.sanitize_text(&text),
            }),
        };
        self.last_assistant = Some(pending.id.clone());
        self.push_recent_native(&native_id);
        out.push(BridgeFrame::Event { event: Box::new(event) });
        Ok(())
    }

    fn flush_all(&mut self, out: &mut Vec<BridgeFrame>) -> Result<(), AgentError> {
        let ids: Vec<String> = self.pending.iter().map(|pending| pending.id.clone()).collect();
        for id in ids {
            self.flush_one(&id, out)?;
        }
        Ok(())
    }

    fn remember_raw(&mut self, key: String) {
        if self.raw_seen.iter().any(|seen| seen == &key) {
            return;
        }
        self.raw_seen.push_back(key);
        while self.raw_seen.len() > RECENT_LIMIT {
            self.raw_seen.pop_front();
        }
    }

    fn skip_event(&mut self, raw_key: String, seq: Option<u64>) {
        self.remember_raw(raw_key.clone());
        if self.pending.is_empty() {
            self.advance_cursor(raw_key, seq, None);
        }
    }

    fn push_recent_native(&mut self, native_id: &str) {
        if self.recent_native.iter().any(|seen| seen == native_id) {
            return;
        }
        self.recent_native.push_back(native_id.to_owned());
        while self.recent_native.len() > RECENT_LIMIT {
            self.recent_native.pop_front();
        }
    }

    fn advance_cursor(&mut self, raw_key: String, seq: Option<u64>, native_id: Option<&str>) {
        self.cursor.last_event_id = Some(raw_key);
        if let Some(seq) = seq {
            self.cursor.last_seq = Some(seq);
        }
        if let Some(native_id) = native_id {
            self.cursor.last_native_id = Some(native_id.to_owned());
        }
        if let Err(error) = self.write_cursor() {
            // The cursor is a backfill aid, not the authority: the home deduplicates by
            // native id. A write failure must not drop an already-spooled event.
            let _ = error;
        }
    }

    fn write_cursor(&self) -> Result<(), AgentError> {
        let file = MirrorFile {
            surface: self.surface.clone(),
            session: self.session.clone(),
            cursor: self.cursor.clone(),
            updated_at: Timestamp::now(),
            recent_native: self.recent_native.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_private(&self.mirror_path, &bytes)
    }

    fn sanitize_text(&self, text: &str) -> String {
        let mut value = text.to_owned();
        for redaction in &self.redactions {
            if value.contains(&redaction.value) {
                value = value.replace(&redaction.value, &redaction.marker());
            }
        }
        let value = self.normalize_paths(&value);
        let mut value = value;
        for redaction in &self.redactions {
            if value.contains(&redaction.value) {
                value = value.replace(&redaction.value, &redaction.marker());
            }
        }
        value
    }

    fn sanitize_json(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.sanitize_text(&text)),
            Value::Array(values) => Value::Array(values.into_iter().map(|value| self.sanitize_json(value)).collect()),
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, self.sanitize_json(value)))
                    .collect(),
            ),
            other => other,
        }
    }

    fn normalize_paths(&self, text: &str) -> String {
        let mut output = String::with_capacity(text.len());
        let mut index = 0;
        while index < text.len() {
            let rest = &text[index..];
            let Some(character) = rest.chars().next() else {
                break;
            };
            if character == '/'
                && path_starts_at(text, index)
                && let Some((end, token)) = scan_path(text, index)
            {
                output.push_str(&self.map_abs_path(token));
                index = end;
                continue;
            }
            output.push(character);
            index += character.len_utf8();
        }
        replace_unmapped_homes(output)
    }

    fn map_abs_path(&self, path: &str) -> String {
        for root in &self.roots {
            let Some(root_text) = root.root.to_str() else {
                continue;
            };
            if path == root_text {
                return root.path_ref.to_string();
            }
            if let Some(rest) = path.strip_prefix(root_text).and_then(|rest| rest.strip_prefix('/')) {
                if rest.is_empty() {
                    return root.path_ref.to_string();
                }
                // The longest matching root wins. A path below a worktree root has no
                // narrower PathRef today, so it is external rather than a project subdir.
                if let PathRef::Project { project } = &root.path_ref {
                    let candidate = format!("subdir:{project}/{rest}");
                    if PathRef::parse(&candidate).is_ok() {
                        return candidate;
                    }
                }
                return "<external>".to_owned();
            }
        }
        "<external>".to_owned()
    }
}

impl fmt::Debug for OpencodeBridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpencodeBridge")
            .field("surface", &self.surface)
            .field("session", &self.session)
            .field("attached", &self.attached)
            .field("read_only", &self.read_only)
            .field("cursor", &self.cursor)
            .field("roots", &self.roots.len())
            .field("redactions", &self.redactions.len())
            .field("pending", &self.spool.pending_count().unwrap_or(0))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MirrorFile {
    surface: SurfaceId,
    session: SessionId,
    cursor: MirrorCursor,
    updated_at: Timestamp,
    #[serde(default)]
    recent_native: Vec<String>,
}

fn read_mirror(path: &Path, surface: &SurfaceId, session: &SessionId) -> Result<Option<MirrorFile>, AgentError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AgentError::Io(error)),
    };
    let metadata = fs::metadata(path)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(AgentError::Config(
            "opencode bridge mirror.json must be mode 0600".to_owned(),
        ));
    }
    let file: MirrorFile = serde_json::from_slice(&bytes)
        .map_err(|_| AgentError::Config("opencode bridge mirror.json is invalid".to_owned()))?;
    if &file.surface != surface || &file.session != session {
        return Err(AgentError::Config(
            "opencode bridge mirror.json belongs to another surface or session".to_owned(),
        ));
    }
    Ok(Some(file))
}

/// The mode-0600 spool that holds mirrored frames before and after sending.
pub(crate) struct BridgeSpool {
    dir: PathBuf,
    events: File,
    sent: BTreeSet<String>,
    pending: Vec<BridgeFrame>,
}

impl BridgeSpool {
    pub(crate) fn open(dir: &Path) -> Result<Self, AgentError> {
        ensure_private_dir(dir)?;
        let path = dir.join(BRIDGE_EVENTS_FILE);
        let events = OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let sent = read_sent(&dir.join(BRIDGE_SENT_FILE))?;
        let pending = read_records(&path)?
            .into_iter()
            .filter(|frame| !sent.contains(frame.native_id()))
            .collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            events,
            sent,
            pending,
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn append(&mut self, frame: &BridgeFrame) -> Result<(), AgentError> {
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        self.events.write_all(&line)?;
        self.events.sync_all()?;
        self.pending.push(frame.clone());
        Ok(())
    }

    pub(crate) fn pending(&self) -> Result<Vec<BridgeFrame>, AgentError> {
        Ok(self.pending.clone())
    }

    pub(crate) fn pending_count(&self) -> Result<usize, AgentError> {
        Ok(self.pending.len())
    }

    pub(crate) fn mark_sent(&mut self, native_id: &str) -> Result<(), AgentError> {
        if self.sent.insert(native_id.to_owned()) {
            write_sent(&self.dir.join(BRIDGE_SENT_FILE), &self.sent)?;
        }
        self.pending.retain(|frame| frame.native_id() != native_id);
        Ok(())
    }
}

fn read_records(path: &Path) -> Result<Vec<BridgeFrame>, AgentError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(AgentError::Io(error)),
    };
    let mut frames = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let frame: BridgeFrame = serde_json::from_str(line)
            .map_err(|_| AgentError::Config(format!("opencode bridge spool line {} is invalid", number + 1)))?;
        frames.push(frame);
    }
    Ok(frames)
}

#[derive(Serialize, Deserialize)]
struct SentFile {
    #[serde(default)]
    sent: Vec<String>,
}

fn read_sent(path: &Path) -> Result<BTreeSet<String>, AgentError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(AgentError::Io(error)),
    };
    let file: SentFile = serde_json::from_slice(&bytes)
        .map_err(|_| AgentError::Config("opencode bridge sent.json is invalid".to_owned()))?;
    Ok(file.sent.into_iter().collect())
}

fn write_sent(path: &Path, sent: &BTreeSet<String>) -> Result<(), AgentError> {
    let file = SentFile {
        sent: sent.iter().cloned().collect(),
    };
    write_private(path, &serde_json::to_vec_pretty(&file)?)
}

/// One loopback opencode endpoint. Port and password stay private to this module.
#[derive(Clone)]
struct Endpoint {
    host: IpAddr,
    port: u16,
    auth: String,
}

impl Endpoint {
    fn load(service_json: &Path) -> Result<Self, AgentError> {
        let metadata = fs::metadata(service_json).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AgentError::Config("opencode service.json is missing".to_owned())
            } else {
                AgentError::Io(error)
            }
        })?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(AgentError::Config("opencode service.json must be mode 0600".to_owned()));
        }
        let bytes = fs::read(service_json)?;
        let config: ServiceConfig = serde_json::from_slice(&bytes)
            .map_err(|_| AgentError::Config("opencode service.json is invalid".to_owned()))?;
        if config.password.is_empty() {
            return Err(AgentError::Config(
                "opencode service.json has no service password".to_owned(),
            ));
        }
        let (host, port) = config.address()?;
        Ok(Self {
            host,
            port,
            auth: format!(
                "Basic {}",
                base64_encode(format!("opencode:{}", config.password).as_bytes())
            ),
        })
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("host", &"[loopback]")
            .field("port", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
struct ServiceConfig {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    password: String,
}

impl ServiceConfig {
    fn address(&self) -> Result<(IpAddr, u16), AgentError> {
        let Some(url) = &self.url else {
            let Some(port) = self.port.filter(|port| *port != 0) else {
                return Err(AgentError::Config(
                    "opencode service.json has no usable port".to_owned(),
                ));
            };
            return Ok((IpAddr::V4(Ipv4Addr::LOCALHOST), port));
        };
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| AgentError::Config("opencode service.json must use a loopback http URL".to_owned()))?;
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| AgentError::Config("opencode service.json URL has no port".to_owned()))?;
        if !matches!(host, "127.0.0.1" | "localhost") {
            return Err(AgentError::Config(
                "opencode service.json must use a loopback http URL".to_owned(),
            ));
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| AgentError::Config("opencode service.json URL has no usable port".to_owned()))?;
        Ok((IpAddr::V4(Ipv4Addr::LOCALHOST), port))
    }
}

struct SseStream {
    stream: TcpStream,
    decoder: SseDecoder,
    pending_body: Vec<u8>,
}

struct SseRead {
    events: Vec<Value>,
    closed: bool,
}

impl SseStream {
    fn open(endpoint: &Endpoint, cursor: &MirrorCursor) -> Result<Self, AgentError> {
        let address = SocketAddr::new(endpoint.host, endpoint.port);
        let mut stream = TcpStream::connect_timeout(&address, SSE_CONNECT_TIMEOUT)
            .map_err(|_| AgentError::Transport("opencode event stream is not reachable".to_owned()))?;
        stream
            .set_read_timeout(Some(SSE_HEADER_TIMEOUT))
            .map_err(AgentError::Io)?;
        stream.set_write_timeout(Some(SSE_HEADER_TIMEOUT))?;
        let mut path = SSE_PATH.to_owned();
        if let Some(seq) = cursor.last_seq {
            path.push_str(&format!("?since={seq}"));
        }
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n",
            endpoint.auth
        );
        if let Some(last) = cursor.last_event_id.as_deref().or(cursor.last_native_id.as_deref()) {
            if !last.bytes().any(|byte| byte.is_ascii_control()) {
                request.push_str(&format!("Last-Event-ID: {last}\r\n"));
            }
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|_| AgentError::Transport("opencode event stream is not reachable".to_owned()))?;

        let mut header = Vec::new();
        let mut chunk = [0u8; 8192];
        let body_start = loop {
            if header.len() > MAX_HEADER_BYTES {
                return Err(AgentError::Transport(
                    "opencode event stream returned an invalid response".to_owned(),
                ));
            }
            if let Some(position) = header.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(AgentError::Transport(
                        "opencode event stream returned an invalid response".to_owned(),
                    ));
                }
                Ok(read) => header.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    return Err(AgentError::Transport(
                        "opencode event stream returned an invalid response".to_owned(),
                    ));
                }
            }
        };
        let head = String::from_utf8_lossy(&header[..body_start]);
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok());
        match status {
            Some(200) => {}
            Some(401 | 403) => {
                return Err(AgentError::Config(
                    "opencode rejected the local service login".to_owned(),
                ));
            }
            _ => {
                return Err(AgentError::Transport(
                    "opencode event stream returned an invalid response".to_owned(),
                ));
            }
        }
        let chunked = head.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked")
            })
        });
        let decoder = SseDecoder {
            chunked,
            ..SseDecoder::default()
        };
        stream
            .set_read_timeout(Some(SSE_READ_TIMEOUT))
            .map_err(AgentError::Io)?;
        Ok(Self {
            stream,
            decoder,
            pending_body: header[body_start..].to_vec(),
        })
    }

    fn read_available(&mut self) -> Result<SseRead, AgentError> {
        let mut events = Vec::new();
        if !self.pending_body.is_empty() {
            let pending = std::mem::take(&mut self.pending_body);
            self.decoder.feed(&pending, &mut |value| events.push(value));
        }
        let deadline = Instant::now() + SSE_READ_TIMEOUT;
        let mut closed = false;
        let mut chunk = [0u8; 8192];
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(read) => {
                    self.decoder.feed(&chunk[..read], &mut |value| events.push(value));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(_) => {
                    return Err(AgentError::Transport(
                        "opencode event stream was interrupted".to_owned(),
                    ));
                }
            }
        }
        Ok(SseRead { events, closed })
    }
}

/// Incremental SSE decoder for `data:` lines, including chunked transfer encoding.
#[derive(Default)]
struct SseDecoder {
    chunked: bool,
    raw: Vec<u8>,
    line: Vec<u8>,
    remaining: usize,
    crlf: usize,
}

impl SseDecoder {
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

    fn drain_lines(&mut self, on_event: &mut impl FnMut(Value)) {
        while let Some(position) = self.line.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.line.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line);
            let line = text.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(data.trim())
                && value.is_object()
            {
                on_event(value);
            }
        }
    }
}

fn session_of(event: &Value) -> Option<&str> {
    let data = event.get("data")?;
    data.get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/data/sessionID").and_then(Value::as_str))
}

fn durable_seq(event: &Value, data: &Value) -> Option<u64> {
    event
        .pointer("/durable/seq")
        .and_then(Value::as_u64)
        .or_else(|| data.pointer("/durable/seq").and_then(Value::as_u64))
}

fn event_marker(event: &Value, data: &Value) -> String {
    durable_seq(event, data)
        .map(|seq| format!("seq:{seq}"))
        .or_else(|| event.get("id").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| data.get("id").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| event_timestamp(event).to_string())
}

fn raw_event_key(event: &Value, data: &Value) -> String {
    durable_seq(event, data)
        .map(|seq| format!("seq:{seq}"))
        .or_else(|| event.get("id").and_then(Value::as_str).map(|id| format!("id:{id}")))
        .unwrap_or_else(|| {
            // Two non-durable events of different types can share a millisecond; the type
            // keeps their fallback keys distinct so neither is dropped as a duplicate.
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("unknown");
            format!("ts:{kind}:{}", event_timestamp(event))
        })
}

fn event_timestamp(event: &Value) -> Timestamp {
    event
        .get("created")
        .and_then(Value::as_i64)
        .or_else(|| event.pointer("/data/time/created").and_then(Value::as_i64))
        .map(Timestamp::from_unix_millis)
        .unwrap_or_else(Timestamp::now)
}

fn model_of(data: &Value) -> Option<String> {
    data.get("model")
        .and_then(model_name)
        .or_else(|| data.get("modelID").and_then(Value::as_str).map(str::to_owned))
}

fn model_name(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("modelID").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| value.get("id").and_then(Value::as_str).map(str::to_owned))
}

fn tokens_from_value(value: &Value) -> Tokens {
    Tokens {
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

fn tokens_delta(current: &Tokens, previous: &Tokens) -> Tokens {
    Tokens {
        input: current.input.saturating_sub(previous.input),
        output: current.output.saturating_sub(previous.output),
        reasoning: current.reasoning.saturating_sub(previous.reasoning),
        cache_read: current.cache_read.saturating_sub(previous.cache_read),
        cache_write: current.cache_write.saturating_sub(previous.cache_write),
    }
}

fn u64_field(value: &Value, names: &[&str]) -> u64 {
    for name in names {
        if let Some(number) = value.get(*name).and_then(Value::as_u64) {
            return number;
        }
    }
    0
}

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

/// Returns true when the `/` at `index` starts a path rather than a URL or division.
fn path_starts_at(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    if bytes.get(index + 1).is_none_or(|next| *next == b'/') {
        return false;
    }
    if index == 0 {
        return true;
    }
    let Some(previous) = text[..index].chars().next_back() else {
        return true;
    };
    !previous.is_ascii_alphanumeric() && !matches!(previous, '_' | '-' | '.' | '/' | '~' | '%')
}

/// Replaces an absolute user-home path missed by the normal boundary scanner.
///
/// A URL can place a `/Users/<name>` or `/home/<name>` path after characters the scanner
/// treats as part of a host name. The privacy rule is absolute: no such path leaves the
/// bridge. This fallback runs after every mapping and only touches the two conventional home
/// roots.
fn replace_unmapped_homes(mut text: String) -> String {
    for prefix in ["/Users/", "/home/"] {
        while let Some(position) = text.find(prefix) {
            let (end, _) = scan_path(&text, position).unwrap_or((position + prefix.len(), ""));
            text.replace_range(position..end, "<external>");
        }
    }
    text
}

/// Scans one absolute path token and returns its end and text.
fn scan_path(text: &str, start: usize) -> Option<(usize, &str)> {
    let mut end = start;
    for (offset, character) in text[start..].char_indices() {
        if offset == 0 {
            end = start + character.len_utf8();
            continue;
        }
        if character.is_whitespace()
            || matches!(
                character,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
            )
        {
            break;
        }
        end = start + offset + character.len_utf8();
    }
    while end > start + 1 {
        let Some(last) = text[start..end].chars().next_back() else {
            break;
        };
        if matches!(last, '.' | ':' | '!' | '?') {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    if end <= start + 1 {
        return None;
    }
    Some((end, &text[start..end]))
}

fn preview_of(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(&data[..data.len().min(2048)]);
    text.chars().take(512).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
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
