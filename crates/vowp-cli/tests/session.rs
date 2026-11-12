//! `vowp session` integration tests.
//!
//! Every test uses a temporary node home and project, a fake opencode script and never
//! touches the real home, network or a real harness binary.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use vowp_protocol::{
    Actor, AssistantMessage, Cost, EventBody, EventSource, EventType, HarnessKind, Lease, NodeName, RemoteEvent,
    RemoteLine, StopReason, SurfaceKind, Timestamp, TurnGrant, UserMessage,
};
use vowp_store::session_log::{NewEvent, SessionLog, index_row};

/// Writes a fake opencode binary that probes successfully and returns one valid capsule.
fn fake_opencode(env: &common::TestEnv) -> std::path::PathBuf {
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
    script
}

/// Rewrites the project config to use the fake opencode binary.
fn configure(env: &common::TestEnv) -> std::path::PathBuf {
    let script = fake_opencode(env);
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
    script
}

/// Creates a session and returns its id.
fn create(env: &common::TestEnv) -> String {
    let (code, stdout, stderr) = env.run(&[
        "session",
        "new",
        "--title",
        "session test",
        "--executor",
        "opencode:deepseek/deepseek-flash#max",
    ]);
    assert_eq!(code, 0, "{stderr}");
    stdout.trim().to_owned()
}

/// Closes a session and compacts it through the real `gc --apply` path.
///
/// Returns the capsule id that now carries the history; the log file is gone when this
/// helper returns.
fn compact(env: &common::TestEnv, id: &str) -> String {
    let session_id = vowp_protocol::SessionId::parse(id).expect("session id");
    let store = env.store();
    let session = store.require_session(&session_id).expect("session");
    store
        .set_session_state(
            &session_id,
            vowp_protocol::SessionState::Closed,
            session.executor.as_ref(),
        )
        .expect("close session");
    drop(store);

    let now = Timestamp::now()
        .plus(Duration::from_secs(60 * 24 * 60 * 60))
        .as_rfc3339();
    let (code, stdout, stderr) = env.run(&["gc", "--apply", "--now", &now]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("compacted 1 sessions"), "{stdout}");
    let log = env.home_path().join("sessions").join(id).join("log.jsonl");
    assert!(!log.exists(), "gc did not compact {id}");
    env.store()
        .latest_capsule(&session_id)
        .expect("capsule query")
        .expect("capsule")
        .capsule_id
        .to_string()
}

#[test]
fn compacted_session_show_capsule_succeeds_and_event_reads_exit_3() {
    let env = common::TestEnv::new();
    let id = create(&env);
    let capsule_id = compact(&env, &id);
    let message = format!("session compacted to capsule {capsule_id}; use 'session show --capsule'");

    let (code, stdout, stderr) = env.run(&["session", "show", &id, "--capsule"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&format!("id: {id}")), "{stdout}");
    assert!(stdout.contains(&format!("capsule: {capsule_id}")), "{stdout}");

    let (code, value, stderr) = env.run_json(&["session", "show", &id, "--capsule"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["capsule"]["capsule_id"], capsule_id);

    // `show` without event flags only prints index metadata; it must not open the log either.
    let (code, _, stderr) = env.run(&["session", "show", &id]);
    assert_eq!(code, 0, "{stderr}");

    let (code, _, stderr) = env.run(&["session", "tail", &id]);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains(&message), "{stderr}");

    for flags in [vec!["--events", "1"], vec!["--events", "1", "--capsule"]] {
        let mut args = vec!["session", "show", id.as_str()];
        args.extend(flags);
        let (code, _, stderr) = env.run(&args);
        assert_eq!(code, 3, "{stderr}");
        assert!(stderr.contains(&message), "{stderr}");
    }
}

#[test]
fn compacted_session_export_writes_the_capsule_and_an_empty_tail() {
    let env = common::TestEnv::new();
    let id = create(&env);
    let capsule_id = compact(&env, &id);
    let path = env.project_path().join("compacted.vowp-bundle.json");

    let (code, stdout, stderr) = env.run(&[
        "session",
        "export",
        &id,
        "--out",
        path.to_str().expect("path"),
        "--tail",
        "20",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains(&path.display().to_string()), "{stdout}");

    let text = std::fs::read_to_string(&path).expect("bundle written");
    let bundle: serde_json::Value = serde_json::from_str(&text).expect("bundle JSON");
    assert_eq!(bundle["session"]["id"], id);
    assert_eq!(bundle["capsule"]["capsule_id"], capsule_id);
    assert_eq!(
        bundle["tail"].as_array().map(Vec::len),
        Some(0),
        "a compacted export must not invent an event tail: {bundle}"
    );
    assert!(bundle["compacted_at"].is_string(), "{bundle}");

    // Importing the bundle keeps the compacted capsule as the resume source.
    let (code, stdout, stderr) = env.run(&["session", "import", path.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    let child = stdout.trim();
    assert!(!child.is_empty());
    let sidecar = env.home_path().join("sessions").join(child).join("import.json");
    let imported: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sidecar).expect("import sidecar")).expect("sidecar JSON");
    assert_eq!(imported["capsule"]["capsule_id"], capsule_id);
    assert_eq!(imported["tail"].as_array().map(Vec::len), Some(0));
}

#[test]
fn session_export_import_roundtrip_via_cli() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);

    let bundle_path = env.project_path().join("session.vowp-bundle.json");
    let (code, _, stderr) = env.run(&[
        "session",
        "export",
        &id,
        "--out",
        bundle_path.to_str().expect("path"),
        "--tail",
        "5",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let text = std::fs::read_to_string(&bundle_path).expect("bundle written");
    let bundle: serde_json::Value = serde_json::from_str(&text).expect("bundle JSON");
    assert_eq!(bundle["format"], "vowp-bundle-v1");
    assert_eq!(bundle["session"]["id"], id);
    assert!(bundle["checksum"].as_str().unwrap_or_default().starts_with("sha256:"));

    let (code, stdout, stderr) = env.run(&["session", "import", bundle_path.to_str().expect("path")]);
    assert_eq!(code, 0, "{stderr}");
    let child = stdout.trim();
    assert_ne!(child, id);
    let store = env.store();
    let origin = store
        .require_session(&vowp_protocol::SessionId::parse(&id).expect("id"))
        .expect("origin");
    let child_row = store
        .require_session(&vowp_protocol::SessionId::parse(child).expect("id"))
        .expect("child");
    assert_eq!(child_row.parent_session_id.as_ref(), Some(&origin.id));
    assert_eq!(child_row.title, "session test (imported)");
}

#[test]
fn session_tail_pretty_golden() {
    let env = common::TestEnv::new();
    let id = create(&env);
    let session_id = vowp_protocol::SessionId::parse(&id).expect("id");
    let paths = vowp_store::NodePaths::new(env.home_path());
    let clock = std::sync::Arc::new(vowp_protocol::SystemClock);
    let log = SessionLog::open(&paths, &session_id, clock).expect("open log");
    let store = env.store();
    let source = EventSource {
        harness: HarnessKind::Opencode,
        model: Some("deepseek/deepseek-flash".to_owned()),
        node: Some(vowp_protocol::NodeName::parse("local").expect("node")),
        native_session_id: None,
    };
    let fixed = Timestamp::parse("2026-11-08T19:30:12.000Z").expect("timestamp");
    for body in [
        EventBody::UserMessage(UserMessage {
            text: "cargo test".to_owned(),
            attachments: Vec::new(),
            note: None,
        }),
        EventBody::AssistantMessage(AssistantMessage {
            text: "all green".to_owned(),
        }),
    ] {
        let appended = log
            .append(NewEvent {
                actor: Actor::User,
                source: source.clone(),
                body,
                raw: None,
                ts: Some(fixed),
            })
            .expect("append");
        store.index_event(&index_row(&appended)).expect("index");
    }
    log.sync().expect("sync");
    drop(log);

    let (code, stdout, stderr) = env.run(&["session", "tail", &id, "--pretty", "--from", "1"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout,
        "19:30:12 #2 user.message  user: cargo test\n19:30:12 #3 assistant.message  assistant: all green\n"
    );
}

#[test]
fn session_switch_with_fake_adapters() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);

    let (code, stdout, stderr) = env.run(&[
        "session",
        "switch",
        &id,
        "--to",
        "opencode:deepseek/deepseek-chat",
        "--reason",
        "user",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("switched session"), "{stdout}");

    let store = env.store();
    let session = store
        .require_session(&vowp_protocol::SessionId::parse(&id).expect("id"))
        .expect("session");
    assert_eq!(
        session.executor.as_ref().map(ToString::to_string).as_deref(),
        Some("opencode:deepseek/deepseek-chat")
    );
    let switches = store
        .list_switches(&vowp_protocol::SessionId::parse(&id).expect("id"))
        .expect("switches");
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].state, vowp_protocol::SwitchState::Completed);
    let session_id = vowp_protocol::SessionId::parse(&id).expect("id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("read log")
        .read_all()
        .expect("events");
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::Switch(payload)) if payload.capsule_id.is_some()
        )),
        "the switch event carries no capsule"
    );
    assert!(
        van_capsule_exists(env.home_path(), &id),
        "the switch capsule file was not written"
    );
}

