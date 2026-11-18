//! The optional WSS ingress: a blocking TLS WebSocket transport and a loopback listener.
//!
//! Spec: `docs/design/v0.4/hardening.md` §2. The proxy terminates TLS and owns the public
//! port; VOWP binds only a loopback address and serves exactly one route, `/agent/v1`, with
//! the `vowp-agent/1` subprotocol. One WebSocket **text** message carries exactly one JSON
//! [`AgentFrame`](vowp_protocol::AgentFrame), with no trailing newline. The listener's line
//! adapter adds the newline back for the unchanged gateway reader, so every frame, the
//! node-key challenge and the lease rules are the same code paths as SSH stdio.
//!
//! The module holds no token, code or path map in a log or error. The certificate
//! fingerprint is a public value; TLS private keys never enter VOWP.

use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::server::ErrorResponse;
use tungstenite::http::{HeaderValue, StatusCode};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Message, WebSocket, WebSocketConfig};
use vowp_protocol::{AgentFrame, WssIngressConfig};

use crate::config::{TransportConfig, WssTlsMode};
use crate::error::AgentError;
use crate::transport::{FrameRead, FrameStream, MAX_FRAME_BYTES, Transport};

/// The one route the listener serves.
pub const WSS_PATH: &str = "/agent/v1";
/// The WebSocket subprotocol every connection must offer and the server must select.
pub const WSS_SUBPROTOCOL: &str = "vowp-agent/1";
/// Default concurrent accepted connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;
/// Default TLS and WebSocket handshake timeout.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the pump waits for an inbound frame before it sends a queued outbound one.
const PUMP_READ_TIMEOUT: Duration = Duration::from_millis(100);
/// Largest HTTP request head accepted before the upgrade.
const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;

/// How the agent verifies the proxy's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WssTls {
    /// System trust store plus hostname verification; the first observed leaf fingerprint is
    /// recorded, and a later change is refused until the owner confirms it.
    System {
        /// Fingerprint recorded by an earlier connection, when one exists.
        expected: Option<String>,
    },
    /// A hard pin for a self-signed deployment. A mismatch is a refusal, never a warning.
    Pin {
        /// Expected `sha256:<hex>` leaf fingerprint.
        fingerprint: String,
    },
}

/// The listener settings resolved from `[ingress.wss]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WssIngress {
    /// Loopback address the listener binds.
    pub listen: SocketAddr,
    /// The single literal route.
    pub path: String,
    /// Concurrent accepted connections.
    pub max_connections: usize,
    /// Largest accepted message in bytes.
    pub max_frame_bytes: usize,
    /// TLS and WebSocket handshake timeout.
    pub handshake_timeout: Duration,
}

impl WssIngress {
    /// Resolves and validates a `[ingress.wss]` table.
    ///
    /// A non-loopback address is refused before any socket is opened, and the route must be
    /// exactly [`WSS_PATH`].
    pub fn from_config(config: &WssIngressConfig) -> Result<Self, AgentError> {
        config
            .validate()
            .map_err(|error| AgentError::Config(error.to_string()))?;
        let listen = config
            .listen_addr()
            .map_err(|error| AgentError::Config(error.to_string()))?;
        Ok(Self {
            listen,
            path: config.path.clone(),
            max_connections: usize::try_from(config.max_connections).unwrap_or(DEFAULT_MAX_CONNECTIONS),
            max_frame_bytes: usize::try_from(config.max_frame_bytes).unwrap_or(MAX_FRAME_BYTES),
            handshake_timeout: Duration::from_secs(config.handshake_timeout_secs),
        })
    }

    /// Builds loopback-only settings for tests and embedding.
    pub fn loopback(listen: SocketAddr) -> Result<Self, AgentError> {
        if !listen.ip().is_loopback() {
            return Err(AgentError::Config(format!(
                "the wss listener must bind a loopback address, not {}",
                listen.ip()
            )));
        }
        Ok(Self {
            listen,
            path: WSS_PATH.to_owned(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_frame_bytes: MAX_FRAME_BYTES,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        })
    }
}

/// Returns the `sha256:<hex>` leaf-certificate fingerprint.
pub fn certificate_fingerprint(der: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(der)))
}

/// Returns whether `value` is a `sha256:` fingerprint with 64 lowercase hex characters.
pub fn is_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}

/// The parsed pieces of a `wss://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WssUrl {
    /// Host name or IP address.
    pub host: String,
    /// TCP port; 443 when the URL omits it.
    pub port: u16,
    /// The one accepted path.
    pub path: String,
}

