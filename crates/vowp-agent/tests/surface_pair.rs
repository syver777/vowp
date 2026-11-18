//! Surface pairing tests for the Mac-side agent client.
//!
//! The fake home speaks the public [`Transport`]/[`FrameStream`] boundary in memory: no SSH,
//! network, store or real `~/.vowp-agent` is touched.

use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, BridgeFrame, FrameRead, FrameStream, NodeKey, RunOptions,
    RunOutcome, SurfacePairRequest, Transport, pending_spool_bytes,
};
use vowp_protocol::agent::surface::SurfaceEvent;
use vowp_protocol::{
    AGENT_PROTOCOL, AgentFrame, AgentRole, ErrorKind, EventBody, JobId, NodeName, OrchestratorCommand, PathRef,
    ProjectName, RunId, SessionId, SurfaceId, SurfaceScope, Timestamp, UserMessage,
};

const CODE: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const SURFACE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Builds a fake token at runtime so no secret-shaped literal is committed.
fn token() -> String {
    format!("vowp_surf_{}", "A".repeat(43))
}

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-14T10:00:00Z").expect("timestamp")
}

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

#[derive(Default)]
struct FakeState {
    connections: usize,
    used_codes: HashSet<String>,
    received: Vec<AgentFrame>,
    live_frames_sent: bool,
}

#[derive(Clone)]
struct FakeHome {
    shared: Arc<Mutex<FakeState>>,
    clock: FixedClock,
    surface_id: SurfaceId,
    token: String,
    scopes: Vec<SurfaceScope>,
    expires_at: Timestamp,
    live_frames: Vec<AgentFrame>,
    pair_frames: Vec<AgentFrame>,
}

impl FakeHome {
    fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(FakeState::default())),
            clock: FixedClock(ts()),
            surface_id: SurfaceId(SURFACE.to_owned()),
            token: token(),
            scopes: vec![
                SurfaceScope::new(format!("session.read:{SESSION}")),
                SurfaceScope::new(format!("session.write:{SESSION}")),
                SurfaceScope::new("fleet.read"),
            ],
            expires_at: ts().plus(Duration::from_secs(30 * 24 * 60 * 60)),
            live_frames: Vec::new(),
            pair_frames: Vec::new(),
        }
    }

    /// Queues frames the home pushes before the surface-pairing answer.
    fn with_pair_frames(mut self, frames: Vec<AgentFrame>) -> Self {
        self.pair_frames = frames;
        self
    }

    /// Queues frames the home pushes right after the next authenticated heartbeat.
    fn with_live_frames(mut self, frames: Vec<AgentFrame>) -> Self {
        self.live_frames = frames;
        self
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .clone()
    }

    fn connections(&self) -> usize {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connections
    }

    fn surface_pairs(&self) -> Vec<(String, SurfaceId, SessionId, String)> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                AgentFrame::SurfacePair {
                    code,
                    surface_id,
                    session,
                    app,
                } => Some((code, surface_id, session, app)),
                _ => None,
            })
            .collect()
    }
}

impl Transport for FakeHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let connection = {
            let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connections += 1;
            state.connections
        };
        Ok(Box::new(FakeStream {
            shared: Arc::clone(&self.shared),
            clock: self.clock,
            surface_id: self.surface_id.clone(),
            token: self.token.clone(),
            scopes: self.scopes.clone(),
            expires_at: self.expires_at,
            live_frames: self.live_frames.clone(),
            pair_frames: self.pair_frames.clone(),
            connection,
            outbound: VecDeque::new(),
        }))
    }
}

struct FakeStream {
    shared: Arc<Mutex<FakeState>>,
    clock: FixedClock,
    surface_id: SurfaceId,
    token: String,
    scopes: Vec<SurfaceScope>,
    expires_at: Timestamp,
    live_frames: Vec<AgentFrame>,
    pair_frames: Vec<AgentFrame>,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
}

