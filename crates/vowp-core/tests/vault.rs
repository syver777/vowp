//! Secret vault integration tests: storage, injection, redaction and the one-time page.
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §9 and the vault tests in §15.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::json;
use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::tempdir;
use vowp_core::session::service::SessionService;
use vowp_core::vault::{
    self, REQUEST_TTL, Redactor, SecretRequestServer, redact_capsule, redact_event, redact_job_artifacts, redact_text,
};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    Actor, AssistantMessage, CancelToken, Capsule, CapsuleId, CapsuleState, CapsuleTrigger, Cost, EventBody,
    EventSource, Executor, ExecutorRef, FakeClock, HarnessConfig, HarnessKind, JobId, JobKind, MemoryLog, NewJob,
    NewSession, NodeName, Role, Run, RunPurpose, RunRole, SecretInfo, SessionId, SessionRole, Timestamp, ToolCall,
    TurnRequest, VowpConfig,
};
use vowp_store::session_files::{read_capsule_file, write_capsule_file};
use vowp_store::session_log::{NewEvent, SessionLog};
use vowp_store::{NodePaths, Store};

/// Fixed instant every test clock starts at.
fn now() -> Timestamp {
    Timestamp::parse("2026-11-10T14:00:00Z").expect("test timestamp")
}

/// Builds hermetic dependencies with a fixed clock and collecting log.
fn deps_at(root: &Path, config: VowpConfig, clock: Timestamp) -> (Deps, MemoryLog) {
    let paths = NodePaths::new(root.join("node"));
    paths.ensure().expect("node paths");
    let log = MemoryLog::new();
    let deps = Deps {
        paths,
        config: Arc::new(config),
        node: NodeName::parse("local").expect("node"),
        clock: Arc::new(FakeClock::at(clock)),
        log: Arc::new(log.clone()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: Arc::new(vowp_core::SystemGit::default()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    };
    Store::open(&deps.paths).expect("global store");
    (deps, log)
}

/// Builds hermetic dependencies at the default test instant.
fn deps(root: &Path) -> (Deps, MemoryLog) {
    deps_at(root, VowpConfig::with_defaults_for_tests(), now())
}

/// Builds an executor reference for the default opencode harness.
fn executor() -> ExecutorRef {
    ExecutorRef {
        executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
        node: None,
    }
}

/// Returns the basic-mode secret path.
fn secret_path(deps: &Deps, name: &str) -> PathBuf {
    deps.paths.root().join("secrets").join(name)
}

/// Reads a `SecretInfo` list by name.
fn names(infos: &[vowp_protocol::SecretInfo]) -> Vec<String> {
    infos.iter().map(|info| info.name.to_string()).collect()
}

/// Returns the workspace `vowp` binary next to the test executable, when it is built.
fn workspace_vowp_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?;
    let candidate = profile_dir.join("vowp");
    candidate.is_file().then_some(candidate)
}

#[test]
fn real_helper_context_does_not_need_the_owner_project() {
    let Some(vowp) = workspace_vowp_binary() else {
        eprintln!("skipping real-helper test: the workspace `vowp` binary is not built");
        return;
    };
    let tmp = tempdir().expect("tempdir");
    let vault_home = tmp.path().join("vault-home");
    let helper_home = vault_home.join("helper");
    fs::create_dir_all(&helper_home).expect("helper home");
    let config = toml::to_string(&VowpConfig::with_defaults_for_tests()).expect("helper config");
    fs::write(helper_home.join("vowp.toml"), config).expect("helper config file");
    fs::write(helper_home.join("vowp.db"), "").expect("helper db");
    let owner_project = tmp.path().join("project");
    fs::create_dir_all(&owner_project).expect("project dir");
    let owner_config = owner_project.join("vowp.toml");
    fs::write(&owner_config, "unreadable to the vault user\n").expect("owner config");
    fs::set_permissions(&owner_config, fs::Permissions::from_mode(0o000)).expect("owner config mode");

    let mut child = Command::new(&vowp)
        .arg("--home")
        .arg(&helper_home)
        .arg("--project")
        .arg(&helper_home)
        .args(["__vault-admin", "set", "HELPER_KEY"])
        .env("VOWP_VAULT_HOME", &vault_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper");
    child
        .stdin
        .as_mut()
        .expect("helper stdin")
        .write_all(b"helper-value-7531\n")
        .expect("helper value");
    let output = child.wait_with_output().expect("helper output");
    assert!(
        output.status.success(),
        "the real helper must run without the owner project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored = vault_home.join("secrets/HELPER_KEY");
    assert_eq!(
        fs::read_to_string(&stored).expect("stored helper value"),
        "helper-value-7531\n",
        "the hidden helper must store the value byte-exactly"
    );
    assert_eq!(
        fs::metadata(&stored).expect("stored metadata").permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn basic_mode_writes_lists_replaces_and_removes() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("DEEPSEEK_API_KEY").expect("name");

    let first = vault::write_secret(&deps, &name, "sk-first-value-4f2a").expect("write");
    assert_eq!(first.name, name);
    assert_eq!(first.last4, "4f2a");
    assert_eq!(first.created_at, now());
    assert_eq!(first.updated_at, now());
    let path = secret_path(&deps, "DEEPSEEK_API_KEY");
    assert_eq!(fs::read_to_string(&path).expect("value"), "sk-first-value-4f2a");
    assert_eq!(
        fs::metadata(&path).expect("file metadata").permissions().mode() & 0o777,
        0o600,
        "secret files are mode 0600"
    );
    assert_eq!(
        fs::metadata(path.parent().expect("parent"))
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "the secrets directory is mode 0700"
    );

    let listed = vault::list_secrets(&deps).expect("list");
    assert_eq!(names(&listed), vec!["DEEPSEEK_API_KEY"]);
    assert_eq!(listed[0].last4, "4f2a");

    let later = now().plus(Duration::from_secs(60));
    let (later_deps, _) = deps_at(tmp.path(), VowpConfig::with_defaults_for_tests(), later);
    let second = vault::write_secret(&later_deps, &name, "sk-second-value-9c1d").expect("replace");
    assert_eq!(second.created_at, now(), "created_at survives a replacement");
    assert_eq!(second.updated_at, later);
    assert_eq!(second.last4, "9c1d");
    assert_eq!(fs::read_to_string(&path).expect("value"), "sk-second-value-9c1d");

    assert!(vault::remove_secret(&later_deps, &name).expect("remove"));
    assert!(!path.exists());
    assert!(vault::list_secrets(&later_deps).expect("list").is_empty());
    assert!(!vault::remove_secret(&later_deps, &name).expect("second remove"));
}

#[test]
fn legacy_kv_secret_metadata_migrates_to_the_secret_index_once() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("LEGACY_KEY").expect("name");
    let legacy = SecretInfo {
        name: name.clone(),
        last4: "4f2a".to_owned(),
        created_at: now(),
        updated_at: now().plus(Duration::from_secs(60)),
    };
    let secrets = deps.paths.root().join("secrets");
    fs::create_dir_all(&secrets).expect("secrets dir");
    fs::write(secrets.join("mode"), "basic\n").expect("mode file");
    {
        let store = Store::open_global(&deps.paths).expect("store");
        let text = serde_json::to_string(&legacy).expect("legacy metadata");
        store.kv_set("secret:LEGACY_KEY", &text).expect("legacy row");
        store.kv_set("unrelated", "kept").expect("unrelated row");
    }

    let listed = vault::list_secrets(&deps).expect("list migrates the legacy row");

    assert_eq!(listed, vec![legacy.clone()]);
    {
        let store = Store::open_global(&deps.paths).expect("store");
        assert_eq!(store.list_secret_index().expect("index rows"), vec![legacy.clone()]);
        assert!(
            store.kv_prefix("secret:").expect("legacy rows").is_empty(),
            "the migrated kv keys must be removed"
        );
        assert_eq!(
            store.kv_get("unrelated").expect("unrelated row"),
            Some("kept".to_owned()),
            "unrelated kv rows must stay"
        );
    }
    assert_eq!(
        fs::read_to_string(secrets.join("mode")).expect("mode"),
        "basic\n",
        "the vault mode metadata must be preserved"
    );
    assert_eq!(
        vault::list_secrets(&deps).expect("second access"),
        vec![legacy],
        "a second access and migration is a no-op"
    );
}

#[test]
fn invalid_and_empty_values_are_refused() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    assert!(vowp_protocol::SecretName::parse("lowercase").is_err());
    assert!(vowp_protocol::SecretName::parse("1LEADING").is_err());
    let name = vowp_protocol::SecretName::parse("OK_NAME").expect("name");
    let error = vault::write_secret(&deps, &name, "").expect_err("empty value");
    assert!(error.to_string().contains("must not be empty"));
    assert!(!secret_path(&deps, "OK_NAME").exists());
}

#[test]
fn env_for_resolves_only_configured_names_and_warns_for_missing() {
    let tmp = tempdir().expect("tempdir");
    let mut config = VowpConfig::with_defaults_for_tests();
    config.harnesses.insert(
        "opencode".to_owned(),
        HarnessConfig {
            env_pass: vec![
                "CONFIGURED_KEY".to_owned(),
                "SECOND_KEY".to_owned(),
                "MISSING_KEY".to_owned(),
                "not a name".to_owned(),
            ],
            ..HarnessConfig::default()
        },
    );
    let (deps, log) = deps_at(tmp.path(), config, now());
    for (name, value) in [
        ("CONFIGURED_KEY", "configured-value"),
        ("SECOND_KEY", "second-value"),
        ("UNLISTED_KEY", "unlisted-value"),
    ] {
        vault::write_secret(&deps, &vowp_protocol::SecretName::parse(name).expect("name"), value).expect("write");
    }
    let request = TurnRequest {
        session_id: SessionId::new(),
        executor: executor(),
        purpose: RunPurpose::Start,
        role: RunRole::Worker,
        job_id: None,
        prompt: String::new(),
        profile: None,
        rung: 0,
        window: None,
        timeout: None,
    };
    let env = vault::env_for(&deps, &request).expect("env");
    assert_eq!(
        env,
        vec![
            ("CONFIGURED_KEY".to_owned(), "configured-value".to_owned()),
            ("SECOND_KEY".to_owned(), "second-value".to_owned()),
        ]
    );
    assert!(log.contains("MISSING_KEY"));
    assert!(log.contains("invalid secret name"));
    assert!(!log.contains("configured-value"));
    assert!(!log.contains("second-value"));
}

#[test]
fn redactor_replaces_exact_values_longest_first() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    for (name, value) in [("SHORT_KEY", "abcd"), ("LONG_KEY", "abcdef")] {
        vault::write_secret(&deps, &vowp_protocol::SecretName::parse(name).expect("name"), value).expect("write");
    }
    let redactor = Redactor::load(&deps).expect("load");
    assert_eq!(
        redactor.redact("xxabcdefyy abcd"),
        "xx[REDACTED:LONG_KEY]yy [REDACTED:SHORT_KEY]"
    );
    assert_eq!(
        redact_text(&deps, "abcdef and abcd"),
        "[REDACTED:LONG_KEY] and [REDACTED:SHORT_KEY]"
    );
    assert!(!redactor.is_empty());
}

#[test]
fn redact_event_covers_typed_body_and_raw_json() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    vault::write_secret(
        &deps,
        &vowp_protocol::SecretName::parse("EVENT_KEY").expect("name"),
        "event-secret-7d1e",
    )
    .expect("write");
    let event = NewEvent {
        actor: Actor::Assistant,
        source: EventSource {
            harness: HarnessKind::Opencode,
            model: Some("deepseek/deepseek-flash".to_owned()),
            node: Some(NodeName::parse("local").expect("node")),
            native_session_id: None,
        },
        body: EventBody::AssistantMessage(AssistantMessage {
            text: "the token is event-secret-7d1e".to_owned(),
        }),
        raw: Some(json!({"stdout": "event-secret-7d1e", "nested": {"value": "event-secret-7d1e"}})),
        ts: Some(now()),
    };
    let redacted = redact_event(&deps, event).expect("redact");
    let text = serde_json::to_string(&redacted.body.to_value()).expect("json");
    assert!(!text.contains("event-secret-7d1e"));
    assert!(text.contains("[REDACTED:EVENT_KEY]"));
    let raw = serde_json::to_string(redacted.raw.as_ref().expect("raw")).expect("raw json");
    assert!(!raw.contains("event-secret-7d1e"));
    assert!(raw.contains("[REDACTED:EVENT_KEY]"));
    match redacted.body {
        EventBody::AssistantMessage(message) => {
            assert_eq!(message.text, "the token is [REDACTED:EVENT_KEY]");
        }
        other => panic!("unexpected body: {other:?}"),
    }
}

