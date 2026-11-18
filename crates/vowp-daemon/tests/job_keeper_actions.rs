//! Hermetic `POST /api/v1/jobs/{id}/nudge|restart|replan|fail` tests: a temporary home, a
//! real loopback listener and real HTTP requests.
//!
//! The fixture has jobs in `failed`, `stalled`, `parked`, `ready`, `running`, `pending`,
//! `done`, `cancelled` and `replanned`. Every successful response is validated against the
//! exported `JobActionResponse` schema, the queued request row is decoded with
//! `vowp_core`'s own type, the job row is checked unchanged, and the request is then
//! drained and applied through `vowp_core::requests::apply_direct` to prove it is the same
//! state change the CLI queues. The token is a fixed test value; no process environment,
//! real home, harness or tmux server is touched.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_core::requests::{self, Request};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemGit, SystemRunner};
use vowp_protocol::api::{ApiError, JobAction, JobActionResponse};
use vowp_protocol::{
    FakeClock, Job, JobId, JobKind, JobState, MemoryLog, NewJob, NodeName, ParkedReason, Project, ProjectName, Role,
    Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

/// The fixed write token used by these tests.
const TOKEN: &str = "test-keeper-token";

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-10T12:00:00.000Z").expect("epoch")
}

/// A temporary home with one project and jobs in every state the keeper routes guard.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    node: NodeName,
    clock: FakeClock,
    failed: JobId,
    stalled: JobId,
    parked: JobId,
    ready: JobId,
    running: JobId,
    pending: JobId,
    done: JobId,
    cancelled: JobId,
    replanned: JobId,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let clock = FakeClock::at(epoch());
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

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

        let insert = |id: &str| store.insert_job(&project, &new_job(id), "test").expect("job row").id;
        let failed = insert("failed-job");
        let stalled = insert("stalled-job");
        let parked = insert("parked-job");
        let ready = insert("ready-job");
        let running = insert("running-job");
        let pending = insert("pending-job");
        let done = insert("done-job");
        let cancelled = insert("cancelled-job");
        let replanned = insert("replanned-job");
        for (job, state, parked_reason) in [
            (&failed, JobState::Failed, None),
            (&stalled, JobState::Stalled, None),
            (&parked, JobState::Parked, Some(ParkedReason::Manual)),
            (&ready, JobState::Ready, None),
            (&running, JobState::Running, None),
            (&pending, JobState::Pending, None),
            (&done, JobState::Done, None),
            (&cancelled, JobState::Cancelled, None),
            (&replanned, JobState::Replanned, None),
        ] {
            store
                .set_job_state(
                    job,
                    state,
                    &StateChange {
                        note: None,
                        parked_reason,
                    },
                )
                .expect("job state");
        }
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            node,
            clock,
            failed,
            stalled,
            parked,
            ready,
            running,
            pending,
            done,
            cancelled,
            replanned,
        }
    }

    /// Opens the served project read-only to inspect what a request wrote.
    fn read_store(&self) -> Store {
        Store::open_read_only(&self.paths, Some(&self.project)).expect("read-only store")
    }

    /// Opens the served project writable for `apply_direct`.
    fn write_store(&self) -> Store {
        Store::open_writable(&self.paths, Some(&self.project))
            .expect("writable store")
            .with_clock(Arc::new(self.clock.clone()))
    }

    /// Returns every queued request row as `(key, value)`.
    fn queued_requests(&self) -> Vec<(String, String)> {
        self.read_store().kv_prefix("req:").expect("request rows")
    }

    /// Returns the full current job row.
    fn job(&self, job: &JobId) -> Job {
        self.read_store().require_job(job).expect("job row")
    }

    /// Returns the current state of `job`.
    fn state(&self, job: &JobId) -> JobState {
        self.job(job).state
    }

    /// Builds the dependencies `apply_direct` needs, with fakes for every service.
    fn deps(&self) -> Deps {
        let cmd: Arc<dyn vowp_core::CommandRunner> = Arc::new(SystemRunner);
        Deps {
            paths: self.paths.clone(),
            config: Arc::new(VowpConfig::with_defaults_for_tests()),
            node: self.node.clone(),
            clock: Arc::new(self.clock.clone()),
            log: Arc::new(MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(SystemGit::new(Arc::clone(&cmd))),
            cmd,
            rng: Arc::new(FixedRng(0)),
        }
    }

    /// Drains exactly one queued request, asserts it equals `expected`, applies it and
    /// asserts the queue is empty afterwards.
    fn apply_one(&self, expected: &Request) {
        let store = self.write_store();
        let drained = requests::drain(&store).expect("drain requests");
        assert_eq!(drained.len(), 1, "expected exactly one queued request: {drained:?}");
        assert_eq!(&drained[0].1, expected);
        requests::apply_direct(&self.deps(), &store, &drained[0].1).expect("apply request");
        drop(store);
        assert!(self.queued_requests().is_empty(), "apply must consume the request row");
    }

    /// Builds the action path for one job.
    fn path(&self, job: &JobId, action: JobAction) -> String {
        let suffix = match action {
            JobAction::Retry => "retry",
            JobAction::Park => "park",
            JobAction::Cancel => "cancel",
            JobAction::Nudge => "nudge",
            JobAction::Restart => "restart",
            JobAction::Replan => "replan",
            JobAction::Fail => "fail",
        };
        format!("/api/v1/jobs/{job}/{suffix}")
    }
}

