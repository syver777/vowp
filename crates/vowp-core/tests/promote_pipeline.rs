//! Integration tests for the promotion pipeline: bank ordering, diff, approval, deploy and log.
//! Spec: `docs/design/crates/vowp-core.md` §8.

#[path = "promote_support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use vowp_core::promote::{PromoteOpts, Promoter};
use vowp_core::vault;
use vowp_protocol::{Approval, InboxKind, InboxState, LedgerKind, MemoryLog, ReleaseState, SecretName};
use vowp_store::LedgerFilter;

use support::*;

/// A repo whose HEAD adds `change.txt`, plus an environment checkout at that commit.
fn candidate_fixture(root: &Path) -> (PathBuf, PathBuf, String) {
    let repo = root.join("repo");
    init_repo(&repo);
    write(&repo.join("change.txt"), "new\n");
    let candidate = commit_all(&repo, "candidate");
    let checkout = root.join("checkout");
    clone_at(&repo, &checkout, &candidate);
    (repo, checkout, candidate)
}

/// The scripts and log a pipeline test uses.
struct Scripts {
    log: PathBuf,
    bank: PathBuf,
    deploy: PathBuf,
    probe: PathBuf,
    rollback: PathBuf,
}

fn scripts(root: &Path) -> Scripts {
    let log = root.join("order.log");
    let bank = root.join("bank.sh");
    script(
        &bank,
        &format!("echo bank >> {}\necho snapshot > \"$VOWP_BANK_PATH\"", log.display()),
    );
    let deploy = root.join("deploy.sh");
    script(&deploy, &format!("echo deploy >> {}", log.display()));
    let probe = root.join("probe.sh");
    script(&probe, &format!("echo probe >> {}", log.display()));
    let rollback = root.join("rollback.sh");
    script(&rollback, &format!("echo \"rollback $1\" >> {}", log.display()));
    Scripts {
        log,
        bank,
        deploy,
        probe,
        rollback,
    }
}

fn order(root: &Path) -> Vec<String> {
    let log = root.join("order.log");
    if !log.exists() {
        return Vec::new();
    }
    read(&log)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn serving_bank_runs_before_deploy_order_asserted_via_script_log() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);
    let guard = root.join("guard.sh");
    script(&guard, &format!("echo guard >> {}", scripts.log.display()));

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.guards = vec![format!("ok:{}", guard.display())];
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
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

    assert_eq!(release.state, ReleaseState::Live);
    assert_eq!(order(root), vec!["guard", "bank", "deploy", "probe"]);
    let snapshot = release.snapshot_path.as_ref().expect("snapshot path");
    assert!(Path::new(snapshot).exists(), "bank script created {snapshot}");
    let rollback = release.rollback_cmd.as_ref().expect("rollback command");
    assert!(rollback.contains(snapshot));
    assert!(rollback.contains("rollback.sh"));
    assert_eq!(release.probe["passed"], true);
}

#[test]
fn diff_unexpected_files_refused() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);
    write(&checkout.join("rogue.txt"), "live drift\n");

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
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
    assert!(release.note.contains("rogue.txt"), "note: {}", release.note);
    assert_eq!(order(root), vec!["bank"], "the deploy and probe never run");
}

#[test]
fn human_approval_creates_inbox_and_waits() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.rollback = Some(scripts.rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let release = promoter
        .promote(&store, &project, "prod", &candidate, PromoteOpts::default())
        .unwrap();

    assert_eq!(release.state, ReleaseState::Banked);
    assert!(release.snapshot_path.is_some());
    assert!(release.rollback_cmd.is_some());
    let inbox = store.list_inbox(None).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::Approval);
    assert_eq!(inbox[0].state, InboxState::Open);
    assert!(inbox[0].title.contains("Promote"));
    assert!(inbox[0].body.contains(&release.id.to_string()));
    assert_eq!(order(root), vec!["bank"], "deploy waits for the human answer");
}

