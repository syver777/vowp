//! Hermetic `POST /api/v1/sessions/{id}/switch` tests: a temporary home, a real loopback
//! listener and real HTTP requests.
//!
//! The fixture has idle, running, switching and closed sessions, one active provider limit
//! and fake adapters that replay a capsule draft. No real harness, tmux server, process
//! environment, home directory or network beyond loopback is touched. The token is a fixed
//! test value.

mod support;

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_core::session::service::SessionService;
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_daemon::{ApiState, ApiToken, select_project};
use vowp_protocol::api::ApiError;
use vowp_protocol::{
    Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, Cost, EventBody, Executor, FakeClock,
    HarnessEvent, HarnessKind, Limit, LimitRowKind, MemoryLog, NodeName, ProbeResult, Project, ProjectName, RunOutcome,
    RunSummary, SessionId, SessionRole, SessionState, StartInput, StopReason, Switch, SwitchReason, SwitchState,
    Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, Store};
use vowp_tmux::fake::FakeWindows;

/// The fixed write token used by these tests.
const TOKEN: &str = "test-write-token";

/// The idle session's source executor.
const SOURCE: &str = "opencode:deepseek/deepseek-flash#max";

/// A target on another provider so the fixture's `claude` limit does not block it.
const TARGET: &str = "opencode:other/model#max";

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("epoch")
}

/// Return a valid capsule draft JSON document.
fn draft_json() -> String {
    json!({
        "intent": "carry the fixture work over",
        "constraints": [],
        "decisions": [],
        "state": {"done": ["work"], "in_progress": [], "blocked": [], "next_steps": ["continue"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    })
    .to_string()
}

/// A fake harness that probes successfully and replays fixed events.
struct FakeAdapter {
    kind: HarnessKind,
    events: Vec<HarnessEvent>,
    summary: RunSummary,
    probe: ProbeResult,
    starts: Arc<Mutex<Vec<StartInput>>>,
}

impl FakeAdapter {
    fn new(kind: HarnessKind, events: Vec<HarnessEvent>) -> Self {
        Self {
            kind,
            events,
            summary: RunSummary {
                exit_code: Some(0),
                outcome: RunOutcome::Ok,
                native_session_id: None,
                cost: Cost::unknown(),
                limit: None,
                result_text: None,
                stop_reason: StopReason::EndTurn,
            },
            probe: ProbeResult {
                ok: true,
                version: Some("fake-1.0".to_owned()),
                path: None,
                detail: "fake adapter".to_owned(),
            },
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A fake adapter that answers every turn with a valid capsule draft.
    fn capsule(kind: HarnessKind) -> Self {
        Self::new(
            kind,
            vec![HarnessEvent {
                ts: Some(epoch()),
                actor: vowp_protocol::Actor::Assistant,
                body: EventBody::AssistantMessage(AssistantMessage { text: draft_json() }),
                raw: None,
            }],
        )
    }
}

impl Adapter for FakeAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        self.probe.clone()
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::default()
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input);
        let (sender, receiver) = channel();
        for event in &self.events {
            let _ = sender.send(event.clone());
        }
        drop(sender);
        Ok(Box::new(FakeRun {
            receiver,
            summary: self.summary.clone(),
        }))
    }
}

/// One replayed fake run.
struct FakeRun {
    receiver: Receiver<HarnessEvent>,
    summary: RunSummary,
}

impl AdapterRun for FakeRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("fake adapter has no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        Ok(self.summary.clone())
    }
}

