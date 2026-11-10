//! `cmd` gate tests: exit codes, expected patterns, timeouts and evidence tails.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::fs;
use std::path::Path;
use std::time::Duration;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{CmdGate, Evidence, Gate, JobKind, VowpConfig};

/// Builds one evaluable job and its dependencies.
fn setup(
    tmp: &Path,
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
    let job = insert_job(&store, &project, "cmd-test", JobKind::Build, &["src/**"], Vec::new());
    let job = set_workspace(&store, &job, "job/cmd-test", Some(&wt));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store, wt)
}

/// Evaluates one command gate.
fn evaluate(
    deps: &vowp_core::Deps,
    job: &vowp_protocol::Job,
    store: &vowp_store::Store,
    wt: &Path,
    gate: &CmdGate,
) -> vowp_protocol::GateResult {
    let runner = GateRunner::new(deps.clone());
    let context = ctx(deps, job, store, wt, wt);
    runner.evaluate(&context, 0, &Gate::Cmd(gate.clone())).unwrap()
}

#[test]
fn cmd_gate_exit_zero_passes() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "printf 'all good\\n'".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(result.passed);
    match &result.evidence {
        Evidence::Cmd {
            exit_code,
            timed_out,
            tail,
            ..
        } => {
            assert_eq!(*exit_code, Some(0));
            assert!(!timed_out);
            assert!(tail.contains("all good"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_wrong_exit_fails_with_tail() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "printf 'boom detail\\n'; exit 3".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd { exit_code, tail, .. } => {
            assert_eq!(*exit_code, Some(3));
            assert!(tail.contains("boom detail"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_expect_pattern() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());

    let matching = CmdGate {
        run: "printf 'test result: ok. 31 passed\\n'".to_owned(),
        expect_pattern: Some("31 passed".to_owned()),
        ..CmdGate::default()
    };
    assert!(evaluate(&deps, &job, &store, &wt, &matching).passed);

    let not_matching = CmdGate {
        run: "printf 'test result: FAILED\\n'".to_owned(),
        expect_pattern: Some("31 passed".to_owned()),
        ..CmdGate::default()
    };
    let result = evaluate(&deps, &job, &store, &wt, &not_matching);
    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd { exit_code, tail, .. } => {
            assert_eq!(*exit_code, Some(0), "the process itself exited fine");
            assert!(tail.contains("FAILED"), "tail: {tail:?}");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}

#[test]
fn cmd_gate_timeout_kills_group_and_fails() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let marker = wt.join("survived");
    let gate = CmdGate {
        run: format!("(sleep 2; printf survived > '{}') & sleep 30", marker.display()),
        timeout_secs: 1,
        ..CmdGate::default()
    };

    let started = std::time::Instant::now();
    let result = evaluate(&deps, &job, &store, &wt, &gate);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "gate should return promptly after the timeout"
    );
    assert!(!result.passed);
    match &result.evidence {
        Evidence::Cmd {
            timed_out, exit_code, ..
        } => {
            assert!(timed_out, "timeout must be recorded");
            assert_eq!(*exit_code, None, "a killed process has no exit code");
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }

    std::thread::sleep(Duration::from_millis(2300));
    assert!(
        fs::metadata(&marker).is_err(),
        "the background child survived; the gate did not kill the process group"
    );
}

#[test]
fn cmd_gate_tail_is_last_200_lines() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path());
    let gate = CmdGate {
        run: "for i in $(seq 1 250); do echo line$i; done".to_owned(),
        ..CmdGate::default()
    };

    let result = evaluate(&deps, &job, &store, &wt, &gate);

    assert!(result.passed);
    match &result.evidence {
        Evidence::Cmd { tail, .. } => {
            let lines: Vec<&str> = tail.lines().collect();
            assert_eq!(lines.len(), 200, "tail must hold exactly the last 200 lines");
            assert_eq!(lines.first(), Some(&"line51"));
            assert_eq!(lines.last(), Some(&"line250"));
        }
        other => panic!("expected cmd evidence, got {other:?}"),
    }
}