#[test]
fn redact_event_also_covers_tool_arguments() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    vault::write_secret(
        &deps,
        &vowp_protocol::SecretName::parse("TOOL_KEY").expect("name"),
        "tool-secret-2b8f",
    )
    .expect("write");
    let event = NewEvent {
        actor: Actor::Assistant,
        source: EventSource {
            harness: HarnessKind::Opencode,
            model: None,
            node: None,
            native_session_id: None,
        },
        body: EventBody::ToolCall(ToolCall {
            call_id: "call_1".to_owned(),
            name: "shell".to_owned(),
            input: json!({"command": "curl -H 'Authorization: tool-secret-2b8f'"}),
        }),
        raw: None,
        ts: None,
    };
    let redacted = redact_event(&deps, event).expect("redact");
    let text = serde_json::to_string(&redacted.body.to_value()).expect("json");
    assert!(!text.contains("tool-secret-2b8f"));
    assert!(text.contains("[REDACTED:TOOL_KEY]"));
}

#[test]
fn redact_capsule_rewrites_file_and_database_row() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    fs::create_dir_all(tmp.path().join("project")).expect("project dir");
    let service = vowp_core::session::service::SessionService::new(deps.clone());
    let session = service
        .create_session(NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: "capsule redaction".to_owned(),
            cwd: tmp.path().join("project"),
            parent: None,
            executor: Some(executor().executor),
        })
        .expect("session");
    vault::write_secret(
        &deps,
        &vowp_protocol::SecretName::parse("CAPSULE_KEY").expect("name"),
        "capsule-secret-1a2b",
    )
    .expect("write");
    let capsule = Capsule {
        version: vowp_protocol::CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: now(),
        at_seq: 1,
        trigger: CapsuleTrigger::TurnEnd,
        from: executor(),
        intent: "use capsule-secret-1a2b".to_owned(),
        constraints: vec!["never print capsule-secret-1a2b".to_owned()],
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: Default::default(),
    };
    let store = Store::open(&deps.paths).expect("store");
    let path = write_capsule_file(&deps.paths, &capsule).expect("capsule file");
    store
        .insert_capsule(&capsule, &path.display().to_string())
        .expect("capsule row");

    let redacted = redact_capsule(&deps, &store, &capsule).expect("redact");

    assert!(!redacted.intent.contains("capsule-secret-1a2b"));
    assert!(redacted.intent.contains("[REDACTED:CAPSULE_KEY]"));
    let file = read_capsule_file(&path).expect("read file");
    assert!(!file.intent.contains("capsule-secret-1a2b"));
    let row = store.capsule(&capsule.capsule_id).expect("row").expect("exists");
    assert!(!row.intent.contains("capsule-secret-1a2b"));
    assert_eq!(row, redacted);
}