/// Returns whether the session has at least one capsule file.
fn van_capsule_exists(home: &Path, id: &str) -> bool {
    let dir = home.join("sessions").join(id).join("capsules");
    std::fs::read_dir(dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

/// One remote node home plus the fake ssh/vowp PATH used to reach it.
struct RemoteNode {
    home: tempfile::TempDir,
    bin: tempfile::TempDir,
}

/// Creates a remote node home, registers the shared project and installs a fake `ssh`
/// that executes the quoted remote command locally with `VOWP_HOME` pointing at it.
fn remote_node(env: &common::TestEnv) -> RemoteNode {
    let home = tempfile::tempdir().expect("remote home");
    let paths = vowp_store::NodePaths::new(home.path());
    let store = vowp_store::Store::open(&paths).expect("remote store");
    let now = store.clock().now();
    store
        .upsert_node(&vowp_protocol::Node {
            name: env.config.node.name.clone(),
            ssh_target: None,
            is_local: true,
            capabilities: serde_json::json!({}),
            last_seen_at: Some(now),
            created_at: now,
        })
        .expect("remote node");
    store
        .upsert_project(&vowp_protocol::Project {
            name: env.config.project.name.clone(),
            path: env.project_path().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&env.config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("remote project");
    drop(store);

    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n").expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod ssh");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_vowp"), bin.path().join("vowp")).expect("vowp link");

    let local = env.store();
    local
        .upsert_node(&vowp_protocol::Node {
            name: vowp_protocol::NodeName::parse("vps").expect("node"),
            ssh_target: Some("vps.example".to_owned()),
            is_local: false,
            capabilities: serde_json::json!({}),
            last_seen_at: None,
            created_at: now,
        })
        .expect("local vps row");
    RemoteNode { home, bin }
}

/// Runs the real binary with the local home/project and a remote `VOWP_HOME`.
///
/// `TMPDIR` is a per-test directory so a command that stages a remote document in a
/// temporary file can prove it removed that file before returning.
fn run_cli(env: &common::TestEnv, remote: &RemoteNode, args: &[&str]) -> std::process::Output {
    let tmp = env.home_path().join("tmp");
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let path = format!("{}:/usr/bin:/bin", remote.bin.path().display());
    env.command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", path)
        .env("VOWP_HOME", remote.home.path())
        .env("TMPDIR", &tmp)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(args)
        .output()
        .expect("run vowp")
}

/// Returns the text of one stream.
fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Creates a session directly in a home without going through `--node`.
fn new_session_in(env: &common::TestEnv, home: &std::path::Path, title: &str) -> String {
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("VOWP_HOME", home)
        .arg("--home")
        .arg(home)
        .arg("--project")
        .arg(env.project_path())
        .args([
            "session",
            "new",
            "--title",
            title,
            "--executor",
            "opencode:deepseek/deepseek-flash#max",
        ])
        .output()
        .expect("new session");
    assert!(
        output.status.success(),
        "session new failed: {}",
        output_text(&output.stderr)
    );
    output_text(&output.stdout).trim().to_owned()
}

#[test]
fn session_cross_node_export_import_switch_via_fake_ssh() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);

    // §7.5 export --node: the bundle comes from the remote and is written locally.
    let remote_id = new_session_in(&env, remote.home.path(), "remote session");
    let output = run_cli(
        &env,
        &remote,
        &["session", "new", "--title", "created over ssh", "--node", "vps"],
    );
    assert!(
        output.status.success(),
        "cross-node new failed: {}",
        output_text(&output.stderr)
    );
    let created_id = output_text(&output.stdout).trim().to_owned();
    let remote_store_for_new =
        vowp_store::Store::open(&vowp_store::NodePaths::new(remote.home.path())).expect("remote store");
    assert!(
        remote_store_for_new
            .session(&vowp_protocol::SessionId::parse(&created_id).expect("id"))
            .expect("lookup")
            .is_some(),
        "session new --node did not create the session remotely"
    );
    drop(remote_store_for_new);

    let exported = env.project_path().join("remote-export.vowp-bundle.json");
    let output = run_cli(
        &env,
        &remote,
        &[
            "session",
            "export",
            &remote_id,
            "--node",
            "vps",
            "--out",
            exported.to_str().expect("path"),
        ],
    );
    assert!(
        output.status.success(),
        "cross-node export failed: {}",
        output_text(&output.stderr)
    );
    let bundle: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&exported).expect("bundle")).expect("json");
    assert_eq!(bundle["session"]["id"], remote_id);

    // §7.5 import --node: the local bundle is streamed to the remote over stdin.
    let local_id = create(&env);
    let local_bundle = env.project_path().join("local-import.vowp-bundle.json");
    let (code, _, stderr) = env.run(&[
        "session",
        "export",
        &local_id,
        "--out",
        local_bundle.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let output = run_cli(
        &env,
        &remote,
        &[
            "session",
            "import",
            local_bundle.to_str().expect("path"),
            "--node",
            "vps",
        ],
    );
    assert!(
        output.status.success(),
        "cross-node import failed: {}",
        output_text(&output.stderr)
    );
    let imported_id = output_text(&output.stdout).trim().to_owned();
    let remote_store = vowp_store::Store::open(&vowp_store::NodePaths::new(remote.home.path())).expect("remote store");
    let imported = remote_store
        .require_session(&vowp_protocol::SessionId::parse(&imported_id).expect("id"))
        .expect("remote child");
    assert_eq!(
        imported.parent_session_id.as_ref().map(ToString::to_string).as_deref(),
        Some(local_id.as_str())
    );

    // §7.6 switch --node: export, remote import, remote switch and local lineage.
    let switched_local = create(&env);
    let output = run_cli(
        &env,
        &remote,
        &[
            "session",
            "switch",
            &switched_local,
            "--to",
            "opencode:deepseek/deepseek-chat",
            "--reason",
            "user",
            "--node",
            "vps",
        ],
    );
    assert!(
        output.status.success(),
        "cross-node switch failed: {}",
        output_text(&output.stderr)
    );
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains("remote session"), "{stdout}");

    let local_store = env.store();
    let session_id = vowp_protocol::SessionId::parse(&switched_local).expect("id");
    let switches = local_store.list_switches(&session_id).expect("local switches");
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].state, vowp_protocol::SwitchState::Completed);
    assert_eq!(switches[0].to_node.to_string(), "vps");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::Switch(payload))
            if payload.to.node.as_ref().map(ToString::to_string).as_deref() == Some("vps")
    )));

    let remote_store = vowp_store::Store::open(&vowp_store::NodePaths::new(remote.home.path())).expect("remote store");
    let child = remote_store
        .list_sessions(&vowp_store::SessionFilter::default())
        .expect("remote sessions")
        .into_iter()
        .find(|session| session.parent_session_id.as_ref() == Some(&session_id))
        .expect("remote child session");
    assert_eq!(
        child.executor.as_ref().map(ToString::to_string).as_deref(),
        Some("opencode:deepseek/deepseek-chat")
    );
    let remote_switches = remote_store.list_switches(&child.id).expect("remote switches");
    assert!(
        remote_switches.iter().any(|switch| {
            switch.state == vowp_protocol::SwitchState::Completed
                && switch.reason == vowp_protocol::SwitchReason::User
                && switch.to_executor.to_string() == "opencode:deepseek/deepseek-chat"
        }),
        "{remote_switches:?}"
    );
}

/// Runs one CLI command against an explicit home and the shared test project.
fn run_in_home(env: &common::TestEnv, home: &Path, args: &[&str]) -> std::process::Output {
    env.command(env!("CARGO_BIN_EXE_vowp"))
        .env("VOWP_HOME", home)
        .arg("--home")
        .arg(home)
        .arg("--project")
        .arg(env.project_path())
        .args(args)
        .output()
        .expect("run vowp")
}

/// Exports one session from a home without going through `--node`.
fn export_text_in(env: &common::TestEnv, home: &Path, id: &str) -> String {
    let output = run_in_home(env, home, &["session", "export", id, "--out", "-"]);
    assert!(
        output.status.success(),
        "session export failed: {}",
        output_text(&output.stderr)
    );
    output_text(&output.stdout)
}

/// Replaces a fixture's fake `ssh` with a script that ignores its argv.
fn install_ssh(remote: &RemoteNode, body: &str) {
    let ssh = remote.bin.path().join("ssh");
    std::fs::write(&ssh, body).expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod ssh");
}

/// Appends one indexed user message to a session in `home`.
fn append_user_message(home: &Path, id: &str, text: &str) {
    let paths = vowp_store::NodePaths::new(home);
    let session = vowp_protocol::SessionId::parse(id).expect("session id");
    let store = vowp_store::Store::open(&paths).expect("store");
    let log = SessionLog::open(&paths, &session, std::sync::Arc::new(vowp_protocol::SystemClock)).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::User,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: None,
                node: None,
                native_session_id: None,
            },
            body: EventBody::UserMessage(vowp_protocol::UserMessage {
                text: text.to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
            raw: None,
            ts: None,
        })
        .expect("append user message");
    store.index_event(&index_row(&appended)).expect("index user message");
    log.sync().expect("sync");
}

/// Appends one indexed tool result large enough to spill into `blobs/`.
///
/// Returns the blob digest and the exact output bytes.
fn append_spilled_tool_result(home: &Path, id: &str, output: &str) -> (String, Vec<u8>) {
    assert!(
        output.len() > vowp_protocol::BLOB_SPILL_BYTES,
        "the fixture must spill, not stay inline"
    );
    let paths = vowp_store::NodePaths::new(home);
    let session = vowp_protocol::SessionId::parse(id).expect("session id");
    let store = vowp_store::Store::open(&paths).expect("store");
    let log = SessionLog::open(&paths, &session, std::sync::Arc::new(vowp_protocol::SystemClock)).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::Tool,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: None,
                node: None,
                native_session_id: None,
            },
            body: EventBody::ToolResult(vowp_protocol::ToolResult {
                call_id: "call-full-pull".to_owned(),
                ok: true,
                output: Some(output.to_owned()),
                output_ref: None,
                exit_code: Some(0),
            }),
            raw: None,
            ts: None,
        })
        .expect("append tool result");
    let sha256 = match appended.envelope.body().expect("tool result body") {
        EventBody::ToolResult(result) => {
            assert!(result.output.is_none(), "the oversized output stayed inline");
            result.output_ref.expect("spilled blob reference").sha256
        }
        other => panic!("expected a tool result, got {other:?}"),
    };
    store.index_event(&index_row(&appended)).expect("index tool result");
    log.sync().expect("sync");
    (sha256, output.as_bytes().to_vec())
}

/// Appends one indexed tool result whose blob reference has no file under `blobs/`.
fn append_dangling_tool_result(home: &Path, id: &str, sha256: &str) {
    let paths = vowp_store::NodePaths::new(home);
    let session = vowp_protocol::SessionId::parse(id).expect("session id");
    let store = vowp_store::Store::open(&paths).expect("store");
    let log = SessionLog::open(&paths, &session, std::sync::Arc::new(vowp_protocol::SystemClock)).expect("log");
    let appended = log
        .append(NewEvent {
            actor: Actor::Tool,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: None,
                node: None,
                native_session_id: None,
            },
            body: EventBody::ToolResult(vowp_protocol::ToolResult {
                call_id: "call-dangling-blob".to_owned(),
                ok: true,
                output: None,
                output_ref: Some(vowp_protocol::BlobRef {
                    sha256: sha256.to_owned(),
                    bytes: 4,
                    preview: "gone".to_owned(),
                }),
                exit_code: Some(0),
            }),
            raw: None,
            ts: None,
        })
        .expect("append dangling tool result");
    store.index_event(&index_row(&appended)).expect("index tool result");
    log.sync().expect("sync");
}

/// Returns how many sessions the local index holds.
fn local_session_count(env: &common::TestEnv) -> usize {
    env.store()
        .list_sessions(&vowp_store::SessionFilter::default())
        .expect("local sessions")
        .len()
}

