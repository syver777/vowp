//! Switch protocol tests: happy path, probe failure, drain signals and turn numbering,
//! seed rejection, drained-limit restoration and the claude → opencode → claude
//! failover round trip. Hermetic: temp node, fake clock, fake adapters; the only real
//! process is the drain-signal child, killed by the test.
//! Spec: `docs/design/sessions.md` §5 and `docs/design/crates/vowp-core.md` §3.

mod support;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use support::signal_child::shell_command;
use tempfile::{TempDir, tempdir};
use vowp_core::session::service::SessionService;
use vowp_core::session::switch::{run_switch, run_switch_with_input};
use vowp_core::{AdapterRegistry, CoreError, Deps};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, AttachAction, CancelToken, Clock,
    Cost, EventBody, EventEnvelope, EventId, EventIndexRow, EventSource, EventType, Executor, ExecutorRef, FakeClock,
    HarnessEvent, HarnessKind, Limit, LimitRowKind, MemoryLog, NodeName, ProbeResult, Run, RunId, RunOutcome,
    RunPurpose, RunRole, RunSummary, SessionId, SessionOps, SessionRole, StartInput, StopReason, SwitchReason,
    SwitchRequest, SwitchState, Timestamp, TurnRequest,
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

fn deps_with_clock(tmp: &TempDir, registry: AdapterRegistry, clock: FakeClock) -> Deps {
    deps_with_config(tmp, config_for(tmp.path()), registry, clock)
}

fn deps_with_config(
    tmp: &TempDir,
    config: vowp_protocol::VowpConfig,
    registry: AdapterRegistry,
    clock: FakeClock,
) -> Deps {
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    Deps {
        paths,
        config: Arc::new(config),
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

fn deps(tmp: &TempDir, registry: AdapterRegistry) -> Deps {
    deps_with_clock(tmp, registry, FakeClock::at(ts()))
}

fn draft_json(intent: &str) -> String {
    serde_json::json!({
        "intent": intent,
        "constraints": [],
        "decisions": [],
        "state": {"done": ["work"], "in_progress": [], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

fn capsule_adapter(intent: &str) -> FakeAdapter {
    FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: draft_json(intent),
        })],
    )
}

fn create_session_at(service: &SessionService, name: &str, executor: Executor, cwd: PathBuf) -> SessionId {
    service
        .create_session(vowp_protocol::NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: name.to_owned(),
            cwd,
            parent: None,
            executor: Some(executor),
        })
        .expect("create session")
}

fn create_session(service: &SessionService, tmp: &TempDir, name: &str) -> SessionId {
    create_session_at(service, name, opencode(), tmp.path().join("project"))
}

fn request(session: &SessionId, to: Executor, reason: SwitchReason) -> SwitchRequest {
    SwitchRequest {
        session_id: session.clone(),
        to: ExecutorRef {
            executor: to,
            node: None,
        },
        reason,
        drain_secs: 5,
    }
}

fn read_events(deps: &Deps, session: &SessionId) -> Vec<EventEnvelope> {
    SessionLog::open_read(&deps.paths, session)
        .expect("open log")
        .read_all()
        .expect("read log")
}

/// Poll a file written asynchronously by a fake child until every `marker` appears.
///
/// Signal handlers run asynchronously, so reading the file once right after the child
/// is signalled can race under load. Re-read every 20 ms for up to 10 s, then panic
/// with the file's actual contents.
fn wait_for_markers(path: &Path, markers: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = std::fs::read_to_string(path).unwrap_or_default();
        if markers.iter().all(|marker| contents.contains(marker)) {
            return contents;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after 10s waiting for {markers:?} in {}; contents: {contents:?}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
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

/// An adapter whose run is a real child process in its own process group.
///
/// The monitor thread reaps the child and emits `TurnEnded{Interrupted}`, so a real
/// `SessionService::run_turn` finishes only after the drain's signals reached the group.
struct ProcessAdapter {
    kind: HarnessKind,
    signal_path: PathBuf,
    ready_path: PathBuf,
}

impl ProcessAdapter {
    fn new(kind: HarnessKind, signal_path: PathBuf, ready_path: PathBuf) -> Self {
        Self {
            kind,
            signal_path,
            ready_path,
        }
    }
}

impl Adapter for ProcessAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: None,
            path: Some("sh".to_owned()),
            detail: "process fake".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        // Publish readiness only after both traps exist. A signal that arrives while
        // `sh` is still starting up can be ignored or handled by the shell before the
        // user traps are installed, which would silently lose the `int` marker.
        //
        // Start the shell through `shell_command`: a non-interactive shell cannot trap
        // SIGINT if it inherited SIGINT ignored, which happens when a gate runs as a
        // background shell job. Resetting the disposition makes the INT trap real, so
        // the `int` marker is an acknowledgment that SIGINT was delivered.
        let script = format!(
            "trap 'echo int >> {path}' INT; \
             trap 'echo term >> {path}; exit 0' TERM; \
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
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = sender.send(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::TurnEnded(vowp_protocol::TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Interrupted,
                    duration_ms: None,
                }),
                raw: None,
            });
        });
        Ok(Box::new(ProcessRun { pid, receiver }))
    }
}

