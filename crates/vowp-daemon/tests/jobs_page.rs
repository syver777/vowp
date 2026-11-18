//! Hermetic `/jobs` page and job-action consumer tests: a temporary home, a real loopback
//! listener and real HTTP requests.
//!
//! The page is a static HTML shell, so these tests pin that the daemon serves it as
//! `text/html`, that it fetches `/api/v1/jobs` on load and every five seconds, that it
//! carries the job fields, the state-to-action mapping, the cancel confirmation and the
//! shared unlock panel, and that the same fixture backs the API and the existing job-action
//! writes. No browser, Node runtime, harness, tmux server or real home is involved.

mod support;

use std::sync::Arc;

use serde_json::Value;
use tempfile::TempDir;
use vowp_protocol::api::JobPage;
use vowp_protocol::{
    FakeClock, JobId, JobKind, JobState, NewJob, ParkedReason, Project, ProjectName, Role, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, StateChange, Store};

/// The fixed write token used by these tests.
const WRITE_TOKEN: &str = "test-jobs-page-token";

/// A fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-10T12:00:00.000Z").expect("epoch")
}

/// A temporary home with one project and jobs covering the page's action mapping.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    failed: JobId,
    stalled: JobId,
    parked: JobId,
    ready: JobId,
    running: JobId,
    done: JobId,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let project = ProjectName::parse("proj").expect("project name");
        let node = vowp_protocol::NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node;
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.validate().expect("valid fixture config");

        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(FakeClock::at(epoch())));
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
        let done = insert("done-job");
        for (job, state, parked_reason) in [
            (&failed, JobState::Failed, None),
            (&stalled, JobState::Stalled, None),
            (&parked, JobState::Parked, Some(ParkedReason::Manual)),
            (&ready, JobState::Ready, None),
            (&running, JobState::Running, None),
            (&done, JobState::Done, None),
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
            failed,
            stalled,
            parked,
            ready,
            running,
            done,
        }
    }

    /// Returns every fixture job id and its state.
    fn states(&self) -> Vec<(&JobId, JobState)> {
        vec![
            (&self.failed, JobState::Failed),
            (&self.stalled, JobState::Stalled),
            (&self.parked, JobState::Parked),
            (&self.ready, JobState::Ready),
            (&self.running, JobState::Running),
            (&self.done, JobState::Done),
        ]
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

/// Asserts a body contains neither the temporary home nor the configured token value.
fn assert_no_home_or_token(body: &str, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!body.contains(&home), "body leaked the home path");
    assert!(!body.contains(WRITE_TOKEN), "body leaked the token value");
}

#[test]
fn jobs_page_serves_the_shell_fields_and_state_action_mapping() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let page = support::get(server.address, "/jobs");
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "jobs page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"jobs\"",
        "fetch(\"/api/v1/jobs\")",
        "const REFRESH_MS = 5000;",
        "setInterval(refresh, REFRESH_MS)",
        "Last updated",
        "Showing data from ",
        "Could not load the jobs",
        "No jobs.",
        "sessionStorage",
        "Authorization",
        "Bearer",
        "window.confirm(",
        "Cancel job ",
        "window.prompt(",
        "Park reason (optional) for ",
        "JSON.stringify(payload)",
        "\"/retry\"",
        "\"/park\"",
        "\"/cancel\"",
        "response.status === 202",
        "response.status === 401",
        "response.status === 503",
        "Writes are disabled",
        "queued",
        "ID",
        "State",
        "Kind",
        "Role",
        "Priority",
        "Needs",
        "Attempts",
        "Updated",
        "Note",
        "Actions",
        "Retry",
        "Park",
        "Cancel",
        "aria-current=\"page\"",
        "href=\"/jobs\"",
        "function jobIdCell(job)",
        "link.href = \"/jobs/\" + encodeURIComponent(job.id);",
        "jobIdCell(job)",
        "<noscript>The jobs page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "jobs page is missing {needle:?}");
    }

    // The page reads jobs token-free and every write uses the bearer header to one of the
    // existing action routes; it must not invent a second API path.
    assert!(
        page.body.contains("method: \"POST\""),
        "the jobs page must POST its actions"
    );
    assert!(
        page.body
            .contains("const ACTION_PATHS = { retry: \"/retry\", park: \"/park\", cancel: \"/cancel\" };"),
        "action URLs must be built from the documented action suffixes"
    );

    // The state/action mapping is explicit: retry is limited to failed, stalled and parked;
    // park and cancel apply to every non-terminal state; terminal states offer no action.
    for needle in [
        "const RETRY_STATES = [\"failed\", \"stalled\", \"parked\"];",
        "const TERMINAL_STATES = [\"done\", \"cancelled\", \"replanned\"];",
        "function canRetry(state)",
        "return RETRY_STATES.includes(state);",
        "function canParkOrCancel(state)",
        "return !TERMINAL_STATES.includes(state);",
        "if (!unlocked)",
        "if (canRetry(job.state))",
        "if (canParkOrCancel(job.state))",
    ] {
        assert!(
            page.body.contains(needle),
            "jobs page is missing the state/action mapping {needle:?}"
        );
    }
    let retry_line = page
        .body
        .lines()
        .find(|line| line.contains("const RETRY_STATES"))
        .expect("retry state list");
    for state in [
        "pending",
        "ready",
        "running",
        "gating",
        "reviewing",
        "fixing",
        "merging",
        "done",
        "cancelled",
        "replanned",
    ] {
        assert!(
            !retry_line.contains(&format!("\"{state}\"")),
            "retry must not be offered for {state:?}: {retry_line}"
        );
    }
    let terminal_line = page
        .body
        .lines()
        .find(|line| line.contains("const TERMINAL_STATES"))
        .expect("terminal state list");
    for state in ["pending", "ready", "running", "failed", "stalled", "parked"] {
        assert!(
            !terminal_line.contains(&format!("\"{state}\"")),
            "{state:?} is not terminal: {terminal_line}"
        );
    }

    // Reads stay token-free and the page never prints a home path or the token value.
    assert_no_home_or_token(&page.body, &fixture);
    assert!(
        !page.body.contains("VOWP_API_TOKEN"),
        "the page must not name the daemon environment secret"
    );

    // The word token belongs only to the unlock panel; the rest of the page says "key".
    let unlock_start = page.body.find("<section id=\"unlock\"").expect("unlock panel");
    let unlock_end = page.body[unlock_start..]
        .find("</section>")
        .map(|end| end + unlock_start + "</section>".len())
        .expect("unlock panel end");
    let lowered = page.body.to_ascii_lowercase();
    for (index, _) in lowered.match_indices("token") {
        assert!(
            index >= unlock_start && index + "token".len() <= unlock_end,
            "the word token appears outside the unlock panel at byte {index}"
        );
    }

    // The page's data source is the jobs API; the same fixture must return the exact
    // stored rows the table renders.
    let response = support::get(server.address, "/api/v1/jobs");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &fixture);
    let value = body(&response);
    assert_schema("JobPage", &value);
    let page: JobPage = serde_json::from_value(value).expect("JobPage");
    assert_eq!(page.total, 6);
    assert_eq!(page.next, None);
    let returned: Vec<(String, JobState)> = page
        .items
        .iter()
        .map(|job| (job.id.as_str().to_owned(), job.state))
        .collect();
    for (job, state) in fixture.states() {
        assert!(
            returned.contains(&(job.as_str().to_owned(), state)),
            "the API is missing {job} in {state:?}: {returned:?}"
        );
    }
}