impl WssUrl {
    /// Parses a `wss://host[:port]/agent/v1` URL.
    ///
    /// No query, fragment, user-info, plain `ws://` or alternative path is accepted.
    pub fn parse(url: &str) -> Result<Self, AgentError> {
        let rest = url
            .strip_prefix("wss://")
            .ok_or_else(|| AgentError::Config("the wss url must start with wss://".to_owned()))?;
        if rest.is_empty() {
            return Err(AgentError::Config("the wss url has no host".to_owned()));
        }
        if rest.contains('?') || rest.contains('#') || rest.contains('@') {
            return Err(AgentError::Config(
                "the wss url must not carry a query, fragment or user-info".to_owned(),
            ));
        }
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        if path != WSS_PATH {
            return Err(AgentError::Config(format!(
                "the wss url path must be exactly {WSS_PATH}"
            )));
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let Some(end) = bracketed.find(']') else {
                return Err(AgentError::Config(
                    "the wss url has an unterminated IPv6 host".to_owned(),
                ));
            };
            let host = &bracketed[..end];
            let remainder = &bracketed[end + 1..];
            let port = match remainder {
                "" => 443,
                rest => rest
                    .strip_prefix(':')
                    .and_then(|port| port.parse::<u16>().ok())
                    .ok_or_else(|| AgentError::Config("the wss url has an invalid port".to_owned()))?,
            };
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) if !host.contains(':') => {
                    let port = port
                        .parse::<u16>()
                        .map_err(|_| AgentError::Config("the wss url has an invalid port".to_owned()))?;
                    (host, port)
                }
                _ => (authority, 443),
            }
        };
        if host.is_empty() {
            return Err(AgentError::Config("the wss url has no host".to_owned()));
        }
        if port == 0 {
            return Err(AgentError::Config("the wss url port must not be zero".to_owned()));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    fn request_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("wss://{host}:{}{}", self.port, self.path)
    }
}

/// A blocking TLS WebSocket transport for the one agent connection.
#[derive(Debug, Clone)]
pub struct WssTransport {
    url: WssUrl,
    tls: WssTls,
    max_frame_bytes: usize,
    handshake_timeout: Duration,
    observed: Arc<Mutex<Option<String>>>,
}

impl WssTransport {
    /// Builds a transport for one URL and TLS mode.
    pub fn new(url: impl AsRef<str>, tls: WssTls) -> Result<Self, AgentError> {
        let url = WssUrl::parse(url.as_ref())?;
        if let WssTls::Pin { fingerprint } = &tls
            && !is_fingerprint(fingerprint)
        {
            return Err(AgentError::Config(
                "a pinned wss fingerprint must be sha256:<64 lowercase hex>".to_owned(),
            ));
        }
        Ok(Self {
            url,
            tls,
            max_frame_bytes: MAX_FRAME_BYTES,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            observed: Arc::new(Mutex::new(None)),
        })
    }

    /// Builds the transport a stored `agent.json` transport section names.
    pub fn from_transport_config(config: &TransportConfig) -> Result<Self, AgentError> {
        let TransportConfig::Wss { url, tls, fingerprint } = config else {
            return Err(AgentError::Config("agent.transport is not wss".to_owned()));
        };
        let tls = match tls {
            WssTlsMode::System => WssTls::System {
                expected: fingerprint.clone(),
            },
            WssTlsMode::Pin => WssTls::Pin {
                fingerprint: fingerprint.clone().ok_or_else(|| {
                    AgentError::Config("agent.transport tls = \"pin\" requires a fingerprint".to_owned())
                })?,
            },
        };
        Self::new(url, tls)
    }

    /// Replaces the accepted frame size.
    pub fn with_max_frame_bytes(mut self, max: usize) -> Self {
        self.max_frame_bytes = max;
        self
    }

    /// Replaces the TLS and WebSocket handshake timeout.
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Returns the fingerprint the last connection presented, when one was observed.
    pub fn observed_fingerprint(&self) -> Option<String> {
        self.observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Transport for WssTransport {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        let (config, verifier) = client_tls_config(&self.tls)?;
        let addresses: Vec<SocketAddr> = (self.url.host.as_str(), self.url.port)
            .to_socket_addrs()
            .map_err(|error| AgentError::Transport(format!("wss resolve {}: {error}", self.url.host)))?
            .collect();
        if addresses.is_empty() {
            return Err(AgentError::Transport(format!(
                "wss resolve {}: no addresses",
                self.url.host
            )));
        }
        let mut last_error = None;
        let mut connected = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, self.handshake_timeout) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let tcp = connected.ok_or_else(|| {
            AgentError::Transport(format!(
                "wss connect {}:{}: {}",
                self.url.host,
                self.url.port,
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "no address".to_owned())
            ))
        })?;
        tcp.set_nodelay(true)
            .map_err(|error| AgentError::Transport(format!("wss nodelay: {error}")))?;
        tcp.set_read_timeout(Some(self.handshake_timeout))
            .map_err(|error| AgentError::Transport(format!("wss timeout: {error}")))?;
        tcp.set_write_timeout(Some(self.handshake_timeout))
            .map_err(|error| AgentError::Transport(format!("wss timeout: {error}")))?;