struct ProcessRun {
    pid: u32,
    receiver: Receiver<HarnessEvent>,
}

impl AdapterRun for ProcessRun {
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

/// An adapter whose run stream closes immediately and whose `wait()` fails.
struct WaitErrorAdapter;

impl Adapter for WaitErrorAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: None,
            path: None,
            detail: "wait-error fake".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, _input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        let (_sender, receiver) = channel();
        Ok(Box::new(WaitErrorRun { receiver }))
    }
}

struct WaitErrorRun {
    receiver: Receiver<HarnessEvent>,
}

impl AdapterRun for WaitErrorRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("wait-error fake has no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Err(AdapterError::Io(std::io::Error::other("wait failed")))
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

fn summary(outcome: RunOutcome) -> RunSummary {
    RunSummary {
        exit_code: Some(0),
        outcome,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("continued".to_owned()),
        stop_reason: StopReason::EndTurn,
    }
}

#[test]
fn switch_happy_path_events_rows_and_lineage() {
    let tmp = tempdir().expect("tempdir");
    let target = FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new());
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("Carry the work over")))
            .with("claude-code", Arc::new(target)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "switch happy");
    let store = deps.open_store().expect("store");

    let switch = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::User),
        &CancelToken::default(),
    )
    .expect("switch");

    assert_eq!(switch.state, SwitchState::Completed);
    assert_eq!(switch.from_executor, Some(opencode()));
    assert_eq!(switch.to_executor, claude());
    assert!(switch.capsule_id.is_some(), "the switch carries the capsule");
    assert!(switch.ended_at.is_some());

    let row = store
        .switch(&switch.id)
        .expect("switch row")
        .expect("switch row exists");
    assert_eq!(row.state, SwitchState::Completed);
    assert_eq!(row.reason, SwitchReason::User);
    assert_eq!(row.to_executor, claude());
    assert!(row.ended_at.is_some());

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.executor, Some(claude()));
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);

    let events = read_events(&deps, &session);
    let capsule_at = events
        .iter()
        .position(|event| event.event_type == EventType::Capsule)
        .expect("capsule event");
    let switch_event_at = events
        .iter()
        .position(|event| event.event_type == EventType::Switch)
        .expect("switch event");
    assert!(
        capsule_at < switch_event_at,
        "capsule is recorded before the switch event"
    );

    let detach_at = events
        .iter()
        .position(|event| {
            matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(payload))
                    if payload.action == AttachAction::Detach && payload.executor == opencode()
            )
        })
        .expect("source detach");
    let attach_at = events
        .iter()
        .position(|event| {
            matches!(
                event.body(),
                Ok(EventBody::SessionExecutor(payload))
                    if payload.action == AttachAction::Attach && payload.executor == claude()
            )
        })
        .expect("target attach");
    assert!(detach_at < attach_at);

    let lineage = events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::Switch(payload)) => Some(payload),
            _ => None,
        })
        .expect("switch payload");
    assert_eq!(lineage.switch_id, switch.id);
    assert_eq!(lineage.reason, SwitchReason::User);
    assert_eq!(lineage.to.executor, claude());
    assert_eq!(lineage.capsule_id, switch.capsule_id);

    assert!(
        events.iter().all(|event| !matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload)) if payload.note.as_deref() == Some("handoff")
        )),
        "a pure switch does not start a turn or deliver a seed"
    );
}

