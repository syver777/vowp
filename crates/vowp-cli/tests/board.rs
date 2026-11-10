//! `vowp board` integration tests, including the one-frame golden.

mod common;

use vowp_protocol::{JobId, JobKind, JobState, Timestamp};
use vowp_store::StateChange;

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