#[test]
fn redact_job_artifacts_redacts_report_and_run_mirror() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    vault::write_secret(
        &deps,
        &vowp_protocol::SecretName::parse("ARTIFACT_KEY").expect("name"),
        "artifact-secret-3c4d",
    )
    .expect("write");
    let store = Store::open(&deps.paths).expect("store");
    let project_name = vowp_protocol::ProjectName::parse("demo").expect("project");
    let project_root = tmp.path().join("project");
    fs::create_dir_all(&project_root).expect("project dir");
    store
        .upsert_project(&vowp_protocol::Project {
            name: project_name.clone(),
            path: project_root.display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: json!({}),
            created_at: now(),
            updated_at: now(),
        })
        .expect("project row");
    let job = store
        .insert_job(
            &project_name,
            &NewJob {
                id: JobId::parse("vault-check").expect("job id"),
                title: "vault test".to_owned(),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: None,
                needs: Vec::new(),
                owns: Vec::new(),
                reads: Vec::new(),
                gates: None,
                priority: None,
                loop_: None,
                brief_text: None,
            },
            "test",
        )
        .expect("job");
    let worktree = tmp.path().join("worktree");
    fs::create_dir_all(&worktree).expect("worktree");
    store
        .set_job_workspace(&job.id, Some("job/test"), Some(&worktree.display().to_string()))
        .expect("workspace");
    let job = store.require_job(&job.id).expect("job row");
    let report = worktree.join(&job.report);
    fs::create_dir_all(report.parent().expect("report parent")).expect("report dir");
    fs::write(&report, "report contains artifact-secret-3c4d\n").expect("report");
    let run = Run {
        id: vowp_protocol::RunId::new(),
        job_id: Some(job.id.clone()),
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(executor().executor),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").expect("node"),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: Some(worktree.display().to_string()),
        started_at: now(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&run).expect("run");
    let raw = deps.paths.run_raw_path(&run.id);
    fs::write(&raw, "{\"stdout\":\"artifact-secret-3c4d\"}\n").expect("raw");

    redact_job_artifacts(&deps, &store, Some(&job.id)).expect("redact");

    let report_text = fs::read_to_string(&report).expect("report");
    let raw_text = fs::read_to_string(&raw).expect("raw");
    assert!(!report_text.contains("artifact-secret-3c4d"));
    assert!(report_text.contains("[REDACTED:ARTIFACT_KEY]"));
    assert!(!raw_text.contains("artifact-secret-3c4d"));
    assert!(raw_text.contains("[REDACTED:ARTIFACT_KEY]"));
}

