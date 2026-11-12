//! The append-only session log: `sessions/<id>/log.jsonl`.
//! Spec: `docs/design/crates/vowp-store.md` §3.6 and §4.1 rules 1–6, and
//! `docs/design/sessions.md` §1.
//!
//! One JSON [`EventEnvelope`] per line. [`SessionLog::append`] assigns the sequence number,
//! id and timestamp and writes exactly one line; [`SessionLog::open`] takes the single-writer
//! lock, validates the existing lines and truncates a torn final line. Readers
//! ([`SessionLogReader`]) never lock and never modify the file.
//!
//! The same module defines the `vowp-session-dir-v1` manifest and the file listing and
//! verification helpers used by a full `session pull --full` directory snapshot.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use vowp_protocol::{
    Actor, BLOB_SPILL_BYTES, Clock, EventBody, EventEnvelope, EventIndexRow, EventSource, EventType, SessionId,
    Timestamp,
};

use crate::blobs::BlobStore;
use crate::error::StoreError;
use crate::paths::NodePaths;

/// A `raw` JSON value larger than this many bytes is dropped from the line.
const RAW_MAX_BYTES: usize = 64 * 1024;

/// What a caller appends: the session layer chooses actor/source/body; the log assigns id,
/// seq, ts and session_id.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Who produced the event.
    pub actor: Actor,
    /// Harness and model that produced the event.
    pub source: EventSource,
    /// Typed payload.
    pub body: EventBody,
    /// Original harness event, kept for display only.
    pub raw: Option<serde_json::Value>,
    /// Harness-reported time; the log's clock is used when absent.
    pub ts: Option<Timestamp>,
}

/// The result of [`SessionLogReader::verify`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    /// Number of lines examined, including a torn tail fragment.
    pub lines: u64,
    /// Highest sequence number that was read successfully.
    pub last_seq: u64,
    /// Human-readable problems, one per bad line, for example
    /// `"line 12: seq 5 expected 4"`.
    pub problems: Vec<String>,
    /// Bytes belonging to a non-terminated or unparsable final line.
    pub torn_tail_bytes: u64,
}

impl VerifyReport {
    /// Returns true when the log has no problems and no torn tail.
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty() && self.torn_tail_bytes == 0
    }
}

/// The last complete event in a session log.
///
/// A divergence report needs the log position and the last event's type and time without
/// reading or indexing the whole session twice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogHead {
    /// Sequence number of the last event; 0 for an empty log.
    pub last_seq: u64,
    /// Type of the last event, when the log has one.
    pub event_type: Option<EventType>,
    /// Timestamp of the last event, when the log has one.
    pub ts: Option<Timestamp>,
}

/// What [`SessionLog::open`] found and repaired; all zero after [`SessionLog::create`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// Bytes truncated from a non-terminated or unparsable final line.
    pub torn_tail_bytes: u64,
    /// Events found in the file before the torn tail, if any.
    pub events_found: u64,
}

/// One event written by the log, with the location needed to index it.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendedEvent {
    /// The stored envelope.
    pub envelope: EventEnvelope,
    /// File offset of the first byte of the line.
    pub byte_offset: u64,
    /// Length of the line in bytes, including the trailing newline.
    pub byte_len: u64,
}

/// Mutable state behind a mutex so [`SessionLog`] methods can take `&self`.
struct LogState {
    file: File,
    next_seq: u64,
    bytes: u64,
    dirty: bool,
    poisoned: bool,
}

/// The single writer of one session log.
///
/// Created with [`SessionLog::create`] (new session) or opened for append with
/// [`SessionLog::open`]. Dropping the handle syncs best-effort and releases the lock file.
pub struct SessionLog {
    path: PathBuf,
    session_id: SessionId,
    clock: Arc<dyn Clock>,
    blobs: BlobStore,
    state: Mutex<LogState>,
    open_report: OpenReport,
    sync_count: AtomicU64,
    _lock: WriterLock,
}

