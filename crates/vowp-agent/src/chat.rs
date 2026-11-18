//! `vowp chat --agent`: one local chat turn over the existing agent connection.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.5, §8.3–§8.4 and §9.2–§9.5. The home
//! owns the log; this module acquires the surface lease, asks the home to begin the turn,
//! runs the adapter on this machine, spools every event before sending it, and replays the
//! first unacknowledged event after a transport drop. A takeover or lease loss stops the
//! adapter and leaves the spool for inspection.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vowp_protocol::{
    Adapter, AdapterError, AgentFrame, CancelToken, EventBody, EventId, EventSource, Executor, HarnessEvent, JobId,
    PathRef, RemoteEvent, RemoteLine, RunId, RunPurpose, SessionId, StartInput, Timestamp, TurnSummary,
};

use crate::client::{AgentClient, AgentClock, PAIR_TIMEOUT};
use crate::config::{PairedToken, paired_token_for_session};
use crate::error::AgentError;
use crate::spool::{DEFAULT_SPOOL_CAP_BYTES, Spool, SpoolAppend};
use crate::transport::FrameRead;

/// How long the agent waits for the next adapter event before sending a lease heartbeat.
pub const EVENT_POLL: Duration = Duration::from_secs(1);

/// How often a held lease is refreshed while a turn runs.
pub const LEASE_HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

/// Reconnect attempts before a turn fails; the spool stays for the owner.
pub const MAX_RECONNECTS: u32 = 4;

/// Run epoch used for every line of one chat turn.
pub const CHAT_EPOCH: u64 = 1;

/// One turn requested from the Mac.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatTurnRequest {
    /// Executor to run locally.
    pub executor: Executor,
    /// Prompt for the turn.
    pub prompt: String,
    /// Local working directory.
    pub cwd: PathBuf,
    /// Project-relative working directory sent to the home.
    pub cwd_ref: PathRef,
    /// Job the session belongs to, when job-scoped.
    pub job: Option<JobId>,
    /// Why the run starts.
    pub purpose: RunPurpose,
}

/// How one local chat turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTurnStatus {
    /// The summary was acknowledged by the home.
    Done,
    /// The lease was taken over or lost; the adapter is stopped and the spool kept.
    ReadOnly {
        /// Human-readable reason.
        reason: String,
    },
    /// The local spool reached its cap; nothing else was sent.
    SpoolFull {
        /// Size the refused write would have reached.
        bytes: u64,
        /// Configured cap.
        cap: u64,
    },
    /// Ctrl-C or the stop file asked the turn to stop.
    Interrupted,
}

/// The local side of one agent chat surface.
///
/// The surface token is read from `surfaces/<id>/token` and never printed; `Debug` for
/// [`PairedToken`] redacts it. The lease token returned by the home stays in memory and in
/// the 0600 spool, never in `status.json` or a log line.
pub struct ChatClient {
    client: AgentClient,
    surface: PairedToken,
    session: SessionId,
    lease: Option<String>,
    spool_cap: u64,
}

impl ChatClient {
    /// Builds a chat client around an existing frame client.
    pub fn new(client: AgentClient, surface: PairedToken, session: SessionId) -> Self {
        Self {
            client,
            surface,
            session,
            lease: None,
            spool_cap: DEFAULT_SPOOL_CAP_BYTES,
        }
    }

    /// Loads the agent config, node key, transport and paired surface token.
    pub fn from_state_dir(
        state_dir: PathBuf,
        session: SessionId,
        clock: Arc<dyn AgentClock>,
    ) -> Result<Self, AgentError> {
        let surface = paired_token_for_session(&state_dir, &session)?.ok_or_else(|| {
            AgentError::NotEnrolled(format!(
                "no paired surface for session {session}; run `vowp agent pair` first"
            ))
        })?;
        let client = AgentClient::from_state_dir(state_dir, clock)?;
        Ok(Self::new(client, surface, session))
    }

    /// Sets the spool cap (tests use a small cap to exercise `spool_full`).
    pub fn with_spool_cap(mut self, cap: u64) -> Self {
        self.spool_cap = cap;
        self
    }

    /// Returns the paired surface id.
    pub fn surface_id(&self) -> &vowp_protocol::SurfaceId {
        &self.surface.surface_id
    }

    /// Returns the held plaintext lease token, when the surface holds the lease.
    ///
    /// Callers must not print it; the accessor exists so tests can assert it is not stored.
    pub fn lease_token(&self) -> Option<&str> {
        self.lease.as_deref()
    }

