//! The transport boundary: a duplex stream of JSON-line [`AgentFrame`]s.
//!
//! The real transport runs the documented SSH command. Tests implement [`Transport`] and
//! [`FrameStream`] in memory, so no test opens SSH, a socket or the network.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vowp_protocol::AgentFrame;

use crate::config::TransportConfig;
use crate::error::AgentError;

/// Largest accepted frame, including its trailing newline.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// One receive result.
///
/// A frame is up to 1 MiB by protocol, so the enum is deliberately sized for the frame case;
/// callers match it immediately and never keep a queue of reads.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum FrameRead {
    /// A complete frame arrived.
    Frame(AgentFrame),
    /// No frame arrived before the timeout.
    Timeout,
    /// The peer closed the stream.
    Closed,
}

/// One bidirectional line stream carrying [`AgentFrame`]s.
pub trait FrameStream: Send {
    /// Receives one frame, waiting at most `timeout`; `None` waits indefinitely.
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError>;
    /// Sends one frame.
    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError>;
}

/// Opens a fresh duplex stream to the home.
pub trait Transport: Send {
    /// Opens one new stream. Each call is one SSH process or one fake connection.
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError>;

    /// Returns the certificate fingerprint observed by the last [`Transport::open`], when the
    /// transport records one. The SSH and in-memory transports return `None`.
    fn recorded_fingerprint(&self) -> Option<String> {
        None
    }
}

/// Builds the transport named by an `agent.json` transport section.
///
/// An SSH transport runs the documented forced command; a WSS transport connects through the
/// owner's reverse proxy with the stored TLS policy. There is no fallback between the two.
pub fn transport_for_config(config: &TransportConfig) -> Result<Box<dyn Transport>, AgentError> {
    match config {
        TransportConfig::Ssh { target } => Ok(Box::new(SshTransport::new(target))),
        TransportConfig::Wss { .. } => Ok(Box::new(crate::wss::WssTransport::from_transport_config(config)?)),
    }
}

/// The real Phase 0 SSH stdio transport.
#[derive(Debug, Clone)]
pub struct SshTransport {
    target: String,
    binary: String,
    max_frame_bytes: usize,
}

impl SshTransport {
    /// Builds a transport for `target` using the `ssh` on `PATH`.
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            binary: "ssh".to_owned(),
            max_frame_bytes: MAX_FRAME_BYTES,
        }
    }

    /// Builds a transport with an explicit ssh binary (tests and unusual installs).
    pub fn with_binary(target: impl Into<String>, binary: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            binary: binary.into(),
            max_frame_bytes: MAX_FRAME_BYTES,
        }
    }

    /// Returns the ssh argv for one connection, without running it.
    pub fn argv(&self) -> Vec<String> {
        vec![
            "-T".to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "ServerAliveInterval=15".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            self.target.clone(),
            "vowp agent gateway --stdio".to_owned(),
        ]
    }
}

impl Transport for SshTransport {
    fn open(&mut self) -> Result<Box<dyn FrameStream>, AgentError> {
        if self.target.trim().is_empty() {
            return Err(AgentError::Config("ssh target must not be empty".to_owned()));
        }
        let mut child = Command::new(&self.binary)
            .args(self.argv())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| AgentError::Transport(format!("{}: {error}", self.binary)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Transport("ssh stdin was not piped".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Transport("ssh stdout was not piped".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AgentError::Transport("ssh stderr was not piped".to_owned()))?;
        let stderr = spawn_stderr_reader(stderr);
        let (sender, receiver) = mpsc::channel();
        let max = self.max_frame_bytes;
        let reader = std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                match read_line_limited(&mut stdout, max) {
                    Ok(None) => {
                        let _ = sender.send(Ok(None));
                        return;
                    }
                    Ok(Some(line)) => {
                        let parsed = serde_json::from_slice::<AgentFrame>(&line)
                            .map(Some)
                            .map_err(AgentError::Json);
                        let failed = parsed.is_err();
                        if sender.send(parsed).is_err() || failed {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(AgentError::Io(error)));
                        return;
                    }
                }
            }
        });
        Ok(Box::new(SshFrameStream {
            child,
            stdin,
            receiver,
            _reader: reader,
            stderr,
            max_frame_bytes: max,
            closed: false,
        }))
    }
}