impl SessionLog {
    /// Creates `<session_dir>/{log.jsonl,blobs/,capsules/}` and takes the writer lock.
    ///
    /// Returns [`StoreError::Conflict`] when `log.jsonl` already exists; an existing stale
    /// lock file is replaced.
    pub fn create(paths: &NodePaths, id: &SessionId, clock: Arc<dyn Clock>) -> Result<SessionLog, StoreError> {
        let session_dir = paths.session_dir(id);
        for dir in [session_dir.clone(), paths.blobs_dir(id), paths.capsules_dir(id)] {
            std::fs::create_dir_all(&dir).map_err(|err| StoreError::io(dir.display(), err))?;
        }
        let lock = WriterLock::acquire(&paths.lock_path(id))?;

        let path = paths.log_path(id);
        let file = match OpenOptions::new().create_new(true).append(true).open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StoreError::Conflict(format!(
                    "session log already exists: {}",
                    path.display()
                )));
            }
            Err(err) => return Err(StoreError::io(path.display(), err)),
        };
        sync_dir(&session_dir)?;

        Ok(SessionLog {
            path,
            session_id: id.clone(),
            clock,
            blobs: BlobStore::new(paths.blobs_dir(id)),
            state: Mutex::new(LogState {
                file,
                next_seq: 1,
                bytes: 0,
                dirty: false,
                poisoned: false,
            }),
            open_report: OpenReport::default(),
            sync_count: AtomicU64::new(0),
            _lock: lock,
        })
    }

    /// Opens an existing log for append: takes the lock, validates the file and repairs a
    /// torn final line by truncating it.
    ///
    /// Corruption before the final line is reported as [`StoreError::Corrupt`] and the file
    /// is left untouched.
    pub fn open(paths: &NodePaths, id: &SessionId, clock: Arc<dyn Clock>) -> Result<SessionLog, StoreError> {
        let lock = WriterLock::acquire(&paths.lock_path(id))?;
        let path = paths.log_path(id);
        let scan = scan_for_append(&path, id)?;
        if scan.torn_tail_bytes > 0 {
            let file = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|err| StoreError::io(path.display(), err))?;
            file.set_len(scan.bytes)
                .map_err(|err| StoreError::io(path.display(), err))?;
            file.sync_all().map_err(|err| StoreError::io(path.display(), err))?;
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|err| StoreError::io(path.display(), err))?;

        Ok(SessionLog {
            path: path.clone(),
            session_id: id.clone(),
            clock,
            blobs: BlobStore::new(paths.blobs_dir(id)),
            state: Mutex::new(LogState {
                file,
                next_seq: scan.last_seq + 1,
                bytes: scan.bytes,
                dirty: false,
                poisoned: false,
            }),
            open_report: OpenReport {
                torn_tail_bytes: scan.torn_tail_bytes,
                events_found: scan.events,
            },
            sync_count: AtomicU64::new(0),
            _lock: lock,
        })
    }

    /// Opens an existing log read-only: no lock and no repair.
    ///
    /// Used by `session show`, `tail`, `export` and `reindex`; a torn tail is ignored by the
    /// iterator and reported by [`SessionLogReader::verify`].
    pub fn open_read(paths: &NodePaths, id: &SessionId) -> Result<SessionLogReader, StoreError> {
        let path = paths.log_path(id);
        if !path.is_file() {
            return Err(StoreError::not_found("session log", id.as_str()));
        }
        Ok(SessionLogReader {
            path,
            session_id: id.clone(),
            blobs: BlobStore::new(paths.blobs_dir(id)),
        })
    }

    /// Returns what open found and repaired (zero after create).
    pub fn open_report(&self) -> &OpenReport {
        &self.open_report
    }

    /// Returns the session this log belongs to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the sequence number of the last written event (0 for a new log).
    pub fn last_seq(&self) -> u64 {
        self.lock_state().next_seq - 1
    }

    /// Returns the current log size in bytes.
    pub fn byte_len(&self) -> u64 {
        self.lock_state().bytes
    }

    /// Appends one event, spilling a tool result larger than [`BLOB_SPILL_BYTES`].
    ///
    /// Assigns `seq` and `ts`, writes one line with a single `write_all` and returns the
    /// envelope plus its byte range. The write is flushed to the OS but not fsynced; call
    /// [`SessionLog::sync`] at the fsync points listed in the spec.
    pub fn append(&self, ev: NewEvent) -> Result<AppendedEvent, StoreError> {
        let mut state = self.lock_state();
        if state.poisoned {
            return Err(StoreError::Poisoned);
        }
        if state.next_seq == u64::MAX {
            return Err(StoreError::InvalidInput(
                "session log sequence numbers are exhausted".to_owned(),
            ));
        }

        let mut body = ev.body;
        spill_tool_result(&self.blobs, &mut body)?;

        let ts = ev.ts.unwrap_or_else(|| self.clock.now());
        let mut envelope = EventEnvelope::new(self.session_id.clone(), state.next_seq, ts, ev.actor, ev.source, &body);
        envelope.raw = ev.raw.filter(|raw| serialises_at_most(raw, RAW_MAX_BYTES));

        let line = encode_line(&envelope)?;
        let appended = self.write_line(&mut state, envelope, &line)?;
        Ok(appended)
    }

    /// Appends one event with a caller-supplied id, spilling oversized tool output.
    ///
    /// `ingest` uses this so a remote event keeps the id it was assigned before the spool.
    pub fn append_with_id(&self, ev: NewEvent, id: vowp_protocol::EventId) -> Result<AppendedEvent, StoreError> {
        let mut state = self.lock_state();
        if state.poisoned {
            return Err(StoreError::Poisoned);
        }
        if state.next_seq == u64::MAX {
            return Err(StoreError::InvalidInput(
                "session log sequence numbers are exhausted".to_owned(),
            ));
        }

        let mut body = ev.body;
        spill_tool_result(&self.blobs, &mut body)?;

        let ts = ev.ts.unwrap_or_else(|| self.clock.now());
        let mut envelope = EventEnvelope::new(self.session_id.clone(), state.next_seq, ts, ev.actor, ev.source, &body);
        envelope.id = id;
        envelope.raw = ev.raw.filter(|raw| serialises_at_most(raw, RAW_MAX_BYTES));

        let line = encode_line(&envelope)?;
        self.write_line(&mut state, envelope, &line)
    }

    /// Appends a pre-built envelope verbatim (import and reindex tools).
    ///
    /// Requires `env.seq == last_seq + 1` and `env.session_id == self.session_id`.
    pub fn append_envelope(&self, env: &EventEnvelope) -> Result<AppendedEvent, StoreError> {
        let mut state = self.lock_state();
        if state.poisoned {
            return Err(StoreError::Poisoned);
        }
        if state.next_seq == u64::MAX {
            return Err(StoreError::InvalidInput(
                "session log sequence numbers are exhausted".to_owned(),
            ));
        }
        if env.session_id != self.session_id {
            return Err(StoreError::SessionMismatch {
                expected: self.session_id.to_string(),
                got: env.session_id.to_string(),
            });
        }
        if env.seq != state.next_seq {
            return Err(StoreError::SeqGap {
                session: self.session_id.to_string(),
                expected: state.next_seq,
                got: env.seq,
            });
        }

        let line = encode_line(env)?;
        self.write_line(&mut state, env.clone(), &line)
    }

    /// Fsyncs the log file when it has unsynced appends; a clean log is a no-op.
    pub fn sync(&self) -> Result<(), StoreError> {
        let mut state = self.lock_state();
        if !state.dirty {
            return Ok(());
        }
        state
            .file
            .sync_all()
            .map_err(|err| StoreError::io(self.path.display(), err))?;
        state.dirty = false;
        self.sync_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Returns the session's blob store.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Returns how many times this handle has fsynced the log file.
    ///
    /// Observability for tests: a clean [`SessionLog::sync`] does not change it.
    #[doc(hidden)]
    pub fn sync_count(&self) -> u64 {
        self.sync_count.load(Ordering::Relaxed)
    }

    /// Locks the state mutex, recovering from a panic in another caller.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes one encoded line and advances the counters; poisons the log on a write error.
    fn write_line(
        &self,
        state: &mut LogState,
        envelope: EventEnvelope,
        line: &[u8],
    ) -> Result<AppendedEvent, StoreError> {
        let byte_offset = state.bytes;
        let byte_len = line.len() as u64;
        if let Err(err) = state.file.write_all(line) {
            state.poisoned = true;
            return Err(StoreError::io(self.path.display(), err));
        }
        state.bytes += byte_len;
        state.next_seq += 1;
        state.dirty = true;
        Ok(AppendedEvent {
            envelope,
            byte_offset,
            byte_len,
        })
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

/// A read-only view of one session log.
pub struct SessionLogReader {
    path: PathBuf,
    session_id: SessionId,
    blobs: BlobStore,
}

impl SessionLogReader {
    /// Returns the sequence number of the last complete event, or 0 for an empty log.
    pub fn last_seq(&self) -> Result<u64, StoreError> {
        let mut last = 0;
        for event in self.iter()? {
            last = event?.envelope.seq;
        }
        Ok(last)
    }

    /// Returns the last sequence number and the last event's type and timestamp.
    ///
    /// Reads the log once, like [`Self::last_seq`], but keeps the last complete event so a
    /// caller can report a session head. An empty log yields [`LogHead::default`].
    pub fn head(&self) -> Result<LogHead, StoreError> {
        let mut head = LogHead::default();
        for event in self.iter()? {
            let envelope = event?.envelope;
            head.last_seq = envelope.seq;
            head.event_type = Some(envelope.event_type);
            head.ts = Some(envelope.ts);
        }
        Ok(head)
    }

    /// Reads every complete event in file order.
    pub fn read_all(&self) -> Result<Vec<EventEnvelope>, StoreError> {
        let mut events = Vec::new();
        for event in self.iter()? {
            events.push(event?.envelope);
        }
        Ok(events)
    }

    /// Reads events with `seq > after_seq`, up to `limit` events.
    pub fn read_from(&self, after_seq: u64, limit: Option<usize>) -> Result<Vec<EventEnvelope>, StoreError> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        for event in self.iter()? {
            let event = event?;
            if event.envelope.seq <= after_seq {
                continue;
            }
            events.push(event.envelope);
            if limit.is_some_and(|limit| events.len() >= limit) {
                break;
            }
        }
        Ok(events)
    }

    /// Returns the last `n` events in ascending order.
    pub fn tail(&self, n: usize) -> Result<Vec<EventEnvelope>, StoreError> {
        let mut events = self.read_all()?;
        let start = events.len().saturating_sub(n);
        Ok(events.split_off(start))
    }

    /// Streams events with their byte offsets.
    pub fn iter(&self) -> Result<EventIter, StoreError> {
        let file = File::open(&self.path).map_err(|err| StoreError::io(self.path.display(), err))?;
        Ok(EventIter {
            reader: BufReader::new(file),
            path: self.path.clone(),
            session_id: self.session_id.clone(),
            offset: 0,
            expected_seq: 1,
            line: 0,
            done: false,
        })
    }

    /// Reads one event at the byte range recorded in the index.
    pub fn read_at(&self, byte_offset: u64, byte_len: u64) -> Result<EventEnvelope, StoreError> {
        let mut file = File::open(&self.path).map_err(|err| StoreError::io(self.path.display(), err))?;
        file.seek(SeekFrom::Start(byte_offset))
            .map_err(|err| StoreError::io(self.path.display(), err))?;
        let mut buffer = vec![0_u8; byte_len as usize];
        file.read_exact(&mut buffer)
            .map_err(|err| StoreError::io(self.path.display(), err))?;
        let envelope: EventEnvelope = serde_json::from_slice(&buffer).map_err(|err| StoreError::Corrupt {
            path: self.path.display().to_string(),
            line: None,
            reason: format!("invalid event at offset {byte_offset}: {err}"),
        })?;
        if envelope.session_id != self.session_id {
            return Err(StoreError::Corrupt {
                path: self.path.display().to_string(),
                line: None,
                reason: format!(
                    "event at offset {byte_offset} belongs to session {}, not {}",
                    envelope.session_id, self.session_id
                ),
            });
        }
        Ok(envelope)
    }

    /// Checks every line and reports problems without modifying the file.
    pub fn verify(&self) -> Result<VerifyReport, StoreError> {
        let bytes = std::fs::read(&self.path).map_err(|err| StoreError::io(self.path.display(), err))?;
        let mut report = VerifyReport::default();
        let mut expected = 1;
        let mut offset = 0_usize;
        let mut last = 0_u64;

        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            offset += line.len();
            report.lines += 1;
            let is_last = offset == bytes.len();
            let terminated = line.ends_with(b"\n");
            let body = trim_newline(line);
            match serde_json::from_slice::<EventEnvelope>(body) {
                Ok(envelope) => {
                    if envelope.seq != expected {
                        report.problems.push(format!(
                            "line {}: seq {} expected {expected}",
                            report.lines, envelope.seq
                        ));
                        break;
                    }
                    if envelope.session_id != self.session_id {
                        report.problems.push(format!(
                            "line {}: session {} is not {}",
                            report.lines, envelope.session_id, self.session_id
                        ));
                        break;
                    }
                    last = envelope.seq;
                    expected += 1;
                    if !terminated {
                        report.torn_tail_bytes = (bytes.len() - (offset - line.len())) as u64;
                        report.problems.push(format!("line {}: torn tail", report.lines));
                    }
                }
                Err(err) => {
                    if is_last || !terminated {
                        let start = offset - line.len();
                        report.torn_tail_bytes = (bytes.len() - start) as u64;
                        report
                            .problems
                            .push(format!("line {}: invalid JSON: {err}", report.lines));
                    } else {
                        report
                            .problems
                            .push(format!("line {}: invalid JSON: {err}", report.lines));
                    }
                    break;
                }
            }
        }
        report.last_seq = last;
        Ok(report)
    }

    /// Returns the session's blob store.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }
}

