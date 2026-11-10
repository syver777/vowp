//! Session switch protocol: freeze, drain, capture a capsule, detach the source and
//! attach (optionally start) the target. Spec: `docs/design/sessions.md` §5 and
//! `docs/design/crates/vowp-core.md` §3.
//!
//! The log is never rewritten; a switch only appends events. The `switches` row is the
//! crash-recovery record, [`crate::session::recovery`] finishes it after a restart.
//! A pure switch records the target attach and delivers the seed with the next turn;
//! [`run_switch_with_input`] starts the first seeded turn immediately (the L6 failover
//! path) and retries once with a capsule-only seed when the target rejects the tail.

use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use vowp_adapters::proc::{Signal, signal_group};
use vowp_protocol::{
    Actor, AttachAction, CancelToken, CapsuleTrigger, Clock, CostEvent, ErrorEvent, ErrorKind, EventBody, EventSource,
    Executor, ExecutorRef, HarnessKind, Level, Limit, LimitEvent, LimitRowKind, NodeName, Run, RunId, RunOutcome,
    RunPurpose, RunRole, Session, SessionExecutor, SessionId, SessionRole, SessionState, StartInput, StopReason,
    Switch, SwitchEvent, SwitchId, SwitchRequest, SwitchState, Timestamp, TurnEnded, TurnStarted, UserMessage,
};
use vowp_store::session_files::{AdapterMapping, add_adapter_mapping};
use vowp_store::session_log::{AppendedEvent, NewEvent, SessionLog, index_row};
use vowp_store::{RunFinish, Store};

use crate::deps::Deps;
use crate::error::CoreError;
use crate::session::capsule::CapsuleWriter;
use crate::session::recovery::last_turn_blocked;
use crate::session::seed::seed_prompt;
use crate::session::service::tail_of_complete_turns;

/// Poll interval for adapter events during a seeded switch turn.
const EVENT_POLL: Duration = Duration::from_millis(200);

/// Poll interval while waiting for an active turn to drain.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// Wait after SIGINT before the normal stop, per sessions.md §5 step 4.
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

/// Wait after SIGTERM before the escalated kill, matching `AdapterRun::stop`.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Bounded grace for the runner thread to finish its run row after a stop.
const RUNNER_GRACE: Duration = Duration::from_secs(5);

/// Bounded wait for the runner thread to drop the single-writer session log.
const LOG_RELEASE_GRACE: Duration = Duration::from_secs(5);

/// Shorter bound for an active run row without a pid: there is no process group to
/// signal, so only a runner between polls can finish it.
const UNOWNED_GRACE: Duration = Duration::from_millis(500);

/// Margin added once when a seeded turn reports a provider limit.
const LIMIT_MARGIN: Duration = Duration::from_secs(120);

/// Sleeps between bounded drain polls.
///
/// Production uses [`SystemSleeper`]. Tests inject a sleeper that advances a fake clock,
/// so clock-derived deadlines expire without wall-clock waits.
pub trait DrainSleeper: Send + Sync {
    /// Sleeps for `duration`; a test implementation may only advance a fake clock.
    fn sleep(&self, duration: Duration);
}

/// The real drain sleeper: blocks the current thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemSleeper;

impl DrainSleeper for SystemSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// A deadline measured on the injected clock, with a monotonic backstop.
///
/// The clock deadline is the bound callers reason about (and what FakeClock tests
/// advance). The monotonic deadline is a safety net: a stalled injected clock must never
/// turn a drain wait into an infinite loop.
#[derive(Debug, Clone, Copy)]
struct DrainDeadline {
    clock_deadline: Timestamp,
    wall_deadline: Instant,
}

impl DrainDeadline {
    /// Starts a deadline `budget` from now on both the injected clock and the monotonic one.
    fn after(clock: &dyn Clock, budget: Duration) -> Self {
        Self {
            clock_deadline: clock.now().plus(budget),
            wall_deadline: Instant::now() + budget,
        }
    }

    /// Returns whether either the injected clock or the monotonic backstop has expired.
    fn expired(&self, clock: &dyn Clock) -> bool {
        clock.now() >= self.clock_deadline || Instant::now() >= self.wall_deadline
    }
}

/// How the drain phase ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drain {
    /// The active turn ended before the deadline; `blocked` is true when its last
    /// outcome was a limit, credits or auth failure (sessions.md §2).
    Finished { blocked: bool },
    /// The deadline passed and the active run was stopped.
    Interrupted,
}

/// Runs the switch protocol for `req`; the target is attached idle.
pub fn run_switch(deps: &Deps, req: &SwitchRequest, cancel: &CancelToken) -> Result<Switch, CoreError> {
    run_switch_inner(deps, req, None, cancel, &SystemSleeper)
}