#[test]
fn session_ops_switch_uses_run_switch() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("trait handoff")))
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "trait switch");
    let store = deps.open_store().expect("store");

    let ops: &dyn SessionOps = &service;
    let switch = ops
        .switch(request(&session, claude(), SwitchReason::User))
        .expect("switch through SessionOps");

    assert_eq!(switch.state, SwitchState::Completed);
    assert_eq!(switch.from_executor, Some(opencode()));
    assert_eq!(switch.to_executor, claude());
    assert!(switch.capsule_id.is_some());
    assert!(switch.ended_at.is_some());

    let events = read_events(&deps, &session);
    assert_eq!(
        events.iter().map(|event| event.event_type).collect::<Vec<_>>(),
        vec![
            EventType::SessionCreated,
            EventType::Capsule,
            EventType::SessionExecutor,
            EventType::SessionExecutor,
            EventType::Switch,
        ],
        "the service path runs the sessions.md §5 happy-path sequence"
    );
    assert!(matches!(
        events[2].body(),
        Ok(EventBody::SessionExecutor(payload))
            if payload.action == AttachAction::Detach && payload.executor == opencode()
    ));
    assert!(matches!(
        events[3].body(),
        Ok(EventBody::SessionExecutor(payload))
            if payload.action == AttachAction::Attach && payload.executor == claude()
    ));
    let recorded = match events[4].body().expect("switch payload") {
        EventBody::Switch(payload) => payload,
        other => panic!("expected switch, got {other:?}"),
    };
    assert_eq!(recorded.switch_id, switch.id);
    assert_eq!(recorded.capsule_id, switch.capsule_id);

    let row = store
        .switch(&switch.id)
        .expect("switch row")
        .expect("switch row exists");
    assert_eq!(row.state, SwitchState::Completed);
    assert_eq!(row.capsule_id, switch.capsule_id, "the row links the capsule");
    assert!(row.ended_at.is_some());

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, Some(claude()));
    assert!(
        events.iter().all(|event| !matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload)) if payload.note.as_deref() == Some("handoff")
        )),
        "a pure switch delivers the seed with the next turn"
    );
}

#[test]
fn switch_error_after_capsule_aborts_row() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("late failure handoff")))
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "late failure");
    let store = deps.open_store().expect("store");

    // The capsule event is seq N+1; pre-indexing N+2 makes the later source-detach
    // append fail in the store after the capsule has been captured.
    let last_seq = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .last_seq()
        .expect("seq");
    store
        .index_event(&EventIndexRow {
            session_id: session.clone(),
            seq: last_seq + 2,
            id: EventId::new(),
            ts: ts(),
            event_type: EventType::UserMessage,
            actor: Actor::User,
            harness: HarnessKind::Opencode,
            model: None,
            byte_offset: 0,
            byte_len: 1,
            summary: "injected index conflict".to_owned(),
        })
        .expect("inject index conflict");

    let error = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::User),
        &CancelToken::default(),
    )
    .expect_err("the post-capsule append fails");
    assert!(
        matches!(error, CoreError::Store(vowp_store::StoreError::Conflict(_))),
        "got {error:?}"
    );

    let row = store
        .list_switches(&session)
        .expect("switches")
        .into_iter()
        .next()
        .expect("switch row");
    assert_eq!(
        row.state,
        SwitchState::Aborted,
        "a failure after the plan row never leaves it started"
    );
    assert!(row.error.as_deref().is_some_and(|error| error.contains("conflict")));

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, Some(opencode()), "the source stays attached");
    let events = read_events(&deps, &session);
    assert!(
        !events.iter().any(|event| event.event_type == EventType::Switch),
        "an aborted switch records no switch event"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.body(), Ok(EventBody::Error(_)))),
        "the abort is visible in the log"
    );
}

