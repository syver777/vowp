//! Wiring of the gate anti-pattern ban into `GateRunner::evaluate`.
//! Spec: `docs/design/loops.md` §4 and `docs/design/crates/vowp-core.md` §4.

mod gates_support;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{CmdGate, Evidence, Gate, JobKind, VowpConfig};

#[test]
fn evaluate_fails_invalid_gate_without_running_it() {
    let tmp = tempdir().unwrap();
    let node = tmp.path().join("node");
    let wt = worktree(tmp.path());
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(
        &store,
        &project,
        "invalid-gate",
        JobKind::Build,
        &["src/**"],
        Vec::new(),
    );
    let job = set_workspace(&store, &job, "job/invalid-gate", Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);

    // This command would create the marker if it ran. It is invalid because it starts with a
    // file-existence-only check, so validation must reject it before the shell sees it.
    let marker = wt.join("gate-ran");
    let gate = Gate::Cmd(CmdGate {
        run: format!("test -f missing.txt || touch '{}'", marker.display()),
        ..CmdGate::default()
    });

    let result = runner.evaluate(&context, 0, &gate).unwrap();

    assert!(!result.passed, "an invalid gate must fail");
    assert!(!marker.exists(), "the command must never execute");
    match &result.evidence {
        Evidence::Cmd {
            exit_code,
            timed_out,
            tail,
            ..
        } => {
            assert_eq!(*exit_code, None, "an invalid gate never runs");
            assert!(!timed_out);
            assert!(
                tail.contains("anti-pattern") && tail.contains("execute and check results"),
                "evidence must state the violated rule: {tail:?}"
            );
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}