    /// Runs one prompt and blocks until the home accepts the summary.
    ///
    /// `on_event` is called once for every new adapter event (never for a replayed event).
    /// A takeover, lease loss, interrupted turn or full spool returns a status instead of a
    /// hard error; the spool is kept in every case.
    pub fn turn(
        &mut self,
        request: &ChatTurnRequest,
        adapter: &dyn Adapter,
        on_event: &mut dyn FnMut(&HarnessEvent),
        cancel: Option<&CancelToken>,
    ) -> Result<ChatTurnStatus, AgentError> {
        if !request.cwd.is_dir() {
            return Err(AgentError::Config(format!(
                "working directory {} does not exist on this machine",
                request.cwd.display()
            )));
        }
        if !self.ensure_lease()? {
            return Ok(ChatTurnStatus::ReadOnly {
                reason: "the lease is held by another surface".to_owned(),
            });
        }
        let lease = self
            .lease
            .clone()
            .ok_or_else(|| AgentError::Protocol("lease grant is missing".to_owned()))?;
        let run = self.begin_turn(request, &lease)?;
        let Some(run) = run else {
            return Ok(ChatTurnStatus::ReadOnly {
                reason: "the home did not grant the turn".to_owned(),
            });
        };

        let mut spool = Spool::with_cap(self.client.state_dir(), &self.session, &run, self.spool_cap)?;
        let hello = AgentFrame::RemoteLine {
            run_id: run.clone(),
            epoch: CHAT_EPOCH,
            line: RemoteLine::Hello {
                session_id: self.session.clone(),
                run_id: run.clone(),
                executor: request.executor.to_string(),
                node: self.surface_node(),
                spool_id: format!("{}-{}", self.session, run),
                lease: lease.clone(),
            },
        };
        if let Err(error) = self.client.send_frame(&hello) {
            if error.is_fatal() {
                return Err(error);
            }
            match self.reconnect(&mut spool, &run)? {
                Ack::Acked | Ack::Duplicate => {}
                Ack::ReadOnly(reason) => return Ok(ChatTurnStatus::ReadOnly { reason }),
                Ack::Failed(message) => {
                    return Err(AgentError::Remote {
                        kind: vowp_protocol::ErrorKind::Conflict,
                        message,
                        retryable: false,
                    });
                }
                Ack::TransportLost => {
                    return Err(AgentError::Transport(
                        "the home did not accept the replayed hello".to_owned(),
                    ));
                }
            }
        }
        let start = StartInput {
            session_id: self.session.clone(),
            executor: request.executor.clone(),
            cwd: request.cwd.clone(),
            prompt: request.prompt.clone(),
            resume: None,
            new_native_id: None,
            env: Vec::new(),
            permission_mode: None,
            system_note: None,
            event_log_hint: None,
            timeout: None,
        };
        let mut adapter_run = adapter.start(start).map_err(adapter_error)?;
        let mut last_heartbeat = Timestamp::now();
        loop {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                let _ = adapter_run.stop();
                return Ok(ChatTurnStatus::Interrupted);
            }
            if Timestamp::now().since(last_heartbeat) >= LEASE_HEARTBEAT_EVERY {
                last_heartbeat = Timestamp::now();
                if let Some(status) = self.heartbeat()? {
                    let _ = adapter_run.stop();
                    return Ok(status);
                }
            }
            match adapter_run.events().recv_timeout(EVENT_POLL) {
                Ok(event) => {
                    // The summary owns the canonical turn.ended; forwarding the adapter's
                    // terminal event as an event would append it twice.
                    if matches!(event.body, EventBody::TurnEnded(_)) {
                        continue;
                    }
                    let line = self.event_line(&event, &request.executor);
                    let frame = AgentFrame::RemoteLine {
                        run_id: run.clone(),
                        epoch: CHAT_EPOCH,
                        line: RemoteLine::Event { event: line },
                    };
                    match spool.append(&frame)? {
                        SpoolAppend::Written => {}
                        SpoolAppend::Full { bytes, cap } => {
                            let _ = adapter_run.stop();
                            return Ok(ChatTurnStatus::SpoolFull { bytes, cap });
                        }
                    }
                    match self.send_and_ack(&mut spool, &run, &frame)? {
                        Ack::Acked => on_event(&event),
                        Ack::Duplicate => {}
                        Ack::ReadOnly(reason) => {
                            let _ = adapter_run.stop();
                            return Ok(ChatTurnStatus::ReadOnly { reason });
                        }
                        Ack::Failed(message) => {
                            let _ = adapter_run.stop();
                            return Err(AgentError::Remote {
                                kind: vowp_protocol::ErrorKind::Conflict,
                                message,
                                retryable: false,
                            });
                        }
                        Ack::TransportLost => {
                            let _ = adapter_run.stop();
                            return Err(AgentError::Transport(
                                "the home did not acknowledge the event before the retry budget ran out".to_owned(),
                            ));
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let summary = adapter_run.wait().map_err(adapter_error)?;
        let frame = AgentFrame::TurnEnd {
            run_id: run.clone(),
            summary: TurnSummary {
                exit_code: summary.exit_code,
                outcome: summary.outcome,
                native_session_id: summary.native_session_id,
                cost: summary.cost,
                limit: summary.limit,
                result_text: summary.result_text,
                stop_reason: summary.stop_reason,
                usage: None,
            },
        };
        match spool.append(&frame)? {
            SpoolAppend::Written => {}
            SpoolAppend::Full { bytes, cap } => return Ok(ChatTurnStatus::SpoolFull { bytes, cap }),
        }
        match self.send_and_ack(&mut spool, &run, &frame)? {
            Ack::Acked | Ack::Duplicate => Ok(ChatTurnStatus::Done),
            Ack::ReadOnly(reason) => Ok(ChatTurnStatus::ReadOnly { reason }),
            Ack::Failed(message) => Err(AgentError::Remote {
                kind: vowp_protocol::ErrorKind::Conflict,
                message,
                retryable: false,
            }),
            Ack::TransportLost => Err(AgentError::Transport(
                "the home did not acknowledge the summary before the retry budget ran out".to_owned(),
            )),
        }
    }

    /// Releases the lease when the chat surface exits.
    pub fn release(&mut self) -> Result<(), AgentError> {
        let Some(token) = self.lease.take() else {
            return Ok(());
        };
        self.client.send_frame(&AgentFrame::LeaseRelease {
            surface: self.surface.surface_id.clone(),
            session: self.session.clone(),
            token,
        })
    }

    /// Acquires the lease if this surface does not already hold it.
    ///
    /// Returns `false` when the lease is now held by another surface; the caller must stay
    /// read-only and must not begin a turn.
    fn ensure_lease(&mut self) -> Result<bool, AgentError> {
        if self.lease.is_some() {
            return Ok(true);
        }
        self.client.connect()?;
        self.client.send_frame(&AgentFrame::LeaseAcquire {
            surface: self.surface.surface_id.clone(),
            session: self.session.clone(),
            surface_token: self.surface.token.clone(),
            mode: vowp_protocol::LeaseMode::Acquire,
        })?;
        loop {
            match self.client.recv_frame(Some(PAIR_TIMEOUT))? {
                FrameRead::Frame(AgentFrame::LeaseGrant { lease_token, .. }) => {
                    self.lease = Some(lease_token);
                    return Ok(true);
                }
                FrameRead::Frame(AgentFrame::LeaseLost { reason: _ })
                | FrameRead::Frame(AgentFrame::LeaseRevoked { .. }) => {
                    self.lease = None;
                    return Ok(false);
                }
                FrameRead::Frame(AgentFrame::Error {
                    kind,
                    message,
                    retryable,
                }) => {
                    return Err(remote_error(kind, message, retryable));
                }
                FrameRead::Frame(AgentFrame::Revoked { reason }) => return Err(AgentError::Revoked(reason)),
                FrameRead::Frame(AgentFrame::Superseded { connection_id }) => {
                    return Err(AgentError::Superseded(connection_id));
                }
                FrameRead::Timeout => {
                    return Err(AgentError::Transport(
                        "the home did not answer the lease request".to_owned(),
                    ));
                }
                FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the home closed the connection during the lease request".to_owned(),
                    ));
                }
                FrameRead::Frame(_) => {}
            }
        }
    }

    /// Sends one `TurnBegin` and waits for the grant; `None` means the lease is gone.
    fn begin_turn(&mut self, request: &ChatTurnRequest, lease: &str) -> Result<Option<RunId>, AgentError> {
        self.client.send_frame(&AgentFrame::TurnBegin {
            surface: self.surface.surface_id.clone(),
            session: self.session.clone(),
            lease_token: lease.to_owned(),
            prompt: request.prompt.clone(),
            executor: request.executor.to_string(),
            cwd_ref: request.cwd_ref.clone(),
            job: request.job.clone(),
            purpose: request.purpose,
        })?;
        loop {
            match self.client.recv_frame(Some(PAIR_TIMEOUT))? {
                FrameRead::Frame(AgentFrame::TurnGrant { run_id, .. }) => return Ok(Some(run_id)),
                FrameRead::Frame(AgentFrame::LeaseLost { .. }) | FrameRead::Frame(AgentFrame::LeaseRevoked { .. }) => {
                    self.lease = None;
                    return Ok(None);
                }
                FrameRead::Frame(AgentFrame::Error {
                    kind,
                    message,
                    retryable,
                }) => return Err(remote_error(kind, message, retryable)),
                FrameRead::Frame(AgentFrame::RunLost { reason, .. }) => {
                    self.lease = None;
                    return Err(AgentError::Remote {
                        kind: vowp_protocol::ErrorKind::Conflict,
                        message: reason,
                        retryable: false,
                    });
                }
                FrameRead::Frame(AgentFrame::Revoked { reason }) => return Err(AgentError::Revoked(reason)),
                FrameRead::Frame(AgentFrame::Superseded { connection_id }) => {
                    return Err(AgentError::Superseded(connection_id));
                }
                FrameRead::Timeout => {
                    return Err(AgentError::Transport("the home did not answer TurnBegin".to_owned()));
                }
                FrameRead::Closed => {
                    return Err(AgentError::Transport(
                        "the home closed the connection during TurnBegin".to_owned(),
                    ));
                }
                FrameRead::Frame(_) => {}
            }
        }
    }

    /// Sends a lease heartbeat and reports a loss that arrived with it.
    fn heartbeat(&mut self) -> Result<Option<ChatTurnStatus>, AgentError> {
        let Some(token) = self.lease.clone() else {
            return Ok(Some(ChatTurnStatus::ReadOnly {
                reason: "the lease was released".to_owned(),
            }));
        };
        self.client.send_frame(&AgentFrame::LeaseHeartbeat {
            surface: self.surface.surface_id.clone(),
            session: self.session.clone(),
            token,
        })?;
        loop {
            match self.client.recv_frame(Some(Duration::from_millis(50)))? {
                FrameRead::Frame(AgentFrame::LeaseLost { reason }) => {
                    self.lease = None;
                    return Ok(Some(ChatTurnStatus::ReadOnly { reason }));
                }
                FrameRead::Frame(AgentFrame::LeaseRevoked { reason, .. }) => {
                    self.lease = None;
                    return Ok(Some(ChatTurnStatus::ReadOnly { reason }));
                }
                FrameRead::Frame(AgentFrame::RunLost { reason, .. }) => {
                    self.lease = None;
                    return Ok(Some(ChatTurnStatus::ReadOnly { reason }));
                }
                FrameRead::Frame(AgentFrame::Ping) => {
                    self.client.send_frame(&AgentFrame::Pong)?;
                }
                FrameRead::Frame(_) => return Ok(None),
                FrameRead::Timeout | FrameRead::Closed => return Ok(None),
            }
        }
    }

    /// Sends one spooled frame and waits for its acknowledgement.
    fn send_and_ack(&mut self, spool: &mut Spool, run: &RunId, frame: &AgentFrame) -> Result<Ack, AgentError> {
        match self.client.send_frame(frame) {
            Ok(()) => {}
            Err(error) if !error.is_fatal() => {
                let ack = self.reconnect(spool, run)?;
                return self.resend_after_reconnect(spool, run, frame, ack);
            }
            Err(error) => return Err(error),
        }
        match self.wait_for_ack(spool, run)? {
            Ack::TransportLost => {
                let ack = self.reconnect(spool, run)?;
                self.resend_after_reconnect(spool, run, frame, ack)
            }
            ack => Ok(ack),
        }
    }

    /// Sends the current frame again after a replay when the earlier send may not have arrived.
    ///
    /// The frame is already in the spool, so the home either appends it once or reports it as
    /// a duplicate by event id; neither path can append twice.
    fn resend_after_reconnect(
        &mut self,
        spool: &mut Spool,
        run: &RunId,
        frame: &AgentFrame,
        ack: Ack,
    ) -> Result<Ack, AgentError> {
        match ack {
            Ack::Acked | Ack::Duplicate => {
                // The replay may already have carried this frame; only resend when it is
                // still unacknowledged, so a reconnect does not send every line twice.
                if !spool.unacked()?.iter().any(|pending| pending == frame) {
                    return Ok(ack);
                }
                self.client.send_frame(frame)?;
                self.wait_for_ack(spool, run)
            }
            other => Ok(other),
        }
    }

    /// Reconnects, proves the lease is still held and replays the first unacked frame.
    fn reconnect(&mut self, spool: &mut Spool, run: &RunId) -> Result<Ack, AgentError> {
        'attempts: for _ in 0..MAX_RECONNECTS {
            self.client.sleep(Duration::from_millis(100));
            if self.client.connect().is_err() {
                continue;
            }
            let Some(token) = self.lease.clone() else {
                return Ok(Ack::ReadOnly("the lease was released".to_owned()));
            };
            if let Err(error) = self.client.send_frame(&AgentFrame::LeaseHeartbeat {
                surface: self.surface.surface_id.clone(),
                session: self.session.clone(),
                token: token.clone(),
            }) {
                if error.is_fatal() {
                    return Err(error);
                }
                continue;
            }
            // The home pushes a loss on the heartbeat; a short read catches it before the
            // replay, while a quiet connection proceeds to the hello and the replay.
            match self.client.recv_frame(Some(Duration::from_millis(50)))? {
                FrameRead::Frame(AgentFrame::LeaseLost { reason }) => {
                    self.lease = None;
                    return Ok(Ack::ReadOnly(reason));
                }
                FrameRead::Frame(AgentFrame::LeaseRevoked { reason, .. }) => {
                    self.lease = None;
                    return Ok(Ack::ReadOnly(reason));
                }
                FrameRead::Frame(AgentFrame::Ping) => {
                    self.client.send_frame(&AgentFrame::Pong)?;
                }
                FrameRead::Frame(_) | FrameRead::Timeout => {}
                FrameRead::Closed => continue,
            }
            if let Err(error) = self.client.send_frame(&AgentFrame::RemoteLine {
                run_id: run.clone(),
                epoch: CHAT_EPOCH,
                line: RemoteLine::Hello {
                    session_id: self.session.clone(),
                    run_id: run.clone(),
                    executor: String::new(),
                    node: self.surface_node(),
                    spool_id: format!("{}-{}", self.session, run),
                    lease: token,
                },
            }) {
                if error.is_fatal() {
                    return Err(error);
                }
                continue;
            }
            let frames = spool.unacked()?;
            for frame in frames {
                match self.client.send_frame(&frame) {
                    Ok(()) => {}
                    Err(error) if !error.is_fatal() => continue 'attempts,
                    Err(error) => return Err(error),
                }
                match self.wait_for_ack(spool, run)? {
                    Ack::Acked | Ack::Duplicate => {}
                    Ack::TransportLost => continue 'attempts,
                    ack => return Ok(ack),
                }
            }
            return Ok(Ack::Acked);
        }
        Err(AgentError::Transport(format!(
            "could not reconnect after {MAX_RECONNECTS} attempts; the spool is kept at {}",
            spool.dir().display()
        )))
    }

