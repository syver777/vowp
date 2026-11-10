//! Bounded switch-drain tests: a never-released log writer, a runner that never finishes,
//! the prompt positive path and the recovered capsule id on a recovery-appended switch
//! event. Hermetic: temporary node, `FakeClock`, fake adapters; the process-backed fake
//! is the only real child and is killed by the drain. Spec: plan §5.3 and
//! `docs/design/sessions.md` §5.

mod support;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use support::signal_child::{shell_command, wait_for_markers};
use tempfile::{TempDir, tempdir};
use vowp_core::session::recovery;
use vowp_core::session::service::SessionService;
use vowp_core::session::switch::{DrainSleeper, run_switch_with_sleeper};
use vowp_core::{AdapterRegistry, CoreError, Deps};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AttachAction, CancelToken, Capsule, CapsuleEvent,
    CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, Clock, Cost, EventBody, EventEnvelope, EventSource,
    Executor, ExecutorRef, FakeClock, HarnessEvent, HarnessKind, MemoryLog, NodeName, ProbeResult, Run, RunId,
    RunOutcome, RunPurpose, RunRole, RunSummary, SessionId, SessionRole, StartInput, StopReason, SwitchId,
    SwitchReason, SwitchRequest, SwitchState, Timestamp, TurnEnded, TurnRequest, TurnStarted,
};
use vowp_store::NodePaths;
use vowp_store::session_log::{NewEvent, SessionLog, index_row};
use vowp_tmux::fake::FakeWindows;

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn opencode() -> Executor {
    Executor::parse("opencode:deepseek/deepseek-flash#max").expect("opencode executor")
}

fn claude() -> Executor {
    Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("claude executor")
}

fn node() -> NodeName {
    NodeName::parse("local").expect("node name")
}

fn config_for(tmp: &Path) -> vowp_protocol::VowpConfig {
    let mut config = vowp_protocol::VowpConfig::with_defaults_for_tests();
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.to_string_lossy().into_owned());
    config
}

fn deps(tmp: &TempDir, registry: AdapterRegistry, clock: FakeClock) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config_for(tmp.path())),
        node: node(),
        clock: Arc::new(clock),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(registry),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(vowp_core::SystemRunner),
        rng: Arc::new(vowp_core::FixedRng(0)),
    }
}

fn create_session(service: &SessionService, tmp: &TempDir, name: &str) -> SessionId {
    service
        .create_session(vowp_protocol::NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: name.to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: Some(opencode()),
        })
        .expect("create session")
}

/// The drain window requested by [`request`]; the signal test's clock advancer mirrors it.
const DRAIN_SECS: u64 = 5;
const DRAIN_WINDOW: Duration = Duration::from_secs(DRAIN_SECS);

/// Production's drain graces, mirrored by the signal test's clock advancer.
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
const STOP_GRACE: Duration = Duration::from_secs(3);
const RUNNER_GRACE: Duration = Duration::from_secs(5);

fn request(session: &SessionId, to: Executor, reason: SwitchReason) -> SwitchRequest {
    SwitchRequest {
        session_id: session.clone(),
        to: ExecutorRef {
            executor: to,
            node: None,
        },
        reason,
        drain_secs: DRAIN_SECS,
    }
}

fn turn(session: &SessionId, executor: Executor, prompt: &str) -> TurnRequest {
    TurnRequest {
        session_id: session.clone(),
        executor: ExecutorRef { executor, node: None },
        purpose: RunPurpose::Start,
        role: RunRole::Worker,
        job_id: None,
        prompt: prompt.to_owned(),
        profile: Some("worker".to_owned()),
        rung: 0,
        window: None,
        timeout: None,
    }
}

fn open_run(session: &SessionId, pid: Option<u32>) -> Run {
    Run {
        id: RunId::new(),
        job_id: None,
        session_id: Some(session.clone()),
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(opencode()),
        provider: Some("deepseek".to_owned()),
        node: node(),
        native_session_id: None,
        pid,
        window: None,
        cwd: None,
        started_at: ts(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    }
}

fn append_event(deps: &Deps, session: &SessionId, body: EventBody) {
    let store = deps.open_store().expect("store");
    let log = SessionLog::open(&deps.paths, session, deps.clock.clone()).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Vowp,
                model: None,
                node: Some(node()),
                native_session_id: None,
            },
            body,
            raw: None,
            ts: None,
        })
        .expect("append");
    store.index_event(&index_row(&appended)).expect("index");
    log.sync().expect("sync");
}