/// A temporary home with one project, one active limit and four switchable sessions.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    idle: SessionId,
    running: SessionId,
    switching: SessionId,
    closed: SessionId,
    deps: Deps,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let clock = FakeClock::at(epoch());

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.validate().expect("valid fixture config");

        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: epoch(),
                updated_at: epoch(),
            })
            .expect("project row");
        store
            .upsert_limit(&Limit {
                provider: "claude".to_owned(),
                harness: Some(HarnessKind::ClaudeCode),
                kind: LimitRowKind::Quota,
                resets_at: Some(epoch().plus(std::time::Duration::from_secs(3600))),
                detail: "fixture quota".to_owned(),
                since: epoch(),
                last_probe_at: None,
                probe_fail_streak: 0,
                concurrency_cap: None,
                cap_until: None,
            })
            .expect("limit row");
        drop(store);

        let cmd: Arc<dyn vowp_core::CommandRunner> = Arc::new(SystemRunner);
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: node.clone(),
            clock: Arc::new(clock),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(
                AdapterRegistry::new()
                    .with("opencode", Arc::new(FakeAdapter::capsule(HarnessKind::Opencode)))
                    .with(
                        "claude-code",
                        Arc::new(FakeAdapter::new(HarnessKind::ClaudeCode, Vec::new())),
                    ),
            ),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::clone(&cmd))),
            cmd,
            rng: Arc::new(FixedRng(0)),
        };
        let service = SessionService::new(deps.clone());
        let create = |title: &str| {
            service
                .create_session(vowp_protocol::NewSession {
                    project: Some(project.clone()),
                    job_id: None,
                    role: SessionRole::Worker,
                    title: title.to_owned(),
                    cwd: project_dir.clone(),
                    parent: None,
                    executor: Some(Executor::parse(SOURCE).expect("source executor")),
                })
                .expect("create session")
        };
        let idle = create("idle session");
        let running = create("running session");
        let switching = create("switching session");
        let closed = create("closed session");

        let store = deps.open_store().expect("session store");
        for (id, state) in [
            (&running, SessionState::Running),
            (&switching, SessionState::Switching),
            (&closed, SessionState::Closed),
        ] {
            store.set_session_state(id, state, None).expect("session state");
        }
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            idle,
            running,
            switching,
            closed,
            deps,
        }
    }

    /// Opens the served project read-only to inspect what a request wrote.
    fn read_store(&self) -> Store {
        Store::open_read_only(&self.paths, Some(&self.project)).expect("read-only store")
    }

    /// Builds request state with a token and the fixture's fake dependencies.
    fn state(&self, token: Option<&str>) -> ApiState {
        let selection = select_project(&self.paths, None).expect("project selection");
        assert_eq!(selection.project, self.project);
        assert_eq!(selection.node, self.node);
        ApiState::new(&self.paths, &selection)
            .with_api_token(token.map(ApiToken::new))
            .with_session_deps(self.deps.clone())
    }
}

/// Parses a response body as JSON.
fn body(response: &support::HttpResponse) -> Value {
    serde_json::from_str(&response.body).unwrap_or_else(|error| panic!("invalid JSON: {error}\n{}", response.body))
}

/// Asserts that `value` validates against the exported schema for `name`.
fn assert_schema(name: &str, value: &Value) {
    let schema =
        vowp_protocol::schema::schema_for_name(name).unwrap_or_else(|| panic!("schema {name} is not exported"));
    let validator =
        jsonschema::validator_for(&schema).unwrap_or_else(|error| panic!("schema {name} is invalid: {error}"));
    if !validator.is_valid(value) {
        let errors = validator
            .iter_errors(value)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        panic!("{name} document does not validate: {errors:?}\n{value}");
    }
}

/// Asserts a response is the JSON error document with `status` and `kind`.
fn assert_api_error(response: &support::HttpResponse, status: u16, kind: &str) -> ApiError {
    assert_eq!(response.status, status, "body: {}", response.body);
    let value = body(response);
    assert_schema("ApiError", &value);
    let error: ApiError = serde_json::from_value(value).expect("ApiError");
    assert_eq!(
        serde_json::to_value(error.error.kind).expect("kind"),
        Value::String(kind.to_owned())
    );
    error
}

/// Builds one switch request body.
fn switch_body(to: &str, force: bool) -> String {
    json!({ "to": to, "force": force }).to_string()
}

/// Asserts a response contains neither the served home nor the configured token.
fn assert_no_home_or_token(response: &support::HttpResponse, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!response.body.contains(&home), "response leaked the home path");
    assert!(!response.body.contains(TOKEN), "response leaked the token");
}

#[test]
fn switch_is_disabled_when_no_token_is_configured() {
    let fixture = Fixture::new();
    let server = support::Server::start_with_state(fixture.state(None));

    let response = support::post(
        server.address,
        &format!("/api/v1/sessions/{}/switch", fixture.idle),
        &switch_body(TARGET, false),
        Some("Bearer whatever"),
    );
    let error = assert_api_error(&response, 503, "config");
    assert!(
        error.error.message.contains("writes are disabled"),
        "{}",
        error.error.message
    );
    assert_no_home_or_token(&response, &fixture);

    let session = fixture.read_store().require_session(&fixture.idle).expect("session");
    assert_eq!(
        session.executor,
        Some(Executor::parse(SOURCE).expect("source executor"))
    );
}

