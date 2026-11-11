//! The one-time secret paste page.
//!
//! A request binds `127.0.0.1:0`, prints a URL carrying a random 32-byte token and accepts
//! exactly one POST. The page is reachable through an SSH tunnel; there is no TLS because the
//! listener is loopback-only. It expires after [`REQUEST_TTL`].
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9.2 and §9.4.

use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use vowp_protocol::{SecretInfo, SecretName, SecretRequest, Timestamp};

use crate::CoreError;
use crate::deps::Deps;

use super::write_secret;

/// How long a paste page stays open.
pub const REQUEST_TTL: Duration = Duration::from_secs(600);

/// Largest request body the page accepts.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Largest request header block the page accepts.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// How long one accepted connection may take to send its request.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);

/// The number of connections without a complete request that ends the page.
///
/// A loopback preconnect or port scan must not end the one-time request, but the loop still
/// needs a bound so a client cannot keep the page busy forever.
const MAX_MALFORMED_CONNECTIONS: u32 = 16;

/// A running one-time paste page.
///
/// [`SecretRequestServer::start`] returns only the request document and serves in the
/// background; use [`SecretRequestServer::bind`] when the caller must wait for the value.
pub struct SecretRequestServer {
    deps: Deps,
    name: SecretName,
    request: SecretRequest,
    listener: TcpListener,
    token: String,
}

impl SecretRequestServer {
    /// Binds `127.0.0.1:0`, serves exactly one POST in a background thread and exits.
    ///
    /// This is the spec's document-returning entry point used by `vowp mcp`; the CLI uses
    /// [`SecretRequestServer::bind`] so it can wait for the result in the same process.
    pub fn start(deps: &Deps, name: &SecretName, now: Timestamp) -> Result<SecretRequest, CoreError> {
        let server = Self::bind(deps, name, now)?;
        let request = server.request.clone();
        thread::spawn(move || {
            let _ = server.wait(REQUEST_TTL);
        });
        Ok(request)
    }

    /// Binds `127.0.0.1:0` and returns the running server with a random token.
    pub fn bind(deps: &Deps, name: &SecretName, now: Timestamp) -> Result<SecretRequestServer, CoreError> {
        let token = random_token()?;
        Self::bind_with_token(deps, name, now, token)
    }

    /// Binds `127.0.0.1:0` with an explicit token, for tests.
    pub fn bind_with_token(
        deps: &Deps,
        name: &SecretName,
        now: Timestamp,
        token: String,
    ) -> Result<SecretRequestServer, CoreError> {
        let name = SecretName::parse(name.as_str())?;
        if token.is_empty()
            || !token
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            return Err(CoreError::Invalid(
                "secret request token must be non-empty and URL-safe".to_owned(),
            ));
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|source| CoreError::Io {
            path: "127.0.0.1:0".to_owned(),
            source,
        })?;
        listener.set_nonblocking(true).map_err(|source| CoreError::Io {
            path: "127.0.0.1".to_owned(),
            source,
        })?;
        let address = listener.local_addr().map_err(|source| CoreError::Io {
            path: "127.0.0.1".to_owned(),
            source,
        })?;
        let request = SecretRequest {
            name: name.clone(),
            url: format!("http://{address}/s/{token}"),
            expires_at: now.plus(REQUEST_TTL),
            single_use: true,
        };
        Ok(SecretRequestServer {
            deps: deps.clone(),
            name,
            request,
            listener,
            token,
        })
    }

    /// Returns the request document (URL, expiry and single-use flag).
    pub fn request(&self) -> &SecretRequest {
        &self.request
    }

    /// Returns the bound loopback address.
    pub fn local_addr(&self) -> Result<SocketAddr, CoreError> {
        self.listener.local_addr().map_err(|source| CoreError::Io {
            path: "127.0.0.1".to_owned(),
            source,
        })
    }

    /// Waits for the owner's value and returns the stored metadata.
    ///
    /// The call ends after one successful POST, after `timeout`, or at the request's fixed
    /// expiry, whichever comes first. Malformed requests receive a page and do not consume
    /// the single use. A connection that sends no complete request (a preconnect or a port
    /// scan) is ignored too, up to `MAX_MALFORMED_CONNECTIONS` connections.
    pub fn wait(self, timeout: Duration) -> Result<SecretInfo, CoreError> {
        let deadline = Instant::now() + timeout;
        let mut malformed = 0_u32;
        loop {
            if self.deps.clock.now().unix_secs() >= self.request.expires_at.unix_secs() {
                return Err(CoreError::Command(
                    "secret request expired before a value was pasted".to_owned(),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CoreError::Command(
                    "secret request timed out before a value was pasted".to_owned(),
                ));
            }
            match self.listener.accept() {
                Ok((stream, _)) => match self.serve(stream) {
                    Ok(Served::Saved(info)) => return Ok(info),
                    Ok(Served::Handled) => {}
                    Ok(Served::Malformed) => {
                        malformed += 1;
                        if malformed >= MAX_MALFORMED_CONNECTIONS {
                            return Err(CoreError::Command(format!(
                                "secret request gave up after {malformed} connections without a request"
                            )));
                        }
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(remaining.min(Duration::from_millis(20)));
                }
                Err(source) => {
                    return Err(CoreError::Io {
                        path: "127.0.0.1".to_owned(),
                        source,
                    });
                }
            }
        }
    }

    /// Serves one connection; a successful POST returns the stored metadata.
    ///
    /// An unreadable request is answered when possible and reported as
    /// [`Served::Malformed`], so the one-time page keeps waiting for the owner.
    fn serve(&self, mut stream: TcpStream) -> Result<Served, CoreError> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err(_) => {
                let _ = respond(
                    &mut stream,
                    400,
                    "Bad Request",
                    "text/plain; charset=utf-8",
                    "bad request",
                );
                return Ok(Served::Malformed);
            }
        };
        let path = request.target.split('?').next().unwrap_or(&request.target);
        let expected = format!("/s/{}", self.token);
        if path != expected {
            let _ = respond(&mut stream, 404, "Not Found", "text/plain; charset=utf-8", "not found");
            return Ok(Served::Handled);
        }
        if request.method == "GET" {
            let page = form_page(&self.name);
            let _ = respond(&mut stream, 200, "OK", "text/html; charset=utf-8", &page);
            return Ok(Served::Handled);
        }
        if request.method != "POST" {
            let _ = respond(
                &mut stream,
                405,
                "Method Not Allowed",
                "text/plain; charset=utf-8",
                "method not allowed",
            );
            return Ok(Served::Handled);
        }
        let Some(value) = posted_value(&request) else {
            let _ = respond(
                &mut stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                "a non-empty value is required",
            );
            return Ok(Served::Handled);
        };
        let info = match write_secret(&self.deps, &self.name, &value) {
            Ok(info) => info,
            Err(error) => {
                let _ = respond(
                    &mut stream,
                    500,
                    "Internal Server Error",
                    "text/plain; charset=utf-8",
                    "could not save the secret",
                );
                return Err(error);
            }
        };
        let page = saved_page(&info);
        let _ = respond(&mut stream, 200, "OK", "text/html; charset=utf-8", &page);
        Ok(Served::Saved(info))
    }
}