fn read_events(deps: &Deps, session: &SessionId) -> Vec<EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

/// A sleeper that advances a fake clock, so bounded drain deadlines pass quickly while
/// yielding briefly to let real runner threads make progress.
struct AdvancingSleeper {
    clock: FakeClock,
}

impl AdvancingSleeper {
    fn new(clock: FakeClock) -> Self {
        Self { clock }
    }
}

impl DrainSleeper for AdvancingSleeper {
    fn sleep(&self, duration: Duration) {
        self.clock.advance(duration);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A sleeper for a test whose fake clock is advanced by an advancer thread.
///
/// It yields one millisecond per drain poll and reports the two moments the drain's
/// clock arithmetic cannot otherwise expose:
///
/// - the first poll, after which the drain deadline is fixed;
/// - the first poll at or past drain + interrupt + stop, which can only be the runner
///   grace, because every earlier wait stops polling once that much fake time has passed.
struct YieldingSleeper {
    clock: FakeClock,
    start: Timestamp,
    first_poll: Mutex<Option<Sender<()>>>,
    runner_grace: Mutex<Option<Sender<()>>>,
}

impl YieldingSleeper {
    fn new(clock: FakeClock, start: Timestamp, first_poll: Sender<()>, runner_grace: Sender<()>) -> Self {
        Self {
            clock,
            start,
            first_poll: Mutex::new(Some(first_poll)),
            runner_grace: Mutex::new(Some(runner_grace)),
        }
    }
}

impl DrainSleeper for YieldingSleeper {
    fn sleep(&self, _duration: Duration) {
        if let Some(first_poll) = self.first_poll.lock().expect("first poll lock").take() {
            let _ = first_poll.send(());
        }
        if self.clock.now().since(self.start) >= DRAIN_WINDOW + INTERRUPT_GRACE + STOP_GRACE
            && let Some(runner_grace) = self.runner_grace.lock().expect("runner grace lock").take()
        {
            let _ = runner_grace.send(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Advances the fake clock through the drain's bounded signal windows.
///
/// The caller waits for the drain's first poll before calling this, so the drain
/// deadline is already fixed at the pre-advance clock. Fake time then moves to the
/// deadline (SIGINT), waits for the child's `int` acknowledgment before entering the
/// SIGTERM grace, waits for `term` before entering the SIGKILL grace, and advances the
/// runner grace only after the sleeper reports the drain's first poll past SIGKILL.
/// Without the acknowledgments the compressed fake clock can send SIGTERM while the
/// child's SIGINT trap is still pending, which loses `int`.
fn advance_drain_clock(clock: FakeClock, signal_path: PathBuf, runner_grace: Receiver<()>) {
    clock.advance(DRAIN_WINDOW);
    wait_for_markers(&signal_path, &["int"]);
    clock.advance(INTERRUPT_GRACE);
    wait_for_markers(&signal_path, &["term"]);
    clock.advance(STOP_GRACE);
    runner_grace
        .recv_timeout(Duration::from_secs(10))
        .expect("the drain polls its runner grace after SIGKILL");
    clock.advance(RUNNER_GRACE);
}

/// A sleeper that advances the fake clock, but on its first sleep hands off to a test
/// writer and waits until that writer has appended its terminal event.
///
/// The drain loop's deadline is measured on the fake clock, while the writer is a real
/// thread. Without the handoff a loaded scheduler can delay the writer past all 250 poll
/// sleeps (5 s of fake time), so the drain falls into its interrupt path even though the
/// runner only needed a few milliseconds. The handoff ties fake time to the writer: the
/// event exists before the next poll, and the test proves the drain releases promptly
/// instead of waiting out `drain_secs`.
struct HandoffSleeper {
    clock: FakeClock,
    release: Mutex<Option<Sender<()>>>,
    appended: Mutex<Receiver<()>>,
}

impl HandoffSleeper {
    fn new(clock: FakeClock, release: Sender<()>, appended: Receiver<()>) -> Self {
        Self {
            clock,
            release: Mutex::new(Some(release)),
            appended: Mutex::new(appended),
        }
    }
}

impl DrainSleeper for HandoffSleeper {
    fn sleep(&self, duration: Duration) {
        self.clock.advance(duration);
        let release = self.release.lock().expect("release lock").take();
        if let Some(release) = release {
            release.send(()).expect("the writer waits for the first drain poll");
            self.appended
                .lock()
                .expect("appended lock")
                .recv_timeout(Duration::from_secs(10))
                .expect("the writer appends before the next drain poll");
        }
    }
}

/// A process-backed adapter whose child ignores SIGINT and SIGTERM, and whose `wait()`
/// blocks until the test releases it. That keeps the run row (and the log writer) alive
/// while the drain's bounded runner grace expires.
struct BlockingProcessAdapter {
    kind: HarnessKind,
    signal_path: PathBuf,
    ready_path: PathBuf,
    release: Arc<Mutex<Option<Sender<()>>>>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl BlockingProcessAdapter {
    fn new(kind: HarnessKind, signal_path: PathBuf, ready_path: PathBuf) -> Self {
        Self {
            kind,
            signal_path,
            ready_path,
            release: Arc::new(Mutex::new(None)),
            pid: Arc::new(Mutex::new(None)),
        }
    }

    fn release(&self) {
        if let Some(sender) = self.release.lock().expect("release lock").take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for BlockingProcessAdapter {
    fn drop(&mut self) {
        if let Some(pid) = *self.pid.lock().expect("pid lock") {
            let _ = vowp_adapters::proc::signal_group(pid, vowp_adapters::proc::Signal::Kill);
        }
    }
}

impl Adapter for BlockingProcessAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: None,
            path: Some("sh".to_owned()),
            detail: "blocking process fake".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        // Publish readiness only after both traps exist. A signal that arrives while
        // `sh` is still starting up can be handled by the shell before the user traps
        // are installed, which silently loses the `int` marker. Start the shell through
        // `shell_command`: a non-interactive shell cannot trap SIGINT if it inherited
        // SIGINT ignored (for example when a gate runs as a background shell job), so
        // the disposition is reset before the traps are installed.
        let script = format!(
            "trap 'echo int >> {path}' INT; \
             trap 'echo term >> {path}' TERM; \
             echo ready >> {ready}; \
             while :; do sleep 0.01; done",
            path = self.signal_path.display(),
            ready = self.ready_path.display()
        );
        let mut child = shell_command(&script)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(AdapterError::Io)?;
        let pid = child.id();
        *self.pid.lock().expect("pid lock") = Some(pid);

        let (sender, receiver) = channel();
        let (release_sender, release_receiver) = channel();
        *self.release.lock().expect("release lock") = Some(release_sender);
        let pid_slot = self.pid.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            *pid_slot.lock().expect("pid lock") = None;
            let _ = sender.send(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Interrupted,
                    duration_ms: None,
                }),
                raw: None,
            });
        });
        Ok(Box::new(BlockingProcessRun {
            pid,
            receiver,
            release: release_receiver,
        }))
    }
}

struct BlockingProcessRun {
    pid: u32,
    receiver: Receiver<HarnessEvent>,
    release: Receiver<()>,
}

impl AdapterRun for BlockingProcessRun {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("process fake has no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        vowp_adapters::proc::signal_group(self.pid, vowp_adapters::proc::Signal::Int).map_err(AdapterError::Io)
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        vowp_adapters::proc::signal_group(self.pid, vowp_adapters::proc::Signal::Term).map_err(AdapterError::Io)
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        let _ = self.release.recv();
        Ok(RunSummary {
            exit_code: None,
            outcome: RunOutcome::Killed,
            native_session_id: None,
            cost: Cost::unknown(),
            limit: None,
            result_text: None,
            stop_reason: StopReason::Interrupted,
        })
    }
}

#[test]
fn drain_is_bounded_when_writer_never_released() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with(
                "opencode",
                Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new())),
            )
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "writer forever");
    let store = deps.open_store().expect("store");
    let run = open_run(&session, None);
    store.insert_run(&run).expect("run row");

    // Simulate a live runner: this writer lock is held for the whole switch attempt.
    let held = SessionLog::open(&deps.paths, &session, deps.clock.clone()).expect("held log");
    let appended = held
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: None,
                node: Some(node()),
                native_session_id: None,
            },
            body: EventBody::TurnStarted(TurnStarted { turn: 1 }),
            raw: None,
            ts: None,
        })
        .expect("turn started");
    store.index_event(&index_row(&appended)).expect("index");
    held.sync().expect("sync");
    let log_path = deps.paths.log_path(&session);
    let bytes_before = std::fs::metadata(&log_path).expect("log metadata").len();

    let sleeper = AdvancingSleeper::new(clock.clone());
    let before = clock.now();
    let error = run_switch_with_sleeper(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
        &sleeper,
    )
    .expect_err("a writer that is never released must abort the switch");
    let elapsed = clock.now().since(before);

    assert!(
        matches!(error, CoreError::Store(vowp_store::StoreError::Locked { .. })),
        "got {error:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(5),
        "the drain waited for its clock deadline, got {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_secs(14),
        "the drain stayed inside drain_secs plus its bounded graces, got {elapsed:?}"
    );

    let row = store
        .list_switches(&session)
        .expect("switches")
        .into_iter()
        .next()
        .expect("switch row");
    assert_eq!(row.state, SwitchState::Aborted, "the row is not left started");
    assert!(
        row.error.as_deref().is_some_and(|error| error.contains("locked")),
        "the row records the writer failure: {:?}",
        row.error
    );

    let current = store.require_session(&session).expect("session");
    assert_eq!(
        current.state,
        vowp_protocol::SessionState::Idle,
        "original state restored"
    );
    assert_eq!(current.executor, Some(opencode()), "the source stays attached");
    assert_eq!(
        std::fs::metadata(&log_path).expect("log metadata").len(),
        bytes_before,
        "the switch never wrote through the held writer"
    );
    assert_eq!(
        SessionLog::open_read(&deps.paths, &session)
            .expect("read log")
            .last_seq()
            .expect("last seq"),
        2,
        "only session.created and the runner's turn.started are in the log"
    );

    // The held writer is untouched and still usable for its owner.
    let still = held
        .append(NewEvent {
            actor: Actor::System,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: None,
                node: Some(node()),
                native_session_id: None,
            },
            body: EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: StopReason::Interrupted,
                duration_ms: None,
            }),
            raw: None,
            ts: None,
        })
        .expect("held writer still appends");
    store.index_event(&index_row(&still)).expect("index");
    held.sync().expect("sync");
}