#[test]
fn switch_rejects_missing_and_wrong_tokens_before_reading_the_body() {
    let fixture = Fixture::new();
    let server = support::Server::start_with_state(fixture.state(Some(TOKEN)));
    let path = format!("/api/v1/sessions/{}/switch", fixture.idle);

    for (authorization, label) in [
        (None, "missing token"),
        (Some("Bearer wrong-token"), "wrong token"),
        (Some("Basic test-write-token"), "wrong scheme"),
        (Some("Bearer "), "empty bearer"),
    ] {
        let response = support::post(server.address, &path, "{", authorization);
        let error = assert_api_error(&response, 401, "auth");
        assert!(
            error.error.message.contains("bearer"),
            "{label} returned an unclear message: {}",
            error.error.message
        );
        assert_eq!(
            response.header("www-authenticate"),
            Some("Bearer"),
            "{label} is missing the Bearer challenge"
        );
        assert_no_home_or_token(&response, &fixture);
    }

    let session = fixture.read_store().require_session(&fixture.idle).expect("session");
    assert_eq!(
        session.executor,
        Some(Executor::parse(SOURCE).expect("source executor"))
    );
}

#[test]
fn switch_returns_the_completed_switch_and_moves_the_session() {
    let fixture = Fixture::new();
    let server = support::Server::start_with_state(fixture.state(Some(TOKEN)));

    let response = support::post(
        server.address,
        &format!("/api/v1/sessions/{}/switch", fixture.idle),
        &switch_body(TARGET, false),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response, &fixture);
    let value = body(&response);
    assert_schema("Switch", &value);
    let switch: Switch = serde_json::from_value(value).expect("Switch");
    assert_eq!(switch.session_id, fixture.idle);
    assert_eq!(switch.state, SwitchState::Completed);
    assert_eq!(switch.reason, SwitchReason::User);
    assert_eq!(switch.error, None);
    assert_eq!(
        switch.from_executor,
        Some(Executor::parse(SOURCE).expect("source executor"))
    );
    assert_eq!(switch.to_executor, Executor::parse(TARGET).expect("target executor"));
    assert!(switch.capsule_id.is_some(), "the switch response carries its capsule");
    assert!(switch.ended_at.is_some());

    let store = fixture.read_store();
    let row = store
        .switch(&switch.id)
        .expect("switch row")
        .expect("switch row exists");
    assert_eq!(row.state, SwitchState::Completed);
    assert_eq!(row.to_executor, Executor::parse(TARGET).expect("target executor"));
    let session = store.require_session(&fixture.idle).expect("session");
    assert_eq!(
        session.executor,
        Some(Executor::parse(TARGET).expect("target executor"))
    );
    assert_eq!(session.state, SessionState::Idle);

    let detail = support::get(server.address, &format!("/api/v1/sessions/{}", fixture.idle));
    assert_eq!(detail.status, 200, "body: {}", detail.body);
    let detail: Value = body(&detail);
    assert_eq!(detail["session"]["executor"], json!(TARGET));
    assert_eq!(detail["session"]["state"], json!("idle"));
}

#[test]
fn switch_guards_unknown_running_switching_closed_and_limited_sessions() {
    let fixture = Fixture::new();
    let server = support::Server::start_with_state(fixture.state(Some(TOKEN)));
    let authorization = format!("Bearer {TOKEN}");

    let unknown = SessionId::new();
    assert_api_error(
        &support::post(
            server.address,
            &format!("/api/v1/sessions/{unknown}/switch"),
            &switch_body(TARGET, false),
            Some(&authorization),
        ),
        404,
        "not_found",
    );

    for (session, label) in [
        (&fixture.running, "running"),
        (&fixture.switching, "switching"),
        (&fixture.closed, "closed"),
    ] {
        let error = assert_api_error(
            &support::post(
                server.address,
                &format!("/api/v1/sessions/{session}/switch"),
                &switch_body(TARGET, false),
                Some(&authorization),
            ),
            409,
            "conflict",
        );
        assert!(
            error.error.message.contains(label) || (label == "running" && error.error.message.contains("running")),
            "{label} guard message is unclear: {}",
            error.error.message
        );
    }

    // The fixture marks `claude` limited; that guard runs before the target probe.
    let error = assert_api_error(
        &support::post(
            server.address,
            &format!("/api/v1/sessions/{}/switch", fixture.idle),
            &switch_body("claude-code:claude-sonnet-5-5@medium", false),
            Some(&authorization),
        ),
        409,
        "conflict",
    );
    assert!(error.error.message.contains("limited"), "{}", error.error.message);

    for (body, label) in [
        ("{".to_owned(), "malformed body"),
        (json!({"to": ""}).to_string(), "empty target"),
        (json!({"to": "not-an-executor"}).to_string(), "invalid target"),
        (json!({"to": TARGET, "force": "yes"}).to_string(), "invalid force"),
    ] {
        let error = assert_api_error(
            &support::post(
                server.address,
                &format!("/api/v1/sessions/{}/switch", fixture.idle),
                &body,
                Some(&authorization),
            ),
            400,
            "invalid_input",
        );
        assert!(!error.error.message.is_empty(), "{label} has no message");
    }
}
