//! Remote execution: begin a turn on the home and ingest the executor's lines.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §7.2 and §13.2. The home owns the log and
//! assigns every sequence number; the executor only supplies stable event ids. A dropped
//! transport is replayed by the client, and ingest deduplicates by event id.
//!
//! [`begin_turn`] writes the framing events and the run row. [`IngestSession`] is the
//! line-at-a-time state machine behind [`ingest_lines`]; the CLI uses it directly so it can
//! read one line and answer on stdout without buffering the whole stream.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use vowp_protocol::{
    Actor, AttachAction, BLOB_SPILL_BYTES, CostEvent, ErrorEvent, ErrorKind, EventBody, EventEnvelope, EventSource,
    EventType, Executor, ExecutorRef, HarnessKind, Limit, LimitEvent, LimitRowKind, NodeName, RemoteEvent, RemoteLine,
    RemoteUsage, Run, RunId, RunOutcome, RunPurpose, RunRole, RunUsage, Session, SessionExecutor, SessionId,
    SessionRole, SessionState, StopReason, Timestamp, TurnEnded, TurnGrant, TurnStarted, UserMessage,
};
use vowp_store::session_files::{AdapterMapping, add_adapter_mapping};
use vowp_store::session_log::{AppendedEvent, NewEvent, SessionLog, index_row};
use vowp_store::{RunFinish, Store};

use crate::CoreError;
use crate::deps::Deps;
use crate::session::bundle;
use crate::session::seed::seed_prompt;
use crate::session::service::{native_for_node, tail_of_complete_turns};

/// How long an ingest waits for a previous writer of the same session log.
const WRITER_WAIT: Duration = Duration::from_secs(5);

/// Poll interval while waiting for the session log writer.
const WRITER_POLL: Duration = Duration::from_millis(20);

/// Safety margin added once when a provider limit is written to `limits`.
const LIMIT_MARGIN: Duration = Duration::from_secs(120);

/// The grant plus the executor-side spool directory.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTurn {
    /// The grant returned to the surface.
    pub grant: TurnGrant,
    /// Directory that holds `events.jsonl` and `acked.json`.
    pub spool_dir: PathBuf,
}

/// A request to begin a remote turn.
#[derive(Debug, Clone, PartialEq)]
pub struct BeginTurn {
    /// Session to continue.
    pub session: SessionId,
    /// Lease token presented by the surface.
    pub lease: String,
    /// Executor the surface wants to run.
    pub executor: ExecutorRef,
    /// Purpose of the run.
    pub purpose: RunPurpose,
    /// Executor node.
    pub node: NodeName,
    /// Working directory on the surface, when it differs from the home's session cwd.
    ///
    /// The path belongs to the executor node, so the home never requires it to exist here.
    /// `vowp __exec` validates it on the machine that will actually run the harness.
    pub cwd: Option<String>,
    /// Prompt to send.
    pub prompt: String,
}

/// The home-side grant used while ingesting one run.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestGrant {
    /// Session the run belongs to.
    pub session: SessionId,
    /// Expected lease token; empty means "take the token from `RemoteLine::Hello`".
    pub lease: String,
    /// Run id.
    pub run: RunId,
}

/// What one ingest pass processed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Events appended.
    pub events: u64,
    /// Events skipped as duplicates.
    pub duplicates: u64,
    /// Whether the run summary was accepted.
    pub done: bool,
}