/// Streaming iterator over the complete events of a log.
pub struct EventIter {
    reader: BufReader<File>,
    path: PathBuf,
    session_id: SessionId,
    offset: u64,
    expected_seq: u64,
    line: u64,
    done: bool,
}

impl Iterator for EventIter {
    type Item = Result<IndexedEvent, StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let start = self.offset;
        let mut buffer = Vec::new();
        let read = match self.reader.read_until(b'\n', &mut buffer) {
            Ok(read) => read,
            Err(err) => {
                self.done = true;
                return Some(Err(StoreError::io(self.path.display(), err)));
            }
        };
        if read == 0 {
            self.done = true;
            return None;
        }
        self.offset += read as u64;
        self.line += 1;
        // A fragment without a trailing newline is a torn tail, not an event.
        if !buffer.ends_with(b"\n") {
            self.done = true;
            return None;
        }
        match serde_json::from_slice::<EventEnvelope>(trim_newline(&buffer)) {
            Ok(envelope) => {
                if envelope.seq != self.expected_seq {
                    self.done = true;
                    return Some(Err(StoreError::Corrupt {
                        path: self.path.display().to_string(),
                        line: Some(self.line),
                        reason: format!("seq {} expected {}", envelope.seq, self.expected_seq),
                    }));
                }
                if envelope.session_id != self.session_id {
                    self.done = true;
                    return Some(Err(StoreError::Corrupt {
                        path: self.path.display().to_string(),
                        line: Some(self.line),
                        reason: format!("session {} is not {}", envelope.session_id, self.session_id),
                    }));
                }
                self.expected_seq += 1;
                Some(Ok(IndexedEvent {
                    envelope,
                    byte_offset: start,
                    byte_len: read as u64,
                }))
            }
            Err(err) => {
                let at_eof = self.reader.fill_buf().map(|rest| rest.is_empty()).unwrap_or(true);
                self.done = true;
                if at_eof {
                    // The final line is unparsable: a torn tail, ignored by the iterator.
                    None
                } else {
                    Some(Err(StoreError::Corrupt {
                        path: self.path.display().to_string(),
                        line: Some(self.line),
                        reason: format!("invalid JSON: {err}"),
                    }))
                }
            }
        }
    }
}