/// Builds a minimal job input.
fn new_job(id: &str) -> NewJob {
    NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("{id} title"),
        kind: JobKind::Build,
        role: Role::Worker,
        profile: None,
        needs: Vec::new(),
        owns: Vec::new(),
        reads: Vec::new(),
        gates: None,
        priority: None,
        loop_: None,
        brief_text: None,
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

/// Asserts a response contains neither the served home nor the configured token.
fn assert_no_home_or_token(response: &support::HttpResponse, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!response.body.contains(&home), "response leaked the home path");
    assert!(!response.body.contains(TOKEN), "response leaked the token");
    assert!(
        !response
            .headers
            .iter()
            .any(|(_, value)| value.contains(TOKEN) || value.contains(&home)),
        "response header leaked the token or home"
    );
}

/// Asserts a `202` response is the schema-valid document for `job` and `action`, that its
/// `request` id names the one queued request row, and that the row decodes to `expected`.
fn assert_queued_response(
    response: &support::HttpResponse,
    fixture: &Fixture,
    job: &JobId,
    action: JobAction,
    expected: &Request,
) -> JobActionResponse {
    assert_eq!(response.status, 202, "body: {}", response.body);
    assert_no_home_or_token(response, fixture);
    let value = body(response);
    assert_schema("JobActionResponse", &value);
    let document: JobActionResponse = serde_json::from_value(value).expect("JobActionResponse");
    assert_eq!(document.job, *job);
    assert_eq!(document.action, action);

    let requests = fixture.queued_requests();
    assert_eq!(requests.len(), 1, "exactly one request row: {requests:?}");
    let (key, payload) = &requests[0];
    assert_eq!(
        key,
        &format!("req:{}:{}", fixture.project, document.request),
        "response request id must name the queued row"
    );
    let queued: Request = serde_json::from_str(payload).expect("stored request");
    assert_eq!(&queued, expected, "the queued request must match the CLI request");
    document
}

#[test]
fn keeper_nudge_queues_one_request_for_a_stalled_job() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");

    let before = fixture.job(&fixture.stalled);
    assert_eq!(before.state, JobState::Stalled);
    let expected = Request::NudgeJob {
        job: fixture.stalled.clone(),
    };
    let response = support::post(
        server.address,
        &fixture.path(&fixture.stalled, JobAction::Nudge),
        "",
        Some(&authorization),
    );
    let document = assert_queued_response(&response, &fixture, &fixture.stalled, JobAction::Nudge, &expected);
    assert!(!document.request.is_empty(), "the response must carry a request id");
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "the endpoint must not change the job row"
    );

    fixture.apply_one(&expected);
    assert_eq!(fixture.state(&fixture.stalled), JobState::Running);
}

#[test]
fn keeper_restart_queues_one_request_for_a_stalled_job() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");

    let before = fixture.job(&fixture.stalled);
    assert_eq!(before.state, JobState::Stalled);
    let expected = Request::RestartJob {
        job: fixture.stalled.clone(),
    };
    let response = support::post(
        server.address,
        &fixture.path(&fixture.stalled, JobAction::Restart),
        "",
        Some(&authorization),
    );
    let document = assert_queued_response(&response, &fixture, &fixture.stalled, JobAction::Restart, &expected);
    assert!(!document.request.is_empty(), "the response must carry a request id");
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "the endpoint must not change the job row"
    );

    fixture.apply_one(&expected);
    assert_eq!(fixture.state(&fixture.stalled), JobState::Running);
}