        let server_name = ServerName::try_from(self.url.host.clone())
            .map_err(|_| AgentError::Config("the wss url host is not a usable TLS name".to_owned()))?;
        let connection = ClientConnection::new(Arc::new(config), server_name)
            .map_err(|error| AgentError::Transport(format!("wss tls: {error}")))?;
        let tls = StreamOwned::new(connection, tcp.try_clone().map_err(AgentError::Io)?);

        let mut request = self
            .url
            .request_url()
            .into_client_request()
            .map_err(|error| AgentError::Transport(format!("wss url: {error}")))?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(WSS_SUBPROTOCOL));

        let (socket, response) =
            match tungstenite::client::client_with_config(request, tls, Some(client_ws_config(self.max_frame_bytes))) {
                Ok(connected) => connected,
                Err(error) => {
                    if let Some(message) = verifier.rejection() {
                        return Err(AgentError::Transport(format!("wss tls: {message}")));
                    }
                    return Err(AgentError::Transport(format!("wss handshake: {error}")));
                }
            };
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|value| value.to_str().ok())
            != Some(WSS_SUBPROTOCOL)
        {
            return Err(AgentError::Protocol(format!(
                "the wss server did not select {WSS_SUBPROTOCOL}"
            )));
        }
        let observed = verifier
            .observed()
            .ok_or_else(|| AgentError::Transport("the wss server presented no certificate".to_owned()))?;
        // A system-mode transport also pins the first observation for later reconnects in
        // this process; a restart reads the same value from agent.json.
        if let WssTls::System { expected } = &mut self.tls
            && expected.is_none()
        {
            *expected = Some(observed.clone());
        }
        *self.observed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observed);
        let deadline = Instant::now() + self.handshake_timeout;
        Ok(Box::new(WssFrameStream {
            socket,
            tcp,
            max_frame_bytes: self.max_frame_bytes,
            handshake_deadline: Some(deadline),
            closed: false,
        }))
    }

    fn recorded_fingerprint(&self) -> Option<String> {
        self.observed_fingerprint()
    }
}

/// One established agent-side WSS stream.
pub struct WssFrameStream {
    socket: WebSocket<StreamOwned<ClientConnection, TcpStream>>,
    tcp: TcpStream,
    max_frame_bytes: usize,
    handshake_deadline: Option<Instant>,
    closed: bool,
}

impl WssFrameStream {
    fn close(&mut self, code: CloseCode, reason: &'static str) -> Result<(), tungstenite::Error> {
        self.closed = true;
        self.socket.close(Some(CloseFrame {
            code,
            reason: reason.into(),
        }))
    }
}

impl FrameStream for WssFrameStream {
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        if self.closed {
            return Ok(FrameRead::Closed);
        }
        let timeout = match (timeout, self.handshake_deadline) {
            (Some(timeout), Some(deadline)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AgentError::Transport(
                        "the wss handshake did not finish before the timeout".to_owned(),
                    ));
                }
                Some(timeout.min(remaining))
            }
            (None, Some(deadline)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AgentError::Transport(
                        "the wss handshake did not finish before the timeout".to_owned(),
                    ));
                }
                Some(remaining)
            }
            (timeout, None) => timeout,
        };
        self.tcp
            .set_read_timeout(timeout)
            .map_err(|error| AgentError::Transport(format!("wss read timeout: {error}")))?;
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    self.handshake_deadline = None;
                    let bytes = text.as_bytes();
                    if bytes.len() > self.max_frame_bytes {
                        let _ = self.close(CloseCode::Size, "frame too large");
                        return Err(AgentError::FrameTooLarge {
                            size: bytes.len(),
                            max: self.max_frame_bytes,
                        });
                    }
                    let frame = serde_json::from_slice::<AgentFrame>(bytes).map_err(AgentError::Json)?;
                    return Ok(FrameRead::Frame(frame));
                }
                Ok(Message::Binary(_)) => {
                    let _ = self.close(CloseCode::Unsupported, "text frames only");
                    return Err(AgentError::Transport("wss binary messages are refused".to_owned()));
                }
                Ok(Message::Close(_)) => {
                    self.closed = true;
                    return Ok(FrameRead::Closed);
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                {
                    return Ok(FrameRead::Timeout);
                }
                Err(tungstenite::Error::ConnectionClosed) => {
                    self.closed = true;
                    return Ok(FrameRead::Closed);
                }
                Err(tungstenite::Error::Capacity(_)) => {
                    let _ = self.close(CloseCode::Size, "frame too large");
                    return Err(AgentError::FrameTooLarge {
                        size: self.max_frame_bytes.saturating_add(1),
                        max: self.max_frame_bytes,
                    });
                }
                Err(error) => {
                    self.closed = true;
                    return Err(AgentError::Transport(format!("wss: {error}")));
                }
            }
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        if self.closed {
            return Err(AgentError::Transport("wss stream is closed".to_owned()));
        }
        let bytes = serde_json::to_vec(frame)?;
        if bytes.len() > self.max_frame_bytes {
            return Err(AgentError::FrameTooLarge {
                size: bytes.len(),
                max: self.max_frame_bytes,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| AgentError::Protocol("a frame is not UTF-8".to_owned()))?;
        self.socket
            .send(Message::text(text))
            .map_err(|error| AgentError::Transport(format!("wss write: {error}")))
    }
}