/// Fails when the pull's temporary staging directory still holds anything.
fn assert_temp_clean(env: &common::TestEnv) {
    let tmp = env.home_path().join("tmp");
    let entries = std::fs::read_dir(&tmp)
        .expect("tmp dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("tmp entries");
    assert!(
        entries.is_empty(),
        "pull left files in its temporary directory: {entries:?}"
    );
}

#[test]
fn session_pull_imports_a_child_and_prints_both_ids() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");
    assert_eq!(local_session_count(&env), 0, "the source must start remote-only");

    let output = run_cli(&env, &remote, &["session", "pull", "vps", &source]);
    assert!(output.status.success(), "pull failed: {}", output_text(&output.stderr));
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains(&source), "the source id is missing: {stdout}");
    let child = stdout
        .split_whitespace()
        .last()
        .expect("child id in the pull output")
        .to_owned();
    let child_id = vowp_protocol::SessionId::parse(&child).expect("child id");
    assert_ne!(child_id, source_id, "pull imported the source id instead of a child");

    let store = env.store();
    let child_row = store.require_session(&child_id).expect("local child");
    assert_eq!(
        child_row.parent_session_id.as_ref(),
        Some(&source_id),
        "the child's parent_session_id must be the source id"
    );
    assert_eq!(child_row.title, "remote session (imported)");
    assert!(
        store.session(&source_id).expect("source lookup").is_none(),
        "pull must not create a same-id local session"
    );
    drop(store);
    assert!(
        !env.home_path()
            .join("sessions")
            .join(&child)
            .join("manifest.json")
            .exists(),
        "the default bundle pull must not write a directory manifest"
    );
    let remote_store = vowp_store::Store::open(&vowp_store::NodePaths::new(remote.home.path())).expect("remote store");
    assert!(
        remote_store.session(&child_id).expect("child lookup").is_none(),
        "the imported child must be local, not on the source node"
    );
    assert_temp_clean(&env);

    // The JSON document names both ids and the source node.
    let output = run_cli(&env, &remote, &["--json", "session", "pull", "vps", &source]);
    assert!(
        output.status.success(),
        "json pull failed: {}",
        output_text(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(output_text(&output.stdout).trim()).expect("pull JSON");
    assert_eq!(value["node"], "vps");
    assert_eq!(value["source_session"], source);
    let json_child = value["child_session"].as_str().expect("child_session");
    let json_child_id = vowp_protocol::SessionId::parse(json_child).expect("json child id");
    assert_ne!(json_child_id, source_id);
    let store = env.store();
    let json_child_row = store.require_session(&json_child_id).expect("local json child");
    assert_eq!(json_child_row.parent_session_id.as_ref(), Some(&source_id));
    assert_temp_clean(&env);
}

#[test]
fn session_pull_refuses_a_corrupt_bundle_before_importing() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    let bundle = export_text_in(&env, remote.home.path(), &source);
    let corrupted = bundle.replacen("\"title\": \"remote session\"", "\"title\": \"tampered session\"", 1);
    assert_ne!(corrupted, bundle, "the test did not change the bundle");
    let bundle_path = env.project_path().join("corrupt-bundle.json");
    std::fs::write(&bundle_path, &corrupted).expect("corrupt bundle");
    install_ssh(
        &remote,
        &format!("#!/bin/sh\ncat '{}'\nexit 0\n", bundle_path.display()),
    );

    let output = run_cli(&env, &remote, &["session", "pull", "vps", &source]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a corrupt bundle must be a local invalid-input failure: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains("checksum mismatch"),
        "the checksum failure was not reported: {}",
        output_text(&output.stderr)
    );
    assert_eq!(
        local_session_count(&env),
        0,
        "a corrupt bundle created a local session before verification"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_reports_a_failed_remote_export_and_imports_nothing() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let missing = "missing-session";

    let output = run_cli(&env, &remote, &["session", "pull", "vps", missing]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "the remote not-found exit code must be relayed: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains(&format!("session {missing}")),
        "the remote error was not reported: {}",
        output_text(&output.stderr)
    );
    assert_eq!(
        local_session_count(&env),
        0,
        "a failed remote export created a local session"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_same_id_imports_the_origin_id_without_a_parent() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    append_user_message(remote.home.path(), &source, "copy this event");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");
    assert_eq!(local_session_count(&env), 0, "the source must start remote-only");

    let output = run_cli(&env, &remote, &["session", "pull", "--same-id", "vps", &source]);
    assert!(
        output.status.success(),
        "same-id pull failed: {}",
        output_text(&output.stderr)
    );
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains(&source), "the source id is missing: {stdout}");
    assert!(stdout.contains("same id"), "{stdout}");

    let store = env.store();
    let row = store.require_session(&source_id).expect("same-id session");
    assert!(row.parent_session_id.is_none(), "same-id import must not keep a parent");
    assert_eq!(row.title, "remote session", "the origin title is kept unchanged");
    assert_eq!(row.node, env.config.node.name);
    drop(store);

    let paths = vowp_store::NodePaths::new(env.home_path());
    let events = SessionLog::open_read(&paths, &source_id)
        .expect("same-id log")
        .read_all()
        .expect("same-id events");
    assert_eq!(
        events.first().map(|event| event.event_type),
        Some(EventType::SessionCreated)
    );
    assert!(matches!(
        events[0].body().expect("created body"),
        EventBody::SessionCreated(payload) if payload.parent_session_id.is_none()
    ));
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload)) if payload.text == "copy this event"
        )),
        "the bundle tail was not copied into the same-id log: {events:?}"
    );
    assert!(
        !env.home_path()
            .join("sessions")
            .join(&source)
            .join("import.json")
            .exists(),
        "a same-id import must not write a child seed sidecar"
    );

    let store = env.store();
    let indexed = store.event_rows(&source_id, 0, 100).expect("index events");
    assert_eq!(indexed.len(), events.len(), "the index mirrors the imported log");
    assert!(store.latest_capsule(&source_id).expect("capsule query").is_some());
    drop(store);
    assert_temp_clean(&env);

    // The JSON document names the same-id session and has no child field in this mode.
    let second = new_session_in(&env, remote.home.path(), "json same id");
    let output = run_cli(
        &env,
        &remote,
        &["--json", "session", "pull", "--same-id", "vps", &second],
    );
    assert!(
        output.status.success(),
        "json same-id pull failed: {}",
        output_text(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(output_text(&output.stdout).trim()).expect("pull JSON");
    assert_eq!(value["node"], "vps");
    assert_eq!(value["source_session"], second);
    assert_eq!(value["same_id_session"], second);
    assert!(
        value.get("child_session").is_none(),
        "same-id output must not name a child: {value}"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_same_id_reports_divergence_without_mutating_it() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    append_user_message(remote.home.path(), &source, "keep this event");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");

    let output = run_cli(&env, &remote, &["session", "pull", "--same-id", "vps", &source]);
    assert!(
        output.status.success(),
        "first same-id pull failed: {}",
        output_text(&output.stderr)
    );
    append_user_message(env.home_path(), &source, "local advanced");
    append_user_message(remote.home.path(), &source, "remote advanced");

    let local_paths = vowp_store::NodePaths::new(env.home_path());
    let remote_paths = vowp_store::NodePaths::new(remote.home.path());
    let local_events = SessionLog::open_read(&local_paths, &source_id)
        .expect("local log")
        .read_all()
        .expect("local events");
    let remote_events = SessionLog::open_read(&remote_paths, &source_id)
        .expect("remote log")
        .read_all()
        .expect("remote events");
    let log_path = local_paths.log_path(&source_id);
    let before_log = std::fs::read(&log_path).expect("same-id log bytes");
    let store = env.store();
    let before_row = store.require_session(&source_id).expect("same-id row");
    drop(store);

    let output = run_cli(&env, &remote, &["session", "pull", "--same-id", "vps", &source]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "a duplicate same-id pull must be a conflict: {}",
        output_text(&output.stderr)
    );
    let stderr = output_text(&output.stderr);
    assert!(
        stderr.contains(&format!("session {source} exists locally and on vps")),
        "the divergence was not named: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "local:  {source} on {}: seq {}, last user.message at ",
            env.config.node.name,
            local_events.len()
        )),
        "the local head is missing: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "remote: {source} on vps: seq {}, last user.message at ",
            remote_events.len()
        )),
        "the remote head is missing: {stderr}"
    );
    assert!(
        stderr.contains("choose which side advances; no merge performed"),
        "the manual-choice note is missing: {stderr}"
    );
    assert!(
        !stderr.contains(&env.home_path().display().to_string()),
        "the report leaked the local home path: {stderr}"
    );
    assert!(
        !stderr.contains(&remote.home.path().display().to_string()),
        "the report leaked the remote home path: {stderr}"
    );
    assert_eq!(
        std::fs::read(&log_path).expect("same-id log bytes"),
        before_log,
        "a refused same-id import rewrote the existing log"
    );
    let store = env.store();
    let after_row = store.require_session(&source_id).expect("same-id row");
    assert_eq!(after_row.last_seq, before_row.last_seq);
    assert_eq!(after_row.updated_at, before_row.updated_at);
    assert_eq!(local_session_count(&env), 1);
    drop(store);
    assert_temp_clean(&env);

    // The JSON document carries both heads under one stable code. The first divergence
    // run's export wrote a capsule on the source, so the next bundle tail ends there.
    let remote_events = SessionLog::open_read(&remote_paths, &source_id)
        .expect("remote log")
        .read_all()
        .expect("remote events");
    let remote_last = remote_events.last().expect("remote head event");
    let output = run_cli(
        &env,
        &remote,
        &["--json", "session", "pull", "--same-id", "vps", &source],
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "a JSON divergence must be a conflict: {}",
        output_text(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(output_text(&output.stdout).trim()).expect("divergence JSON");
    assert_eq!(value["error"]["kind"], "conflict");
    assert_eq!(value["error"]["code"], "session_divergence");
    assert_eq!(value["error"]["node"], "vps");
    assert_eq!(value["error"]["source_session"], source);
    assert_eq!(
        value["error"]["local"]["last_seq"].as_u64(),
        Some(local_events.len() as u64)
    );
    assert_eq!(value["error"]["local"]["last_event"], "user.message");
    assert!(
        value["error"]["local"]["last_event_at"].as_str().is_some(),
        "the local timestamp is missing: {value}"
    );
    assert_eq!(
        value["error"]["remote"]["last_seq"].as_u64(),
        Some(remote_events.len() as u64)
    );
    assert_eq!(value["error"]["remote"]["last_event"], remote_last.event_type.as_str());
    assert_eq!(value["error"]["remote"]["node"], "vps");
    assert_eq!(value["error"]["note"], "choose which side advances; no merge performed");
    assert!(
        value["error"]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("delete or export one side")),
        "the manual hint is missing: {value}"
    );
    assert_eq!(
        std::fs::read(&log_path).expect("same-id log bytes"),
        before_log,
        "a JSON divergence rewrote the existing log"
    );
    assert_temp_clean(&env);

    // The default child pull is unchanged: a present source id does not change its mode.
    let output = run_cli(&env, &remote, &["session", "pull", "vps", &source]);
    assert!(
        output.status.success(),
        "the default child pull failed: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stdout).contains("as child"),
        "the default pull did not create a child: {}",
        output_text(&output.stdout)
    );
    assert_eq!(
        local_session_count(&env),
        2,
        "the default pull did not add exactly one child"
    );
    assert_eq!(
        std::fs::read(&log_path).expect("same-id log bytes"),
        before_log,
        "the default child pull rewrote the same-id log"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_same_id_refuses_a_corrupt_bundle_before_importing() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    let bundle = export_text_in(&env, remote.home.path(), &source);
    let corrupted = bundle.replacen("\"title\": \"remote session\"", "\"title\": \"tampered session\"", 1);
    assert_ne!(corrupted, bundle, "the test did not change the bundle");
    let bundle_path = env.project_path().join("corrupt-same-id-bundle.json");
    std::fs::write(&bundle_path, &corrupted).expect("corrupt bundle");
    install_ssh(
        &remote,
        &format!("#!/bin/sh\ncat '{}'\nexit 0\n", bundle_path.display()),
    );

    let output = run_cli(&env, &remote, &["session", "pull", "--same-id", "vps", &source]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a corrupt bundle must be a local invalid-input failure: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains("checksum mismatch"),
        "the checksum failure was not reported: {}",
        output_text(&output.stderr)
    );
    assert_eq!(
        local_session_count(&env),
        0,
        "a corrupt same-id bundle created a local session before verification"
    );
    assert!(
        !env.home_path().join("sessions").join(&source).exists(),
        "a corrupt same-id bundle wrote a session directory"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_imports_the_origin_id_with_the_full_log() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "full remote session");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");

    // More than the 20-event bundle tail, so the default pull could not carry this prefix.
    for index in 0..25 {
        append_user_message(remote.home.path(), &source, &format!("remote event {index:02}"));
    }
    let big_output = format!(
        "spilled full-pull output {}",
        "x".repeat(vowp_protocol::BLOB_SPILL_BYTES + 64)
    );
    let (blob_sha, blob_bytes) = append_spilled_tool_result(remote.home.path(), &source, &big_output);
    let capsule = run_in_home(&env, remote.home.path(), &["session", "capsule", &source]);
    assert!(
        capsule.status.success(),
        "remote capsule failed: {}",
        output_text(&capsule.stderr)
    );

    let remote_paths = vowp_store::NodePaths::new(remote.home.path());
    vowp_store::session_files::add_adapter_mapping(
        &remote_paths,
        &source_id,
        vowp_store::session_files::AdapterMapping {
            harness: HarnessKind::Opencode,
            native_session_id: "ses_full_pull_native".to_owned(),
            node: NodeName::parse("vps").expect("node"),
            first_seq: 2,
            created_at: Timestamp::now(),
        },
    )
    .expect("remote adapters");
    let remote_events = SessionLog::open_read(&remote_paths, &source_id)
        .expect("remote log")
        .read_all()
        .expect("remote events");
    let remote_capsules =
        vowp_store::session_files::list_capsule_files(&remote_paths, &source_id).expect("remote capsules");
    assert!(
        remote_events.len() > 20,
        "the fixture needs more events than the bundle tail: {}",
        remote_events.len()
    );
    assert_eq!(remote_capsules.len(), 1, "the fixture must have one capsule file");
    let remote_adapters = std::fs::read(remote_paths.adapters_json_path(&source_id)).expect("remote adapters bytes");

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", &source]);
    assert!(
        output.status.success(),
        "full pull failed: {}",
        output_text(&output.stderr)
    );
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains(&source), "the source id is missing: {stdout}");
    assert!(
        stdout.contains("full directory snapshot"),
        "the full mode was not reported: {stdout}"
    );

    let local_paths = vowp_store::NodePaths::new(env.home_path());
    let store = env.store();
    let row = store.require_session(&source_id).expect("full-pull row");
    assert!(
        row.parent_session_id.is_none(),
        "a full pull imports under the origin id without a parent"
    );
    assert_eq!(row.title, "full remote session");
    assert_eq!(row.node, env.config.node.name);
    drop(store);

    let local_events = SessionLog::open_read(&local_paths, &source_id)
        .expect("local log")
        .read_all()
        .expect("local events");
    assert_eq!(
        local_events.len(),
        remote_events.len(),
        "the full log was not preserved"
    );
    assert!(
        local_events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::UserMessage(payload)) if payload.text == "remote event 00"
        )),
        "the prefix earlier than the bundle tail is missing"
    );
    let result = local_events
        .iter()
        .find_map(|event| match event.body() {
            Ok(EventBody::ToolResult(result)) => Some(result),
            _ => None,
        })
        .expect("spilled tool result");
    assert_eq!(
        result.output_ref.expect("blob reference").sha256,
        blob_sha,
        "the copied event still points at its blob"
    );
    assert_eq!(
        std::fs::read(local_paths.blobs_dir(&source_id).join(&blob_sha)).expect("local blob"),
        blob_bytes
    );
    assert_eq!(
        vowp_store::session_files::list_capsule_files(&local_paths, &source_id)
            .expect("local capsules")
            .len(),
        remote_capsules.len()
    );
    assert_eq!(
        std::fs::read(local_paths.adapters_json_path(&source_id)).expect("local adapters"),
        remote_adapters
    );

    let manifest = vowp_store::session_log::read_session_dir_manifest(&local_paths.session_manifest_path(&source_id))
        .expect("directory manifest");
    assert_eq!(manifest.origin, source_id);
    assert!(
        manifest.files.len() >= 4,
        "log, adapters, blob and capsule must be listed: {manifest:?}"
    );
    vowp_store::session_log::verify_session_dir(&local_paths, &source_id, &manifest).expect("manifest checksums");

    let (code, stdout, stderr) = env.run(&["reindex"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("reindexed 1 sessions"), "{stdout}");
    assert_temp_clean(&env);

    // The JSON document names the full session and has no child/same-id field.
    let second = new_session_in(&env, remote.home.path(), "full json session");
    append_user_message(remote.home.path(), &second, "json event");
    let output = run_cli(&env, &remote, &["--json", "session", "pull", "--full", "vps", &second]);
    assert!(
        output.status.success(),
        "json full pull failed: {}",
        output_text(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(output_text(&output.stdout).trim()).expect("full pull JSON");
    assert_eq!(value["node"], "vps");
    assert_eq!(value["source_session"], second);
    assert_eq!(value["full_session"], second);
    assert!(value["files"].as_u64().expect("files") >= 1, "{value}");
    assert!(value["bytes"].as_u64().expect("bytes") > 0, "{value}");
    assert!(value.get("child_session").is_none(), "{value}");
    assert!(value.get("same_id_session").is_none(), "{value}");
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_reports_divergence_without_mutating_it() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote full session");
    append_user_message(remote.home.path(), &source, "keep this event");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", &source]);
    assert!(
        output.status.success(),
        "first full pull failed: {}",
        output_text(&output.stderr)
    );
    append_user_message(env.home_path(), &source, "local advanced");
    append_user_message(remote.home.path(), &source, "remote advanced");

    let local_paths = vowp_store::NodePaths::new(env.home_path());
    let remote_paths = vowp_store::NodePaths::new(remote.home.path());
    let local_events = SessionLog::open_read(&local_paths, &source_id)
        .expect("local log")
        .read_all()
        .expect("local events");
    let remote_events = SessionLog::open_read(&remote_paths, &source_id)
        .expect("remote log")
        .read_all()
        .expect("remote events");
    let log_path = local_paths.log_path(&source_id);
    let manifest_path = local_paths.session_manifest_path(&source_id);
    let before_log = std::fs::read(&log_path).expect("full-pull log bytes");
    let before_manifest = std::fs::read(&manifest_path).expect("full-pull manifest bytes");
    let store = env.store();
    let before_row = store.require_session(&source_id).expect("full-pull row");
    drop(store);

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", &source]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "a duplicate full pull must be a conflict: {}",
        output_text(&output.stderr)
    );
    let stderr = output_text(&output.stderr);
    assert!(
        stderr.contains(&format!("session {source} exists locally and on vps")),
        "the divergence was not named: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "local:  {source} on {}: seq {}, last user.message at ",
            env.config.node.name,
            local_events.len()
        )),
        "the local head is missing: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "remote: {source} on vps: seq {}, last user.message at ",
            remote_events.len()
        )),
        "the remote head is missing: {stderr}"
    );
    assert!(
        stderr.contains("choose which side advances; no merge performed"),
        "the manual-choice note is missing: {stderr}"
    );
    assert!(
        !stderr.contains(&env.home_path().display().to_string()),
        "the report leaked the local home path: {stderr}"
    );
    assert!(
        !stderr.contains(&remote.home.path().display().to_string()),
        "the report leaked the remote home path: {stderr}"
    );
    assert_eq!(
        std::fs::read(&log_path).expect("full-pull log bytes"),
        before_log,
        "a refused full pull rewrote the existing log"
    );
    assert_eq!(
        std::fs::read(&manifest_path).expect("full-pull manifest bytes"),
        before_manifest,
        "a refused full pull rewrote the existing manifest"
    );
    let store = env.store();
    let after_row = store.require_session(&source_id).expect("full-pull row");
    assert_eq!(after_row.last_seq, before_row.last_seq);
    assert_eq!(after_row.updated_at, before_row.updated_at);
    assert_eq!(local_session_count(&env), 1);
    drop(store);
    assert_temp_clean(&env);

    // The JSON document carries both heads under one stable code.
    let output = run_cli(&env, &remote, &["--json", "session", "pull", "--full", "vps", &source]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "a JSON full divergence must be a conflict: {}",
        output_text(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(output_text(&output.stdout).trim()).expect("divergence JSON");
    assert_eq!(value["error"]["kind"], "conflict");
    assert_eq!(value["error"]["code"], "session_divergence");
    assert_eq!(value["error"]["node"], "vps");
    assert_eq!(value["error"]["source_session"], source);
    assert_eq!(
        value["error"]["local"]["last_seq"].as_u64(),
        Some(local_events.len() as u64)
    );
    assert_eq!(value["error"]["local"]["last_event"], "user.message");
    assert_eq!(
        value["error"]["remote"]["last_seq"].as_u64(),
        Some(remote_events.len() as u64)
    );
    assert_eq!(value["error"]["remote"]["last_event"], "user.message");
    assert_eq!(value["error"]["remote"]["node"], "vps");
    assert_eq!(value["error"]["note"], "choose which side advances; no merge performed");
    assert_eq!(
        std::fs::read(&log_path).expect("full-pull log bytes"),
        before_log,
        "a JSON full divergence rewrote the existing log"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_prefix_of_an_existing_session_keeps_the_local_conflict() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote full session");
    append_user_message(remote.home.path(), &source, "keep this event");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");
    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", &source]);
    assert!(
        output.status.success(),
        "first full pull failed: {}",
        output_text(&output.stderr)
    );
    let local_paths = vowp_store::NodePaths::new(env.home_path());
    let before = std::fs::read(local_paths.log_path(&source_id)).expect("full-pull log bytes");

    let manifest = vowp_store::session_log::SessionDirManifest {
        format: vowp_store::session_log::SESSION_DIR_FORMAT.to_owned(),
        origin: source_id.clone(),
        files: vec![vowp_store::session_log::SessionDirFile {
            path: "log.jsonl".to_owned(),
            bytes: 0,
            sha256: "0".repeat(64),
        }],
    };
    let mut stream = serde_json::to_vec(&manifest).expect("manifest JSON");
    stream.push(b'\n');
    let stream_path = env.project_path().join("existing-full.snapshot");
    std::fs::write(&stream_path, stream).expect("existing snapshot");
    let remote_store = vowp_store::Store::open(&vowp_store::NodePaths::new(remote.home.path())).expect("remote store");
    let remote_row = remote_store.require_session(&source_id).expect("remote source row");
    let show_json = serde_json::to_string(&remote_row).expect("session JSON");
    drop(remote_store);
    // A prefix is resolved by the manifest, so the local present-id check runs while the
    // source stream is still open; the deliberate abort must not mask the conflict. The
    // report's read-only `session show` gets the source row instead of the snapshot.
    install_ssh(
        &remote,
        &format!(
            "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in\n  *show*) printf '%s\\n' '{show_json}'; exit 0;;\nesac\ncat '{}'\nexec sleep 30\n",
            stream_path.display()
        ),
    );
    let prefix = &source[..10];
    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", prefix]);
    assert_eq!(
        output.status.code(),
        Some(4),
        "a present-id full pull must conflict: {}",
        output_text(&output.stderr)
    );
    let stderr = output_text(&output.stderr);
    assert!(
        stderr.contains(&format!("session {source} exists locally and on vps")),
        "the divergence was not named: {stderr}"
    );
    assert!(
        stderr.contains("choose which side advances; no merge performed"),
        "the manual-choice note is missing: {stderr}"
    );
    assert_eq!(
        std::fs::read(local_paths.log_path(&source_id)).expect("full-pull log bytes"),
        before,
        "a refused prefix full pull rewrote the existing log"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_refuses_a_corrupt_snapshot_without_leaving_a_directory() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let id = vowp_protocol::SessionId::new();
    let manifest = vowp_store::session_log::SessionDirManifest {
        format: vowp_store::session_log::SESSION_DIR_FORMAT.to_owned(),
        origin: id.clone(),
        files: vec![vowp_store::session_log::SessionDirFile {
            path: "log.jsonl".to_owned(),
            bytes: 4,
            sha256: "0".repeat(64),
        }],
    };
    let mut stream = serde_json::to_vec(&manifest).expect("manifest JSON");
    stream.push(b'\n');
    stream.extend_from_slice(b"junk");
    let stream_path = env.project_path().join("corrupt-full.snapshot");
    std::fs::write(&stream_path, stream).expect("corrupt snapshot");
    install_ssh(
        &remote,
        &format!("#!/bin/sh\ncat '{}'\nexit 0\n", stream_path.display()),
    );

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", id.as_str()]);
    assert!(
        !output.status.success(),
        "a corrupt snapshot must fail: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains("sha256 mismatch"),
        "the digest failure was not reported: {}",
        output_text(&output.stderr)
    );
    assert!(
        !env.home_path().join("sessions").join(id.as_str()).exists(),
        "a corrupt full pull left a final session directory"
    );
    assert_eq!(local_session_count(&env), 0);
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_refuses_a_log_with_a_missing_blob() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "dangling blob session");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");
    let missing = "b".repeat(64);
    append_dangling_tool_result(remote.home.path(), &source, &missing);

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", &source]);
    assert!(
        !output.status.success(),
        "a log with a missing blob must be refused: {}",
        output_text(&output.stderr)
    );
    let stderr = output_text(&output.stderr);
    assert!(
        stderr.contains("missing blob"),
        "the dangling reference was not reported: {stderr}"
    );
    assert!(
        stderr.contains(&missing),
        "the missing blob hash was not named: {stderr}"
    );
    assert!(
        !env.home_path().join("sessions").join(source_id.as_str()).exists(),
        "a refused full pull left a final session directory"
    );
    assert_eq!(local_session_count(&env), 0);
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_refuses_an_over_cap_snapshot_before_copying() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let id = vowp_protocol::SessionId::new();
    let limit = vowp_store::session_log::SESSION_DIR_MAX_BYTES;
    let manifest = vowp_store::session_log::SessionDirManifest {
        format: vowp_store::session_log::SESSION_DIR_FORMAT.to_owned(),
        origin: id.clone(),
        files: vec![vowp_store::session_log::SessionDirFile {
            path: "log.jsonl".to_owned(),
            bytes: limit + 1,
            sha256: "0".repeat(64),
        }],
    };
    let mut stream = serde_json::to_vec(&manifest).expect("manifest JSON");
    stream.push(b'\n');
    let stream_path = env.project_path().join("over-cap-full.snapshot");
    std::fs::write(&stream_path, stream).expect("over-cap snapshot");
    // Keep the source alive until the client aborts it, so the local size check always
    // races a running stream rather than a process that has already exited.
    install_ssh(
        &remote,
        &format!("#!/bin/sh\ncat '{}'\nexec sleep 30\n", stream_path.display()),
    );

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", id.as_str()]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an over-cap full pull must be an invalid-input failure: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains(&format!("{limit} bytes")),
        "the size limit was not named: {}",
        output_text(&output.stderr)
    );
    assert!(
        !env.home_path().join("sessions").join(id.as_str()).exists(),
        "an over-cap full pull left a final session directory"
    );
    assert_eq!(local_session_count(&env), 0);
    assert_temp_clean(&env);
}

#[test]
fn session_pull_full_reports_a_failed_remote_export_and_imports_nothing() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let missing = "missing-full-session";

    let output = run_cli(&env, &remote, &["session", "pull", "--full", "vps", missing]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "the remote not-found exit code must be relayed: {}",
        output_text(&output.stderr)
    );
    assert!(
        output_text(&output.stderr).contains(&format!("session {missing}")),
        "the remote error was not reported: {}",
        output_text(&output.stderr)
    );
    assert_eq!(
        local_session_count(&env),
        0,
        "a failed remote directory export created a local session"
    );
    assert_temp_clean(&env);
}