struct SshFrameStream {
    child: Child,
    stdin: ChildStdin,
    receiver: Receiver<Result<Option<AgentFrame>, AgentError>>,
    _reader: std::thread::JoinHandle<()>,
    stderr: Arc<Mutex<Vec<u8>>>,
    max_frame_bytes: usize,
    closed: bool,
}

impl SshFrameStream {
    fn stderr_tail(&self) -> String {
        let bytes = self.stderr.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    fn closed_error(&mut self, action: &str, error: std::io::Error) -> AgentError {
        let tail = self.stderr_tail();
        self.closed = true;
        if tail.is_empty() {
            AgentError::Transport(format!("{action}: {error}"))
        } else {
            AgentError::Transport(format!("{action}: {error}: {tail}"))
        }
    }
}

impl FrameStream for SshFrameStream {
    fn recv(&mut self, timeout: Option<Duration>) -> Result<FrameRead, AgentError> {
        if self.closed {
            return Ok(FrameRead::Closed);
        }
        let received = match timeout {
            Some(timeout) => match self.receiver.recv_timeout(timeout) {
                Ok(received) => received,
                Err(RecvTimeoutError::Timeout) => return Ok(FrameRead::Timeout),
                Err(RecvTimeoutError::Disconnected) => {
                    self.closed = true;
                    return Ok(FrameRead::Closed);
                }
            },
            None => match self.receiver.recv() {
                Ok(received) => received,
                Err(_) => {
                    self.closed = true;
                    return Ok(FrameRead::Closed);
                }
            },
        };
        match received {
            Ok(Some(frame)) => Ok(FrameRead::Frame(frame)),
            Ok(None) => {
                self.closed = true;
                Ok(FrameRead::Closed)
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }

    fn send(&mut self, frame: &AgentFrame) -> Result<(), AgentError> {
        if self.closed {
            return Err(AgentError::Transport("stream is closed".to_owned()));
        }
        let mut bytes = serde_json::to_vec(frame)?;
        bytes.push(b'\n');
        if bytes.len() > self.max_frame_bytes {
            return Err(AgentError::FrameTooLarge {
                size: bytes.len(),
                max: self.max_frame_bytes,
            });
        }
        if let Err(error) = self.stdin.write_all(&bytes).and_then(|()| self.stdin.flush()) {
            return Err(self.closed_error("write frame", error));
        }
        Ok(())
    }
}

impl Drop for SshFrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_stderr_reader(stderr: ChildStderr) -> Arc<Mutex<Vec<u8>>> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return,
                Ok(count) => {
                    let mut tail = sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    tail.extend_from_slice(&chunk[..count]);
                    if tail.len() > 4096 {
                        let excess = tail.len() - 4096;
                        tail.drain(..excess);
                    }
                }
                Err(_) => return,
            }
        }
    });
    buffer
}

/// Reads one newline-terminated line, refusing to buffer more than `max` bytes.
fn read_line_limited(reader: &mut impl BufRead, max: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        if let Some(position) = chunk.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&chunk[..position]);
            reader.consume(position + 1);
            if line.len() > max {
                return Err(too_large(max));
            }
            return Ok(Some(line));
        }
        line.extend_from_slice(chunk);
        let consumed = chunk.len();
        reader.consume(consumed);
        if line.len() > max {
            return Err(too_large(max));
        }
    }
}

fn too_large(max: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("frame exceeds the {max}-byte maximum"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_argv_matches_the_documented_command() {
        let transport = SshTransport::new("home.example");
        assert_eq!(
            transport.argv(),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "home.example",
                "vowp agent gateway --stdio",
            ]
        );
    }

    #[test]
    fn read_line_limited_rejects_an_oversized_frame() {
        let mut input = BufReader::new(&b"0123456789\n"[..]);
        let error = read_line_limited(&mut input, 4).expect_err("oversized line");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
