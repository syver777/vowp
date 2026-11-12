//! Hermetic capsule-page tests: a temporary home, a real loopback listener and real GETs.
//!
//! The capsule page is static HTML that reads its own path and fetches
//! `/api/v1/sessions/{id}/capsules/{capsuleId}`. These tests pin the API path, the labelled
//! sections, the session back link, the `<noscript>` fallback and the absence of paths or
//! secrets. They also fetch the same fixture through the capsule API to prove the page's data
//! source, and pin the API 404 behind the page's visible not-found line. No browser, Node
//! runtime, harness or real home is involved.

mod support;

use std::sync::Arc;

use support::{Server, get};
use tempfile::TempDir;
use vowp_protocol::capsule::CAPSULE_VERSION;
use vowp_protocol::{
    Capsule, CapsuleDecision, CapsuleFile, CapsuleId, CapsuleState, CapsuleTelemetry, CapsuleTrigger, Clock, ErrorKind,
    Executor, ExecutorRef, FakeClock, FileRole, NodeName, Project, ProjectName, Session, SessionId, SessionRole,
    SessionState, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, Store};

/// A temporary home with one project, one session and one fully populated capsule.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    session_id: SessionId,
    capsule: Capsule,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        let clock = FakeClock::at(epoch());
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse("proj").expect("project name");
        let node = NodeName::parse("test-node").expect("node name");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = node.clone();
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

        let session_id = SessionId::new();
        let capsule = capsule(&session_id, clock.now());
        store
            .upsert_session(&Session {
                id: session_id.clone(),
                project: Some(project),
                job_id: None,
                role: Some(SessionRole::Worker),
                title: "capsule fixture session".to_owned(),
                parent_session_id: None,
                node,
                cwd: "/tmp/proj".to_owned(),
                state: SessionState::Running,
                executor: Some(Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor")),
                last_seq: capsule.at_seq,
                last_capsule_id: Some(capsule.capsule_id.clone()),
                log_bytes: 0,
                created_at: clock.now(),
                updated_at: clock.now(),
            })
            .expect("session row");
        store
            .insert_capsule(&capsule, "sessions/fixture/capsules/7.json")
            .expect("capsule row");
        drop(store);

        Self {
            _tmp: tmp,
            paths,
            session_id,
            capsule,
        }
    }
}

/// Builds a capsule with every rendered section populated.
fn capsule(session: &SessionId, at: Timestamp) -> Capsule {
    Capsule {
        version: CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: at,
        at_seq: 7,
        trigger: CapsuleTrigger::TurnEnd,
        from: ExecutorRef {
            executor: Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("capsule executor"),
            node: Some(NodeName::parse("test-node").expect("capsule node")),
        },
        intent: "hand the capsule fixture off".to_owned(),
        constraints: vec!["keep the capsule page read-only".to_owned()],
        decisions: vec![CapsuleDecision {
            what: "render the structured fields".to_owned(),
            why: "the capsule API returns them".to_owned(),
            at_seq: Some(7),
        }],
        state: CapsuleState {
            done: vec!["capsule page shell".to_owned()],
            in_progress: vec!["capsule page tests".to_owned()],
            blocked: vec!["nothing".to_owned()],
            next_steps: vec!["link the session panel".to_owned()],
        },
        files: vec![CapsuleFile {
            path: "crates/vowp-daemon/web/capsule.html".to_owned(),
            role: FileRole::Created,
            note: "capsule page".to_owned(),
        }],
        gotchas: vec!["no Markdown parser".to_owned()],
        open_questions: vec!["none".to_owned()],
        telemetry: CapsuleTelemetry {
            turns: 2,
            tokens_in: 120,
            tokens_out: 45,
            usd: Some(0.12),
        },
    }
}

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
}