/// The client's WebSocket limits: one message is one frame, no compression, 1 MiB cap.
fn client_ws_config(max_frame_bytes: usize) -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_frame_bytes);
    config.max_frame_size = Some(max_frame_bytes);
    config
}

/// The server's WebSocket limits; an oversized message is a capacity error, closed with 1009.
fn server_ws_config(max_frame_bytes: usize) -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_frame_bytes);
    config.max_frame_size = Some(max_frame_bytes);
    config
}

/// Builds the client TLS configuration and its fingerprint verifier.
fn client_tls_config(tls: &WssTls) -> Result<(ClientConfig, Arc<FingerprintVerifier>), AgentError> {
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let inner: Option<Arc<dyn ServerCertVerifier>> = match tls {
        WssTls::System { .. } => {
            let mut roots = RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            let (added, _ignored) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err(AgentError::Config(
                    "no system trust roots are available for the wss transport".to_owned(),
                ));
            }
            let verifier =
                rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
                    .build()
                    .map_err(|error| AgentError::Config(format!("the system trust store is unusable: {error}")))?;
            Some(verifier)
        }
        WssTls::Pin { .. } => None,
    };
    let verifier = Arc::new(FingerprintVerifier {
        inner,
        expected: match tls {
            WssTls::System { expected } => expected.clone(),
            WssTls::Pin { fingerprint } => Some(fingerprint.clone()),
        },
        pin: matches!(tls, WssTls::Pin { .. }),
        observed: Mutex::new(None),
        rejection: Mutex::new(None),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|error| AgentError::Config(format!("tls 1.3 is unavailable: {error}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    Ok((config, verifier))
}

/// A verifier that captures the leaf fingerprint and enforces the configured trust.
struct FingerprintVerifier {
    inner: Option<Arc<dyn ServerCertVerifier>>,
    expected: Option<String>,
    pin: bool,
    observed: Mutex<Option<String>>,
    rejection: Mutex<Option<String>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl FingerprintVerifier {
    fn observed(&self) -> Option<String> {
        self.observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn rejection(&self) -> Option<String> {
        self.rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn refuse(&self, message: String) -> rustls::Error {
        *self.rejection.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
        rustls::Error::General(message)
    }
}

impl std::fmt::Debug for FingerprintVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FingerprintVerifier")
            .field("pin", &self.pin)
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let observed = certificate_fingerprint(end_entity.as_ref());
        if self.pin {
            if self.expected.as_deref() != Some(observed.as_str()) {
                return Err(self.refuse(format!(
                    "the pinned certificate fingerprint changed: expected {}, presented {observed}",
                    self.expected.as_deref().unwrap_or("<none>")
                )));
            }
        } else {
            let Some(inner) = &self.inner else {
                return Err(self.refuse("the system certificate verifier is unavailable".to_owned()));
            };
            inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
            if let Some(expected) = &self.expected
                && expected != &observed
            {
                return Err(self.refuse(format!(
                    "the certificate fingerprint changed: expected {expected}, presented {observed}; \
                     confirm it with `vowp agent trust --fingerprint {observed}` or re-enroll"
                )));
            }
        }
        *self.observed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observed);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        match &self.inner {
            Some(inner) => inner.verify_tls12_signature(message, cert, dss),
            None => verify_tls12_signature(message, cert, dss, &self.algorithms),
        }
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        match &self.inner {
            Some(inner) => inner.verify_tls13_signature(message, cert, dss),
            None => verify_tls13_signature(message, cert, dss, &self.algorithms),
        }
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        match &self.inner {
            Some(inner) => inner.supported_verify_schemes(),
            None => self.algorithms.supported_schemes(),
        }
    }
}

/// The loopback WebSocket listener behind the owner's TLS reverse proxy.
///
/// The proxy terminates TLS on the public port and forwards the plain WebSocket upgrade to
/// this loopback address. VOWP never terminates the public TLS connection and stores no
/// proxy key; the listener is a plain `ws://127.0.0.1:<port>/agent/v1`.
pub struct WssListener {
    listener: TcpListener,
    ingress: WssIngress,
    active: Arc<AtomicUsize>,
    next_connection: Arc<AtomicU64>,
}

impl WssListener {
    /// Binds the configured loopback address.
    ///
    /// A non-loopback address is refused before `bind`, so a misconfiguration cannot open a
    /// public port.
    pub fn bind(ingress: &WssIngress) -> Result<Self, AgentError> {
        if !ingress.listen.ip().is_loopback() {
            return Err(AgentError::Config(format!(
                "the wss listener must bind a loopback address, not {}",
                ingress.listen.ip()
            )));
        }
        if ingress.path != WSS_PATH {
            return Err(AgentError::Config(format!("the wss listener serves only {WSS_PATH}")));
        }
        let listener = TcpListener::bind(ingress.listen)
            .map_err(|error| AgentError::Transport(format!("wss bind {}: {error}", ingress.listen)))?;
        Ok(Self {
            listener,
            ingress: ingress.clone(),
            active: Arc::new(AtomicUsize::new(0)),
            next_connection: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Returns the address the listener actually bound.
    pub fn local_addr(&self) -> Result<SocketAddr, AgentError> {
        self.listener
            .local_addr()
            .map_err(|error| AgentError::Transport(format!("wss listener address: {error}")))
    }

    /// Returns the number of accepted connections still open.
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Accepts one loopback connection and finishes the WebSocket handshake.
    ///
    /// An unknown path is answered `404`, a non-upgrade `GET` `426`, a wrong subprotocol
    /// `400`, and a binary message is refused with close code `1003`.
    #[allow(clippy::result_large_err)] // tungstenite's Callback returns the large ErrorResponse.
    pub fn accept(&self) -> Result<WssConnection, AgentError> {
        let (tcp, peer) = self
            .listener
            .accept()
            .map_err(|error| AgentError::Transport(format!("wss accept: {error}")))?;
        tcp.set_nodelay(true)
            .map_err(|error| AgentError::Transport(format!("wss nodelay: {error}")))?;
        tcp.set_read_timeout(Some(self.ingress.handshake_timeout))
            .map_err(|error| AgentError::Transport(format!("wss timeout: {error}")))?;
        tcp.set_write_timeout(Some(self.ingress.handshake_timeout))
            .map_err(|error| AgentError::Transport(format!("wss timeout: {error}")))?;
        if self.active.load(Ordering::SeqCst) >= self.ingress.max_connections {
            let mut busy = tcp;
            let _ = write_http_status(&mut busy, StatusCode::SERVICE_UNAVAILABLE);
            return Err(AgentError::Transport("wss connection limit reached".to_owned()));
        }

        let mut probe = tcp.try_clone().map_err(AgentError::Io)?;
        let head = read_http_head(&mut probe, self.ingress.handshake_timeout)?;
        let request = HttpHead::parse(&head)?;
        if let Err((status, message)) = request.policy(&self.ingress.path) {
            let _ = write_http_status(&mut probe, status);
            return Err(AgentError::Transport(format!("wss {status}: {message}")));
        }

        let replay = PrefixedStream {
            prefix: head,
            position: 0,
            inner: tcp,
        };
        let socket = match tungstenite::accept_hdr_with_config(
            replay,
            move |request: &tungstenite::handshake::server::Request,
                  mut response: tungstenite::handshake::server::Response|
                  -> Result<_, ErrorResponse> {
                let offered = request
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if !offered
                    .split(',')
                    .any(|value| value.trim().eq_ignore_ascii_case(WSS_SUBPROTOCOL))
                {
                    return Err(error_response(StatusCode::BAD_REQUEST));
                }
                if request.uri().path() != WSS_PATH {
                    return Err(error_response(StatusCode::NOT_FOUND));
                }
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(WSS_SUBPROTOCOL));
                Ok(response)
            },
            Some(server_ws_config(self.ingress.max_frame_bytes)),
        ) {
            Ok(socket) => socket,
            Err(error) => return Err(AgentError::Transport(format!("wss upgrade refused: {error}"))),
        };
        self.active.fetch_add(1, Ordering::SeqCst);
        Ok(WssConnection {
            socket: Some(socket),
            tcp: Some(probe),
            peer,
            max_frame_bytes: self.ingress.max_frame_bytes,
            active: Arc::clone(&self.active),
            armed: true,
            connection: self.next_connection.fetch_add(1, Ordering::SeqCst),
        })
    }
}

fn error_response(status: StatusCode) -> ErrorResponse {
    let mut response = tungstenite::http::Response::new(Some(status.canonical_reason().unwrap_or("error").to_owned()));
    *response.status_mut() = status;
    response
}

/// One accepted WSS connection, ready to be split into the gateway's reader and writer.
pub struct WssConnection {
    socket: Option<WebSocket<PrefixedStream<TcpStream>>>,
    tcp: Option<TcpStream>,
    peer: SocketAddr,
    max_frame_bytes: usize,
    active: Arc<AtomicUsize>,
    armed: bool,
    connection: u64,
}

impl WssConnection {
    /// Returns the peer address (always a loopback address).
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Returns the connection number used in diagnostics; no secret or path.
    pub fn connection_id(&self) -> u64 {
        self.connection
    }

    /// Splits the connection into the newline adapter the gateway reads and writes.
    ///
    /// A pump thread owns the WebSocket and alternates between one bounded read and the
    /// queued outgoing lines, so a quiet home never blocks a heartbeat write.
    pub fn split(mut self) -> Result<(WssLineReader, WssLineWriter), AgentError> {
        let socket = self
            .socket
            .take()
            .ok_or_else(|| AgentError::Transport("the wss connection is already split".to_owned()))?;
        let tcp = self
            .tcp
            .take()
            .ok_or_else(|| AgentError::Transport("the wss connection is already split".to_owned()))?;
        tcp.set_read_timeout(Some(PUMP_READ_TIMEOUT))
            .map_err(|error| AgentError::Transport(format!("wss pump timeout: {error}")))?;
        let pump_tcp = tcp.try_clone().map_err(AgentError::Io)?;
        let (incoming, incoming_rx) = mpsc::channel();
        let (outgoing, outgoing_rx) = mpsc::channel();
        let active = Arc::clone(&self.active);
        self.armed = false;
        let max_frame_bytes = self.max_frame_bytes;
        let spawned = std::thread::Builder::new()
            .name(format!("vowp-wss-{}", self.connection))
            .spawn(move || {
                pump(socket, max_frame_bytes, outgoing_rx, incoming, pump_tcp);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(error) = spawned {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(AgentError::Transport(format!(
                "the wss pump thread could not start: {error}"
            )));
        }
        Ok((
            WssLineReader::new(incoming_rx),
            WssLineWriter::new(outgoing, max_frame_bytes),
        ))
    }
}

impl Drop for WssConnection {
    fn drop(&mut self) {
        if self.armed {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The reader half of the listener adapter: one text message becomes one newline-terminated
/// line, so the unchanged gateway line reader sees the same bytes as SSH stdio.
pub struct WssLineReader {
    incoming: Receiver<Vec<u8>>,
    line: Vec<u8>,
    position: usize,
    closed: bool,
}

impl WssLineReader {
    fn new(incoming: Receiver<Vec<u8>>) -> Self {
        Self {
            incoming,
            line: Vec::new(),
            position: 0,
            closed: false,
        }
    }
}

impl Read for WssLineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        if available.is_empty() {
            return Ok(0);
        }
        let count = available.len().min(buf.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for WssLineReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position >= self.line.len() && !self.closed {
            match self.incoming.recv() {
                Ok(mut message) => {
                    message.push(b'\n');
                    self.line = message;
                    self.position = 0;
                }
                Err(_) => {
                    self.closed = true;
                    self.line.clear();
                    self.position = 0;
                }
            }
        }
        Ok(&self.line[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        self.position = (self.position + amount).min(self.line.len());
    }
}

/// The writer half of the listener adapter: one newline-terminated line becomes one text
/// message. A partial line is flushed as a message of its own.
pub struct WssLineWriter {
    outgoing: Sender<Vec<u8>>,
    buffer: Vec<u8>,
    max_frame_bytes: usize,
    closed: bool,
}

impl WssLineWriter {
    fn new(outgoing: Sender<Vec<u8>>, max_frame_bytes: usize) -> Self {
        Self {
            outgoing,
            buffer: Vec::new(),
            max_frame_bytes,
            closed: false,
        }
    }

    fn send_line(&mut self, line: Vec<u8>) -> io::Result<()> {
        if line.len() > self.max_frame_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds the {}-byte maximum", self.max_frame_bytes),
            ));
        }
        if self.outgoing.send(line).is_err() {
            self.closed = true;
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the wss connection closed"));
        }
        Ok(())
    }
}

impl Write for WssLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the wss connection closed"));
        }
        self.buffer.extend_from_slice(buf);
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=position).collect();
            line.pop();
            self.send_line(line)?;
        }
        if self.buffer.len() > self.max_frame_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds the {}-byte maximum", self.max_frame_bytes),
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let line = std::mem::take(&mut self.buffer);
        self.send_line(line)
    }
}

/// Owns the accepted WebSocket and moves messages in both directions.
fn pump(
    mut socket: WebSocket<PrefixedStream<TcpStream>>,
    max_frame_bytes: usize,
    outgoing: Receiver<Vec<u8>>,
    incoming: Sender<Vec<u8>>,
    tcp: TcpStream,
) {
    let _ = tcp.set_read_timeout(Some(PUMP_READ_TIMEOUT));
    loop {
        loop {
            match outgoing.try_recv() {
                Ok(line) => {
                    if line.len() > max_frame_bytes {
                        let _ = socket.close(Some(CloseFrame {
                            code: CloseCode::Size,
                            reason: "frame too large".into(),
                        }));
                        return;
                    }
                    let Ok(text) = String::from_utf8(line) else {
                        let _ = socket.close(Some(CloseFrame {
                            code: CloseCode::Invalid,
                            reason: "frame is not UTF-8".into(),
                        }));
                        return;
                    };
                    if socket.send(Message::text(text)).is_err() {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = socket.close(None);
                    return;
                }
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if text.len() > max_frame_bytes {
                    let _ = socket.close(Some(CloseFrame {
                        code: CloseCode::Size,
                        reason: "frame too large".into(),
                    }));
                    return;
                }
                if incoming.send(text.as_bytes().to_vec()).is_err() {
                    return;
                }
            }
            Ok(Message::Binary(_)) => {
                let _ = socket.close(Some(CloseFrame {
                    code: CloseCode::Unsupported,
                    reason: "text frames only".into(),
                }));
                return;
            }
            Ok(Message::Close(_)) => return,
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::Capacity(_)) => {
                let _ = socket.close(Some(CloseFrame {
                    code: CloseCode::Size,
                    reason: "frame too large".into(),
                }));
                return;
            }
            Err(_) => return,
        }
    }
}

fn write_http_status(stream: &mut impl Write, status: StatusCode) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error")
    )?;
    stream.flush()
}

/// Reads exactly the HTTP request head, one byte at a time, so no WebSocket byte is consumed
/// before tungstenite's handshake sees it.
fn read_http_head(stream: &mut impl Read, timeout: Duration) -> Result<Vec<u8>, AgentError> {
    let deadline = Instant::now() + timeout;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
        if head.len() >= MAX_HTTP_HEAD_BYTES {
            return Err(AgentError::Transport("the wss request head is too large".to_owned()));
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(AgentError::Transport(
                    "the wss client closed during the handshake".to_owned(),
                ));
            }
            Ok(_) => head.push(byte[0]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(AgentError::Transport("the wss handshake timed out".to_owned()));
                }
            }
            Err(error) => return Err(AgentError::Transport(format!("wss handshake: {error}"))),
        }
    }
}

