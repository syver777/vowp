//! Hermetic `POST /api/v1/inbox/{id}/answer` tests: a temporary home, a real loopback
//! listener and real HTTP requests.
//!
//! The fixture has a parked job with an open inbox item, a second open item without a job
//! and an already-answered item. Every successful response is validated against the exported
//! `InboxItem` schema and the stored request row is decoded with `vowp_core`'s own type.
//! The token is a fixed test value; no process environment, real home, harness or tmux
//! server is touched.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_core::requests::Request;
use vowp_protocol::api::ApiError;
use vowp_protocol::{
    Clock, FakeClock, InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, NewJob, NodeName, ParkedReason,
    Project, ProjectName, Role, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, StateChange, Store};

/// The fixed write token used by these tests.
const TOKEN: &str = "test-write-token";

/// A temporary home with one parked job and inbox items in several states.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    project: ProjectName,
    job_item: InboxId,
    plain_item: InboxId,
    answered_item: InboxId,
    job: JobId,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let clock = FakeClock::at(Timestamp::parse("2026-01-10T12:00:00.000Z").expect("clock"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node;
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("project row");

        let job = store
            .insert_job(
                &project,
                &NewJob {
                    id: JobId::parse("job-a").expect("job id"),
                    title: "job-a title".to_owned(),
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
                },
                "test",
            )
            .expect("job row")
            .id;
        store
            .set_job_state(
                &job,
                vowp_protocol::JobState::Parked,
                &StateChange {
                    note: None,
                    parked_reason: Some(ParkedReason::Question),
                },
            )
            .expect("park job");

        let job_item = InboxId::new();
        let plain_item = InboxId::new();
        let answered_item = InboxId::new();
        for (id, job_id, title, state, answer, answered_at) in [
            (
                job_item.clone(),
                Some(job.clone()),
                "job question",
                InboxState::Open,
                None,
                None,
            ),
            (plain_item.clone(), None, "plain question", InboxState::Open, None, None),
            (
                answered_item.clone(),
                None,
                "answered question",
                InboxState::Answered,
                Some("old answer"),
                Some(clock.now()),
            ),
        ] {
            store
                .insert_inbox(&InboxItem {
                    id,
                    project: Some(project.clone()),
                    job_id,
                    kind: InboxKind::Question,
                    title: title.to_owned(),
                    body: format!("{title} body"),
                    options: vec!["yes".to_owned(), "no".to_owned()],
                    state,
                    answer: answer.map(str::to_owned),
                    answered_by: answer.map(|_| "owner".to_owned()),
                    created_at: clock.now(),
                    answered_at,
                })
                .expect("inbox row");
        }
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            project,
            job_item,
            plain_item,
            answered_item,
            job,
        }
    }

    /// Opens the served project read-only to inspect what a request wrote.
    fn read_store(&self) -> Store {
        Store::open_read_only(&self.paths, Some(&self.project)).expect("read-only store")
    }

    /// Returns every queued request row as `(key, value)`.
    fn requests(&self) -> Vec<(String, String)> {
        self.read_store().kv_prefix("req:").expect("request rows")
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

/// Builds the answer JSON body.
fn answer_body(answer: &str) -> String {
    json!({ "answer": answer }).to_string()
}

#[test]
fn answer_is_disabled_when_no_token_is_configured() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);

    let response = support::post(
        server.address,
        &format!("/api/v1/inbox/{}/answer", fixture.job_item),
        &answer_body("yes"),
        Some("Bearer whatever"),
    );
    let error = assert_api_error(&response, 503, "config");
    assert!(
        error.error.message.contains("writes are disabled"),
        "{}",
        error.error.message
    );
    assert_no_home_or_token(&response, &fixture);

    assert_eq!(
        fixture
            .read_store()
            .inbox_item(&fixture.job_item)
            .expect("read")
            .expect("item")
            .state,
        InboxState::Open,
        "a disabled write answered the item"
    );
    assert!(fixture.requests().is_empty(), "a disabled write queued a request");

    let response = support::get(server.address, "/api/v1/inbox");
    assert_eq!(response.status, 200, "reads must stay token-free: {}", response.body);
}

#[test]
fn answer_rejects_missing_and_wrong_tokens_before_reading_the_body() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let path = format!("/api/v1/inbox/{}/answer", fixture.job_item);

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

    assert_eq!(
        fixture
            .read_store()
            .inbox_item(&fixture.job_item)
            .expect("read")
            .expect("item")
            .state,
        InboxState::Open,
        "a rejected write answered the item"
    );
    assert!(fixture.requests().is_empty(), "a rejected write queued a request");
}