impl FrameStream for FakeStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Closed),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .received
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { .. } => {
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: format!("nonce-{}", self.connection),
                    home_id: "home-1".to_owned(),
                    connection_id: format!("connection-{}", self.connection),
                    server_time: self.clock.now(),
                    heartbeat_secs: 10,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { .. } => {
                self.outbound.push_back(AgentFrame::HomeWelcome {
                    protocol: AGENT_PROTOCOL.to_owned(),
                    home_id: "home-1".to_owned(),
                    node: NodeName::parse("mac-studio").expect("node slug"),
                    connection_id: format!("connection-{}", self.connection),
                });
            }
            AgentFrame::SurfacePair { code, .. } => {
                self.outbound.extend(self.pair_frames.iter().cloned());
                let fresh = self
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .used_codes
                    .insert(code.clone());
                if fresh {
                    self.outbound.push_back(AgentFrame::SurfaceIssued {
                        surface_id: self.surface_id.clone(),
                        token: self.token.clone(),
                        scopes: self.scopes.clone(),
                        expires_at: self.expires_at,
                    });
                } else {
                    self.outbound.push_back(AgentFrame::Error {
                        kind: ErrorKind::Refused,
                        message: "pairing code is invalid, expired or already used".to_owned(),
                        retryable: false,
                    });
                }
            }
            AgentFrame::Heartbeat { .. } => {
                let send_live = {
                    let mut state = self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.live_frames_sent || self.live_frames.is_empty() {
                        false
                    } else {
                        state.live_frames_sent = true;
                        true
                    }
                };
                if send_live {
                    self.outbound.extend(self.live_frames.iter().cloned());
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn client(dir: &Path, home: FakeHome) -> AgentClient {
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        dir.to_path_buf(),
        "home.example",
    );
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir).expect("node key");
    AgentClient::new(
        dir.to_path_buf(),
        key,
        config,
        Box::new(home),
        Arc::new(FixedClock(ts())),
    )
}

fn request() -> SurfacePairRequest {
    SurfacePairRequest::new(
        CODE,
        SurfaceId(SURFACE.to_owned()),
        SessionId::parse(SESSION).expect("session id"),
        "opencode",
    )
}

#[test]
fn agent_pair_writes_0600_token_and_prints_no_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = FakeHome::new();
    let mut client = client(dir.path(), home.clone());
    let request = request();

    let paired = client.pair(&request).expect("pair");
    assert_eq!(paired.surface_id, request.surface_id);
    assert_eq!(paired.scopes, home.scopes);
    assert_eq!(paired.expires_at, home.expires_at);
    assert_eq!(
        paired.token_path,
        AgentConfig::surface_token_path(dir.path(), &request.surface_id)
    );

    let token = token();
    let stored = std::fs::read_to_string(&paired.token_path).expect("token file");
    assert_eq!(stored.trim(), token.as_str());
    assert!(
        !stored.contains(CODE),
        "the code must never be written next to the token"
    );
    let mode = std::fs::metadata(&paired.token_path)
        .expect("token metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the surface token must be 0600");
    let expiry = std::fs::read_to_string(AgentConfig::surface_expiry_path(dir.path(), &request.surface_id))
        .expect("expiry file");
    assert_eq!(
        Timestamp::parse(expiry.trim()).expect("expiry timestamp"),
        home.expires_at
    );

    let debug = format!("{paired:?}");
    assert!(
        !debug.contains(token.as_str()),
        "the result Debug must not carry the token: {debug}"
    );
    let debug = format!("{request:?}");
    assert!(
        !debug.contains(CODE),
        "the request Debug must not carry the code: {debug}"
    );

    let pairs = home.surface_pairs();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, CODE);
    assert_eq!(pairs[0].1, request.surface_id);
    assert_eq!(pairs[0].2, request.session);
    assert_eq!(pairs[0].3, "opencode");
}

#[test]
fn agent_pair_refuses_a_replayed_code() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = FakeHome::new();
    let mut client = client(dir.path(), home.clone());
    let request = request();

    let paired = client.pair(&request).expect("first pairing");
    let stored = std::fs::read_to_string(&paired.token_path).expect("token file");

    let error = client.pair(&request).expect_err("a replayed code is refused");
    match error {
        AgentError::Remote {
            kind,
            message,
            retryable,
        } => {
            assert_eq!(kind, ErrorKind::Refused);
            assert!(!retryable);
            assert!(!message.contains(CODE), "the refusal must not echo the code: {message}");
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&paired.token_path).expect("token file"),
        stored,
        "a refused replay must not rewrite the stored token"
    );
    assert_eq!(home.surface_pairs().len(), 2, "both attempts are sent");
}

