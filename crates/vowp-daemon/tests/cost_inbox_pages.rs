//! Hermetic cost and inbox page tests: a temporary home, a real loopback listener and real GETs.
//!
//! Both pages are static HTML shells served by the daemon. These tests pin the `text/html`
//! response, the API fetch paths and selectors, the rendered column labels, the unknown-cost
//! wording, the `<noscript>` fallback, the navigation links and the absence of a home path.
//! The inbox shell also carries the token unlock and answer form; the test pins those code
//! paths and the no-token-in-URL behavior, but no token value is present. The same test then
//! reads the cost and inbox APIs from the same fixture, which is the only data source the
//! pages use. No browser, Node runtime, harness or real home is involved.

mod support;

use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use vowp_protocol::api::{CostReport, InboxPage};
use vowp_protocol::{
    Clock, Cost, CostSource, FakeClock, InboxId, InboxItem, InboxKind, InboxState, JobId, JobKind, LedgerEntry,
    LedgerKind, NewJob, NodeName, Project, ProjectName, Role, RunRole, Timestamp, Tokens, VowpConfig,
};
use vowp_store::{NodePaths, Store};

/// A temporary home with inbox items in every state and cost rows from every source.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let clock = FakeClock::at(timestamp("2026-01-10T12:00:00.000Z"));
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

        store
            .insert_job(&project, &new_job("job-a", JobKind::Build, Role::Worker), "test")
            .expect("job a");

        for (title, kind, state, answer, answered_at, created_at, job) in [
            (
                "open question",
                InboxKind::Question,
                InboxState::Open,
                None,
                None,
                "2026-01-01T00:00:00.000Z",
                Some("job-a"),
            ),
            (
                "answered question",
                InboxKind::Question,
                InboxState::Answered,
                Some("yes"),
                Some("2026-01-01T01:30:00.000Z"),
                "2026-01-01T01:00:00.000Z",
                None,
            ),
            (
                "dismissed notice",
                InboxKind::Notice,
                InboxState::Dismissed,
                None,
                Some("2026-01-01T02:30:00.000Z"),
                "2026-01-01T02:00:00.000Z",
                None,
            ),
        ] {
            store
                .insert_inbox(&InboxItem {
                    id: InboxId::new(),
                    project: Some(project.clone()),
                    job_id: job.map(|id| JobId::parse(id).expect("job id")),
                    kind,
                    title: title.to_owned(),
                    body: format!("{title} body with a short preview worth showing"),
                    options: vec!["yes".to_owned(), "no".to_owned()],
                    state,
                    answer: answer.map(str::to_owned),
                    answered_by: answer.map(|_| "owner".to_owned()),
                    created_at: timestamp(created_at),
                    answered_at: answered_at.map(timestamp),
                })
                .expect("inbox row");
        }

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
            job_id: Some(JobId::parse(job).expect("job id")),
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

/// Asserts the shared shell response is `text/html`.
fn assert_html(page: &support::HttpResponse) {
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "page must be text/html, got {content_type:?}"
    );
}

/// Asserts a page never prints the served home path.
fn assert_no_home(page: &support::HttpResponse, fixture: &Fixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "page leaked the home path");
}

/// Asserts a read-only page never prints the served home or auth-token material.
///
/// The cost table deliberately prints ledger token counts under a "Tokens" column, so the
/// forbidden checks name auth-token forms rather than the bare word "token". The inbox page
/// is different: it must carry the unlock and `Authorization` code, but no token value.
fn assert_no_home_or_auth_token(page: &support::HttpResponse, fixture: &Fixture) {
    assert_no_home(page, fixture);
    let lowered = page.body.to_ascii_lowercase();
    for forbidden in ["secret", "api_key", "apikey", "bearer", "authorization", "api-token"] {
        assert!(!lowered.contains(forbidden), "page must not expose {forbidden:?}");
    }
}