/// Runs the switch protocol with an injected sleeper for the drain waits.
///
/// Tests use this with a sleeper that advances a `FakeClock`; production callers use
/// [`run_switch`], which sleeps on the current thread.
pub fn run_switch_with_sleeper(
    deps: &Deps,
    req: &SwitchRequest,
    cancel: &CancelToken,
    sleeper: &dyn DrainSleeper,
) -> Result<Switch, CoreError> {
    run_switch_inner(deps, req, None, cancel, sleeper)
}

/// Runs the switch protocol and starts the first turn on the target with a seed.
///
/// This is the failover/context variant of [`run_switch`]: the seed is built from the
/// fresh capsule and the last complete turns; if the target rejects it, the switch
/// retries once with the capsule alone and then aborts, re-attaching the source.
pub fn run_switch_with_input(
    deps: &Deps,
    req: &SwitchRequest,
    next_input: &str,
    cancel: &CancelToken,
) -> Result<Switch, CoreError> {
    run_switch_inner(deps, req, Some(next_input), cancel, &SystemSleeper)
}

/// Shared implementation of the pure and seeded switch protocols.
fn run_switch_inner(
    deps: &Deps,
    req: &SwitchRequest,
    next_input: Option<&str>,
    cancel: &CancelToken,
    sleeper: &dyn DrainSleeper,
) -> Result<Switch, CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let store = deps.open_store()?;
    let session = store.require_session(&req.session_id)?;
    if session.state == SessionState::Switching {
        return Err(CoreError::Store(vowp_store::StoreError::Conflict(
            "session is already switching".to_owned(),
        )));
    }
    if session.state == SessionState::Closed {
        return Err(CoreError::Invalid("session is closed; cannot switch".to_owned()));
    }
    if next_input.is_some() {
        check_cwd(deps, &session.cwd)?;
    }

    let previous_executor = session.executor.clone();
    let mut previous_state = session.state;
    let to_node = req.to.node.clone().unwrap_or_else(|| deps.node.clone());
    let to = ExecutorRef {
        executor: req.to.executor.clone(),
        node: Some(to_node.clone()),
    };
    let at_seq = SessionLog::open_read(&deps.paths, &req.session_id)?.last_seq()?;

    store.set_session_state(&req.session_id, SessionState::Switching, previous_executor.as_ref())?;
    let mut switch = Switch {
        id: SwitchId::new(),
        session_id: req.session_id.clone(),
        from_executor: previous_executor.clone(),
        to_executor: req.to.executor.clone(),
        from_node: previous_executor.as_ref().map(|_| session.node.clone()),
        to_node: to_node.clone(),
        capsule_id: None,
        reason: req.reason,
        state: SwitchState::Started,
        error: None,
        started_at: deps.clock.now(),
        ended_at: None,
        at_seq,
    };
    store.insert_switch(&switch)?;

    let result = (|| -> Result<(SessionState, Option<vowp_protocol::CapsuleId>), CoreError> {
        let target = deps.adapters.get(&req.to.executor);
        // TODO(sessions.md §5 step 3, remote-node probe): a switch whose `to_node` is not
        // this node should run `vowp --node <n> doctor --json` and abort when it is not ok.
        // v0.1 probes only the local AdapterRegistry, so a remote target can currently be
        // attached without proving its harness is available. This probe is deliberately
        // skipped in v0.1.
        let probe = target.as_ref().map(|adapter| adapter.probe());
        let probe_ok = probe.as_ref().is_some_and(|probe| probe.ok);
        if !probe_ok {
            let detail = match probe {
                Some(probe) => format!("target probe failed: {}", probe.detail),
                None => "target harness is not installed".to_owned(),
            };
            return Err(abort(
                deps,
                &store,
                &switch,
                previous_state,
                previous_executor.as_ref(),
                &session.node,
                false,
                ErrorKind::AdapterUnavailable,
                &detail,
            ));
        }

        if is_active(&store, &session)? {
            match drain(deps, &store, &session, req.drain_secs, cancel, sleeper) {
                Ok(Drain::Finished { blocked }) => {
                    previous_state = if blocked {
                        SessionState::Blocked
                    } else {
                        SessionState::Idle
                    };
                }
                Ok(Drain::Interrupted) => previous_state = SessionState::Idle,
                Err(CoreError::Cancelled) => {
                    let _ = abort(
                        deps,
                        &store,
                        &switch,
                        previous_state,
                        previous_executor.as_ref(),
                        &session.node,
                        false,
                        ErrorKind::Internal,
                        "switch cancelled during drain",
                    );
                    return Err(CoreError::Cancelled);
                }
                Err(error) => {
                    let detail = error.to_string();
                    let _ = abort(
                        deps,
                        &store,
                        &switch,
                        previous_state,
                        previous_executor.as_ref(),
                        &session.node,
                        false,
                        ErrorKind::Internal,
                        &detail,
                    );
                    return Err(error);
                }
            }
        }

        let capsule = match CapsuleWriter::new(deps.clone()).write(&req.session_id, CapsuleTrigger::Switch) {
            Ok(capsule) => Some(capsule),
            Err(error) => match store.latest_capsule(&req.session_id)? {
                Some(previous) => {
                    let log = SessionLog::open(&deps.paths, &req.session_id, deps.clock.clone())?;
                    append_event(
                        &store,
                        &log,
                        HarnessKind::Vowp,
                        EventBody::Note(vowp_protocol::Note {
                            text: format!("stale capsule used: {error}"),
                        }),
                        Some(deps.node.clone()),
                        None,
                    )?;
                    log.sync()?;
                    Some(previous)
                }
                None => {
                    return Err(abort(
                        deps,
                        &store,
                        &switch,
                        previous_state,
                        previous_executor.as_ref(),
                        &session.node,
                        false,
                        ErrorKind::Parse,
                        &format!("capsule invalid after repair: {error}"),
                    ));
                }
            },
        };
        let capsule_id = capsule.as_ref().map(|capsule| capsule.capsule_id.clone());

        let log = SessionLog::open(&deps.paths, &req.session_id, deps.clock.clone())?;
        if let Some(source) = &previous_executor {
            append_event(
                &store,
                &log,
                source.harness,
                EventBody::SessionExecutor(SessionExecutor {
                    action: AttachAction::Detach,
                    executor: source.clone(),
                    node: session.node.clone(),
                    native_session_id: None,
                    run_id: None,
                    reason: Some("switch".to_owned()),
                }),
                Some(session.node.clone()),
                None,
            )?;
        }
        store.set_session_state(&req.session_id, SessionState::Switching, None)?;

        let mut final_state = SessionState::Idle;
        match next_input {
            Some(input) => {
                let capsule = capsule
                    .as_ref()
                    .ok_or_else(|| CoreError::CapsuleInvalid("switch has no capsule to seed".to_owned()))?;
                let events = SessionLog::open_read(&deps.paths, &req.session_id)?.read_all()?;
                let tail = tail_of_complete_turns(&events, deps.config.capsule.tail_turns);
                let seeded = seed_prompt(capsule, &tail, deps.config.capsule.tail_turns, input);
                let capsule_only = seed_prompt(capsule, &[], deps.config.capsule.tail_turns, input);
                match run_target_turn(deps, &store, &log, &session, &to, seeded) {
                    Ok(state) => final_state = state,
                    Err(first) => match run_target_turn(deps, &store, &log, &session, &to, capsule_only) {
                        Ok(state) => final_state = state,
                        Err(second) => {
                            drop(log);
                            return Err(abort(
                                deps,
                                &store,
                                &switch,
                                previous_state,
                                previous_executor.as_ref(),
                                &session.node,
                                true,
                                ErrorKind::AdapterFailed,
                                &format!("target rejected the seed twice: {first}; {second}"),
                            ));
                        }
                    },
                }
            }
            None => {
                append_event(
                    &store,
                    &log,
                    req.to.executor.harness,
                    EventBody::SessionExecutor(SessionExecutor {
                        action: AttachAction::Attach,
                        executor: req.to.executor.clone(),
                        node: to_node.clone(),
                        native_session_id: None,
                        run_id: None,
                        reason: None,
                    }),
                    Some(to_node.clone()),
                    None,
                )?;
                store.set_session_state(&req.session_id, SessionState::Switching, Some(&req.to.executor))?;
            }
        }

        append_event(
            &store,
            &log,
            req.to.executor.harness,
            EventBody::Switch(SwitchEvent {
                switch_id: switch.id.clone(),
                from: previous_executor.clone().map(|executor| ExecutorRef {
                    executor,
                    node: Some(session.node.clone()),
                }),
                to,
                capsule_id: capsule_id.clone(),
                reason: req.reason,
            }),
            Some(to_node),
            None,
        )?;
        log.sync()?;
        store.finish_switch(&switch.id, SwitchState::Completed, None, capsule_id.as_ref())?;
        Ok((final_state, capsule_id))
    })();

    match result {
        Ok((final_state, capsule_id)) => {
            store.set_session_state(&req.session_id, final_state, Some(&req.to.executor))?;
            switch.capsule_id = capsule_id;
            switch.state = SwitchState::Completed;
            switch.ended_at = Some(deps.clock.now());
            Ok(switch)
        }
        Err(error) => {
            // Any failure after the plan row exists must finish it; a row left `started`
            // would block crash recovery and hide the failure.
            let still_started = match store.switch(&switch.id) {
                Ok(Some(row)) => row.state == SwitchState::Started,
                Ok(None) => false,
                Err(_) => true,
            };
            if still_started {
                let detail = error.to_string();
                let _ = abort(
                    deps,
                    &store,
                    &switch,
                    previous_state,
                    previous_executor.as_ref(),
                    &session.node,
                    false,
                    ErrorKind::Internal,
                    &detail,
                );
            }
            Err(error)
        }
    }
}

