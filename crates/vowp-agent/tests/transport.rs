//! Hermetic WSS tests: a local TLS proxy on `127.0.0.1:0`, a generated test certificate and
//! the same frame protocol as SSH stdio. No public network, real home or real certificate.
//!
//! Spec: `docs/design/v0.4/hardening.md` §2.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::server::{Request as ServerRequest, Response as ServerResponse};
use tungstenite::protocol::Message;
use vowp_agent::{
    AgentClient, AgentClock, AgentConfig, AgentError, FrameRead, FrameStream, NodeKey, RunOptions, RunOutcome,
    Transport, TransportConfig, WssIngress, WssListener, WssTls, WssTlsMode, WssTransport, certificate_fingerprint,
    handshake_payload,
};
use vowp_protocol::{AGENT_PROTOCOL, AgentFrame, AgentPathMap, AgentRole, NodeName, ProjectName, Timestamp};

const HEARTBEAT_SECS: u64 = 10;
const SUBPROTOCOL: &str = "vowp-agent/1";

#[derive(Debug, Clone, Copy)]
struct FixedClock(Timestamp);

impl AgentClock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn sleep(&self, _duration: Duration) {}
}

fn fixed_clock() -> FixedClock {
    FixedClock(Timestamp::parse("2026-11-15T10:00:00Z").expect("timestamp"))
}

struct TestCertificate {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    fingerprint: String,
}

impl TestCertificate {
    fn generate() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("self-signed cert");
        let cert = certified.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
        let fingerprint = certificate_fingerprint(cert.as_ref());
        Self { cert, key, fingerprint }
    }

    fn server_config(&self) -> Arc<ServerConfig> {
        let config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], self.key.clone_key())
            .expect("server certificate");
        Arc::new(config)
    }
}

/// One message the fake home received, with its raw WebSocket text so a newline regression is
/// visible.
#[derive(Debug, Clone)]
struct ReceivedFrame {
    frame: AgentFrame,
    text: String,
}

#[derive(Debug, Clone, Default)]
struct HomeConfig {
    reject_signature: bool,
    replay_nonce: bool,
    revoked_after_heartbeats: Option<usize>,
    max_connections: usize,
}

#[derive(Default)]
struct HomeState {
    frames: Vec<ReceivedFrame>,
    accepted: usize,
    auth_ok: usize,
    auth_bad: usize,
}

struct TestHome {
    addr: SocketAddr,
    fingerprint: String,
    state: Arc<Mutex<HomeState>>,
    handle: Option<JoinHandle<()>>,
}

impl TestHome {
    fn start(config: HomeConfig) -> Self {
        let certificate = TestCertificate::generate();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test proxy");
        let addr = listener.local_addr().expect("test proxy address");
        let tls = certificate.server_config();
        let fingerprint = certificate.fingerprint.clone();
        let state = Arc::new(Mutex::new(HomeState::default()));
        let thread_state = Arc::clone(&state);
        let connections = config.max_connections.max(1);
        let handle = thread::spawn(move || {
            for connection in 1..=connections {
                let Ok((tcp, _)) = listener.accept() else {
                    return;
                };
                let Ok(server) = ServerConnection::new(Arc::clone(&tls)) else {
                    continue;
                };
                let stream = StreamOwned::new(server, tcp);
                let socket = match socket_accept(stream) {
                    Ok(socket) => socket,
                    Err(_) => continue,
                };
                thread_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .accepted += 1;
                serve_home(socket, &config, &thread_state, connection);
            }
        });
        Self {
            addr,
            fingerprint,
            state,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("wss://localhost:{}/agent/v1", self.addr.port())
    }

    fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    fn frames(&self) -> Vec<ReceivedFrame> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frames
            .clone()
    }

    fn auth_ok(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auth_ok
    }

    fn auth_bad(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .auth_bad
    }

    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().expect("test proxy");
        }
    }
}

type TestSocket = tungstenite::WebSocket<StreamOwned<ServerConnection, TcpStream>>;

#[allow(clippy::result_large_err)] // tungstenite's Callback returns the large ErrorResponse.
fn socket_accept(stream: StreamOwned<ServerConnection, TcpStream>) -> Result<TestSocket, String> {
    tungstenite::accept_hdr(
        stream,
        |request: &ServerRequest, mut response: ServerResponse| -> Result<_, _> {
            let offered = request
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !offered.split(',').any(|value| value.trim() == SUBPROTOCOL) {
                return Err(tungstenite::http::Response::new(Some("subprotocol".to_owned())));
            }
            response.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
            );
            Ok(response)
        },
    )
    .map_err(|error| error.to_string())
}