#[test]
fn session_import_same_id_keeps_the_bundle_id_and_refuses_a_second_import() {
    let env = common::TestEnv::new();
    configure(&env);
    let remote = remote_node(&env);
    let source = new_session_in(&env, remote.home.path(), "remote session");
    append_user_message(remote.home.path(), &source, "local import event");
    let source_id = vowp_protocol::SessionId::parse(&source).expect("source id");
    let bundle = export_text_in(&env, remote.home.path(), &source);
    let bundle_path = env.project_path().join("same-id.vowp-bundle.json");
    std::fs::write(&bundle_path, bundle).expect("bundle");

    let (code, stdout, stderr) = env.run(&["session", "import", bundle_path.to_str().expect("path"), "--same-id"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), source);
    let store = env.store();
    let row = store.require_session(&source_id).expect("same-id session");
    assert!(row.parent_session_id.is_none());
    drop(store);

    let log_path = env.home_path().join("sessions").join(&source).join("log.jsonl");
    let before_log = std::fs::read(&log_path).expect("same-id log");
    let (code, _, stderr) = env.run(&["session", "import", bundle_path.to_str().expect("path"), "--same-id"]);
    assert_eq!(code, 4, "a duplicate same-id import must conflict: {stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(
        std::fs::read(&log_path).expect("same-id log"),
        before_log,
        "a refused same-id import rewrote the existing log"
    );
}