/// Returns whether the session has a run that has not finished.
fn is_active(store: &Store, session: &Session) -> Result<bool, CoreError> {
    if session.state == SessionState::Running {
        return Ok(true);
    }
    Ok(store
        .open_runs()?
        .iter()
        .any(|run| run.session_id.as_ref() == Some(&session.id)))
}

/// Waits up to `drain_secs` for the active turn to end, then stops it.
///
/// A run started by `SessionService::run_turn` owns the session's single-writer log and
/// the run row. This function signals the run's process group (SIGINT, 3 s, SIGTERM,
/// 3 s, SIGKILL), waits for the runner thread to finish the row, retries opening the log
/// until the runner's writer lock is released, and only then appends the synthetic
/// interrupted `turn.ended` if the adapter did not emit one. It never finishes the run
/// row itself, so the runner cannot race with "run already ended".
///
/// Every wait is bounded: the drain deadline is `drain_secs`; after it the interrupt path
/// gets at most `INTERRUPT_GRACE` (3 s) + `STOP_GRACE` (3 s) + `RUNNER_GRACE` (5 s) +
/// `LOG_RELEASE_GRACE` (5 s). Each deadline follows the injected clock and has a
/// monotonic backstop, so even a stalled clock cannot make the switch hang.
fn drain(
    deps: &Deps,
    store: &Store,
    session: &Session,
    drain_secs: u64,
    cancel: &CancelToken,
    sleeper: &dyn DrainSleeper,
) -> Result<Drain, CoreError> {
    let start_seq = SessionLog::open_read(&deps.paths, &session.id)?.last_seq()?;
    let deadline = DrainDeadline::after(deps.clock.as_ref(), Duration::from_secs(drain_secs));
    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let events = SessionLog::open_read(&deps.paths, &session.id)?.read_all()?;
        if events
            .iter()
            .any(|event| event.seq > start_seq && matches!(event.body(), Ok(EventBody::TurnEnded(_))))
        {
            return Ok(Drain::Finished {
                blocked: last_turn_blocked(&events),
            });
        }
        if deadline.expired(deps.clock.as_ref()) {
            break;
        }
        // Bound: the drain deadline above (`drain_secs`, default 5 s).
        sleeper.sleep(DRAIN_POLL);
    }

    let active: Vec<Run> = store
        .open_runs()?
        .into_iter()
        .filter(|run| run.session_id.as_ref() == Some(&session.id))
        .collect();
    let signalable = active.iter().any(|run| run.pid.is_some());
    let mut active_ids = Vec::with_capacity(active.len());
    if !active.is_empty() {
        for run in &active {
            active_ids.push(run.id.clone());
            signal_run(run, Signal::Int)?;
        }
        if signalable {
            // Bound: INTERRUPT_GRACE (3 s) after SIGINT.
            let _ = wait_window(deps, &session.id, start_seq, INTERRUPT_GRACE, cancel, sleeper)?;
            for run in &active {
                signal_run(run, Signal::Term)?;
            }
            // Bound: STOP_GRACE (3 s) after SIGTERM.
            let _ = wait_window(deps, &session.id, start_seq, STOP_GRACE, cancel, sleeper)?;
            for run in &active {
                signal_run(run, Signal::Kill)?;
            }
        } else {
            // No process group to signal: give a runner between polls one short window.
            // Bound: UNOWNED_GRACE (500 ms).
            let _ = wait_window(deps, &session.id, start_seq, UNOWNED_GRACE, cancel, sleeper)?;
        }
    }
    // The runner thread owns the run row and the session log writer. Let it finish
    // applying the interrupted turn, then take the writer once it releases the lock;
    // never finish the run row from here or the runner would see "already ended".
    // Bound: RUNNER_GRACE (5 s) for a stopped process; UNOWNED_GRACE (500 ms) otherwise.
    let grace = if signalable { RUNNER_GRACE } else { UNOWNED_GRACE };
    wait_for_runs_finished(store, &active_ids, grace, deps.clock.as_ref(), sleeper, signalable)?;
    // Bound: LOG_RELEASE_GRACE (5 s) for a stopped process; UNOWNED_GRACE (500 ms) otherwise.
    let log_wait = if signalable { LOG_RELEASE_GRACE } else { UNOWNED_GRACE };
    let log = open_log_when_released(deps, &session.id, log_wait, sleeper, signalable)?;
    let events = SessionLog::open_read(&deps.paths, &session.id)?.read_all()?;
    let interrupted = events
        .iter()
        .any(|event| event.seq > start_seq && matches!(event.body(), Ok(EventBody::TurnEnded(_))));
    if !interrupted {
        let turn = events
            .iter()
            .rev()
            .find_map(|event| match event.body() {
                Ok(EventBody::TurnStarted(payload)) => Some(payload.turn),
                _ => None,
            })
            .unwrap_or_else(|| {
                events
                    .iter()
                    .filter(|event| event.event_type == vowp_protocol::EventType::TurnStarted)
                    .count()
                    .saturating_add(1) as u32
            });
        append_event(
            store,
            &log,
            HarnessKind::Vowp,
            EventBody::TurnEnded(TurnEnded {
                turn,
                stop_reason: StopReason::Interrupted,
                duration_ms: None,
            }),
            Some(deps.node.clone()),
            None,
        )?;
        log.sync()?;
    }
    drop(log);
    Ok(Drain::Interrupted)
}

