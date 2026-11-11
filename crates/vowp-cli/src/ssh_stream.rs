//! The executor-side SSH ingest stream: spool, send, acknowledge and replay.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.2, §12.5 and §13.6. The client writes
//! every event to `<spool>/events.jsonl` and fsyncs it before the frame reaches ssh. Acks
//! are recorded by event id in `<spool>/acked.json`, so a new process can replay everything
//! that was not acknowledged. The home is the only sequence writer.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vowp_protocol::{IngestAck, NodeName, RemoteLine, RunId, SessionId};

use crate::CliError;

/// Number of unacknowledged events the client keeps in flight.
pub const INGEST_ACK_WINDOW: usize = 64;

/// How long `finish` waits for the home to acknowledge the run summary.
const SUMMARY_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a full in-flight window may block before the next send leaves the event spooled.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(60);

/// The home a surface dials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// Home node name.
    pub node: NodeName,
    /// SSH destination; empty means "run the home command locally".
    pub ssh_target: String,
    /// Absolute remote home path.
    pub remote_home: String,
}

/// Everything the reader thread sends to the writer.
enum ReaderEvent {
    /// One parsed acknowledgement.
    Ack(IngestAck),
    /// A line that was not valid JSON.
    LineError(String),
    /// The ingest child closed stdout.
    Eof,
}

/// One live child process plus its asynchronous ack reader.
struct Connection {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<ReaderEvent>,
    reader: Option<JoinHandle<()>>,
}

impl Connection {
    /// Closes stdin, waits for the child and returns its exit status text.
    fn close(mut self) -> Result<String, CliError> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|error| CliError::Remote(format!("ingest child: {error}")))?;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        Ok(format!("{status}"))
    }

    /// Kills the child and joins the reader without blocking on a live stream.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.stdin.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// What `<spool>/acked.json` records between processes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AckFile {
    /// Event id to the sequence number the home assigned.
    #[serde(default)]
    events: BTreeMap<String, u64>,
    /// Whether the home acknowledged the run summary.
    #[serde(default)]
    summary: bool,
}

/// A client that spools, sends and replays remote event lines.
pub struct IngestClient {
    target: SshTarget,
    session: SessionId,
    run: RunId,
    lease: String,
    executor: String,
    executor_node: NodeName,
    spool: PathBuf,
    project: Option<String>,
    connection: Option<Connection>,
    acked: AckFile,
    inflight: VecDeque<vowp_protocol::EventId>,
    summary_inflight: bool,
    spool_checked: bool,
    spool_syncs: usize,
    max_inflight: usize,
    window_timeout: Duration,
    summary_timeout: Duration,
    revoked: Option<IngestAck>,
}

impl IngestClient {
    /// Creates a client. The caller starts the SSH child with [`crate::remote`].
    pub fn new(target: SshTarget, session: SessionId, run: RunId, lease: String, spool: PathBuf) -> Self {
        let acked = load_ack_file(&spool).unwrap_or_default();
        let executor_node = target.node.clone();
        Self {
            target,
            session,
            run,
            lease,
            executor: String::new(),
            executor_node,
            spool,
            project: None,
            connection: None,
            acked,
            inflight: VecDeque::new(),
            summary_inflight: false,
            spool_checked: false,
            spool_syncs: 0,
            max_inflight: 0,
            window_timeout: WINDOW_TIMEOUT,
            summary_timeout: SUMMARY_ACK_TIMEOUT,
            revoked: None,
        }
    }

    /// Selects the project the remote ingest should open; `None` lets the home choose.
    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    /// Sets the executor string and executor node carried by every `Hello` frame.
    pub fn with_executor(mut self, executor: impl Into<String>, node: NodeName) -> Self {
        self.executor = executor.into();
        self.executor_node = node;
        self
    }

    /// Returns whether the spool already holds at least one frame.
    ///
    /// A corrupt complete line is an error, not an empty spool: starting a second harness
    /// on top of unreadable events would hide the loss.
    pub fn has_spool(&self) -> Result<bool, CliError> {
        Ok(!self.spool_lines()?.is_empty())
    }

