//! `vowp doctor` schedule validation tests.
//!
//! Every test gets a temporary node home and project from [`common::TestEnv`] and runs the real
//! binary with an isolated `PATH`, so no real harness, `git`, `tmux`, `df` or node home is used.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use vowp_protocol::Timestamp;
use vowp_protocol::{
    Executor, HarnessConfig, OnCreditsExhausted, ProviderConfig, ScheduleAction, ScheduleConfig, VowpConfig,
};

/// Provider key variables the tests remove so the runner's environment cannot leak in.
const PROVIDER_KEY_VARIABLES: &[&str] = &["CLAUDE_API_KEY", "DEEPSEEK_API_KEY", "OPENROUTER_API_KEY"];

/// Fake tools doctor invokes; both harnesses answer `--version` successfully.
const FAKE_TOOLS: &[(&str, &str)] = &[
    ("git", "#!/bin/sh\necho 'git version 9.9.9'\nexit 0\n"),
    ("tmux", "#!/bin/sh\necho 'tmux 9.9'\nexit 0\n"),
    (
        "df",
        "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho '/dev/fake 9999999 1 8000000 1% /'\nexit 0\n",
    ),
    (
        "claude",
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\nexit 0\n",
    ),
    (
        "opencode",
        "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo 'opencode 2.0.0'; exit 0; fi\nexit 0\n",
    ),
];

/// Creates an executable fake `PATH` directory from `FAKE_TOOLS`.
fn fake_bin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("fake bin");
    for (name, body) in FAKE_TOOLS {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("fake script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    dir
}

/// Writes `config` to the temp project's `vowp.toml`.
fn write_config(env: &common::TestEnv, config: &VowpConfig) {
    let text = toml::to_string_pretty(config).expect("config serialises");
    std::fs::write(env.project_path().join("vowp.toml"), text).expect("write vowp.toml");
}

/// Rewrites the temp project's `vowp.toml` with `schedules`.
fn write_schedules(env: &common::TestEnv, schedules: Vec<ScheduleConfig>) {
    let mut config = env.config.clone();
    config.schedules = schedules;
    write_config(env, &config);
}

/// Replaces every required profile's ladder with `ladders`.
fn write_ladders(env: &common::TestEnv, ladders: &[&str]) {
    let mut config = env.config.clone();
    let ladder: Vec<Executor> = ladders
        .iter()
        .map(|value| value.parse().expect("executor parses"))
        .collect();
    for profile in config.profiles.values_mut() {
        profile.ladder = ladder.clone();
    }
    write_config(env, &config);
}

/// Runs `vowp --json doctor` with `bin` as the whole `PATH` and `HOME` in the temp node home.
///
/// Provider key variables are removed from the child so the test never depends on the
/// runner's environment.
fn run_doctor(env: &common::TestEnv, bin: &Path) -> Output {
    let mut command = env.command(env!("CARGO_BIN_EXE_vowp"));
    command
        .current_dir(env.project_path())
        .env("PATH", bin)
        .env_remove("NO_COLOR");
    for variable in PROVIDER_KEY_VARIABLES {
        command.env_remove(variable);
    }
    command
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg(env.project_path())
        .args(["--json", "doctor"])
        .output()
        .expect("run vowp doctor")
}

/// Returns one named check from a doctor JSON report.
fn find_check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing check {name}: {report}"))
}

/// A loop action is enough for the parser; doctor never executes schedule actions.
fn loop_action() -> ScheduleAction {
    ScheduleAction::Loop {
        loop_name: "l4-planner".to_owned(),
    }
}

