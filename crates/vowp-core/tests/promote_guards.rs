//! Integration tests for promotion guards and the guard-only dry run.
//! Spec: `docs/design/crates/vowp-core.md` §8.

#[path = "promote_support.rs"]
mod support;

use tempfile::TempDir;
use vowp_core::promote::guards;
use vowp_core::promote::{PromoteOpts, Promoter};
use vowp_protocol::{Approval, LedgerKind, ReleaseState};
use vowp_store::LedgerFilter;

use support::*;

/// A repo whose HEAD adds `change.txt`, plus an environment checkout at that commit.
fn repo_with_candidate(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let repo = root.join("repo");
    init_repo(&repo);
    write(&repo.join("change.txt"), "new\n");
    let candidate = commit_all(&repo, "candidate");
    let checkout = root.join("checkout");
    clone_at(&repo, &checkout, &candidate);
    (repo, checkout, candidate)
}

#[test]
fn guards_clean_tree_dirty_fails() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    init_repo(&repo);
    let config = config_for(&repo);
    let deps = test_deps(root, config);
    let env = env_config(&repo);

    let clean = guards::run_guard(&deps, &env, "clean_tree", &repo).unwrap();
    assert!(clean.passed, "clean repository must pass: {}", clean.detail);
    assert_eq!(clean.name, "clean_tree");

    write(&repo.join("dirty.txt"), "uncommitted\n");
    let dirty = guards::run_guard(&deps, &env, "clean_tree", &repo).unwrap();
    assert!(!dirty.passed, "untracked file must fail clean_tree");
    assert!(dirty.detail.contains("uncommitted") || dirty.detail.contains("untracked"));
}

#[test]
fn guards_head_equals_remote_behind_fails() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let origin = root.join("origin.git");
    init_bare(&origin);
    let repo = root.join("repo");
    init_repo(&repo);
    git_ok(&repo, &["remote", "add", "origin", &origin.display().to_string()]);
    git_ok(&repo, &["push", "-u", "origin", "main"]);

    let checkout = root.join("checkout");
    clone_at(&origin, &checkout, "main");

    let config = config_for(&repo);
    let deps = test_deps(root, config);
    let env = env_config(&checkout);

    let current = guards::run_guard(&deps, &env, "head_equals_remote", &checkout).unwrap();
    assert!(current.passed, "checkout at origin/main must pass: {}", current.detail);

    write(&repo.join("ahead.txt"), "ahead\n");
    commit_all(&repo, "ahead");
    git_ok(&repo, &["push", "origin", "main"]);

    let behind = guards::run_guard(&deps, &env, "head_equals_remote", &checkout).unwrap();
    assert!(!behind.passed, "a checkout behind origin/main must fail");
    assert!(behind.detail.contains("origin/main"), "detail: {}", behind.detail);
}

#[test]
fn guard_failure_marks_refused_and_never_deploys() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = repo_with_candidate(root);
    let log = root.join("order.log");
    let deploy = root.join("deploy.sh");
    script(&deploy, &format!("echo deploy >> {}", log.display()));
    let bank = root.join("bank.sh");
    script(
        &bank,
        &format!("echo bank >> {}\necho snapshot > \"$VOWP_BANK_PATH\"", log.display()),
    );
    let probe = root.join("probe.sh");
    script(&probe, &format!("echo probe >> {}", log.display()));

    write(&checkout.join("dirty.txt"), "dirty\n");
    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.guards = vec!["clean_tree".to_owned()];
    env.serving_bank = Some(format!("{} {{target}}", bank.display()));
    env.deploy = vec![deploy.display().to_string()];
    env.probe = Some(probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(root.join("rollback.sh").display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));

    let release = promoter
        .promote(
            &store,
            &project,
            "prod",
            &candidate,
            PromoteOpts {
                yes: true,
                ..PromoteOpts::default()
            },
        )
        .unwrap();

    assert_eq!(release.state, ReleaseState::Refused);
    assert_eq!(release.guards.len(), 1);
    assert_eq!(release.guards[0].name, "clean_tree");
    assert!(!release.guards[0].passed);
    assert!(release.note.contains("clean_tree"));
    assert!(release.rollback_cmd.is_none());
    assert!(release.snapshot_path.is_none());
    assert!(!log.exists(), "no bank, deploy or probe may run after a guard failure");

    let stored = store.release(&release.id).unwrap().unwrap();
    assert_eq!(stored.state, ReleaseState::Refused);
    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Promote),
            ..LedgerFilter::default()
        })
        .unwrap();
    assert_eq!(ledger.len(), 1, "a refusal records one promote ledger row");
    assert!(read(&node.join("releases.log")).contains("refused"));
}

#[test]
fn dry_run_runs_guards_only() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = repo_with_candidate(root);
    let log = root.join("order.log");
    let guard = root.join("guard.sh");
    script(&guard, &format!("echo guard >> {}", log.display()));
    let bank = root.join("bank.sh");
    script(
        &bank,
        &format!("echo bank >> {}\necho snapshot > \"$VOWP_BANK_PATH\"", log.display()),
    );
    let deploy = root.join("deploy.sh");
    script(&deploy, &format!("echo deploy >> {}", log.display()));
    let probe = root.join("probe.sh");
    script(&probe, &format!("echo probe >> {}", log.display()));

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.guards = vec![format!("ok:{}", guard.display())];
    env.serving_bank = Some(format!("{} {{target}}", bank.display()));
    env.deploy = vec![deploy.display().to_string()];
    env.probe = Some(probe.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));

    let release = promoter
        .promote(
            &store,
            &project,
            "prod",
            &candidate,
            PromoteOpts {
                dry_run: true,
                ..PromoteOpts::default()
            },
        )
        .unwrap();

    assert_eq!(release.state, ReleaseState::Guarded);
    assert_eq!(release.guards.len(), 1);
    assert!(release.guards[0].passed);
    assert!(release.snapshot_path.is_none());
    assert!(release.rollback_cmd.is_none());
    assert_eq!(read(&log).trim(), "guard");
    assert!(!node.join("releases.log").exists(), "guarded is not terminal");
    assert!(
        store.list_inbox(None).unwrap().is_empty(),
        "dry run never asks for approval"
    );
    assert_eq!(
        store.release(&release.id).unwrap().unwrap().state,
        ReleaseState::Guarded
    );

    // The dry run also compares the candidate with the serving checkout: drift is refused
    // without running the bank, deploy or probe scripts.
    write(&checkout.join("rogue.txt"), "live drift\n");
    let drifted = promoter
        .promote(
            &store,
            &project,
            "prod",
            &candidate,
            PromoteOpts {
                dry_run: true,
                ..PromoteOpts::default()
            },
        )
        .unwrap();
    assert_eq!(drifted.state, ReleaseState::Refused);
    assert!(drifted.note.contains("rogue.txt"), "note: {}", drifted.note);
    assert!(drifted.snapshot_path.is_none());
    assert!(drifted.rollback_cmd.is_none());
    let lines = read(&log);
    let lines: Vec<&str> = lines.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    assert_eq!(lines, vec!["guard", "guard"], "only guards ran; no bank/deploy/probe");
    assert!(read(&node.join("releases.log")).contains("refused"));
    assert!(
        store.list_inbox(None).unwrap().is_empty(),
        "dry run never asks for approval"
    );
}
