//! Cross-adapter contract test: every real adapter emits exactly one terminal `TurnEnded`
//! on every exit path (ok, non-zero exit, killed, and a start that produces no output).
//! Spec: `docs/design/crates/vowp-adapters.md` §9 and `docs/design/adapters.md` §1.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use tempfile::{TempDir, tempdir};
use vowp_adapters::acp::{AcpAdapter, AcpConfig};
use vowp_adapters::claude_code::{ClaudeCodeAdapter, ClaudeConfig};
use vowp_adapters::cli::CliAdapter;
use vowp_adapters::opencode::{OpencodeAdapter, OpencodeConfig};
use vowp_core::deps::{AdapterRegistry, BinarySource, HARNESS_BINARY_HINT, resolve_harness_binary};
use vowp_protocol::{
    Adapter, AdapterError, AdapterRun, EventBody, HarnessConfig, HarnessEvent, HarnessKind, RunOutcome, StartInput,
    StopReason, VowpConfig,
};

/// An in-test fake harness; no real `claude`, `opencode` or cli binary is ever reached.
const FAKE_HARNESS: &str = r#"#!/bin/sh
if [ -n "$FAKE_FIXTURE" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    printf '%s\n' "$line"
    if [ -n "$FAKE_SLEEP" ]; then sleep "$FAKE_SLEEP"; fi
  done < "$FAKE_FIXTURE"
fi
if [ "$FAKE_HANG" = "1" ]; then
  while :; do sleep 1; done
fi
exit "${FAKE_EXIT:-0}"
"#;

fn write_fake(dir: &Path) -> PathBuf {
    let path = dir.join("fake-harness.sh");
    std::fs::write(&path, FAKE_HARNESS).expect("write fake harness");
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod fake harness");
    path
}

/// Returns the committed ACP fake, making it executable in case the checkout lost the bit.
fn fake_acp() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../vowp-adapters/tests/fakes/fake_acp.py");
    let mut permissions = std::fs::metadata(&path).expect("stat ACP fake").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod ACP fake");
    path
}

fn write_fixtures(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let claude = dir.join("claude.jsonl");
    std::fs::write(
        &claude,
        concat!(
            r#"{"type":"system","subtype":"init","session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99","model":"claude-sonnet-5-5","cwd":"/home/user/project"}"#,
            "\n",
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[{"type":"text","text":"ok"}]},"session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","stop_reason":"end_turn","terminal_reason":"completed","num_turns":1,"duration_ms":1000,"total_cost_usd":0.1,"usage":{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens_details":{"thinking_tokens":0}},"session_id":"607b6f5f-88b0-4133-8716-9270d9d52c99"}"#,
            "\n"
        ),
    )
    .expect("claude fixture");
    let opencode = dir.join("opencode.jsonl");
    std::fs::write(
        &opencode,
        concat!(
            r#"{"type":"step_start","timestamp":1794168421621,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_1","type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","timestamp":1794168436849,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_2","type":"text","text":"ok"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1794168436874,"sessionID":"ses_e439617b6ffepWG4UNK521b6ly","part":{"id":"prt_3","type":"step-finish","reason":"stop","cost":0.01,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}"#,
            "\n"
        ),
    )
    .expect("opencode fixture");
    let cli = dir.join("cli.txt");
    std::fs::write(&cli, "hello\nworld\n").expect("cli fixture");
    (claude, opencode, cli)
}

fn write_mapping(dir: &Path, script: &Path) -> PathBuf {
    let path = dir.join("mapping.toml");
    let text = format!(
        "name = \"fake\"\n[command]\nargv = [{}]\nprompt_via = \"arg\"\nenv = [\"FAKE_FIXTURE\", \"FAKE_EXIT\", \"FAKE_HANG\", \"FAKE_SLEEP\"]\n[output]\nformat = \"text\"\ntext_chunk_lines = 2\n",
        serde_json::to_string(&script.to_string_lossy()).expect("json string")
    );
    std::fs::write(&path, text).expect("mapping");
    path
}