#[test]
fn agent_pair_applies_a_queued_rotation_for_another_surface() {
    let dir = tempfile::tempdir().expect("temp dir");
    let other = SurfaceId("01ARZ3NDEKTSV4RRFFQ69G5FAX".to_owned());
    let other_token = rotated_token();
    let other_expiry = ts().plus(Duration::from_secs(31 * 24 * 60 * 60));
    let home = FakeHome::new().with_pair_frames(vec![AgentFrame::SurfaceIssued {
        surface_id: other.clone(),
        token: other_token.clone(),
        scopes: vec![SurfaceScope::new("fleet.read")],
        expires_at: other_expiry,
    }]);
    let mut client = client(dir.path(), home);
    let request = request();

    let paired = client.pair(&request).expect("the pair answer still arrives");
    assert_eq!(
        std::fs::read_to_string(&paired.token_path).expect("paired token"),
        format!("{}\n", token())
    );
    let other_path = AgentConfig::surface_token_path(dir.path(), &other);
    assert_eq!(
        std::fs::read_to_string(&other_path).expect("rotated token"),
        format!("{other_token}\n"),
        "a queued rotation is applied before the pair answer"
    );
    let mode = std::fs::metadata(&other_path)
        .expect("token metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn agent_pair_surfaces_path_is_private() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = FakeHome::new();
    let mut client = client(dir.path(), home.clone());
    let request = request();
    let paired = client.pair(&request).expect("pair");

    for path in [
        dir.path().to_path_buf(),
        AgentConfig::surfaces_dir(dir.path()),
        AgentConfig::surface_dir(dir.path(), &request.surface_id),
    ] {
        let mode = std::fs::metadata(&path)
            .expect("surface directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{} must be private", path.display());
    }
    for path in [
        paired.token_path.clone(),
        AgentConfig::surface_expiry_path(dir.path(), &request.surface_id),
    ] {
        let mode = std::fs::metadata(&path).expect("surface file").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{} must be private", path.display());
    }
}

/// Builds a replacement token at runtime so no secret-shaped literal is committed.
fn rotated_token() -> String {
    format!("vowp_surf_{}", "B".repeat(43))
}

/// Lists every file below `root`, following directories only.
fn files_below(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn agent_applies_a_rotated_token_from_the_live_connection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let rotated = rotated_token();
    let rotated_expiry = ts().plus(Duration::from_secs(31 * 24 * 60 * 60));
    let home = FakeHome::new().with_live_frames(vec![
        AgentFrame::SurfaceIssued {
            surface_id: SurfaceId(SURFACE.to_owned()),
            token: rotated.clone(),
            scopes: vec![
                SurfaceScope::new(format!("session.read:{SESSION}")),
                SurfaceScope::new(format!("session.write:{SESSION}")),
                SurfaceScope::new("fleet.read"),
            ],
            expires_at: rotated_expiry,
        },
        AgentFrame::Superseded {
            connection_id: "test-done".to_owned(),
        },
    ]);
    let mut client = client(dir.path(), home);
    let request = request();
    let paired = client.pair(&request).expect("pair");
    let old = std::fs::read_to_string(&paired.token_path).expect("old token");
    assert_eq!(old.trim(), token());

    let outcome = client
        .run(&RunOptions::new(AgentRole::Both))
        .expect("the live connection ends cleanly");
    assert_eq!(
        outcome,
        RunOutcome::Superseded {
            connection_id: "test-done".to_owned()
        }
    );

    let stored = std::fs::read_to_string(&paired.token_path).expect("rotated token file");
    assert_eq!(stored.trim(), rotated.as_str(), "the live frame replaces the token");
    let mode = std::fs::metadata(&paired.token_path)
        .expect("token metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the rotated token must stay 0600");
    let expiry = std::fs::read_to_string(AgentConfig::surface_expiry_path(dir.path(), &request.surface_id))
        .expect("expiry file");
    assert_eq!(expiry.trim(), rotated_expiry.as_rfc3339());

    let session = std::fs::read_to_string(AgentConfig::surface_session_path(dir.path(), &request.surface_id))
        .expect("session binding");
    assert_eq!(session, SESSION, "the existing binding is left in place");

    let status_text = std::fs::read_to_string(dir.path().join("status.json")).expect("status");
    let status: serde_json::Value = serde_json::from_str(&status_text).expect("status JSON");
    assert_eq!(
        status["surface_token_expires_at"],
        rotated_expiry.as_rfc3339(),
        "status reports the new expiry"
    );

    for path in files_below(dir.path()) {
        if path == paired.token_path {
            continue;
        }
        let bytes = std::fs::read(&path).expect("state file");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(&rotated),
            "{} must not contain the rotated token",
            path.display()
        );
    }
}