/// Writes the framing events for a remote turn and returns its grant.
pub fn begin_turn(deps: &Deps, store: &Store, req: &BeginTurn) -> Result<TurnGrant, CoreError> {
    let session = store.require_session(&req.session)?;
    if session.state == SessionState::Switching {
        return Err(CoreError::Store(vowp_store::StoreError::Conflict(
            "session is switching; input is rejected".to_owned(),
        )));
    }
    if session.state == SessionState::Closed {
        return Err(CoreError::Invalid("session is closed; no further turns".to_owned()));
    }
    // The cwd belongs to the executor node; `__exec` runs there and checks its own paths.
    // The home must not reject a valid remote cwd because the laptop uses another root.
    // A live lease must match the token presented by the surface.
    crate::surface::check_lease_with(store, &req.session, Some(&req.lease))?;

    let executor = req.executor.executor.clone();
    let node = req.node.clone();
    // The home's cwd for this session names its own checkout, which may not exist on a
    // surface that runs a different worktree. The request may therefore carry the
    // surface's absolute path; the home records it without probing the surface filesystem
    // and `__exec` checks it where the harness will actually start.
    let cwd = match &req.cwd {
        Some(cwd) if !cwd.is_empty() => {
            if !std::path::Path::new(cwd).is_absolute() {
                return Err(CoreError::Invalid(format!("surface cwd must be absolute: {cwd}")));
            }
            cwd.clone()
        }
        _ => session.cwd.clone(),
    };
    let prior = SessionLog::open_read(&deps.paths, &req.session)?.read_all()?;
    let turn = prior
        .iter()
        .filter(|event| event.event_type == EventType::TurnStarted)
        .count()
        .saturating_add(1) as u32;
    // Mirror `run_turn`: native resume is only requested from a harness that supports it,
    // and only for the node that owns the run. A native id recorded on the home must never
    // be resumed by a turn that executes on another machine.
    let can_resume_native = deps
        .adapters
        .get(&executor)
        .is_some_and(|adapter| adapter.capabilities().can_resume_native);
    let native = if can_resume_native {
        native_for_node(store, &req.session, executor.harness, &node)?
    } else {
        None
    };
    // An imported child keeps the bundle tail out of its log, so its first turn seeds from
    // the sidecar written by `bundle::import` (sessions.md §6.3 step 5). This is the same
    // precedence as `run_turn`: no resumable native id means the bundle seed comes first,
    // then the latest capsule.
    let imported = if native.is_none() {
        bundle::pending_import_seed(deps, &req.session, &req.prompt)?
    } else {
        None
    };
    let imported_used = imported.is_some();
    let (prompt, handoff) = if let Some(seed) = imported {
        (seed, true)
    } else if native.is_none() {
        match store.latest_capsule(&req.session)? {
            Some(capsule) => {
                let tail = tail_of_complete_turns(&prior, deps.config.capsule.tail_turns);
                (
                    seed_prompt(&capsule, &tail, deps.config.capsule.tail_turns, &req.prompt),
                    true,
                )
            }
            None => (req.prompt.clone(), false),
        }
    } else {
        (req.prompt.clone(), false)
    };

    let run_id = RunId::new();
    let run = Run {
        id: run_id.clone(),
        job_id: session.job_id.clone(),
        session_id: Some(req.session.clone()),
        role: run_role(session.role),
        purpose: req.purpose,
        profile: None,
        rung: 0,
        executor: Some(executor.clone()),
        provider: Some(executor.provider()),
        node: node.clone(),
        native_session_id: native.clone(),
        pid: None,
        window: None,
        cwd: Some(cwd.clone()),
        started_at: deps.clock.now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: vowp_protocol::Cost::unknown(),
        detail: String::new(),
    };

    let log = open_log_wait(deps, &req.session, WRITER_WAIT)?;
    append_event(
        deps,
        store,
        &log,
        executor.harness,
        EventBody::SessionExecutor(SessionExecutor {
            action: AttachAction::Attach,
            executor: executor.clone(),
            node: node.clone(),
            native_session_id: native.clone(),
            run_id: Some(run_id.clone()),
            reason: None,
        }),
        Some(node.clone()),
        native.clone(),
    )?;
    append_event(
        deps,
        store,
        &log,
        executor.harness,
        EventBody::TurnStarted(TurnStarted { turn }),
        Some(node.clone()),
        native.clone(),
    )?;
    append_event(
        deps,
        store,
        &log,
        executor.harness,
        EventBody::UserMessage(UserMessage {
            text: prompt.clone(),
            attachments: Vec::new(),
            note: handoff.then(|| "handoff".to_owned()),
        }),
        Some(node.clone()),
        native.clone(),
    )?;
    if imported_used {
        bundle::clear_pending_import(deps, &req.session)?;
    }
    log.sync()?;
    store.insert_run(&run)?;
    store.set_session_state(&req.session, SessionState::Running, Some(&executor))?;
    let tail_seq = log.last_seq();
    drop(log);

    Ok(TurnGrant {
        run_id,
        session_id: req.session.clone(),
        executor: executor.to_string(),
        purpose: req.purpose,
        node,
        cwd,
        prompt,
        resume_native: native.clone(),
        native_hint: native.is_none().then(|| fresh_native_id(deps)),
        env_names: env_names(deps, &executor),
        lease_token: req.lease.clone(),
        tail_seq,
    })
}

