//! Durable outbox for system-initiated frames sent to one node's live agent.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §4.3 and
//! `docs/design/v0.4/hardening.md` §4.2. A home command that changes a surface (today
//! `vowp surface rotate`) cannot write to the agent connection itself. It queues one frame
//! envelope below the home root instead:
//!
//! ```text
//! agent-frames/<node>/<request>.json    {"queued_at": …, "overlap_until": …, "frame": {…}}
//! ```
//!
//! The gateway drains the directory of its authenticated node, sends each frame and
//! removes it after a successful write. A frame queued while the node is offline stays on
//! disk for the next authenticated connection. A rotation envelope records when the frame
//! was queued and when the old tokens stop working; the frame itself carries the
//! replacement token's expiry, which is the delivery deadline. A bare frame written by an
//! earlier build reads as an envelope with both times absent.
//!
//! Files are 0600 in a 0700 directory and are written through a private temporary file and
//! a rename, so a reader never sees a partial frame. A `SurfaceIssued` frame contains a
//! plaintext token: the file is a secret and the caller must never log or print it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_protocol::{AgentFrame, NodeName, Timestamp};

use crate::CoreError;
use crate::surface::native::write_private_json;

/// Home directory holding one frame per node and request.
const FRAMES_DIR: &str = "agent-frames";

/// How often [`AgentFrameSpool::wait`] re-checks the outbox.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// One frame waiting for a node's next authenticated connection.
///
/// `queued_at` and `overlap_until` are set for a rotation. A bare frame written by
/// `v4-local-4a-surface-rotate` reads with both fields absent.
#[derive(Clone)]
pub struct QueuedAgentFrame {
    /// Stable request id; also the file stem.
    pub request_id: String,
    /// When the producing command queued the frame.
    pub queued_at: Option<Timestamp>,
    /// When the old surface tokens stop working.
    pub overlap_until: Option<Timestamp>,
    /// Frame to send on the authenticated connection.
    pub frame: AgentFrame,
}

impl QueuedAgentFrame {
    /// Returns the instant after which this frame is useless even if delivered.
    ///
    /// A rotation's replacement token expires at the frame's `expires_at`, so a frame
    /// delivered later would install a dead token. Other frames have no delivery deadline.
    pub fn delivery_deadline(&self) -> Option<Timestamp> {
        match &self.frame {
            AgentFrame::SurfaceIssued { expires_at, .. } => Some(*expires_at),
            _ => None,
        }
    }
}

/// The on-disk envelope around one unchanged [`AgentFrame`].
#[derive(Serialize, Deserialize)]
struct FrameEnvelope {
    /// When the producing command queued the frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queued_at: Option<Timestamp>,
    /// When the old surface tokens stop working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overlap_until: Option<Timestamp>,
    /// The frame itself; its schema is unchanged.
    frame: AgentFrame,
}

/// Accepts the current envelope and the bare frame an earlier build wrote.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredFrame {
    /// The current `{"queued_at": …, "overlap_until": …, "frame": …}` form.
    Envelope(FrameEnvelope),
    /// A `4a` bare `AgentFrame`.
    Bare(AgentFrame),
}

/// The per-node durable outbox of system-initiated frames.
///
/// Layout below the home root:
///
/// ```text
/// agent-frames/<node>/<request>.json
/// ```
///
/// The gateway for a node reads only that node's directory, so a frame queued for another
/// node is never sent on this connection.
#[derive(Debug, Clone)]
pub struct AgentFrameSpool {
    root: PathBuf,
}

impl AgentFrameSpool {
    /// Builds a spool below the home `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the per-node frame directory.
    fn dir(&self, node: &NodeName) -> PathBuf {
        self.root.join(FRAMES_DIR).join(node.as_str())
    }

    /// Queues one frame and atomically replaces an earlier frame with the same request id.
    ///
    /// A request id is a plain file-name component: one to 128 ASCII alphanumerics, `-` or
    /// `_`. `rotate` uses the surface id, so a second rotation before delivery replaces the
    /// older token instead of leaving several plaintext tokens on disk. This form is for a
    /// frame with no rotation times; see [`Self::queue_rotation`].
    pub fn queue(&self, node: &NodeName, request_id: &str, frame: &AgentFrame) -> Result<(), CoreError> {
        self.write(node, request_id, frame, None, None)
    }

    /// Queues one rotation frame with its queue time and overlap deadline.
    ///
    /// The replacement token's `expires_at` inside `frame` is the delivery deadline.
    pub fn queue_rotation(
        &self,
        node: &NodeName,
        request_id: &str,
        frame: &AgentFrame,
        queued_at: Timestamp,
        overlap_until: Timestamp,
    ) -> Result<(), CoreError> {
        self.write(node, request_id, frame, Some(queued_at), Some(overlap_until))
    }

    /// Writes one envelope through a private temporary file and a rename.
    fn write(
        &self,
        node: &NodeName,
        request_id: &str,
        frame: &AgentFrame,
        queued_at: Option<Timestamp>,
        overlap_until: Option<Timestamp>,
    ) -> Result<(), CoreError> {
        validate_request_id(request_id)?;
        let dir = self.dir(node);
        create_private_dir(&self.root.join(FRAMES_DIR))?;
        create_private_dir(&dir)?;
        let path = dir.join(format!("{request_id}.json"));
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        let envelope = FrameEnvelope {
            queued_at,
            overlap_until,
            frame: frame.clone(),
        };
        write_private_json(&temp, &envelope)?;
        fs::rename(&temp, &path).map_err(|source| {
            let _ = fs::remove_file(&temp);
            CoreError::Io {
                path: path.display().to_string(),
                source,
            }
        })
    }