    /// Waits for one acknowledgement for `run`.
    fn wait_for_ack(&mut self, spool: &mut Spool, run: &RunId) -> Result<Ack, AgentError> {
        loop {
            match self.client.recv_frame(Some(EVENT_POLL)) {
                Ok(FrameRead::Frame(AgentFrame::IngestAck {
                    run_id,
                    event_id,
                    seq,
                    duplicate,
                    lease_revoked,
                    error,
                })) => {
                    if run_id != *run {
                        continue;
                    }
                    if let Some(reason) = lease_revoked {
                        self.lease = None;
                        return Ok(Ack::ReadOnly(reason));
                    }
                    if let Some(message) = error {
                        return Ok(Ack::Failed(message));
                    }
                    match (event_id, seq) {
                        (Some(id), Some(seq)) => spool.mark_acked(id.as_str(), seq)?,
                        (None, _) => spool.mark_summary_acked()?,
                        _ => {}
                    }
                    return Ok(if duplicate { Ack::Duplicate } else { Ack::Acked });
                }
                Ok(FrameRead::Frame(AgentFrame::Credit { run_id: credit_run, .. })) => {
                    if credit_run != *run {
                        continue;
                    }
                    // The home opens the next window only after the agent confirms it
                    // consumed this Credit. A lease heartbeat is a bound, ordered
                    // round-trip on the run's session, so use it as the confirmation.
                    if let Some(token) = self.lease.clone() {
                        self.client.send_frame(&AgentFrame::LeaseHeartbeat {
                            surface: self.surface.surface_id.clone(),
                            session: self.session.clone(),
                            token,
                        })?;
                    }
                }
                Ok(FrameRead::Frame(AgentFrame::Ping)) => {
                    self.client.send_frame(&AgentFrame::Pong)?;
                }
                Ok(FrameRead::Frame(AgentFrame::LeaseLost { reason })) => {
                    self.lease = None;
                    return Ok(Ack::ReadOnly(reason));
                }
                Ok(FrameRead::Frame(AgentFrame::LeaseRevoked { reason, .. })) => {
                    self.lease = None;
                    return Ok(Ack::ReadOnly(reason));
                }
                Ok(FrameRead::Frame(AgentFrame::RunLost { reason, .. })) => {
                    self.lease = None;
                    return Ok(Ack::ReadOnly(reason));
                }
                Ok(FrameRead::Frame(AgentFrame::Error {
                    kind,
                    message,
                    retryable,
                })) => {
                    if retryable && kind == vowp_protocol::ErrorKind::Conflict {
                        // The home refused this event at the flow-control window. A
                        // reconnect starts a fresh stream and replay follows.
                        return Ok(Ack::TransportLost);
                    }
                    if matches!(
                        kind,
                        vowp_protocol::ErrorKind::Refused | vowp_protocol::ErrorKind::Conflict
                    ) {
                        self.lease = None;
                        return Ok(Ack::ReadOnly(message));
                    }
                    return Err(remote_error(kind, message, retryable));
                }
                Ok(FrameRead::Frame(AgentFrame::Revoked { reason })) => return Err(AgentError::Revoked(reason)),
                Ok(FrameRead::Frame(AgentFrame::Superseded { connection_id })) => {
                    return Err(AgentError::Superseded(connection_id));
                }
                Ok(FrameRead::Frame(_)) => {}
                Ok(FrameRead::Timeout) => return Ok(Ack::TransportLost),
                Ok(FrameRead::Closed) => return Ok(Ack::TransportLost),
                Err(error) if error.is_fatal() => return Err(error),
                Err(_) => return Ok(Ack::TransportLost),
            }
        }
    }

