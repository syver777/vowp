//! Hermetic board-page tests: a temporary home, a real loopback listener and real GETs.
//!
//! The page is static HTML, so these tests pin that the daemon serves it as `text/html`,
//! that it carries the board markup, the `/api/v1/board` fetch and the board SSE stream with
//! its reconnect and polling fallback, and that the same fixture backs the API with the
//! expected job counts and running job. No browser or Node runtime is involved, and nothing
//! touches the real home or a harness.

mod support;

use std::collections::BTreeMap;

use support::{BoardFixture, Server, get};
use vowp_protocol::api::Board;

#[test]
fn board_page_serves_the_static_page_and_the_fixture_board() {
    let fixture = BoardFixture::new();
    let server = Server::start(&fixture);

    let page = get(server.address, "/");
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "board page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"board\"",
        "fetch(\"/api/v1/board\")",
        "const STREAM_URL = \"/api/v1/board/stream\";",
        "new EventSource(STREAM_URL)",
        "addEventListener(\"board\"",
        "addEventListener(\"heartbeat\"",
        "REFRESH_MS = 5000",
        "startFallbackPolling",
        "stopFallbackPolling",
        "Reconnecting...",
        "typeof EventSource !== \"function\"",
        "Job counts",
        "Running jobs",
        "Slots",
        "Planner",
        "Keeper",
        "Limits",
        "Cost today",
        "Inbox open",
        "Last checkpoint",
        "Last updated",
        "Live updates",
        "href=\"/sessions\"",
        "<noscript>The board requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "page is missing {needle:?}");
    }

    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "page leaked the home path");
    let lowered = page.body.to_ascii_lowercase();
    for forbidden in ["token", "secret", "api_key", "apikey"] {
        assert!(
            !lowered.contains(forbidden),
            "page must not mention {forbidden:?} at all"
        );
    }

    // The page's data source is the board API; the same fixture must return the counts
    // and the running job the page renders.
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

    // `/` and `/board` are the page routes; the API fallback still answers unknown paths.
    let missing = get(server.address, "/not-the-board");
    assert_eq!(missing.status, 404, "body: {}", missing.body);
}