#[test]
fn mode_file_is_read_and_unknown_values_are_refused() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    assert_eq!(
        vault::mode(&deps).expect("missing mode"),
        vowp_protocol::VaultMode::Basic
    );
    let dir = deps.paths.root().join("secrets");
    fs::create_dir_all(&dir).expect("secrets dir");
    fs::write(dir.join("mode"), "vault\n").expect("mode");
    assert_eq!(
        vault::mode(&deps).expect("vault mode"),
        vowp_protocol::VaultMode::VaultUser
    );
    fs::write(dir.join("mode"), "bogus\n").expect("mode");
    let error = vault::mode(&deps).expect_err("bogus mode");
    assert!(error.to_string().contains("must contain"));
}

#[test]
fn run_turn_injects_configured_secret_and_never_writes_it_to_the_log_or_events() {
    let tmp = tempdir().expect("tempdir");
    let mut config = VowpConfig::with_defaults_for_tests();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).expect("project dir");
    config.project.path = Some(project.display().to_string());
    config.harnesses.insert(
        "opencode".to_owned(),
        HarnessConfig {
            env_pass: vec!["TURN_KEY".to_owned()],
            ..HarnessConfig::default()
        },
    );
    let paths = NodePaths::new(tmp.path().join("node"));
    paths.ensure().expect("node paths");
    let log = MemoryLog::new();
    let fake = std::sync::Arc::new(FakeAdapter::from_bodies(
        HarnessKind::Opencode,
        now(),
        vec![EventBody::AssistantMessage(AssistantMessage {
            text: "the harness saw turn-secret-5e6f".to_owned(),
        })],
    ));
    let deps = Deps {
        paths,
        config: std::sync::Arc::new(config),
        node: NodeName::parse("local").expect("node"),
        clock: std::sync::Arc::new(FakeClock::at(now())),
        log: std::sync::Arc::new(log),
        adapters: std::sync::Arc::new(AdapterRegistry::new().with("opencode", fake.clone())),
        windows: std::sync::Arc::new(vowp_tmux::fake::FakeWindows::new()),
        git: std::sync::Arc::new(FakeGit::new()),
        cmd: std::sync::Arc::new(SystemRunner),
        rng: std::sync::Arc::new(FixedRng(0)),
    };
    Store::open(&deps.paths).expect("store");
    let name = vowp_protocol::SecretName::parse("TURN_KEY").expect("name");
    vault::write_secret(&deps, &name, "turn-secret-5e6f").expect("write");
    let service = SessionService::new(deps.clone());
    let session = service
        .create_session(NewSession {
            project: None,
            job_id: None,
            role: SessionRole::Worker,
            title: "secret turn".to_owned(),
            cwd: project,
            parent: None,
            executor: Some(executor().executor),
        })
        .expect("session");
    let request = TurnRequest {
        session_id: session.clone(),
        executor: executor(),
        purpose: RunPurpose::Start,
        role: RunRole::Worker,
        job_id: None,
        prompt: "run".to_owned(),
        profile: Some("worker".to_owned()),
        rung: 0,
        window: None,
        timeout: None,
    };
    service.run_turn(request, &CancelToken::default()).expect("turn");

    let starts = fake.starts();
    assert_eq!(
        starts[0].env,
        vec![("TURN_KEY".to_owned(), "turn-secret-5e6f".to_owned())],
        "the configured name is injected into the adapter environment"
    );
    let events = SessionLog::open_read(&deps.paths, &session)
        .expect("log")
        .read_all()
        .expect("events");
    for event in &events {
        let body = serde_json::to_string(&event.body().expect("body").to_value()).expect("body json");
        assert!(!body.contains("turn-secret-5e6f"), "value in event: {body}");
        if let Some(raw) = &event.raw {
            assert!(!raw.to_string().contains("turn-secret-5e6f"));
        }
    }
    let log_text = fs::read_to_string(deps.paths.log_path(&session)).expect("log file");
    assert!(!log_text.contains("turn-secret-5e6f"));
}

