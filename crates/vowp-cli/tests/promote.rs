//! `vowp promote`, `rollback` and `releases` integration tests.
//!
//! Every test uses a temporary node home and project, a real git repository, and fake
//! guard/bank/deploy/probe/rollback shell scripts. Nothing reads the real home directory,
//! starts a harness or touches the network.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use vowp_protocol::{Approval, EnvConfig, InboxKind, InboxState, Release, ReleaseState};

/// The scripts one environment runs, plus the shared order log.
struct Scripts {
    /// One line per script that ran.
    log: PathBuf,
    /// A guard that passes.
    guard: PathBuf,
    /// A guard that fails.
    fail_guard: PathBuf,
    /// Snapshots the serving state into `$VOWP_BANK_PATH`.
    bank: PathBuf,
    /// Deploys the candidate.
    deploy: PathBuf,
    /// Probes the environment.
    probe: PathBuf,
    /// Restores the snapshot passed as `$1`.
    rollback: PathBuf,
}

/// Writes an executable POSIX shell script.
fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script permissions");
}

/// Creates the fake scripts under `root`.
fn scripts(root: &Path) -> Scripts {
    let log = root.join("order.log");
    let guard = root.join("guard.sh");
    script(&guard, &format!("echo guard >> {}", log.display()));
    let fail_guard = root.join("fail-guard.sh");
    script(&fail_guard, &format!("echo guard-failed >> {}\nexit 1", log.display()));
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
        guard,
        fail_guard,
        bank,
        deploy,
        probe,
        rollback,
    }
}

/// Returns the order log lines, or an empty vector when nothing ran.
fn order(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds one environment over `checkout` with the fake scripts.
fn env_config(checkout: &Path, scripts: &Scripts, approval: Approval) -> EnvConfig {
    EnvConfig {
        checkout: checkout.display().to_string(),
        deploy: vec![scripts.deploy.display().to_string()],
        guards: vec![format!("ok:{}", scripts.guard.display())],
        approval,
        serving_bank: Some(format!("{} {{target}}", scripts.bank.display())),
        probe: Some(scripts.probe.display().to_string()),
        rollback: Some(scripts.rollback.display().to_string()),
    }
}

/// Writes `env.config` back to the project's `vowp.toml`.
fn write_config(env: &common::TestEnv) {
    let text = toml::to_string_pretty(&env.config).expect("config serialises");
    fs::write(env.project_path().join("vowp.toml"), text).expect("write vowp.toml");
}

/// Adds `prod` to a fresh test environment, writes the config and returns the scripts.
fn with_prod(env: &mut common::TestEnv, approval: Approval) -> Scripts {
    let scripts = scripts(env.home_path());
    let prod = env_config(env.project_path(), &scripts, approval);
    env.config.env.insert("prod".to_owned(), prod);
    write_config(env);
    scripts
}

/// Runs `git` in `cwd` with a hermetic identity and returns trimmed stdout.
fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "VOWP Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "VOWP Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Commits the configured project as `base`, adds `change.txt` as the candidate, and leaves
/// the checkout on `base` so the candidate is genuinely ahead of the serving copy.
fn repo_with_candidate(env: &common::TestEnv) -> String {
    let root = env.project_path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("change.txt"), "new\n").expect("write candidate change");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "candidate"]);
    let candidate = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", &base]);
    candidate
}

/// Returns the only release of `env_name`, asserting there is exactly one.
fn only_release(env: &common::TestEnv, env_name: &str) -> Release {
    let releases = env
        .store()
        .list_releases(&env.config.project.name, Some(env_name))
        .expect("list releases");
    assert_eq!(releases.len(), 1, "expected one release: {releases:?}");
    releases.into_iter().next().expect("one release")
}

