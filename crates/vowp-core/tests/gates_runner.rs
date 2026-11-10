//! `evaluate_all` ordering/persistence and merge-check tests.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::fs;
use std::path::Path;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{CmdGate, Evidence, Gate, JobKind, ReportGate, ReviewGate, VowpConfig};

/// Builds dependencies and a job with the given gates.
fn setup(
    tmp: &Path,
    id: &str,
    gates: Vec<Gate>,
) -> (
    vowp_core::Deps,
    vowp_protocol::Job,
    vowp_store::Store,
    std::path::PathBuf,
) {
    let node = tmp.join("node");
    let wt = worktree(tmp);
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, id, JobKind::Build, &["src/**"], gates);
    let job = set_workspace(&store, &job, &format!("job/{id}"), Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store, wt)
}

/// Builds a simple command gate.
fn command(run: &str) -> Gate {
    Gate::Cmd(CmdGate {
        run: run.to_owned(),
        ..CmdGate::default()
    })
}

#[test]
fn evaluate_all_continues_after_failure_except_report_and_persists_results() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(
        tmp.path(),
        "all-continue",
        vec![
            command("exit 1"),
            command("printf 'second ran\\n'"),
            command("printf 'third ran\\n'; exit 2"),
            Gate::Review(ReviewGate::default()),
        ],
    );
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);

    let outcome = runner.evaluate_all(&context).unwrap();

    assert_eq!(outcome.results.len(), 3, "review is left for L3");
    assert_eq!(outcome.results[0].gate_index, 0);
    assert!(!outcome.results[0].passed);
    assert_eq!(outcome.results[1].gate_index, 1);
    assert!(outcome.results[1].passed, "evaluation continues after a failure");
    assert_eq!(outcome.results[2].gate_index, 2);
    assert!(!outcome.results[2].passed);
    assert_eq!(outcome.first_failure, Some(0));
    assert!(!outcome.all_passed_before_review);
    assert!(outcome.needs_review);

    let persisted = store.gate_results_for(&job.id, 0).unwrap();
    assert_eq!(persisted.len(), 3, "every evaluated gate is persisted");
    let indices: Vec<i32> = persisted.iter().map(|result| result.gate_index).collect();
    assert_eq!(indices, vec![0, 1, 2]);
    assert!(persisted.iter().all(|result| result.id.is_some()));

    let marker = tmp.path().join("short-circuit-ran");
    let (deps, job, store, wt) = setup(
        tmp.path().join("report").as_path(),
        "all-report-stop",
        vec![
            Gate::Report(ReportGate::default()),
            command(&format!("touch '{}'", marker.display())),
        ],
    );
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let outcome = runner.evaluate_all(&context).unwrap();

    assert_eq!(outcome.results.len(), 1, "a failed report gate stops the rest");
    assert!(!outcome.results[0].passed);
    assert_eq!(outcome.first_failure, Some(0));
    assert!(!outcome.needs_review);
    assert!(
        fs::metadata(&marker).is_err(),
        "the gate after a failed report must not run"
    );
    assert_eq!(store.gate_results_for(&job.id, 0).unwrap().len(), 1);
}

#[test]
fn evaluate_merge_checks_uses_project_check_cmds() {
    let tmp = tempdir().unwrap();
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.check_cmds = vec![
        "printf 'project check one\\n'".to_owned(),
        "printf 'project check two\\n'".to_owned(),
    ];
    config.merge.check_cmds.clear();
    config.merge.secret_scan = false;
    config.merge.timeout_secs = 30;

    let node = tmp.path().join("node");
    let wt = worktree(tmp.path());
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "merge-checks", JobKind::Build, &[], Vec::new());
    let job = set_workspace(&store, &job, "job/merge-checks", Some(&wt));
    let expected_commands = config.project.check_cmds.clone();
    let deps = test_deps(&node, config);
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);

    let results = runner.evaluate_merge_checks(&context).unwrap();

    assert_eq!(results.len(), 2);
    let commands: Vec<String> = results
        .iter()
        .map(|result| match &result.evidence {
            Evidence::Cmd { command, .. } => command.clone(),
            other => panic!("expected cmd evidence, got {other:?}"),
        })
        .collect();
    assert_eq!(commands, expected_commands);
    assert!(results.iter().all(|result| result.passed));
    assert!(results.iter().all(|result| result.gate_index == -1));

    let persisted = store.latest_gate_results(&job.id).unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(persisted.iter().all(|result| result.gate_index == -1));
}