    /// Returns one queued frame, or `None` when it was drained or never existed.
    pub fn read(&self, node: &NodeName, request_id: &str) -> Result<Option<QueuedAgentFrame>, CoreError> {
        validate_request_id(request_id)?;
        let path = self.dir(node).join(format!("{request_id}.json"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CoreError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let envelope = parse_envelope(&path, &bytes)?;
        Ok(Some(QueuedAgentFrame {
            request_id: request_id.to_owned(),
            queued_at: envelope.queued_at,
            overlap_until: envelope.overlap_until,
            frame: envelope.frame,
        }))
    }

    /// Returns the frames waiting for `node`, oldest request id first.
    ///
    /// A missing directory is an empty queue. A malformed frame is an error: the gateway
    /// closes the connection rather than silently dropping a system-initiated frame.
    pub fn pending(&self, node: &NodeName) -> Result<Vec<QueuedAgentFrame>, CoreError> {
        let dir = self.dir(node);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: dir.display().to_string(),
                    source,
                });
            }
        };
        let mut frames = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: dir.display().to_string(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(request_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            // A frame removed between `read_dir` and `read` is already drained.
            if let Some(queued) = self.read(node, request_id)? {
                frames.push(queued);
            }
        }
        frames.sort_by(|left, right| left.request_id.cmp(&right.request_id));
        Ok(frames)
    }

    /// Removes one delivered frame. A frame that is already gone is not an error.
    pub fn complete(&self, node: &NodeName, request_id: &str) -> Result<(), CoreError> {
        validate_request_id(request_id)?;
        let path = self.dir(node).join(format!("{request_id}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CoreError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Removes one sent frame; kept for the gateway's drain call site.
    pub fn remove(&self, node: &NodeName, request_id: &str) -> Result<(), CoreError> {
        self.complete(node, request_id)
    }

    /// Waits until the frame is drained, its delivery deadline passes, or `timeout` expires.
    ///
    /// Returns `true` only when the frame is gone (the gateway wrote it). A replacement
    /// token that expires before delivery can never be applied, so waiting stops there even
    /// when the caller's timeout is longer. Returns `false` on either unsuccessful outcome.
    pub fn wait(&self, node: &NodeName, request_id: &str, timeout: Duration) -> Result<bool, CoreError> {
        validate_request_id(request_id)?;
        let started = Instant::now();
        loop {
            match self.read(node, request_id)? {
                None => return Ok(true),
                Some(queued) => {
                    if queued
                        .delivery_deadline()
                        .is_some_and(|deadline| deadline <= Timestamp::now())
                    {
                        return Ok(false);
                    }
                }
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Ok(false);
            }
            thread::sleep(WAIT_POLL.min(timeout - elapsed));
        }
    }
}

/// Parses the current envelope or a bare `4a` frame.
fn parse_envelope(path: &Path, bytes: &[u8]) -> Result<FrameEnvelope, CoreError> {
    let stored: StoredFrame =
        serde_json::from_slice(bytes).map_err(|error| CoreError::Invalid(format!("{}: {error}", path.display())))?;
    Ok(match stored {
        StoredFrame::Envelope(envelope) => envelope,
        StoredFrame::Bare(frame) => FrameEnvelope {
            queued_at: None,
            overlap_until: None,
            frame,
        },
    })
}

/// Returns whether a request id is a plain file-name component.
///
/// The same shape as the orchestrator command spool: no separator, no `..`, no empty stem.
fn validate_request_id(request_id: &str) -> Result<(), CoreError> {
    let safe = !request_id.is_empty()
        && request_id.len() <= 128
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if safe {
        Ok(())
    } else {
        Err(CoreError::Invalid(format!(
            "invalid agent frame request id {request_id:?}"
        )))
    }
}

/// Creates one directory below the home root with mode 0700.
fn create_private_dir(dir: &Path) -> Result<(), CoreError> {
    fs::create_dir_all(dir).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| CoreError::Io {
        path: dir.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vowp_protocol::{SurfaceId, SurfaceScope};

    const NODE: &str = "mac-studio";
    const REQUEST: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn node() -> NodeName {
        NodeName::parse(NODE).expect("node")
    }

    #[test]
    fn queued_frame_reads_a_bare_4a_frame_as_an_envelope_without_times() {
        let root = tempfile::tempdir().expect("temp home");
        let spool = AgentFrameSpool::new(root.path());
        let dir = root.path().join(FRAMES_DIR).join(NODE);
        std::fs::create_dir_all(&dir).expect("frame dir");
        let frame = AgentFrame::SurfaceIssued {
            surface_id: SurfaceId(REQUEST.to_owned()),
            token: "vowp_surf_bare-frame-fixture".to_owned(),
            scopes: vec![SurfaceScope::new("fleet.read")],
            expires_at: Timestamp::parse("2026-12-14T10:00:00Z").expect("timestamp"),
        };
        std::fs::write(
            dir.join(format!("{REQUEST}.json")),
            serde_json::to_vec(&frame).expect("frame JSON"),
        )
        .expect("bare frame");

        let queued = spool.read(&node(), REQUEST).expect("read").expect("queued frame");
        assert!(queued.queued_at.is_none());
        assert!(queued.overlap_until.is_none());
        assert_eq!(queued.frame, frame);
        assert_eq!(
            queued.delivery_deadline(),
            Some(Timestamp::parse("2026-12-14T10:00:00Z").expect("timestamp"))
        );
    }
}
