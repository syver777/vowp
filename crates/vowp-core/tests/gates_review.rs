//! `review` gate tests: latest verifier report verdict, pending before verification.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4–§5.

mod gates_support;

use std::path::Path;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree, write};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{Edge, EdgeKind, Evidence, Gate, Job, JobKind, ReportGate, ReviewGate, Verdict, VowpConfig};

/// Builds the original job plus its dependencies.
fn setup(tmp: &Path) -> (vowp_core::Deps, Job, vowp_store::Store, std::path::PathBuf) {
    let node = tmp.join("node");
    let wt = worktree(tmp);
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(
        &store,
        &project,
        "review-test",
        JobKind::Build,
        &["src/**"],
        vec![Gate::Review(ReviewGate::default())],
    );
    let job = set_workspace(&store, &job, "job/review-test", Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store, wt)
}

/// Inserts a verify job for `attempt` with a `verifies` edge to `original`.
fn add_verify_job(store: &vowp_store::Store, original: &Job, wt: &Path, attempt: u32) -> Job {
    let project = original.project.clone();
    let id = format!("{}-verify-{attempt}", original.id);
    let verify = insert_job(
        store,
        &project,
        &id,
        JobKind::Verify,
        &[],
        vec![Gate::Report(ReportGate::default())],
    );
    let verify = set_workspace(
        store,
        &verify,
        original.branch.as_deref().unwrap_or("job/review-test"),
        Some(wt),
    );
    store
        .add_edge(&Edge {
            src: verify.id.clone(),
            dst: original.id.clone(),
            kind: EdgeKind::Verifies,
        })
        .unwrap();
    verify
}

/// Writes a verifier report with the given verdict and findings.
fn write_verifier_report(path: &Path, verify: &Job, verdict: &str, findings: &[&str]) {
    let findings_yaml = if findings.is_empty() {
        "[]".to_owned()
    } else {
        format!(
            "[{}]",
            findings
                .iter()
                .map(|finding| format!("\"{finding}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    write(
        path,
        &format!(
            "---\njob: {}\nstatus: done\nbranch: {}\ncommits: []\ntests: \"reviewed\"\nverdict: {verdict}\nfindings: {findings_yaml}\n---\nverified\n",
            verify.id,
            verify.branch.as_deref().unwrap()
        ),
    );
}

#[test]
fn review_gate_reads_verifier_report_verdict() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let verify = add_verify_job(&store, &job, &wt, 0);
    let report_path = wt.join(&verify.report);

    write_verifier_report(&report_path, &verify, "pass", &[]);
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let passed = runner
        .evaluate(&context, 0, &Gate::Review(ReviewGate::default()))
        .unwrap();
    assert!(passed.passed, "evidence: {:?}", passed.evidence);
    match &passed.evidence {
        Evidence::Review {
            verify_job,
            verdict,
            findings,
        } => {
            assert_eq!(verify_job.as_ref(), Some(&verify.id));
            assert_eq!(*verdict, Some(Verdict::Pass));
            assert!(findings.is_empty());
        }
        other => panic!("expected review evidence, got {other:?}"),
    }

    write_verifier_report(
        &report_path,
        &verify,
        "fail",
        &["missing regression test", "unused import"],
    );
    let failed = runner
        .evaluate(&context, 0, &Gate::Review(ReviewGate::default()))
        .unwrap();
    assert!(!failed.passed);
    match &failed.evidence {
        Evidence::Review {
            verify_job,
            verdict,
            findings,
        } => {
            assert_eq!(verify_job.as_ref(), Some(&verify.id));
            assert_eq!(*verdict, Some(Verdict::Fail));
            assert_eq!(
                findings,
                &["missing regression test".to_owned(), "unused import".to_owned()]
            );
        }
        other => panic!("expected review evidence, got {other:?}"),
    }
}

#[test]
fn review_gate_pending_when_no_verify_job_yet() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);

    let result = runner
        .evaluate(&context, 0, &Gate::Review(ReviewGate::default()))
        .unwrap();

    assert!(!result.passed, "a job cannot pass review before it is verified");
    match &result.evidence {
        Evidence::Review {
            verify_job,
            verdict,
            findings,
        } => {
            assert!(verify_job.is_none());
            assert!(verdict.is_none());
            assert!(findings.is_empty());
        }
        other => panic!("expected review evidence, got {other:?}"),
    }
}

#[test]
fn review_gate_ignores_verifier_from_earlier_attempt() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());

    // Attempt 0 was verified and passed.
    let earlier = add_verify_job(&store, &job, &wt, 0);
    write_verifier_report(&wt.join(&earlier.report), &earlier, "pass", &[]);

    // The job moves to attempt 1; no verifier has run for it yet.
    store.set_job_counters(&job.id, Some(1), None, None).unwrap();
    let job = store.require_job(&job.id).unwrap();
    assert_eq!(job.attempts, 1);
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);

    let missing = runner
        .evaluate(&context, 0, &Gate::Review(ReviewGate::default()))
        .unwrap();
    assert!(
        !missing.passed,
        "an earlier attempt's passing verifier must not satisfy the current attempt: {:?}",
        missing.evidence
    );
    match &missing.evidence {
        Evidence::Review {
            verify_job,
            verdict,
            findings,
        } => {
            assert!(verify_job.is_none(), "no current-attempt verifier exists");
            assert!(verdict.is_none());
            assert!(findings.is_empty());
        }
        other => panic!("expected review evidence, got {other:?}"),
    }

    // A current-attempt verifier that failed also fails the gate.
    let current = add_verify_job(&store, &job, &wt, 1);
    write_verifier_report(
        &wt.join(&current.report),
        &current,
        "fail",
        &["acceptable only after the fix"],
    );
    let failed = runner
        .evaluate(&context, 0, &Gate::Review(ReviewGate::default()))
        .unwrap();
    assert!(!failed.passed);
    match &failed.evidence {
        Evidence::Review {
            verify_job,
            verdict,
            findings,
        } => {
            assert_eq!(verify_job.as_ref(), Some(&current.id));
            assert_eq!(*verdict, Some(Verdict::Fail));
            assert_eq!(findings, &["acceptable only after the fix".to_owned()]);
        }
        other => panic!("expected review evidence, got {other:?}"),
    }
}