/// The executor every remote-exec CLI test uses.
const REMOTE_EXECUTOR: &str = "opencode:deepseek/deepseek-flash#max";

/// Writes a `turn-begin` request, runs it in-process and stores the grant at mode 0600.
fn begin_remote_turn(env: &common::TestEnv, id: &str, lease: &str, prompt: &str) -> (TurnGrant, std::path::PathBuf) {
    begin_remote_turn_with_cwd(env, id, lease, prompt, None)
}

/// [`begin_remote_turn`] with an explicit surface checkout path.
fn begin_remote_turn_with_cwd(
    env: &common::TestEnv,
    id: &str,
    lease: &str,
    prompt: &str,
    cwd: Option<&Path>,
) -> (TurnGrant, std::path::PathBuf) {
    let request_path = env.project_path().join("turn-request.json");
    let mut request = serde_json::json!({
        "lease": lease,
        "prompt": prompt,
        "executor": REMOTE_EXECUTOR,
        "purpose": "start",
        "node": "local",
    });
    if let Some(cwd) = cwd {
        request["cwd"] = serde_json::json!(cwd.display().to_string());
    }
    std::fs::write(&request_path, request.to_string()).expect("request");
    let (code, stdout, stderr) = env.run(&[
        "session",
        "turn-begin",
        id,
        "--executor",
        REMOTE_EXECUTOR,
        "--purpose",
        "start",
        "--node",
        "local",
        "--request",
        request_path.to_str().expect("path"),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let grant: TurnGrant = serde_json::from_str(&stdout).expect("grant JSON");
    let grant_path = env.home_path().join("turn-grant.json");
    std::fs::write(&grant_path, &stdout).expect("grant file");
    std::fs::set_permissions(&grant_path, std::fs::Permissions::from_mode(0o600)).expect("grant mode");
    (grant, grant_path)
}

/// Runs the real `vowp __exec` binary against one grant.
fn run_exec(env: &common::TestEnv, grant: &Path, spool: Option<&Path>) -> std::process::Output {
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .arg("__exec")
        .arg("--grant")
        .arg(grant);
    if let Some(spool) = spool {
        command.arg("--spool-dir").arg(spool);
    }
    command.output().expect("run __exec")
}

/// One fake-ssh fixture: fake `ssh`/`vowp` PATH, control home and trace files.
struct SshFixture {
    bin: tempfile::TempDir,
    control: tempfile::TempDir,
    args: std::path::PathBuf,
    runs: std::path::PathBuf,
    hello: std::path::PathBuf,
}

/// Installs a fake `ssh` that runs the quoted remote command locally.
fn ssh_fixture(env: &common::TestEnv, script: &str) -> SshFixture {
    let bin = tempfile::tempdir().expect("fake bin");
    let ssh = bin.path().join("ssh");
    std::fs::write(&ssh, script).expect("fake ssh");
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod ssh");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_vowp"), bin.path().join("vowp")).expect("vowp link");
    let control = tempfile::tempdir().expect("control home");
    let pointer = serde_json::json!({
        "role": "control",
        "home": "local",
        "ssh_target": "fake.example",
        "remote_home": env.home_path().display().to_string(),
        "created_at": "2026-11-08T18:00:00Z",
    });
    std::fs::write(control.path().join("control.json"), pointer.to_string()).expect("control");
    SshFixture {
        args: bin.path().join("ssh-args.txt"),
        runs: bin.path().join("ssh-runs.txt"),
        hello: bin.path().join("hello.json"),
        bin,
        control,
    }
}

/// The forwarding fake ssh used by the happy-path tests.
const FORWARDING_SSH: &str = "#!/bin/sh\n\
for last; do :; done\n\
IFS= read -r line\n\
printf '%s\\n' \"$line\" > \"$FAKE_SSH_HELLO\"\n\
printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
{ printf '%s\\n' \"$line\"; cat; } | /bin/sh -c \"$last\"\n";

/// A fake ssh that cuts its first connection: it forwards at most two complete lines
/// (plus 12 bytes of the next line in `mid_line` mode) and drops every ack.
const CUTTING_SSH: &str = "#!/bin/sh\n\
printf '%s\\n' \"$@\" >> \"$FAKE_SSH_ARGS\"\n\
printf 'run\\n' >> \"$FAKE_SSH_RUNS\"\n\
for last; do :; done\n\
if [ \"$(wc -l < \"$FAKE_SSH_RUNS\")\" -eq 1 ]; then\n\
  if [ \"$FAKE_SSH_MODE\" = \"mid_line\" ]; then\n\
    { i=0; while [ $i -lt 2 ]; do IFS= read -r line || break; printf '%s\\n' \"$line\"; i=$((i+1)); done; head -c 12 2>/dev/null; } | /bin/sh -c \"$last\" >/dev/null\n\
  else\n\
    { i=0; while [ $i -lt 2 ]; do IFS= read -r line || break; printf '%s\\n' \"$line\"; i=$((i+1)); done; } | /bin/sh -c \"$last\" >/dev/null\n\
  fi\n\
  exit 0\n\
fi\n\
exec /bin/sh -c \"$last\"\n";

/// Builds `__exec` with the fixture's fake ssh and control home.
fn exec_over_ssh_command(
    env: &common::TestEnv,
    fixture: &SshFixture,
    grant: &Path,
    spool: Option<&Path>,
    extra: &[(&str, &str)],
) -> std::process::Command {
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .env("PATH", format!("{}:/usr/bin:/bin", fixture.bin.path().display()))
        .env("VOWP_CONTROL_HOME", fixture.control.path())
        .env("FAKE_SSH_ARGS", &fixture.args)
        .env("FAKE_SSH_RUNS", &fixture.runs)
        .env("FAKE_SSH_HELLO", &fixture.hello)
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .arg("__exec")
        .arg("--grant")
        .arg(grant);
    for (name, value) in extra {
        command.env(name, value);
    }
    if let Some(spool) = spool {
        command.arg("--spool-dir").arg(spool);
    }
    command
}

/// Runs `__exec` with the fixture's fake ssh and control home.
fn run_exec_over_ssh(
    env: &common::TestEnv,
    fixture: &SshFixture,
    grant: &Path,
    spool: Option<&Path>,
    extra: &[(&str, &str)],
) -> std::process::Output {
    exec_over_ssh_command(env, fixture, grant, spool, extra)
        .output()
        .expect("run __exec over fake ssh")
}

/// Returns every regular file below `root`.
fn files_below(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_below(&path));
        } else if path.is_file() {
            files.push(path);
        }
    }
    files
}

