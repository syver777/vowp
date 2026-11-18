//! The local chat spool: append-only events, fsynced before they are sent.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §8.4 and §9.2. A spool lives at
//! `~/.vowp-agent/spool/<session>/<run>/`; `events.jsonl` holds the exact frames the agent
//! sent, and `acked.json` records the event ids the home acknowledged. The default cap is
//! 2 GiB. A full spool stops the run; this module never deletes an unacknowledged event.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vowp_protocol::{AgentFrame, RemoteLine, RunId, SessionId};

use crate::config::ensure_private_dir;
use crate::error::AgentError;

/// Directory below the agent state directory that holds every spool.
pub const SPOOL_DIR: &str = "spool";
/// Append-only event frames.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Acknowledgement records.
pub const ACKED_FILE: &str = "acked.json";
/// Default spool cap: 2 GiB.
pub const DEFAULT_SPOOL_CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The acknowledgement file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AckedFile {
    /// Event id to home sequence; the reserved key `summary` marks the terminal frame.
    #[serde(default)]
    acked: BTreeMap<String, u64>,
}

/// The result of one spool append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoolAppend {
    /// The frame was written and fsynced.
    Written,
    /// The frame would exceed the cap; nothing was written.
    Full {
        /// Size the refused write would have reached.
        bytes: u64,
        /// Configured cap.
        cap: u64,
    },
}

/// One run's local spool.
pub struct Spool {
    dir: PathBuf,
    cap: u64,
    file: File,
    bytes: u64,
    acked: AckedFile,
}

impl Spool {
    /// Opens (or creates) the spool for one run with the default cap.
    pub fn open(state_dir: &Path, session: &SessionId, run: &RunId) -> Result<Self, AgentError> {
        Self::with_cap(state_dir, session, run, DEFAULT_SPOOL_CAP_BYTES)
    }

    /// Opens (or creates) the spool for one run with an explicit cap.
    pub fn with_cap(state_dir: &Path, session: &SessionId, run: &RunId, cap: u64) -> Result<Self, AgentError> {
        let dir = spool_dir(state_dir, session, run);
        ensure_private_dir(&dir)?;
        let path = dir.join(EVENTS_FILE);
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let bytes = file.metadata()?.len();
        let acked = match fs::read_to_string(dir.join(ACKED_FILE)) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AckedFile::default(),
            Err(error) => return Err(AgentError::Io(error)),
        };
        Ok(Self {
            dir,
            cap,
            file,
            bytes,
            acked,
        })
    }

    /// Returns the spool directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the number of bytes written to the spool.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Appends one frame and fsyncs it before it can be sent.
    ///
    /// A frame that would grow the spool past the cap is reported as
    /// [`SpoolAppend::Full`] and nothing is written.
    pub fn append(&mut self, frame: &AgentFrame) -> Result<SpoolAppend, AgentError> {
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        let size = u64::try_from(line.len()).unwrap_or(u64::MAX);
        let next = self.bytes.saturating_add(size);
        if next > self.cap {
            return Ok(SpoolAppend::Full {
                bytes: next,
                cap: self.cap,
            });
        }
        self.file.write_all(&line)?;
        self.file.sync_all()?;
        self.bytes = next;
        Ok(SpoolAppend::Written)
    }

    /// Records one event id and its home sequence in `acked.json`.
    pub fn mark_acked(&mut self, event_id: &str, seq: u64) -> Result<(), AgentError> {
        self.acked.acked.insert(event_id.to_owned(), seq);
        self.write_acked()
    }

    /// Records that the home accepted the terminal summary.
    pub fn mark_summary_acked(&mut self) -> Result<(), AgentError> {
        self.acked.acked.insert("summary".to_owned(), 0);
        self.write_acked()
    }

    /// Returns whether the home accepted the terminal summary.
    pub fn summary_acked(&self) -> bool {
        self.acked.acked.contains_key("summary")
    }

    /// Returns the unacknowledged frames in their original order.
    ///
    /// `Hello` and heartbeat lines are never written to the spool; a reconnect sends a fresh
    /// `Hello` before replaying.
    pub fn unacked(&self) -> Result<Vec<AgentFrame>, AgentError> {
        let mut text = String::new();
        File::open(self.dir.join(EVENTS_FILE))?.read_to_string(&mut text)?;
        let mut frames = Vec::new();
        for (number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let frame: AgentFrame = serde_json::from_str(line)
                .map_err(|error| AgentError::Protocol(format!("spool line {} is invalid: {error}", number + 1)))?;
            if !self.is_acked(&frame) {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    /// Returns the number of unacknowledged event frames (the terminal summary is not an event).
    pub fn unacked_events(&self) -> Result<usize, AgentError> {
        Ok(self
            .unacked()?
            .iter()
            .filter(|frame| {
                matches!(
                    frame,
                    AgentFrame::RemoteLine {
                        line: RemoteLine::Event { .. },
                        ..
                    }
                )
            })
            .count())
    }

    fn is_acked(&self, frame: &AgentFrame) -> bool {
        match frame {
            AgentFrame::RemoteLine {
                line: RemoteLine::Event { event },
                ..
            } => self.acked.acked.contains_key(event.id.as_str()),
            AgentFrame::TurnEnd { .. } => self.acked.acked.contains_key("summary"),
            _ => false,
        }
    }

    fn write_acked(&self) -> Result<(), AgentError> {
        let path = self.dir.join(ACKED_FILE);
        let temp = self.dir.join(".acked.json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.acked)?;
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// Returns `state_dir/spool/<session>/<run>`.
pub fn spool_dir(state_dir: &Path, session: &SessionId, run: &RunId) -> PathBuf {
    state_dir.join(SPOOL_DIR).join(session.as_str()).join(run.as_str())
}

/// Returns `state_dir/spool/<session>`.
pub fn session_spool_dir(state_dir: &Path, session: &SessionId) -> PathBuf {
    state_dir.join(SPOOL_DIR).join(session.as_str())
}