#[test]
fn cost_page_serves_the_shell_and_the_fixture_report() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);

    let page = support::get(server.address, "/cost");
    assert_html(&page);
    for needle in [
        "id=\"cost\"",
        "id=\"by\"",
        "id=\"chart-by-label\"",
        "<option value=\"job\" selected>",
        "<option value=\"day\">",
        "<option value=\"role\">",
        "<option value=\"provider\">",
        "<option value=\"model\">",
        "\"/api/v1/cost?by=\"",
        "REFRESH_MS = 5000",
        "setInterval(refresh, REFRESH_MS)",
        "<th>Key</th>",
        "<th>Runs</th>",
        "<th>USD</th>",
        "<th>Tokens</th>",
        "<th>Source</th>",
        "<th>Unknown</th>",
        "Unknown costs are shown as",
        "never counted as zero",
        // The inline chart shows known USD and labels its unknown rows separately.
        "id=\"chart\"",
        "role=\"img\"",
        "aria-labelledby=\"chart-title\"",
        "aria-describedby=\"chart-summary\"",
        "Known USD by",
        "id=\"chart-unknown\"",
        "Unknown USD (not charted)",
        "No known USD to chart.",
        "renderChart(rows, by)",
        "chartByLabel.textContent = group",
        "const amount = knownUsd(item)",
        "if (amount === null)",
        "unknown.push(item)",
        "bar.style.width = (max > 0 ? Math.min(100, (amount / max) * 100) : 0) + \"%\"",
        "bySelect.addEventListener(\"change\"",
        "No cost entries.",
        "Last updated",
        "href=\"/\"",
        "href=\"/sessions\"",
        "href=\"/inbox\"",
        "<noscript>The cost page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "cost page is missing {needle:?}");
    }
    assert_no_home_or_auth_token(&page, &fixture);
    assert!(
        !page.body.contains("<script src="),
        "the chart must be inline and load no external library"
    );

    // The page's data source is the cost API and therefore the chart's only data source;
    // the same fixture must return the rows and totals the chart scales, including a null
    // USD row for an all-unknown group.
    let response = support::get(server.address, "/api/v1/cost?by=job");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let report: CostReport = serde_json::from_str(&response.body).expect("CostReport");
    assert_eq!(report.by, "job");
    assert_eq!(
        report.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
        vec!["job-a", "job-c", "job-b"]
    );
    assert_eq!(report.rows[0].runs, 2);
    assert_eq!(report.rows[0].usd, Some(0.5));
    assert_eq!(report.rows[0].source, "mixed");
    assert_eq!(report.rows[0].unknown_entries, 1);
    assert_eq!(report.rows[1].usd, Some(0.25));
    assert_eq!(report.rows[1].source, "price-table");
    assert_eq!(report.rows[2].usd, None, "unknown costs are null, never zero");
    assert_eq!(report.rows[2].source, "unknown");
    assert_eq!(report.total.runs, 4);
    assert_eq!(report.total.usd, Some(0.75));
    assert_eq!(report.total.unknown_entries, 2);
    assert_eq!(report.total.source, "mixed");

    // The selector's other grouping values fetch the same API with a different `by`.
    let response = support::get(server.address, "/api/v1/cost?by=day");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let report: CostReport = serde_json::from_str(&response.body).expect("CostReport");
    assert_eq!(report.by, "day");
    assert_eq!(
        report.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
        vec!["2026-01-02", "2026-01-03"]
    );
    assert_eq!(report.rows[0].usd, Some(0.5));
    assert_eq!(report.rows[1].usd, Some(0.25));
}

