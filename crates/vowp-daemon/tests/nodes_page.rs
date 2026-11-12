//! Hermetic nodes page and API tests: a temporary home, a real loopback listener and real GETs.
//!
//! The page is a static HTML shell, so these tests pin that the daemon serves it as
//! `text/html`, that it fetches `/api/v1/nodes` on load and every five seconds, that it
//! carries the node fields and the empty and failed-fetch states, and that the same fixture
//! backs the API with one local and one remote node. No browser, Node runtime, harness, tmux
//! server or real home is involved.

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use tempfile::TempDir;
use vowp_protocol::api::NodePage;
use vowp_protocol::{Clock, FakeClock, Node, NodeName, Project, ProjectName, Timestamp, VowpConfig};
use vowp_store::{NodePaths, Store};

/// A temporary home with one registered project and, optionally, one local and one remote node.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    local_seen: Timestamp,
    remote_seen: Timestamp,
}

impl Fixture {
    fn with_nodes() -> Self {
        Self::new(true)
    }

    fn empty() -> Self {
        Self::new(false)
    }

    fn new(with_nodes: bool) -> Self {
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

        let local_seen = timestamp("2026-01-10T11:00:00.000Z");
        let remote_seen = timestamp("2026-01-10T10:30:00.000Z");
        if with_nodes {
            store
                .upsert_node(&Node {
                    name: NodeName::parse("local-node").expect("local node name"),
                    ssh_target: None,
                    is_local: true,
                    capabilities: json!({
                        "vowp": "0.3.0",
                        "probe_ok": true,
                        "probe": "local node",
                    }),
                    last_seen_at: Some(local_seen),
                    created_at: timestamp("2026-01-01T00:00:00.000Z"),
                })
                .expect("local node");
            store
                .upsert_node(&Node {
                    name: NodeName::parse("remote-node").expect("remote node name"),
                    ssh_target: Some("vowp@example.invalid".to_owned()),
                    is_local: false,
                    capabilities: json!({
                        "vowp": "0.2.9",
                        "probe_ok": false,
                        "probe": "ssh: connect to host builder.example.test port 22: Connection refused",
                    }),
                    last_seen_at: Some(remote_seen),
                    created_at: timestamp("2026-01-01T00:00:00.000Z"),
                })
                .expect("remote node");
        }
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            local_seen,
            remote_seen,
        }
    }
}

/// Parses a fixed RFC 3339 test instant.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
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

#[test]
fn nodes_page_serves_the_shell_and_the_fixture_nodes() {
    let fixture = Fixture::with_nodes();
    let server = support::Server::start_for(&fixture.paths);

    let page = support::get(server.address, "/nodes");
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "nodes page must be text/html, got {content_type:?}"
    );
    for needle in [
        "id=\"nodes\"",
        "fetch(\"/api/v1/nodes\")",
        "const REFRESH_MS = 5000;",
        "setInterval(refresh, REFRESH_MS)",
        "Name",
        "SSH target",
        "Local / remote",
        "Last seen",
        "Version",
        "Capabilities / probe",
        "capabilities.vowp",
        "surface-only",
        "probe ok",
        "probe failed",
        "No nodes.",
        "Could not load the nodes",
        "Showing data from ",
        "Last updated",
        "href=\"/\"",
        "href=\"/sessions\"",
        "href=\"/cost\"",
        "href=\"/inbox\"",
        "<noscript>The nodes page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "nodes page is missing {needle:?}");
    }

    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "nodes page leaked the home path");
    let lowered = page.body.to_ascii_lowercase();
    for forbidden in ["token", "secret", "api_key", "apikey", "bearer", "authorization"] {
        assert!(
            !lowered.contains(forbidden),
            "nodes page must not mention {forbidden:?} at all"
        );
    }

    // The page's data source is the nodes API; the same fixture must return the exact stored
    // rows, sorted by name.
    let response = support::get(server.address, "/api/v1/nodes");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(
        !response.body.contains(&home),
        "nodes API response leaked the home path"
    );
    let value: Value = serde_json::from_str(&response.body).expect("nodes JSON");
    assert_schema("NodePage", &value);
    let page: NodePage = serde_json::from_value(value).expect("NodePage");
    assert_eq!(page.total, 2);
    assert_eq!(page.next, None);
    assert_eq!(
        page.items.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(),
        vec!["local-node", "remote-node"]
    );

    let local = &page.items[0];
    assert!(local.is_local);
    assert_eq!(local.ssh_target, None);
    assert_eq!(local.capabilities["vowp"], json!("0.3.0"));
    assert_eq!(local.last_seen_at, Some(fixture.local_seen));
    assert_schema("Node", &serde_json::to_value(local).expect("local node JSON"));

    let remote = &page.items[1];
    assert!(!remote.is_local);
    assert_eq!(remote.ssh_target.as_deref(), Some("vowp@example.invalid"));
    assert_eq!(remote.capabilities["vowp"], json!("0.2.9"));
    assert_eq!(remote.capabilities["probe_ok"], json!(false));
    assert_eq!(remote.last_seen_at, Some(fixture.remote_seen));
    assert_schema("Node", &serde_json::to_value(remote).expect("remote node JSON"));
}

#[test]
fn nodes_page_serves_the_empty_state_and_an_empty_page() {
    let fixture = Fixture::empty();
    let server = support::Server::start_for(&fixture.paths);

    let page = support::get(server.address, "/nodes");
    assert_eq!(page.status, 200, "body: {}", page.body);
    assert!(
        page.body.contains("No nodes."),
        "the page must carry the empty-list message"
    );
    assert!(
        !page.body.contains(&fixture.paths.root().display().to_string()),
        "nodes page leaked the home path"
    );

    let response = support::get(server.address, "/api/v1/nodes");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let value: Value = serde_json::from_str(&response.body).expect("nodes JSON");
    assert_schema("NodePage", &value);
    let page: NodePage = serde_json::from_value(value).expect("NodePage");
    assert_eq!(page.total, 0);
    assert_eq!(page.next, None);
    assert!(page.items.is_empty());
}
