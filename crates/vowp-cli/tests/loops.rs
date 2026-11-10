//! `vowp loops` integration tests.

mod common;

use vowp_protocol::{ActiveState, Loop, LoopBounds, LoopKind};

/// Inserts the two loop rows these tests use.
fn insert_loops(env: &common::TestEnv) {
    let store = env.store();
    let now = store.clock().now();
    for (name, kind, interval_secs) in [
        ("l1-supervisor", LoopKind::L1, 60_u64),
        ("l4-planner", LoopKind::L4, 1_800_u64),
    ] {
        store
            .upsert_loop(&Loop {
                name: name.to_owned(),
                kind,
                project: Some(env.config.project.name.clone()),
                interval_secs,
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
}

#[test]
fn loops_pause_resume_and_unknown_loop_exit_3() {
    let env = common::TestEnv::new();
    insert_loops(&env);

    let (code, value, stderr) = env.run_json(&["loops"]);
    assert_eq!(code, 0, "{stderr}");
    let rows = value.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["name"], "l4-planner");

    let (code, value, stderr) = env.run_json(&["loops", "pause", "l4-planner"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["state"], "paused");

    let (code, value, stderr) = env.run_json(&["loops", "resume", "l4-planner"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["state"], "active");

    let (code, _, stderr) = env.run(&["loops", "pause", "no-such-loop"]);
    assert_eq!(code, 3, "unknown loop must be not found: {stderr}");
    assert!(stderr.contains("no-such-loop"), "{stderr}");

    let (code, _, stderr) = env.run(&["loops", "pause", "l1-supervisor"]);
    assert_eq!(code, 2, "L1 cannot be paused: {stderr}");
}