#[test]
fn inbox_page_serves_the_shell_and_the_fixture_items() {
    let fixture = Fixture::new();
    let server = support::Server::start_for(&fixture.paths);

    let page = support::get(server.address, "/inbox");
    assert_html(&page);
    for needle in [
        "id=\"inbox\"",
        "id=\"state\"",
        "<option value=\"open\" selected>",
        "<option value=\"all\">",
        "<option value=\"answered\">",
        "<option value=\"dismissed\">",
        "\"/api/v1/inbox?state=\"",
        "DEFAULT_STATE = \"open\"",
        "REFRESH_MS = 5000",
        "setInterval(refresh, REFRESH_MS)",
        "<th>ID</th>",
        "<th>Kind</th>",
        "<th>Job</th>",
        "<th>Title</th>",
        "<th>Age</th>",
        "<th>State</th>",
        "<th>Options</th>",
        "<th>Body preview</th>",
        "<th>Answer</th>",
        "No inbox items.",
        "Last updated",
        "href=\"/\"",
        "href=\"/sessions\"",
        "href=\"/cost\"",
        "<noscript>The inbox page requires JavaScript.</noscript>",
        // The token unlock panel stores the token in sessionStorage, never the URL.
        "id=\"unlock\"",
        "id=\"unlock-form\"",
        "id=\"token\"",
        "type=\"password\"",
        "Save token",
        "TOKEN_KEY = \"vowp-api-token\"",
        "window.sessionStorage.setItem(TOKEN_KEY, value)",
        "window.sessionStorage.removeItem(TOKEN_KEY)",
        "sessionStorage",
        // The answer form posts JSON to the answer write route with a bearer header.
        "input.type = \"text\"",
        "input.maxLength = MAX_ANSWER_BYTES",
        "form.appendChild(select)",
        "method: \"POST\"",
        "Authorization: \"Bearer \" + token",
        "\"/api/v1/inbox/\" + encodeURIComponent(itemId) + \"/answer\"",
        "body: JSON.stringify({ answer })",
        "if (response.status === 202)",
        "QUEUED_MESSAGE = \"Answer queued; the supervisor applies it on the next tick\"",
        "if (response.status === 401)",
        "if (response.status === 503)",
        "Writes are disabled: the daemon was started without VOWP_API_TOKEN.",
        "The token was rejected. Unlock with the current daemon write token.",
        "id=\"notice\"",
        "role=\"status\"",
        // Reads stay token-free and the token never enters the URL.
        "fetch(\"/api/v1/inbox?state=\" + encodeURIComponent(value))",
        "role=\"alert\"",
    ] {
        assert!(page.body.contains(needle), "inbox page is missing {needle:?}");
    }
    assert!(
        !page.body.contains("token="),
        "inbox page must not put the token in a query string"
    );
    assert!(
        !page.body.contains("searchParams.set(\"token\""),
        "inbox page must not put the token in the URL"
    );
    assert_no_home(&page, &fixture);
    assert!(
        !page.body.contains("test-write-token"),
        "the page must not embed a token value"
    );

    // The page's data source is the inbox API. The same fixture must return one item per
    // state with the fields the page renders.
    for (state, title, expected) in [
        ("open", "open question", InboxState::Open),
        ("answered", "answered question", InboxState::Answered),
        ("dismissed", "dismissed notice", InboxState::Dismissed),
    ] {
        let path = format!("/api/v1/inbox?state={state}");
        let response = support::get(server.address, &path);
        assert_eq!(response.status, 200, "body: {}", response.body);
        let page: InboxPage = serde_json::from_str(&response.body).expect("InboxPage");
        assert_eq!(page.total, 1, "{state} page total");
        assert_eq!(page.items.len(), 1, "{state} page items");
        assert_eq!(page.items[0].title, title);
        assert_eq!(page.items[0].state, expected);
        assert_eq!(page.items[0].options, vec!["yes".to_owned(), "no".to_owned()]);
        assert!(page.items[0].body.contains("short preview"));
        if state == "open" {
            assert_eq!(page.items[0].job_id.as_ref().map(JobId::as_str), Some("job-a"));
        }
        if state == "answered" {
            assert_eq!(page.items[0].answer.as_deref(), Some("yes"));
        }
    }
}