#[test]
fn redact_text_redacts_a_snapshot_shaped_fixture() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    vault::write_secret(
        &deps,
        &vowp_protocol::SecretName::parse("SNAPSHOT_KEY").expect("name"),
        "snapshot-secret-9a8b",
    )
    .expect("write");
    let fixture = json!({
        "schema": "vowp.snapshot",
        "version": 1,
        "projects": [{"name": "demo", "note": "snapshot-secret-9a8b"}],
        "sessions": [{"id": "ses_01", "title": "use snapshot-secret-9a8b"}],
        "inbox": [{"title": "question", "body": "value snapshot-secret-9a8b"}]
    });
    let text = serde_json::to_string(&fixture).expect("fixture json");

    let redacted = redact_text(&deps, &text);

    assert!(!redacted.contains("snapshot-secret-9a8b"));
    assert!(redacted.contains("[REDACTED:SNAPSHOT_KEY]"));
    let parsed: serde_json::Value = serde_json::from_str(&redacted).expect("valid json");
    assert_eq!(parsed["projects"][0]["note"], "[REDACTED:SNAPSHOT_KEY]");
}

/// Runs one HTTP request against a loopback URL and returns status and body.
fn http(url: &str, method: &str, content_type: Option<&str>, body: &str) -> (u16, String) {
    let rest = url.strip_prefix("http://").expect("http url");
    let (authority, path) = rest.split_once('/').expect("path");
    let mut stream = TcpStream::connect(authority).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let mut request = format!("{method} /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if method == "POST" {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn one_time_page_serves_a_form_and_stores_exactly_one_value() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("REQUEST_KEY").expect("name");
    let server = SecretRequestServer::bind_with_token(&deps, &name, now(), "fixed-token".to_owned()).expect("bind");
    let url = server.request().url.clone();
    assert_eq!(server.request().expires_at, now().plus(REQUEST_TTL));
    assert!(server.request().single_use);
    assert_eq!(
        url,
        format!("http://{}/s/fixed-token", server.local_addr().expect("addr"))
    );

    let client = {
        let url = url.clone();
        thread::spawn(move || {
            let (wrong_status, _) = http(&url.replace("fixed-token", "wrong-token"), "GET", None, "");
            assert_eq!(wrong_status, 404, "a wrong token must not open the form");
            let (status, body) = http(&url, "GET", None, "");
            assert_eq!(status, 200);
            assert!(body.contains("REQUEST_KEY"));
            assert!(body.contains("type=\"password\""));
            let (status, body) = http(
                &url,
                "POST",
                Some("application/x-www-form-urlencoded"),
                "value=sk+test%2B4f2a",
            );
            assert_eq!(status, 200);
            assert!(body.contains("REQUEST_KEY saved"));
            assert!(body.contains("4f2a"));
        })
    };
    let info = server.wait(Duration::from_secs(5)).expect("saved");
    client.join().expect("client");
    assert_eq!(info.name, name);
    assert_eq!(info.last4, "4f2a");
    assert_eq!(
        fs::read_to_string(secret_path(&deps, "REQUEST_KEY")).expect("value"),
        "sk test+4f2a",
        "form values are percent-decoded"
    );
    assert!(
        TcpStream::connect(url.trim_start_matches("http://").split('/').next().expect("authority")).is_err(),
        "the listener closes after one successful POST"
    );
}

#[test]
fn one_time_page_keeps_the_request_open_after_a_malformed_post() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("RETRY_KEY").expect("name");
    let server = SecretRequestServer::bind_with_token(&deps, &name, now(), "retry-token".to_owned()).expect("bind");
    let url = server.request().url.clone();
    let client = {
        let url = url.clone();
        thread::spawn(move || {
            let (status, _) = http(
                &url,
                "POST",
                Some("application/x-www-form-urlencoded"),
                "other=not-the-value",
            );
            assert_eq!(status, 400, "a POST without `value` is rejected");
            let (status, body) = http(
                &url,
                "POST",
                Some("application/x-www-form-urlencoded"),
                "value=retry-value-1357",
            );
            assert_eq!(status, 200);
            assert!(body.contains("RETRY_KEY saved"));
        })
    };
    let info = server.wait(Duration::from_secs(5)).expect("saved after retry");
    client.join().expect("client");
    assert_eq!(info.last4, "1357");
    assert_eq!(
        fs::read_to_string(secret_path(&deps, "RETRY_KEY")).expect("value"),
        "retry-value-1357"
    );
}