#[test]
fn agent_revoked_surface_deletes_its_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let surface = SurfaceId(SURFACE.to_owned());
    let home = FakeHome::new().with_live_frames(vec![
        AgentFrame::SurfaceRevoked {
            surface_id: surface.clone(),
            reason: "surface was revoked".to_owned(),
        },
        AgentFrame::Superseded {
            connection_id: "test-done".to_owned(),
        },
    ]);
    let mut client = client(dir.path(), home);
    let paired = client.pair(&request()).expect("pair");
    assert!(paired.token_path.is_file());

    let outcome = client
        .run(&RunOptions::new(AgentRole::Both))
        .expect("the live connection ends cleanly");
    assert_eq!(
        outcome,
        RunOutcome::Superseded {
            connection_id: "test-done".to_owned()
        }
    );

    assert!(!paired.token_path.exists(), "a revoked surface deletes its token file");
    assert!(
        !AgentConfig::surface_expiry_path(dir.path(), &surface).exists(),
        "a revoked surface deletes its expiry file"
    );
    let status_text = std::fs::read_to_string(dir.path().join("status.json")).expect("status");
    let status: serde_json::Value = serde_json::from_str(&status_text).expect("status JSON");
    assert!(
        status["surface_token_expires_at"].is_null(),
        "status no longer reports the revoked token's expiry"
    );
}