#[test]
fn dry_run_of_banked_release_never_deploys_or_asks() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.rollback = Some(scripts.rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let banked = promoter
        .promote(&store, &project, "prod", &candidate, PromoteOpts::default())
        .unwrap();
    assert_eq!(banked.state, ReleaseState::Banked);
    let item = store.list_inbox(Some(InboxState::Open)).unwrap().pop().unwrap();
    store.answer_inbox(&item.id, "approve", "alice").unwrap();

    let dry = promoter
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

    assert_eq!(dry.state, ReleaseState::Banked);
    assert_eq!(dry.id, banked.id);
    assert!(dry.approved_by.is_none(), "a dry run records no approver");
    assert_eq!(order(root), vec!["bank"], "a dry run never deploys or probes");
    let inbox = store.list_inbox(None).unwrap();
    assert_eq!(inbox.len(), 1, "a dry run never creates another approval item");
    assert_eq!(inbox[0].state, InboxState::Answered);

    // The answered approval is still usable by a real promotion afterwards.
    let live = promoter
        .promote(&store, &project, "prod", &candidate, PromoteOpts::default())
        .unwrap();
    assert_eq!(live.state, ReleaseState::Live);
    assert_eq!(live.approved_by.as_deref(), Some("alice"));
    assert_eq!(order(root), vec!["bank", "deploy", "probe"]);
}

#[test]
fn answer_approve_then_promote_goes_live_and_records_rollback_cmd() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.rollback = Some(scripts.rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let banked = promoter
        .promote(&store, &project, "prod", &candidate, PromoteOpts::default())
        .unwrap();
    assert_eq!(banked.state, ReleaseState::Banked);

    let item = store.list_inbox(Some(InboxState::Open)).unwrap().pop().unwrap();
    store.answer_inbox(&item.id, "approve", "alice").unwrap();

    let release = promoter
        .promote(&store, &project, "prod", &candidate, PromoteOpts::default())
        .unwrap();
    assert_eq!(release.state, ReleaseState::Live);
    assert_eq!(release.id, banked.id, "the banked release is resumed, not replaced");
    assert_eq!(release.approved_by.as_deref(), Some("alice"));
    assert_eq!(order(root), vec!["bank", "deploy", "probe"], "bank runs once");
    let rollback = release.rollback_cmd.as_ref().expect("rollback command");
    assert!(rollback.contains(release.snapshot_path.as_deref().unwrap()));
    assert_eq!(store.release(&banked.id).unwrap().unwrap().state, ReleaseState::Live);
}

#[test]
fn policy_approval_with_yes_deploys() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
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
                approved_by: Some("ops".to_owned()),
                ..PromoteOpts::default()
            },
        )
        .unwrap();

    assert_eq!(release.state, ReleaseState::Live);
    assert_eq!(release.approved_by.as_deref(), Some("ops"));
    assert!(store.list_inbox(None).unwrap().is_empty());
    assert_eq!(order(root), vec!["bank", "deploy", "probe"]);
}

