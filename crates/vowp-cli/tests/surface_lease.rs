//! `vowp session lease` / `session takeover` / `surface rename` ownership output.
//! Spec: `docs/design/v0.5.1/session-ownership.md` §2.4 and §2.6. Hermetic: a temporary home.

use std::os::unix::fs::PermissionsExt;

mod common;

/// Writes a hermetic fake `opencode` binary that probes cleanly and emits one valid
/// handoff capsule so CI runners without a host `opencode` binary can run `session takeover`.
fn configure_fake_opencode(env: &common::TestEnv) {
    let dir = env.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("fake bin dir");
    let draft = serde_json::json!({
        "intent": "continue the session",
        "constraints": [],
        "decisions": [],
        "state": {"done": ["started"], "in_progress": [], "blocked": [], "next_steps": ["finish"]},
        "files": [],
        "gotchas": [],
        "open_questions": []
    });
    let events = [
        serde_json::json!({
            "type": "text",
            "timestamp": 1_794_168_421_621_i64,
            "sessionID": "ses_fake",
            "part": {"text": draft.to_string()}
        }),
        serde_json::json!({
            "type": "step_finish",
            "timestamp": 1_794_168_421_622_i64,
            "sessionID": "ses_fake",
            "part": {"cost": 0.0, "tokens": {"input": 1, "output": 1}, "reason": "stop"}
        }),
    ];
    let fixture = dir.join("capsule.jsonl");
    let text = events.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    std::fs::write(&fixture, format!("{text}\n")).expect("fixture");
    let script = dir.join("opencode");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.0.28'; exit 0; fi\ncat '{}'\n",
        fixture.display()
    );
    std::fs::write(&script, body).expect("fake script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let mut config = env.config.clone();
    config.harnesses.insert(
        "opencode".to_owned(),
        vowp_protocol::HarnessConfig {
            binary: Some(script.display().to_string()),
            ..vowp_protocol::HarnessConfig::default()
        },
    );
    std::fs::write(
        env.project_path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("write config");
}

/// Creates a session and returns its id.
fn create(env: &common::TestEnv) -> String {
    let (code, stdout, stderr) = env.run(&[
        "session",
        "new",
        "--title",
        "ownership",
        "--executor",
        "opencode:deepseek/deepseek-flash#max",
    ]);
    assert_eq!(code, 0, "{stderr}");
    stdout.trim().to_owned()
}

#[test]
fn session_lease_accepts_desktop_ui_and_prints_mode_state_epoch() {
    let env = common::TestEnv::new();
    let id = create(&env);

    let (code, stdout, stderr) = env.run(&[
        "session",
        "lease",
        &id,
        "--acquire",
        "desktop-ui",
        "--holder",
        "StackSpirit",
        "--label",
        "Desk UI",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let token = stdout.trim().to_owned();
    assert_eq!(token.len(), 52, "--acquire prints the one-time token");

    let (code, status, stderr) = env.run_json(&["session", "lease", &id]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(status["held"], true);
    assert_eq!(status["kind"], "desktop_ui");
    assert_eq!(status["holder"], "StackSpirit");
    assert_eq!(status["mode"], "interactive");
    assert_eq!(status["state"], "active");
    assert_eq!(status["activity"], "idle");
    assert_eq!(status["epoch"], 1);
    assert_eq!(status["label"], "Desk UI");
    assert!(status["token"].is_null(), "a plain status never carries the token");
    assert!(!status.to_string().contains(&token), "the token is not echoed");

    let (code, human, stderr) = env.run(&["session", "lease", &id]);
    assert_eq!(code, 0, "{stderr}");
    assert!(human.contains("(desktop-ui)"), "{human}");
    assert!(
        human.contains("mode interactive state active activity idle epoch 1"),
        "{human}"
    );
    assert!(!human.contains(&token), "{human}");
}

#[test]
fn session_takeover_increments_the_epoch_and_names_the_new_owner() {
    let env = common::TestEnv::new();
    configure_fake_opencode(&env);
    let id = create(&env);
    let (code, _, stderr) = env.run(&["session", "lease", &id, "--acquire", "chat", "--holder", "laptop"]);
    assert_eq!(code, 0, "{stderr}");

    let takeover = |holder: &str, label: Option<&str>| {
        let mut args = vec![
            "session",
            "takeover",
            &id,
            "--kind",
            "desktop-ui",
            "--holder",
            holder,
            "--node",
            "local",
        ];
        if let Some(label) = label {
            args.extend(["--label", label]);
        }
        env.run(&args)
    };
    let (code, _, stderr) = takeover("StackSpirit", Some("Desk UI"));
    assert_eq!(code, 0, "{stderr}");
    let (_, status, _) = env.run_json(&["session", "lease", &id]);
    assert_eq!(status["epoch"], 2);
    assert_eq!(status["label"], "Desk UI");
    assert_eq!(status["kind"], "desktop_ui");

    // The plumbing command never asks: it fences whoever holds the session, even mid-turn,
    // and each takeover is one more epoch. The caller (chat, a UI, MCP) does the asking.
    let session = vowp_protocol::SessionId::parse(&id).expect("session id");
    env.store()
        .set_session_state(&session, vowp_protocol::SessionState::Running, None)
        .expect("running");
    let (_, running, _) = env.run_json(&["session", "lease", &id]);
    assert_eq!(
        running["activity"], "running",
        "the owner view says the turn is running"
    );
    let (code, _, stderr) = takeover("phone", None);
    assert_eq!(code, 0, "{stderr}");
    let (_, status, _) = env.run_json(&["session", "lease", &id]);
    assert_eq!(status["holder"], "phone");
    assert_eq!(status["epoch"], 3);
}

#[test]
fn unknown_surface_kind_lists_desktop_ui() {
    let env = common::TestEnv::new();
    let id = create(&env);
    let (code, _, stderr) = env.run(&["session", "lease", &id, "--acquire", "toaster"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("desktop-ui"), "{stderr}");
}