#[test]
fn doctor_flags_invalid_cron_schedule() {
    let env = common::TestEnv::new();
    write_schedules(
        &env,
        vec![ScheduleConfig {
            name: "bad-minute".to_owned(),
            cron: Some("61 9 * * *".to_owned()),
            every: None,
            tz: Some("UTC".to_owned()),
            missed: None,
            action: loop_action(),
        }],
    );
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor must fail; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(
        find_check(&value, "config")["status"],
        "ok",
        "VowpConfig::validate only checks the cron shape"
    );

    let schedules = find_check(&value, "schedules");
    assert_eq!(schedules["status"], "fail", "{schedules}");
    let detail = schedules["detail"].as_str().expect("detail");
    assert!(detail.contains("bad-minute"), "missing schedule name: {detail}");
    assert!(detail.contains("61 9 * * *"), "missing spec: {detail}");
    assert!(detail.contains("invalid cron"), "missing parser error: {detail}");
}

#[test]
fn doctor_fails_when_snapshot_tmp_is_not_a_writable_directory() {
    let env = common::TestEnv::new();
    let tmp = env.home_path().join("tmp");
    std::fs::remove_dir_all(&tmp).expect("remove tmp dir");
    std::fs::write(&tmp, b"not a directory").expect("replace tmp with a file");

    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());
    assert_eq!(
        output.status.code(),
        Some(1),
        "doctor must fail; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    let snapshot = find_check(&value, "snapshot");
    assert_eq!(snapshot["status"], "fail", "{snapshot}");
    let detail = snapshot["detail"].as_str().expect("detail");
    assert!(detail.contains("tmp"), "the check names tmp/: {detail}");
}

#[test]
fn doctor_accepts_valid_schedules() {
    let env = common::TestEnv::new();
    write_schedules(
        &env,
        vec![
            ScheduleConfig {
                name: "morning-report".to_owned(),
                cron: Some("0 7 * * 1-5".to_owned()),
                every: None,
                tz: Some("+02:00".to_owned()),
                missed: None,
                action: loop_action(),
            },
            ScheduleConfig {
                name: "heartbeat".to_owned(),
                cron: None,
                every: Some("30m".to_owned()),
                tz: None,
                missed: None,
                action: loop_action(),
            },
        ],
    );
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must pass; stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    assert_eq!(value["ok"], true, "{value}");
    let schedules = find_check(&value, "schedules");
    assert_eq!(schedules["status"], "ok", "{schedules}");
    assert_eq!(schedules["detail"], "2 schedules parse", "{schedules}");
}

#[test]
fn doctor_probes_a_configured_acp_harness() {
    let env = common::TestEnv::new();
    let binary = env.project_path().join("fake-acp");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write fake ACP server");
    let mut permissions = std::fs::metadata(&binary).expect("stat fake ACP server").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("chmod fake ACP server");

    let mut config = env.config.clone();
    config.harnesses.insert(
        "acp".to_owned(),
        HarnessConfig {
            binary: Some(binary.to_string_lossy().into_owned()),
            ..HarnessConfig::default()
        },
    );
    let acp: Executor = "acp:claude-code-acp".parse().expect("ACP executor parses");
    for profile in config.profiles.values_mut() {
        profile.ladder = vec![acp.clone()];
    }
    write_config(&env, &config);

    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());
    assert_eq!(
        output.status.code(),
        Some(0),
        "a configured ACP server must not fail doctor; stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    assert_eq!(value["ok"], true, "{value}");
    let harness = find_check(&value, "harness:acp");
    assert_eq!(harness["status"], "ok", "{harness}");
    let detail = harness["detail"].as_str().expect("detail");
    assert!(detail.contains("fake-acp"), "the configured binary is probed: {detail}");
    let profiles = find_check(&value, "profiles");
    assert_eq!(profiles["status"], "ok", "{profiles}");
}

#[test]
fn doctor_accepts_claude_code_login_without_api_key() {
    let env = common::TestEnv::new();
    write_ladders(&env, &["claude-code:claude-sonnet-5-5@medium"]);
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must pass; stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    let providers = find_check(&value, "providers");
    assert_eq!(providers["status"], "ok", "{providers}");
    let detail = providers["detail"].as_str().expect("detail");
    assert!(
        detail.contains("claude-code uses its own login"),
        "missing harness login detail: {detail}"
    );
    assert!(
        !detail.contains("CLAUDE_API_KEY"),
        "a harness-owned provider must not ask for a key: {detail}"
    );
}