fn adapters(dir: &Path) -> Vec<(&'static str, Box<dyn Adapter>, PathBuf)> {
    let script = write_fake(dir);
    let (claude_fixture, opencode_fixture, cli_fixture) = write_fixtures(dir);
    let mapping = write_mapping(dir, &script);
    let claude = ClaudeCodeAdapter::new(ClaudeConfig {
        binary: Some(script.to_string_lossy().into_owned()),
        ..ClaudeConfig::default()
    });
    let opencode = OpencodeAdapter::new(OpencodeConfig {
        binary: Some(script.to_string_lossy().into_owned()),
        ..OpencodeConfig::default()
    });
    let cli = CliAdapter::from_mapping_file(&mapping).expect("cli mapping");
    let acp = AcpAdapter::new(AcpConfig {
        binary: Some(fake_acp().to_string_lossy().into_owned()),
        ..AcpConfig::default()
    });
    vec![
        ("claude-code", Box::new(claude), claude_fixture),
        ("opencode", Box::new(opencode), opencode_fixture),
        ("cli", Box::new(cli), cli_fixture.clone()),
        ("acp", Box::new(acp), cli_fixture),
    ]
}

fn start_input(cwd: &Path, fixture: &Path, exit: &str, hang: bool) -> StartInput {
    // The ACP fake selects its scripted turn from the same scenario; the other adapters
    // ignore this variable.
    let acp_mode = if hang {
        "hang"
    } else if exit != "0" {
        "quota"
    } else if fixture.exists() {
        "ok"
    } else {
        "empty"
    };
    StartInput {
        session_id: vowp_protocol::SessionId::new(),
        executor: vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor"),
        cwd: cwd.to_path_buf(),
        prompt: "say hello".to_owned(),
        resume: None,
        new_native_id: None,
        env: vec![
            ("FAKE_FIXTURE".to_owned(), fixture.to_string_lossy().into_owned()),
            ("FAKE_EXIT".to_owned(), exit.to_owned()),
            (
                "FAKE_HANG".to_owned(),
                if hang { "1".to_owned() } else { String::new() },
            ),
            ("FAKE_SLEEP".to_owned(), "0".to_owned()),
            ("FAKE_ACP_MODE".to_owned(), acp_mode.to_owned()),
        ],
        permission_mode: None,
        system_note: None,
        event_log_hint: None,
        timeout: None,
    }
}

fn drain(run: &dyn AdapterRun) -> Vec<HarnessEvent> {
    let mut events = Vec::new();
    loop {
        match run.events().recv_timeout(Duration::from_secs(10)) {
            Ok(event) => events.push(event),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("adapter never closed its event channel"),
        }
    }
    events
}

fn assert_terminal(events: &[HarnessEvent]) {
    let terminals = events
        .iter()
        .filter(|event| matches!(event.body, EventBody::TurnEnded(_)))
        .count();
    assert_eq!(terminals, 1, "exactly one terminal TurnEnded");
    assert!(
        matches!(events.last().map(|event| &event.body), Some(EventBody::TurnEnded(_))),
        "the terminal TurnEnded is the last event"
    );
}

#[test]
fn adapters_emit_terminal_turn_ended_on_every_exit_path() {
    let tmp: TempDir = tempdir().expect("tempdir");
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let adapters = adapters(tmp.path());
    let missing = tmp.path().join("missing.jsonl");

    for (name, adapter, fixture) in &adapters {
        for (path, exit, hang, label) in [
            (fixture.as_path(), "0", false, "ok"),
            (fixture.as_path(), "1", false, "error exit"),
            (fixture.as_path(), "0", true, "killed"),
            (missing.as_path(), "0", false, "no output"),
        ] {
            let input = start_input(&cwd, path, exit, hang);
            let mut run = adapter
                .start(input)
                .unwrap_or_else(|error| panic!("{name} start: {error}"));
            if hang {
                std::thread::sleep(Duration::from_millis(200));
                run.stop().unwrap_or_else(|error| panic!("{name} stop: {error}"));
            }
            let summary = run
                .wait()
                .unwrap_or_else(|error| panic!("{name} {label} wait: {error}"));
            let events = drain(run.as_ref());
            assert_terminal(&events);
            if hang {
                assert_eq!(summary.outcome, RunOutcome::Killed, "{name} killed outcome");
                assert_eq!(
                    events.last().and_then(|event| match &event.body {
                        EventBody::TurnEnded(payload) => Some(payload.stop_reason),
                        _ => None,
                    }),
                    Some(StopReason::Interrupted),
                    "{name} killed stop reason"
                );
            }
        }
    }
}

