//! Hermetic PWA asset tests: a temporary home, a real loopback listener and real GETs.
//!
//! These pin the manifest document, the icon and service-worker content types and headers,
//! the bounded shell cache list, the explicit `/api/` and stream bypass, and the manifest
//! link plus service-worker registration on every page shell. No browser, Node runtime,
//! harness, tmux server or real home is involved.

mod support;

use serde_json::Value;
use support::{BoardFixture, Server, get};

/// Every static page shell route, including one dynamic session and capsule path.
const PAGES: [&str; 8] = [
    "/",
    "/board",
    "/sessions",
    "/sessions/ses-test",
    "/sessions/ses-test/capsules/cap-test",
    "/cost",
    "/inbox",
    "/nodes",
];

/// A fixed write token used to prove no page or asset embeds it.
const WRITE_TOKEN: &str = "test-pwa-write-token";

/// Asserts the page shells and assets never leak the served home path.
fn assert_no_home(body: &str, fixture: &BoardFixture) {
    let home = fixture.paths.root().display().to_string();
    assert!(!body.contains(&home), "response leaked the home path");
}

/// Asserts an asset never carries auth-token material.
fn assert_no_auth_material(body: &str) {
    let lowered = body.to_ascii_lowercase();
    for forbidden in ["secret", "api_key", "apikey", "bearer", "authorization"] {
        assert!(!lowered.contains(forbidden), "asset must not mention {forbidden:?}");
    }
    assert!(
        !body.contains(WRITE_TOKEN),
        "asset must not embed the daemon write token"
    );
}

#[test]
fn manifest_describes_the_installable_shell() {
    let fixture = BoardFixture::new();
    let server = Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let response = get(server.address, "/manifest.webmanifest");
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/manifest+json"),
        "manifest must use the manifest content type"
    );

    let manifest: Value = serde_json::from_str(&response.body).expect("manifest JSON");
    assert_eq!(manifest["name"], "VOWP daemon window");
    assert_eq!(manifest["short_name"], "VOWP");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["scope"], "/");
    assert_eq!(manifest["display"], "standalone");
    assert_eq!(manifest["theme_color"], "#16855b");
    assert_eq!(manifest["background_color"], "#f6f5f1");

    let icons = manifest["icons"].as_array().expect("icons array");
    assert_eq!(icons.len(), 1, "the manifest carries one SVG icon");
    assert_eq!(icons[0]["src"], "/icon.svg");
    assert_eq!(icons[0]["type"], "image/svg+xml");
    assert_eq!(icons[0]["sizes"], "any");
    assert_no_home(&response.body, &fixture);
    assert_no_auth_material(&response.body);
}

#[test]
fn pwa_assets_are_served_with_their_content_types_and_headers() {
    let fixture = BoardFixture::new();
    let server = Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let icon = get(server.address, "/icon.svg");
    assert_eq!(icon.status, 200, "body: {}", icon.body);
    assert_eq!(
        icon.content_type.as_deref(),
        Some("image/svg+xml"),
        "icon must be served as image/svg+xml"
    );
    assert!(icon.body.contains("<svg"), "icon must be an SVG document");
    assert!(icon.body.contains("xmlns=\"http://www.w3.org/2000/svg\""));
    assert_no_home(&icon.body, &fixture);
    assert_no_auth_material(&icon.body);

    let worker = get(server.address, "/sw.js");
    assert_eq!(worker.status, 200, "body: {}", worker.body);
    let content_type = worker.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/javascript"),
        "service worker must be JavaScript, got {content_type:?}"
    );
    assert_eq!(
        worker.header("service-worker-allowed"),
        Some("/"),
        "the service worker must be allowed to control the root scope"
    );
    assert_eq!(
        worker.header("cache-control"),
        Some("no-cache"),
        "the service worker script must be revalidated so updates are picked up"
    );
    assert_no_home(&worker.body, &fixture);
    assert_no_auth_material(&worker.body);

    // The manifest itself is fetched again here to pin all three PWA asset responses in
    // one place; the document assertions live in the first test.
    let manifest = get(server.address, "/manifest.webmanifest");
    assert_eq!(manifest.status, 200);
    assert_eq!(manifest.content_type.as_deref(), Some("application/manifest+json"));
    assert_no_home(&manifest.body, &fixture);
    assert_no_auth_material(&manifest.body);
}

