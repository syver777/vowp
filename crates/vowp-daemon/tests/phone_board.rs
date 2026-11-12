//! Hermetic phone-board tests: a temporary home, a real loopback listener and real GETs.
//!
//! The phone board is static HTML that fetches `/api/v1/board`, so these tests pin the
//! narrow-viewport card markup, the labels an operator reads, the PWA manifest and
//! service-worker hooks, the `<noscript>` fallback and the absence of paths or secrets. The
//! same fixture proves the API fields the page renders: one running job, one limit with a
//! reset time and an unknown cost total. No browser, Node runtime or real home is involved.

mod support;

use std::collections::BTreeMap;

use support::{BoardFixture, Server, get};
use vowp_protocol::api::Board;

#[test]
fn phone_board_serves_the_card_page_and_the_fixture_board() {
    let fixture = BoardFixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, "/board");
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "phone board must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"board\"",
        "class=\"card\"",
        "class=\"job-card",
        "class=\"facts\"",
        "fetch(\"/api/v1/board\")",
        "REFRESH_MS = 5000",
        "setInterval(refresh, REFRESH_MS)",
        "formatCost(data.cost_today_usd)",
        "Job counts",
        "Running jobs",
        "Job",
        "Executor",
        "Age",
        "Last event age",
        "Slots",
        "Planner",
        "Keeper",
        "Limits",
        "Resets",
        "Cost today",
        "Inbox open",
        "Last updated",
        "<noscript>The board requires JavaScript.</noscript>",
        "@media (max-width: 360px)",
        "<link rel=\"manifest\" href=\"/manifest.webmanifest\">",
        "<meta name=\"theme-color\" content=\"#16855b\">",
        "if ('serviceWorker' in navigator)",
        "navigator.serviceWorker.register('/sw.js')",
    ] {
        assert!(page.body.contains(needle), "page is missing {needle:?}");
    }

    // Running jobs come first, and the narrow layout uses cards rather than a table
    // that would scroll sideways on a 360 px phone.
    let running = page.body.find("id=\"running\"").expect("running list");
    let counts = page.body.find("id=\"counts\"").expect("counts list");
    assert!(
        running < counts,
        "running jobs must come before the job counts on the phone board"
    );
    assert!(!page.body.contains("<table"), "phone board must use cards, not tables");

    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "page leaked the home path");
    let lowered = page.body.to_ascii_lowercase();
    for forbidden in ["token", "secret", "api_key", "apikey"] {
        assert!(
            !lowered.contains(forbidden),
            "phone board must not mention {forbidden:?}"
        );
    }

    // `/` still serves the board shell; `/board` is the phone route.
    let root = get(server.address, "/");
    assert_eq!(root.status, 200, "body: {}", root.body);
    assert!(root.body.contains("id=\"board\""), "root lost the board shell");

    // The page's data source is the board API; the same fixture must return the running
    // job, limit reset and cost fields the phone page renders.
    let response = get(server.address, "/api/v1/board");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(
        !response.body.contains(&home),
        "board API response leaked the home path"
    );
    let board: Board = serde_json::from_str(&response.body).expect("Board");
    assert_eq!(board.project, fixture.project);
    assert_eq!(
        board.jobs,
        BTreeMap::from([
            ("done".to_owned(), 1),
            ("pending".to_owned(), 1),
            ("running".to_owned(), 1),
        ])
    );
    assert_eq!(board.running.len(), 1);
    let run = &board.running[0];
    assert_eq!(run.job, fixture.running_job);
    assert_eq!(run.executor.as_deref(), Some(fixture.run_executor.as_str()));
    assert!(
        (110..=300).contains(&run.age_secs),
        "run age must come from the live run: {}",
        run.age_secs
    );
    let last_event_age = run.last_event_age_secs.expect("session last event age");
    assert!(
        (0..=90).contains(&last_event_age),
        "last event age must come from the session update: {last_event_age}"
    );
    assert_eq!(board.slots_busy, 2);
    assert_eq!(board.slots_total, 4);
    assert_eq!(board.planner.as_deref(), Some("claude-code:planner-model@medium"));
    assert_eq!(board.keeper.as_deref(), Some("opencode:provider/keeper-model#max"));
    assert_eq!(board.limits.len(), 1);
    assert_eq!(board.limits[0].provider, "claude");
    assert_eq!(board.limits[0].resets_at, Some(fixture.limit_resets_at));
    assert_eq!(board.cost_today_usd, None, "no cost row means an unknown total");
    assert_eq!(board.inbox_open, 1);
    assert_eq!(board.last_checkpoint, Some(fixture.checkpoint));
}
