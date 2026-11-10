//! `report` gate and anti-pattern validation tests.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::path::Path;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree, write};
use tempfile::tempdir;
use vowp_core::CoreError;
use vowp_core::gates::{GateRunner, validate_gate};
use vowp_protocol::{CmdGate, DeployGate, Evidence, Gate, JobKind, ReportGate, VowpConfig};

/// Creates a store, job and dependencies for one report test.
fn setup(
    tmp: &Path,
    id: &str,
    gate: Gate,
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
    let job = insert_job(&store, &project, id, JobKind::Build, &["src/**"], vec![gate]);
    let job = set_workspace(&store, &job, &format!("job/{id}"), Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store, wt)
}

#[test]
fn report_gate_passes_with_required_fields() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path(), "report-pass", Gate::Report(ReportGate::default()));
    write(
        &wt.join(&job.report),
        &format!(
            "---\njob: report-pass\nstatus: done\nbranch: {}\ncommits: []\ntests: \"cargo test -p vowp-core: ok\"\n---\nall good\n",
            job.branch.as_deref().unwrap()
        ),
    );

    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner
        .evaluate(&context, 0, &Gate::Report(ReportGate::default()))
        .unwrap();

    assert!(result.passed, "evidence: {:?}", result.evidence);
    match &result.evidence {
        Evidence::Report {
            front_matter,
            missing_fields,
            problems,
        } => {
            assert!(front_matter.is_some());
            assert!(missing_fields.is_empty());
            assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        }
        other => panic!("expected report evidence, got {other:?}"),
    }
}

#[test]
fn report_gate_fails_missing_field_lists_names() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path(), "report-missing", Gate::Report(ReportGate::default()));
    write(
        &wt.join(&job.report),
        &format!(
            "---\njob: report-missing\nstatus: done\nbranch: {}\n---\nbody\n",
            job.branch.as_deref().unwrap()
        ),
    );

    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner
        .evaluate(&context, 0, &Gate::Report(ReportGate::default()))
        .unwrap();

    assert!(!result.passed);
    match &result.evidence {
        Evidence::Report { missing_fields, .. } => {
            assert_eq!(missing_fields, &["commits".to_owned(), "tests".to_owned()]);
        }
        other => panic!("expected report evidence, got {other:?}"),
    }
}

#[test]
fn report_gate_missing_file_fails_with_evidence() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path(), "report-absent", Gate::Report(ReportGate::default()));

    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner
        .evaluate(&context, 0, &Gate::Report(ReportGate::default()))
        .unwrap();

    assert!(!result.passed);
    match &result.evidence {
        Evidence::Report {
            front_matter,
            missing_fields,
            problems,
        } => {
            assert!(front_matter.is_none());
            assert!(!missing_fields.is_empty());
            assert!(
                problems.iter().any(|problem| problem.contains("report")),
                "problems should name the missing report: {problems:?}"
            );
        }
        other => panic!("expected report evidence, got {other:?}"),
    }
}

#[test]
fn report_gate_status_partial_fails_done_requirement() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path(), "report-partial", Gate::Report(ReportGate::default()));
    write(
        &wt.join(&job.report),
        &format!(
            "---\njob: report-partial\nstatus: partial\nbranch: {}\ncommits: []\ntests: \"cargo test: ok\"\n---\nstill working\n",
            job.branch.as_deref().unwrap()
        ),
    );

    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner
        .evaluate(&context, 0, &Gate::Report(ReportGate::default()))
        .unwrap();

    assert!(!result.passed, "partial must never advance a job");
    match &result.evidence {
        Evidence::Report { problems, .. } => {
            assert!(
                problems.iter().any(|problem| problem.contains("not done")),
                "expected a done requirement, got {problems:?}"
            );
        }
        other => panic!("expected report evidence, got {other:?}"),
    }
}

#[test]
fn file_exists_only_gate_rejected_by_validate() {
    let existence_only = Gate::Cmd(CmdGate {
        run: "test -f Cargo.toml".to_owned(),
        ..CmdGate::default()
    });
    match validate_gate(&existence_only) {
        Err(CoreError::Invalid(message)) => assert!(message.contains("anti-pattern"), "{message}"),
        other => panic!("file-existence gate should be rejected, got {other:?}"),
    }

    let listing = Gate::Script(CmdGate {
        run: "ls -la".to_owned(),
        ..CmdGate::default()
    });
    assert!(matches!(validate_gate(&listing), Err(CoreError::Invalid(_))));

    let deploy = Gate::Deploy(DeployGate {
        run: "cargo run -- deploy".to_owned(),
        probe: CmdGate {
            run: "test -e target/release/vowp".to_owned(),
            ..CmdGate::default()
        },
        env: None,
    });
    assert!(matches!(validate_gate(&deploy), Err(CoreError::Invalid(_))));

    let real = Gate::Cmd(CmdGate {
        run: "cargo test -p vowp-core".to_owned(),
        ..CmdGate::default()
    });
    validate_gate(&real).expect("a real command gate must validate");
}

#[test]
fn validate_gate_rejects_nonzero_exit_without_pattern() {
    let no_pattern = Gate::Cmd(CmdGate {
        run: "grep -q 'needle' haystack.txt".to_owned(),
        expect_exit: 1,
        ..CmdGate::default()
    });
    match validate_gate(&no_pattern) {
        Err(CoreError::Invalid(message)) => {
            assert!(message.contains("expect_pattern"), "{message}");
        }
        other => panic!("a nonzero exit without a pattern should be rejected, got {other:?}"),
    }

    let negative = Gate::Cmd(CmdGate {
        run: "grep -q 'needle' haystack.txt".to_owned(),
        expect_exit: 1,
        expect_pattern: Some("needle".to_owned()),
        ..CmdGate::default()
    });
    validate_gate(&negative).expect("a negative gate with an expected pattern must validate");
}
