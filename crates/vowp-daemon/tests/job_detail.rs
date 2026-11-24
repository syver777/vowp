//! Hermetic tests for the job-detail slice: `GET /api/v1/jobs/{id}`, its `/runs` and
//! `/gates` sub-resources, and the `/jobs/{id}` page shell.
//!
//! A temporary home holds one job with two needs, runs recorded out of order, and gate
//! results from two attempts; a real loopback listener serves the daemon routes. Every
//! JSON document is checked against the schema exported by `vowp_protocol::schema`. The
//! page is a static HTML shell over the same three endpoints and the token-gated keeper
//! actions, so the tests pin its shell fields, its state-gated action lists and its write
//! wiring, then read the endpoints again to prove its data source. No browser, Node runtime,
//! harness, tmux server or real home is involved.

mod support;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use vowp_protocol::api::{ApiError, GateResultPage, RunPage};
use vowp_protocol::{
    CmdGate, Cost, Evidence, Executor, FakeClock, Gate, GateResult, Job, JobId, JobKind, JobState, LimitKind, NewJob,
    NodeName, Project, ProjectName, ReportGate, Role, Run, RunId, RunOutcome, RunPurpose, RunRole, Timestamp,
    VowpConfig,
};
use vowp_store::{NodePaths, RunFinish, StateChange, Store};

/// The fixed write token used by these tests; the read-only page must never leak it.
const WRITE_TOKEN: &str = "test-job-detail-token";

/// Values that must stay out of the page because it renders no sensitive job field.
const SECRET_WORKTREE: &str = "/tmp/fixture-secret-worktree";
const SECRET_CWD: &str = "/tmp/fixture-secret-cwd";
const SECRET_WINDOW: &str = "fixture-secret-window";

/// A fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-10T12:00:00.000Z").expect("epoch")
}

/// A temporary home with one project, one detailed job, three runs and two gate attempts.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    job: Job,
    early_run: RunId,
    middle_run: RunId,
    late_run: RunId,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");
        let base = epoch();

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.project.name = project.clone();
        config.project.path = Some(project_dir.display().to_string());
        config.validate().expect("valid fixture config");

        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(FakeClock::at(base)));
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: project_dir.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: base,
                updated_at: base,
            })
            .expect("project row");

        // A completed dependency gives the detail page a non-empty needs list.
        let dependency = store
            .insert_job(&project, &new_job("dep-a", JobKind::Build, Role::Worker), "test")
            .expect("dependency job");
        store
            .set_job_state(
                &dependency.id,
                JobState::Done,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("dependency state");

        let job = store
            .insert_job(
                &project,
                &NewJob {
                    needs: vec![dependency.id.clone()],
                    priority: Some(70),
                    ..new_job("job-a", JobKind::Test, Role::Worker)
                },
                "test",
            )
            .expect("job row");
        store
            .set_job_state(
                &job.id,
                JobState::Ready,
                &StateChange {
                    note: Some("waiting on the dependency".to_owned()),
                    parked_reason: None,
                },
            )
            .expect("job state");
        store
            .set_job_counters(&job.id, Some(2), None, None)
            .expect("job counters");
        store
            .set_job_workspace(&job.id, Some("job/job-a"), Some(SECRET_WORKTREE))
            .expect("job workspace");
        let job = store.job(&job.id).expect("read job").expect("job exists");

        // Insert the three runs out of order; the API must return them by started_at, then id.
        let late = run(&job.id, &node, base.plus(Duration::from_secs(120)), None, "fix");
        let early = run(
            &job.id,
            &node,
            base,
            Some("opencode:deepseek/deepseek-flash#max"),
            "start",
        );
        let middle = run(
            &job.id,
            &node,
            base.plus(Duration::from_secs(60)),
            Some("claude-code:claude-sonnet-5-5@medium"),
            "verify",
        );
        for run in [&late, &early, &middle] {
            store.insert_run(run).expect("run row");
        }
        store
            .finish_run(
                &early.id,
                &RunFinish {
                    outcome: RunOutcome::Ok,
                    exit_code: Some(0),
                    limit_kind: None,
                    cost: Cost::unknown(),
                    native_session_id: None,
                    detail: String::new(),
                },
            )
            .expect("finish early run");
        store
            .finish_run(
                &late.id,
                &RunFinish {
                    outcome: RunOutcome::Limit,
                    exit_code: Some(1),
                    limit_kind: Some(LimitKind::Quota),
                    cost: Cost::unknown(),
                    native_session_id: None,
                    detail: String::new(),
                },
            )
            .expect("finish late run");

        // Attempt 1 is the older round; attempt 2 is the newest and is inserted out of index
        // order, so the gates API must both select the attempt and sort by gate index.
        store
            .insert_gate_result(&gate_result(&job.id, 1, 0, false, base))
            .expect("old gate result");
        for (gate_index, passed) in [(2, true), (0, true), (1, false)] {
            store
                .insert_gate_result(&gate_result(&job.id, 2, gate_index, passed, base))
                .expect("new gate result");
        }
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            job,
            early_run: early.id,
            middle_run: middle.id,
            late_run: late.id,
        }
    }
}