#[test]
fn switch_target_turn_error_terminates_turn() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("wait error handoff")))
            .with("claude-code", Arc::new(WaitErrorAdapter)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "wait error");
    let store = deps.open_store().expect("store");

    let error = run_switch_with_input(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        "continue",
        &CancelToken::default(),
    )
    .expect_err("the target wait fails twice");
    assert!(matches!(error, CoreError::SwitchAborted(_)), "got {error:?}");

    let events = read_events(&deps, &session);
    let mut open_turn = None;
    let mut started = Vec::new();
    let mut ended = Vec::new();
    let mut target_detaches = 0;
    for event in &events {
        match event.body() {
            Ok(EventBody::TurnStarted(payload)) => {
                assert!(open_turn.is_none(), "a new turn started before the previous ended");
                open_turn = Some(payload.turn);
                started.push(payload.turn);
            }
            Ok(EventBody::TurnEnded(payload)) => {
                assert_eq!(open_turn.take(), Some(payload.turn), "turn.ended matches the open turn");
                ended.push(payload.turn);
            }
            Ok(EventBody::SessionExecutor(payload))
                if payload.action == AttachAction::Detach && payload.executor == claude() =>
            {
                target_detaches += 1;
            }
            _ => {}
        }
    }
    assert!(open_turn.is_none(), "no unterminated turn remains");
    assert_eq!(started, vec![1, 2], "both seeded attempts are numbered");
    assert_eq!(ended, vec![1, 2], "each failed attempt terminates its turn");
    assert_eq!(target_detaches, 2, "each failed attempt detaches the target");
    assert_eq!(
        store
            .list_switches(&session)
            .expect("switches")
            .into_iter()
            .next()
            .expect("row")
            .state,
        SwitchState::Aborted
    );
}

#[test]
fn switch_target_probe_fails_stays_on_source() {
    let tmp = tempdir().expect("tempdir");
    let target = FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new()).with_probe(ProbeResult {
        ok: false,
        version: None,
        path: None,
        detail: "binary missing".to_owned(),
    });
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("should not be written")))
            .with("claude-code", Arc::new(target)),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "probe failure");
    let store = deps.open_store().expect("store");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Blocked, Some(&opencode()))
        .expect("blocked");

    let error = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
    )
    .expect_err("probe failure aborts");
    assert!(matches!(error, CoreError::SwitchAborted(_)), "got {error:?}");

    let current = store.require_session(&session).expect("session");
    assert_eq!(current.executor, Some(opencode()), "still on the source");
    assert_eq!(
        current.state,
        vowp_protocol::SessionState::Blocked,
        "blocked stays blocked"
    );

    let row = store
        .list_switches(&session)
        .expect("switches")
        .into_iter()
        .next()
        .expect("row");
    assert_eq!(row.state, SwitchState::Aborted);
    assert!(row.error.as_deref().is_some_and(|error| error.contains("probe")));

    let events = read_events(&deps, &session);
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::Error(payload)) if payload.kind == vowp_protocol::ErrorKind::AdapterUnavailable
        )),
        "the abort is visible in the log"
    );
    assert!(
        !events.iter().any(|event| event.event_type == EventType::Capsule),
        "no capsule is written when the target cannot run"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Detach
        )),
        "the source is never detached"
    );
}

#[test]
fn switch_drain_timeout_interrupts_after_5s_fakeclock() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let signal_path = tmp.path().join("signals.txt");
    let ready_path = tmp.path().join("ready.txt");
    let source = Arc::new(ProcessAdapter::new(
        HarnessKind::Opencode,
        signal_path.clone(),
        ready_path.clone(),
    ));
    let capsule = Arc::new(capsule_adapter("drained handoff"));
    let mut config = config_for(tmp.path());
    config.capsule.executor = Some(claude());
    let deps = deps_with_config(
        &tmp,
        config,
        AdapterRegistry::new()
            .with("opencode", source)
            .with("claude-code", capsule),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "drain timeout");
    let store = deps.open_store().expect("store");

    // The real service turn owns the session log writer; the switch must wait for it.
    let worker_service = service.clone();
    let worker_session = session.clone();
    let worker_cancel = CancelToken::default();
    let worker = std::thread::spawn(move || {
        worker_service.run_turn(turn(&worker_session, opencode(), "long running turn"), &worker_cancel)
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut active_run = None;
    while Instant::now() < deadline {
        let open = store.open_runs().expect("open runs");
        if let Some(run) = open
            .into_iter()
            .find(|run| run.session_id.as_ref() == Some(&session) && run.pid.is_some())
        {
            active_run = Some(run);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let active_run = active_run.expect("the service run is active");

    // The fake child writes `ready` only after both traps are installed. Wait for it
    // before the switch can reach its signal phase; otherwise a fast fake clock can
    // deliver SIGINT while `sh` is still starting, and the marker is lost.
    wait_for_markers(&ready_path, &["ready"]);

    let advanced = clock.clone();
    let int_path = signal_path.clone();
    let term_path = signal_path.clone();
    let advancer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        advanced.advance(Duration::from_secs(5));
        // The child's `int` marker is the SIGINT acknowledgment. Advance to the SIGTERM
        // grace only after it exists: advancing first could deliver TERM while the INT
        // trap is still pending, and the TERM trap's exit would lose `int`. The child
        // starts with default signal dispositions (see `shell_command`), so this wait
        // observes a real trap rather than an inherited ignored SIGINT.
        wait_for_markers(&int_path, &["int"]);
        advanced.advance(Duration::from_secs(3));
        wait_for_markers(&term_path, &["term"]);
        advanced.advance(Duration::from_secs(3));
    });
    let before = clock.now();
    let switch = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
    )
    .expect("active-run switch completes");
    advancer.join().expect("advancer");
    let elapsed = clock.now().since(before);
    assert!(elapsed >= Duration::from_secs(5), "the drain waited for the fake 5 s");
    let result = worker.join().expect("worker").expect("run_turn finishes cleanly");
    assert_eq!(result.summary.outcome, RunOutcome::Killed);
    assert_eq!(switch.state, SwitchState::Completed);

    let signals = wait_for_markers(&signal_path, &["int", "term"]);
    let int_at = signals.find("int").expect("SIGINT marker");
    let term_at = signals.find("term").expect("SIGTERM marker");
    assert!(
        int_at < term_at,
        "drain sent SIGINT before SIGTERM; contents: {signals:?}"
    );

    let finished = store.run(&active_run.id).expect("run").expect("run row");
    assert_eq!(finished.outcome, Some(RunOutcome::Killed));
    assert!(finished.ended_at.is_some());

    let events = read_events(&deps, &session);
    let ended: Vec<_> = events
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::TurnEnded(payload)) => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(ended.len(), 1, "the runner's terminal event is not duplicated");
    assert_eq!(ended[0].stop_reason, StopReason::Interrupted);
    assert_eq!(ended[0].turn, 1, "the active turn number is preserved");
    assert_eq!(
        store.require_session(&session).expect("session").executor,
        Some(claude())
    );
}