#[test]
fn drain_is_bounded_when_runner_never_finishes() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let signal_path = tmp.path().join("signals.txt");
    let ready_path = tmp.path().join("ready.txt");
    let source = Arc::new(BlockingProcessAdapter::new(
        HarnessKind::Opencode,
        signal_path.clone(),
        ready_path.clone(),
    ));
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with("opencode", source.clone()).with(
            "claude-code",
            Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
        ),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "runner forever");
    let store = deps.open_store().expect("store");

    let worker_service = service.clone();
    let worker_session = session.clone();
    let worker = std::thread::spawn(move || {
        worker_service.run_turn(
            turn(&worker_session, opencode(), "never finishes"),
            &CancelToken::default(),
        )
    });

    let wait_until = Instant::now() + Duration::from_secs(2);
    let mut active = None;
    while Instant::now() < wait_until {
        if let Some(run) = store
            .open_runs()
            .expect("open runs")
            .into_iter()
            .find(|run| run.session_id.as_ref() == Some(&session) && run.pid.is_some())
        {
            active = Some(run);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let active = active.expect("the blocking run is active");

    // The child writes `ready` only after both traps are installed. Wait for it before
    // the switch can signal the group; SIGINT sent while `sh` is still starting is
    // lost, and only the later SIGTERM would be recorded.
    wait_for_markers(&ready_path, &["ready"]);

    // The fake clock is driven from a separate thread, like the production wall clock:
    // the drain deadline first, then each grace only after the child acknowledged the
    // previous signal. The sleeper reports its first poll (the drain deadline is fixed)
    // and its first poll after SIGKILL (the runner grace has started), so fake time
    // never moves ahead of the phase it is meant to expire.
    let before = clock.now();
    let (first_poll, drain_started) = channel();
    let (runner_grace, waiting_for_runner) = channel();
    let sleeper = YieldingSleeper::new(clock.clone(), before, first_poll, runner_grace);
    let advancer = {
        let clock = clock.clone();
        let signal_path = signal_path.clone();
        std::thread::spawn(move || {
            drain_started
                .recv_timeout(Duration::from_secs(10))
                .expect("the drain takes its first fake-clock poll");
            advance_drain_clock(clock, signal_path, waiting_for_runner);
        })
    };
    let error = run_switch_with_sleeper(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
        &sleeper,
    )
    .expect_err("a runner that never finishes must abort the switch");
    advancer.join().expect("clock advancer");
    let elapsed = clock.now().since(before);

    // Release the fake adapter's blocked wait and let the runner clean up its row.
    source.release();
    let worker_result = worker.join().expect("worker thread");
    assert!(worker_result.is_ok(), "the released runner finishes: {worker_result:?}");

    assert!(matches!(error, CoreError::SwitchAborted(_)), "got {error:?}");
    assert!(
        error.to_string().contains("runner did not finish"),
        "the error names the bounded runner wait: {error}"
    );
    assert!(
        elapsed >= Duration::from_secs(16),
        "drain 5 s + interrupt 3 s + stop 3 s + runner 5 s elapsed, got {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_secs(25),
        "the runner wait stayed bounded, got {elapsed:?}"
    );

    let signals = wait_for_markers(&signal_path, &["int", "term"]);
    let int_at = signals.find("int").expect("SIGINT marker");
    let term_at = signals.find("term").expect("SIGTERM marker");
    assert!(
        int_at < term_at,
        "drain sent SIGINT before SIGTERM; contents: {signals:?}"
    );

    let row = store
        .switch(
            &store
                .list_switches(&session)
                .expect("switches")
                .into_iter()
                .next()
                .expect("switch row")
                .id,
        )
        .expect("switch row")
        .expect("switch row exists");
    assert_eq!(row.state, SwitchState::Aborted, "the row is not left started");
    assert!(
        row.error
            .as_deref()
            .is_some_and(|error| error.contains("runner did not finish")),
        "the row records the runner failure: {:?}",
        row.error
    );
    let finished = store.run(&active.id).expect("run").expect("run row");
    assert_eq!(finished.outcome, Some(RunOutcome::Killed));
    assert!(finished.ended_at.is_some(), "the released runner finished its row");
    assert_eq!(
        store.require_session(&session).expect("session").state,
        vowp_protocol::SessionState::Idle,
        "the released runner leaves the source usable"
    );
}

#[test]
fn drain_releases_promptly_when_runner_finishes() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let capsule = FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(vowp_protocol::AssistantMessage {
            text: serde_json::json!({
                "intent": "prompt drain handoff",
                "constraints": [],
                "decisions": [],
                "state": {"done": ["work"], "in_progress": [], "blocked": [], "next_steps": ["continue"]},
                "files": [],
                "gotchas": [],
                "open_questions": []
            })
            .to_string(),
        })],
    );
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with("opencode", Arc::new(capsule)).with(
            "claude-code",
            Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
        ),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "prompt drain");
    let store = deps.open_store().expect("store");
    let run = open_run(&session, None);
    store.insert_run(&run).expect("run row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Running, Some(&opencode()))
        .expect("running");

    let (release_writer, wait_release) = channel();
    let (writer_appended, wait_appended) = channel();
    let writer = {
        let deps = deps.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            wait_release
                .recv_timeout(Duration::from_secs(10))
                .expect("the drain releases the writer on its first poll");
            append_event(
                &deps,
                &session,
                EventBody::TurnEnded(TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::EndTurn,
                    duration_ms: None,
                }),
            );
            writer_appended
                .send(())
                .expect("the sleeper waits for the terminal event");
        })
    };

    let sleeper = HandoffSleeper::new(clock.clone(), release_writer, wait_appended);
    let before = clock.now();
    let switch = run_switch_with_sleeper(
        &deps,
        &request(&session, claude(), SwitchReason::User),
        &CancelToken::default(),
        &sleeper,
    )
    .expect("the runner finishes before the deadline");
    let elapsed = clock.now().since(before);
    writer.join().expect("writer");

    assert_eq!(switch.state, SwitchState::Completed);
    assert!(
        elapsed < Duration::from_secs(5),
        "a finishing runner releases well under drain_secs, got {elapsed:?}"
    );
    let events = read_events(&deps, &session);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.body(), Ok(EventBody::TurnEnded(_))))
            .count(),
        1,
        "the runner's terminal event is not duplicated"
    );
    assert!(
        events
            .iter()
            .any(|event| event.event_type == vowp_protocol::EventType::Switch),
        "the switch completes after the prompt drain"
    );
}