fn serve_home(mut socket: TestSocket, config: &HomeConfig, state: &Arc<Mutex<HomeState>>, connection: usize) {
    let mut key_id: Option<String> = None;
    let mut challenge: Option<(String, String, String)> = None;
    let mut heartbeats = 0usize;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let frame: AgentFrame = match serde_json::from_str(&text) {
                    Ok(frame) => frame,
                    Err(_) => return,
                };
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .frames
                    .push(ReceivedFrame {
                        frame: frame.clone(),
                        text: text.to_string(),
                    });
                match frame {
                    AgentFrame::AgentHello { key_id: key, .. } => {
                        key_id = Some(key);
                        let nonce = if config.replay_nonce {
                            "fixed-nonce".to_owned()
                        } else {
                            format!("nonce-{connection}")
                        };
                        let home_id = "home-1".to_owned();
                        let connection_id = format!("connection-{connection}");
                        challenge = Some((nonce.clone(), home_id.clone(), connection_id.clone()));
                        if socket
                            .send(Message::text(
                                serde_json::to_string(&AgentFrame::HomeChallenge {
                                    nonce,
                                    home_id,
                                    connection_id,
                                    server_time: Timestamp::now(),
                                    heartbeat_secs: HEARTBEAT_SECS,
                                    max_frame_bytes: 1024 * 1024,
                                })
                                .expect("challenge JSON"),
                            ))
                            .is_err()
                        {
                            return;
                        }
                    }
                    AgentFrame::AgentAuth { signature } => {
                        let ok = !config.reject_signature
                            && key_id.as_deref().zip(challenge.as_ref()).is_some_and(
                                |(key, (nonce, home_id, connection_id))| {
                                    verify_signature(key, nonce, home_id, connection_id, &signature)
                                },
                            );
                        {
                            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            if ok {
                                state.auth_ok += 1;
                            } else {
                                state.auth_bad += 1;
                            }
                        }
                        let frame = if ok {
                            AgentFrame::HomeWelcome {
                                protocol: AGENT_PROTOCOL.to_owned(),
                                home_id: "home-1".to_owned(),
                                node: NodeName::parse("mac-studio").expect("node slug"),
                                connection_id: format!("connection-{connection}"),
                            }
                        } else {
                            AgentFrame::Error {
                                kind: vowp_protocol::ErrorKind::Auth,
                                message: "signature rejected".to_owned(),
                                retryable: false,
                            }
                        };
                        if socket
                            .send(Message::text(serde_json::to_string(&frame).expect("frame JSON")))
                            .is_err()
                        {
                            return;
                        }
                        if !ok {
                            return;
                        }
                    }
                    AgentFrame::Heartbeat { .. } => {
                        heartbeats += 1;
                        if config.revoked_after_heartbeats.is_some_and(|limit| heartbeats >= limit) {
                            let frame = AgentFrame::Revoked {
                                reason: "revoked by owner".to_owned(),
                            };
                            if socket
                                .send(Message::text(serde_json::to_string(&frame).expect("revoked JSON")))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => return,
            Ok(_) => {}
            Err(_) => return,
        };
    }
}

fn verify_signature(public_hex: &str, nonce: &str, home_id: &str, connection_id: &str, signature_hex: &str) -> bool {
    let Ok(public) = hex::decode(public_hex) else {
        return false;
    };
    let Ok(public) = <[u8; 32]>::try_from(public.as_slice()) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&public) else {
        return false;
    };
    let Ok(signature) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    key.verify(&handshake_payload(nonce, home_id, connection_id), &signature)
        .is_ok()
}

fn wss_client(dir: &Path, url: &str, tls: WssTls) -> AgentClient {
    let (mode, fingerprint) = match &tls {
        WssTls::Pin { fingerprint } => (WssTlsMode::Pin, Some(fingerprint.clone())),
        WssTls::System { expected } => (WssTlsMode::System, expected.clone()),
    };
    let config = AgentConfig {
        home: "home-1".to_owned(),
        node: NodeName::parse("mac-studio").expect("node slug"),
        state_dir: dir.to_path_buf(),
        transport: TransportConfig::Wss {
            url: url.to_owned(),
            tls: mode,
            fingerprint,
        },
        path_maps: Vec::new(),
    };
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir).expect("node key");
    AgentClient::new(
        dir.to_path_buf(),
        key,
        config,
        Box::new(WssTransport::new(url, tls).expect("wss transport")),
        Arc::new(fixed_clock()),
    )
}