    /// Returns whether the spool holds a run summary frame.
    ///
    /// Propagates a spool parse error for the same reason as [`IngestClient::has_spool`].
    pub fn summary_present(&self) -> Result<bool, CliError> {
        Ok(self
            .spool_lines()?
            .iter()
            .any(|line| matches!(line, RemoteLine::Summary { .. })))
    }

    /// Returns whether the home acknowledged the run summary.
    pub fn summary_acked(&self) -> bool {
        self.acked.summary
    }

    /// Returns whether every spooled event id has an acknowledgement.
    pub fn all_acked(&self) -> Result<bool, CliError> {
        Ok(self.spool_lines()?.iter().all(|line| match line {
            RemoteLine::Event { event } => self.acked.events.contains_key(event.id.as_str()),
            _ => true,
        }))
    }

    /// Spools an event and sends it, returning an ack when one arrived.
    pub fn send(&mut self, line: &RemoteLine) -> Result<Option<IngestAck>, CliError> {
        if let Some(revoked) = &self.revoked {
            return Ok(Some(revoked.clone()));
        }
        match line {
            RemoteLine::Hello { .. } => {
                self.ensure_connected()?;
                self.drain_acks(false)
            }
            RemoteLine::Heartbeat { .. } => {
                self.ensure_connected()?;
                if self.connection.is_some() && self.write_line(line).is_err() {
                    self.disconnect();
                    return Ok(None);
                }
                self.drain_acks(false)
            }
            RemoteLine::Event { event } => {
                // Spool before the first connection attempt: a spawn failure must not lose
                // an event that the harness already produced.
                self.spool_append(line)?;
                if self.acked.events.contains_key(event.id.as_str()) {
                    return self.drain_acks(false);
                }
                if !self.ensure_connected()? {
                    return Ok(None);
                }
                if self.inflight.iter().any(|id| id == &event.id) {
                    // A reconnect replayed this event before we got here.
                    return self.drain_acks(false);
                }
                if !self.wait_for_window()? {
                    return Ok(None);
                }
                if self.acked.events.contains_key(event.id.as_str()) {
                    return self.drain_acks(false);
                }
                if self.write_line(line).is_err() {
                    // The event is already fsynced in the spool; the next send or replay
                    // reconnects and starts from the first unacked frame.
                    self.disconnect();
                    return Ok(None);
                }
                self.inflight.push_back(event.id.clone());
                self.max_inflight = self.max_inflight.max(self.inflight.len());
                self.drain_acks(false)
            }
            RemoteLine::Summary { .. } => {
                // The summary is spooled first too: a killed client can finish the turn from
                // the spool without a second harness process.
                self.spool_append(line)?;
                if self.acked.summary {
                    return self.drain_acks(false);
                }
                if !self.ensure_connected()? {
                    return Ok(None);
                }
                if self.summary_inflight {
                    return self.drain_acks(false);
                }
                if !self.wait_for_window()? {
                    return Ok(None);
                }
                if self.acked.summary {
                    return self.drain_acks(false);
                }
                if self.write_line(line).is_err() {
                    self.disconnect();
                    return Ok(None);
                }
                self.summary_inflight = true;
                self.drain_acks(true)
            }
        }
    }

    /// Resends every event from the first unacked one and returns how many were sent.
    pub fn replay(&mut self) -> Result<u32, CliError> {
        if self.revoked.is_some() {
            return Ok(0);
        }
        if !self.ensure_connected()? {
            return Ok(0);
        }
        let sent = self.replay_pending()?;
        let _ = self.drain_acks(false)?;
        Ok(sent)
    }

    /// Returns the takeover notice, when the home revoked the lease.
    pub fn revoked(&self) -> Option<&IngestAck> {
        self.revoked.as_ref()
    }

    /// Flushes the spool, waits for the summary ack and closes the stream.
    pub fn finish(&mut self) -> Result<(), CliError> {
        self.finish_with_timeout(self.summary_timeout)
    }