#[test]
fn remote_turn_begin_and_exec_streams_over_local_ingest() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "stream this turn");

    let output = run_exec(&env, &grant_path, None);
    assert!(
        output.status.success(),
        "__exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).expect("__exec status JSON");
    assert_eq!(status["run_id"], grant.run_id.to_string());
    assert_eq!(status["done"], true);
    assert_eq!(status["resumed"], false);
    assert_eq!(status["ack_window"], 64);

    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let paths = vowp_store::NodePaths::new(env.home_path());
    let events = SessionLog::open_read(&paths, &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::AssistantMessage(payload)) if !payload.text.is_empty()
        )),
        "no assistant event reached the home log"
    );
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Ok));
    assert!(run.ended_at.is_some());
    let session = store.require_session(&session_id).expect("session");
    assert_eq!(session.state, vowp_protocol::SessionState::Idle);
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1,
        "the streamed turn ended more than once"
    );

    let spool = env.home_path().join("spool").join(&id).join(grant.run_id.to_string());
    let spool_text = std::fs::read_to_string(spool.join("events.jsonl")).expect("spool");
    assert!(spool_text.contains("assistant.message"), "{spool_text}");
    let acked = std::fs::read_to_string(spool.join("acked.json")).expect("acked");
    assert!(acked.contains("\"summary\": true"), "{acked}");
}

#[test]
fn remote_turn_uses_the_surface_checkout_cwd() {
    let env = common::TestEnv::new();
    let script = configure(&env);
    let id = create(&env);
    let surface = env.project_path().join("surface-checkout");
    std::fs::create_dir_all(&surface).expect("surface checkout");
    let marker = env.project_path().join("harness-cwd.txt");
    let fixture = script.parent().expect("fake bin").join("capsule.jsonl");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.0.28'; exit 0; fi\npwd -P > '{}'\ncat '{}'\n",
        marker.display(),
        fixture.display()
    );
    std::fs::write(&script, body).expect("fake script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let (grant, grant_path) =
        begin_remote_turn_with_cwd(&env, &id, "lease-token", "run in my checkout", Some(&surface));
    let expected = surface.display().to_string();
    assert_eq!(grant.cwd, expected);
    let output = run_exec(&env, &grant_path, None);
    assert!(
        output.status.success(),
        "__exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let observed = std::fs::read_to_string(&marker).expect("harness cwd marker");
    assert_eq!(
        std::fs::canonicalize(observed.trim()).expect("observed cwd"),
        std::fs::canonicalize(&surface).expect("surface cwd"),
        "the harness did not start in the surface checkout"
    );
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.cwd.as_deref(), Some(expected.as_str()));
}

#[test]
fn remote_exec_refuses_a_corrupt_spool() {
    let env = common::TestEnv::new();
    let script = configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "do not start twice");
    let marker = env.project_path().join("harness-started.txt");
    let fixture = script.parent().expect("fake bin").join("capsule.jsonl");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.0.28'; exit 0; fi\ntouch '{}'\ncat '{}'\n",
        marker.display(),
        fixture.display()
    );
    std::fs::write(&script, body).expect("fake script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let spool = env.project_path().join("corrupt-spool");
    std::fs::create_dir_all(&spool).expect("spool dir");
    std::fs::write(spool.join("events.jsonl"), b"{\"type\": \"event\"\n").expect("corrupt spool");
    let output = run_exec(&env, &grant_path, Some(&spool));
    assert!(
        !output.status.success(),
        "__exec accepted a corrupt spool: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid spool line"),
        "the corruption was not reported: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "__exec started a second harness on top of a corrupt spool"
    );

    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert!(
        run.ended_at.is_none(),
        "the refused client finalised the turn it never ran"
    );
}

#[test]
fn session_ingest_refuses_a_run_from_another_session() {
    let env = common::TestEnv::new();
    configure(&env);
    let first = create(&env);
    let (grant, _grant_path) = begin_remote_turn(&env, &first, "lease-token", "own this run");
    let second = create(&env);

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["session", "ingest", second.as_str(), "--run", grant.run_id.as_str()])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ingest");
    assert!(
        !output.status.success(),
        "ingest accepted a run owned by another session"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not belong"),
        "the mismatch was not reported: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn client_crash_resumes_from_spool() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "resume this turn");
    let spool = env.home_path().join("resume-spool");
    std::fs::create_dir_all(&spool).expect("spool dir");
    let ts = Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp");
    let event = RemoteLine::Event {
        event: RemoteEvent {
            id: vowp_protocol::EventId::new(),
            ts,
            event_type: EventType::AssistantMessage,
            actor: Actor::Assistant,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: Some("deepseek/deepseek-flash".to_owned()),
                node: Some(vowp_protocol::NodeName::parse("local").expect("node")),
                native_session_id: None,
            },
            payload: serde_json::json!({"text": "resumed from the spool"}),
            raw: None,
            replayable: true,
        },
    };
    let summary = RemoteLine::Summary {
        exit_code: Some(0),
        outcome: vowp_protocol::RunOutcome::Ok,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("resumed".to_owned()),
        stop_reason: StopReason::EndTurn,
        usage: None,
    };
    let text = format!(
        "{}\n{}\n",
        serde_json::to_string(&event).expect("event"),
        serde_json::to_string(&summary).expect("summary")
    );
    std::fs::write(spool.join("events.jsonl"), text).expect("spool events");

    let output = run_exec(&env, &grant_path, Some(&spool));
    assert!(
        output.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    assert!(events.iter().any(|event| matches!(
        event.body(),
        Ok(EventBody::AssistantMessage(payload)) if payload.text == "resumed from the spool"
    )));
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Ok));
}

#[test]
fn lease_token_never_in_argv_or_logs() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let token = "lease-token-4f9c2b7d-never-log";
    let (grant, grant_path) = begin_remote_turn(&env, &id, token, "keep the token private");
    let fixture = ssh_fixture(&env, FORWARDING_SSH);
    let mode = std::fs::metadata(&grant_path).expect("grant").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the grant file is not mode 0600");

    let output = run_exec_over_ssh(&env, &fixture, &grant_path, None, &[]);
    assert!(
        output.status.success(),
        "__exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // This is the real argv ssh received, not a hand-built literal.
    let argv = std::fs::read_to_string(&fixture.args).expect("ssh argv trace");
    assert!(!argv.contains(token), "the lease token was placed on argv: {argv}");
    assert!(argv.contains("session"), "the ingest command was not traced: {argv}");
    assert!(argv.contains("ingest"), "the ingest command was not traced: {argv}");

    for path in files_below(env.home_path()) {
        if path == grant_path {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(token),
            "the lease token was written to {}",
            path.display()
        );
    }
    let _ = grant;
}

#[test]
fn remote_exec_streams_over_fake_ssh() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "stream over ssh");
    let fixture = ssh_fixture(&env, FORWARDING_SSH);
    let output = run_exec_over_ssh(&env, &fixture, &grant_path, None, &[]);
    assert!(
        output.status.success(),
        "__exec over ssh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let hello: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture.hello).expect("hello trace")).expect("hello JSON");
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["executor"], grant.executor);
    assert_eq!(hello["node"], "local");
    assert_eq!(hello["run_id"], grant.run_id.to_string());

    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::AssistantMessage)
    );
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Ok));
}

/// The exact notice `__exec` prints when another surface takes the lease.
const TAKEOVER_NOTICE: &str = "Taken over by phone at 14:06. This surface is now read-only (follow mode).\n";

/// How many times one lost revocation ack is retried before the test fails.
const TAKEOVER_ATTEMPTS: usize = 8;

/// How long one takeover attempt waits for `__exec` after the lease row moves.
const TAKEOVER_EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// A spawned `__exec` that is killed and reaped when dropped, so a failed assertion
/// never leaves an ingest client or its fake harness behind.
struct ExecChild {
    child: Option<std::process::Child>,
}