/// One event and its byte range in the log.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedEvent {
    /// The stored envelope.
    pub envelope: EventEnvelope,
    /// File offset of the first byte of the line.
    pub byte_offset: u64,
    /// Length of the line in bytes, including the trailing newline.
    pub byte_len: u64,
}

/// Builds the `session_events` index row for an appended event.
pub fn index_row(a: &AppendedEvent) -> EventIndexRow {
    index_row_parts(
        &a.envelope,
        a.byte_offset,
        a.byte_len,
        a.envelope.body().map(|body| body.summary()).unwrap_or_default(),
    )
}

/// Builds an index row from an envelope and its byte range, with a caller-computed summary.
///
/// Reindex uses this so it does not need to re-encode an [`AppendedEvent`] it read from disk.
pub(crate) fn index_row_parts(
    envelope: &EventEnvelope,
    byte_offset: u64,
    byte_len: u64,
    summary: String,
) -> EventIndexRow {
    EventIndexRow {
        session_id: envelope.session_id.clone(),
        seq: envelope.seq,
        id: envelope.id.clone(),
        ts: envelope.ts,
        event_type: envelope.event_type,
        actor: envelope.actor,
        harness: envelope.source.harness,
        model: envelope.source.model.clone(),
        byte_offset,
        byte_len,
        summary,
    }
}

/// What an append-only scan found.
struct ScanOutcome {
    last_seq: u64,
    bytes: u64,
    events: u64,
    torn_tail_bytes: u64,
}

/// Scans a log before opening it for append.
///
/// Corruption before the final line is an error; a final fragment without a newline or an
/// unparsable final line is a torn tail to truncate.
fn scan_for_append(path: &Path, id: &SessionId) -> Result<ScanOutcome, StoreError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::not_found("session log", id.as_str()));
        }
        Err(err) => return Err(StoreError::io(path.display(), err)),
    };

    let mut expected = 1_u64;
    let mut good_end = 0_usize;
    let mut events = 0_u64;
    let mut offset = 0_usize;

    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += line.len();
        let line_no = events + 1;
        let is_last = offset == bytes.len();
        let terminated = line.ends_with(b"\n");
        match serde_json::from_slice::<EventEnvelope>(trim_newline(line)) {
            Ok(envelope) => {
                if envelope.seq != expected {
                    return Err(StoreError::Corrupt {
                        path: path.display().to_string(),
                        line: Some(line_no),
                        reason: format!("seq {} expected {expected}", envelope.seq),
                    });
                }
                if envelope.session_id != *id {
                    return Err(StoreError::Corrupt {
                        path: path.display().to_string(),
                        line: Some(line_no),
                        reason: format!("session {} is not {}", envelope.session_id, id),
                    });
                }
                if !terminated {
                    break;
                }
                expected += 1;
                events += 1;
                good_end = offset;
                if is_last {
                    break;
                }
            }
            Err(_) => {
                if is_last || !terminated {
                    break;
                }
                return Err(StoreError::Corrupt {
                    path: path.display().to_string(),
                    line: Some(line_no),
                    reason: "invalid JSON".to_owned(),
                });
            }
        }
    }

    let torn_tail_bytes = (bytes.len() - good_end) as u64;
    Ok(ScanOutcome {
        last_seq: events,
        bytes: good_end as u64,
        events,
        torn_tail_bytes,
    })
}

/// Spills oversized tool output to the blob store, replacing it with a reference.
fn spill_tool_result(blobs: &BlobStore, body: &mut EventBody) -> Result<(), StoreError> {
    let EventBody::ToolResult(result) = body else {
        return Ok(());
    };
    let Some(output) = result.output.take() else {
        return Ok(());
    };
    if output.len() > BLOB_SPILL_BYTES {
        result.output_ref = Some(blobs.put(output.as_bytes())?);
        result.output = None;
    } else {
        result.output = Some(output);
    }
    Ok(())
}