/// A minimal parsed HTTP request head, used before tungstenite's upgrade.
struct HttpHead {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl HttpHead {
    fn parse(bytes: &[u8]) -> Result<Self, AgentError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| AgentError::Transport("the wss request head is not UTF-8".to_owned()))?;
        let mut lines = text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| AgentError::Transport("the wss request line is missing".to_owned()))?;
        let mut parts = request_line.split_whitespace();
        let method = parts
            .next()
            .ok_or_else(|| AgentError::Transport("the wss request method is missing".to_owned()))?;
        let target = parts
            .next()
            .ok_or_else(|| AgentError::Transport("the wss request target is missing".to_owned()))?;
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        Ok(Self {
            method: method.to_owned(),
            path: target.to_owned(),
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Returns the refusal status and reason for one request, if it is not a valid upgrade.
    fn policy(&self, path: &str) -> Result<(), (StatusCode, &'static str)> {
        if self.method != "GET" {
            return Err((StatusCode::METHOD_NOT_ALLOWED, "the route accepts GET only"));
        }
        if self.path != path {
            return Err((StatusCode::NOT_FOUND, "the route is /agent/v1 only"));
        }
        let upgrade = self
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let connection = self
            .header("connection")
            .is_some_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case("upgrade")));
        if !upgrade || !connection {
            return Err((StatusCode::UPGRADE_REQUIRED, "a websocket upgrade is required"));
        }
        if self.header("sec-websocket-version") != Some("13") {
            return Err((StatusCode::UPGRADE_REQUIRED, "websocket version 13 is required"));
        }
        let offered = self.header("sec-websocket-protocol").unwrap_or_default();
        if !offered
            .split(',')
            .any(|value| value.trim().eq_ignore_ascii_case(WSS_SUBPROTOCOL))
        {
            return Err((StatusCode::BAD_REQUEST, "the vowp-agent/1 subprotocol is required"));
        }
        Ok(())
    }
}

