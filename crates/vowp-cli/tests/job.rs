//! `vowp job` integration tests.

mod common;

use vowp_protocol::{Evidence, Gate, GateResult, JobId, JobState, ParkedReason, Timestamp};
use vowp_store::StateChange;

/// Returns the indented lines of the human `--gates` section.
fn gate_lines(stdout: &str) -> Vec<&str> {
    let mut lines = stdout.lines().skip_while(|line| *line != "gates:");
    assert_eq!(lines.next(), Some("gates:"), "missing gates section: {stdout:?}");
    lines.take_while(|line| line.starts_with("  ")).collect()
}

/// Builds a stored gate result for `job` at `gate_index`.
fn gate_result(
    job: &JobId,
    attempt: u32,
    gate_index: i32,
    gate: &str,
    passed: bool,
    evidence: Evidence,
    duration_ms: u64,
) -> GateResult {
    GateResult {
        id: None,
        job_id: job.clone(),
        run_id: None,
        attempt,
        gate_index,
        gate: Gate::from_short(gate).expect("gate short form"),
        passed,
        evidence,
        output_blob: None,
        duration_ms,
        ts: Timestamp::from_unix_secs(1_700_000_000),
    }
}

#[test]
fn job_add_ls_show_roundtrip_json() {
    let env = common::TestEnv::new();
    let (code, job, stderr) = env.run_json(&[
        "job",
        "add",
        "--id",
        "n04-graph",
        "--title",
        "Graph dispatch",
        "--kind",
        "build",
        "--owns",
        "crates/vowp-graph/**",
        "--reads",
        "docs/design/**",
        "--gates",
        "report,secret_scan",
        "--priority",
        "70",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(job["id"], "n04-graph");
    assert_eq!(job["title"], "Graph dispatch");
    assert_eq!(job["kind"], "build");
    assert_eq!(job["role"], "worker");
    assert_eq!(job["state"], "pending");
    assert_eq!(job["priority"], 70);
    assert_eq!(job["gates"].as_array().expect("gates").len(), 2);

    let brief = std::fs::read_to_string(env.brief_path("n04-graph")).expect("brief written");
    assert!(brief.starts_with("---\njob: n04-graph\n"), "{brief}");
    assert!(brief.contains("owns: [\"crates/vowp-graph/**\"]"), "{brief}");
    assert!(brief.contains("\"secret_scan\""), "{brief}");

    let (code, listing, stderr) = env.run_json(&["job", "ls"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let listing = listing.as_array().expect("array");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0]["id"], "n04-graph");

    let (code, shown, stderr) = env.run_json(&["job", "show", "n04-graph"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(shown["id"], "n04-graph");
    assert_eq!(shown["reads"][0], "docs/design/**");

    let (code, _, _) = env.run(&["job", "show", "n04-graph", "--brief"]);
    assert_eq!(code, 0);
}

#[test]
fn job_add_rejects_cycle_and_unknown_need() {
    let env = common::TestEnv::new();
    let (code, _, stderr) = env.run(&["job", "add", "--id", "a", "--title", "A", "--kind", "build"]);
    assert_eq!(code, 0, "{stderr}");

    let (code, _, stderr) = env.run(&[
        "job", "add", "--id", "b", "--title", "B", "--kind", "build", "--needs", "missing",
    ]);
    assert_eq!(code, 2, "unknown need must be invalid input: {stderr}");
    assert!(stderr.contains("unknown need"), "{stderr}");

    let (code, _, stderr) = env.run(&[
        "job", "add", "--id", "c", "--title", "C", "--kind", "build", "--needs", "c",
    ]);
    assert_eq!(code, 2, "self need must be a cycle: {stderr}");
    assert!(stderr.contains("cycle"), "{stderr}");

    let (code, _, stderr) = env.run(&[
        "job", "add", "--id", "d", "--title", "D", "--kind", "build", "--needs", "a",
    ]);
    assert_eq!(code, 0, "{stderr}");

    let store = env.store();
    for id in ["b", "c"] {
        assert!(
            store
                .job(&vowp_protocol::JobId::parse(id).expect("slug"))
                .expect("lookup")
                .is_none(),
            "{id} must not be inserted"
        );
    }
}

#[test]
fn job_retry_only_from_failed_stalled_parked() {
    let env = common::TestEnv::new();
    let store = env.store();
    let failed = env.insert_job("failed-job", "Failed", vowp_protocol::JobKind::Build, &[]);
    let stalled = env.insert_job("stalled-job", "Stalled", vowp_protocol::JobKind::Build, &[]);
    let parked = env.insert_job("parked-job", "Parked", vowp_protocol::JobKind::Build, &[]);
    let ready = env.insert_job("ready-job", "Ready", vowp_protocol::JobKind::Build, &[]);

    let change = StateChange {
        note: None,
        parked_reason: None,
    };
    store
        .set_job_state(&failed.id, JobState::Failed, &change)
        .expect("failed");
    store
        .set_job_state(&stalled.id, JobState::Stalled, &change)
        .expect("stalled");
    store
        .set_job_state(
            &parked.id,
            JobState::Parked,
            &StateChange {
                note: Some("waiting".to_owned()),
                parked_reason: Some(ParkedReason::Manual),
            },
        )
        .expect("parked");
    store.set_job_state(&ready.id, JobState::Ready, &change).expect("ready");
    store
        .set_job_counters(&failed.id, Some(2), Some(1), Some(1))
        .expect("counters");

    let (code, _, stderr) = env.run(&["job", "retry", "failed-job"]);
    assert_eq!(code, 0, "{stderr}");
    let job = store.require_job(&failed.id).expect("failed job");
    assert_eq!(job.state, JobState::Ready);
    assert_eq!((job.attempts, job.continues, job.hang_restarts), (0, 0, 0));

    let (code, _, stderr) = env.run(&["job", "retry", "stalled-job"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        store.require_job(&stalled.id).expect("stalled job").state,
        JobState::Running
    );

    let (code, _, stderr) = env.run(&["job", "retry", "parked-job"]);
    assert_eq!(code, 0, "{stderr}");
    let job = store.require_job(&parked.id).expect("parked job");
    assert_eq!(job.state, JobState::Ready);
    assert_eq!(job.parked_reason, None);

    let (code, _, stderr) = env.run(&["job", "retry", "ready-job"]);
    assert_eq!(code, 4, "retry from ready must be a conflict: {stderr}");
    assert_eq!(store.require_job(&ready.id).expect("ready job").state, JobState::Ready);
}

#[test]
fn job_park_and_cancel_enqueue_request_without_supervisor_apply_directly() {
    let env = common::TestEnv::new();
    let job = env.insert_job("job-a", "Job A", vowp_protocol::JobKind::Build, &[]);
    let store = env.store();

    let (code, stdout, stderr) = env.run(&["job", "park", "job-a", "--reason", "owner asked"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("parked job job-a"), "{stdout}");
    let stored = store.require_job(&job.id).expect("parked");
    assert_eq!(stored.state, JobState::Parked);
    assert_eq!(stored.parked_reason, Some(ParkedReason::Manual));
    assert!(stored.note.contains("owner asked"), "{}", stored.note);
    assert!(
        store.take_requests().expect("requests").is_empty(),
        "no supervisor means direct application, not a queued row"
    );

    let (code, stdout, stderr) = env.run(&["job", "cancel", "job-a"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("cancelled job job-a"), "{stdout}");
    assert_eq!(
        store.require_job(&job.id).expect("cancelled").state,
        JobState::Cancelled
    );
    assert!(store.take_requests().expect("requests").is_empty());
}

#[test]
fn job_update_rejects_multi_job_cycle() {
    let env = common::TestEnv::new();
    env.insert_job("a", "A", vowp_protocol::JobKind::Build, &[]);
    env.insert_job("b", "B", vowp_protocol::JobKind::Build, &["a"]);

    let (code, _, stderr) = env.run(&["job", "update", "a", "--add-needs", "b"]);
    assert_eq!(code, 2, "a multi-job cycle must be invalid input: {stderr}");
    assert!(stderr.contains("cycle"), "{stderr}");
    let store = env.store();
    let a = store
        .require_job(&vowp_protocol::JobId::parse("a").expect("slug"))
        .expect("a");
    assert!(a.needs.is_empty());
}

#[test]
fn job_add_missing_brief_leaves_no_job() {
    let env = common::TestEnv::new();
    let missing = env.project_path().join("no-such-brief.md");

    let (code, _, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "no-brief-file",
        "--title",
        "Missing brief file",
        "--kind",
        "build",
        "--brief-file",
        missing.to_str().expect("path"),
    ]);
    assert_eq!(code, 1, "missing brief file is an io error: {stderr}");
    let store = env.store();
    assert!(
        store
            .job(&vowp_protocol::JobId::parse("no-brief-file").expect("slug"))
            .expect("lookup")
            .is_none(),
        "a failed add must not leave a job row"
    );

    let (code, _, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "no-brief-value",
        "--title",
        "Missing brief value",
        "--kind",
        "build",
        "--brief",
        missing.to_str().expect("path"),
    ]);
    assert_eq!(code, 1, "missing --brief file is an io error: {stderr}");
    assert!(
        store
            .job(&vowp_protocol::JobId::parse("no-brief-value").expect("slug"))
            .expect("lookup")
            .is_none(),
        "a failed add must not leave a job row"
    );
}

#[test]
fn job_keeper_commands_enqueue_request_rows() {
    let env = common::TestEnv::new();
    let job = env.insert_job("stalled-job", "Stalled", vowp_protocol::JobKind::Build, &[]);
    let store = env.store();
    store
        .set_job_state(
            &job.id,
            JobState::Stalled,
            &StateChange {
                note: None,
                parked_reason: None,
            },
        )
        .expect("stalled");

    let (code, _, stderr) = env.run(&["job", "nudge", "stalled-job"]);
    assert_eq!(code, 0, "{stderr}");
    let requests = store.take_requests().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].kind, "nudge_job");
    // The state only changes when the supervisor applies the request.
    assert_eq!(store.require_job(&job.id).expect("job").state, JobState::Stalled);
}

#[test]
fn job_show_gates_lists_every_configured_gate_as_not_run() {
    let env = common::TestEnv::new();
    let (code, _, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "fresh-gates",
        "--title",
        "Fresh gates",
        "--kind",
        "build",
        "--gates",
        "report,secret_scan,cargo test --workspace",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, stdout, stderr) = env.run(&["job", "show", "fresh-gates", "--gates"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        gate_lines(&stdout),
        vec![
            "  report  not run",
            "  secret_scan  not run",
            "  cargo test --workspace  not run",
        ]
    );
}

#[test]
fn job_show_gates_replaces_only_recorded_results() {
    let env = common::TestEnv::new();
    let (code, _, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "recorded-gates",
        "--title",
        "Recorded gates",
        "--kind",
        "build",
        "--gates",
        "report,secret_scan,cargo test --workspace",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let job = JobId::parse("recorded-gates").expect("job id");
    let store = env.store();
    store
        .insert_gate_result(&gate_result(
            &job,
            0,
            0,
            "report",
            false,
            Evidence::Report {
                front_matter: None,
                missing_fields: vec!["status".to_owned()],
                problems: Vec::new(),
            },
            7,
        ))
        .expect("report result");
    store
        .insert_gate_result(&gate_result(
            &job,
            0,
            2,
            "cargo test --workspace",
            true,
            Evidence::Cmd {
                command: "cargo test --workspace".to_owned(),
                exit_code: Some(0),
                timed_out: false,
                tail: "test result: ok. 12 passed".to_owned(),
                duration_ms: 42,
            },
            42,
        ))
        .expect("command result");
    // Repo-wide merge-queue results carry gate_index -1 and are not part of job.gates.
    store
        .insert_gate_result(&gate_result(
            &job,
            0,
            -1,
            "cargo test --workspace",
            true,
            Evidence::Cmd {
                command: "cargo test --workspace".to_owned(),
                exit_code: Some(0),
                timed_out: false,
                tail: "merge-queue evidence".to_owned(),
                duration_ms: 5,
            },
            5,
        ))
        .expect("merge-queue result");

    let (code, stdout, stderr) = env.run(&["job", "show", "recorded-gates", "--gates"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let lines = gate_lines(&stdout);
    assert_eq!(lines.len(), 3, "one line per configured gate: {stdout}");
    assert!(lines[0].starts_with("  report  fail  7ms  "), "{}", lines[0]);
    assert!(lines[0].contains("\"missing_fields\":[\"status\"]"), "{}", lines[0]);
    assert_eq!(lines[1], "  secret_scan  not run");
    assert!(lines[2].starts_with("  cmd  pass  42ms  "), "{}", lines[2]);
    assert!(lines[2].contains("test result: ok. 12 passed"), "{}", lines[2]);
    assert!(!stdout.contains("merge-queue evidence"), "{stdout}");
}

#[test]
fn job_show_gates_maps_duplicate_configured_gates_by_index() {
    let env = common::TestEnv::new();
    let (code, _, stderr) = env.run(&[
        "job",
        "add",
        "--id",
        "duplicate-gates",
        "--title",
        "Duplicate gates",
        "--kind",
        "build",
        "--gates",
        "report,report",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let job = JobId::parse("duplicate-gates").expect("job id");
    let store = env.store();
    store
        .insert_gate_result(&gate_result(
            &job,
            0,
            1,
            "report",
            true,
            Evidence::Report {
                front_matter: None,
                missing_fields: Vec::new(),
                problems: Vec::new(),
            },
            3,
        ))
        .expect("report result");

    let (code, stdout, stderr) = env.run(&["job", "show", "duplicate-gates", "--gates"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let lines = gate_lines(&stdout);
    assert_eq!(lines.len(), 2, "one line per configured entry: {stdout}");
    assert_eq!(lines[0], "  report  not run");
    assert!(lines[1].starts_with("  report  pass  3ms  "), "{}", lines[1]);
}