#[test]
fn doctor_warns_when_opencode_provider_key_is_absent() {
    let env = common::TestEnv::new();
    write_ladders(&env, &["opencode:deepseek/deepseek-flash"]);
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    assert_eq!(
        output.status.code(),
        Some(0),
        "a warning is not a failure; stderr: {}; stdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    let providers = find_check(&value, "providers");
    assert_eq!(providers["status"], "warn", "{providers}");
    let detail = providers["detail"].as_str().expect("detail");
    assert!(detail.contains("DEEPSEEK_API_KEY"), "missing variable name: {detail}");
    assert!(detail.contains("absent"), "missing presence detail: {detail}");
}

#[test]
fn doctor_keeps_mixed_harness_provider_key_driven() {
    let env = common::TestEnv::new();
    write_ladders(
        &env,
        &["claude-code:claude-sonnet-5-5@medium", "opencode:claude/sonnet-5-5"],
    );
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    let providers = find_check(&value, "providers");
    assert_eq!(providers["status"], "warn", "{providers}");
    let detail = providers["detail"].as_str().expect("detail");
    assert!(
        detail.contains("CLAUDE_API_KEY"),
        "mixed rungs stay key-driven: {detail}"
    );
    assert!(
        !detail.contains("uses its own login"),
        "mixed rungs are not harness-owned: {detail}"
    );
}

#[test]
fn doctor_treats_configured_provider_without_rung_as_key_driven() {
    let env = common::TestEnv::new();
    let mut config = env.config.clone();
    config.providers.insert(
        "openrouter".to_owned(),
        ProviderConfig {
            max_concurrent: 4,
            on_credits_exhausted: OnCreditsExhausted::Park,
            prices: BTreeMap::new(),
        },
    );
    write_config(&env, &config);
    let bin = fake_bin();
    let output = run_doctor(&env, bin.path());

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    let providers = find_check(&value, "providers");
    assert_eq!(providers["status"], "warn", "{providers}");
    let detail = providers["detail"].as_str().expect("detail");
    assert!(
        detail.contains("OPENROUTER_API_KEY"),
        "configured key has no rung: {detail}"
    );
    assert!(detail.contains("absent"), "missing presence detail: {detail}");
}

/// Creates a private directory.
#[cfg(target_os = "linux")]
fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("directory");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("directory mode");
}