#[test]
fn job_actions_still_answer_beside_the_page() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));
    let authorization = format!("Bearer {WRITE_TOKEN}");

    let before: Vec<(String, JobState)> = {
        let response = support::get(server.address, "/api/v1/jobs");
        let page: JobPage = serde_json::from_value(body(&response)).expect("JobPage");
        page.items
            .iter()
            .map(|job| (job.id.as_str().to_owned(), job.state))
            .collect()
    };

    for (job, action, request_body) in [
        (&fixture.failed, "retry", ""),
        (&fixture.ready, "park", "{\"reason\":\"owner said stop\"}"),
        (&fixture.running, "cancel", ""),
    ] {
        let path = format!("/api/v1/jobs/{job}/{action}");
        let response = support::post(server.address, &path, request_body, Some(&authorization));
        assert_eq!(response.status, 202, "{path} body: {}", response.body);
        assert_no_home_or_token(&response.body, &fixture);
        let value = body(&response);
        assert_schema("JobActionResponse", &value);
        assert_eq!(value["job"], Value::String(job.as_str().to_owned()));
        assert_eq!(value["action"], Value::String(action.to_owned()));
        assert!(!value["request"].as_str().unwrap_or_default().is_empty());
    }

    // The page is a consumer: the routes queue requests and leave every job row unchanged.
    let response = support::get(server.address, "/api/v1/jobs");
    let page: JobPage = serde_json::from_value(body(&response)).expect("JobPage");
    let after: Vec<(String, JobState)> = page
        .items
        .iter()
        .map(|job| (job.id.as_str().to_owned(), job.state))
        .collect();
    assert_eq!(after, before, "a queued action must not change a job row");

    let missing = support::post(
        server.address,
        &format!("/api/v1/jobs/{}/retry", fixture.done),
        "",
        Some(&authorization),
    );
    assert_eq!(missing.status, 409, "a done job is not retryable: {}", missing.body);

    let unauthorized = support::post(
        server.address,
        &format!("/api/v1/jobs/{}/retry", fixture.failed),
        "",
        None,
    );
    assert_eq!(
        unauthorized.status, 401,
        "the page's write route must stay token-gated: {}",
        unauthorized.body
    );
}

#[test]
fn unknown_paths_still_return_the_json_404() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);

    // `/jobs/{id}` is now the static detail page, not a fallback path; an unknown nested
    // path and an unknown page still get the API's JSON 404.
    for path in ["/jobs/missing/extra", "/no-such-page"] {
        let response = support::get(server.address, path);
        assert_eq!(response.status, 404, "{path} body: {}", response.body);
        let value = body(&response);
        assert_schema("ApiError", &value);
        assert!(
            !response.body.contains(&fixture.paths.root().display().to_string()),
            "{path} leaked the home path"
        );
    }
}