#[test]
fn one_time_page_survives_a_connection_that_sends_no_headers() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("PRECONNECT_KEY").expect("name");
    let server =
        SecretRequestServer::bind_with_token(&deps, &name, now(), "preconnect-token".to_owned()).expect("bind");
    let url = server.request().url.clone();
    let client = {
        let url = url.clone();
        thread::spawn(move || {
            let authority = url
                .strip_prefix("http://")
                .expect("http url")
                .split('/')
                .next()
                .expect("authority");
            // A loopback preconnect that closes before sending a request line must not end
            // the one-time page.
            drop(TcpStream::connect(authority).expect("preconnect"));
            let (status, body) = http(&url, "GET", None, "");
            assert_eq!(status, 200, "the form is still served after the preconnect");
            assert!(body.contains("PRECONNECT_KEY"));
            let (status, body) = http(&url, "POST", Some("text/plain; charset=utf-8"), "preconnect-value-2468");
            assert_eq!(status, 200);
            assert!(body.contains("PRECONNECT_KEY saved"));
        })
    };
    let info = server
        .wait(Duration::from_secs(5))
        .expect("saved after a connection without headers");
    client.join().expect("client");
    assert_eq!(info.last4, "2468");
    assert_eq!(
        fs::read_to_string(secret_path(&deps, "PRECONNECT_KEY")).expect("value"),
        "preconnect-value-2468"
    );
}

#[test]
fn one_time_page_times_out_without_a_value() {
    let tmp = tempdir().expect("tempdir");
    let (deps, _) = deps(tmp.path());
    let name = vowp_protocol::SecretName::parse("TIMEOUT_KEY").expect("name");
    let server = SecretRequestServer::bind_with_token(&deps, &name, now(), "timeout-token".to_owned()).expect("bind");
    let error = server.wait(Duration::from_millis(30)).expect_err("must time out");
    assert!(error.to_string().contains("timed out"));
    assert!(!secret_path(&deps, "TIMEOUT_KEY").exists());
}
