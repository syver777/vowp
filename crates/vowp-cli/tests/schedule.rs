//! `vowp schedule` integration tests.

mod common;

use vowp_protocol::{ActiveState, Loop, LoopBounds, LoopKind};

/// Inserts the planner loop used by the loop-action schedule.
fn insert_planner(env: &common::TestEnv) {
    let store = env.store();
    let now = store.clock().now();
    store
        .upsert_loop(&Loop {
            name: "l4-planner".to_owned(),
            kind: LoopKind::L4,
            project: Some(env.config.project.name.clone()),
            interval_secs: 1_800,
            state: ActiveState::Active,
            bounds: LoopBounds::default(),
            last_run_at: None,
            next_run_at: None,
            last_status: String::new(),
            fail_streak: 0,
            updated_at: now,
        })
        .expect("loop row");
}

#[test]
fn schedule_add_cron_prints_next_run() {
    let env = common::TestEnv::new();
    insert_planner(&env);

    let (code, stdout, stderr) = env.run(&[
        "schedule",
        "add",
        "0 7 * * 1-5",
        "loop",
        "l4-planner",
        "--name",
        "morning-report",
        "--tz",
        "+02:00",
        "--missed",
        "run_once",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("added schedule morning-report"), "{stdout}");
    assert!(stdout.contains("next "), "{stdout}");

    let store = env.store();
    let schedule = store.schedule("morning-report").expect("lookup").expect("schedule");
    assert_eq!(schedule.spec, "0 7 * * 1-5");
    assert_eq!(schedule.tz, "+02:00");
    assert!(schedule.next_run_at.is_some());

    let (code, value, stderr) = env.run_json(&["schedule", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let rows = value.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "morning-report");
    assert_eq!(rows[0]["spec_kind"], "cron");

    let (code, _, stderr) = env.run(&["schedule", "pause", "morning-report"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        store.schedule("morning-report").expect("lookup").expect("row").state,
        ActiveState::Paused
    );
    let (code, _, stderr) = env.run(&["schedule", "resume", "morning-report"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run(&["schedule", "rm", "morning-report"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(store.schedule("morning-report").expect("lookup").is_none());
}

#[test]
fn schedule_add_rejects_bad_cron_exit_2() {
    let env = common::TestEnv::new();
    insert_planner(&env);
    let (code, _, stderr) = env.run(&[
        "schedule",
        "add",
        "every 10s",
        "loop",
        "l4-planner",
        "--name",
        "too-fast",
    ]);
    assert_eq!(code, 2, "an interval under 60 s is invalid input: {stderr}");

    let (code, _, stderr) = env.run(&[
        "schedule",
        "add",
        "not a cron",
        "loop",
        "l4-planner",
        "--name",
        "bad-cron",
    ]);
    assert_eq!(code, 2, "a bad cron is invalid input: {stderr}");
    assert!(!stderr.is_empty());
    let store = env.store();
    assert!(store.schedule("bad-cron").expect("lookup").is_none());
    assert!(store.schedule("too-fast").expect("lookup").is_none());
}

#[test]
fn schedule_add_command_and_interval() {
    let env = common::TestEnv::new();
    let (code, value, stderr) = env.run_json(&[
        "schedule",
        "add",
        "every 30m",
        "command",
        "--name",
        "heartbeat",
        "--",
        "vowp",
        "status",
        "--check",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["spec_kind"], "interval");
    assert_eq!(value["action"]["kind"], "command");
    assert_eq!(value["action"]["argv"][0], "vowp");
    assert!(value["next_run_at"].is_string());
}