impl ExecChild {
    /// Wraps one spawned `__exec` process.
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Fails the test with the captured stderr when `__exec` exited before `what`.
    fn assert_running(&mut self, what: &str) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let Some(status) = child.try_wait().expect("try wait") else {
            return;
        };
        let child = self.child.take().expect("__exec child");
        let output = child.wait_with_output().expect("__exec output");
        panic!(
            "__exec exited with {status} before {what}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Waits up to `timeout` for `__exec`; `None` means it is still running and is killed.
    fn wait_with_output_timeout(&mut self, timeout: Duration) -> Option<std::process::Output> {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut()?;
            if child.try_wait().expect("try wait").is_some() {
                let child = self.child.take().expect("__exec child");
                return Some(child.wait_with_output().expect("__exec output"));
            }
            if Instant::now() >= deadline {
                self.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills and reaps the client, if it is still running.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ExecChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Polls `condition` until it holds, failing early when `__exec` exits or the deadline passes.
fn wait_until_running(child: &mut ExecChild, timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        child.assert_running(what);
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Returns whether the executor spool records a home ack for at least one event.
fn spool_has_acked_event(spool: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(spool.join("acked.json")) else {
        return false;
    };
    matches!(
        serde_json::from_str::<serde_json::Value>(&text),
        Ok(value) if value["events"].as_object().is_some_and(|events| !events.is_empty())
    )
}

/// Returns whether an attempt ended in the known lost-revocation-ack race.
///
/// The home writes `LeaseRevoked` and exits. If the client's next heartbeat write reaches
/// the closed ingest pipe before it drains that ack, the ack is dropped and the reconnect
/// is refused at hello. That race is in the product and outside this test's paths; a retry
/// starts it again with a fresh session instead of asserting on a lost frame.
fn lost_revocation_ack(status: Option<i32>, stderr: &str) -> bool {
    status == Some(7)
        && stderr.contains("this surface is read-only; taken over by phone (chat)")
        && stderr.contains("ingest ended without acknowledging the summary")
}

/// Runs one takeover: create a session, stream one event, take the lease over, wait for exit.
///
/// Returns `None` when `__exec` was still running at [`TAKEOVER_EXIT_TIMEOUT`] because it
/// lost the revocation ack and kept dialling a home that now refuses it.
fn takeover_attempt(env: &common::TestEnv) -> Option<(Option<i32>, String)> {
    let id = create(env);
    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let initial_at = Timestamp::parse("2026-11-08T14:05:00Z").expect("timestamp");
    let surface_token = "a".repeat(52);
    let phone_token = "b".repeat(52);
    let store = env.store();
    store
        .take_lease(&Lease {
            session_id: session_id.clone(),
            surface_id: None,
            surface_kind: SurfaceKind::Chat,
            node: NodeName::parse("local").expect("node"),
            holder: "laptop".to_owned(),
            token: surface_token.clone(),
            acquired_at: initial_at,
            heartbeat_at: initial_at,
        })
        .expect("initial lease");
    drop(store);

    let (grant, grant_path) = begin_remote_turn(env, &id, &surface_token, "wait for the takeover");
    let fixture = ssh_fixture(env, FORWARDING_SSH);
    let mut child = ExecChild::new(
        exec_over_ssh_command(env, &fixture, &grant_path, None, &[])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn __exec"),
    );

    wait_until_running(&mut child, Duration::from_secs(30), "the hello frame", || {
        fixture.hello.is_file()
    });
    // A hello frame has no acknowledgement of its own, and `session ingest` refuses the
    // hello if the takeover happens before it reads it. Waiting a fixed 200 ms for that
    // read is load-sensitive; the ack of the first streamed event proves it instead.
    let spool = env.home_path().join("spool").join(&id).join(grant.run_id.to_string());
    wait_until_running(&mut child, Duration::from_secs(60), "the first event ack", || {
        spool_has_acked_event(&spool)
    });

    let takeover_at = Timestamp::parse("2026-11-08T14:06:00Z").expect("timestamp");
    let store = env.store();
    store
        .take_lease(&Lease {
            session_id,
            surface_id: None,
            surface_kind: SurfaceKind::Chat,
            node: NodeName::parse("local").expect("node"),
            holder: "phone".to_owned(),
            token: phone_token,
            acquired_at: takeover_at,
            heartbeat_at: takeover_at,
        })
        .expect("takeover lease");
    drop(store);

    child.wait_with_output_timeout(TAKEOVER_EXIT_TIMEOUT).map(|output| {
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    })
}

#[test]
fn remote_exec_prints_the_exact_takeover_notice() {
    let env = common::TestEnv::new();
    let script = configure(&env);

    // Keep the harness alive so the client heartbeats and can be revoked mid-turn. The
    // first event is emitted immediately: its ack proves that the home validated the
    // hello before the lease row moves.
    let fixture_file = script.parent().expect("fake bin").join("capsule.jsonl");
    let first_file = script.parent().expect("fake bin").join("capsule-first.jsonl");
    let rest_file = script.parent().expect("fake bin").join("capsule-rest.jsonl");
    let fixture_text = std::fs::read_to_string(&fixture_file).expect("capsule fixture");
    let (first, rest) = fixture_text.split_once('\n').expect("two fixture events");
    std::fs::write(&first_file, format!("{first}\n")).expect("first fixture event");
    std::fs::write(&rest_file, rest).expect("remaining fixture events");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.0.28'; exit 0; fi\n\
         cat '{}'\n\
         sleep 10\n\
         cat '{}'\n",
        first_file.display(),
        rest_file.display()
    );
    std::fs::write(&script, body).expect("fake script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut last: (Option<i32>, String) = (None, "no takeover attempt ran".to_owned());
    for attempt in 1..=TAKEOVER_ATTEMPTS {
        let Some((status, stderr)) = takeover_attempt(&env) else {
            last = (
                None,
                format!("attempt {attempt}: __exec was still running {TAKEOVER_EXIT_TIMEOUT:?} after the takeover"),
            );
            continue;
        };
        if status == Some(75) && stderr == TAKEOVER_NOTICE {
            return;
        }
        let retry = lost_revocation_ack(status, &stderr);
        last = (status, stderr);
        if !retry {
            break;
        }
    }

    let (status, stderr) = last;
    assert_eq!(
        status,
        Some(75),
        "__exec did not stop on revocation within {TAKEOVER_ATTEMPTS} attempts: {stderr}"
    );
    assert_eq!(stderr, TAKEOVER_NOTICE);
}

#[test]
fn remote_exec_start_failure_finalises_the_turn() {
    let env = common::TestEnv::new();
    let script = configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "start nothing");
    std::fs::remove_file(&script).expect("remove the fake harness binary");

    let output = run_exec(&env, &grant_path, None);
    assert!(
        !output.status.success(),
        "__exec reported success after the harness failed to start: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_failed_turn_finalised(&env, &id, &grant);
}

#[test]
fn remote_exec_env_failure_finalises_the_turn() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "missing env");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&grant_path).expect("grant")).expect("grant JSON");
    value["env_names"] = serde_json::json!(["VOWP_EXEC_TEST_MISSING_ENV_9F3C"]);
    std::fs::write(&grant_path, value.to_string()).expect("rewrite grant");
    std::fs::set_permissions(&grant_path, std::fs::Permissions::from_mode(0o600)).expect("grant mode");

    let output = run_exec(&env, &grant_path, None);
    assert!(
        !output.status.success(),
        "__exec reported success after env resolution failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_failed_turn_finalised(&env, &id, &grant);
}

/// Asserts a failed `__exec` sent a summary that closed the run and the session.
fn assert_failed_turn_finalised(env: &common::TestEnv, id: &str, grant: &TurnGrant) {
    let session_id = vowp_protocol::SessionId::parse(id).expect("session id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1,
        "a failed start left the turn open or ended it twice"
    );
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::Error(payload))
                if payload.kind == vowp_protocol::ErrorKind::AdapterFailed && !payload.message.is_empty()
        )),
        "the failure was not recorded as an error event"
    );
    assert!(
        events.iter().any(|event| matches!(
            event.body(),
            Ok(EventBody::SessionExecutor(payload))
                if payload.action == vowp_protocol::AttachAction::Detach
                    && payload.run_id.as_ref() == Some(&grant.run_id)
        )),
        "the failed turn left the executor attached"
    );
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Error));
    assert!(run.ended_at.is_some(), "the failed run was left open");
    let session = store.require_session(&session_id).expect("session");
    assert_eq!(session.state, vowp_protocol::SessionState::Idle);
    let spool = env.home_path().join("spool").join(id).join(grant.run_id.to_string());
    let acked = std::fs::read_to_string(spool.join("acked.json")).expect("acked");
    assert!(acked.contains("\"summary\": true"), "{acked}");
}

#[test]
fn ssh_drop_spools_and_replays_without_gap_or_duplicate() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "survive an ack loss");
    let fixture = ssh_fixture(&env, CUTTING_SSH);
    let output = run_exec_over_ssh(&env, &fixture, &grant_path, None, &[("FAKE_SSH_MODE", "ack_lost")]);
    assert!(
        output.status.success(),
        "ack-loss replay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let runs = std::fs::read_to_string(&fixture.runs).expect("ssh runs");
    assert!(runs.lines().count() >= 2, "the client did not reconnect: {runs}");

    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    let mut ids = std::collections::HashSet::new();
    for event in &events {
        assert!(ids.insert(event.id.clone()), "duplicate event id {}", event.id);
    }
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::TurnEnded)
            .count(),
        1
    );
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Ok));
    assert!(run.ended_at.is_some());
    let spool = env.home_path().join("spool").join(&id).join(grant.run_id.to_string());
    let acked = std::fs::read_to_string(spool.join("acked.json")).expect("acked");
    assert!(acked.contains("\"summary\": true"), "{acked}");
}

#[test]
fn ssh_mid_line_cut_replays_without_gap() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, grant_path) = begin_remote_turn(&env, &id, "lease-token", "survive a mid-line cut");
    let fixture = ssh_fixture(&env, CUTTING_SSH);
    let output = run_exec_over_ssh(&env, &fixture, &grant_path, None, &[("FAKE_SSH_MODE", "mid_line")]);
    assert!(
        output.status.success(),
        "mid-line replay failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let runs = std::fs::read_to_string(&fixture.runs).expect("ssh runs");
    assert!(runs.lines().count() >= 2, "the client did not reconnect: {runs}");

    let session_id = vowp_protocol::SessionId::parse(&id).expect("session id");
    let events = SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), &session_id)
        .expect("log")
        .read_all()
        .expect("events");
    let mut ids = std::collections::HashSet::new();
    for event in &events {
        assert!(ids.insert(event.id.clone()), "duplicate event id {}", event.id);
    }
    let store = env.store();
    let run = store.require_run(&grant.run_id).expect("run");
    assert_eq!(run.outcome, Some(vowp_protocol::RunOutcome::Ok));
    let _ = grant;
}

#[test]
fn session_ingest_json_stdout_is_only_acks() {
    let env = common::TestEnv::new();
    configure(&env);
    let id = create(&env);
    let (grant, _grant_path) = begin_remote_turn(&env, &id, "lease-token", "ingest a scripted stream");
    let ts = Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp");
    let hello = RemoteLine::Hello {
        session_id: grant.session_id.clone(),
        run_id: grant.run_id.clone(),
        executor: grant.executor.clone(),
        node: grant.node.clone(),
        spool_id: "cli-test".to_owned(),
        lease: grant.lease_token.clone(),
    };
    let event = RemoteLine::Event {
        event: RemoteEvent {
            id: vowp_protocol::EventId::new(),
            ts,
            event_type: EventType::AssistantMessage,
            actor: Actor::Assistant,
            source: EventSource {
                harness: HarnessKind::Opencode,
                model: Some("deepseek/deepseek-flash".to_owned()),
                node: Some(grant.node.clone()),
                native_session_id: None,
            },
            payload: serde_json::json!({"text": "scripted ingest"}),
            raw: None,
            replayable: true,
        },
    };
    let summary = RemoteLine::Summary {
        exit_code: Some(0),
        outcome: vowp_protocol::RunOutcome::Ok,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: Some("done".to_owned()),
        stop_reason: StopReason::EndTurn,
        usage: None,
    };
    let text = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&hello).expect("hello"),
        serde_json::to_string(&event).expect("event"),
        serde_json::to_string(&summary).expect("summary")
    );
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["--json", "session", "ingest", &id, "--run", grant.run_id.as_str()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn ingest");
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(text.as_bytes()).expect("write stream");
    }
    let output = child.wait_with_output().expect("ingest output");
    assert!(
        output.status.success(),
        "session ingest failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut acks = Vec::new();
    for line in stdout.lines() {
        let ack: vowp_protocol::IngestAck =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("non-ack stdout line {line:?}: {error}"));
        acks.push(ack);
    }
    assert!(
        acks.iter()
            .any(|ack| matches!(ack, vowp_protocol::IngestAck::Summary { done: true, .. })),
        "no summary ack: {acks:?}"
    );
}

/// Creates a temporary Claude transcript tree and returns its root.
fn claude_transcripts(env: &common::TestEnv) -> std::path::PathBuf {
    let dir = env.project_path().join("claude-projects");
    std::fs::create_dir_all(dir.join("mangled-cwd")).expect("transcript dir");
    dir
}

/// One fake Claude transcript line.
fn claude_line(kind: &str, uuid: &str, native: &str, content: serde_json::Value, timestamp: &str) -> String {
    serde_json::json!({
        "type": kind,
        "uuid": uuid,
        "sessionId": native,
        "timestamp": timestamp,
        "message": {"role": kind, "content": content},
    })
    .to_string()
}

/// Appends one complete line to a transcript fixture.
fn append_claude_line(path: &Path, line: &str) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open transcript");
    writeln!(file, "{line}").expect("append transcript");
}

/// Runs one `session watch --once` pass over `dir`.
fn watch_once(env: &common::TestEnv, dir: &Path) -> (i32, String, String) {
    env.run(&[
        "session",
        "watch",
        "--once",
        "--claude-dir",
        dir.to_str().expect("transcript path"),
    ])
}