/// Reads remote lines and appends their events to the home session log.
///
/// `grant.lease` is the token the caller expects; an empty value accepts the token carried
/// by the first `RemoteLine::Hello` (the CLI never receives it on argv). Every event is
/// checked through the lease hook, so a live lease is enforced without touching this
/// transport.
pub fn ingest_lines(
    deps: &Deps,
    store: &Store,
    grant: &IngestGrant,
    reader: &mut dyn BufRead,
    ack: &mut dyn Write,
) -> Result<IngestReport, CoreError> {
    let mut session = IngestSession::begin(deps, store, grant)?;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|source| CoreError::Io {
            path: format!("remote ingest {}", grant.session),
            source,
        })?;
        if read == 0 {
            break;
        }
        if session.line(&line, ack)? {
            break;
        }
    }
    session.finish()
}

/// The line-at-a-time home side of remote ingest.
pub struct IngestSession<'a> {
    deps: &'a Deps,
    store: &'a Store,
    grant: IngestGrant,
    log: SessionLog,
    hello: Option<String>,
    lease_seen: bool,
    done: bool,
    events: u64,
    duplicates: u64,
}

impl<'a> IngestSession<'a> {
    /// Validates the run/session pair, then opens the session log.
    ///
    /// The `--run` argument is often assembled by hand or by an older surface, so the
    /// check happens before the first event is accepted rather than when the summary
    /// arrives. A run that does not exist or belongs to another session must not write
    /// into this log.
    pub fn begin(deps: &'a Deps, store: &'a Store, grant: &IngestGrant) -> Result<Self, CoreError> {
        let run = store.require_run(&grant.run)?;
        if run.session_id.as_ref() != Some(&grant.session) {
            return Err(CoreError::Invalid(format!(
                "run {} does not belong to session {}",
                grant.run, grant.session
            )));
        }
        let log = open_log_wait(deps, &grant.session, WRITER_WAIT)?;
        Ok(Self {
            deps,
            store,
            grant: grant.clone(),
            log,
            hello: None,
            lease_seen: false,
            done: false,
            events: 0,
            duplicates: 0,
        })
    }