#[test]
fn adapter_kinds_match_their_contract() {
    let tmp = tempdir().expect("tempdir");
    let adapters = adapters(tmp.path());
    let kinds: Vec<HarnessKind> = adapters.iter().map(|(_, adapter, _)| adapter.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            HarnessKind::ClaudeCode,
            HarnessKind::Opencode,
            HarnessKind::Cli,
            HarnessKind::Acp
        ]
    );
}
/// Writes an executable script named `name` under `directory` and returns its path.
fn write_named_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(directory).expect("create script directory");
    let path = directory.join(name);
    std::fs::write(&path, body).expect("write script");
    let mut permissions = std::fs::metadata(&path).expect("stat script").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod script");
    path
}

/// Builds an opencode config that names `binary` (a path or a bare name).
fn opencode_config(binary: Option<&str>) -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.harnesses.insert(
        "opencode".to_owned(),
        HarnessConfig {
            binary: binary.map(str::to_owned),
            ..HarnessConfig::default()
        },
    );
    config
}

/// Builds a registry whose built-ins resolve through `search_path`.
fn registry_with_path(config: &VowpConfig, search_path: &Path) -> AdapterRegistry {
    AdapterRegistry::from_config_with_search_path(config, Some(search_path.as_os_str().to_os_string()))
        .expect("registry builds")
}

fn opencode_executor() -> vowp_protocol::Executor {
    vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")
}

#[test]
fn configured_binary_resolution_prefers_config_then_path() {
    let tmp = tempdir().expect("tempdir");
    let config_dir = tmp.path().join("config");
    let path_dir = tmp.path().join("path");
    let configured = write_named_script(&config_dir, "fake-harness", "#!/bin/sh\nexit 0\n");
    let on_path = write_named_script(&path_dir, "fake-harness", "#!/bin/sh\nexit 0\n");
    let search = Some(path_dir.as_os_str());

    let absolute = resolve_harness_binary(
        Some(configured.to_string_lossy().as_ref()),
        "fake-harness",
        search,
        None,
        None,
    )
    .expect("the configured absolute path wins");
    assert_eq!(absolute.path, configured, "an absolute configured path wins over PATH");
    assert_eq!(absolute.source, BinarySource::Config);

    let bare = resolve_harness_binary(Some("fake-harness"), "fake-harness", search, None, None)
        .expect("a bare configured name uses PATH");
    assert_eq!(bare.path, on_path);
    assert_eq!(bare.source, BinarySource::Path);

    let missing = tmp.path().join("moved/fake-harness");
    let never_resolved = resolve_harness_binary(
        Some(missing.to_string_lossy().as_ref()),
        "fake-harness",
        search,
        None,
        None,
    );
    assert!(
        never_resolved.is_none(),
        "a configured path that never resolved is a config error, not a PATH guess"
    );

    let moved = resolve_harness_binary(
        Some(missing.to_string_lossy().as_ref()),
        "fake-harness",
        search,
        None,
        Some(missing.as_path()),
    )
    .expect("a previously resolved configured path that moved falls back to PATH");
    assert_eq!(moved.path, on_path);
    assert_eq!(moved.source, BinarySource::Path);

    let default = resolve_harness_binary(None, "fake-harness", search, None, None).expect("the default name uses PATH");
    assert_eq!(default.path, on_path);
    assert_eq!(default.source, BinarySource::Path);
}

#[test]
fn adapter_start_recovers_when_the_configured_binary_moved_to_path() {
    let tmp = tempdir().expect("tempdir");
    let stale_dir = tmp.path().join("stale");
    let path_dir = tmp.path().join("path");
    let marker = tmp.path().join("recovered");
    let stale = write_named_script(
        &stale_dir,
        "opencode",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.0.28'; exit 0; fi\nexit 0\n",
    );

    // The PATH binary runs the same JSON fixture the contract tests use.
    let (_, fixture, _) = write_fixtures(tmp.path());
    write_named_script(
        &path_dir,
        "opencode",
        &format!(
            "#!/bin/sh\nprintf ran > '{}'\ncat '{}'\nexit 0\n",
            marker.display(),
            fixture.display()
        ),
    );

    let config = opencode_config(Some(stale.to_string_lossy().as_ref()));
    let registry = registry_with_path(&config, &path_dir);
    let adapter = registry.get(&opencode_executor()).expect("opencode is registered");
    let probe = adapter.probe();
    assert!(probe.ok, "{}", probe.detail);
    assert_eq!(probe.path.as_deref(), Some(stale.to_string_lossy().as_ref()));
    std::fs::remove_file(&stale).expect("remove the probed binary");

    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut run = adapter
        .start(start_input(&cwd, &tmp.path().join("missing.jsonl"), "0", false))
        .expect("start re-resolves the moved binary from PATH");
    let summary = run.wait().expect("the recovered run finishes");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert!(marker.is_file(), "the PATH binary ran after re-resolution");
}