#[test]
fn env_pass_injects_stored_secret_into_bank_deploy_probe_and_rollback() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);

    let token = "deploy-token-value-9f3a7c";
    let other_value = "unconfigured-value-1b7c4d";
    let bank_token = root.join("bank-token.env");
    let bank_other = root.join("bank-other.env");
    let deploy_token = root.join("deploy-token.env");
    let deploy_other = root.join("deploy-other.env");
    let probe_token = root.join("probe-token.env");
    let rollback_token = root.join("rollback-token.env");

    let guard = root.join("guard.sh");
    script(&guard, "echo guard-pass");
    let bank = root.join("bank.sh");
    script(
        &bank,
        &format!(
            "printf '%s' \"$DEPLOY_TOKEN\" > {}\nprintf '%s' \"${{VOWP_TEST_UNCONFIGURED_SECRET_7F1A-UNSET}}\" > {}\necho snapshot > \"$VOWP_BANK_PATH\"",
            bank_token.display(),
            bank_other.display()
        ),
    );
    let deploy = root.join("deploy.sh");
    script(
        &deploy,
        &format!(
            "printf '%s' \"$DEPLOY_TOKEN\" > {}\nprintf '%s' \"${{VOWP_TEST_UNCONFIGURED_SECRET_7F1A-UNSET}}\" > {}",
            deploy_token.display(),
            deploy_other.display()
        ),
    );
    let probe = root.join("probe.sh");
    script(
        &probe,
        &format!("printf '%s' \"$DEPLOY_TOKEN\" > {}", probe_token.display()),
    );
    let rollback = root.join("rollback.sh");
    script(
        &rollback,
        &format!("printf '%s' \"$DEPLOY_TOKEN\" > {}", rollback_token.display()),
    );

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.env_pass = vec!["DEPLOY_TOKEN".to_owned()];
    env.guards = vec![format!("ok:{}", guard.display())];
    env.serving_bank = Some(format!("{} {{target}}", bank.display()));
    env.deploy = vec![deploy.display().to_string()];
    env.probe = Some(probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let log = Arc::new(MemoryLog::new());
    let store = test_store(&node);
    let project = insert_project(&store);
    let deps = test_deps_with_log(&node, config, Arc::clone(&log));
    let token_name = SecretName::parse("DEPLOY_TOKEN").unwrap();
    let other_name = SecretName::parse("VOWP_TEST_UNCONFIGURED_SECRET_7F1A").unwrap();
    vault::write_secret(&deps, &token_name, token).unwrap();
    vault::write_secret(&deps, &other_name, other_value).unwrap();
    let promoter = Promoter::new(deps.clone());

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
    assert_eq!(release.state, ReleaseState::Live);

    assert_eq!(read(&bank_token), token, "the serving bank sees the secret");
    assert_eq!(read(&deploy_token), token, "the deploy sees the secret");
    assert_eq!(read(&probe_token), token, "the probe sees the secret");
    assert_eq!(
        read(&bank_other),
        "UNSET",
        "an unconfigured secret is absent from the bank command"
    );
    assert_eq!(
        read(&deploy_other),
        "UNSET",
        "an unconfigured secret is absent from the deploy command"
    );

    let release_json = serde_json::to_string(&release).expect("release serialises");
    assert!(!release_json.contains(token), "the release record leaked the value");
    assert!(
        !serde_json::to_string(&release.guards)
            .expect("guards serialise")
            .contains(token),
        "the gate evidence leaked the value"
    );

    let rolled_back = promoter
        .rollback(&store, &release.id, PromoteOpts::default())
        .expect("rollback");
    assert_eq!(rolled_back.state, ReleaseState::RolledBack);
    assert_eq!(read(&rollback_token), token, "the rollback sees the secret");
    assert!(
        !serde_json::to_string(&rolled_back)
            .expect("rollback release serialises")
            .contains(token),
        "the rollback record leaked the value"
    );

    let releases_log = read(&node.join("releases.log"));
    assert!(!releases_log.contains(token), "releases.log leaked the value");
    let ledger = store.ledger_since(&LedgerFilter::default()).unwrap();
    assert!(!ledger.is_empty());
    assert!(
        !serde_json::to_string(&ledger).unwrap().contains(token),
        "the ledger summary or detail leaked the value"
    );
    assert!(!log.contains(token), "the supervisor log leaked the value");
}

#[test]
fn missing_env_pass_secret_fails_before_banking_or_deploying() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.env_pass = vec!["MISSING_DEPLOY_TOKEN".to_owned()];
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let error = promoter
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
        .expect_err("a configured but missing secret must fail the promotion");

    assert!(
        error.to_string().contains("MISSING_DEPLOY_TOKEN"),
        "the error names the variable: {error}"
    );
    assert!(order(root).is_empty(), "no guard, bank or deploy command ran");
    assert!(
        store.list_releases(&project, None).unwrap().is_empty(),
        "the promotion did not record a release"
    );
}

#[test]
fn rollback_with_missing_env_pass_secret_does_not_run_the_command() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);
    let rollback_marker = root.join("rollback-ran");
    let rollback = root.join("rollback.sh");
    script(
        &rollback,
        &format!(
            ": > {}\necho \"rollback $1\" >> {}",
            rollback_marker.display(),
            scripts.log.display()
        ),
    );

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.env_pass = vec!["ROLLBACK_TOKEN".to_owned()];
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let deps = test_deps(&node, config);
    let name = SecretName::parse("ROLLBACK_TOKEN").unwrap();
    vault::write_secret(&deps, &name, "rollback-token-value-4e2b").unwrap();
    let promoter = Promoter::new(deps.clone());

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
    assert_eq!(release.state, ReleaseState::Live);

    vault::remove_secret(&deps, &name).unwrap();
    let error = promoter
        .rollback(&store, &release.id, PromoteOpts::default())
        .expect_err("a configured but missing secret must fail the rollback");

    assert!(
        error.to_string().contains("ROLLBACK_TOKEN"),
        "the error names the variable: {error}"
    );
    assert!(!rollback_marker.exists(), "the rollback command did not run");
    assert_eq!(
        store.release(&release.id).unwrap().unwrap().state,
        ReleaseState::Live,
        "the original release is unchanged"
    );
    assert_eq!(
        promoter.releases(&store, &project, Some("prod"), 10).unwrap().len(),
        1,
        "the failed rollback recorded no release"
    );
}