/// Returns true when `value` serialises to at most `limit` bytes.
fn serialises_at_most(value: &serde_json::Value, limit: usize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= limit)
}

/// Encodes one envelope as a JSON line.
fn encode_line(envelope: &EventEnvelope) -> Result<Vec<u8>, StoreError> {
    let mut line = serde_json::to_vec(envelope)?;
    line.push(b'\n');
    Ok(line)
}

/// Removes the trailing newline from a line before parsing it.
fn trim_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// The single-writer lock file (`sessions/<id>/.lock`).
struct WriterLock {
    path: PathBuf,
}

impl WriterLock {
    /// Creates the lock file, replacing a stale one exactly once.
    fn acquire(path: &Path) -> Result<WriterLock, StoreError> {
        for _attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    let pid = std::process::id();
                    let secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_secs())
                        .unwrap_or_default();
                    let write = writeln!(file, "{pid}")
                        .and_then(|()| writeln!(file, "{secs}"))
                        .and_then(|()| file.sync_all());
                    if let Err(err) = write {
                        let _ = std::fs::remove_file(path);
                        return Err(StoreError::io(path.display(), err));
                    }
                    return Ok(WriterLock {
                        path: path.to_path_buf(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => match read_lock_pid(path) {
                    Some(pid) if pid_alive(pid) => return Err(StoreError::Locked { pid }),
                    _ => {
                        std::fs::remove_file(path).map_err(|err| StoreError::io(path.display(), err))?;
                    }
                },
                Err(err) => return Err(StoreError::io(path.display(), err)),
            }
        }
        Err(StoreError::Conflict(format!(
            "could not acquire session log lock {}",
            path.display()
        )))
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reads the pid from the first line of a lock file.
fn read_lock_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().next()?.trim().parse().ok()
}

/// Returns true when `pid` names a process this program may safely signal.
///
/// On platforms whose `pid_t` is signed 32-bit, `kill` parses large `u32` values as negative
/// ids. In particular, `u32::MAX` becomes `-1`, which addresses every process the caller may
/// signal. VOWP therefore refuses pid/gid 0, pid 1 and every value above [`i32::MAX`].
pub fn valid_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Returns whether a process with `pid` is alive.
///
/// Invalid ids always count as not alive. Linux reads `/proc/<pid>`; other platforms use
/// `kill -0`, which never signals the target.
pub(crate) fn pid_alive(pid: u32) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Fsyncs a directory so a newly created entry survives a crash.
fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    let file = File::open(dir).map_err(|err| StoreError::io(dir.display(), err))?;
    file.sync_all().map_err(|err| StoreError::io(dir.display(), err))
}

// ---------------------------------------------------------------------------
// Full session-directory snapshots
// ---------------------------------------------------------------------------

/// The only session-directory manifest format this build reads and writes.
pub const SESSION_DIR_FORMAT: &str = "vowp-session-dir-v1";

/// Default upper bound for one full session-directory pull (1 GiB).
pub const SESSION_DIR_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Name of the manifest stored inside a copied session directory.
pub const SESSION_DIR_MANIFEST: &str = "manifest.json";

/// One file in a [`SessionDirManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDirFile {
    /// Path relative to `sessions/<id>/`, using `/` separators.
    pub path: String,
    /// Exact byte size copied for this file.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA-256 of the file content.
    pub sha256: String,
}

/// The manifest of one full session-directory snapshot.
///
/// The remote side writes it as the first line of the snapshot stream and the importing
/// side writes the verified manifest into the copied directory as `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDirManifest {
    /// Always [`SESSION_DIR_FORMAT`].
    pub format: String,
    /// Origin session id.
    pub origin: SessionId,
    /// Every copied file, sorted by `path`.
    pub files: Vec<SessionDirFile>,
}

impl SessionDirManifest {
    /// Checks the format, the file paths, the digests and the sort order.
    ///
    /// Paths are relative to the session directory and limited to the layout this build
    /// knows: `log.jsonl`, `adapters.json`, `blobs/<sha256>` and `capsules/<name>.json`.
    /// Traversal, absolute paths, duplicates and the reserved `manifest.json` are refused.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.format != SESSION_DIR_FORMAT {
            return Err(StoreError::InvalidInput(format!(
                "unknown session directory manifest format: {}",
                self.format
            )));
        }
        if self.files.is_empty() {
            return Err(StoreError::InvalidInput(
                "session directory manifest has no files".to_owned(),
            ));
        }
        let mut previous: Option<&str> = None;
        let mut has_log = false;
        for file in &self.files {
            validate_session_dir_path(&self.origin, &file.path)?;
            if !is_lower_hex_sha256(&file.sha256) {
                return Err(StoreError::InvalidInput(format!(
                    "session {} manifest has an invalid sha256 for {}",
                    self.origin, file.path
                )));
            }
            if let Some(previous) = previous
                && file.path.as_str() <= previous
            {
                return Err(StoreError::InvalidInput(format!(
                    "session {} manifest is duplicated or unsorted at {}",
                    self.origin, file.path
                )));
            }
            previous = Some(file.path.as_str());
            has_log |= file.path == "log.jsonl";
        }
        if !has_log {
            return Err(StoreError::InvalidInput(format!(
                "session {} manifest has no log.jsonl",
                self.origin
            )));
        }
        Ok(())
    }

    /// Returns the sum of every file's declared size, saturating at `u64::MAX`.
    pub fn total_bytes(&self) -> u64 {
        self.files
            .iter()
            .fold(0_u64, |total, file| total.saturating_add(file.bytes))
    }
}

/// One file found while scanning a session directory, before hashing.
struct SessionDirEntry {
    path: String,
    path_on_disk: PathBuf,
    bytes: u64,
}

