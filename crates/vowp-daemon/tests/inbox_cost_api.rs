//! Hermetic `/api/v1/inbox` and `/api/v1/cost` tests: a temporary home, a real loopback
//! listener and real GETs.
//!
//! The fixture has open, answered and dismissed inbox items and cost ledger rows from a
//! receipt, the price table and unknown sources. Every returned document is validated
//! against the schema exported by `vowp_protocol::schema`. No test here touches the real
//! home, the network beyond `127.0.0.1`, a tmux server or a harness.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_protocol::api::{ApiError, CostReport, InboxPage};
use vowp_protocol::{
    Clock, Cost, CostSource, FakeClock, InboxId, InboxItem, InboxKind, InboxState, LedgerEntry, LedgerKind, NodeName,
    Project, ProjectName, RunRole, Timestamp, Tokens, VowpConfig,
};
use vowp_store::{NodePaths, Store};

/// A temporary home with inbox items and cost rows from every source.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(timestamp("2026-01-10T12:00:00.000Z"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
        config.project.name = project.clone();
        config.project.path = Some(tmp.path().join("project").display().to_string());
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: tmp.path().join("project").display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config JSON"),
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("project row");

        for (title, state, answer, answered_at) in [
            ("open question", InboxState::Open, None, None),
            (
                "answered question",
                InboxState::Answered,
                Some("yes"),
                Some(timestamp("2026-01-01T01:30:00.000Z")),
            ),
            (
                "dismissed notice",
                InboxState::Dismissed,
                None,
                Some(timestamp("2026-01-01T02:30:00.000Z")),
            ),
        ] {
            store
                .insert_inbox(&InboxItem {
                    id: InboxId::new(),
                    project: Some(project.clone()),
                    job_id: None,
                    kind: InboxKind::Question,
                    title: title.to_owned(),
                    body: format!("{title} body"),
                    options: vec!["yes".to_owned(), "no".to_owned()],
                    state,
                    answer: answer.map(str::to_owned),
                    answered_by: answer.map(|_| "owner".to_owned()),
                    created_at: timestamp(match state {
                        InboxState::Open => "2026-01-01T00:00:00.000Z",
                        InboxState::Answered => "2026-01-01T01:00:00.000Z",
                        InboxState::Dismissed => "2026-01-01T02:00:00.000Z",
                    }),
                    answered_at,
                })
                .expect("inbox row");
        }

        // job-a has a receipt and an unknown entry, so its row is partially known and
        // `mixed`; job-b is unknown only, so its USD total must stay null; job-c is a
        // price-table estimate.
        append_cost(
            &store,
            &project,
            "job-a",
            "2026-01-02T10:00:00.000Z",
            RunRole::Worker,
            Cost::receipt(
                0.5,
                Tokens {
                    input: 100,
                    output: 20,
                    ..Tokens::default()
                },
            ),
        );
        append_cost(
            &store,
            &project,
            "job-a",
            "2026-01-02T11:00:00.000Z",
            RunRole::Worker,
            Cost {
                usd: None,
                tokens: Tokens {
                    input: 12,
                    output: 3,
                    ..Tokens::default()
                },
                source: CostSource::Unknown,
            },
        );
        append_cost(
            &store,
            &project,
            "job-b",
            "2026-01-03T10:00:00.000Z",
            RunRole::Verifier,
            Cost {
                usd: None,
                tokens: Tokens {
                    input: 7,
                    ..Tokens::default()
                },
                source: CostSource::Unknown,
            },
        );
        append_cost(
            &store,
            &project,
            "job-c",
            "2026-01-03T12:00:00.000Z",
            RunRole::Worker,
            Cost {
                usd: Some(0.25),
                tokens: Tokens::default(),
                source: CostSource::PriceTable,
            },
        );
        drop(store);

        Self { _tmp: tmp, paths }
    }
}

/// Appends one cost ledger row for `job`.
fn append_cost(store: &Store, project: &ProjectName, job: &str, at: &str, role: RunRole, cost: Cost) {
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: timestamp(at),
            kind: LedgerKind::Cost,
            project: Some(project.clone()),
            job_id: Some(vowp_protocol::JobId::parse(job).expect("job id")),
            run_id: None,
            session_id: None,
            role: Some(role),
            provider: Some("claude".to_owned()),
            harness: None,
            model: Some("model-a".to_owned()),
            cost: Some(cost),
            summary: "run cost".to_owned(),
            detail: json!({}),
        })
        .expect("cost ledger row");
}

/// Parses a fixed RFC 3339 test instant.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
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