#[test]
fn promote_policy_with_yes_goes_live_and_releases_lists() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Policy);
    let candidate = repo_with_candidate(&env);

    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("promoted"), "{stdout}");
    assert!(stdout.contains("release "), "{stdout}");
    assert_eq!(
        order(&scripts.log),
        vec!["guard", "bank", "deploy", "probe"],
        "the pipeline runs in order"
    );

    let release = only_release(&env, "prod");
    assert_eq!(release.state, ReleaseState::Live);
    assert_eq!(release.change_ref, candidate);
    assert_eq!(release.to_commit.as_deref(), Some(candidate.as_str()));
    let snapshot = release.snapshot_path.as_deref().expect("snapshot path");
    assert!(Path::new(snapshot).exists(), "bank created {snapshot}");
    let rollback = release.rollback_cmd.as_deref().expect("rollback command");
    assert!(rollback.contains(&scripts.rollback.display().to_string()), "{rollback}");

    let (code, stdout, stderr) = env.run(&["releases"]);
    assert_eq!(code, 0, "{stderr}");
    let header = stdout.lines().next().expect("header line");
    assert!(header.contains("ID"), "{stdout}");
    assert!(header.contains("ENV"), "{stdout}");
    assert!(header.contains("CHANGE"), "{stdout}");
    assert!(header.contains("STATE"), "{stdout}");
    assert!(header.contains("FROM→TO"), "{stdout}");
    assert!(header.contains("CREATED"), "{stdout}");
    assert!(stdout.contains("prod"), "{stdout}");
    assert!(stdout.contains("live"), "{stdout}");

    let (code, listed, stderr) = env.run_json(&["releases", "--env", "prod", "--limit", "5"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = listed.as_array().expect("array of releases");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], release.id.as_str());
    assert_eq!(listed[0]["state"], "live");
    assert_eq!(listed[0]["env"], "prod");
    assert_eq!(listed[0]["change_ref"], candidate);

    let (code, listed, stderr) = env.run_json(&["releases", "--env", "staging"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(listed.as_array().expect("array").len(), 0);
}

#[test]
fn promote_human_approval_stages_inbox_item_then_goes_live() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Human);
    let candidate = repo_with_candidate(&env);

    // `--yes` alone never bypasses a human approval.
    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("staged"), "{stdout}");
    let banked = only_release(&env, "prod");
    assert_eq!(banked.state, ReleaseState::Banked);

    let store = env.store();
    let inbox = store.list_inbox(Some(InboxState::Open)).expect("open inbox items");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::Approval);
    assert!(inbox[0].title.contains("Promote"), "{}", inbox[0].title);
    assert!(inbox[0].body.contains(banked.id.as_str()), "{}", inbox[0].body);
    assert!(
        stdout.contains(inbox[0].id.as_str()),
        "promote must print the inbox id: {stdout}"
    );
    assert_eq!(
        order(&scripts.log),
        vec!["guard", "bank"],
        "deploy waits for the human answer"
    );

    store
        .answer_inbox(&inbox[0].id, "approve", "alice")
        .expect("answer the approval");

    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod"]);
    assert_eq!(code, 0, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("promoted"), "{stdout}");
    let live = only_release(&env, "prod");
    assert_eq!(live.id, banked.id, "the banked release is resumed");
    assert_eq!(live.state, ReleaseState::Live);
    assert_eq!(live.approved_by.as_deref(), Some("alice"));
    assert_eq!(
        order(&scripts.log),
        vec!["guard", "bank", "deploy", "probe"],
        "the serving bank runs once"
    );
}