/// An in-memory home with the same script, for the frame-parity comparison.
#[derive(Default)]
struct StdioState {
    frames: Vec<AgentFrame>,
    auth_ok: usize,
    connections: usize,
    heartbeats: usize,
    status_on_reconnect: Option<String>,
}

/// How the stdio fake answers a heartbeat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HeartbeatReply {
    /// End the run with `Revoked`, as the parity tests expect.
    #[default]
    Revoke,
    /// Answer with `Ping` and then an old-home `PathMapUpdate` refusal, then `Revoked`
    /// on the next connection.
    RejectPathMap,
    /// Answer with a non-retryable `Internal` error, then `Revoked` on the next connection.
    RefuseOnce,
}

#[derive(Clone)]
struct StdioHome {
    state: Arc<Mutex<StdioState>>,
    heartbeat_reply: HeartbeatReply,
    status_path: Option<PathBuf>,
}

impl Default for StdioHome {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(StdioState::default())),
            heartbeat_reply: HeartbeatReply::Revoke,
            status_path: None,
        }
    }
}

impl Transport for StdioHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let connection = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connections += 1;
            state.connections
        };
        if connection > 1
            && let Some(path) = self.status_path.as_deref()
            && let Ok(text) = std::fs::read_to_string(path)
        {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status_on_reconnect = Some(text);
        }
        Ok(Box::new(StdioStream {
            state: Arc::clone(&self.state),
            heartbeat_reply: self.heartbeat_reply,
            connection,
            outbound: VecDeque::new(),
            key: None,
            challenge: None,
        }))
    }
}

struct StdioStream {
    state: Arc<Mutex<StdioState>>,
    heartbeat_reply: HeartbeatReply,
    connection: usize,
    outbound: VecDeque<AgentFrame>,
    key: Option<String>,
    challenge: Option<(String, String, String)>,
}