/// Builds a minimal job input.
fn new_job(id: &str, kind: JobKind, role: Role) -> NewJob {
    NewJob {
        id: JobId::parse(id).expect("job id"),
        title: format!("{id} title"),
        kind,
        role,
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

/// Builds one run row; `executor` names the purpose for a readable fixture.
fn run(job: &JobId, node: &NodeName, started_at: Timestamp, executor: Option<&str>, purpose: &str) -> Run {
    let (role, purpose, provider) = match purpose {
        "verify" => (RunRole::Verifier, RunPurpose::Verify, Some("claude".to_owned())),
        "fix" => (RunRole::Worker, RunPurpose::Fix, None),
        _ => (RunRole::Worker, RunPurpose::Start, Some("deepseek".to_owned())),
    };
    Run {
        id: RunId::new(),
        job_id: Some(job.clone()),
        session_id: None,
        role,
        purpose,
        profile: None,
        rung: 0,
        executor: executor.map(|value| Executor::parse(value).expect("executor")),
        provider,
        node: node.clone(),
        native_session_id: None,
        pid: None,
        window: Some(SECRET_WINDOW.to_owned()),
        cwd: Some(SECRET_CWD.to_owned()),
        started_at,
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: "fixture detail".to_owned(),
    }
}

/// Builds one gate result for `attempt` at `gate_index`.
fn gate_result(job: &JobId, attempt: u32, gate_index: i32, passed: bool, ts: Timestamp) -> GateResult {
    let (gate, evidence) = match gate_index {
        1 => (
            Gate::OwnsRespected(true),
            Evidence::OwnsRespected {
                changed: Vec::new(),
                violations: Vec::new(),
            },
        ),
        2 => (
            Gate::Report(ReportGate::default()),
            Evidence::Report {
                front_matter: None,
                missing_fields: Vec::new(),
                problems: Vec::new(),
            },
        ),
        _ => (
            Gate::Cmd(CmdGate {
                run: "cargo test".to_owned(),
                ..CmdGate::default()
            }),
            Evidence::Cmd {
                command: "cargo test".to_owned(),
                exit_code: Some(if passed { 0 } else { 1 }),
                timed_out: false,
                tail: String::new(),
                duration_ms: 25,
            },
        ),
    };
    GateResult {
        id: None,
        job_id: job.clone(),
        run_id: None,
        attempt,
        gate_index,
        gate,
        passed,
        evidence,
        output_blob: None,
        duration_ms: 25,
        ts,
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

/// Asserts a body contains neither the temporary home nor the configured write token.
fn assert_no_home_or_token(body: &str, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!body.contains(&home), "body leaked the home path");
    assert!(!body.contains(WRITE_TOKEN), "body leaked the token value");
}

#[test]
fn job_detail_api_returns_one_job_and_a_404_for_an_unknown_id() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let response = support::get(server.address, &format!("/api/v1/jobs/{}", fixture.job.id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &fixture);
    let value = body(&response);
    assert_schema("Job", &value);
    let job: Job = serde_json::from_value(value).expect("Job");
    assert_eq!(job, fixture.job);
    assert_eq!(job.state, JobState::Ready);
    assert_eq!(job.kind, JobKind::Test);
    assert_eq!(job.role, Role::Worker);
    assert_eq!(job.priority, 70);
    assert_eq!(job.attempts, 2);
    assert_eq!(job.needs, vec![JobId::parse("dep-a").expect("dep id")]);
    assert_eq!(job.note, "waiting on the dependency");

    // A valid but unknown job id is the API's not-found document.
    let missing = support::get(server.address, "/api/v1/jobs/no-such-job");
    assert_no_home_or_token(&missing.body, &fixture);
    assert_api_error(&missing, 404, "not_found");

    // The list route still answers beside the new detail route.
    let list = support::get(server.address, "/api/v1/jobs");
    assert_eq!(list.status, 200, "body: {}", list.body);
    assert_schema("JobPage", &body(&list));
}

#[test]
fn job_detail_api_rejects_an_invalid_id_with_400() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    for path in [
        "/api/v1/jobs/NOT_A_JOB".to_owned(),
        "/api/v1/jobs/NOT_A_JOB/runs".to_owned(),
        "/api/v1/jobs/NOT_A_JOB/gates".to_owned(),
    ] {
        let response = support::get(server.address, &path);
        assert_no_home_or_token(&response.body, &fixture);
        assert_api_error(&response, 400, "invalid_input");
    }
}

#[test]
fn job_runs_api_returns_the_jobs_runs_in_order() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let response = support::get(server.address, &format!("/api/v1/jobs/{}/runs", fixture.job.id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &fixture);
    let value = body(&response);
    assert_schema("RunPage", &value);
    let page: RunPage = serde_json::from_value(value).expect("RunPage");
    assert_eq!(page.total, 3);
    assert_eq!(page.next, None);
    let ids: Vec<&str> = page.items.iter().map(|run| run.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            fixture.early_run.as_str(),
            fixture.middle_run.as_str(),
            fixture.late_run.as_str(),
        ],
        "runs must come back by started_at, then id"
    );
    assert_eq!(page.items[0].outcome, Some(RunOutcome::Ok));
    assert_eq!(page.items[1].ended_at, None);
    assert_eq!(page.items[2].outcome, Some(RunOutcome::Limit));
    assert_eq!(page.items[2].limit_kind, Some(LimitKind::Quota));

    // An unknown job never answers an empty page.
    let missing = support::get(server.address, "/api/v1/jobs/no-such-job/runs");
    assert_no_home_or_token(&missing.body, &fixture);
    assert_api_error(&missing, 404, "not_found");
}

#[test]
fn job_gates_api_returns_the_latest_attempt() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let response = support::get(server.address, &format!("/api/v1/jobs/{}/gates", fixture.job.id));
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_no_home_or_token(&response.body, &fixture);
    let value = body(&response);
    assert_schema("GateResultPage", &value);
    let page: GateResultPage = serde_json::from_value(value).expect("GateResultPage");
    assert_eq!(page.total, 3);
    assert_eq!(page.next, None);
    assert!(
        page.items.iter().all(|result| result.attempt == 2),
        "only the newest attempt must be returned: {:?}",
        page.items
    );
    let indexes: Vec<i32> = page.items.iter().map(|result| result.gate_index).collect();
    assert_eq!(indexes, vec![0, 1, 2], "gate results must come back by gate index");
    assert_eq!(
        page.items.iter().map(|result| result.passed).collect::<Vec<_>>(),
        vec![true, false, true]
    );

    let missing = support::get(server.address, "/api/v1/jobs/no-such-job/gates");
    assert_no_home_or_token(&missing.body, &fixture);
    assert_api_error(&missing, 404, "not_found");
}