    /// Processes one JSON line and writes its acknowledgement(s).
    ///
    /// Returns `true` when the run summary was accepted and the caller should stop reading.
    /// A line without a trailing newline is a torn transport tail and is ignored.
    pub fn line(&mut self, line: &str, ack: &mut dyn Write) -> Result<bool, CoreError> {
        if self.done {
            return Ok(true);
        }
        if self.hello.is_some() && self.check_revocation(ack)? {
            return Ok(true);
        }
        if !line.ends_with('\n') {
            // A dropped SSH connection can cut the last line mid-JSON. It was never a
            // complete frame; the client resends it from its spool.
            return Ok(false);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        let remote: RemoteLine = match serde_json::from_str(trimmed) {
            Ok(line) => line,
            Err(error) => {
                write_ack(
                    ack,
                    &vowp_protocol::IngestAck::Error {
                        kind: ErrorKind::Parse,
                        message: format!("invalid remote line: {error}"),
                    },
                )?;
                return Err(CoreError::Invalid(format!("invalid remote line: {error}")));
            }
        };
        match remote {
            RemoteLine::Hello {
                session_id,
                run_id,
                executor: _,
                node: _,
                spool_id: _,
                lease,
            } => {
                if self.hello.is_some() {
                    return Err(CoreError::Invalid("duplicate hello frame".to_owned()));
                }
                if session_id != self.grant.session {
                    return Err(CoreError::Invalid(format!(
                        "hello names session {session_id}, expected {}",
                        self.grant.session
                    )));
                }
                if run_id != self.grant.run {
                    return Err(CoreError::Invalid(format!(
                        "hello names run {run_id}, expected {}",
                        self.grant.run
                    )));
                }
                if !self.grant.lease.is_empty() && lease != self.grant.lease {
                    return Err(CoreError::Refused("lease token does not match the grant".to_owned()));
                }
                crate::surface::check_lease_with(self.store, &self.grant.session, Some(&lease))?;
                self.lease_seen = self.store.lease(&self.grant.session)?.is_some();
                self.hello = Some(lease);
                Ok(false)
            }
            RemoteLine::Event { event } => {
                self.require_hello()?;
                self.ingest_event(event, ack)?;
                Ok(false)
            }
            RemoteLine::Heartbeat { at: _ } => {
                self.require_hello()?;
                Ok(false)
            }
            RemoteLine::Summary {
                exit_code,
                outcome,
                native_session_id,
                cost,
                limit,
                result_text,
                stop_reason,
                usage,
            } => {
                self.require_hello()?;
                self.finish_summary(
                    ack,
                    exit_code,
                    outcome,
                    native_session_id,
                    cost,
                    limit,
                    result_text,
                    stop_reason,
                    usage,
                )?;
                self.done = true;
                Ok(true)
            }
        }
    }

    /// Flushes the log and returns what the pass processed.
    pub fn finish(self) -> Result<IngestReport, CoreError> {
        self.log.sync()?;
        Ok(IngestReport {
            events: self.events,
            duplicates: self.duplicates,
            done: self.done,
        })
    }

    /// Returns the report without consuming the session; used by tests and callers that
    /// keep the state machine alive after the last line.
    pub fn report(&self) -> IngestReport {
        IngestReport {
            events: self.events,
            duplicates: self.duplicates,
            done: self.done,
        }
    }

    /// Requires a validated hello before any event or summary is accepted.
    fn require_hello(&self) -> Result<(), CoreError> {
        if self.hello.is_none() {
            return Err(CoreError::Invalid(
                "remote stream must start with a hello frame".to_owned(),
            ));
        }
        Ok(())
    }

    /// Writes `LeaseRevoked` and stops when the shared lease row no longer carries the
    /// token from the hello frame.
    ///
    /// This is the cross-process push: the takeover command (and the sweep) changes the
    /// lease row, which `vowp session ingest` reads here on every line and one-second
    /// heartbeat. A lease that was absent at hello is a job session and stays accepted.
    fn check_revocation(&mut self, ack: &mut dyn Write) -> Result<bool, CoreError> {
        if !self.lease_seen {
            return Ok(false);
        }
        let Some(token) = self.hello.as_deref() else {
            return Ok(false);
        };
        let Some(frame) =
            crate::surface::lease::revocation_ack_for(self.store, &self.grant.session, token, self.log.last_seq())?
        else {
            return Ok(false);
        };
        write_ack(ack, &frame)?;
        self.done = true;
        Ok(true)
    }

    /// Appends one remote event or acknowledges its existing id.
    fn ingest_event(&mut self, event: RemoteEvent, ack: &mut dyn Write) -> Result<(), CoreError> {
        crate::surface::check_lease_with(self.store, &self.grant.session, self.hello.as_deref())?;
        if let Some(row) = self.store.event_row_by_id(&event.id)? {
            if row.session_id != self.grant.session {
                let message = format!("event id {} belongs to another session", event.id);
                write_ack(
                    ack,
                    &vowp_protocol::IngestAck::Error {
                        kind: ErrorKind::Conflict,
                        message: message.clone(),
                    },
                )?;
                return Err(CoreError::Invalid(message));
            }
            let existing =
                SessionLog::open_read(&self.deps.paths, &self.grant.session)?.read_at(row.byte_offset, row.byte_len)?;
            if same_event(self.deps, &self.grant.session, &existing, &event)? {
                self.duplicates += 1;
                write_ack(
                    ack,
                    &vowp_protocol::IngestAck::Duplicate {
                        event_id: event.id,
                        seq: row.seq,
                    },
                )?;
                return Ok(());
            }
            let message = format!("duplicate-conflict: event id {} has different content", event.id);
            write_ack(
                ack,
                &vowp_protocol::IngestAck::Error {
                    kind: ErrorKind::Conflict,
                    message: message.clone(),
                },
            )?;
            return Err(CoreError::Invalid(message));
        }

        let body = EventBody::from_parts(event.event_type, event.payload.clone())?;
        let new = NewEvent {
            actor: event.actor,
            source: event.source,
            body,
            raw: event.raw,
            ts: Some(event.ts),
        };
        let appended = self
            .log
            .append_with_id(crate::vault::redact_event(self.deps, new)?, event.id.clone())?;
        self.store.index_event(&index_row(&appended))?;
        self.events += 1;
        write_ack(
            ack,
            &vowp_protocol::IngestAck::Ack {
                event_id: event.id,
                seq: appended.envelope.seq,
            },
        )?;
        Ok(())
    }

    /// Finishes the run once and acknowledges the summary; a replay is a no-op.
    #[allow(clippy::too_many_arguments)]
    fn finish_summary(
        &mut self,
        ack: &mut dyn Write,
        exit_code: Option<i32>,
        outcome: RunOutcome,
        native_session_id: Option<String>,
        cost: vowp_protocol::Cost,
        limit: Option<LimitEvent>,
        result_text: Option<String>,
        stop_reason: StopReason,
        usage: Option<RemoteUsage>,
    ) -> Result<(), CoreError> {
        let run = self.store.require_run(&self.grant.run)?;
        if run.ended_at.is_some() {
            write_ack(
                ack,
                &vowp_protocol::IngestAck::Summary {
                    run_id: self.grant.run.clone(),
                    done: true,
                },
            )?;
            return Ok(());
        }
        let session = self.store.require_session(&self.grant.session)?;
        let executor = run.executor.clone();
        let harness = executor.as_ref().map_or(HarnessKind::Vowp, |executor| executor.harness);
        let node = run.node.clone();
        let prior = SessionLog::open_read(&self.deps.paths, &self.grant.session)?.read_all()?;
        let turn = prior
            .iter()
            .filter(|event| event.event_type == EventType::TurnStarted)
            .count() as u32;
        let run_start = prior
            .iter()
            .position(|event| {
                matches!(
                    event.body(),
                    Ok(EventBody::SessionExecutor(payload))
                        if payload.action == AttachAction::Attach
                            && payload.run_id.as_ref() == Some(&self.grant.run)
                )
            })
            .unwrap_or(0);
        let run_events = &prior[run_start..];
        // Harness cost events carry no run id (`CostEvent { run_id: None }`), so the run
        // slice — which starts at this run's attach — is the authority: any cost after the
        // attach belongs to this run and the summary must not append a second one.
        let saw_cost = run_events.iter().any(|event| event.event_type == EventType::Cost);
        let saw_limit = run_events.iter().any(|event| event.event_type == EventType::Limit);
        let saw_error = run_events.iter().any(|event| event.event_type == EventType::Error);
        // A home crash after appending `turn.ended` but before `finish_run` leaves the run
        // row open. The terminal event is the marker that the turn was already closed, so a
        // replayed summary must finish the row without appending the framing events again.
        let saw_terminal = run_events.iter().any(|event| event.event_type == EventType::TurnEnded);
        let saw_detach = run_events.iter().any(|event| {
            matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(payload))
                    if payload.action == AttachAction::Detach
                        && payload.run_id.as_ref() == Some(&self.grant.run)
            )
        });

