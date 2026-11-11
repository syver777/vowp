//! `vowp board` integration tests, including the one-frame golden.

mod common;

use tempfile::TempDir;
use vowp_protocol::{JobId, JobKind, JobState, NewJob, Project, ProjectName, Role, Timestamp, VowpConfig};
use vowp_store::{NodePaths, StateChange, Store};

#[test]
fn board_once_golden() {
    let env = common::TestEnv::new();
    let alpha = env.insert_job("n01-alpha", "Alpha", JobKind::Build, &[]);
    env.insert_job("n02-beta", "Beta", JobKind::Build, &["n01-alpha"]);
    env.store()
        .set_job_state(
            &alpha.id,
            JobState::Running,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("running");

    let (code, stdout, stderr) = env.run(&["board", "--once"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let mut lines = stdout.lines();
    let header = lines.next().expect("header line");
    let stamp = header.strip_prefix("test @ ").expect("project and timestamp");
    assert!(
        Timestamp::parse(stamp).is_ok(),
        "header must end in an RFC 3339 timestamp: {header:?}"
    );

    let lines: Vec<&str> = lines.collect();
    assert_eq!(lines.len(), 10, "board golden changed: {stdout}");
    assert_eq!(lines[0], "jobs: pending 1, running 1");
    assert_eq!(lines[1], "running:");
    assert!(lines[2].starts_with("  n01-alpha  -  "), "{}", lines[2]);
    assert!(lines[2].ends_with("s  -"), "{}", lines[2]);
    assert_eq!(lines[3], "slots: 0/6");
    assert_eq!(lines[4], "planner: opencode:deepseek/deepseek-flash");
    assert_eq!(lines[5], "keeper: opencode:deepseek/deepseek-flash");
    assert_eq!(lines[6], "limits: none");
    assert_eq!(lines[7], "cost today: ? (unknown)");
    assert_eq!(lines[8], "inbox: 0 open");
    assert_eq!(lines[9], "checkpoint: -");
}

#[test]
fn board_json_carries_counts_and_slots() {
    let env = common::TestEnv::new();
    env.insert_job("n01-alpha", "Alpha", JobKind::Build, &[]);
    let pending = env.insert_job("n02-beta", "Beta", JobKind::Build, &[]);
    env.store()
        .set_job_state(
            &JobId::parse("n02-beta").expect("slug"),
            JobState::Ready,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("ready");

    let (code, value, stderr) = env.run_json(&["board", "--once"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["project"], "test");
    assert_eq!(value["jobs"]["pending"], 1);
    assert_eq!(value["jobs"]["ready"], 1);
    assert_eq!(value["slots_busy"], 0);
    assert_eq!(value["slots_total"], 6);
    assert_eq!(value["planner"], "opencode:deepseek/deepseek-flash");
    assert_eq!(value["keeper"], "opencode:deepseek/deepseek-flash");
    assert_eq!(value["limits"], serde_json::json!([]));
    assert_eq!(value["inbox_open"], 0);
    assert_eq!(value["running"], serde_json::json!([]));
    assert!(value["last_checkpoint"].is_null());
    assert_eq!(pending.state, JobState::Pending);
}

/// Registers and populates a second project in the same temporary home.
fn add_project_with_job(env: &common::TestEnv, name: &str, job_id: &str) -> (TempDir, VowpConfig) {
    let dir = tempfile::tempdir().expect("second project dir");
    let mut config = env.config.clone();
    config.project.name = ProjectName::parse(name).expect("project name");
    config.project.path = Some(dir.path().display().to_string());
    std::fs::write(
        dir.path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config"),
    )
    .expect("write vowp.toml");
    let paths = NodePaths::new(env.home_path());
    let store = Store::open(&paths).expect("store");
    let now = store.clock().now();
    store
        .upsert_project(&Project {
            name: config.project.name.clone(),
            path: dir.path().display().to_string(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).expect("config json"),
            created_at: now,
            updated_at: now,
        })
        .expect("project row");
    let project_store = Store::open_with_project(&paths, &config.project.name).expect("project store");
    project_store
        .insert_job(
            &config.project.name,
            &NewJob {
                id: JobId::parse(job_id).expect("job id"),
                title: format!("job {job_id}"),
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
        .expect("insert job");
    (dir, config)
}

#[test]
fn board_json_compat_with_one_project() {
    let env = common::TestEnv::new();
    env.insert_job("n01-alpha", "Alpha", JobKind::Build, &[]);

    let (code, value, stderr) = env.run_json(&["board", "--once"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let keys: Vec<&str> = value
        .as_object()
        .expect("board object")
        .keys()
        .map(String::as_str)
        .collect();
    for key in [
        "at",
        "project",
        "jobs",
        "running",
        "slots_busy",
        "slots_total",
        "planner",
        "keeper",
        "limits",
        "cost_today_usd",
        "inbox_open",
        "last_checkpoint",
    ] {
        assert!(keys.contains(&key), "one-project board lost v0.1 key {key}: {value}");
    }
}

#[test]
fn board_shows_every_project() {
    let env = common::TestEnv::new();
    env.insert_job("n01-alpha", "Alpha", JobKind::Build, &[]);
    let (dir, config) = add_project_with_job(&env, "beta", "b01-pending");

    let (code, stdout, stderr) = env.run(&["board", "--once"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("projects: 2"), "{stdout}");
    assert!(stdout.contains("test"), "{stdout}");
    assert!(stdout.contains("beta"), "{stdout}");
    assert!(
        stdout.contains("b01-pending"),
        "the other project's queue is shown:\n{stdout}"
    );

    let (code, value, stderr) = env.run_json(&["board", "--once"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["home"], "local");
    let projects = value["projects"].as_array().expect("projects");
    assert_eq!(projects.len(), 2, "{value}");
    assert!(projects.iter().any(|project| project["name"] == "beta"));
    drop((dir, config));
}