/// Waits for the runner thread to finish the given runs, bounded by `grace`.
///
/// `owned` is true when at least one run has a process group we stopped: then a still-open
/// row means the runner never finished and the switch must fail instead of waiting forever.
/// Rows without a pid belong to no process we control; their log-release wait decides.
fn wait_for_runs_finished(
    store: &Store,
    ids: &[RunId],
    grace: Duration,
    clock: &dyn Clock,
    sleeper: &dyn DrainSleeper,
    owned: bool,
) -> Result<(), CoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    let deadline = DrainDeadline::after(clock, grace);
    loop {
        let open = store.open_runs()?;
        if !open.iter().any(|run| ids.iter().any(|id| id == &run.id)) {
            return Ok(());
        }
        if deadline.expired(clock) {
            if owned {
                return Err(CoreError::SwitchAborted(format!(
                    "drain: runner did not finish {} run row(s) within {grace:?}",
                    ids.len()
                )));
            }
            return Ok(());
        }
        // Bound: the `grace` deadline constructed above.
        sleeper.sleep(DRAIN_POLL);
    }
}

/// Opens the session log, retrying while the runner thread still holds its writer lock.
///
/// The retry loop is bounded by `timeout`. When the stopped run was owned (`owned`), a
/// still-locked log after the grace is a switch failure; otherwise the lock error is
/// returned as-is so the caller still aborts the switch row.
fn open_log_when_released(
    deps: &Deps,
    session: &SessionId,
    timeout: Duration,
    sleeper: &dyn DrainSleeper,
    owned: bool,
) -> Result<SessionLog, CoreError> {
    let deadline = DrainDeadline::after(deps.clock.as_ref(), timeout);
    loop {
        match SessionLog::open(&deps.paths, session, deps.clock.clone()) {
            Ok(log) => return Ok(log),
            Err(vowp_store::StoreError::Locked { .. }) if !deadline.expired(deps.clock.as_ref()) => {
                // Bound: the `timeout` deadline constructed above.
                sleeper.sleep(DRAIN_POLL);
            }
            Err(vowp_store::StoreError::Locked { .. }) if owned => {
                return Err(CoreError::SwitchAborted(format!(
                    "drain: session log writer was not released within {timeout:?}"
                )));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Signals a run's process group, when the adapter recorded a pid.
fn signal_run(run: &Run, signal: Signal) -> Result<(), CoreError> {
    if let Some(pid) = run.pid {
        signal_group(pid, signal).map_err(|source| CoreError::Io {
            path: format!("process group {pid}"),
            source,
        })?;
    }
    Ok(())
}

/// Waits up to `window` for a `turn.ended` after `start_seq`, bounded by the injected clock.
fn wait_window(
    deps: &Deps,
    session: &SessionId,
    start_seq: u64,
    window: Duration,
    cancel: &CancelToken,
    sleeper: &dyn DrainSleeper,
) -> Result<bool, CoreError> {
    let deadline = DrainDeadline::after(deps.clock.as_ref(), window);
    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let events = SessionLog::open_read(&deps.paths, session)?.read_all()?;
        if events
            .iter()
            .any(|event| event.seq > start_seq && matches!(event.body(), Ok(EventBody::TurnEnded(_))))
        {
            return Ok(true);
        }
        if deadline.expired(deps.clock.as_ref()) {
            return Ok(false);
        }
        // Bound: the `window` deadline constructed above.
        sleeper.sleep(DRAIN_POLL);
    }
}

/// Checks `cwd` against the project path, worktree root and `[node].cwd_allow`.
fn check_cwd(deps: &Deps, cwd: &str) -> Result<(), CoreError> {
    let path = Path::new(cwd);
    let mut roots: Vec<&str> = Vec::new();
    if let Some(project) = &deps.config.project.path {
        roots.push(project);
    }
    if let Some(root) = &deps.config.project.worktree_root {
        roots.push(root);
    }
    roots.extend(deps.config.node.cwd_allow.iter().map(String::as_str));
    if roots.is_empty() {
        return Ok(());
    }
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    for root in roots {
        let root = Path::new(root);
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if path.starts_with(&root) {
            return Ok(());
        }
    }
    Err(CoreError::Refused(format!(
        "working directory {cwd} is outside the allow-list"
    )))
}

/// Aborts a switch: error event, state restored, row finished `aborted`.
///
/// The row and session state are the recovery record, so they are finished even when
/// the log writer is held or an event append fails. The returned error is always the
/// one the caller should propagate.
#[allow(clippy::too_many_arguments)]
fn abort(
    deps: &Deps,
    store: &Store,
    switch: &Switch,
    previous_state: SessionState,
    previous_executor: Option<&Executor>,
    previous_node: &NodeName,
    reattach_source: bool,
    kind: ErrorKind,
    detail: &str,
) -> CoreError {
    // The active runner may still hold the log writer. Append best-effort; the switch
    // row, state restore and finish below must happen regardless.
    let log = match SessionLog::open(&deps.paths, &switch.session_id, deps.clock.clone()) {
        Ok(log) => Some(log),
        Err(vowp_store::StoreError::Locked { .. }) => None,
        Err(error) => {
            deps.log.log(
                Level::Warn,
                "session switch",
                &format!("abort could not open the log: {error}"),
            );
            None
        }
    };
    if let Some(log) = &log {
        if let Err(error) = append_event(
            store,
            log,
            HarnessKind::Vowp,
            EventBody::Error(ErrorEvent {
                kind,
                message: vowp_adapters::classify::redact(detail, 300),
                retryable: false,
            }),
            Some(deps.node.clone()),
            None,
        ) {
            deps.log.log(
                Level::Warn,
                "session switch",
                &format!("abort could not append the error: {error}"),
            );
        }
    }
    if reattach_source {
        match previous_executor {
            Some(executor) => {
                if let Some(log) = &log {
                    if let Err(error) = append_event(
                        store,
                        log,
                        executor.harness,
                        EventBody::SessionExecutor(SessionExecutor {
                            action: AttachAction::Attach,
                            executor: executor.clone(),
                            node: previous_node.clone(),
                            native_session_id: None,
                            run_id: None,
                            reason: Some("switch_abort".to_owned()),
                        }),
                        Some(previous_node.clone()),
                        None,
                    ) {
                        deps.log.log(
                            Level::Warn,
                            "session switch",
                            &format!("abort could not append the source re-attach: {error}"),
                        );
                    }
                }
                if let Err(error) = store.set_session_state(&switch.session_id, previous_state, Some(executor)) {
                    deps.log.log(
                        Level::Warn,
                        "session switch",
                        &format!("abort could not restore state: {error}"),
                    );
                }
            }
            None => {
                if let Err(error) = store.set_session_state(&switch.session_id, SessionState::Blocked, None) {
                    deps.log.log(
                        Level::Warn,
                        "session switch",
                        &format!("abort could not restore state: {error}"),
                    );
                }
            }
        }
    } else if let Err(error) = store.set_session_state(&switch.session_id, previous_state, previous_executor) {
        deps.log.log(
            Level::Warn,
            "session switch",
            &format!("abort could not restore state: {error}"),
        );
    }
    if let Some(log) = &log {
        let _ = log.sync();
    }
    if let Err(error) = store.finish_switch(&switch.id, SwitchState::Aborted, Some(detail), None) {
        return error.into();
    }
    CoreError::SwitchAborted(detail.to_owned())
}

/// Runs the first turn after a seeded switch, retrying is the caller's policy.
///
/// The caller holds the session's writer lock for `log`; this function appends the
/// attach, turn framing, adapter events and detach, in the same order as `run_turn`.
fn run_target_turn(
    deps: &Deps,
    store: &Store,
    log: &SessionLog,
    session: &Session,
    target: &ExecutorRef,
    prompt: String,
) -> Result<SessionState, CoreError> {
    check_cwd(deps, &session.cwd)?;
    let executor = target.executor.clone();
    let node = target.node.clone().unwrap_or_else(|| deps.node.clone());
    let adapter = deps.adapters.get(&executor).ok_or_else(|| {
        CoreError::Adapter(vowp_protocol::AdapterError::Unavailable(
            "harness not installed".to_owned(),
        ))
    })?;
    let prior = SessionLog::open_read(&deps.paths, &session.id)?.read_all()?;
    let turn = prior
        .iter()
        .filter(|event| event.event_type == vowp_protocol::EventType::TurnStarted)
        .count()
        .saturating_add(1) as u32;
    let run_id = RunId::new();
    let run = Run {
        id: run_id.clone(),
        job_id: session.job_id.clone(),
        session_id: Some(session.id.clone()),
        role: run_role(session.role),
        purpose: RunPurpose::Continue,
        profile: None,
        rung: 0,
        executor: Some(executor.clone()),
        provider: Some(executor.provider()),
        node: node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some(session.cwd.clone()),
        started_at: deps.clock.now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: vowp_protocol::Cost::unknown(),
        detail: String::new(),
    };
    let attach = append_event(
        store,
        log,
        executor.harness,
        EventBody::SessionExecutor(SessionExecutor {
            action: AttachAction::Attach,
            executor: executor.clone(),
            node: node.clone(),
            native_session_id: None,
            run_id: Some(run_id.clone()),
            reason: None,
        }),
        Some(node.clone()),
        None,
    )?;
    let attach_seq = attach.envelope.seq;
    append_event(
        store,
        log,
        executor.harness,
        EventBody::TurnStarted(TurnStarted { turn }),
        Some(node.clone()),
        None,
    )?;
    append_event(
        store,
        log,
        executor.harness,
        EventBody::UserMessage(UserMessage {
            text: prompt.clone(),
            attachments: Vec::new(),
            note: Some("handoff".to_owned()),
        }),
        Some(node.clone()),
        None,
    )?;
    store.insert_run(&run)?;
    store.set_session_state(&session.id, SessionState::Running, Some(&executor))?;

    let start = StartInput {
        session_id: session.id.clone(),
        executor: executor.clone(),
        cwd: Path::new(&session.cwd).to_path_buf(),
        prompt,
        resume: None,
        new_native_id: None,
        env: start_env(deps, &executor),
        permission_mode: harness_config(deps, &executor).and_then(|config| config.permission_mode.clone()),
        system_note: None,
        event_log_hint: Some(deps.paths.run_raw_path(&run_id)),
        timeout: None,
    };
    let mut adapter_run = match adapter.start(start) {
        Ok(adapter_run) => adapter_run,
        Err(error) => {
            append_event(
                store,
                log,
                executor.harness,
                EventBody::Error(ErrorEvent {
                    kind: ErrorKind::AdapterFailed,
                    message: vowp_adapters::classify::redact(&error.to_string(), 300),
                    retryable: false,
                }),
                Some(node.clone()),
                None,
            )?;
            append_event(
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
            append_event(
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
            store.finish_run(
                &run_id,
                &RunFinish {
                    outcome: RunOutcome::Error,
                    exit_code: None,
                    limit_kind: None,
                    cost: vowp_protocol::Cost::unknown(),
                    native_session_id: None,
                    detail: vowp_adapters::classify::redact(&error.to_string(), 1500),
                },
            )?;
            store.set_session_state(&session.id, SessionState::Idle, Some(&executor))?;
            return Err(CoreError::Adapter(error));
        }
    };
    store.update_run_attach(&run_id, adapter_run.pid(), None, None, None)?;

    let mut emitted_cost = false;
    let mut emitted_limit = false;
    let mut emitted_error = false;
    let mut terminal: Option<vowp_protocol::HarnessEvent> = None;
    loop {
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
                append_harness(store, log, &event, &executor, &node)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let summary = match adapter_run.wait() {
        Ok(summary) => summary,
        Err(error) => {
            // A wait error must still terminate the turn and detach the executor; otherwise
            // the log keeps an unterminated turn that recovery cannot interpret. When the
            // adapter had already emitted `turn.ended`, do not duplicate it.
            if terminal.is_none() {
                append_event(
                    store,
                    log,
                    executor.harness,
                    EventBody::TurnEnded(TurnEnded {
                        turn,
                        stop_reason: StopReason::Error,
                        duration_ms: None,
                    }),
                    Some(node.clone()),
                    None,
                )?;
            }
            append_event(
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
            store.finish_run(
                &run_id,
                &RunFinish {
                    outcome: RunOutcome::Error,
                    exit_code: None,
                    limit_kind: None,
                    cost: vowp_protocol::Cost::unknown(),
                    native_session_id: None,
                    detail: error.to_string(),
                },
            )?;
            store.set_session_state(&session.id, SessionState::Idle, Some(&executor))?;
            return Err(CoreError::Adapter(error));
        }
    };
    if summary.cost.usd.is_some() && !emitted_cost {
        append_event(
            store,
            log,
            executor.harness,
            EventBody::Cost(CostEvent {
                cost: summary.cost.clone(),
                run_id: Some(run_id.clone()),
            }),
            Some(node.clone()),
            summary.native_session_id.clone(),
        )?;
    }
    if let Some(limit) = &summary.limit
        && !emitted_limit
    {
        append_event(
            store,
            log,
            executor.harness,
            EventBody::Limit(limit.clone()),
            Some(node.clone()),
            summary.native_session_id.clone(),
        )?;
    } else if !emitted_error && matches!(summary.outcome, RunOutcome::Error | RunOutcome::Crash) {
        append_event(
            store,
            log,
            executor.harness,
            EventBody::Error(ErrorEvent {
                kind: ErrorKind::AdapterFailed,
                message: vowp_adapters::classify::redact(
                    summary
                        .result_text
                        .clone()
                        .unwrap_or_else(|| "adapter ended without a result".to_owned())
                        .as_str(),
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
    append_ts_event(
        store,
        log,
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
    append_event(
        store,
        log,
        executor.harness,
        EventBody::SessionExecutor(SessionExecutor {
            action: AttachAction::Detach,
            executor: executor.clone(),
            node: node.clone(),
            native_session_id: summary.native_session_id.clone(),
            run_id: Some(run_id.clone()),
            reason: Some(
                match summary.limit {
                    Some(_) => "limit",
                    None if summary.outcome == RunOutcome::Crash => "crash",
                    None => "turn_done",
                }
                .to_owned(),
            ),
        }),
        Some(node.clone()),
        summary.native_session_id.clone(),
    )?;
    log.sync()?;

    if let Some(native) = &summary.native_session_id {
        store.upsert_native(&session.id, executor.harness, native, &node, attach_seq)?;
        add_adapter_mapping(
            &deps.paths,
            &session.id,
            AdapterMapping {
                harness: executor.harness,
                native_session_id: native.clone(),
                node: node.clone(),
                first_seq: attach_seq,
                created_at: deps.clock.now(),
            },
        )?;
    }
    let state = if matches!(
        summary.outcome,
        RunOutcome::Limit | RunOutcome::Credits | RunOutcome::Auth
    ) {
        SessionState::Blocked
    } else {
        SessionState::Idle
    };
    store.set_session_state(&session.id, state, Some(&executor))?;
    store.finish_run(
        &run_id,
        &RunFinish {
            outcome: summary.outcome,
            exit_code: summary.exit_code,
            limit_kind: summary.limit.as_ref().map(|limit| limit.kind),
            cost: summary.cost.clone(),
            native_session_id: summary.native_session_id.clone(),
            detail: vowp_adapters::classify::redact(
                summary
                    .limit
                    .as_ref()
                    .map(|limit| limit.message.clone())
                    .or_else(|| summary.result_text.clone())
                    .unwrap_or_default()
                    .as_str(),
                1500,
            ),
        },
    )?;
    if let Some(limit) = &summary.limit {
        record_limit(deps, store, limit, executor.harness)?;
    } else if summary.outcome == RunOutcome::Ok {
        store.clear_limit(&executor.provider())?;
    }
    Ok(state)
}

/// Appends one event with the canonical actor and indexes it.
fn append_event(
    store: &Store,
    log: &SessionLog,
    harness: HarnessKind,
    body: EventBody,
    node: Option<NodeName>,
    native: Option<String>,
) -> Result<AppendedEvent, CoreError> {
    append_ts_event(store, log, harness, body, None, node, native)
}

/// Appends one event with an optional harness timestamp and indexes it.
fn append_ts_event(
    store: &Store,
    log: &SessionLog,
    harness: HarnessKind,
    body: EventBody,
    ts: Option<Timestamp>,
    node: Option<NodeName>,
    native: Option<String>,
) -> Result<AppendedEvent, CoreError> {
    let appended = log.append(NewEvent {
        actor: actor_for(&body),
        source: EventSource {
            harness,
            model: None,
            node,
            native_session_id: native,
        },
        body,
        raw: None,
        ts,
    })?;
    store.index_event(&index_row(&appended))?;
    Ok(appended)
}

/// Forwards one adapter event.
fn append_harness(
    store: &Store,
    log: &SessionLog,
    event: &vowp_protocol::HarnessEvent,
    executor: &Executor,
    node: &NodeName,
) -> Result<(), CoreError> {
    let appended = log.append(NewEvent {
        actor: event.actor,
        source: EventSource {
            harness: executor.harness,
            model: Some(executor.model.clone()),
            node: Some(node.clone()),
            native_session_id: None,
        },
        body: event.body.clone(),
        raw: event.raw.clone(),
        ts: event.ts,
    })?;
    store.index_event(&index_row(&appended))?;
    Ok(())
}

/// Records a provider limit with the single two-minute margin.
fn record_limit(deps: &Deps, store: &Store, event: &LimitEvent, harness: HarnessKind) -> Result<(), CoreError> {
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
        since: deps.clock.now(),
        last_probe_at: None,
        probe_fail_streak: 0,
        concurrency_cap: None,
        cap_until: None,
    })?;
    Ok(())
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

/// Maps a session role to the run role used by a seeded switch turn.
fn run_role(role: Option<SessionRole>) -> RunRole {
    match role {
        Some(SessionRole::Planner) => RunRole::Planner,
        Some(SessionRole::Keeper) => RunRole::Keeper,
        Some(SessionRole::Verifier) => RunRole::Verifier,
        Some(SessionRole::Worker | SessionRole::User) | None => RunRole::Worker,
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

/// Resolves `[harnesses.*].env_pass` names from the process environment.
fn start_env(deps: &Deps, executor: &Executor) -> Vec<(String, String)> {
    let Some(config) = harness_config(deps, executor) else {
        return Vec::new();
    };
    config
        .env_pass
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
        .collect()
}