#[test]
fn agent_marks_a_refused_surface_needs_pair_and_stops_retrying() {
    let dir = tempfile::tempdir().expect("temp dir");
    let surface = SurfaceId(SURFACE.to_owned());
    let home = FakeHome::new().with_live_frames(vec![
        AgentFrame::Error {
            kind: ErrorKind::Refused,
            message: "surface token has expired".to_owned(),
            retryable: false,
        },
        AgentFrame::Superseded {
            connection_id: "test-done".to_owned(),
        },
    ]);
    let mut client = client(dir.path(), home.clone());
    let paired = client.pair(&request()).expect("pair");
    let stored = std::fs::read_to_string(&paired.token_path).expect("token file");

    // The refusal path names the surface, the reason and the recovery commands, and never
    // repeats the token. This is the same message the live `Error` frame produces.
    let refusal = client
        .mark_surface_needs_pair(&paired.surface_id, "surface token has expired")
        .expect("mark needs_pair");
    assert!(refusal.message.contains("needs pairing"), "{}", refusal.message);
    assert!(refusal.message.contains("vowp surface revoke"), "{}", refusal.message);
    assert!(refusal.message.contains("vowp surface pair"), "{}", refusal.message);
    assert!(refusal.message.contains("vowp agent pair"), "{}", refusal.message);
    assert!(
        !refusal.message.contains("vowp_surf_"),
        "the message never carries a token: {}",
        refusal.message
    );
    let status = vowp_agent::AgentStatus::load(dir.path())
        .expect("status")
        .expect("status written");
    assert_eq!(status.last_error.as_deref(), Some(refusal.message.as_str()));

    // Re-pair the same surface with a fresh one-time code so the live run exercises the
    // frame path, not only the direct marker call.
    let fresh = SurfacePairRequest::new(
        "B".repeat(52),
        SurfaceId(SURFACE.to_owned()),
        SessionId::parse(SESSION).expect("session id"),
        "opencode",
    );
    let paired = client.pair(&fresh).expect("re-pair for the live run");
    assert_eq!(
        vowp_agent::surface_statuses(dir.path())[0].state,
        vowp_agent::SurfaceState::Paired
    );
    let connections_before_run = home.connections();

    let outcome = client
        .run(&RunOptions::new(AgentRole::Both))
        .expect("a dead surface token does not end the node connection");
    assert_eq!(
        outcome,
        RunOutcome::Superseded {
            connection_id: "test-done".to_owned()
        },
        "the refusal marks the surface, then the normal terminal frame ends the run"
    );
    assert_eq!(
        home.connections(),
        connections_before_run + 1,
        "a dead token never triggers a reconnect loop"
    );

    // The dead token stays on disk for inspection, but is no longer presented.
    assert_eq!(
        std::fs::read_to_string(&paired.token_path).expect("token file"),
        stored,
        "the dead token file is kept"
    );
    assert!(
        !vowp_agent::paired_token_for_session(dir.path(), &SessionId::parse(SESSION).expect("session id"))
            .expect("read pairing")
            .is_some(),
        "a needs_pair surface is not returned as an active pairing"
    );

    let status = vowp_agent::AgentStatus::load(dir.path())
        .expect("status")
        .expect("status written");
    assert_eq!(status.connection, vowp_agent::ConnectionState::Superseded);
    assert!(
        status.surface_token_expires_at.is_none(),
        "a needs_pair surface leaves the active expiry set"
    );

    let statuses = vowp_agent::surface_statuses(dir.path());
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].surface, surface);
    assert_eq!(statuses[0].state, vowp_agent::SurfaceState::NeedsPair);
    assert!(statuses[0].expires_at.is_none());

    let text = std::fs::read_to_string(dir.path().join("status.json")).expect("status JSON");
    let document: serde_json::Value = serde_json::from_str(&text).expect("status document");
    assert_eq!(document["surfaces"][0]["state"], "needs_pair");
    assert_eq!(document["surfaces"][0]["surface"], SURFACE);
    assert!(
        document["surfaces"][0]["expires_at"].is_null(),
        "needs_pair has no active expiry: {}",
        document["surfaces"][0]
    );
    assert!(!text.contains(&token()), "status.json never carries the token");
}

#[test]
fn agent_status_shows_the_surface_prefix_and_expiry_without_the_token() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = FakeHome::new().with_live_frames(vec![AgentFrame::Superseded {
        connection_id: "test-done".to_owned(),
    }]);
    let mut client = client(dir.path(), home);
    let paired = client.pair(&request()).expect("pair");
    let outcome = client.run(&RunOptions::new(AgentRole::Both)).expect("run");
    assert_eq!(
        outcome,
        RunOutcome::Superseded {
            connection_id: "test-done".to_owned()
        }
    );

    let statuses = vowp_agent::surface_statuses(dir.path());
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].surface, paired.surface_id);
    assert_eq!(statuses[0].state, vowp_agent::SurfaceState::Paired);
    let prefix = token()
        .strip_prefix("vowp_surf_")
        .expect("scheme")
        .chars()
        .take(12)
        .collect::<String>();
    assert_eq!(statuses[0].prefix.as_deref(), Some(prefix.as_str()));
    assert_eq!(statuses[0].expires_at, Some(paired.expires_at));

    let text = std::fs::read_to_string(dir.path().join("status.json")).expect("status JSON");
    let document: serde_json::Value = serde_json::from_str(&text).expect("status document");
    assert_eq!(document["surfaces"][0]["state"], "paired");
    assert_eq!(document["surfaces"][0]["prefix"], prefix);
    assert_eq!(document["surfaces"][0]["expires_at"], paired.expires_at.as_rfc3339());
    assert_eq!(document["surface_token_expires_at"], paired.expires_at.as_rfc3339());
    assert!(!text.contains(&token()), "status.json never carries the token");
}