#[test]
fn promote_guard_failure_exits_5_and_records_refused() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Policy);
    let mut prod = env.config.env.get("prod").cloned().expect("prod env");
    prod.guards = vec![format!("blocked:{}", scripts.fail_guard.display())];
    env.config.env.insert("prod".to_owned(), prod);
    write_config(&env);
    let candidate = repo_with_candidate(&env);

    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--yes"]);
    assert_eq!(
        code, 5,
        "a failed guard must exit 5: stdout: {stdout}; stderr: {stderr}"
    );
    assert!(stdout.contains("refused"), "{stdout}");

    let release = only_release(&env, "prod");
    assert_eq!(release.state, ReleaseState::Refused);
    assert!(release.note.contains("guard"), "note: {}", release.note);
    assert_eq!(
        order(&scripts.log),
        vec!["guard-failed"],
        "nothing is banked or deployed after a refusal"
    );

    let (code, listed, stderr) = env.run_json(&["releases"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(listed[0]["state"], "refused");
    assert_eq!(listed[0]["id"], release.id.as_str());
}

#[test]
fn promote_unexpected_diff_exits_5() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Policy);
    let candidate = repo_with_candidate(&env);
    fs::write(env.project_path().join("rogue.txt"), "live drift\n").expect("write drift");

    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--yes"]);
    assert_eq!(code, 5, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("refused"), "{stdout}");
    let release = only_release(&env, "prod");
    assert_eq!(release.state, ReleaseState::Refused);
    assert!(release.note.contains("rogue.txt"), "note: {}", release.note);
    assert_eq!(order(&scripts.log), vec!["guard", "bank"], "the deploy never runs");
}

#[test]
fn promote_dry_run_runs_guards_and_reports_guarded() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Policy);
    let candidate = repo_with_candidate(&env);

    let (code, stdout, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--dry-run", "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("dry run"), "{stdout}");
    let release = only_release(&env, "prod");
    assert_eq!(release.state, ReleaseState::Guarded);
    assert_eq!(order(&scripts.log), vec!["guard"], "a dry run never banks or deploys");
    assert!(
        !env.home_path().join("releases.log").exists(),
        "a dry run is not a terminal release"
    );
}

#[test]
fn rollback_resolves_prefix_and_runs_banked_command() {
    let mut env = common::TestEnv::new();
    let scripts = with_prod(&mut env, Approval::Policy);
    let candidate = repo_with_candidate(&env);

    let (code, _, stderr) = env.run(&["promote", &candidate, "--to", "prod", "--yes"]);
    assert_eq!(code, 0, "{stderr}");
    let live = only_release(&env, "prod");
    assert_eq!(live.state, ReleaseState::Live);
    let prefix = &live.id.as_str()[..8];

    let (code, _, stderr) = env.run(&["rollback", "abc", "--yes"]);
    assert_eq!(code, 2, "a short prefix is invalid input: {stderr}");
    let (code, _, stderr) = env.run(&["rollback", "zzzzzz", "--yes"]);
    assert_eq!(code, 3, "an unknown prefix is not found: {stderr}");

    let (code, stdout, stderr) = env.run(&["rollback", prefix, "--yes"]);
    assert_eq!(code, 0, "stdout: {stdout}; stderr: {stderr}");
    assert!(stdout.contains("rolled back"), "{stdout}");

    let store = env.store();
    let original = store
        .release(&live.id)
        .expect("release lookup")
        .expect("original release");
    assert_eq!(original.state, ReleaseState::RolledBack);
    let rollback_id = original.rolled_back_by.clone().expect("linked rollback release");
    let rollback = store
        .release(&rollback_id)
        .expect("release lookup")
        .expect("rollback release");
    assert_eq!(rollback.state, ReleaseState::RolledBack);
    assert_eq!(rollback.env, "prod");
    assert!(rollback.note.contains("restored"), "note: {}", rollback.note);

    let log = order(&scripts.log);
    assert_eq!(log.len(), 5, "guard, bank, deploy, probe, rollback: {log:?}");
    let last = log.last().expect("rollback line");
    assert!(last.starts_with("rollback "), "{last}");
    assert!(
        last.contains(live.snapshot_path.as_deref().expect("snapshot")),
        "{last}"
    );

    let (code, listed, stderr) = env.run_json(&["releases", "--env", "prod"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = listed.as_array().expect("array");
    assert_eq!(listed.len(), 2);
    let states: Vec<&str> = listed.iter().filter_map(|row| row["state"].as_str()).collect();
    assert!(states.iter().all(|state| *state == "rolled_back"), "{states:?}");
}
