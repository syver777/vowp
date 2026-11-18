//! The Mac-side v0.4 agent client.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4, §5.3, §8 and §12.2 and
//! `docs/design/crates/vowp-agent.md`. The crate owns local config and the node key, speaks the
//! `vowp-protocol` agent frames over an injected transport, follows the local opencode event
//! stream, and never depends on a store or on `vowp-core`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bridge;
pub mod cache;
pub mod chat;
pub mod client;
pub mod config;
pub mod error;
pub mod executor;
pub mod isolation;
pub mod key;
pub mod orchestrator;
pub mod service;
pub mod socket;
pub mod spool;
pub mod status;
pub mod surface_state;
pub mod transport;
pub mod worktree;
pub mod wss;

pub use bridge::{
    BRIDGE_BLOBS_DIR, BRIDGE_DIR, BRIDGE_EVENTS_FILE, BRIDGE_SENT_FILE, BRIDGE_SPOOL_DIR, BridgeAttachment,
    BridgeFrame, BridgeNotice, BridgeState, BridgeSync, DEFAULT_MAX_ATTACHMENT_BYTES, DRAIN_TIMEOUT, MIRROR_FILE,
    MirrorCursor, OpencodeBridge, PathRoot, Redaction, SSE_PATH, SSE_READ_TIMEOUT,
};
pub use cache::{
    CACHE_DIR, CACHE_MAX_BYTES, CACHE_MAX_ENTRIES, CACHE_TTL, Cache, CacheStats, CachedRead, READ_TOOLS,
    is_cacheable_tool,
};
pub use chat::{
    CHAT_EPOCH, ChatClient, ChatTurnRequest, ChatTurnStatus, EVENT_POLL, LEASE_HEARTBEAT_EVERY, MAX_RECONNECTS,
};
pub use client::{
    AGENT_VERSION, AgentClient, AgentClock, Backoff, Enrollment, HEARTBEAT_SECS, MAX_BACKOFF, PAIR_TIMEOUT,
    PairedSurface, RunOptions, RunOutcome, STABLE_CONNECTION, SurfacePairRequest, SystemAgentClock,
    validate_enrollment_code, validate_pairing_code, validate_surface_id,
};
pub use config::{
    AGENT_DIR_NAME, AgentConfig, CONFIG_FILE, DEFAULT_WORKTREE_DIR, PairedToken, STATUS_FILE, SURFACE_EXPIRY_FILE,
    SURFACE_SESSION_FILE, SURFACE_TOKEN_FILE, SURFACES_DIR, TransportConfig, WssTlsMode, paired_token_for_session,
};
pub use error::AgentError;
pub use executor::{
    AcceptedRun, EVENT_WINDOW, HandlerReject, PushStatus, QUARANTINE_FILE, RunHandler, RunSink, RunSinkError,
    RunSummary,
};
pub use isolation::{
    IsolationError, PRIVATE_FILE_MODE, STATE_DIR_MODE, check_private_file, check_state_dir, current_uid,
    ensure_log_dir, ensure_not_root, ensure_state_dir,
};
pub use key::{NODE_KEY_FILE, NODE_PUB_FILE, NodeKey, fingerprint_of, handshake_payload, load_public};
pub use orchestrator::{
    AppCommand, LiveHost, ORCHESTRATOR_CONFIG_FILE, ORCHESTRATOR_HOST_FILE, ORCHESTRATOR_POLL,
    ORCHESTRATOR_START_TIMEOUT, ORCHESTRATOR_STOP_GRACE, ORCHESTRATOR_XDG_DIR, ORCHESTRATORS_DIR, OrchestratorConfig,
    OrchestratorHost, OrchestratorHostState, PrepareOutcome, PreparedHost, SurfaceAttachment, SurfaceBinding, app_name,
    redact_detail, validate_name,
};
pub use service::{
    CommandRecord, InstallOptions, InstallReport, LAUNCHD_LABEL, LAUNCHD_PLIST, LOG_DIR, LOG_FILE, RUN_MARKER,
    SERVICE_MARKER, SYSTEMD_UNIT, ServiceError, ServiceManager, ServicePlatform, ServiceRunOutcome, ServiceSpec,
    ServiceStatus, SystemManager, UninstallReport, check_run_environment, install, prepare_service_run,
    remove_run_marker, run_marker_path, status, uninstall, write_run_marker,
};
pub use socket::{
    AGENT_SOCKET_FILE, AgentSocket, MAX_SOCKET_REQUEST_BYTES, SocketRequest, SocketResponse, TOOL_CALL_TIMEOUT,
};
pub use spool::{ACKED_FILE, DEFAULT_SPOOL_CAP_BYTES, EVENTS_FILE, SPOOL_DIR, Spool, SpoolAppend, spool_dir};
pub use status::{AgentStatus, ConnectionState, pending_spool_bytes, surface_token_expiry};
pub use surface_state::{
    SURFACE_PREFIX_CHARS, SURFACE_STATE_FILE, SurfaceRefusal, SurfaceState, SurfaceStatus, bound_session,
    mark_needs_pair, mark_paired, mark_revoked, recovery_commands, state_of, surface_statuses, token_prefix,
};
pub use transport::{FrameRead, FrameStream, MAX_FRAME_BYTES, SshTransport, Transport, transport_for_config};
pub use worktree::{
    DEFAULT_DISK_RESERVE_BYTES, GitOutput, GitRunner, JOB_BRANCH_PREFIX, PathMap, PushResult, STDERR_TAIL_BYTES,
    SystemGit, Worktree, WorktreeError, WorktreeManager, WorktreeSpec, strip_remote_credentials,
};
pub use wss::{
    DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_MAX_CONNECTIONS, WSS_PATH, WSS_SUBPROTOCOL, WssConnection, WssFrameStream,
    WssIngress, WssLineReader, WssLineWriter, WssListener, WssTls, WssTransport, WssUrl, certificate_fingerprint,
    is_fingerprint,
};