#[test]
fn keeper_replan_accepts_stalled_parked_and_failed() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");

    for job in [&fixture.stalled, &fixture.parked, &fixture.failed] {
        let before = fixture.job(job);
        let expected = Request::ReplanJob { job: job.clone() };
        let response = support::post(
            server.address,
            &fixture.path(job, JobAction::Replan),
            "",
            Some(&authorization),
        );
        assert_queued_response(&response, &fixture, job, JobAction::Replan, &expected);
        assert_eq!(fixture.job(job), before, "the endpoint must not change the job row");

        fixture.apply_one(&expected);
        assert_eq!(fixture.state(job), JobState::Replanned);
    }
}

#[test]
fn keeper_fail_requires_a_reason_and_queues_it() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");
    let path = fixture.path(&fixture.stalled, JobAction::Fail);
    let before = fixture.job(&fixture.stalled);

    for (request_body, label) in [
        ("".to_owned(), "missing body"),
        ("{}".to_owned(), "missing reason"),
        (json!({"reason": ""}).to_string(), "empty reason"),
        (json!({"reason": "   "}).to_string(), "blank reason"),
        ("{".to_owned(), "malformed JSON"),
        ("[]".to_owned(), "wrong JSON shape"),
        (json!({"reason": 1}).to_string(), "non-string reason"),
        (json!({"text": "x"}).to_string(), "unknown field"),
    ] {
        let response = support::post(server.address, &path, &request_body, Some(&authorization));
        let error = assert_api_error(&response, 400, "invalid_input");
        assert!(!error.error.message.is_empty(), "{label} has no message");
        assert_no_home_or_token(&response, &fixture);
    }
    let oversized = json!({"reason": "a".repeat(10_001)}).to_string();
    let response = support::post(server.address, &path, &oversized, Some(&authorization));
    let error = assert_api_error(&response, 400, "invalid_input");
    assert!(
        error.error.message.contains("10000"),
        "oversized reason message is unclear: {}",
        error.error.message
    );
    assert_no_home_or_token(&response, &fixture);
    assert!(fixture.queued_requests().is_empty(), "a rejected fail queued a request");
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "a rejected fail changed the job row"
    );

    let expected = Request::FailJob {
        job: fixture.stalled.clone(),
        reason: "owner gave up".to_owned(),
    };
    let response = support::post(
        server.address,
        &path,
        &json!({"reason": "  owner gave up  "}).to_string(),
        Some(&authorization),
    );
    assert_queued_response(&response, &fixture, &fixture.stalled, JobAction::Fail, &expected);
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "the endpoint must not change the job row"
    );

    fixture.apply_one(&expected);
    let failed = fixture.job(&fixture.stalled);
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.note, "owner gave up");
}

