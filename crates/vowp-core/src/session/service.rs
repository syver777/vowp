//! The session service: create a session, run one turn, write a capsule, switch executors.
//! Spec: `docs/design/crates/vowp-core.md` §3 and `docs/design/sessions.md` §1–5.
//!
//! `run_turn` owns the event framing an adapter must not emit: `session.executor` attach,
//! `turn.started` and `user.message` before the harness events, and `session.executor` detach
//! after the run's `turn.ended`. Capsules and seeds live in the sibling modules; this file
//! wires them into the service and keeps the store's index rows in step with the log.

use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use vowp_protocol::{
    Actor, AttachAction, CancelToken, Capsule, CapsuleEvent, CapsuleTrigger, CostEvent, ErrorEvent, ErrorKind,
    EventBody, EventEnvelope, EventSource, Executor, HarnessKind, Limit, LimitEvent, LimitRowKind, NewSession,
    NodeName, Run, RunOutcome, ServiceError, Session, SessionExecutor, SessionId, SessionOps, SessionState, Switch,
    SwitchRequest, Timestamp, TurnEnded, TurnRequest, TurnResult, TurnStarted, UserMessage,
};
use vowp_store::session_files::{AdapterMapping, add_adapter_mapping, write_capsule_file};
use vowp_store::session_log::{AppendedEvent, NewEvent, SessionLog, index_row};
use vowp_store::{RunFinish, Store, StoreError};

use crate::deps::Deps;
use crate::error::CoreError;
use crate::session::capsule::CapsuleWriter;
use crate::session::seed::seed_prompt;
use crate::session::{bundle, recovery, switch};

/// Poll interval for adapter events; cancellation is checked at this cadence.
const EVENT_POLL: Duration = Duration::from_millis(200);

/// Safety margin added once when a provider limit is written to `limits`.
const LIMIT_MARGIN: Duration = Duration::from_secs(120);

/// Startup recovery counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `started` switch rows finished as completed.
    pub switches_completed: u32,
    /// `started` switch rows finished as aborted.
    pub switches_aborted: u32,
    /// Dangling runs closed as crashed.
    pub runs_closed: u32,
    /// Session locks found and cleared (always zero: locks are files released with the process).
    pub sessions_unlocked: u32,
}

/// The session service. Cloning shares the injected [`Deps`].
#[derive(Clone)]
pub struct SessionService {
    deps: Deps,
}

impl SessionService {
    /// Creates a service over `deps`.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Returns the injected dependencies.
    pub fn deps(&self) -> &Deps {
        &self.deps
    }

    /// Creates a session: index row, log directory and the `session.created` event.
    pub fn create_session(&self, req: NewSession) -> Result<SessionId, CoreError> {
        let store = self.deps.open_store()?;
        let id = SessionId::new();
        let now = self.deps.clock.now();
        let session = Session {
            id: id.clone(),
            project: req.project.clone(),
            job_id: req.job_id.clone(),
            role: Some(req.role),
            title: req.title.clone(),
            parent_session_id: req.parent.clone(),
            node: self.deps.node.clone(),
            cwd: req.cwd.to_string_lossy().into_owned(),
            state: SessionState::Idle,
            executor: req.executor.clone(),
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        };
        store.upsert_session(&session)?;

        let log = SessionLog::create(&self.deps.paths, &id, self.deps.clock.clone())?;
        let body = EventBody::SessionCreated(vowp_protocol::SessionCreated {
            title: req.title,
            cwd: session.cwd.clone(),
            project: req.project,
            job_id: req.job_id,
            role: Some(req.role),
            parent_session_id: req.parent,
        });
        self.append(&store, &log, HarnessKind::Vowp, body, None, None)?;
        log.sync()?;
        Ok(id)
    }