impl FrameStream for StdioStream {
    fn recv(&mut self, _timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        match self.outbound.pop_front() {
            Some(frame) => Ok(FrameRead::Frame(frame)),
            None => Ok(FrameRead::Closed),
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frames
            .push(frame.clone());
        match frame {
            AgentFrame::AgentHello { key_id, .. } => {
                self.key = Some(key_id.clone());
                let challenge = (
                    format!("stdio-nonce-{}", self.connection),
                    "home-1".to_owned(),
                    format!("stdio-connection-{}", self.connection),
                );
                self.challenge = Some(challenge.clone());
                self.outbound.push_back(AgentFrame::HomeChallenge {
                    nonce: challenge.0,
                    home_id: challenge.1,
                    connection_id: challenge.2,
                    server_time: Timestamp::now(),
                    heartbeat_secs: HEARTBEAT_SECS,
                    max_frame_bytes: 1024 * 1024,
                });
            }
            AgentFrame::AgentAuth { signature } => {
                let ok = self.key.as_deref().zip(self.challenge.as_ref()).is_some_and(
                    |(key, (nonce, home_id, connection_id))| {
                        verify_signature(key, nonce, home_id, connection_id, signature)
                    },
                );
                if ok {
                    self.state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .auth_ok += 1;
                    self.outbound.push_back(AgentFrame::HomeWelcome {
                        protocol: AGENT_PROTOCOL.to_owned(),
                        home_id: "home-1".to_owned(),
                        node: NodeName::parse("mac-studio").expect("node slug"),
                        connection_id: format!("stdio-connection-{}", self.connection),
                    });
                } else {
                    self.outbound.push_back(AgentFrame::Error {
                        kind: vowp_protocol::ErrorKind::Auth,
                        message: "signature rejected".to_owned(),
                        retryable: false,
                    });
                }
            }
            AgentFrame::Heartbeat { .. } => {
                let first = {
                    let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.heartbeats += 1;
                    state.heartbeats == 1
                };
                match (first, self.heartbeat_reply) {
                    (true, HeartbeatReply::RejectPathMap) => {
                        // The home answers the heartbeat with `Ping` before it has read the
                        // `PathMapUpdate` that follows it, then refuses the unknown frame.
                        self.outbound.push_back(AgentFrame::Ping);
                        self.outbound.push_back(AgentFrame::Error {
                            kind: vowp_protocol::ErrorKind::InvalidInput,
                            message: "unexpected frame".to_owned(),
                            retryable: false,
                        });
                    }
                    (true, HeartbeatReply::RefuseOnce) => {
                        self.outbound.push_back(AgentFrame::Error {
                            kind: vowp_protocol::ErrorKind::Internal,
                            message: "the home had an internal failure".to_owned(),
                            retryable: false,
                        });
                    }
                    _ => self.outbound.push_back(AgentFrame::Revoked {
                        reason: "revoked by owner".to_owned(),
                    }),
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn stdio_client(dir: &Path, home: StdioHome) -> AgentClient {
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        dir.to_path_buf(),
        "home.example",
    );
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir).expect("node key");
    AgentClient::new(dir.to_path_buf(), key, config, Box::new(home), Arc::new(fixed_clock()))
}

impl StdioHome {
    fn with_heartbeat_reply(heartbeat_reply: HeartbeatReply) -> Self {
        Self {
            state: Arc::new(Mutex::new(StdioState::default())),
            heartbeat_reply,
            status_path: None,
        }
    }

    fn with_status_path(mut self, path: PathBuf) -> Self {
        self.status_path = Some(path);
        self
    }

    fn status_on_reconnect(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status_on_reconnect
            .clone()
    }

    fn connections(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connections
    }

    fn frames(&self) -> Vec<AgentFrame> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frames
            .clone()
    }
}

#[test]
fn agent_reconnects_after_a_ping_precedes_an_old_home_path_map_refusal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join(".vowp/worktrees")).expect("project and worktree root");
    let map = AgentPathMap {
        project: ProjectName::parse("vowp").expect("project"),
        local_root: root.display().to_string(),
        worktree_root: None,
        allow: Vec::new(),
    };
    let config = AgentConfig::new(
        "home-1",
        NodeName::parse("mac-studio").expect("node slug"),
        dir.path().to_path_buf(),
        "home.example",
    )
    .with_path_maps(vec![map]);
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir.path()).expect("node key");
    let spool_marker = dir.path().join("spool").join("kept-events.jsonl");
    std::fs::create_dir_all(spool_marker.parent().expect("spool parent")).expect("spool dir");
    std::fs::write(&spool_marker, "pending events\n").expect("spool marker");
    let home =
        StdioHome::with_heartbeat_reply(HeartbeatReply::RejectPathMap).with_status_path(dir.path().join("status.json"));
    let mut client = AgentClient::new(
        dir.path().to_path_buf(),
        key,
        config,
        Box::new(home.clone()),
        Arc::new(fixed_clock()),
    );

    let outcome = client
        .run(&RunOptions::new(AgentRole::Both))
        .expect("an old home's path map refusal reconnects instead of exiting");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "{outcome:?}");
    assert_eq!(home.connections(), 2, "the agent reconnected after the refusal");
    let status = home
        .status_on_reconnect()
        .expect("the status file is written before the reconnect");
    assert!(status.contains("reconnecting"), "{status}");
    assert!(status.contains("path map"), "the refusal is recorded: {status}");
    assert_eq!(
        std::fs::read_to_string(&spool_marker).expect("spool marker survives"),
        "pending events\n",
        "the spool is preserved across the reconnect"
    );
    let frames = home.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, AgentFrame::PathMapUpdate { .. }))
            .count(),
        1,
        "path map updates are disabled for this run after the refusal"
    );
    assert!(
        frames.iter().any(|frame| matches!(frame, AgentFrame::Pong)),
        "the ping is still answered: {frames:?}"
    );
    assert!(
        AgentConfig::load(dir.path()).is_ok(),
        "the enrolled state is preserved across the reconnect"
    );
    assert!(
        dir.path().join("status.json").is_file(),
        "the status file is preserved across the reconnect"
    );
}