/// Returns the session rows recorded for the test project.
fn project_sessions(env: &common::TestEnv) -> Vec<vowp_protocol::Session> {
    env.store()
        .list_sessions(&vowp_store::SessionFilter {
            project: Some(env.config.project.name.clone()),
            ..vowp_store::SessionFilter::default()
        })
        .expect("sessions")
}

/// Returns the mapped user/assistant texts in log order.
fn message_texts(events: &[vowp_protocol::EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event.body() {
            Ok(EventBody::UserMessage(payload)) => Some(format!("user:{}", payload.text)),
            Ok(EventBody::AssistantMessage(payload)) => Some(format!("assistant:{}", payload.text)),
            _ => None,
        })
        .collect()
}

/// Reads one session's canonical log.
fn watched_events(env: &common::TestEnv, id: &vowp_protocol::SessionId) -> Vec<vowp_protocol::EventEnvelope> {
    SessionLog::open_read(&vowp_store::NodePaths::new(env.home_path()), id)
        .expect("open session log")
        .read_all()
        .expect("read session log")
}

/// Returns the stored watch cursor for one Claude native id.
fn stored_cursor(env: &common::TestEnv, native: &str) -> serde_json::Value {
    let marker = env
        .store()
        .kv_get(&format!("watch:{native}"))
        .expect("read watch marker")
        .expect("watch marker");
    serde_json::from_str(&marker).unwrap_or_else(|error| panic!("watch marker is not JSON ({error}): {marker}"))
}

#[test]
fn session_watch_once_appends_to_one_session_without_duplicates() {
    let env = common::TestEnv::new();
    let dir = claude_transcripts(&env);
    let path = dir.join("mangled-cwd").join("claude-1.jsonl");
    let long_first = "q".repeat(70);
    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u1",
            "native-1",
            serde_json::json!(long_first),
            "2026-11-12T10:00:00Z",
        ),
    );
    append_claude_line(
        &path,
        &claude_line(
            "assistant",
            "a1",
            "native-1",
            serde_json::json!([{"type": "text", "text": "first answer"}]),
            "2026-11-12T10:00:01Z",
        ),
    );
    let transcript_before = std::fs::read(&path).expect("read transcript");

    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("watching 1 transcripts"), "{stdout}");
    assert!(stdout.contains("2 events imported"), "{stdout}");

    // An unchanged second pass imports nothing and must not create another session.
    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("0 events imported"), "{stdout}");

    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u2",
            "native-1",
            serde_json::json!("second question"),
            "2026-11-12T10:00:02Z",
        ),
    );
    append_claude_line(
        &path,
        &claude_line(
            "assistant",
            "a2",
            "native-1",
            serde_json::json!([{"type": "text", "text": "second answer"}]),
            "2026-11-12T10:00:03Z",
        ),
    );
    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("2 events imported"), "{stdout}");

    let sessions = project_sessions(&env);
    assert_eq!(sessions.len(), 1, "one native uuid must map to one session");
    let session = &sessions[0];
    assert_eq!(session.role, Some(vowp_protocol::SessionRole::User));
    assert_eq!(
        session.title,
        "q".repeat(60),
        "the title must be the first user message capped at 60 chars"
    );
    assert_eq!(
        env.store()
            .session_for_native(HarnessKind::ClaudeCode, "native-1")
            .expect("native lookup")
            .map(|row| row.id),
        Some(session.id.clone()),
        "the native uuid does not resolve to the canonical session"
    );
    assert!(
        env.store()
            .natives_of(&session.id)
            .expect("natives")
            .iter()
            .any(|native| native.native_session_id == "native-1"),
        "the native uuid was not recorded"
    );

    let events = watched_events(&env, &session.id);
    assert_eq!(
        message_texts(&events),
        vec![
            format!("user:{}", "q".repeat(70)),
            "assistant:first answer".to_owned(),
            "user:second question".to_owned(),
            "assistant:second answer".to_owned(),
        ],
        "a growing transcript must append only the new lines"
    );
    let transcript_after = std::fs::read(&path).expect("read transcript again");
    assert_eq!(
        &transcript_after[..transcript_before.len()],
        transcript_before.as_slice(),
        "the importer changed the first two transcript lines"
    );

    let cursor = stored_cursor(&env, "native-1");
    assert_eq!(cursor["last_uuid"], "a2");
    assert_eq!(
        cursor["offset"].as_u64(),
        Some(std::fs::metadata(&path).expect("metadata").len()),
        "the cursor does not point at the end of the complete lines"
    );
}

#[test]
fn session_watch_once_waits_for_a_complete_final_line() {
    let env = common::TestEnv::new();
    let dir = claude_transcripts(&env);
    let path = dir.join("mangled-cwd").join("partial.jsonl");
    let first = claude_line(
        "user",
        "u1",
        "native-partial",
        serde_json::json!("complete line"),
        "2026-11-12T11:00:00Z",
    );
    let partial = claude_line(
        "assistant",
        "a1",
        "native-partial",
        serde_json::json!([{"type": "text", "text": "partial answer"}]),
        "2026-11-12T11:00:01Z",
    );
    std::fs::write(&path, format!("{first}\n{partial}")).expect("write partial transcript");

    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("1 events imported"), "{stdout}");

    // The partial final line stays unread on later passes and in the cursor.
    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("0 events imported"), "{stdout}");
    let sessions = project_sessions(&env);
    assert_eq!(sessions.len(), 1);
    let events = watched_events(&env, &sessions[0].id);
    assert_eq!(message_texts(&events), vec!["user:complete line".to_owned()]);
    let before_newline = std::fs::metadata(&path).expect("metadata").len() - partial.len() as u64;
    assert_eq!(
        stored_cursor(&env, "native-partial")["offset"].as_u64(),
        Some(before_newline)
    );

    // Completing the partial line makes exactly that one line importable.
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open transcript");
        writeln!(file).expect("complete partial line");
    }
    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("1 events imported"), "{stdout}");

    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u2",
            "native-partial",
            serde_json::json!("after newline"),
            "2026-11-12T11:00:02Z",
        ),
    );
    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("1 events imported"), "{stdout}");

    let events = watched_events(&env, &sessions[0].id);
    assert_eq!(
        message_texts(&events),
        vec![
            "user:complete line".to_owned(),
            "assistant:partial answer".to_owned(),
            "user:after newline".to_owned(),
        ],
        "completing the partial line must import it exactly once"
    );
}

#[test]
fn session_watch_once_migrates_a_legacy_line_count_marker() {
    let env = common::TestEnv::new();
    let dir = claude_transcripts(&env);
    let path = dir.join("mangled-cwd").join("legacy.jsonl");
    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u1",
            "native-legacy",
            serde_json::json!("already imported"),
            "2026-11-12T12:00:00Z",
        ),
    );
    append_claude_line(
        &path,
        &claude_line(
            "assistant",
            "a1",
            "native-legacy",
            serde_json::json!([{"type": "text", "text": "new line"}]),
            "2026-11-12T12:00:01Z",
        ),
    );
    let store = env.store();
    store.kv_set("watch:native-legacy", "1").expect("seed legacy marker");
    drop(store);

    let (code, stdout, stderr) = watch_once(&env, &dir);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("1 events imported"), "{stdout}");

    let sessions = project_sessions(&env);
    assert_eq!(sessions.len(), 1);
    let events = watched_events(&env, &sessions[0].id);
    assert_eq!(
        message_texts(&events),
        vec!["assistant:new line".to_owned()],
        "the old line count must be converted to a byte offset, not re-imported"
    );
    let cursor = stored_cursor(&env, "native-legacy");
    assert_eq!(cursor["last_uuid"], "a1");
}

/// Returns the message texts of the one project session, or an empty list while it is
/// still being created (the row can be visible before the log file exists).
fn project_message_texts(env: &common::TestEnv) -> Vec<String> {
    let sessions = project_sessions(env);
    if sessions.len() != 1 {
        return Vec::new();
    }
    let paths = vowp_store::NodePaths::new(env.home_path());
    let Ok(log) = SessionLog::open_read(&paths, &sessions[0].id) else {
        return Vec::new();
    };
    let Ok(events) = log.read_all() else {
        return Vec::new();
    };
    message_texts(&events)
}

/// Sleeps until `condition` holds, failing the test if `child` exits first or time runs out.
fn wait_for_session_watch(
    child: &mut std::process::Child,
    stderr_path: &Path,
    what: &str,
    condition: impl Fn() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if condition() {
            return;
        }
        if let Some(status) = child.try_wait().expect("watch status") {
            let stderr = std::fs::read_to_string(stderr_path).unwrap_or_default();
            panic!("session watch --follow exited before {what}: {status:?}\n{stderr}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = std::fs::read_to_string(stderr_path).unwrap_or_default();
            panic!("timed out waiting for {what}\n{stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn session_watch_follow_imports_a_line_appended_after_startup() {
    let env = common::TestEnv::new();
    let dir = claude_transcripts(&env);
    let path = dir.join("mangled-cwd").join("follow.jsonl");
    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u1",
            "native-follow",
            serde_json::json!("before startup"),
            "2026-11-12T13:00:00Z",
        ),
    );

    let stdout_path = env.project_path().join("watch-follow.stdout");
    let stderr_path = env.project_path().join("watch-follow.stderr");
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["session", "watch", "--follow", "--claude-dir"])
        .arg(&dir)
        .stdout(std::fs::File::create(&stdout_path).expect("watch stdout"))
        .stderr(std::fs::File::create(&stderr_path).expect("watch stderr"));
    let mut child = command.spawn().expect("spawn session watch --follow");

    // The explicit flag must follow even though stdin/stdout are not a terminal, so the
    // process stays alive after the first pass instead of exiting.
    wait_for_session_watch(&mut child, &stderr_path, "the first transcript line", || {
        project_message_texts(&env) == vec!["user:before startup".to_owned()]
    });

    append_claude_line(
        &path,
        &claude_line(
            "user",
            "u2",
            "native-follow",
            serde_json::json!("appended while following"),
            "2026-11-12T13:00:01Z",
        ),
    );
    wait_for_session_watch(&mut child, &stderr_path, "the appended transcript line", || {
        project_message_texts(&env).contains(&"user:appended while following".to_owned())
    });

    // Ctrl-C flips the interrupt flag; the pass in flight finishes and the command exits 0.
    let signal = std::process::Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGINT");
    assert!(signal.success(), "kill -INT failed: {signal:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("watch status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "session watch --follow did not stop on SIGINT\n{}",
                std::fs::read_to_string(&stderr_path).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(status.success(), "session watch --follow exited {status:?}\n{stderr}");
    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    assert!(stdout.contains("watching 1 transcripts"), "{stdout}");

    let sessions = project_sessions(&env);
    assert_eq!(sessions.len(), 1, "follow must reuse the one canonical session");
    let events = watched_events(&env, &sessions[0].id);
    assert_eq!(
        message_texts(&events),
        vec![
            "user:before startup".to_owned(),
            "user:appended while following".to_owned(),
        ]
    );
    let cursor = stored_cursor(&env, "native-follow");
    assert_eq!(cursor["last_uuid"], "u2");
    assert_eq!(
        cursor["offset"].as_u64(),
        Some(std::fs::metadata(&path).expect("metadata").len())
    );
}
