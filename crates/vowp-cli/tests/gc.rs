//! `vowp gc --older-than` acceptance tests.
//!
//! Each test uses a temporary home and project from [`common::TestEnv`]. No test reads the
//! real home, starts the supervisor, touches the network or runs a real harness.

mod common;

use std::time::{Duration, UNIX_EPOCH};

use vowp_protocol::{SessionId, SessionState, Timestamp};

/// Returns a fixed timestamp `days` days before now.
fn days_ago(days: u64) -> Timestamp {
    Timestamp::now().minus(Duration::from_secs(days * 24 * 60 * 60))
}

/// Sets a file's modification time so the run-output retention sees it as old.
fn set_mtime(path: &std::path::Path, at: Timestamp) {
    let time = UNIX_EPOCH + Duration::from_millis(at.unix_millis() as u64);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime")
        .set_modified(time)
        .expect("set mtime");
}

/// Closes a freshly created session and backdates its last update.
fn backdate_session(env: &common::TestEnv, id: &SessionId, days: u64) {
    let store = env.store();
    let mut session = store.require_session(id).expect("session");
    session.state = SessionState::Closed;
    session.updated_at = days_ago(days);
    store.upsert_session(&session).expect("backdated session");
}

#[test]
fn gc_help_lists_older_than() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["gc", "--help"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("--older-than <DURATION>"), "{stdout}");
}

#[test]
fn gc_older_than_rejects_bad_values_with_exit_two() {
    let env = common::TestEnv::new();
    for args in [
        vec!["gc", "--older-than", "0d"],
        vec!["gc", "--older-than=0s"],
        vec!["gc", "--older-than=-7d"],
        vec!["gc", "--older-than=7"],
        vec!["gc", "--older-than=7x"],
        vec!["gc", "--older-than=3651d"],
        vec!["gc", "--older-than", "7d", "--class", "cache"],
    ] {
        let (code, stdout, stderr) = env.run(&args);
        assert_eq!(code, 2, "`{}`: {stderr}", args.join(" "));
        assert!(
            stderr.contains("older-than"),
            "`{}` did not explain the flag: {stdout}{stderr}",
            args.join(" ")
        );
    }
}

#[test]
fn gc_older_than_dry_run_apply_show_and_reindex() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&[
        "session",
        "new",
        "--title",
        "gc older than",
        "--executor",
        "opencode:deepseek/deepseek-flash#max",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let session = SessionId::parse(stdout.trim()).expect("session id");
    backdate_session(&env, &session, 10);
    let log = env
        .home_path()
        .join("sessions")
        .join(session.as_str())
        .join("log.jsonl");
    assert!(log.is_file(), "session log missing");

    let runs = env.home_path().join("runs");
    std::fs::create_dir_all(&runs).expect("runs dir");
    let run = runs.join("old.jsonl");
    std::fs::write(&run, b"old raw output\n").expect("run output");
    set_mtime(&run, days_ago(10));

    let (code, value, stderr) = env.run_json(&["gc", "--older-than", "7d"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["dry_run"], true, "{value}");
    assert!(value["refused"].is_array(), "{value}");
    let classes = value["classes"].as_array().expect("classes");
    let closed = classes
        .iter()
        .find(|class| class["class"] == "closed_session")
        .unwrap_or_else(|| panic!("closed_session missing: {value}"));
    assert_eq!(closed["eligible_files"], 1, "{closed}");
    let output = classes
        .iter()
        .find(|class| class["class"] == "run_output")
        .unwrap_or_else(|| panic!("run_output missing: {value}"));
    assert_eq!(output["eligible_files"], 1, "{output}");
    assert!(log.exists(), "dry run deleted the session log");
    assert!(run.exists(), "dry run deleted the run output");

    let (code, stdout, stderr) = env.run(&["gc", "--older-than", "7d", "--apply"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("applied:"), "{stdout}");
    assert!(stdout.contains("compacted 1 sessions"), "{stdout}");
    assert!(!log.exists(), "apply did not compact the old session");
    assert!(!run.exists(), "apply did not delete the old run output");

    let (code, stdout, stderr) = env.run(&["session", "show", session.as_str()]);
    assert_eq!(code, 0, "session show failed after gc: {stderr}\n{stdout}");
    assert!(stdout.contains(&format!("id: {session}")), "{stdout}");

    let (code, stdout, stderr) = env.run(&["reindex"]);
    assert_eq!(code, 0, "reindex failed after gc: {stderr}");
    assert!(stdout.contains("reindexed"), "{stdout}");

    let (code, stdout, stderr) = env.run(&["session", "show", session.as_str()]);
    assert_eq!(code, 0, "session show failed after reindex: {stderr}\n{stdout}");
    assert!(stdout.contains(&format!("id: {session}")), "{stdout}");
}