#[test]
fn agent_reconnects_after_any_non_retryable_home_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home =
        StdioHome::with_heartbeat_reply(HeartbeatReply::RefuseOnce).with_status_path(dir.path().join("status.json"));
    let mut client = stdio_client(dir.path(), home.clone());

    let outcome = client
        .run(&RunOptions::new(AgentRole::Both))
        .expect("a post-handshake home error reconnects instead of exiting with a failure");
    assert!(matches!(outcome, RunOutcome::Revoked { .. }), "{outcome:?}");
    assert_eq!(home.connections(), 2, "the agent reconnected after the home error");
    let status = home
        .status_on_reconnect()
        .expect("the status file is written before the reconnect");
    assert!(status.contains("reconnecting"), "{status}");
    assert!(status.contains("Internal"), "the error kind is recorded: {status}");
    let frames = home.frames();
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame, AgentFrame::Heartbeat { .. }))
            .count(),
        2,
        "one heartbeat per connection is enough to show a reconnect: {frames:?}"
    );
}

fn frame_tag(frame: &AgentFrame) -> &'static str {
    match frame {
        AgentFrame::AgentHello { .. } => "AgentHello",
        AgentFrame::AgentAuth { .. } => "AgentAuth",
        AgentFrame::Heartbeat { .. } => "Heartbeat",
        AgentFrame::Enroll { .. } => "Enroll",
        _ => "other",
    }
}

#[test]
fn wss_carries_the_same_frames_as_stdio() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = TestHome::start(HomeConfig {
        max_connections: 1,
        ..HomeConfig::default()
    });
    let mut client = wss_client(
        dir.path(),
        &home.url(),
        WssTls::Pin {
            fingerprint: home.fingerprint(),
        },
    );
    client.connect().expect("wss handshake");
    assert_eq!(home.auth_ok(), 1, "the node signature verifies over wss");
    client.heartbeat().expect("heartbeat over wss");
    drop(client);
    let wss_frames = home.frames();
    home.join();

    let tags: Vec<&str> = wss_frames.iter().map(|received| frame_tag(&received.frame)).collect();
    assert_eq!(tags, ["AgentHello", "AgentAuth", "Heartbeat"], "frame sequence");
    for received in &wss_frames {
        assert!(
            !received.text.ends_with('\n'),
            "one wss text message carries one frame with no trailing newline"
        );
        let reparsed: AgentFrame = serde_json::from_str(&received.text).expect("frame JSON");
        assert_eq!(reparsed, received.frame);
    }

    let stdio_dir = tempfile::tempdir().expect("temp dir");
    let stdio_home = StdioHome::default();
    let mut stdio = stdio_client(stdio_dir.path(), stdio_home.clone());
    stdio.connect().expect("stdio handshake");
    stdio.heartbeat().expect("stdio heartbeat");
    drop(stdio);
    let stdio_frames = stdio_home
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .frames
        .clone();
    let stdio_tags: Vec<&str> = stdio_frames.iter().map(frame_tag).collect();
    assert_eq!(tags, stdio_tags, "wss carries the same frames as the SSH stdio fake");
    let (
        AgentFrame::AgentHello {
            node: wss_node,
            key_id: wss_key,
            ..
        },
        AgentFrame::AgentHello {
            node: stdio_node,
            key_id: stdio_key,
            ..
        },
    ) = (&wss_frames[0].frame, &stdio_frames[0])
    else {
        panic!("both recordings start with AgentHello");
    };
    assert_eq!(wss_node, stdio_node);
    assert_eq!(wss_key.len(), 64, "the wss hello carries the node public key");
    assert_eq!(stdio_key.len(), 64, "the stdio hello carries the node public key");
}

#[test]
fn wss_rejects_a_replayed_nonce_and_a_bad_signature() {
    let dir = tempfile::tempdir().expect("temp dir");
    let home = TestHome::start(HomeConfig {
        replay_nonce: true,
        max_connections: 2,
        ..HomeConfig::default()
    });
    let mut client = wss_client(
        dir.path(),
        &home.url(),
        WssTls::Pin {
            fingerprint: home.fingerprint(),
        },
    );
    client.connect().expect("first wss handshake");
    let error = client.connect().expect_err("a replayed nonce is refused");
    assert!(matches!(error, AgentError::ReplayNonce), "got {error:?}");
    let auth_frames = home
        .frames()
        .into_iter()
        .filter(|received| matches!(received.frame, AgentFrame::AgentAuth { .. }))
        .count();
    assert_eq!(auth_frames, 1, "the replay must not be signed");
    drop(client);
    home.join();

    let bad_dir = tempfile::tempdir().expect("temp dir");
    let bad_home = TestHome::start(HomeConfig {
        reject_signature: true,
        max_connections: 1,
        ..HomeConfig::default()
    });
    let mut bad = wss_client(
        bad_dir.path(),
        &bad_home.url(),
        WssTls::Pin {
            fingerprint: bad_home.fingerprint(),
        },
    );
    let error = bad.connect().expect_err("a bad signature is rejected");
    assert!(matches!(error, AgentError::Auth(_)), "got {error:?}");
    assert_eq!(bad_home.auth_bad(), 1);
    assert_eq!(bad_home.auth_ok(), 0);
    drop(bad);
    bad_home.join();
}