#[test]
fn job_detail_page_serves_the_shell_fields_and_its_data_sources() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let page = support::get(server.address, &format!("/jobs/{}", fixture.job.id));
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "job page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"job\"",
        "/api/v1/jobs/",
        "\"/runs\"",
        "\"/gates\"",
        "const STREAM_URL = \"/api/v1/jobs/\" + encodeURIComponent(jobId) + \"/stream\";",
        "new EventSource(STREAM_URL)",
        "addEventListener(\"job\"",
        "addEventListener(\"heartbeat\"",
        "startFallbackPolling",
        "stopFallbackPolling",
        "Reconnecting...",
        "typeof EventSource !== \"function\"",
        "Last updated: ",
        "Job not found.",
        "Could not load the job",
        "Back to jobs",
        "href=\"/jobs\"",
        "/sessions?job=",
        "Jobs</a>",
        "aria-current=\"page\"",
        "ID",
        "State",
        "Kind",
        "Role",
        "Priority",
        "Attempts",
        "Updated",
        "Needs",
        "Note",
        "Runs",
        "Purpose",
        "Executor",
        "Provider",
        "Started",
        "Ended",
        "Exit",
        "Outcome",
        "Limit",
        "Gates",
        "Gate index",
        "Passed",
        "Duration",
        "Timestamp",
        "No runs.",
        "No gate results.",
        "<noscript>The job page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "job page is missing {needle:?}");
    }

    // The page carries the keeper controls but never duplicates the jobs-page
    // retry/park/cancel actions or their routes.
    for forbidden in ["/retry", "/park", "/cancel"] {
        assert!(
            !page.body.contains(forbidden),
            "job page must not carry the jobs-page action route {forbidden:?}"
        );
    }

    // The data source is the same three endpoints the page fetches. Read them again and
    // compare the values the shell would render.
    let job_response = support::get(server.address, &format!("/api/v1/jobs/{}", fixture.job.id));
    assert_eq!(job_response.status, 200, "body: {}", job_response.body);
    let job: Job = serde_json::from_value(body(&job_response)).expect("Job");
    let runs_response = support::get(server.address, &format!("/api/v1/jobs/{}/runs", fixture.job.id));
    assert_eq!(runs_response.status, 200, "body: {}", runs_response.body);
    let runs: RunPage = serde_json::from_value(body(&runs_response)).expect("RunPage");
    let gates_response = support::get(server.address, &format!("/api/v1/jobs/{}/gates", fixture.job.id));
    assert_eq!(gates_response.status, 200, "body: {}", gates_response.body);
    let gates: GateResultPage = serde_json::from_value(body(&gates_response)).expect("GateResultPage");

    assert_eq!(job.id.as_str(), fixture.job.id.as_str());
    assert_eq!(job.state.as_str(), "ready");
    assert_eq!(job.kind, JobKind::Test);
    assert_eq!(job.role, Role::Worker);
    assert_eq!(job.attempts, 2);
    assert_eq!(job.note, "waiting on the dependency");
    assert_eq!(runs.total, 3);
    assert_eq!(gates.total, 3);
    assert!(gates.items.iter().all(|result| result.attempt == 2));
    assert_no_home_or_token(&page.body, &fixture);
}