/// A stream that replays the already-read request head before reading the socket.
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    position: usize,
    inner: S,
}

impl<S: Read> Read for PrefixedStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position < self.prefix.len() {
            let count = (self.prefix.len() - self.position).min(buf.len());
            buf[..count].copy_from_slice(&self.prefix[self.position..self.position + count]);
            self.position += count;
            return Ok(count);
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for PrefixedStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wss_url_accepts_the_documented_shape_and_rejects_every_other() {
        let url = WssUrl::parse("wss://home.example/agent/v1").expect("documented URL");
        assert_eq!(url.host, "home.example");
        assert_eq!(url.port, 443);
        assert_eq!(url.path, WSS_PATH);

        let with_port = WssUrl::parse("wss://home.example:8443/agent/v1").expect("explicit port");
        assert_eq!(with_port.port, 8443);

        let ipv6 = WssUrl::parse("wss://[::1]:8443/agent/v1").expect("IPv6 URL");
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.port, 8443);

        for bad in [
            "ws://home.example/agent/v1",
            "wss://home.example/",
            "wss://home.example/other",
            "wss://home.example/agent/v1?token=x",
            "wss://user:test@example.invalid/agent/v1",
            "wss://home.example:0/agent/v1",
            "wss://",
        ] {
            assert!(WssUrl::parse(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn fingerprints_are_sha256_of_the_leaf_der() {
        let fingerprint = certificate_fingerprint(b"leaf");
        assert!(is_fingerprint(&fingerprint));
        assert_eq!(fingerprint.len(), "sha256:".len() + 64);
        assert!(!is_fingerprint("sha256:ABC"));
        assert!(!is_fingerprint("md5:00"));
        assert_eq!(fingerprint, format!("sha256:{}", hex::encode(Sha256::digest(b"leaf"))));
    }

    #[test]
    fn ingress_from_config_defaults_off_to_loopback() {
        let ingress = WssIngress::from_config(&WssIngressConfig::default()).expect("defaults");
        assert_eq!(ingress.listen.ip().to_string(), "127.0.0.1");
        assert_eq!(ingress.path, WSS_PATH);
        assert_eq!(ingress.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(ingress.max_frame_bytes, MAX_FRAME_BYTES);
        assert_eq!(ingress.handshake_timeout, DEFAULT_HANDSHAKE_TIMEOUT);
    }

    #[derive(Debug)]
    struct AcceptAll;

    impl ServerCertVerifier for AcceptAll {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            Vec::new()
        }
    }

    #[test]
    fn system_mode_refuses_a_changed_recorded_fingerprint() {
        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        let old = certificate_fingerprint(b"old-leaf");
        let new = certificate_fingerprint(b"new-leaf");
        let verifier = FingerprintVerifier {
            inner: Some(Arc::new(AcceptAll)),
            expected: Some(old.clone()),
            pin: false,
            observed: Mutex::new(None),
            rejection: Mutex::new(None),
            algorithms: provider.signature_verification_algorithms,
        };
        let cert = CertificateDer::from(b"new-leaf".to_vec());
        let name = ServerName::try_from("home.example").expect("server name");
        let error = verifier
            .verify_server_cert(&cert, &[], &name, &[], UnixTime::since_unix_epoch(Duration::ZERO))
            .expect_err("a changed fingerprint is refused");
        let message = error.to_string();
        assert!(message.contains("certificate fingerprint changed"), "{message}");
        assert!(message.contains(&old), "{message}");
        assert!(message.contains(&new), "{message}");
        assert!(
            verifier
                .rejection()
                .is_some_and(|rejection| rejection.contains("vowp agent trust")),
            "the refusal names the owner confirmation command"
        );
    }

    #[test]
    fn http_policy_is_404_426_and_400() {
        let get = |path: &str, headers: &str| {
            HttpHead::parse(format!("GET {path} HTTP/1.1\r\nHost: x\r\n{headers}\r\n").as_bytes()).expect("head")
        };
        assert_eq!(
            get(
                "/health",
                "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: vowp-agent/1\r\n"
            )
            .policy(WSS_PATH),
            Err((StatusCode::NOT_FOUND, "the route is /agent/v1 only"))
        );
        assert_eq!(
            get("/agent/v1", "").policy(WSS_PATH),
            Err((StatusCode::UPGRADE_REQUIRED, "a websocket upgrade is required"))
        );
        assert_eq!(
            get(
                "/agent/v1",
                "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
            )
            .policy(WSS_PATH),
            Err((StatusCode::BAD_REQUEST, "the vowp-agent/1 subprotocol is required"))
        );
        assert!(
            get(
                "/agent/v1",
                "Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: vowp-agent/1\r\n"
            )
            .policy(WSS_PATH)
            .is_ok()
        );
    }
}