    /// Builds the normalized remote event for one harness event.
    fn event_line(&self, event: &HarnessEvent, executor: &Executor) -> RemoteEvent {
        let replayable = !matches!(event.body, EventBody::AssistantReasoning(_));
        RemoteEvent {
            id: EventId::new(),
            ts: event.ts.unwrap_or_else(Timestamp::now),
            event_type: event.body.event_type(),
            actor: event.actor,
            source: EventSource {
                harness: executor.harness,
                model: Some(executor.model.clone()),
                node: Some(self.surface_node()),
                native_session_id: None,
            },
            payload: event.body.to_value(),
            raw: event.raw.clone(),
            replayable,
        }
    }

    /// Returns the node this agent runs on.
    fn surface_node(&self) -> vowp_protocol::NodeName {
        // invariant: `AgentClient::from_state_dir` validated `agent.json`, which always
        // carries a parsed node name.
        self.client.node().clone()
    }
}

/// The result of one send-and-wait.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ack {
    /// The home appended the frame.
    Acked,
    /// The home already had the frame.
    Duplicate,
    /// The lease is gone; the surface must stop.
    ReadOnly(String),
    /// The home rejected the frame with a content or policy error.
    Failed(String),
    /// The transport dropped before an acknowledgement arrived.
    TransportLost,
}

/// Maps a home error frame to the agent error.
fn remote_error(kind: vowp_protocol::ErrorKind, message: String, retryable: bool) -> AgentError {
    if kind == vowp_protocol::ErrorKind::Auth {
        AgentError::Auth(message)
    } else {
        AgentError::Remote {
            kind,
            message,
            retryable,
        }
    }
}

/// Maps an adapter failure onto the agent's remote-error shape.
fn adapter_error(error: AdapterError) -> AgentError {
    AgentError::Remote {
        kind: vowp_protocol::ErrorKind::AdapterFailed,
        message: error.to_string(),
        retryable: false,
    }
}

/// Returns the local spool directory for one run (for status and test assertions).
pub fn spool_dir(state_dir: &Path, session: &SessionId, run: &RunId) -> PathBuf {
    crate::spool::spool_dir(state_dir, session, run)
}