/// What one accepted connection did.
enum Served {
    /// A value was stored.
    Saved(SecretInfo),
    /// The request was answered but did not store a value.
    Handled,
    /// The connection sent no complete request; the page keeps waiting.
    Malformed,
}

/// One parsed HTTP request.
struct HttpRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    /// Returns one header value, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Reads one HTTP request with bounded headers and body.
fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, CoreError> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(CoreError::Command("request headers are too large".to_owned()));
        }
        let read = stream.read(&mut chunk).map_err(|source| CoreError::Io {
            path: "secret request".to_owned(),
            source,
        })?;
        if read == 0 {
            return Err(CoreError::Command("request ended before its headers".to_owned()));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| CoreError::Command("empty request line".to_owned()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| CoreError::Command("missing request method".to_owned()))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| CoreError::Command("missing request target".to_owned()))?
        .to_owned();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let request = HttpRequest {
        method,
        target,
        headers,
        body: buffer[header_end..].to_vec(),
    };
    let length = request
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(CoreError::Command("request body is too large".to_owned()));
    }
    let mut body = request.body;
    while body.len() < length {
        let read = stream.read(&mut chunk).map_err(|source| CoreError::Io {
            path: "secret request".to_owned(),
            source,
        })?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(CoreError::Command("request body is too large".to_owned()));
    }
    Ok(HttpRequest { body, ..request })
}

/// Extracts the posted value from a form, JSON object or raw body.
fn posted_value(request: &HttpRequest) -> Option<String> {
    let content_type = request.header("content-type").unwrap_or_default();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let text = std::str::from_utf8(&request.body).ok()?;
        for pair in text.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key == "value" {
                let decoded = percent_decode(value)?;
                return (!decoded.is_empty()).then_some(decoded);
            }
        }
        return None;
    }
    if content_type.starts_with("application/json") {
        let value: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
        let text = value.get("value")?.as_str()?;
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let text = String::from_utf8(request.body.clone()).ok()?;
    (!text.is_empty()).then_some(text)
}

/// Decodes one `application/x-www-form-urlencoded` component.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1])?;
                let low = hex_value(bytes[index + 2])?;
                out.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Returns one hexadecimal digit value.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Writes one complete HTTP response and closes the connection.
fn respond(stream: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// Renders the one-field paste form.
fn form_page(name: &SecretName) -> String {
    let name = html_escape(name.as_str());
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>VOWP secret</title></head><body>\
         <h1>Paste {name}</h1>\
         <p>The value goes straight to the VOWP vault. This link works once and expires in 10 minutes.</p>\
         <form method=\"post\">\
         <input type=\"password\" name=\"value\" autofocus autocomplete=\"off\" spellcheck=\"false\">\
         <button type=\"submit\">Save</button>\
         </form></body></html>"
    )
}

/// Renders the page shown after a successful save.
fn saved_page(info: &SecretInfo) -> String {
    let name = html_escape(info.name.as_str());
    let last4 = html_escape(&info.last4);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>VOWP secret saved</title></head><body>\
         <h1>{name} saved, ends in …{last4}</h1>\
         <p>You can close this page.</p></body></html>"
    )
}

/// Escapes the five HTML special characters.
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Returns the first position of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// Generates a 32-byte token from `/dev/urandom`, encoded as hex.
fn random_token() -> Result<String, CoreError> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|source| CoreError::Io {
            path: "/dev/urandom".to_owned(),
            source,
        })?;
    let mut token = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

/// Issues one one-time request document and serves it in the background.
pub fn request_secret(deps: &Deps, name: &SecretName, now: Timestamp) -> Result<SecretRequest, CoreError> {
    SecretRequestServer::start(deps, name, now)
}