#[test]
fn wss_refuses_a_changed_fingerprint_without_confirmation() {
    let home = TestHome::start(HomeConfig {
        max_connections: 2,
        ..HomeConfig::default()
    });
    let other = TestCertificate::generate();

    // A pin that names a different certificate is a hard refusal naming both fingerprints.
    let mut pinned = WssTransport::new(
        home.url(),
        WssTls::Pin {
            fingerprint: other.fingerprint.clone(),
        },
    )
    .expect("pin transport");
    let error = pinned.open().err().expect("a changed pinned fingerprint is refused");
    let message = error.to_string();
    assert!(message.contains("pinned certificate fingerprint changed"), "{message}");
    assert!(message.contains(&other.fingerprint), "{message}");
    assert!(message.contains(&home.fingerprint()), "{message}");

    // The same certificate is accepted and its observed fingerprint is recorded.
    let mut accepted = WssTransport::new(
        home.url(),
        WssTls::Pin {
            fingerprint: home.fingerprint(),
        },
    )
    .expect("pin transport");
    let _stream = accepted.open().expect("the pinned certificate is accepted");
    assert_eq!(
        accepted.observed_fingerprint().as_deref(),
        Some(home.fingerprint().as_str())
    );

    // A malformed pin is refused before any connection.
    assert!(matches!(
        WssTransport::new(
            home.url(),
            WssTls::Pin {
                fingerprint: "sha256:ABC".to_owned()
            }
        ),
        Err(AgentError::Config(_))
    ));
}

#[test]
fn wss_listener_refuses_a_non_loopback_bind() {
    for address in ["0.0.0.0:0", "192.0.2.10:0", "[::]:0"] {
        let ingress = WssIngress {
            listen: address.parse().expect("socket address"),
            path: "/agent/v1".to_owned(),
            ..WssIngress::loopback("127.0.0.1:0".parse().expect("loopback")).expect("loopback")
        };
        let error = WssListener::bind(&ingress)
            .err()
            .expect("a non-loopback bind is refused");
        assert!(error.to_string().contains("loopback"), "{error}");
    }
    assert!(
        WssIngress::loopback("192.0.2.10:0".parse().expect("socket address")).is_err(),
        "the loopback constructor refuses it too"
    );
}