#[test]
fn switch_locked_active_log_aborts_cleanly() {
    let tmp = tempdir().expect("tempdir");
    let clock = FakeClock::at(ts());
    let deps = deps_with_clock(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("unused")))
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
        clock.clone(),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "locked log");
    let store = deps.open_store().expect("store");
    let run = open_run(&session, None);
    store.insert_run(&run).expect("run row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Running, Some(&opencode()))
        .expect("running");

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
            body: EventBody::TurnStarted(vowp_protocol::TurnStarted { turn: 1 }),
            raw: None,
            ts: None,
        })
        .expect("turn started");
    store.index_event(&index_row(&appended)).expect("index");
    held.sync().expect("sync");

    let advanced = clock.clone();
    let advancer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        advanced.advance(Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(60));
        advanced.advance(Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(60));
        advanced.advance(Duration::from_secs(3));
    });
    let error = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
    )
    .expect_err("a held log aborts the switch instead of leaving it started");
    advancer.join().expect("advancer");
    assert!(
        matches!(error, CoreError::Store(vowp_store::StoreError::Locked { .. })),
        "got {error:?}"
    );

    let row = store
        .list_switches(&session)
        .expect("switches")
        .into_iter()
        .next()
        .expect("switch row");
    assert_eq!(row.state, SwitchState::Aborted, "the row is not left started");
    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Running);
    assert_eq!(current.executor, Some(opencode()), "the source stays live");
    let still_open = store.open_runs().expect("open runs");
    assert!(
        still_open.iter().any(|open| open.id == run.id),
        "drain never finishes a run owned by another thread"
    );
    assert!(
        !read_events(&deps, &session)
            .iter()
            .any(|event| event.event_type == EventType::Switch),
        "an aborted switch has no switch event"
    );
    drop(held);
}