#[test]
fn job_detail_page_prints_no_home_path_token_or_worktree() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let page = support::get(server.address, &format!("/jobs/{}", fixture.job.id));
    assert_eq!(page.status, 200, "body: {}", page.body);
    assert_no_home_or_token(&page.body, &fixture);

    // The stored job and runs carry these values, but the shell must not name or render
    // them: no field accessor and no fixture value may appear in the page.
    for forbidden in [
        "worktree",
        "brief",
        "owns",
        "cwd",
        "window",
        ".worktree",
        ".brief",
        ".owns",
        ".cwd",
        ".window",
    ] {
        assert!(
            !page.body.to_ascii_lowercase().contains(forbidden),
            "job page must not mention {forbidden:?}"
        );
    }
    for value in [SECRET_WORKTREE, SECRET_CWD, SECRET_WINDOW] {
        assert!(
            !page.body.contains(value),
            "job page rendered the sensitive fixture value {value:?}"
        );
    }
}

#[test]
fn job_detail_page_offers_keeper_actions_only_when_allowed() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let page = support::get(server.address, &format!("/jobs/{}", fixture.job.id));
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "job page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"unlock\"",
        "id=\"unlock-form\"",
        "id=\"actions-card\"",
        "id=\"actions\"",
        "Keeper actions",
        "Unlock keeper actions",
        "vowp-api-token",
        "Authorization",
        "Bearer ",
        "method: \"POST\"",
        "Action queued; the supervisor applies it on the next tick.",
        "Writes are disabled: the daemon was started without a write key.",
        "The key was rejected. Unlock with the current daemon write key.",
        "Unlock to use the keeper actions.",
        "No keeper actions in this state.",
        "A fail reason is required.",
        "Nudge",
        "Restart",
        "Replan",
        "Fail",
        "/nudge",
        "/restart",
        "/replan",
        "/fail",
        "NUDGE_STATES",
        "RESTART_STATES",
        "REPLAN_STATES",
        "FAIL_STATES",
    ] {
        assert!(page.body.contains(needle), "job page is missing {needle:?}");
    }

    // The state lists are the page's gate: nudge, restart and fail only for a stalled
    // job; replan also for parked and failed. Terminal states never appear.
    for rule in [
        "const NUDGE_STATES = [\"stalled\"];",
        "const RESTART_STATES = [\"stalled\"];",
        "const REPLAN_STATES = [\"stalled\", \"parked\", \"failed\"];",
        "const FAIL_STATES = [\"stalled\"];",
    ] {
        assert!(page.body.contains(rule), "job page is missing the state rule {rule:?}");
    }
    for line in page.body.lines().filter(|line| line.contains("_STATES = [")) {
        for state in ["done", "cancelled", "replanned"] {
            assert!(
                !line.contains(&format!("\"{state}\"")),
                "keeper action state list must not allow {state}: {line}"
            );
        }
    }
    assert!(
        page.body.contains("states.includes(job.state)"),
        "the keeper controls must be gated by the job state"
    );
    assert!(
        page.body.contains("await load()"),
        "a queued keeper action must refresh the job and its runs/gates"
    );

    assert_no_home_or_token(&page.body, &fixture);
}

#[test]
fn job_detail_page_renders_not_found_for_an_unknown_job() {
    let fixture = Fixture::new();
    let server = support::Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    // The page shell is served for any id; the API's 404 drives its not-found line.
    let missing = support::get(server.address, "/api/v1/jobs/no-such-job");
    assert_api_error(&missing, 404, "not_found");

    let page = support::get(server.address, "/jobs/no-such-job");
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "job page must be text/html, got {content_type:?}"
    );
    for needle in [
        "Job not found.",
        "<a href=\"/jobs\">Back to jobs</a>",
        "<a href=\"/jobs\" aria-current=\"page\">Jobs</a>",
        "<a href=\"/\">Board</a>",
        "<noscript>The job page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "not-found page is missing {needle:?}");
    }
    assert_no_home_or_token(&page.body, &fixture);
}
