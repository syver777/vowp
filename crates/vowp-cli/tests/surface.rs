//! `vowp surface pair --label` and `vowp surface rename`: friendly saved surface names.
//! Spec: `docs/design/v0.5.1/session-ownership.md` §2.4. Hermetic: a temporary home.

mod common;

use common::TestEnv;
use vowp_protocol::{NodeName, NodeStatus, Session, SessionId, SessionRole, SessionState};

/// Creates a home with an active `mac-studio` node and one idle session.
fn seed() -> (TestEnv, SessionId) {
    let env = TestEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let node = NodeName::parse("mac-studio").expect("node slug");
    store.insert_pending_node(&node, &[], now).expect("pending node");
    store
        .set_node_status(&node, NodeStatus::Active, "", now)
        .expect("active node");
    let session = SessionId::new();
    store
        .upsert_session(&Session {
            id: session.clone(),
            project: Some(env.config.project.name.clone()),
            job_id: None,
            role: Some(SessionRole::User),
            title: "labels".to_owned(),
            parent_session_id: None,
            node,
            cwd: "/tmp/vowp".to_owned(),
            state: SessionState::Idle,
            executor: None,
            last_seq: 0,
            last_capsule_id: None,
            log_bytes: 0,
            created_at: now,
            updated_at: now,
        })
        .expect("session");
    (env, session)
}

fn pair(env: &TestEnv, session: &SessionId, extra: &[&str]) -> String {
    let mut args = vec![
        "surface",
        "pair",
        "--session",
        session.as_ref(),
        "--node",
        "mac-studio",
        "--kind",
        "opencode",
    ];
    args.extend_from_slice(extra);
    let (code, value, stderr) = env.run_json(&args);
    assert_eq!(code, 0, "surface pair failed: {stderr}");
    value["surface"].as_str().expect("surface id").to_owned()
}

fn label_of(env: &TestEnv, id: &str) -> String {
    let (code, value, stderr) = env.run_json(&["surface", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    value
        .as_array()
        .expect("surface list")
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("surface {id} not listed in {value}"))["label"]
        .as_str()
        .expect("label")
        .to_owned()
}

#[test]
fn surface_pair_label_and_rename_command_save_the_label() {
    let (env, session) = seed();
    let plain = pair(&env, &session, &[]);
    assert_eq!(
        label_of(&env, &plain),
        "OpenCode · mac-studio",
        "the default label names kind and node"
    );

    let named = pair(&env, &session, &["--app", "other", "--label", "Kitchen laptop"]);
    assert_eq!(label_of(&env, &named), "Kitchen laptop");

    let (code, stdout, stderr) = env.run(&["surface", "rename", &plain, "Desk monitor"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Desk monitor"), "{stdout}");
    assert_eq!(label_of(&env, &plain), "Desk monitor");
    let (_, human, _) = env.run(&["surface", "ls"]);
    assert!(human.contains("LABEL") && human.contains("Desk monitor"), "{human}");

    let (code, _, stderr) = env.run(&["surface", "rename", &plain, "--clear"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(label_of(&env, &plain), "OpenCode · mac-studio");

    let (code, _, stderr) = env.run(&["surface", "rename", &plain]);
    assert_ne!(code, 0, "a rename with no label and no --clear is refused");
    assert!(stderr.contains("label or --clear"), "{stderr}");
    let (code, _, _) = env.run(&["surface", "rename", &plain, "   "]);
    assert_ne!(code, 0, "a blank label is refused");
}