#[test]
fn answer_stores_the_trimmed_text_and_one_request_row() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let path = format!("/api/v1/inbox/{}/answer", fixture.job_item);

    let response = support::post(
        server.address,
        &path,
        &answer_body("  yes  "),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(response.status, 202, "body: {}", response.body);
    assert_no_home_or_token(&response, &fixture);
    let value = body(&response);
    assert_schema("InboxItem", &value);
    let answered: InboxItem = serde_json::from_value(value).expect("answered InboxItem");
    assert_eq!(answered.id, fixture.job_item);
    assert_eq!(answered.state, InboxState::Answered);
    assert_eq!(answered.answer.as_deref(), Some("yes"));
    assert_eq!(answered.answered_by.as_deref(), Some("web"));
    assert!(answered.answered_at.is_some());

    let store = fixture.read_store();
    let stored = store.inbox_item(&fixture.job_item).expect("read").expect("item");
    assert_eq!(stored, answered);

    let requests = fixture.requests();
    assert_eq!(requests.len(), 1, "exactly one request row: {requests:?}");
    let (key, payload) = &requests[0];
    assert!(
        key.starts_with(&format!("req:{}:", fixture.project.as_str())),
        "request key is not scoped to the served project: {key}"
    );
    let request: Request = serde_json::from_str(payload).expect("stored request");
    assert_eq!(
        request,
        Request::AnswerInbox {
            id: fixture.job_item.clone()
        }
    );
    assert_eq!(stored.job_id, Some(fixture.job.clone()));
    assert_eq!(
        store.require_job(&fixture.job).expect("job").state,
        vowp_protocol::JobState::Parked,
        "the endpoint must not change job state"
    );

    let response = support::get(server.address, "/api/v1/inbox?state=answered");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response, &fixture);
    let page = body(&response);
    assert_schema("InboxPage", &page);
    assert_eq!(page["total"], 2);
    assert!(
        page["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["id"] == json!(fixture.job_item.as_str())),
        "the answered item is not in the answered page: {page}"
    );
}

#[test]
fn answer_without_a_job_writes_no_request_row() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));

    let response = support::post(
        server.address,
        &format!("/api/v1/inbox/{}/answer", fixture.plain_item),
        &answer_body("no"),
        Some(&format!("Bearer {TOKEN}")),
    );
    assert_eq!(response.status, 202, "body: {}", response.body);
    let answered: InboxItem = serde_json::from_value(body(&response)).expect("answered InboxItem");
    assert_eq!(answered.job_id, None);
    assert!(fixture.requests().is_empty(), "a jobless item queued a request");
}

#[test]
fn answer_rejects_bad_ids_bodies_and_states_without_writing() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(TOKEN));
    let authorization = format!("Bearer {TOKEN}");

    let error = assert_api_error(
        &support::post(
            server.address,
            "/api/v1/inbox/not-an-id/answer",
            &answer_body("yes"),
            Some(&authorization),
        ),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("inbox id"), "{}", error.error.message);

    let path = format!("/api/v1/inbox/{}/answer", fixture.job_item);
    let error = assert_api_error(
        &support::post(server.address, &path, &answer_body("   "), Some(&authorization)),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("empty"), "{}", error.error.message);

    let oversized = answer_body(&"a".repeat(10_001));
    let error = assert_api_error(
        &support::post(server.address, &path, &oversized, Some(&authorization)),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("10000"), "{}", error.error.message);

    for malformed in ["{", "[]", r#"{"text":"yes"}"#, r#"{"answer":1}"#] {
        assert_api_error(
            &support::post(server.address, &path, malformed, Some(&authorization)),
            400,
            "invalid_input",
        );
    }

    let unknown = InboxId::new();
    assert_api_error(
        &support::post(
            server.address,
            &format!("/api/v1/inbox/{unknown}/answer"),
            &answer_body("yes"),
            Some(&authorization),
        ),
        404,
        "not_found",
    );

    let error = assert_api_error(
        &support::post(
            server.address,
            &format!("/api/v1/inbox/{}/answer", fixture.answered_item),
            &answer_body("again"),
            Some(&authorization),
        ),
        409,
        "conflict",
    );
    assert!(error.error.message.contains("not open"), "{}", error.error.message);

    assert_eq!(
        fixture
            .read_store()
            .inbox_item(&fixture.job_item)
            .expect("read")
            .expect("item")
            .state,
        InboxState::Open,
        "a rejected request answered the job item"
    );
    assert!(fixture.requests().is_empty(), "a rejected request queued work");
}