/// Builds the manifest for `sessions/<id>/` without writing anything.
///
/// Sizes are checked against `max_bytes` before any file is hashed. The caller must
/// quiesce the source session: this is a read-only scan, not a lock.
pub fn session_dir_manifest(
    paths: &NodePaths,
    id: &SessionId,
    max_bytes: u64,
) -> Result<SessionDirManifest, StoreError> {
    let entries = collect_session_dir_files(paths, id)?;
    let total = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.bytes)
            .ok_or_else(|| StoreError::InvalidInput(format!("session {id} is too large to copy")))
    })?;
    if total > max_bytes {
        return Err(StoreError::InvalidInput(format!(
            "session {id} is {total} bytes; the full pull limit is {max_bytes} bytes"
        )));
    }

    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        files.push(SessionDirFile {
            path: entry.path,
            bytes: entry.bytes,
            sha256: crate::blobs::sha256_file(&entry.path_on_disk)?,
        });
    }
    let manifest = SessionDirManifest {
        format: SESSION_DIR_FORMAT.to_owned(),
        origin: id.clone(),
        files,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Reads and validates a `manifest.json` from a copied session directory.
pub fn read_session_dir_manifest(path: &Path) -> Result<SessionDirManifest, StoreError> {
    let bytes = std::fs::read(path).map_err(|err| StoreError::io(path.display(), err))?;
    let manifest: SessionDirManifest = serde_json::from_slice(&bytes).map_err(|err| StoreError::Corrupt {
        path: path.display().to_string(),
        line: None,
        reason: format!("invalid session directory manifest: {err}"),
    })?;
    manifest.validate()?;
    Ok(manifest)
}

/// Writes the verified manifest into `sessions/<id>/manifest.json` atomically.
pub fn write_session_dir_manifest(
    paths: &NodePaths,
    id: &SessionId,
    manifest: &SessionDirManifest,
) -> Result<PathBuf, StoreError> {
    manifest.validate()?;
    if manifest.origin != *id {
        return Err(StoreError::InvalidInput(format!(
            "session manifest names {}, not {id}",
            manifest.origin
        )));
    }
    let path = paths.session_manifest_path(id);
    let bytes = serde_json::to_vec_pretty(manifest)?;
    crate::session_files::write_atomic(&path, &bytes)?;
    Ok(path)
}

/// Re-hashes every file and checks the log, adapters and capsules of one session directory.
///
/// The manifest must list exactly the files present. A missing, extra, resized or changed
/// file, an unclean log or an invalid capsule is refused, so a caller can run this before
/// renaming a staged directory into `sessions/`.
pub fn verify_session_dir(paths: &NodePaths, id: &SessionId, manifest: &SessionDirManifest) -> Result<(), StoreError> {
    manifest.validate()?;
    if manifest.origin != *id {
        return Err(StoreError::InvalidInput(format!(
            "session manifest names {}, not {id}",
            manifest.origin
        )));
    }

    let mut actual: BTreeMap<String, SessionDirEntry> = collect_session_dir_files(paths, id)?
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    for file in &manifest.files {
        let Some(entry) = actual.remove(&file.path) else {
            return Err(StoreError::Corrupt {
                path: paths.session_dir(id).display().to_string(),
                line: None,
                reason: format!("manifest names a missing file: {}", file.path),
            });
        };
        if entry.bytes != file.bytes {
            return Err(StoreError::Corrupt {
                path: entry.path_on_disk.display().to_string(),
                line: None,
                reason: format!("expected {} bytes, found {}", file.bytes, entry.bytes),
            });
        }
        let actual_sha = crate::blobs::sha256_file(&entry.path_on_disk)?;
        if actual_sha != file.sha256 {
            return Err(StoreError::Corrupt {
                path: entry.path_on_disk.display().to_string(),
                line: None,
                reason: format!(
                    "sha256 mismatch for {}: expected {}, got {actual_sha}",
                    file.path, file.sha256
                ),
            });
        }
    }
    if let Some(entry) = actual.into_values().next() {
        return Err(StoreError::Corrupt {
            path: entry.path_on_disk.display().to_string(),
            line: None,
            reason: format!("file is not listed in the session manifest: {}", entry.path),
        });
    }

    let log = SessionLog::open_read(paths, id)?;
    let report = log.verify()?;
    if !report.is_ok() {
        return Err(StoreError::Corrupt {
            path: paths.log_path(id).display().to_string(),
            line: None,
            reason: format!("session log is not clean: {}", report.problems.join("; ")),
        });
    }
    verify_session_dir_blobs(paths, id, &log)?;
    if paths.adapters_json_path(id).is_file() {
        crate::session_files::read_adapters_json(paths, id)?;
    }
    for path in crate::session_files::list_capsule_files(paths, id)? {
        let capsule = crate::session_files::read_capsule_file(&path)?;
        if capsule.session_id != *id {
            return Err(StoreError::Corrupt {
                path: path.display().to_string(),
                line: None,
                reason: format!("capsule belongs to session {}, not {id}", capsule.session_id),
            });
        }
    }
    Ok(())
}

/// Refuses a session log that names a blob the snapshot's `blobs/` does not contain.
///
/// A full pull never invents a placeholder for a missing blob: the source log and its blob
/// files must match exactly before the staged directory is renamed into place.
fn verify_session_dir_blobs(paths: &NodePaths, id: &SessionId, log: &SessionLogReader) -> Result<(), StoreError> {
    let blobs = BlobStore::new(paths.blobs_dir(id));
    for item in log.iter()? {
        let event = item?;
        let Ok(body) = event.envelope.body() else {
            continue;
        };
        let references: Vec<&str> = match &body {
            EventBody::UserMessage(payload) => payload
                .attachments
                .iter()
                .map(|reference| reference.sha256.as_str())
                .collect(),
            EventBody::ToolResult(payload) => payload
                .output_ref
                .iter()
                .map(|reference| reference.sha256.as_str())
                .collect(),
            EventBody::FileChange(payload) => payload
                .diff_ref
                .iter()
                .map(|reference| reference.sha256.as_str())
                .collect(),
            _ => Vec::new(),
        };
        for sha256 in references {
            if !blobs.exists(sha256) {
                return Err(StoreError::Corrupt {
                    path: paths.log_path(id).display().to_string(),
                    line: None,
                    reason: format!("event {} references a missing blob: {sha256}", event.envelope.seq),
                });
            }
        }
    }
    Ok(())
}

/// Lists the files a session directory snapshot contains, with their sizes.
///
/// Only the documented layout is accepted: `log.jsonl`, optional `adapters.json`, and the
/// regular files under `blobs/` and `capsules/`. Hidden temporary files are skipped;
/// symlinks and unexpected file names are refused rather than followed or silently lost.
fn collect_session_dir_files(paths: &NodePaths, id: &SessionId) -> Result<Vec<SessionDirEntry>, StoreError> {
    let session_dir = paths.session_dir(id);
    let metadata = std::fs::symlink_metadata(&session_dir).map_err(|err| StoreError::io(session_dir.display(), err))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::InvalidInput(format!(
            "session {id} directory is not a plain directory"
        )));
    }
    let mut entries = Vec::new();
    push_session_dir_file(&mut entries, id, &session_dir, "log.jsonl", true)?;
    push_session_dir_file(&mut entries, id, &session_dir, "adapters.json", false)?;

    for dir_name in ["blobs", "capsules"] {
        let dir = session_dir.join(dir_name);
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(StoreError::io(dir.display(), err)),
        };
        if metadata.file_type().is_symlink() {
            return Err(StoreError::InvalidInput(format!(
                "session {id} contains a symlink: {}",
                dir.display()
            )));
        }
        if !metadata.is_dir() {
            return Err(StoreError::InvalidInput(format!(
                "session {id} has a non-directory {dir_name}/"
            )));
        }
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|err| StoreError::io(dir.display(), err))? {
            let entry = entry.map_err(|err| StoreError::io(dir.display(), err))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(StoreError::InvalidInput(format!(
                    "session {id} has a non-UTF-8 file name in {dir_name}/"
                )));
            };
            names.push(name);
        }
        names.sort();
        for name in names {
            if name.starts_with('.') {
                continue;
            }
            let path_on_disk = dir.join(&name);
            let metadata =
                std::fs::symlink_metadata(&path_on_disk).map_err(|err| StoreError::io(path_on_disk.display(), err))?;
            if metadata.file_type().is_symlink() {
                return Err(StoreError::InvalidInput(format!(
                    "session {id} contains a symlink: {}",
                    path_on_disk.display()
                )));
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = format!("{dir_name}/{name}");
            validate_session_dir_path(id, &relative)?;
            entries.push(SessionDirEntry {
                path: relative,
                path_on_disk,
                bytes: metadata.len(),
            });
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

/// Adds one root file to the scan, requiring or skipping it as requested.
fn push_session_dir_file(
    entries: &mut Vec<SessionDirEntry>,
    id: &SessionId,
    session_dir: &Path,
    name: &str,
    required: bool,
) -> Result<(), StoreError> {
    let path_on_disk = session_dir.join(name);
    let metadata = match std::fs::symlink_metadata(&path_on_disk) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(StoreError::NotFound {
                what: "session file",
                id: format!("{id}/{name}"),
            });
        }
        Err(err) => return Err(StoreError::io(path_on_disk.display(), err)),
    };
    if metadata.file_type().is_symlink() {
        return Err(StoreError::InvalidInput(format!(
            "session {id} contains a symlink: {}",
            path_on_disk.display()
        )));
    }
    if !metadata.is_file() {
        return Err(StoreError::InvalidInput(format!("session {id} has a non-file {name}")));
    }
    entries.push(SessionDirEntry {
        path: name.to_owned(),
        path_on_disk,
        bytes: metadata.len(),
    });
    Ok(())
}