#[test]
fn service_worker_caches_only_the_shell_and_bypasses_api_and_streams() {
    let fixture = BoardFixture::new();
    let server = Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    let response = get(server.address, "/sw.js");
    assert_eq!(response.status, 200, "body: {}", response.body);
    let body = &response.body;

    for needle in [
        "const CACHE_NAME = \"vowp-shell-v2\";",
        "const SHELL = [",
        "caches.open(CACHE_NAME)",
        "cache.addAll(SHELL)",
        ".keys()",
        "caches.delete(key)",
        "event.respondWith(networkFirst(request))",
        "event.respondWith(cacheFirst(request))",
        "request.mode === \"navigate\"",
        "url.pathname.startsWith(\"/api/\")",
        "url.pathname.endsWith(\"/stream\")",
        "accept.includes(\"text/event-stream\")",
        "if (isBypassed(request, url)) return;",
        "SHELL.includes(url.pathname)",
        "cache.put(request, response.clone())",
    ] {
        assert!(body.contains(needle), "service worker is missing {needle:?}");
    }

    // The install list is bounded to the static shells and the PWA assets; it contains no
    // API or stream path.
    let start = body.find("const SHELL = [").expect("SHELL list");
    let end = body[start..].find("];").expect("SHELL list end") + start;
    let shell = &body[start..end];
    for route in [
        "/",
        "/board",
        "/sessions",
        "/cost",
        "/inbox",
        "/nodes",
        "/manifest.webmanifest",
        "/icon.svg",
    ] {
        assert!(shell.contains(&format!("\"{route}\"")), "SHELL is missing {route:?}");
    }
    assert!(!shell.contains("/api/"), "SHELL must not contain an API route");
    assert!(!shell.contains("stream"), "SHELL must not contain a stream route");

    // The bypass happens before any cache write, so an API or SSE request can never be
    // stored as authoritative event data.
    let bypass = body
        .find("if (isBypassed(request, url)) return;")
        .expect("fetch bypass");
    let first_cache_write = body.find("cache.put").expect("cache write");
    assert!(
        bypass < first_cache_write,
        "the API/stream bypass must run before the fetch handler caches anything"
    );
    assert!(
        !body.contains("EventSource"),
        "the service worker must not open event streams"
    );
    assert!(
        !body.contains("/api/v1/"),
        "the service worker must not name an API route"
    );
    assert!(
        !body.contains("event:stream"),
        "the service worker must not cache event streams"
    );

    assert_no_home(body, &fixture);
    assert_no_auth_material(body);
}

#[test]
fn every_page_shell_links_the_manifest_and_registers_the_service_worker() {
    let fixture = BoardFixture::new();
    let server = Server::start_for_with_token(&fixture.paths, Some(WRITE_TOKEN));

    for path in PAGES {
        let page = get(server.address, path);
        assert_eq!(page.status, 200, "{path} body: {}", page.body);
        let content_type = page.content_type.as_deref().expect("content-type header");
        assert!(
            content_type.to_ascii_lowercase().starts_with("text/html"),
            "{path} must be text/html, got {content_type:?}"
        );
        for needle in [
            "<link rel=\"manifest\" href=\"/manifest.webmanifest\">",
            "<meta name=\"theme-color\" content=\"#16855b\">",
            "if ('serviceWorker' in navigator)",
            "navigator.serviceWorker.register('/sw.js')",
        ] {
            assert!(page.body.contains(needle), "{path} is missing {needle:?}");
        }
        assert_no_home(&page.body, &fixture);
        assert!(
            !page.body.contains(WRITE_TOKEN),
            "{path} must not embed the daemon write token"
        );
    }
}
