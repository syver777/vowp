//! `vowp doctor` schedule validation tests.
//!
//! Every test gets a temporary node home and project from [`common::TestEnv`] and runs the real
//! binary with an isolated `PATH`, so no real harness, `git`, `tmux`, `df` or node home is used.

mod common;

use std::path::Path;
use std::process::Output;

use vowp_protocol::{ScheduleAction, ScheduleConfig};

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
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("fake bin");
    for (name, body) in FAKE_TOOLS {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("fake script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    dir
}

/// Rewrites the temp project's `vowp.toml` with `schedules`.
fn write_schedules(env: &common::TestEnv, schedules: Vec<ScheduleConfig>) {
    let mut config = env.config.clone();
    config.schedules = schedules;
    let text = toml::to_string_pretty(&config).expect("config serialises");
    std::fs::write(env.project_path().join("vowp.toml"), text).expect("write vowp.toml");
}

/// Runs `vowp --json doctor` with `bin` as the whole `PATH` and `HOME` in the temp node home.
fn run_doctor(env: &common::TestEnv, bin: &Path) -> Output {
    env.command(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(env.project_path())
        .env("PATH", bin)
        .env_remove("NO_COLOR")
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