#[test]
fn capsule_page_renders_the_fixture_capsule_from_the_api() {
    let fixture = Fixture::new();
    let server = Server::start_for(&fixture.paths);

    let page_path = format!(
        "/sessions/{}/capsules/{}",
        fixture.session_id, fixture.capsule.capsule_id
    );
    let page = get(server.address, &page_path);
    assert_eq!(page.status, 200, "body: {}", page.body);
    let content_type = page.content_type.as_deref().expect("content-type header");
    assert!(
        content_type.to_ascii_lowercase().starts_with("text/html"),
        "capsule page must be text/html, got {content_type:?}"
    );

    for needle in [
        "id=\"capsule\"",
        "id=\"capsule-body\"",
        // The shell reads its own path and fetches the capsule API.
        "fetch(url)",
        "\"/api/v1/sessions/\"",
        "\"/capsules/\"",
        "encodeURIComponent(ids.sessionId)",
        "encodeURIComponent(ids.capsuleId)",
        // The session back link is retargeted from the static sessions list.
        "id=\"back-link\"",
        "href=\"/sessions\"",
        "backLink.href = \"/sessions/\" + encodeURIComponent(ids.sessionId)",
        // Metadata labels.
        "<dt>Capsule ID</dt>",
        "<dt>Session ID</dt>",
        "<dt>Created</dt>",
        "<dt>At seq</dt>",
        "<dt>Trigger</dt>",
        "<dt>From</dt>",
        "<dt>Harness</dt>",
        "<dt>Model</dt>",
        "<dt>Profile</dt>",
        // Structured field sections.
        ">Intent</h3>",
        ">Constraints</h3>",
        ">Decisions</h3>",
        ">State</h3>",
        ">Done</h4>",
        ">In progress</h4>",
        ">Blocked</h4>",
        ">Next steps</h4>",
        ">Files</h3>",
        ">Gotchas</h3>",
        ">Open questions</h3>",
        "decision.at_seq",
        "file.role",
        // The 404 branch and the read-only shell.
        "Capsule not found",
        "capsuleBody.hidden = true;",
        "href=\"/\"",
        "href=\"/cost\"",
        "href=\"/inbox\"",
        "<noscript>The capsule page requires JavaScript.</noscript>",
    ] {
        assert!(page.body.contains(needle), "capsule page is missing {needle:?}");
    }

    let home = fixture.paths.root().display().to_string();
    assert!(!page.body.contains(&home), "capsule page leaked the home path");
    let lowered = page.body.to_ascii_lowercase();
    for forbidden in ["token", "secret", "api_key", "apikey"] {
        assert!(
            !lowered.contains(forbidden),
            "capsule page must not mention {forbidden:?}"
        );
    }

    // The page's data source is the capsule API; the same fixture must return every field
    // the page renders.
    let api_path = format!(
        "/api/v1/sessions/{}/capsules/{}",
        fixture.session_id, fixture.capsule.capsule_id
    );
    let response = get(server.address, &api_path);
    assert_eq!(response.status, 200, "body: {}", response.body);
    assert!(
        !response.body.contains(&home),
        "capsule API response leaked the home path"
    );
    let capsule: Capsule = serde_json::from_str(&response.body).expect("Capsule");
    assert_eq!(capsule, fixture.capsule);
    assert_eq!(capsule.session_id, fixture.session_id);
    assert_eq!(capsule.at_seq, 7);
    assert_eq!(capsule.created_at, epoch());
    assert_eq!(capsule.trigger, CapsuleTrigger::TurnEnd);
    assert_eq!(
        capsule.from.executor.to_string(),
        "claude-code:claude-sonnet-5-5@medium"
    );
    assert_eq!(capsule.from.node.as_ref().map(NodeName::as_str), Some("test-node"));
    assert_eq!(capsule.intent, "hand the capsule fixture off");
    assert_eq!(capsule.constraints, vec!["keep the capsule page read-only".to_owned()]);
    assert_eq!(capsule.decisions[0].at_seq, Some(7));
    assert_eq!(capsule.state.done, vec!["capsule page shell".to_owned()]);
    assert_eq!(capsule.state.in_progress, vec!["capsule page tests".to_owned()]);
    assert_eq!(capsule.state.blocked, vec!["nothing".to_owned()]);
    assert_eq!(capsule.state.next_steps, vec!["link the session panel".to_owned()]);
    assert_eq!(capsule.files[0].path, "crates/vowp-daemon/web/capsule.html");
    assert_eq!(capsule.files[0].role, FileRole::Created);
    assert_eq!(capsule.gotchas, vec!["no Markdown parser".to_owned()]);
    assert_eq!(capsule.open_questions, vec!["none".to_owned()]);
}

#[test]
fn capsule_page_reports_an_unknown_capsule_and_session() {
    let fixture = Fixture::new();
    let server = Server::start_for(&fixture.paths);

    let unknown_capsule = CapsuleId::new();
    let page_path = format!("/sessions/{}/capsules/{unknown_capsule}", fixture.session_id);
    let page = get(server.address, &page_path);
    assert_eq!(page.status, 200, "body: {}", page.body);
    assert!(
        page.body.contains("Capsule not found"),
        "the shell must carry the visible not-found line"
    );
    assert!(
        page.body.contains("id=\"back-link\"") && page.body.contains("href=\"/sessions\""),
        "the not-found state must keep the back link"
    );

    let api_path = format!("/api/v1/sessions/{}/capsules/{unknown_capsule}", fixture.session_id);
    let response = get(server.address, &api_path);
    assert_eq!(response.status, 404, "body: {}", response.body);
    let error: vowp_protocol::api::ApiError = serde_json::from_str(&response.body).expect("ApiError");
    assert_eq!(error.error.kind, ErrorKind::NotFound);

    let unknown_session = SessionId::new();
    let page_path = format!("/sessions/{unknown_session}/capsules/{}", fixture.capsule.capsule_id);
    let page = get(server.address, &page_path);
    assert_eq!(page.status, 200, "body: {}", page.body);
    assert!(page.body.contains("Capsule not found"));

    let api_path = format!(
        "/api/v1/sessions/{unknown_session}/capsules/{}",
        fixture.capsule.capsule_id
    );
    let response = get(server.address, &api_path);
    assert_eq!(response.status, 404, "body: {}", response.body);
    let error: vowp_protocol::api::ApiError = serde_json::from_str(&response.body).expect("ApiError");
    assert_eq!(error.error.kind, ErrorKind::NotFound);
}