    /// [`IngestClient::finish`] with an injectable summary-ack deadline.
    fn finish_with_timeout(&mut self, timeout: Duration) -> Result<(), CliError> {
        if !timeout.is_zero() && self.connection.is_none() && self.summary_present()? && !self.acked.summary {
            let _ = self.ensure_connected();
            let _ = self.replay_pending();
        }
        let deadline = Instant::now() + timeout;
        while self.summary_present()? && !self.acked.summary && self.revoked.is_none() && Instant::now() < deadline {
            if self.connection.is_none() {
                if !self.ensure_connected().unwrap_or(false) {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                let _ = self.replay_pending()?;
            }
            match self.pump_one(Duration::from_millis(500)) {
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(connection) = self.connection.take() {
            let status = connection.close()?;
            if self.revoked.is_none() && self.summary_present()? && !self.acked.summary {
                return Err(CliError::Remote(format!(
                    "ingest ended without acknowledging the summary ({status})"
                )));
            }
            if self.revoked.is_none() && !self.summary_present()? {
                return Err(CliError::Remote(format!(
                    "ingest ended without a run summary ({status})"
                )));
            }
        }
        // A takeover means the home deliberately ended this surface's turn; the caller
        // reports the read-only notice and exits 75 rather than treating it as success.
        if self.revoked.is_some() {
            return Ok(());
        }
        if !self.summary_present()? {
            return Err(CliError::Remote("ingest finished without a run summary".to_owned()));
        }
        if !self.acked.summary {
            return Err(CliError::Remote(format!(
                "the home did not acknowledge the summary within {timeout:?}"
            )));
        }
        if !self.all_acked()? {
            return Err(CliError::Remote(
                "the home acknowledged the summary but not every event".to_owned(),
            ));
        }
        Ok(())
    }

    /// Spawns the ingest child and sends its hello frame unless one is already live.
    fn ensure_connected(&mut self) -> Result<bool, CliError> {
        if self.connection.is_some() {
            return Ok(true);
        }
        if self.revoked.is_some() {
            return Ok(false);
        }
        self.connect()?;
        let _ = self.replay_pending()?;
        Ok(self.connection.is_some())
    }

    /// Spawns one ingest process and sends `RemoteLine::Hello`.
    fn connect(&mut self) -> Result<(), CliError> {
        let mut command = self.command()?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|error| CliError::Remote(format!("cannot start ingest: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CliError::Remote("ingest child has no stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CliError::Remote("ingest child has no stdout".to_owned()))?;
        let (sender, events) = channel();
        let reader = spawn_reader(stdout, sender);
        self.connection = Some(Connection {
            child,
            stdin: Some(stdin),
            events,
            reader: Some(reader),
        });
        self.inflight.clear();
        self.summary_inflight = false;
        let hello = RemoteLine::Hello {
            session_id: self.session.clone(),
            run_id: self.run.clone(),
            executor: self.executor.clone(),
            node: self.executor_node.clone(),
            spool_id: self.spool.display().to_string(),
            lease: self.lease.clone(),
        };
        if self.write_line(&hello).is_err() {
            // A failed first write is a transport drop, not a reason to lose the turn.
            self.disconnect();
        }
        Ok(())
    }

    /// Builds the local or ssh command that runs `vowp session ingest`.
    fn command(&self) -> Result<Command, CliError> {
        let mut args = vec![
            "--home".to_owned(),
            self.target.remote_home.clone(),
            "session".to_owned(),
            "ingest".to_owned(),
            self.session.to_string(),
            "--run".to_owned(),
            self.run.to_string(),
        ];
        if let Some(project) = &self.project {
            args.splice(2..2, ["--project".to_owned(), project.clone()]);
        }
        if self.target.ssh_target.is_empty() {
            let program =
                std::env::current_exe().map_err(|error| CliError::Remote(format!("current executable: {error}")))?;
            let mut command = Command::new(program);
            command.args(&args);
            Ok(command)
        } else {
            let mut argv = crate::remote::build_ssh_argv(&self.target.ssh_target, &args, false);
            if argv.first().is_some_and(|program| program == "ssh") {
                argv.insert(1, "-T".to_owned());
            }
            let (program, rest) = argv
                .split_first()
                .ok_or_else(|| CliError::Remote("empty ssh argv".to_owned()))?;
            let mut command = Command::new(program);
            command.args(rest);
            Ok(command)
        }
    }

    /// Writes one JSON line to the child's stdin.
    fn write_line(&mut self, line: &RemoteLine) -> Result<(), CliError> {
        let text = serde_json::to_string(line)
            .map_err(|error| CliError::Remote(format!("cannot encode remote line: {error}")))?;
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| CliError::Remote("ingest is not connected".to_owned()))?;
        let Some(stdin) = connection.stdin.as_mut() else {
            return Err(CliError::Remote("ingest stdin is closed".to_owned()));
        };
        stdin.write_all(text.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    /// Waits until the in-flight window has room; `false` means the connection dropped.
    fn wait_for_window(&mut self) -> Result<bool, CliError> {
        let deadline = Instant::now() + self.window_timeout;
        while self.inflight.len() >= INGEST_ACK_WINDOW {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if self.connection.is_none() {
                return Ok(false);
            }
            let _ = self.pump_one(Duration::from_millis(250))?;
        }
        Ok(true)
    }

    /// Resends spooled frames that are not acknowledged yet.
    fn replay_pending(&mut self) -> Result<u32, CliError> {
        let lines = self.spool_lines()?;
        let mut sent = 0;
        for line in lines {
            match &line {
                RemoteLine::Event { event } => {
                    if self.acked.events.contains_key(event.id.as_str())
                        || self.inflight.iter().any(|id| id == &event.id)
                    {
                        continue;
                    }
                    // Replay obeys the same window as a live send; a reconnect must not
                    // dump an unbounded spool into the pipe.
                    if !self.wait_for_window()? || self.connection.is_none() {
                        self.disconnect();
                        return Ok(sent);
                    }
                    if self.write_line(&line).is_err() {
                        self.disconnect();
                        return Ok(sent);
                    }
                    self.inflight.push_back(event.id.clone());
                    self.max_inflight = self.max_inflight.max(self.inflight.len());
                    sent += 1;
                }
                RemoteLine::Summary { .. } => {
                    if self.acked.summary || self.summary_inflight {
                        continue;
                    }
                    if self.write_line(&line).is_err() {
                        self.disconnect();
                        return Ok(sent);
                    }
                    self.summary_inflight = true;
                }
                RemoteLine::Hello { .. } | RemoteLine::Heartbeat { .. } => {}
            }
        }
        Ok(sent)
    }

    /// Reads one reader event, handling acknowledgements and connection loss.
    fn pump_one(&mut self, timeout: Duration) -> Result<Option<IngestAck>, CliError> {
        let Some(connection) = self.connection.as_ref() else {
            return Ok(None);
        };
        let event = match connection.events.recv_timeout(timeout) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                self.disconnect();
                return Ok(None);
            }
        };
        match event {
            ReaderEvent::Ack(ack) => self.record_ack(ack),
            ReaderEvent::LineError(_message) => {
                // A cut inside an ack line is a transport drop; replay is idempotent.
                self.disconnect();
                Ok(None)
            }
            ReaderEvent::Eof => {
                self.disconnect();
                Ok(None)
            }
        }
    }

    /// Drains every ack that is already available.
    fn drain_acks(&mut self, block: bool) -> Result<Option<IngestAck>, CliError> {
        let mut last = None;
        loop {
            let next = if block {
                self.pump_one(Duration::from_millis(500))?
            } else {
                self.pump_one(Duration::ZERO)?
            };
            match next {
                Some(ack) => {
                    let stop = matches!(
                        ack,
                        IngestAck::LeaseRevoked { .. } | IngestAck::Summary { .. } | IngestAck::Error { .. }
                    );
                    last = Some(ack);
                    if stop {
                        break;
                    }
                }
                None => break,
            }
        }
        Ok(last)
    }

    /// Applies one ack to the in-flight window and the acked sidecar.
    fn record_ack(&mut self, ack: IngestAck) -> Result<Option<IngestAck>, CliError> {
        match &ack {
            IngestAck::Ack { event_id, seq } | IngestAck::Duplicate { event_id, seq } => {
                self.acked.events.insert(event_id.to_string(), *seq);
                self.inflight.retain(|id| id != event_id);
                self.save_ack_file()?;
            }
            IngestAck::Summary { .. } => {
                self.acked.summary = true;
                self.summary_inflight = false;
                self.save_ack_file()?;
            }
            IngestAck::LeaseRevoked { .. } => {
                self.revoked = Some(ack.clone());
            }
            IngestAck::Error { .. } => {}
        }
        Ok(Some(ack))
    }

    /// Drops the child after an I/O failure; spooled frames stay for the next connection.
    fn disconnect(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.kill();
        }
        self.inflight.clear();
        self.summary_inflight = false;
    }

    /// Appends one frame to `events.jsonl` and fsyncs it before it can reach ssh.
    fn spool_append(&mut self, line: &RemoteLine) -> Result<(), CliError> {
        // Repair a torn tail from a killed client once, before the first append of this
        // process; later appends are plain fsynced O_APPEND writes.
        if !self.spool_checked {
            let _ = self.spool_lines()?;
            self.spool_checked = true;
        }
        ensure_private_dir(&self.spool)?;
        let path = self.events_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| CliError::Io(std::io::Error::other(format!("{}: {error}", path.display()))))?;
        set_private_file(&path)?;
        let text = serde_json::to_vec(line)
            .map_err(|error| CliError::Remote(format!("cannot encode spooled line: {error}")))?;
        file.write_all(&text)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        self.spool_syncs += 1;
        Ok(())
    }

    /// Reads and repairs `events.jsonl`, returning its complete frames.
    fn spool_lines(&self) -> Result<Vec<RemoteLine>, CliError> {
        let path = self.events_path();
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let mut file = File::open(&path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if complete < bytes.len() {
            let file = OpenOptions::new().write(true).open(&path)?;
            file.set_len(complete as u64)?;
            file.sync_all()?;
            bytes.truncate(complete);
        }
        let mut lines = Vec::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value: RemoteLine = serde_json::from_slice(line)
                .map_err(|error| CliError::Invalid(format!("{}: invalid spool line: {error}", path.display())))?;
            lines.push(value);
        }
        Ok(lines)
    }

    /// Writes `<spool>/acked.json` atomically.
    fn save_ack_file(&self) -> Result<(), CliError> {
        ensure_private_dir(&self.spool)?;
        let path = self.acked_path();
        let temporary = path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(&self.acked)
            .map_err(|error| CliError::Remote(format!("cannot encode ack file: {error}")))?;
        {
            let mut file = File::create(&temporary)?;
            file.write_all(&text)?;
            file.sync_all()?;
        }
        set_private_file(&temporary)?;
        std::fs::rename(&temporary, &path)?;
        set_private_file(&path)?;
        Ok(())
    }

    /// Returns `<spool>/events.jsonl`.
    fn events_path(&self) -> PathBuf {
        self.spool.join("events.jsonl")
    }

    /// Returns `<spool>/acked.json`.
    fn acked_path(&self) -> PathBuf {
        self.spool.join("acked.json")
    }
}

impl Drop for IngestClient {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.kill();
        }
    }
}