#[test]
fn keeper_actions_refuse_a_wrong_state_with_409() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");

    for (job, action, body, refusal) in [
        (&fixture.ready, JobAction::Nudge, "", "nudged"),
        (&fixture.running, JobAction::Nudge, "", "nudged"),
        (&fixture.pending, JobAction::Nudge, "", "nudged"),
        (&fixture.parked, JobAction::Restart, "", "restarted"),
        (&fixture.pending, JobAction::Restart, "", "restarted"),
        (&fixture.failed, JobAction::Fail, "{\"reason\":\"gave up\"}", "failed"),
        (&fixture.ready, JobAction::Replan, "", "replanned"),
        (&fixture.pending, JobAction::Replan, "", "replanned"),
        (&fixture.done, JobAction::Nudge, "", "nudged"),
        (&fixture.done, JobAction::Restart, "", "restarted"),
        (&fixture.done, JobAction::Replan, "", "replanned"),
        (&fixture.done, JobAction::Fail, "{\"reason\":\"gave up\"}", "failed"),
        (&fixture.cancelled, JobAction::Replan, "", "replanned"),
        (&fixture.replanned, JobAction::Replan, "", "replanned"),
    ] {
        let before = fixture.job(job);
        let response = support::post(server.address, &fixture.path(job, action), body, Some(&authorization));
        let error = assert_api_error(&response, 409, "conflict");
        assert!(
            error.error.message.contains(job.as_str()),
            "refusal must name the job: {}",
            error.error.message
        );
        assert!(
            error.error.message.contains(refusal),
            "refusal must use the CLI wording {refusal:?}: {}",
            error.error.message
        );
        assert_no_home_or_token(&response, &fixture);
        assert_eq!(fixture.job(job), before, "a refusal changed the job row");
        assert!(fixture.queued_requests().is_empty(), "a refusal queued a request row");
    }

    for (path, body) in [
        ("/api/v1/jobs/NOT_A_JOB/nudge".to_owned(), ""),
        ("/api/v1/jobs/NOT_A_JOB/restart".to_owned(), ""),
        ("/api/v1/jobs/NOT_A_JOB/replan".to_owned(), ""),
        ("/api/v1/jobs/NOT_A_JOB/fail".to_owned(), "{\"reason\":\"gave up\"}"),
    ] {
        let response = support::post(server.address, &path, body, Some(&authorization));
        let error = assert_api_error(&response, 400, "invalid_input");
        assert!(
            error.error.message.contains("job id"),
            "invalid-id message is unclear: {}",
            error.error.message
        );
        assert_no_home_or_token(&response, &fixture);
    }

    let unknown = JobId::parse("missing-keeper-job").expect("unknown job id");
    for (action, body) in [
        (JobAction::Nudge, ""),
        (JobAction::Restart, ""),
        (JobAction::Replan, ""),
        (JobAction::Fail, "{\"reason\":\"gave up\"}"),
    ] {
        let response = support::post(
            server.address,
            &fixture.path(&unknown, action),
            body,
            Some(&authorization),
        );
        assert_api_error(&response, 404, "not_found");
        assert_no_home_or_token(&response, &fixture);
    }

    assert!(
        fixture.queued_requests().is_empty(),
        "a rejected keeper request queued work: {:?}",
        fixture.queued_requests()
    );
    assert_eq!(fixture.state(&fixture.stalled), JobState::Stalled);
}

#[test]
fn keeper_actions_require_the_token() {
    let fixture = Fixture::new();
    let before = fixture.job(&fixture.stalled);

    let disabled = support::Server::start_for(&fixture.paths);
    for (action, body) in [
        (JobAction::Nudge, ""),
        (JobAction::Restart, ""),
        (JobAction::Replan, ""),
        (JobAction::Fail, "{\"reason\":\"gave up\"}"),
    ] {
        let response = support::post(
            disabled.address,
            &fixture.path(&fixture.stalled, action),
            body,
            Some("Bearer whatever"),
        );
        let error = assert_api_error(&response, 503, "config");
        assert!(
            error.error.message.contains("writes are disabled"),
            "{}",
            error.error.message
        );
        assert_no_home_or_token(&response, &fixture);
    }
    assert!(
        fixture.queued_requests().is_empty(),
        "a disabled write queued a request"
    );
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "a disabled write changed the job row"
    );

    let response = support::get(disabled.address, "/api/v1/jobs");
    assert_eq!(response.status, 200, "reads must stay token-free: {}", response.body);
    assert_no_home_or_token(&response, &fixture);

    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    for (authorization, label) in [
        (None, "missing token"),
        (Some("Bearer wrong-token"), "wrong token"),
        (Some("Basic test-keeper-token"), "wrong scheme"),
        (Some("Bearer "), "empty bearer"),
    ] {
        // The invalid id and malformed body prove authorization runs first: a 401 is not
        // a 400.
        for (action, body) in [
            (JobAction::Nudge, "{"),
            (JobAction::Restart, "{"),
            (JobAction::Replan, "{"),
            (JobAction::Fail, "{"),
        ] {
            let path = fixture.path(&fixture.stalled, action);
            let response = support::post(server.address, &path, body, authorization);
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

        let response = support::post(server.address, "/api/v1/jobs/NOT_A_JOB/replan", "{", authorization);
        assert_api_error(&response, 401, "auth");
    }

    assert!(
        fixture.queued_requests().is_empty(),
        "a rejected write queued a request"
    );
    assert_eq!(
        fixture.job(&fixture.stalled),
        before,
        "a rejected write changed the job row"
    );

    let response = support::get(server.address, "/api/v1/jobs");
    assert_eq!(response.status, 200, "reads must stay token-free: {}", response.body);
    assert_no_home_or_token(&response, &fixture);
}