    /// Runs one turn, blocking until the adapter finishes.
    ///
    /// Framing, event forwarding, run/limits/ledger rows and the native-session mapping are
    /// exactly the numbered algorithm in `crates/vowp-core.md` §3.
    pub fn run_turn(&self, req: TurnRequest, cancel: &CancelToken) -> Result<TurnResult, CoreError> {
        let store = self.deps.open_store()?;
        let session = store.require_session(&req.session_id)?;
        if session.state == SessionState::Switching {
            return Err(CoreError::Store(StoreError::Conflict(
                "session is switching; input is rejected".to_owned(),
            )));
        }
        if session.state == SessionState::Closed {
            return Err(CoreError::Invalid("session is closed; no further turns".to_owned()));
        }
        self.check_cwd(&session.cwd)?;
        // A live lease must match VOWP_LEASE_TOKEN.
        crate::surface::check_lease(&self.deps, &store, &req.session_id)?;
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let executor = req.executor.executor.clone();
        let node = req.executor.node.clone().unwrap_or_else(|| self.deps.node.clone());
        let adapter = self.deps.adapters.get(&executor).ok_or_else(|| {
            CoreError::Adapter(vowp_protocol::AdapterError::Unavailable(
                "harness not installed".to_owned(),
            ))
        })?;

        let log = SessionLog::open(&self.deps.paths, &req.session_id, self.deps.clock.clone())?;
        let prior_events = SessionLog::open_read(&self.deps.paths, &req.session_id)?.read_all()?;
        let turn = prior_events
            .iter()
            .filter(|event| event.event_type == vowp_protocol::EventType::TurnStarted)
            .count()
            .saturating_add(1) as u32;

        let native = if adapter.capabilities().can_resume_native {
            native_for_node(&store, &req.session_id, executor.harness, &node)?
        } else {
            None
        };
        // An imported child keeps the bundle tail out of its log; the first turn therefore
        // seeds from the bundle sidecar written by `bundle::import` (sessions.md §6.3 step 5).
        let imported = if native.is_none() {
            bundle::pending_import_seed(&self.deps, &req.session_id, &req.prompt)?
        } else {
            None
        };
        let imported_used = imported.is_some();
        let (prompt, handoff) = if let Some(seed) = imported {
            (seed, true)
        } else if native.is_none() {
            match store.latest_capsule(&req.session_id)? {
                Some(capsule) => {
                    let tail = tail_of_complete_turns(&prior_events, self.deps.config.capsule.tail_turns);
                    (
                        seed_prompt(&capsule, &tail, self.deps.config.capsule.tail_turns, &req.prompt),
                        true,
                    )
                }
                None => (req.prompt.clone(), false),
            }
        } else {
            (req.prompt.clone(), false)
        };

        let run_id = vowp_protocol::RunId::new();
        let run = Run {
            id: run_id.clone(),
            job_id: req.job_id.clone().or_else(|| session.job_id.clone()),
            session_id: Some(req.session_id.clone()),
            role: req.role,
            purpose: req.purpose,
            profile: req.profile.clone(),
            rung: req.rung,
            executor: Some(executor.clone()),
            provider: Some(executor.provider()),
            node: node.clone(),
            native_session_id: native.clone(),
            pid: None,
            window: req.window.clone(),
            cwd: Some(session.cwd.clone()),
            started_at: self.deps.clock.now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: vowp_protocol::Cost::unknown(),
            detail: String::new(),
        };

        let attach = SessionExecutor {
            action: AttachAction::Attach,
            executor: executor.clone(),
            node: node.clone(),
            native_session_id: native.clone(),
            run_id: Some(run_id.clone()),
            reason: None,
        };
        let attach_event = self.append(
            &store,
            &log,
            executor.harness,
            EventBody::SessionExecutor(attach),
            Some(node.clone()),
            native.clone(),
        )?;
        let attach_seq = attach_event.envelope.seq;
        self.append(
            &store,
            &log,
            executor.harness,
            EventBody::TurnStarted(TurnStarted { turn }),
            Some(node.clone()),
            native.clone(),
        )?;
        self.append(
            &store,
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
            bundle::clear_pending_import(&self.deps, &req.session_id)?;
        }
        store.insert_run(&run)?;
        store.set_session_state(&req.session_id, SessionState::Running, Some(&executor))?;

        // Inject exactly the configured secret names.
        let mut start_env = self.start_env(&executor);
        start_env.extend(crate::vault::env_for(&self.deps, &req)?);
        let start_input = vowp_protocol::StartInput {
            session_id: req.session_id.clone(),
            executor: executor.clone(),
            cwd: Path::new(&session.cwd).to_path_buf(),
            prompt,
            resume: native.clone(),
            new_native_id: native.is_none().then(|| self.fresh_native_id()),
            env: start_env,
            permission_mode: self
                .harness_config(&executor)
                .and_then(|config| config.permission_mode.clone()),
            system_note: None,
            event_log_hint: Some(self.deps.paths.run_raw_path(&run_id)),
            timeout: req.timeout,
        };
        let mut adapter_run = match adapter.start(start_input) {
            Ok(run) => run,
            Err(error) => {
                self.finish_failed_start(&store, &log, &req, &run_id, turn, &executor, &node, &error)?;
                return Err(CoreError::Adapter(error));
            }
        };
        store.update_run_attach(&run_id, adapter_run.pid(), None, req.window.as_deref(), None)?;

        let mut emitted_cost = false;
        let mut emitted_limit = false;
        let mut emitted_error = false;
        let mut terminal: Option<vowp_protocol::HarnessEvent> = None;
        let mut interrupted = false;
        loop {
            if cancel.is_cancelled() && !interrupted {
                interrupted = true;
                let _ = adapter_run.interrupt();
            }
            match adapter_run.events().recv_timeout(EVENT_POLL) {
                Ok(event) => {
                    match &event.body {
                        EventBody::Cost(_) => emitted_cost = true,
                        EventBody::Limit(_) => emitted_limit = true,
                        EventBody::Error(_) => emitted_error = true,
                        EventBody::TurnEnded(_) => {
                            terminal = Some(event);
                            break;
                        }
                        _ => {}
                    }
                    self.append_harness(&store, &log, event, &executor, &node, native.as_deref())?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        let summary = match adapter_run.wait() {
            Ok(summary) => summary,
            Err(error) => {
                let finish = RunFinish {
                    outcome: RunOutcome::Error,
                    exit_code: None,
                    limit_kind: None,
                    cost: vowp_protocol::Cost::unknown(),
                    native_session_id: None,
                    detail: vowp_adapters::classify::redact(&error.to_string(), 1500),
                };
                let _ = store.finish_run(&run_id, &finish);
                let _ = store.set_session_state(&req.session_id, SessionState::Idle, Some(&executor));
                return Err(CoreError::Adapter(error));
            }
        };

        if summary.cost.usd.is_some() && !emitted_cost {
            self.append(
                &store,
                &log,
                executor.harness,
                EventBody::Cost(CostEvent {
                    cost: summary.cost.clone(),
                    run_id: Some(run_id.clone()),
                }),
                Some(node.clone()),
                summary.native_session_id.clone(),
            )?;
        }
        if let Some(limit) = &summary.limit {
            if !emitted_limit {
                self.append(
                    &store,
                    &log,
                    executor.harness,
                    EventBody::Limit(limit.clone()),
                    Some(node.clone()),
                    summary.native_session_id.clone(),
                )?;
            }
        } else if !emitted_error && matches!(summary.outcome, RunOutcome::Error | RunOutcome::Crash) {
            self.append(
                &store,
                &log,
                executor.harness,
                EventBody::Error(ErrorEvent {
                    kind: ErrorKind::AdapterFailed,
                    message: vowp_adapters::classify::redact(
                        &summary
                            .result_text
                            .clone()
                            .unwrap_or_else(|| "adapter ended without a result".to_owned()),
                        300,
                    ),
                    retryable: summary.outcome == RunOutcome::Crash,
                }),
                Some(node.clone()),
                summary.native_session_id.clone(),
            )?;
        }

        let (duration_ms, terminal_ts) = match terminal.as_ref().map(|event| (&event.body, event.ts)) {
            Some((EventBody::TurnEnded(ended), ts)) => (ended.duration_ms, ts),
            _ => (None, None),
        };
        self.append_event(
            &store,
            &log,
            executor.harness,
            EventBody::TurnEnded(TurnEnded {
                turn,
                stop_reason: summary.stop_reason,
                duration_ms,
            }),
            terminal_ts,
            Some(node.clone()),
            summary.native_session_id.clone(),
        )?;
        let detach_reason = match summary.limit {
            Some(_) => "limit",
            None if summary.outcome == RunOutcome::Crash => "crash",
            None => "turn_done",
        };
        self.append(
            &store,
            &log,
            executor.harness,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Detach,
                executor: executor.clone(),
                node: node.clone(),
                native_session_id: summary.native_session_id.clone(),
                run_id: Some(run_id.clone()),
                reason: Some(detach_reason.to_owned()),
            }),
            Some(node.clone()),
            summary.native_session_id.clone(),
        )?;
        log.sync()?;

        if let Some(native_id) = &summary.native_session_id {
            let first_seq = if native.as_deref() == Some(native_id.as_str()) {
                attach_seq
            } else {
                log.last_seq()
            };
            store.upsert_native(&req.session_id, executor.harness, native_id, &node, first_seq)?;
            add_adapter_mapping(
                &self.deps.paths,
                &req.session_id,
                AdapterMapping {
                    harness: executor.harness,
                    native_session_id: native_id.clone(),
                    node: node.clone(),
                    first_seq,
                    created_at: self.deps.clock.now(),
                },
            )?;
        }

        // sessions.md §2: only limit/credits/auth outcomes block a session.
        let state = if matches!(
            summary.outcome,
            RunOutcome::Limit | RunOutcome::Credits | RunOutcome::Auth
        ) {
            SessionState::Blocked
        } else {
            SessionState::Idle
        };
        store.set_session_state(&req.session_id, state, Some(&executor))?;

        let finish = RunFinish {
            outcome: summary.outcome,
            exit_code: summary.exit_code,
            limit_kind: summary.limit.as_ref().map(|limit| limit.kind),
            cost: summary.cost.clone(),
            native_session_id: summary.native_session_id.clone(),
            detail: vowp_adapters::classify::redact(
                &summary
                    .limit
                    .as_ref()
                    .map(|limit| limit.message.clone())
                    .or_else(|| summary.result_text.clone())
                    .unwrap_or_default(),
                1500,
            ),
        };
        store.finish_run(&run_id, &finish)?;

        if let Some(limit) = &summary.limit {
            self.record_limit(&store, limit, executor.harness)?;
        } else if summary.outcome == RunOutcome::Ok {
            store.clear_limit(&executor.provider())?;
        }
        self.record_cost_ledger(&store, &run, &summary)?;

        // Redact job artifacts written by the harness (report.md, raw mirror).
        let _ = crate::vault::redact_job_artifacts(&self.deps, &store, req.job_id.as_ref());
        Ok(TurnResult { run_id, summary })
    }

    /// Writes a capsule through [`CapsuleWriter`].
    pub fn write_capsule(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, CoreError> {
        let capsule = CapsuleWriter::new(self.deps.clone()).write(session, trigger)?;
        // Redact the capsule file and body row.
        let store = self.deps.open_store()?;
        crate::vault::redact_capsule(&self.deps, &store, &capsule)
    }

    /// Startup recovery: finish or abort switches left in `started`, close dangling runs.
    ///
    /// The implementation lives in [`crate::session::recovery`]; this method is the
    /// service boundary used by startup reconciliation.
    pub fn recover(&self, store: &Store) -> Result<RecoveryReport, CoreError> {
        recovery::recover(&self.deps, store)
    }

    /// Switches a session to another executor (a pure switch: the seed is delivered next turn).
    ///
    /// The protocol lives in [`crate::session::switch`]; this method is the service boundary
    /// used by `SessionOps` and the CLI.
    pub fn switch(&self, req: &SwitchRequest) -> Result<Switch, CoreError> {
        switch::run_switch(&self.deps, req, &CancelToken::default())
    }

    /// Checks session `cwd` against the shared allow-list.
    fn check_cwd(&self, cwd: &str) -> Result<(), CoreError> {
        check_cwd(&self.deps, cwd)
    }

    /// Returns the configured harness entry for `executor`.
    fn harness_config(&self, executor: &Executor) -> Option<&vowp_protocol::HarnessConfig> {
        let key = match executor.harness {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Cli => executor.model.as_str(),
            HarnessKind::Vowp => return None,
        };
        self.deps
            .config
            .harnesses
            .get(key)
            .or_else(|| self.deps.config.harnesses.get(&format!("cli:{key}")))
    }

    /// Resolves `[harnesses.*].env_pass` names from the process environment.
    fn start_env(&self, executor: &Executor) -> Vec<(String, String)> {
        let Some(config) = self.harness_config(executor) else {
            return Vec::new();
        };
        config
            .env_pass
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect()
    }

    /// Generates the uuid-v4 a fresh harness session is pre-assigned (adapters.md §2.1).
    ///
    /// [`Rng`](crate::deps::Rng) exposes bounded jitter only, so two full-width draws fill
    /// the 128 random bits; the version and RFC 4122 variant bits are set in place.
    fn fresh_native_id(&self) -> String {
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&self.deps.rng.jitter_secs(u64::MAX).to_be_bytes());
        bytes[8..].copy_from_slice(&self.deps.rng.jitter_secs(u64::MAX).to_be_bytes());
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

    /// Appends a framing event and indexes it.
    fn append(
        &self,
        store: &Store,
        log: &SessionLog,
        harness: HarnessKind,
        body: EventBody,
        node: Option<NodeName>,
        native: Option<String>,
    ) -> Result<AppendedEvent, CoreError> {
        self.append_event(store, log, harness, body, None, node, native)
    }

    /// Appends one event with an optional harness-reported timestamp and raw value.
    #[allow(clippy::too_many_arguments)]
    fn append_event(
        &self,
        store: &Store,
        log: &SessionLog,
        harness: HarnessKind,
        body: EventBody,
        ts: Option<Timestamp>,
        node: Option<NodeName>,
        native: Option<String>,
    ) -> Result<AppendedEvent, CoreError> {
        let source = EventSource {
            harness,
            model: None,
            node,
            native_session_id: native,
        };
        let appended = log.append(
            // Redact exact values before the log line is written.
            crate::vault::redact_event(
                &self.deps,
                NewEvent {
                    actor: actor_for(&body),
                    source,
                    body,
                    raw: None,
                    ts,
                },
            )?,
        )?;
        store.index_event(&index_row(&appended))?;
        Ok(appended)
    }

    /// Appends one adapter event.
    fn append_harness(
        &self,
        store: &Store,
        log: &SessionLog,
        event: vowp_protocol::HarnessEvent,
        executor: &Executor,
        node: &NodeName,
        native: Option<&str>,
    ) -> Result<(), CoreError> {
        let source = EventSource {
            harness: executor.harness,
            model: Some(executor.model.clone()),
            node: Some(node.clone()),
            native_session_id: native.map(str::to_owned),
        };
        let appended = log.append(
            // Redact exact values before the log line is written.
            crate::vault::redact_event(
                &self.deps,
                NewEvent {
                    actor: event.actor,
                    source,
                    body: event.body,
                    raw: event.raw,
                    ts: event.ts,
                },
            )?,
        )?;
        store.index_event(&index_row(&appended))?;
        Ok(())
    }

    /// Records a provider limit with the single two-minute safety margin.
    fn record_limit(&self, store: &Store, event: &LimitEvent, harness: HarnessKind) -> Result<(), CoreError> {
        let resets_at = event.resets_at.map(|resets| resets.plus(LIMIT_MARGIN));
        store.upsert_limit(&Limit {
            provider: event.provider.clone(),
            harness: Some(harness),
            kind: limit_row_kind(event.kind),
            resets_at,
            detail: event.message.clone(),
            since: self.deps.clock.now(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })?;
        Ok(())
    }

    /// Appends the run's cost ledger row.
    fn record_cost_ledger(
        &self,
        store: &Store,
        run: &Run,
        summary: &vowp_protocol::RunSummary,
    ) -> Result<(), CoreError> {
        let mut detail = serde_json::json!({"billed": true, "basis": "receipt"});
        if run
            .executor
            .as_ref()
            .is_some_and(|executor| executor.harness == HarnessKind::ClaudeCode)
        {
            detail = serde_json::json!({"billed": false, "basis": "list"});
        }
        store.append_ledger(&vowp_protocol::LedgerEntry {
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
            cost: Some(summary.cost.clone()),
            summary: format!("run {} finished: {:?}", run.id, summary.outcome),
            detail,
        })?;
        Ok(())
    }

    /// Appends an error + terminal event for a run whose adapter could not start.
    #[allow(clippy::too_many_arguments)]
    fn finish_failed_start(
        &self,
        store: &Store,
        log: &SessionLog,
        req: &TurnRequest,
        run_id: &vowp_protocol::RunId,
        turn: u32,
        executor: &Executor,
        node: &NodeName,
        error: &vowp_protocol::AdapterError,
    ) -> Result<(), CoreError> {
        self.append(
            store,
            log,
            executor.harness,
            EventBody::Error(ErrorEvent {
                kind: ErrorKind::AdapterUnavailable,
                message: vowp_adapters::classify::redact(&error.to_string(), 300),
                retryable: false,
            }),
            Some(node.clone()),
            None,
        )?;
        self.append(
            store,
            log,
            executor.harness,
            EventBody::TurnEnded(TurnEnded {
                turn,
                stop_reason: vowp_protocol::StopReason::Error,
                duration_ms: None,
            }),
            Some(node.clone()),
            None,
        )?;
        self.append(
            store,
            log,
            executor.harness,
            EventBody::SessionExecutor(SessionExecutor {
                action: AttachAction::Detach,
                executor: executor.clone(),
                node: node.clone(),
                native_session_id: None,
                run_id: Some(run_id.clone()),
                reason: Some("crash".to_owned()),
            }),
            Some(node.clone()),
            None,
        )?;
        log.sync()?;
        store.set_session_state(&req.session_id, SessionState::Idle, Some(executor))?;
        store.finish_run(
            run_id,
            &RunFinish {
                outcome: RunOutcome::Error,
                exit_code: None,
                limit_kind: None,
                cost: vowp_protocol::Cost::unknown(),
                native_session_id: None,
                detail: vowp_adapters::classify::redact(&error.to_string(), 1500),
            },
        )?;
        Ok(())
    }
}

/// Checks `cwd` against the shared session allow-list.
///
/// The allowed roots are the project path, the worktree root, `[node].cwd_allow` and the
/// VOWP-owned orchestrator root `<VOWP_HOME>/orchestrators`. Orchestrator sessions run in
/// `<VOWP_HOME>/orchestrators/<name>/`, so a takeover or repair must accept that directory
/// even though it is outside the project checkout.
///
/// A configuration with no project path, worktree root or `[node].cwd_allow` keeps the
/// historical fallback: no allow-list is configured, so every directory is admitted.
pub(crate) fn check_cwd(deps: &Deps, cwd: &str) -> Result<(), CoreError> {
    let path = Path::new(cwd);
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(project) = &deps.config.project.path {
        roots.push(PathBuf::from(project));
    }
    if let Some(root) = &deps.config.project.worktree_root {
        roots.push(PathBuf::from(root));
    }
    roots.extend(deps.config.node.cwd_allow.iter().map(PathBuf::from));
    if roots.is_empty() {
        return Ok(());
    }
    // VOWP creates and owns this root; it is not a user-supplied `cwd_allow` entry.
    roots.push(deps.paths.root().join("orchestrators"));

    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for root in &roots {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if path.starts_with(&root) {
            return Ok(());
        }
    }
    Err(CoreError::Refused(format!(
        "working directory {cwd} is outside the allow-list"
    )))
}

impl SessionOps for SessionService {
    fn create_session(&self, req: NewSession) -> Result<SessionId, ServiceError> {
        SessionService::create_session(self, req).map_err(service_error)
    }

    fn run_turn(&self, req: TurnRequest, cancel: &CancelToken) -> Result<TurnResult, ServiceError> {
        SessionService::run_turn(self, req, cancel).map_err(service_error)
    }

    fn switch(&self, req: SwitchRequest) -> Result<Switch, ServiceError> {
        SessionService::switch(self, &req).map_err(service_error)
    }

    fn write_capsule(&self, session: &SessionId, trigger: CapsuleTrigger) -> Result<Capsule, ServiceError> {
        SessionService::write_capsule(self, session, trigger).map_err(service_error)
    }
}

/// Converts a core error into the service boundary, preserving the exact missing-harness text.
fn service_error(error: CoreError) -> ServiceError {
    match error {
        CoreError::Adapter(vowp_protocol::AdapterError::Unavailable(message)) if message == "harness not installed" => {
            ServiceError::Failed(message)
        }
        other => ServiceError::from(other),
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

/// Converts an event limit kind to its `limits` row kind.
fn limit_row_kind(kind: vowp_protocol::LimitKind) -> LimitRowKind {
    match kind {
        vowp_protocol::LimitKind::Quota => LimitRowKind::Quota,
        vowp_protocol::LimitKind::Credits => LimitRowKind::Credits,
        vowp_protocol::LimitKind::Rate => LimitRowKind::Rate,
        vowp_protocol::LimitKind::Context => LimitRowKind::Context,
        vowp_protocol::LimitKind::Auth => LimitRowKind::Auth,
    }
}

/// Returns the newest native session id for `harness` recorded on `node`.
///
/// A native id is only valid on the machine that homes it: resuming a session id from
/// another node either fails or, worse, attaches to an unrelated conversation. The store
/// keeps the node column; callers must pass the node the run will actually execute on.
pub(crate) fn native_for_node(
    store: &Store,
    session: &SessionId,
    harness: HarnessKind,
    node: &NodeName,
) -> Result<Option<String>, CoreError> {
    // `natives_of` sorts oldest first by (created_at, first_seq, native_session_id), so the
    // reverse scan returns the most recently recorded id for this node.
    Ok(store
        .natives_of(session)?
        .into_iter()
        .rev()
        .find(|native| native.harness == harness && &native.node == node)
        .map(|native| native.native_session_id))
}

/// Selects the events of the last `turns` complete turns from `events`.
pub(crate) fn tail_of_complete_turns(events: &[EventEnvelope], turns: u32) -> Vec<EventEnvelope> {
    if turns == 0 {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, event) in events.iter().enumerate() {
        match event.body() {
            Ok(EventBody::TurnStarted(_)) => start = Some(index),
            Ok(EventBody::TurnEnded(_)) => {
                if let Some(begin) = start.take() {
                    ranges.push((begin, index));
                }
            }
            _ => {}
        }
    }
    if ranges.is_empty() {
        return if events.is_empty() { Vec::new() } else { events.to_vec() };
    }
    let first = ranges.len().saturating_sub(turns as usize);
    let (begin, _) = ranges[first];
    let (_, end) = ranges[ranges.len() - 1];
    events[begin..=end].to_vec()
}

/// Convenience used by the capsule writer: count turns and sum costs in one event slice.
pub(crate) fn telemetry(events: &[EventEnvelope]) -> vowp_protocol::CapsuleTelemetry {
    let mut telemetry = vowp_protocol::CapsuleTelemetry::default();
    let mut usd = None;
    for event in events {
        match event.body() {
            Ok(EventBody::TurnEnded(_)) => telemetry.turns = telemetry.turns.saturating_add(1),
            Ok(EventBody::Cost(payload)) => {
                telemetry.tokens_in = telemetry.tokens_in.saturating_add(payload.cost.tokens.input);
                telemetry.tokens_out = telemetry.tokens_out.saturating_add(payload.cost.tokens.output);
                if let Some(value) = payload.cost.usd {
                    usd = Some(usd.unwrap_or(0.0) + value);
                }
            }
            _ => {}
        }
    }
    telemetry.usd = usd;
    telemetry
}

/// Selects the last assistant message text from a session log.
pub(crate) fn last_assistant_text(
    paths: &vowp_store::NodePaths,
    session: &SessionId,
) -> Result<Option<String>, CoreError> {
    let events = SessionLog::open_read(paths, session)?.read_all()?;
    Ok(events.iter().rev().find_map(|event| match event.body() {
        Ok(EventBody::AssistantMessage(payload)) => Some(payload.text),
        _ => None,
    }))
}

/// Appends the `capsule` event to a summarised session and syncs the log.
pub(crate) fn append_capsule_event(deps: &Deps, store: &Store, capsule: &Capsule) -> Result<(), CoreError> {
    let log = SessionLog::open(&deps.paths, &capsule.session_id, deps.clock.clone())?;
    let appended = log.append(
        // Redact exact values before the log line is written.
        crate::vault::redact_event(
            deps,
            NewEvent {
                actor: Actor::System,
                source: EventSource {
                    harness: HarnessKind::Vowp,
                    model: None,
                    node: Some(deps.node.clone()),
                    native_session_id: None,
                },
                body: EventBody::Capsule(CapsuleEvent {
                    capsule_id: capsule.capsule_id.clone(),
                    version: capsule.version,
                    trigger: capsule.trigger,
                    at_seq: capsule.at_seq,
                }),
                raw: None,
                ts: None,
            },
        )?,
    )?;
    store.index_event(&index_row(&appended))?;
    log.sync()?;
    Ok(())
}

/// Writes the capsule file and inserts its row.
pub(crate) fn persist_capsule(deps: &Deps, store: &Store, capsule: &Capsule) -> Result<(), CoreError> {
    let path = write_capsule_file(&deps.paths, capsule)?;
    store.insert_capsule(capsule, &path.to_string_lossy())?;
    append_capsule_event(deps, store, capsule)
}

/// Builds the `CapsuleTrigger` wire name for prompts.
pub(crate) fn trigger_name(trigger: CapsuleTrigger) -> &'static str {
    match trigger {
        CapsuleTrigger::Switch => "switch",
        CapsuleTrigger::Manual => "manual",
        CapsuleTrigger::TurnEnd => "turn_end",
        CapsuleTrigger::Limit => "limit",
        CapsuleTrigger::Context => "context",
        CapsuleTrigger::Export => "export",
        CapsuleTrigger::JobEnd => "job_end",
    }
}