        let ended_at = self.deps.clock.now();
        if !saw_terminal {
            if cost.usd.is_some() && !saw_cost {
                append_event(
                    self.deps,
                    self.store,
                    &self.log,
                    harness,
                    EventBody::Cost(CostEvent {
                        cost: cost.clone(),
                        run_id: Some(self.grant.run.clone()),
                    }),
                    Some(node.clone()),
                    native_session_id.clone(),
                )?;
            }
            if let Some(limit) = &limit {
                if !saw_limit {
                    append_event(
                        self.deps,
                        self.store,
                        &self.log,
                        harness,
                        EventBody::Limit(limit.clone()),
                        Some(node.clone()),
                        native_session_id.clone(),
                    )?;
                }
            } else if !saw_error && matches!(outcome, RunOutcome::Error | RunOutcome::Crash) {
                append_event(
                    self.deps,
                    self.store,
                    &self.log,
                    harness,
                    EventBody::Error(ErrorEvent {
                        kind: ErrorKind::AdapterFailed,
                        message: vowp_adapters::classify::redact(
                            result_text.as_deref().unwrap_or("adapter ended without a result"),
                            300,
                        ),
                        retryable: outcome == RunOutcome::Crash,
                    }),
                    Some(node.clone()),
                    native_session_id.clone(),
                )?;
            }

            let duration_ms = u64::try_from(ended_at.since(run.started_at).as_millis()).unwrap_or(u64::MAX);
            append_event(
                self.deps,
                self.store,
                &self.log,
                harness,
                EventBody::TurnEnded(TurnEnded {
                    turn,
                    stop_reason,
                    duration_ms: Some(duration_ms),
                }),
                Some(node.clone()),
                native_session_id.clone(),
            )?;
        }
        if !saw_detach {
            let detach_reason = match limit {
                Some(_) => "limit",
                None if outcome == RunOutcome::Crash => "crash",
                None => "turn_done",
            };
            append_event(
                self.deps,
                self.store,
                &self.log,
                harness,
                EventBody::SessionExecutor(SessionExecutor {
                    action: AttachAction::Detach,
                    executor: executor.clone().unwrap_or_else(unknown_executor),
                    node: node.clone(),
                    native_session_id: native_session_id.clone(),
                    run_id: Some(self.grant.run.clone()),
                    reason: Some(detach_reason.to_owned()),
                }),
                Some(node.clone()),
                native_session_id.clone(),
            )?;
        }
        self.log.sync()?;