/// Writes a private file.
#[cfg(target_os = "linux")]
fn private_file(path: &Path, text: &str) {
    std::fs::write(path, text).expect("file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("file mode");
}

#[cfg(target_os = "linux")]
#[test]
fn agent_doctor_reports_warnings_by_name_only() {
    let env = common::TestEnv::new();
    let state = env.home_path().join(".vowp-agent");
    private_dir(&state);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).expect("loose mode");

    private_dir(&state.join(".ssh"));
    private_file(&state.join(".ssh/id_ed25519"), "COPIED-LOGIN-PLACEHOLDER");
    private_file(&state.join(".env"), "TOKEN=COPIED-ENV-SECRET");
    private_file(&state.join("run"), "");

    let needs_pair = state.join("surfaces/01ARZ3NDEKTSV4RRFFQ69G5FAW");
    private_dir(&needs_pair);
    private_file(&needs_pair.join("state"), "needs_pair");

    let expired = state.join("surfaces/01ARZ3NDEKTSV4RRFFQ69G5FAX");
    private_dir(&expired);
    private_file(&expired.join("token"), "dead-token-placeholder");
    private_file(&expired.join("expires_at"), "2020-01-01T00:00:00Z");
    private_file(&expired.join("state"), "paired");

    let unit = env.home_path().join(".config/systemd/user/vowp-agent.service");
    std::fs::create_dir_all(unit.parent().expect("unit parent")).expect("unit parent");
    std::fs::write(&unit, "# generated by vowp agent service install\n[Service]\n").expect("unit");

    let tools = tempfile::tempdir().expect("fake tools");
    let systemctl = tools.path().join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nexit 1\n",
    )
    .expect("fake systemctl");
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", tools.path())
        .env("XDG_CONFIG_HOME", env.home_path().join(".config"))
        .env_remove("VOWP_AGENT_HOME")
        .args(["--json", "agent", "doctor"])
        .output()
        .expect("run agent doctor");
    assert_eq!(
        output.status.code(),
        Some(0),
        "agent doctor warnings are not failures; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("agent doctor JSON");
    assert_eq!(value["ok"], true, "{value}");

    let state_dir = find_check(&value, "state_dir");
    assert_eq!(state_dir["status"], "warn", "{state_dir}");
    let detail = state_dir["detail"].as_str().expect("detail");
    assert!(detail.contains("mode 755"), "{detail}");

    let service = find_check(&value, "service");
    assert_eq!(service["status"], "warn", "{service}");
    let detail = service["detail"].as_str().expect("detail");
    assert!(detail.contains("not loaded"), "{detail}");
    assert!(detail.contains("vowp-agent.service"), "{detail}");
    assert!(detail.contains("leftover run marker"), "{detail}");

    let surfaces = find_check(&value, "surfaces");
    assert_eq!(surfaces["status"], "warn", "{surfaces}");
    let detail = surfaces["detail"].as_str().expect("detail");
    assert!(detail.contains("01ARZ3NDEKTSV4RRFFQ69G5FAW"), "{detail}");
    assert!(detail.contains("needs pairing"), "{detail}");
    assert!(detail.contains("01ARZ3NDEKTSV4RRFFQ69G5FAX"), "{detail}");
    assert!(detail.contains("expired"), "{detail}");

    let logins = find_check(&value, "logins");
    assert_eq!(logins["status"], "warn", "{logins}");
    let detail = logins["detail"].as_str().expect("detail");
    assert!(detail.contains(".ssh"), "{detail}");
    assert!(detail.contains(".env"), "{detail}");

    let home = env.home_path().display().to_string();
    for (label, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        for secret in [
            "COPIED-LOGIN-PLACEHOLDER",
            "COPIED-ENV-SECRET",
            "dead-token-placeholder",
            home.as_str(),
        ] {
            assert!(!text.contains(secret), "{label} must not carry {secret}: {text}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn agent_doctor_warns_when_a_surface_token_expires_within_seven_days() {
    let env = common::TestEnv::new();
    let state = env.home_path().join(".vowp-agent");
    private_dir(&state);
    let surfaces_dir = state.join("surfaces");
    private_dir(&surfaces_dir);
    let surface = surfaces_dir.join("01ARZ3NDEKTSV4RRFFQ69G5FAZ");
    private_dir(&surface);
    private_file(&surface.join("token"), "expiring-token-placeholder");
    let expiry = Timestamp::now()
        .plus(Duration::from_secs(3 * 24 * 60 * 60))
        .as_rfc3339();
    private_file(&surface.join("expires_at"), &expiry);
    private_file(&surface.join("state"), "paired");

    let tools = tempfile::tempdir().expect("fake tools");
    let systemctl = tools.path().join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\nexit 1\n",
    )
    .expect("fake systemctl");
    std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .env("PATH", tools.path())
        .env("XDG_CONFIG_HOME", env.home_path().join(".config"))
        .env_remove("VOWP_AGENT_HOME")
        .args(["--json", "agent", "doctor"])
        .output()
        .expect("run agent doctor");
    assert_eq!(
        output.status.code(),
        Some(0),
        "agent doctor reports warnings without failing; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("agent doctor JSON");
    let surfaces = find_check(&value, "surfaces");
    assert_eq!(surfaces["status"], "warn", "{surfaces}");
    let detail = surfaces["detail"].as_str().expect("detail");
    assert!(detail.contains("01ARZ3NDEKTSV4RRFFQ69G5FAZ"), "{detail}");
    assert!(detail.contains("within 7 days"), "{detail}");
    assert!(detail.contains("expires"), "{detail}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("expiring-token-placeholder"),
        "doctor never prints a token: {stdout}"
    );
}