#[test]
fn yes_flag_never_bypasses_human_approval() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.rollback = Some(scripts.rollback.display().to_string());
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
                approved_by: Some("intruder".to_owned()),
                ..PromoteOpts::default()
            },
        )
        .unwrap();

    assert_eq!(release.state, ReleaseState::Banked);
    assert!(release.approved_by.is_none());
    assert_eq!(store.list_inbox(Some(InboxState::Open)).unwrap().len(), 1);
    assert_eq!(order(root), vec!["bank"], "deploy waits for a human answer");
}

#[test]
fn probe_mismatch_marks_failed_with_rollback_available() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let (repo, checkout, candidate) = candidate_fixture(root);
    let scripts = scripts(root);
    let bad_probe = root.join("bad_probe.sh");
    script(&bad_probe, &format!("echo probe >> {}\nexit 1", scripts.log.display()));

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(bad_probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
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

    assert_eq!(release.state, ReleaseState::Failed);
    assert!(release.finished_at.is_some());
    let rollback = release.rollback_cmd.as_ref().expect("rollback stays available");
    assert!(rollback.contains(release.snapshot_path.as_deref().unwrap()));
    assert_eq!(release.probe["passed"], false);
    assert_eq!(release.probe["exit_code"], 1);
    assert!(release.note.contains("probe"), "note: {}", release.note);
    assert_eq!(order(root), vec!["bank", "deploy", "probe"]);
    assert!(read(&node.join("releases.log")).contains("failed"));
}

#[test]
fn releases_log_lines_appended() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let node = root.join("node");
    let repo = root.join("repo");
    init_repo(&repo);
    let base = head(&repo);
    write(&repo.join("one.txt"), "one\n");
    let first = commit_all(&repo, "one");
    write(&repo.join("two.txt"), "two\n");
    let second = commit_all(&repo, "two");
    let checkout = root.join("checkout");
    clone_at(&repo, &checkout, &base);
    let scripts = scripts(root);

    let mut config = config_for(&repo);
    let mut env = env_config(&checkout);
    env.serving_bank = Some(format!("{} {{target}}", scripts.bank.display()));
    env.deploy = vec![scripts.deploy.display().to_string()];
    env.probe = Some(scripts.probe.display().to_string());
    env.approval = Approval::Policy;
    env.rollback = Some(scripts.rollback.display().to_string());
    config.env.insert("prod".to_owned(), env);

    let store = test_store(&node);
    let project = insert_project(&store);
    let promoter = Promoter::new(test_deps(&node, config));
    let opts = PromoteOpts {
        yes: true,
        ..PromoteOpts::default()
    };
    let first_release = promoter
        .promote(&store, &project, "prod", &first, opts.clone())
        .unwrap();
    assert_eq!(first_release.state, ReleaseState::Live);
    assert_eq!(first_release.from_commit.as_deref(), Some(base.as_str()));

    git_ok(&checkout, &["checkout", "-q", &first]);
    let second_release = promoter.promote(&store, &project, "prod", &second, opts).unwrap();
    assert_eq!(second_release.state, ReleaseState::Live);
    assert_eq!(second_release.from_commit.as_deref(), Some(first.as_str()));
    assert_eq!(second_release.to_commit.as_deref(), Some(second.as_str()));

    let log = read(&node.join("releases.log"));
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "one line per terminal release: {log}");
    assert!(lines[0].contains(" prod "));
    assert!(lines[0].contains(" live "));
    assert!(lines[0].contains(&format!("{base}->{first}")));
    assert!(lines[1].contains(&format!("{first}->{second}")));

    let listed = promoter.releases(&store, &project, Some("prod"), 10).unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second_release.id);
    assert_eq!(listed[1].id, first_release.id);
    let limited = promoter.releases(&store, &project, Some("prod"), 1).unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, second_release.id);

    let ledger = store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Promote),
            ..LedgerFilter::default()
        })
        .unwrap();
    assert_eq!(ledger.len(), 2);
}