        if let Some(native) = &native_session_id {
            let attach_seq = prior
                .iter()
                .find_map(|event| match event.body() {
                    Ok(EventBody::SessionExecutor(payload))
                        if payload.action == AttachAction::Attach
                            && payload.run_id.as_ref() == Some(&self.grant.run) =>
                    {
                        Some(event.seq)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| self.log.last_seq());
            self.store
                .upsert_native(&self.grant.session, harness, native, &node, attach_seq)?;
            add_adapter_mapping(
                &self.deps.paths,
                &self.grant.session,
                AdapterMapping {
                    harness,
                    native_session_id: native.clone(),
                    node: node.clone(),
                    first_seq: attach_seq,
                    created_at: self.deps.clock.now(),
                },
            )?;
        }

        let state = if matches!(outcome, RunOutcome::Limit | RunOutcome::Credits | RunOutcome::Auth) {
            SessionState::Blocked
        } else {
            SessionState::Idle
        };
        self.store
            .set_session_state(&self.grant.session, state, executor.as_ref())?;
        let finish = RunFinish {
            outcome,
            exit_code,
            limit_kind: limit.as_ref().map(|limit| limit.kind),
            cost: cost.clone(),
            native_session_id: native_session_id.clone(),
            detail: vowp_adapters::classify::redact(
                limit
                    .as_ref()
                    .map(|limit| limit.message.clone())
                    .or(result_text)
                    .unwrap_or_default()
                    .as_str(),
                1500,
            ),
        };
        self.store.finish_run(&self.grant.run, &finish)?;

        if let Some(limit) = &limit {
            record_limit(self.store, limit, harness)?;
        } else if outcome == RunOutcome::Ok {
            if let Some(executor) = &executor {
                self.store.clear_limit(&executor.provider())?;
            }
        }

        let usage = remote_usage_row(self.deps, &session, &run, usage, ended_at, exit_code);
        let usage_value = serde_json::to_value(&usage).unwrap_or(serde_json::Value::Null);
        self.record_cost_ledger(&run, &cost, &usage_value, outcome)?;
        crate::capacity::usage::record(self.store, &usage)?;
        // Redact the home-side copy of the report and raw mirror.
        let _ = crate::vault::redact_job_artifacts(self.deps, self.store, run.job_id.as_ref());
        write_ack(
            ack,
            &vowp_protocol::IngestAck::Summary {
                run_id: self.grant.run.clone(),
                done: true,
            },
        )?;
        Ok(())
    }

    /// Appends the cost ledger row, with the executor-measured usage in its detail.
    fn record_cost_ledger(
        &self,
        run: &Run,
        cost: &vowp_protocol::Cost,
        usage: &serde_json::Value,
        outcome: RunOutcome,
    ) -> Result<(), CoreError> {
        let mut detail = serde_json::json!({"billed": true, "basis": "receipt", "usage": usage});
        if run
            .executor
            .as_ref()
            .is_some_and(|executor| executor.harness == HarnessKind::ClaudeCode)
        {
            detail = serde_json::json!({"billed": false, "basis": "list", "usage": usage});
        }
        self.store.append_ledger(&vowp_protocol::LedgerEntry {
            id: None,
            ts: self.deps.clock.now(),
            kind: vowp_protocol::LedgerKind::Cost,
            project: None,
            job_id: run.job_id.clone(),
            run_id: Some(run.id.clone()),
            session_id: run.session_id.clone(),
            role: Some(run.role),
            provider: run.provider.clone(),
            harness: run.executor.as_ref().map(|executor| executor.harness),
            model: run.executor.as_ref().map(|executor| executor.model.clone()),
            cost: Some(cost.clone()),
            summary: format!("remote run {} finished: {:?}", run.id, outcome),
            detail,
        })?;
        Ok(())
    }
}

/// Opens the session log, waiting for a previous ingest writer to release it.
fn open_log_wait(deps: &Deps, session: &SessionId, timeout: Duration) -> Result<SessionLog, CoreError> {
    let deadline = Instant::now() + timeout;
    loop {
        match SessionLog::open(&deps.paths, session, deps.clock.clone()) {
            Ok(log) => return Ok(log),
            Err(vowp_store::StoreError::Locked { .. }) if Instant::now() < deadline => {
                thread::sleep(WRITER_POLL);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Returns whether a stored envelope carries the same content as a replayed remote event.
///
/// A large tool result is stored with its output spilled to a blob, so the stored payload
/// carries an `output_ref` while the client's replay still carries the inline `output`.
/// Normalising the replay through the same blob store makes a lost ack a duplicate, not a
/// content conflict.
fn same_event(
    deps: &Deps,
    session: &SessionId,
    envelope: &EventEnvelope,
    event: &RemoteEvent,
) -> Result<bool, CoreError> {
    if envelope.id != event.id
        || envelope.event_type != event.event_type
        || envelope.actor != event.actor
        || envelope.source != event.source
        || envelope.ts != event.ts
    {
        return Ok(false);
    }
    let body = EventBody::from_parts(event.event_type, event.payload.clone())?;
    if envelope.payload == body.to_value() {
        return Ok(true);
    }
    let EventBody::ToolResult(result) = &body else {
        return Ok(false);
    };
    let Some(output) = &result.output else {
        return Ok(false);
    };
    if output.len() <= BLOB_SPILL_BYTES {
        return Ok(false);
    }
    let blobs = vowp_store::blobs::BlobStore::new(deps.paths.blobs_dir(session));
    let mut normalised = body.clone();
    if let EventBody::ToolResult(inner) = &mut normalised {
        inner.output = None;
        inner.output_ref = Some(blobs.put(output.as_bytes())?);
    }
    Ok(envelope.payload == normalised.to_value())
}

/// Writes one JSON acknowledgement line and flushes it.
fn write_ack(ack: &mut dyn Write, value: &vowp_protocol::IngestAck) -> Result<(), CoreError> {
    let mut line = serde_json::to_vec(value).map_err(|error| CoreError::Command(error.to_string()))?;
    line.push(b'\n');
    ack.write_all(&line).map_err(|source| CoreError::Io {
        path: "remote ingest acknowledgement".to_owned(),
        source,
    })?;
    ack.flush().map_err(|source| CoreError::Io {
        path: "remote ingest acknowledgement".to_owned(),
        source,
    })?;
    Ok(())
}

/// Appends one event with the canonical actor and indexes it.
fn append_event(
    deps: &Deps,
    store: &Store,
    log: &SessionLog,
    harness: HarnessKind,
    body: EventBody,
    node: Option<NodeName>,
    native: Option<String>,
) -> Result<AppendedEvent, CoreError> {
    let appended = log.append(
        // Redact exact values before the log line is written.
        crate::vault::redact_event(
            deps,
            NewEvent {
                actor: actor_for(&body),
                source: EventSource {
                    harness,
                    model: None,
                    node,
                    native_session_id: native,
                },
                body,
                raw: None,
                ts: None,
            },
        )?,
    )?;
    store.index_event(&index_row(&appended))?;
    Ok(appended)
}

/// Builds the `RunUsage` row a summary carries, defaulting to a conservative row.
fn remote_usage_row(
    deps: &Deps,
    session: &Session,
    run: &Run,
    usage: Option<RemoteUsage>,
    ended_at: Timestamp,
    exit_code: Option<i32>,
) -> RunUsage {
    let project = session
        .project
        .clone()
        .unwrap_or_else(|| deps.config.project.name.clone());
    let Some(usage) = usage else {
        let mut row = crate::capacity::usage::default_for(run, &project, None, ended_at);
        row.exit_code = exit_code;
        return row;
    };
    let executor = run.executor.as_ref();
    RunUsage {
        run_id: run.id.clone(),
        project,
        job_id: run.job_id.clone(),
        job_kind: None,
        role: run.role,
        harness: executor.map(|executor| executor.harness),
        model: executor.map(|executor| executor.model.clone()),
        provider: run.provider.clone(),
        node: run.node.clone(),
        started_at: run.started_at,
        ended_at,
        duration_ms: u64::try_from(ended_at.since(run.started_at).as_millis()).unwrap_or(u64::MAX),
        peak_rss_bytes: usage.peak_rss_bytes,
        anon_peak_bytes: usage.anon_peak_bytes,
        file_peak_bytes: usage.file_peak_bytes,
        kernel_peak_bytes: usage.kernel_peak_bytes,
        cpu_usec: usage.cpu_usec,
        mean_cpu_cores: usage.mean_cpu_cores,
        max_cpu_cores: usage.max_cpu_cores,
        build_mode: usage.build_mode,
        source: usage.source,
        oom_killed: usage.oom_killed,
        exit_code,
    }
}

/// Returns the canonical actor for a framing event.
fn actor_for(body: &EventBody) -> Actor {
    match body {
        EventBody::UserMessage(_) => Actor::User,
        EventBody::AssistantMessage(_)
        | EventBody::AssistantReasoning(_)
        | EventBody::ToolCall(_)
        | EventBody::ToolResult(_)
        | EventBody::FileChange(_)
        | EventBody::Plan(_) => Actor::Assistant,
        _ => Actor::System,
    }
}

/// Maps a session role to the run role.
fn run_role(role: Option<SessionRole>) -> RunRole {
    match role {
        Some(SessionRole::Planner) => RunRole::Planner,
        Some(SessionRole::Keeper) => RunRole::Keeper,
        Some(SessionRole::Verifier) => RunRole::Verifier,
        Some(SessionRole::Worker | SessionRole::User | SessionRole::Orchestrator) | None => RunRole::Worker,
    }
}

/// Returns the configured harness entry for `executor`.
fn harness_config<'a>(deps: &'a Deps, executor: &Executor) -> Option<&'a vowp_protocol::HarnessConfig> {
    let key = match executor.harness {
        HarnessKind::ClaudeCode => "claude-code",
        HarnessKind::Opencode => "opencode",
        HarnessKind::Cli => executor.model.as_str(),
        HarnessKind::Vowp => return None,
    };
    deps.config
        .harnesses
        .get(key)
        .or_else(|| deps.config.harnesses.get(&format!("cli:{key}")))
}

/// Returns the `[harnesses.*].env_pass` names without their values.
fn env_names(deps: &Deps, executor: &Executor) -> Vec<String> {
    harness_config(deps, executor)
        .map(|config| config.env_pass.clone())
        .unwrap_or_default()
}

/// Generates the uuid-v4 a fresh harness session is pre-assigned.
fn fresh_native_id(deps: &Deps) -> String {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&deps.rng.jitter_secs(u64::MAX).to_be_bytes());
    bytes[8..].copy_from_slice(&deps.rng.jitter_secs(u64::MAX).to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

/// A defensive executor value for a run row that lost its executor.
fn unknown_executor() -> Executor {
    Executor {
        harness: HarnessKind::Opencode,
        model: "unknown".to_owned(),
        effort: None,
        variant: None,
    }
}

/// Records a provider limit with the single two-minute safety margin.
fn record_limit(store: &Store, event: &LimitEvent, harness: HarnessKind) -> Result<(), CoreError> {
    store.upsert_limit(&Limit {
        provider: event.provider.clone(),
        harness: Some(harness),
        kind: match event.kind {
            vowp_protocol::LimitKind::Quota => LimitRowKind::Quota,
            vowp_protocol::LimitKind::Credits => LimitRowKind::Credits,
            vowp_protocol::LimitKind::Rate => LimitRowKind::Rate,
            vowp_protocol::LimitKind::Context => LimitRowKind::Context,
            vowp_protocol::LimitKind::Auth => LimitRowKind::Auth,
        },
        resets_at: event.resets_at.map(|resets| resets.plus(LIMIT_MARGIN)),
        detail: event.message.clone(),
        since: store.clock().now(),
        last_probe_at: None,
        probe_fail_streak: 0,
        concurrency_cap: None,
        cap_until: None,
    })?;
    Ok(())
}