#[test]
fn switch_seed_rejected_retries_capsule_only_then_aborts() {
    let tmp = tempdir().expect("tempdir");
    let target = Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new()).with_start_error("seed rejected"));
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("Seed retry handoff")))
            .with("claude-code", target.clone()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "seed rejection");
    service
        .run_turn(
            turn(&session, opencode(), "TAIL-UNIQUE-42, keep working"),
            &CancelToken::default(),
        )
        .expect("prior turn");
    let store = deps.open_store().expect("store");

    let error = run_switch_with_input(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        "continue the work",
        &CancelToken::default(),
    )
    .expect_err("both seed attempts fail");
    assert!(matches!(error, CoreError::SwitchAborted(_)), "got {error:?}");

    let starts = target.starts();
    assert_eq!(starts.len(), 2, "one seeded attempt then one capsule-only attempt");
    assert!(
        starts[0].prompt.contains("TAIL-UNIQUE-42"),
        "the first seed carries the last turns"
    );
    assert!(
        !starts[1].prompt.contains("TAIL-UNIQUE-42"),
        "the retry is the capsule only"
    );
    assert!(starts[1].prompt.contains("Seed retry handoff"));

    let row = store
        .list_switches(&session)
        .expect("switches")
        .into_iter()
        .next()
        .expect("row");
    assert_eq!(row.state, SwitchState::Aborted);
    assert!(row.error.as_deref().is_some_and(|error| error.contains("twice")));
    assert_eq!(
        store.require_session(&session).expect("session").executor,
        Some(opencode()),
        "the aborted switch re-attaches the source"
    );
    let last_attach = read_events(&deps, &session)
        .into_iter()
        .rev()
        .find_map(|event| match event.body() {
            Ok(EventBody::SessionExecutor(payload)) if payload.action == AttachAction::Attach => Some(payload.executor),
            _ => None,
        })
        .expect("source re-attach");
    assert_eq!(last_attach, opencode());
}

#[test]
fn switch_drained_limit_then_capsule_failure_restores_blocked() {
    let tmp = tempdir().expect("tempdir");
    let failing_capsule = capsule_adapter("unused").with_start_error("capsule writer unavailable");
    let deps = deps(
        &tmp,
        AdapterRegistry::new().with("opencode", Arc::new(failing_capsule)).with(
            "claude-code",
            Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
        ),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "drained limit");
    let store = deps.open_store().expect("store");
    let run = open_run(&session, None);
    store.insert_run(&run).expect("run row");
    store
        .set_session_state(&session, vowp_protocol::SessionState::Running, Some(&opencode()))
        .expect("running");
    append_event(
        &deps,
        &session,
        EventBody::TurnStarted(vowp_protocol::TurnStarted { turn: 1 }),
    );

    let writer = {
        let deps = deps.clone();
        let session = session.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            append_event(
                &deps,
                &session,
                EventBody::TurnEnded(vowp_protocol::TurnEnded {
                    turn: 1,
                    stop_reason: StopReason::Limit,
                    duration_ms: None,
                }),
            );
        })
    };

    let error = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::Failover),
        &CancelToken::default(),
    )
    .expect_err("capsule failure aborts after the drained limit");
    writer.join().expect("writer");
    assert!(matches!(error, CoreError::SwitchAborted(_)), "got {error:?}");

    let current = store.require_session(&session).expect("session");
    assert_eq!(
        current.state,
        vowp_protocol::SessionState::Blocked,
        "a drained limit keeps the session blocked when the switch aborts"
    );
    assert_eq!(current.executor, Some(opencode()));
    assert!(
        !read_events(&deps, &session)
            .iter()
            .any(|event| event.event_type == EventType::Switch)
    );
}

#[test]
fn seeded_switch_rate_and_context_do_not_block() {
    for (name, outcome) in [("rate", RunOutcome::Rate), ("context", RunOutcome::Context)] {
        let tmp = tempdir().expect("tempdir");
        let target = FakeAdapter::from_bodies(
            HarnessKind::ClaudeCode,
            ts(),
            vec![EventBody::AssistantMessage(AssistantMessage {
                text: "continued".to_owned(),
            })],
        )
        .with_summary(summary(outcome));
        let deps = deps(
            &tmp,
            AdapterRegistry::new()
                .with("opencode", Arc::new(capsule_adapter("non-blocking handoff")))
                .with("claude-code", Arc::new(target)),
        );
        let service = SessionService::new(deps.clone());
        let session = create_session(&service, &tmp, name);
        let store = deps.open_store().expect("store");

        let switch = run_switch_with_input(
            &deps,
            &request(&session, claude(), SwitchReason::Context),
            "continue",
            &CancelToken::default(),
        )
        .expect("seeded switch");
        assert_eq!(switch.state, SwitchState::Completed);
        let current = store.require_session(&session).expect("session");
        assert_eq!(
            current.state,
            vowp_protocol::SessionState::Idle,
            "{name} is not a blocked outcome (sessions.md §2)"
        );
        assert_eq!(current.executor, Some(claude()));
    }
}