#[test]
fn inbox_serves_pages_filtered_by_state() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);
    let home = fixture.paths.root().display().to_string();

    let response = support::get(server.address, "/api/v1/inbox");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "inbox leaked the home path");
    assert!(
        !response.body.to_ascii_lowercase().contains("token"),
        "inbox leaked a token"
    );
    let value = body(&response);
    assert_schema("InboxPage", &value);
    let page: InboxPage = serde_json::from_value(value).expect("InboxPage");
    assert_eq!(page.total, 1, "the default filter is open");
    assert_eq!(page.next, None);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title, "open question");
    assert_eq!(page.items[0].state, InboxState::Open);
    assert_schema(
        "InboxItem",
        &serde_json::to_value(&page.items[0]).expect("inbox item JSON"),
    );

    let response = support::get(server.address, "/api/v1/inbox?state=answered");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let page: InboxPage = serde_json::from_value(body(&response)).expect("InboxPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].title, "answered question");
    assert_eq!(page.items[0].answer.as_deref(), Some("yes"));

    let response = support::get(server.address, "/api/v1/inbox?state=dismissed");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let page: InboxPage = serde_json::from_value(body(&response)).expect("InboxPage");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].title, "dismissed notice");

    let error = assert_api_error(
        &support::get(server.address, "/api/v1/inbox?state=not-a-state"),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("not-a-state"), "{}", error.error.message);
}

#[test]
fn cost_serves_the_cli_document_with_sources_and_null_unknowns() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);
    let home = fixture.paths.root().display().to_string();

    let response = support::get(server.address, "/api/v1/cost");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(!response.body.contains(&home), "cost leaked the home path");
    assert!(
        !response.body.to_ascii_lowercase().contains("\"token\"")
            && !response.body.to_ascii_lowercase().contains("api-token"),
        "cost leaked a token"
    );
    let value = body(&response);
    assert_schema("CostReport", &value);
    let report: CostReport = serde_json::from_value(value.clone()).expect("CostReport");
    assert_eq!(report.by, "job", "the default grouping is job");
    assert_eq!(
        report.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
        vec!["job-a", "job-c", "job-b"],
        "rows are ordered by known USD descending, then key"
    );

    let job_a = &report.rows[0];
    assert_eq!(job_a.runs, 2);
    assert_eq!(job_a.usd, Some(0.5));
    assert_eq!(job_a.source, "mixed");
    assert_eq!(job_a.unknown_entries, 1);
    assert_eq!(job_a.tokens.input, 112);
    assert_eq!(job_a.tokens.output, 23);

    let job_c = &report.rows[1];
    assert_eq!(job_c.usd, Some(0.25));
    assert_eq!(job_c.source, "price-table");

    let job_b = &report.rows[2];
    assert_eq!(job_b.usd, None, "unknown costs are null, never zero");
    assert_eq!(job_b.source, "unknown");
    assert_eq!(job_b.unknown_entries, 1);
    assert_eq!(json!(job_b.usd), Value::Null, "the wire value is null");

    assert_eq!(report.total.runs, 4);
    assert_eq!(report.total.usd, Some(0.75));
    assert_eq!(report.total.unknown_entries, 2);
    assert_eq!(report.total.source, "mixed");
    for row in &report.rows {
        assert_schema("CostRow", &serde_json::to_value(row).expect("cost row JSON"));
    }
    assert_schema(
        "CostTotal",
        &serde_json::to_value(&report.total).expect("cost total JSON"),
    );

    let response = support::get(server.address, "/api/v1/cost?by=role&since=2026-01-03");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let report: CostReport = serde_json::from_value(body(&response)).expect("CostReport");
    assert_eq!(report.by, "role");
    assert_eq!(
        report.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
        vec!["worker", "verifier"]
    );
    assert_eq!(report.rows[0].source, "price-table");
    assert_eq!(report.rows[0].usd, Some(0.25));
    assert_eq!(report.rows[1].source, "unknown");
    assert_eq!(report.rows[1].usd, None);
    assert_eq!(report.total.runs, 2);
    assert_eq!(report.total.unknown_entries, 1);
    assert_eq!(report.total.source, "mixed");

    // `until` is inclusive and also accepts an RFC 3339 instant.
    let response = support::get(
        server.address,
        "/api/v1/cost?by=job&since=2026-01-02&until=2026-01-02T11:00:00.000Z",
    );
    assert_eq!(response.status, 200, "body: {}", response.body);
    let report: CostReport = serde_json::from_value(body(&response)).expect("CostReport");
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].key, "job-a");
    assert_eq!(report.rows[0].runs, 2);
    assert_eq!(report.rows[0].unknown_entries, 1);

    let response = support::get(server.address, "/api/v1/cost?by=day");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let report: CostReport = serde_json::from_value(body(&response)).expect("CostReport");
    assert_eq!(
        report.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
        vec!["2026-01-02", "2026-01-03"]
    );
    assert_eq!(report.rows[0].runs, 2);
    assert_eq!(report.rows[0].usd, Some(0.5));
    assert_eq!(report.rows[1].runs, 2);
    assert_eq!(report.rows[1].usd, Some(0.25));
}

#[test]
fn cost_rejects_bad_groups_and_timestamps() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);

    let error = assert_api_error(
        &support::get(server.address, "/api/v1/cost?by=job,role"),
        400,
        "invalid_input",
    );
    assert!(error.error.message.contains("job,role"), "{}", error.error.message);
    assert_api_error(
        &support::get(server.address, "/api/v1/cost?by=not-a-group"),
        400,
        "invalid_input",
    );
    assert_api_error(
        &support::get(server.address, "/api/v1/cost?since=not-a-date"),
        400,
        "invalid_input",
    );
    assert_api_error(
        &support::get(server.address, "/api/v1/cost?until=2026-13-01"),
        400,
        "invalid_input",
    );
}