#[test]
fn recover_switch_event_carries_recovered_capsule_id() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with(
            "opencode",
            Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new())),
        ),
        clock,
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "recovered capsule");
    let store = deps.open_store().expect("store");
    let capsule_id = CapsuleId::new();

    append_event(
        &deps,
        &session,
        EventBody::SessionExecutor(vowp_protocol::SessionExecutor {
            action: AttachAction::Attach,
            executor: claude(),
            node: node(),
            native_session_id: None,
            run_id: None,
            reason: None,
        }),
    );
    let at_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    append_event(
        &deps,
        &session,
        EventBody::Capsule(CapsuleEvent {
            capsule_id: capsule_id.clone(),
            version: 1,
            trigger: CapsuleTrigger::Switch,
            at_seq,
        }),
    );
    store
        .insert_capsule(
            &Capsule {
                version: 1,
                capsule_id: capsule_id.clone(),
                session_id: session.clone(),
                created_at: ts(),
                at_seq,
                trigger: CapsuleTrigger::Switch,
                from: ExecutorRef {
                    executor: opencode(),
                    node: Some(node()),
                },
                intent: "recovered capsule".to_owned(),
                constraints: Vec::new(),
                decisions: Vec::new(),
                state: CapsuleState::default(),
                files: Vec::new(),
                gotchas: Vec::new(),
                open_questions: Vec::new(),
                telemetry: CapsuleTelemetry::default(),
            },
            "capsules/recovered.json",
        )
        .expect("capsule row");

    let switch = vowp_protocol::Switch {
        id: SwitchId::new(),
        session_id: session.clone(),
        from_executor: Some(opencode()),
        to_executor: claude(),
        from_node: Some(node()),
        to_node: node(),
        capsule_id: None,
        reason: SwitchReason::User,
        state: SwitchState::Started,
        error: None,
        started_at: ts(),
        ended_at: None,
        at_seq,
    };
    store.insert_switch(&switch).expect("switch row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Switching, Some(&claude()))
        .expect("switching");

    let report = recovery::recover(&deps, &store).expect("recover");
    assert_eq!(report.switches_completed, 1);
    assert_eq!(report.switches_aborted, 0);

    let row = store.switch(&switch.id).expect("row").expect("switch");
    assert_eq!(row.state, SwitchState::Completed);
    assert_eq!(row.capsule_id, Some(capsule_id.clone()), "the row links the capsule");

    let recorded = read_events(&deps, &session)
        .into_iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::Switch(payload)) if payload.switch_id == switch.id => Some(payload),
            _ => None,
        })
        .expect("recovery appends the missing switch event");
    assert_eq!(
        recorded.capsule_id,
        Some(capsule_id),
        "the recovered event carries the capsule id from the log"
    );
}