#[test]
fn probe_names_the_resolved_path_and_origin() {
    let tmp = tempdir().expect("tempdir");
    let config_dir = tmp.path().join("config");
    let path_dir = tmp.path().join("path");
    let configured = write_named_script(
        &config_dir,
        "opencode",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.0.28'; exit 0; fi\nexit 0\n",
    );
    write_named_script(
        &path_dir,
        "opencode",
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '9.9.9'; exit 0; fi\nexit 0\n",
    );

    let configured_config = opencode_config(Some(configured.to_string_lossy().as_ref()));
    let configured_probe = registry_with_path(&configured_config, &path_dir)
        .get(&opencode_executor())
        .expect("opencode is registered")
        .probe();
    assert!(configured_probe.ok, "{}", configured_probe.detail);
    let configured_text = configured.to_string_lossy();
    assert_eq!(
        configured_probe.path.as_deref(),
        Some(configured_text.as_ref()),
        "the probe reports the configured path"
    );
    assert!(
        configured_probe.detail.contains("resolved from config:")
            && configured_probe.detail.contains(configured_text.as_ref()),
        "vowp doctor receives the path and its origin: {}",
        configured_probe.detail
    );

    let path_probe = registry_with_path(&opencode_config(None), &path_dir)
        .get(&opencode_executor())
        .expect("opencode is registered")
        .probe();
    assert!(path_probe.ok, "{}", path_probe.detail);
    let on_path = path_dir.join("opencode");
    assert_eq!(
        path_probe.path.as_deref(),
        Some(on_path.to_string_lossy().as_ref()),
        "the probe reports the PATH path"
    );
    assert!(
        path_probe.detail.contains("resolved from PATH:") && path_probe.detail.contains("9.9.9"),
        "vowp doctor receives the version, path and origin: {}",
        path_probe.detail
    );
}

#[test]
fn adapter_retries_once_when_the_configured_binary_cannot_be_spawned() {
    let tmp = tempdir().expect("tempdir");
    let stale_dir = tmp.path().join("stale");
    let path_dir = tmp.path().join("path");
    let marker = tmp.path().join("retried");
    // The file exists and is executable, but its interpreter does not: the first spawn
    // returns ENOENT, so the adapter must fall back to a PATH lookup and retry once.
    let unspawnable = write_named_script(&stale_dir, "opencode", "#!/definitely/not/an/interpreter\n");

    let (_, fixture, _) = write_fixtures(tmp.path());
    write_named_script(
        &path_dir,
        "opencode",
        &format!(
            "#!/bin/sh\nprintf ran > '{}'\ncat '{}'\nexit 0\n",
            marker.display(),
            fixture.display()
        ),
    );

    let config = opencode_config(Some(unspawnable.to_string_lossy().as_ref()));
    let registry = registry_with_path(&config, &path_dir);
    let adapter = registry.get(&opencode_executor()).expect("opencode is registered");

    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut run = adapter
        .start(start_input(&cwd, &tmp.path().join("missing.jsonl"), "0", false))
        .expect("the locator failure is retried from PATH");
    let summary = run.wait().expect("the retried run finishes");
    assert_eq!(summary.outcome, RunOutcome::Ok);
    assert!(marker.is_file(), "the PATH binary ran after the retry");
}

#[test]
fn missing_binary_is_a_locator_error_with_the_install_hint() {
    let tmp = tempdir().expect("tempdir");
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).expect("empty path");
    let missing = tmp.path().join("moved/opencode");
    let config = opencode_config(Some(missing.to_string_lossy().as_ref()));
    let registry = registry_with_path(&config, &empty);
    let adapter = registry.get(&opencode_executor()).expect("opencode is registered");

    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let error = match adapter.start(start_input(&cwd, &tmp.path().join("missing.jsonl"), "0", false)) {
        Ok(_) => panic!("the binary is missing everywhere"),
        Err(error) => error,
    };
    match error {
        AdapterError::Unavailable(message) => {
            assert!(message.contains("harness not found"), "{message}");
            assert!(message.contains(&missing.to_string_lossy().into_owned()), "{message}");
            assert!(message.contains(HARNESS_BINARY_HINT), "{message}");
        }
        other => panic!("expected a locator error, got {other:?}"),
    }
}