#[test]
fn agent_pair_ignores_unrelated_post_handshake_frames_before_surface_issued() {
    let dir = tempfile::tempdir().expect("temp dir");
    let run = RunId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAX").expect("run id");
    let job = JobId::parse("job-1").expect("job id");
    let project = ProjectName::parse("vowp").expect("project");
    let home = FakeHome::new().with_pair_frames(vec![
        AgentFrame::Ping,
        AgentFrame::OrchestratorCommand(OrchestratorCommand::Stop {
            request_id: "request-stop".to_owned(),
            name: "work".to_owned(),
        }),
        AgentFrame::CancelRun {
            run_id: run.clone(),
            epoch: 0,
            reason: "cancelled".to_owned(),
        },
        AgentFrame::RunLost {
            run_id: run.clone(),
            reason: "lost".to_owned(),
        },
        AgentFrame::Cleanup {
            run_id: run.clone(),
            job,
            worktree: PathRef::Project {
                project: project.clone(),
            },
        },
        AgentFrame::Credit {
            run_id: run.clone(),
            count: 1,
        },
        AgentFrame::Pong,
        AgentFrame::IngestAck {
            run_id: run.clone(),
            event_id: None,
            seq: None,
            duplicate: false,
            lease_revoked: None,
            error: None,
        },
        AgentFrame::LeaseLost {
            reason: "lease lost".to_owned(),
        },
        AgentFrame::LeaseRevoked {
            reason: "lease revoked".to_owned(),
            holder: "phone".to_owned(),
        },
        AgentFrame::BundleAccept {
            run_id: run.clone(),
            epoch: 0,
            offset: 0,
        },
        AgentFrame::BundleAbort {
            run_id: run,
            epoch: 0,
            reason: "aborted".to_owned(),
        },
    ]);
    let mut client = client(dir.path(), home.clone());
    let paired = client
        .pair(&request())
        .expect("pairing must wait for its SurfaceIssued instead of failing on a queued frame");
    assert_eq!(paired.surface_id, SurfaceId(SURFACE.to_owned()));
    assert!(
        home.frames().iter().any(|frame| matches!(frame, AgentFrame::Pong)),
        "a Ping ahead of SurfaceIssued must be answered"
    );
}

#[test]
fn pending_spool_bytes_counts_only_unsent_bridge_records() {
    let dir = tempfile::tempdir().expect("temp dir");
    let spool = dir.path().join("bridge").join(SURFACE).join("spool");
    std::fs::create_dir_all(&spool).expect("spool dir");
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700)).expect("spool mode");
    let surface = SurfaceId(SURFACE.to_owned());
    let session = SessionId::parse(SESSION).expect("session id");
    let frames = [
        BridgeFrame::Event {
            event: Box::new(SurfaceEvent {
                surface: surface.clone(),
                session: session.clone(),
                native_id: "one".to_owned(),
                event: EventBody::UserMessage(UserMessage {
                    text: "first".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
            }),
        },
        BridgeFrame::Event {
            event: Box::new(SurfaceEvent {
                surface,
                session,
                native_id: "two".to_owned(),
                event: EventBody::UserMessage(UserMessage {
                    text: "second".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
            }),
        },
    ];
    let mut record = String::new();
    let mut lengths = Vec::new();
    for frame in &frames {
        let mut line = serde_json::to_string(frame).expect("frame JSON");
        line.push('\n');
        lengths.push(line.len() as u64);
        record.push_str(&line);
    }
    std::fs::write(spool.join("events.jsonl"), &record).expect("events spool");
    std::fs::write(spool.join("sent.json"), r#"{"sent":["one"]}"#).expect("sent index");
    assert_eq!(
        pending_spool_bytes(dir.path()),
        lengths[1],
        "one unsent record remains pending while the other is already sent"
    );

    std::fs::write(spool.join("sent.json"), r#"{"sent":["one","two"]}"#).expect("sent index");
    assert_eq!(
        pending_spool_bytes(dir.path()),
        0,
        "sent records stay on disk but are no longer pending"
    );
}
