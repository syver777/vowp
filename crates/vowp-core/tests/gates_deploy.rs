//! `deploy` gate tests: command then probe, with real shell scripts in a temp checkout.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::fs;
use std::path::Path;

use gates_support::{ctx, insert_job, insert_project, set_workspace, test_deps, test_store, worktree};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{Approval, CmdGate, DeployGate, EnvConfig, Evidence, Gate, JobKind, VowpConfig};

/// Adds one environment to a test configuration.
fn with_env(mut config: VowpConfig, name: &str, env: EnvConfig) -> VowpConfig {
    config.env.insert(name.to_owned(), env);
    config
}

/// Builds the job, store and dependencies for a deploy gate.
fn setup(
    tmp: &Path,
    config: VowpConfig,
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
    let job = insert_job(&store, &project, "deploy-test", JobKind::Build, &[], Vec::new());
    let job = set_workspace(&store, &job, "job/deploy-test", Some(&wt));
    let deps = test_deps(&node, config);
    (deps, job, store, wt)
}

#[test]
fn deploy_gate_runs_command_then_probe() {
    let tmp = tempdir().unwrap();
    let env_dir = tmp.path().join("env");
    fs::create_dir_all(&env_dir).unwrap();
    let order = tmp.path().join("order.log");
    let env = EnvConfig {
        checkout: env_dir.display().to_string(),
        build: Vec::new(),
        env_pass: Vec::new(),
        deploy: Vec::new(),
        guards: Vec::new(),
        approval: Approval::Human,
        serving_bank: None,
        probe: Some("cat state".to_owned()),
        rollback: Some("rollback.sh".to_owned()),
    };
    let config = with_env(VowpConfig::with_defaults_for_tests(), "work", env);
    let (deps, job, store, wt) = setup(tmp.path(), config);

    let gate = DeployGate {
        run: format!("printf 'deploy\\n' >> '{}' && printf v1 > state", order.display()),
        probe: CmdGate {
            run: format!("printf 'probe\\n' >> '{}' && grep -qx v1 state", order.display()),
            ..CmdGate::default()
        },
        env: Some("work".to_owned()),
    };
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner.evaluate(&context, 0, &Gate::Deploy(gate)).unwrap();

    assert!(result.passed, "evidence: {:?}", result.evidence);
    let lines: Vec<String> = fs::read_to_string(&order).unwrap().lines().map(str::to_owned).collect();
    assert_eq!(lines, vec!["deploy".to_owned(), "probe".to_owned()]);
    match &result.evidence {
        Evidence::Deploy {
            command,
            exit_code,
            probe,
            version_before,
            version_after,
            rollback_cmd,
        } => {
            assert!(command.contains("printf 'deploy"));
            assert_eq!(*exit_code, Some(0));
            assert_eq!(*version_before, None, "the environment had no state before deploy");
            assert_eq!(version_after.as_deref(), Some("v1"));
            assert_eq!(rollback_cmd.as_deref(), Some("rollback.sh"));
            match probe.as_ref() {
                Evidence::Cmd {
                    exit_code: probe_exit, ..
                } => assert_eq!(*probe_exit, Some(0)),
                other => panic!("expected cmd probe evidence, got {other:?}"),
            }
        }
        other => panic!("expected deploy evidence, got {other:?}"),
    }
}

#[test]
fn deploy_gate_probe_mismatch_fails() {
    let tmp = tempdir().unwrap();
    let env_dir = tmp.path().join("env");
    fs::create_dir_all(&env_dir).unwrap();
    let env = EnvConfig {
        checkout: env_dir.display().to_string(),
        build: Vec::new(),
        env_pass: Vec::new(),
        deploy: Vec::new(),
        guards: Vec::new(),
        approval: Approval::Human,
        serving_bank: None,
        probe: None,
        rollback: None,
    };
    let config = with_env(VowpConfig::with_defaults_for_tests(), "work", env);
    let (deps, job, store, wt) = setup(tmp.path(), config);

    let gate = DeployGate {
        run: "printf v1 > state".to_owned(),
        probe: CmdGate {
            run: "grep -qx v2 state".to_owned(),
            ..CmdGate::default()
        },
        env: Some("work".to_owned()),
    };
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner.evaluate(&context, 0, &Gate::Deploy(gate)).unwrap();

    assert!(!result.passed, "a mismatched probe must fail the gate");
    match &result.evidence {
        Evidence::Deploy { exit_code, probe, .. } => {
            assert_eq!(*exit_code, Some(0), "the deploy itself succeeded");
            match probe.as_ref() {
                Evidence::Cmd {
                    exit_code: probe_exit,
                    tail,
                    ..
                } => {
                    assert_eq!(*probe_exit, Some(1));
                    assert!(tail.is_empty() || !tail.contains("v2"), "tail: {tail:?}");
                }
                other => panic!("expected cmd probe evidence, got {other:?}"),
            }
        }
        other => panic!("expected deploy evidence, got {other:?}"),
    }
}

#[test]
fn deploy_gate_fails_on_unknown_env() {
    let tmp = tempdir().unwrap();
    let (deps, job, store, wt) = setup(tmp.path(), VowpConfig::with_defaults_for_tests());
    let marker = wt.join("deployed-in-worktree");
    // The probe is behavioural, so the gate passes validation and evaluation really reaches
    // environment resolution. If the unknown environment were ignored and the worktree used
    // instead, both the deploy command and this probe would succeed.
    let probe = CmdGate {
        run: "printf 'probe ok\\n'".to_owned(),
        expect_pattern: Some("probe ok".to_owned()),
        ..CmdGate::default()
    };
    let gate = DeployGate {
        run: format!("printf deployed > '{}'", marker.display()),
        probe,
        env: Some("missing".to_owned()),
    };
    Gate::Deploy(gate.clone())
        .validate()
        .expect("the behavioural probe must pass gate validation");
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &wt, &wt);
    let result = runner.evaluate(&context, 0, &Gate::Deploy(gate)).unwrap();

    assert!(!result.passed, "an unknown environment must fail the gate");
    assert!(
        !marker.exists(),
        "the deploy command must never run in the job worktree"
    );
    match &result.evidence {
        Evidence::Deploy {
            command,
            exit_code,
            probe,
            version_before,
            version_after,
            rollback_cmd,
        } => {
            assert_eq!(command, &format!("printf deployed > '{}'", marker.display()));
            assert_eq!(*exit_code, None, "no deploy command ran");
            assert_eq!(*version_before, None);
            assert_eq!(*version_after, None);
            assert_eq!(*rollback_cmd, None);
            match probe.as_ref() {
                Evidence::Cmd {
                    exit_code: probe_exit,
                    tail,
                    ..
                } => {
                    assert_eq!(*probe_exit, None, "the probe did not run");
                    assert!(
                        tail.contains("missing"),
                        "evidence must name the missing environment: {tail:?}"
                    );
                }
                other => panic!("expected cmd probe evidence, got {other:?}"),
            }
        }
        other => panic!("expected deploy evidence, got {other:?}"),
    }
}