#[test]
fn seeded_switch_ok_turn_clears_the_provider_limit() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("probe back")))
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session(&service, &tmp, "limit clear");
    let store = deps.open_store().expect("store");
    store
        .upsert_limit(&Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: LimitRowKind::Quota,
            resets_at: Some(ts().plus(Duration::from_secs(3600))),
            detail: "quota".to_owned(),
            since: ts(),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit row");
    assert!(store.limit_for("claude").expect("limit").is_some());

    run_switch_with_input(
        &deps,
        &request(&session, claude(), SwitchReason::ProbeBack),
        "continue",
        &CancelToken::default(),
    )
    .expect("seeded switch");

    assert!(
        store.limit_for("claude").expect("limit").is_none(),
        "an ok seeded turn clears the provider limit"
    );
}

#[test]
fn seeded_switch_outside_cwd_is_refused() {
    let tmp = tempdir().expect("tempdir");
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("cwd handoff")))
            .with(
                "claude-code",
                Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
            ),
    );
    let service = SessionService::new(deps.clone());
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    let session = create_session_at(&service, "outside cwd", opencode(), outside);
    let store = deps.open_store().expect("store");

    let error = run_switch_with_input(
        &deps,
        &request(&session, claude(), SwitchReason::User),
        "continue",
        &CancelToken::default(),
    )
    .expect_err("outside cwd refuses the seeded switch");
    assert!(matches!(error, CoreError::Refused(_)), "got {error:?}");
    assert!(store.list_switches(&session).expect("switches").is_empty());
    let current = store.require_session(&session).expect("session");
    assert_eq!(current.state, vowp_protocol::SessionState::Idle);
    assert_eq!(current.executor, Some(opencode()));
}

#[test]
fn failover_roundtrip_claude_to_opencode_and_back_preserves_capsule_content() {
    let tmp = tempdir().expect("tempdir");
    let claude_fake = Arc::new(FakeAdapter::from_bodies(
        HarnessKind::ClaudeCode,
        ts(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: draft_json("Claude source work"),
        })],
    ));
    let deps = deps(
        &tmp,
        AdapterRegistry::new()
            .with("opencode", Arc::new(capsule_adapter("Preserve this handoff")))
            .with("claude-code", claude_fake.clone()),
    );
    let service = SessionService::new(deps.clone());
    let session = create_session_at(&service, "failover roundtrip", claude(), tmp.path().join("project"));
    let store = deps.open_store().expect("store");
    service
        .run_turn(turn(&session, claude(), "start on claude"), &CancelToken::default())
        .expect("source turn");

    let first = run_switch(
        &deps,
        &request(&session, opencode(), SwitchReason::Limit),
        &CancelToken::default(),
    )
    .expect("claude -> opencode");
    let second = run_switch(
        &deps,
        &request(&session, claude(), SwitchReason::ProbeBack),
        &CancelToken::default(),
    )
    .expect("opencode -> claude");

    assert_eq!(first.state, SwitchState::Completed);
    assert_eq!(second.state, SwitchState::Completed);
    assert_eq!(
        store.require_session(&session).expect("session").executor,
        Some(claude())
    );

    let lineage = store.list_switches(&session).expect("lineage");
    assert_eq!(lineage.len(), 2);
    assert_eq!(lineage[0].from_executor, Some(claude()));
    assert_eq!(lineage[0].to_executor, opencode());
    assert_eq!(lineage[0].reason, SwitchReason::Limit);
    assert_eq!(lineage[1].from_executor, Some(opencode()));
    assert_eq!(lineage[1].to_executor, claude());
    assert_eq!(lineage[1].reason, SwitchReason::ProbeBack);

    let capsule = store
        .latest_capsule(&session)
        .expect("capsule")
        .expect("capsule exists");
    assert_eq!(capsule.intent, "Preserve this handoff");
    assert_eq!(
        capsule.from.executor,
        opencode(),
        "the second switch summarises the opencode executor"
    );

    // The next turn on the previously used claude harness is seeded from the capsule.
    service
        .run_turn(
            turn(&session, claude(), "continue after roundtrip"),
            &CancelToken::default(),
        )
        .expect("seeded turn after switching back");
    let starts = claude_fake.starts();
    let seeded = starts.last().expect("seeded start");
    assert!(seeded.prompt.contains("Preserve this handoff"));
    assert!(
        seeded.prompt.contains("start on claude"),
        "the last claude turn is part of the plain-text tail"
    );
    assert!(seeded.prompt.contains("continue after roundtrip"));
}