#[test]
fn wss_route_refuses_every_path_but_agent_v1() {
    let ingress = WssIngress::loopback("127.0.0.1:0".parse().expect("loopback")).expect("loopback");
    let listener = WssListener::bind(&ingress).expect("bind listener");
    let addr = listener.local_addr().expect("listener address");

    let server = thread::spawn(move || {
        let mut results = Vec::new();
        for index in 0..8 {
            match listener.accept() {
                Ok(connection) if index == 7 => {
                    // The one valid connection proves the line adapter both ways.
                    let (mut reader, mut writer) = connection.split().expect("split valid connection");
                    let mut line = String::new();
                    use std::io::BufRead;
                    reader.read_line(&mut line).expect("read one agent line");
                    writer.write_all(b"{\"answer\":true}\n").expect("write one home line");
                    writer.flush().expect("flush home line");
                    results.push(format!("accepted:{line}"));
                }
                Ok(connection) => results.push(format!("accepted:{}", connection.connection_id())),
                Err(error) => results.push(error.to_string()),
            }
        }
        results
    });

    let mut statuses = Vec::new();
    for path in ["/", "/agent", "/agent/v1/", "/health", "/agent/v1?token=x"] {
        let url = format!("ws://127.0.0.1:{}{path}", addr.port());
        let mut request = url.as_str().into_client_request().expect("generated websocket request");
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
        );
        let stream = TcpStream::connect(addr).expect("connect listener");
        match tungstenite::client::client(request, stream) {
            Ok(_) => panic!("{path} must be refused"),
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                statuses.push(response.status().as_u16());
            }
            Err(error) => panic!("{path}: unexpected error {error:?}"),
        }
    }
    // A non-upgrade GET on the one route is 426 Upgrade Required.
    let mut raw = TcpStream::connect(addr).expect("connect listener");
    raw.write_all(b"GET /agent/v1 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write raw request");
    raw.flush().expect("flush raw request");
    let mut response = Vec::new();
    let mut chunk = [0u8; 512];
    while !response.windows(2).any(|window| window == b"\r\n") {
        let read = raw.read(&mut chunk).expect("read raw response");
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&response);
    assert!(head.starts_with("HTTP/1.1 426"), "{head}");
    statuses.push(426);

    // A missing subprotocol is 400.
    let url = format!("ws://127.0.0.1:{}/agent/v1", addr.port());
    let stream = TcpStream::connect(addr).expect("connect listener");
    match tungstenite::client::client(url, stream) {
        Ok(_) => panic!("a missing subprotocol must be refused"),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
            statuses.push(response.status().as_u16());
        }
        Err(error) => panic!("unexpected error {error:?}"),
    }
    assert_eq!(statuses, [404, 404, 404, 404, 404, 426, 400]);

    // The one valid route upgrades, selects the subprotocol and carries one JSON frame per
    // text message in both directions.
    let url = format!("ws://127.0.0.1:{}/agent/v1", addr.port());
    let mut request = url.as_str().into_client_request().expect("generated websocket request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
    );
    let stream = TcpStream::connect(addr).expect("connect listener");
    let (mut socket, response) = tungstenite::client::client(request, stream).expect("valid upgrade");
    assert_eq!(
        response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok()),
        Some(SUBPROTOCOL)
    );
    socket
        .send(Message::text("{\"hello\":true}"))
        .expect("send one agent frame");
    let reply = socket.read().expect("read the home reply");
    assert_eq!(reply.into_text().expect("text reply"), "{\"answer\":true}");

    let results = server.join().expect("listener thread");
    assert!(
        results
            .iter()
            .all(|result| !result.starts_with("accepted:") || result.contains("{\"hello\":true}")),
        "only the valid route is accepted: {results:?}"
    );
    assert!(
        results.iter().any(|result| result == "accepted:{\"hello\":true}\n"),
        "the line adapter adds exactly one newline: {results:?}"
    );
}

/// A stdio home that also reports a certificate fingerprint, as the WSS transport does.
struct FingerprintHome {
    inner: StdioHome,
    fingerprint: String,
}

impl Transport for FingerprintHome {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        self.inner.open()
    }

    fn recorded_fingerprint(&self) -> Option<String> {
        Some(self.fingerprint.clone())
    }
}

#[test]
fn agent_records_the_first_wss_fingerprint() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fingerprint = certificate_fingerprint(b"first-leaf");
    let config = AgentConfig {
        home: "home-1".to_owned(),
        node: NodeName::parse("mac-studio").expect("node slug"),
        state_dir: dir.path().to_path_buf(),
        transport: TransportConfig::Wss {
            url: "wss://home.example/agent/v1".to_owned(),
            tls: WssTlsMode::System,
            fingerprint: None,
        },
        path_maps: Vec::new(),
    };
    config.write().expect("agent.json");
    let key = NodeKey::load_or_generate(dir.path()).expect("node key");
    let mut client = AgentClient::new(
        dir.path().to_path_buf(),
        key,
        config,
        Box::new(FingerprintHome {
            inner: StdioHome::default(),
            fingerprint: fingerprint.clone(),
        }),
        Arc::new(fixed_clock()),
    );
    client.connect().expect("a system-mode handshake records the leaf");
    let stored = AgentConfig::load(dir.path()).expect("stored config");
    match stored.transport {
        TransportConfig::Wss {
            tls: WssTlsMode::System,
            fingerprint: Some(recorded),
            ..
        } => assert_eq!(recorded, fingerprint, "the first leaf fingerprint is informational"),
        other => panic!("expected a recorded system-mode fingerprint, got {other:?}"),
    }
}