/// Validates one manifest path against the session-directory layout.
fn validate_session_dir_path(origin: &SessionId, path: &str) -> Result<(), StoreError> {
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return Err(StoreError::InvalidInput(format!(
            "session {origin} manifest has an invalid path: {path:?}"
        )));
    }
    if path == SESSION_DIR_MANIFEST || path == ".lock" || path == "import.json" {
        return Err(StoreError::InvalidInput(format!(
            "session {origin} manifest may not copy {path}"
        )));
    }
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    let allowed = match (first, rest.as_slice()) {
        ("log.jsonl" | "adapters.json", []) => true,
        ("blobs", [name]) => is_blob_name(name),
        ("capsules", [name]) => is_capsule_name(name),
        _ => false,
    };
    if !allowed {
        return Err(StoreError::InvalidInput(format!(
            "session {origin} manifest has an unexpected path: {path}"
        )));
    }
    Ok(())
}

/// Returns true for a lowercase hexadecimal SHA-256 name.
fn is_blob_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Returns true for a capsule file name (`<at_seq:010>-<ulid>.json`).
fn is_capsule_name(name: &str) -> bool {
    name.len() > ".json".len()
        && name.ends_with(".json")
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
}

/// Returns true for a lowercase hexadecimal SHA-256 digest.
fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64 && is_blob_name(value)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vowp_protocol::{FakeClock, HarnessKind, SessionCreated};

    use super::*;
    use crate::blobs::BlobStore;

    #[test]
    fn trim_newline_removes_only_one_newline() {
        assert_eq!(trim_newline(b"abc\n"), b"abc");
        assert_eq!(trim_newline(b"abc\n\n"), b"abc\n");
        assert_eq!(trim_newline(b"abc"), b"abc");
    }

    #[test]
    fn oversized_raw_is_dropped() {
        let raw = serde_json::json!({ "text": "x".repeat(RAW_MAX_BYTES) });
        assert!(!serialises_at_most(&raw, RAW_MAX_BYTES));
        let small = serde_json::json!({ "text": "x" });
        assert!(serialises_at_most(&small, RAW_MAX_BYTES));
    }

    #[test]
    fn valid_pid_rejects_ids_that_wrap_to_negative_pid_t() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            assert!(!valid_pid(pid), "{pid} must not be signalled");
        }
        assert!(valid_pid(2));
        assert!(valid_pid(i32::MAX as u32));
    }

    #[test]
    fn pid_alive_rejects_invalid_pids_and_finds_this_process() {
        for pid in [0, 1, u32::MAX, i32::MAX as u32 + 1] {
            assert!(!pid_alive(pid), "{pid} must count as not alive");
        }
        assert!(pid_alive(std::process::id()));
    }

    fn snapshot_setup() -> (tempfile::TempDir, NodePaths, SessionId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path().join("node"));
        paths.ensure().expect("paths");
        let id = SessionId::new();
        let log = SessionLog::create(&paths, &id, Arc::new(FakeClock::at(Timestamp::now()))).expect("log");
        log.append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: None,
                native_session_id: None,
            },
            body: EventBody::SessionCreated(SessionCreated {
                title: "snapshot".to_owned(),
                cwd: "/tmp".to_owned(),
                project: None,
                job_id: None,
                role: None,
                parent_session_id: None,
            }),
            raw: None,
            ts: None,
        })
        .expect("created");
        log.sync().expect("sync");
        drop(log);
        (dir, paths, id)
    }

    #[test]
    fn session_log_head_reports_the_last_complete_event() {
        let (_dir, paths, id) = snapshot_setup();
        let log = SessionLog::open(&paths, &id, Arc::new(FakeClock::at(Timestamp::now()))).expect("log");
        let appended = log
            .append(NewEvent {
                actor: Actor::User,
                source: EventSource {
                    harness: HarnessKind::Opencode,
                    model: None,
                    node: None,
                    native_session_id: None,
                },
                body: EventBody::UserMessage(vowp_protocol::UserMessage {
                    text: "advance the local side".to_owned(),
                    attachments: Vec::new(),
                    note: None,
                }),
                raw: None,
                ts: None,
            })
            .expect("user message");
        log.sync().expect("sync");
        drop(log);

        let reader = SessionLog::open_read(&paths, &id).expect("reader");
        let head = reader.head().expect("head");
        assert_eq!(head.last_seq, appended.envelope.seq);
        assert_eq!(head.event_type, Some(EventType::UserMessage));
        assert_eq!(head.ts, Some(appended.envelope.ts));
        assert_eq!(reader.last_seq().expect("last seq"), head.last_seq);
    }

    #[test]
    fn session_log_head_is_empty_for_an_empty_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(dir.path().join("node"));
        paths.ensure().expect("paths");
        let id = SessionId::new();
        let log = SessionLog::create(&paths, &id, Arc::new(FakeClock::at(Timestamp::now()))).expect("log");
        drop(log);

        let head = SessionLog::open_read(&paths, &id)
            .expect("reader")
            .head()
            .expect("head");
        assert_eq!(head, LogHead::default());
    }

    #[test]
    fn session_dir_manifest_verifies_and_detects_corruption() {
        let (_dir, paths, id) = snapshot_setup();
        let blob_content = b"a blob inside the snapshot";
        let blob = BlobStore::new(paths.blobs_dir(&id)).put(blob_content).expect("blob");
        let manifest = session_dir_manifest(&paths, &id, SESSION_DIR_MAX_BYTES).expect("manifest");
        assert_eq!(manifest.format, SESSION_DIR_FORMAT);
        assert_eq!(manifest.origin, id);
        assert!(manifest.files.iter().any(|file| file.path == "log.jsonl"));
        assert!(
            manifest
                .files
                .iter()
                .any(|file| file.path == format!("blobs/{}", blob.sha256))
        );
        verify_session_dir(&paths, &id, &manifest).expect("the fresh snapshot verifies");
        let path = write_session_dir_manifest(&paths, &id, &manifest).expect("manifest write");
        assert_eq!(read_session_dir_manifest(&path).expect("manifest read"), manifest);

        let blob_path = paths.blobs_dir(&id).join(&blob.sha256);
        std::fs::write(&blob_path, vec![b'X'; blob_content.len()]).expect("tamper");
        let error = verify_session_dir(&paths, &id, &manifest).expect_err("a changed blob must fail");
        match error {
            StoreError::Corrupt { reason, .. } => assert!(reason.contains("sha256 mismatch"), "{reason}"),
            other => panic!("expected a corrupt error, got {other:?}"),
        }

        std::fs::write(&blob_path, blob_content).expect("restore");
        let extra = paths.blobs_dir(&id).join("b".repeat(64));

        std::fs::write(&extra, b"not listed").expect("extra file");
        let error = verify_session_dir(&paths, &id, &manifest).expect_err("an extra file must fail");
        match error {
            StoreError::Corrupt { reason, .. } => assert!(reason.contains("not listed"), "{reason}"),
            other => panic!("expected a corrupt error, got {other:?}"),
        }
    }

    #[test]
    fn session_dir_manifest_refuses_a_directory_over_the_size_cap() {
        let (_dir, paths, id) = snapshot_setup();
        let error = session_dir_manifest(&paths, &id, 1).expect_err("the cap must refuse the session");
        match error {
            StoreError::InvalidInput(message) => {
                assert!(message.contains("full pull limit is 1 bytes"), "{message}");
            }
            other => panic!("expected an invalid-input error, got {other:?}"),
        }
    }

    #[test]
    fn session_dir_manifest_rejects_traversal_and_reserved_paths() {
        let id = SessionId::new();
        let valid = SessionDirFile {
            path: "log.jsonl".to_owned(),
            bytes: 1,
            sha256: "0".repeat(64),
        };
        for bad in [
            "../log.jsonl",
            "/log.jsonl",
            "blobs/../x",
            "capsules/../x.json",
            "manifest.json",
            ".lock",
            "import.json",
            "blobs/not-a-digest",
            "capsules/not-json.txt",
            "summary.md",
        ] {
            let manifest = SessionDirManifest {
                format: SESSION_DIR_FORMAT.to_owned(),
                origin: id.clone(),
                files: vec![
                    valid.clone(),
                    SessionDirFile {
                        path: bad.to_owned(),
                        bytes: 1,
                        sha256: "0".repeat(64),
                    },
                ],
            };
            assert!(manifest.validate().is_err(), "{bad} must be refused");
        }
    }
}