/// Reads ack frames from the child's stdout on its own thread.
fn spawn_reader(stdout: std::process::ChildStdout, sender: Sender<ReaderEvent>) -> JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<IngestAck>(text) {
                        Ok(ack) => {
                            if sender.send(ReaderEvent::Ack(ack)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(ReaderEvent::LineError(error.to_string()));
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(ReaderEvent::LineError(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(ReaderEvent::Eof);
    })
}

/// Loads `<spool>/acked.json`, treating a missing file as empty.
fn load_ack_file(spool: &Path) -> Result<AckFile, CliError> {
    let path = spool.join("acked.json");
    if !path.is_file() {
        return Ok(AckFile::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text)
        .map_err(|error| CliError::Invalid(format!("{}: invalid ack file: {error}", path.display())))
}

/// Creates the spool directory with mode 0700.
fn ensure_private_dir(path: &Path) -> Result<(), CliError> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

/// Restricts a spool file to mode 0600.
fn set_private_file(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Instant;

    use vowp_protocol::{Actor, Cost, EventId, EventSource, EventType, HarnessKind, RunOutcome, StopReason, Timestamp};

    fn target() -> SshTarget {
        SshTarget {
            node: NodeName::parse("local").expect("node"),
            ssh_target: String::new(),
            remote_home: ".".to_owned(),
        }
    }

    fn session() -> SessionId {
        SessionId::new()
    }

    fn run() -> RunId {
        RunId::new()
    }

    fn event(text: &str) -> RemoteLine {
        RemoteLine::Event {
            event: vowp_protocol::RemoteEvent {
                id: EventId::new(),
                ts: Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp"),
                event_type: EventType::AssistantMessage,
                actor: Actor::Assistant,
                source: EventSource {
                    harness: HarnessKind::Opencode,
                    model: Some("deepseek/deepseek-flash".to_owned()),
                    node: Some(NodeName::parse("local").expect("node")),
                    native_session_id: None,
                },
                payload: serde_json::json!({"text": text}),
                raw: None,
                replayable: true,
            },
        }
    }

    fn summary() -> RemoteLine {
        RemoteLine::Summary {
            exit_code: Some(0),
            outcome: RunOutcome::Ok,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: Some("done".to_owned()),
            stop_reason: StopReason::EndTurn,
            usage: None,
        }
    }

    /// Attaches `command` as the ingest child with piped stdio and an ack reader.
    fn attach_child(client: &mut IngestClient, command: &mut Command) {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().expect("child");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (sender, events) = channel();
        let reader = spawn_reader(stdout, sender);
        client.connection = Some(Connection {
            child,
            stdin: Some(stdin),
            events,
            reader: Some(reader),
        });
    }

    /// Attaches a child that consumes bytes and never acknowledges anything.
    fn silent_connection(client: &mut IngestClient) {
        attach_child(client, Command::new("sh").args(["-c", "exec sleep 30"]));
    }

    fn write_spool(spool: &Path, lines: &[RemoteLine]) {
        std::fs::create_dir_all(spool).expect("spool dir");
        let text = lines
            .iter()
            .map(|line| serde_json::to_string(line).expect("line"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(spool.join("events.jsonl"), text).expect("spool");
    }

    #[test]
    fn ingest_ack_window_is_64_and_async() {
        assert_eq!(INGEST_ACK_WINDOW, 64);
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), tmp.path().join("spool"));
        client.window_timeout = Duration::from_millis(50);
        silent_connection(&mut client);

        for index in 0..INGEST_ACK_WINDOW {
            let ack = client.send(&event(&format!("event {index}"))).expect("send");
            assert!(ack.is_none(), "the client waited for an ack that never came");
        }
        // Do not bound this loop with wall time: each send fsyncs its spool line, and on
        // a loaded machine those fsyncs can dominate any fixed deadline. The observable
        // async contract is structural: all 64 frames are in flight with no ack received,
        // and only the next send waits for window room.
        assert_eq!(client.inflight.len(), INGEST_ACK_WINDOW);
        assert_eq!(client.max_inflight, INGEST_ACK_WINDOW);

        let start = Instant::now();
        let ack = client.send(&event("one too many")).expect("send");
        assert!(ack.is_none());
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "the 65th event did not wait for window room: {:?}",
            start.elapsed()
        );
        assert_eq!(
            client.inflight.len(),
            INGEST_ACK_WINDOW,
            "the in-flight window grew past {INGEST_ACK_WINDOW}"
        );
        let spool = std::fs::read_to_string(tmp.path().join("spool/events.jsonl")).expect("spool");
        assert_eq!(spool.lines().count(), INGEST_ACK_WINDOW + 1);
    }

    #[test]
    fn replay_obeys_the_inflight_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        let lines: Vec<RemoteLine> = (0..100).map(|index| event(&format!("replayed {index}"))).collect();
        write_spool(&spool, &lines);
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool);
        client.window_timeout = Duration::from_millis(50);
        silent_connection(&mut client);
        let sent = client.replay().expect("replay");
        assert_eq!(sent as usize, INGEST_ACK_WINDOW);
        assert_eq!(
            client.max_inflight, INGEST_ACK_WINDOW,
            "replay put more than {INGEST_ACK_WINDOW} events in flight"
        );
    }

    #[test]
    fn exec_spools_and_fsyncs_before_send() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        let marker = tmp.path().join("child-saw-spool.txt");
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool.clone());
        // The child snapshots the spool after reading the frame. If send wrote before the
        // fsynced append, the snapshot would be empty. Write the marker through a
        // temporary file and rename it so the parent never observes an empty marker.
        attach_child(
            &mut client,
            Command::new("sh").args([
                "-c",
                &format!(
                    "IFS= read -r line; cat '{spool}' > '{marker}.tmp' && mv '{marker}.tmp' '{marker}'; cat >/dev/null",
                    spool = spool.join("events.jsonl").display(),
                    marker = marker.display()
                ),
            ]),
        );
        client.send(&event("spooled before send")).expect("send");

        // A loaded host can delay the child; this is a diagnostic bound, not a latency
        // assertion, so leave generous room before declaring the child missing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                break text;
            }
            assert!(Instant::now() < deadline, "the child never snapshotted the spool");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("spooled before send"), "{text}");

        let path = spool.join("events.jsonl");
        let spool_text = std::fs::read_to_string(&path).expect("spool line");
        assert!(spool_text.contains("spooled before send"), "{spool_text}");
        assert!(spool_text.ends_with('\n'), "the spool line is not complete");
        assert!(client.spool_syncs > 0, "the spool line was not fsynced");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&spool).expect("metadata").permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn finish_fails_when_the_summary_was_never_acknowledged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        write_spool(&spool, &[event("one"), summary()]);
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool);
        client.summary_timeout = Duration::ZERO;
        let error = client
            .finish()
            .expect_err("an unacknowledged summary must not look like success");
        assert!(error.to_string().contains("acknowledge"), "unexpected error: {error}");
    }

    #[test]
    fn finish_fails_when_events_are_unacked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        write_spool(&spool, &[event("not acked"), summary()]);
        std::fs::write(spool.join("acked.json"), br#"{"events":{},"summary":true}"#).expect("acked");
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool);
        let error = client
            .finish()
            .expect_err("an unacked event must not look like success");
        assert!(error.to_string().contains("event"), "unexpected error: {error}");
    }

    #[test]
    fn corrupt_spool_is_reported_instead_of_looking_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        std::fs::create_dir_all(&spool).expect("spool dir");
        std::fs::write(spool.join("events.jsonl"), b"{\"type\": \"event\"\n").expect("corrupt spool line");
        let client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool);

        for (name, error) in [
            ("has_spool", client.has_spool().expect_err("has_spool must fail")),
            (
                "summary_present",
                client.summary_present().expect_err("summary_present must fail"),
            ),
            ("all_acked", client.all_acked().expect_err("all_acked must fail")),
        ] {
            assert!(
                error.to_string().contains("invalid spool line"),
                "{name} hid the corruption: {error}"
            );
        }
    }

    #[test]
    fn finish_accepts_a_recorded_summary_ack() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let spool = tmp.path().join("spool");
        let line = event("one");
        let id = match &line {
            RemoteLine::Event { event } => event.id.to_string(),
            _ => unreachable!(),
        };
        write_spool(&spool, &[line, summary()]);
        let acked = serde_json::json!({
            "events": {id: 5},
            "summary": true,
        });
        std::fs::write(spool.join("acked.json"), acked.to_string()).expect("acked");
        let mut client = IngestClient::new(target(), session(), run(), "lease".to_owned(), spool);
        client.finish().expect("a recorded summary ack finishes");
    }
}
