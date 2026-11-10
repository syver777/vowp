//! `owns_respected` and `pushed` gate tests against real git repositories.
//! Spec: `docs/design/crates/vowp-core.md` §4 and `docs/design/loops.md` §4.

mod gates_support;

use std::path::Path;

use gates_support::{
    commit_all, ctx, git_ok, init_bare, init_repo, insert_job, insert_project, set_workspace, test_deps, test_store,
    write,
};
use tempfile::tempdir;
use vowp_core::gates::GateRunner;
use vowp_protocol::{Evidence, Gate, Job, JobKind, PushedGate, VowpConfig};

/// Builds a repo with `main` plus one job branch carrying `files`.
fn repo_with_branch(root: &Path, branch: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let repo = root.join("repo");
    init_repo(&repo);
    git_ok(&repo, &["checkout", "-b", branch]);
    for (path, text) in files {
        write(&repo.join(path), text);
    }
    commit_all(&repo, "work");
    repo
}

/// Builds a job and dependencies for an ownership test.
fn own_job(
    root: &Path,
    id: &str,
    owns: &[&str],
    repo: &Path,
    branch: &str,
) -> (vowp_core::Deps, Job, vowp_store::Store) {
    let node = root.join("node");
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, id, JobKind::Build, owns, Vec::new());
    let job = set_workspace(&store, &job, branch, Some(repo));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    (deps, job, store)
}

#[test]
fn owns_respected_pass_and_fail_with_real_git() {
    let tmp = tempdir().unwrap();
    let repo = repo_with_branch(tmp.path(), "job/owns-test", &[("src/lib.rs", "pub fn ok() {}\n")]);
    let (deps, job, store) = own_job(tmp.path(), "owns-test", &["src/**"], &repo, "job/owns-test");
    let runner = GateRunner::new(deps.clone());

    let context = ctx(&deps, &job, &store, &repo, &repo);
    let passed = runner.evaluate(&context, 0, &Gate::OwnsRespected(true)).unwrap();
    assert!(passed.passed, "evidence: {:?}", passed.evidence);
    match &passed.evidence {
        Evidence::OwnsRespected { changed, violations } => {
            assert_eq!(changed, &["src/lib.rs".to_owned()]);
            assert!(violations.is_empty());
        }
        other => panic!("expected owns evidence, got {other:?}"),
    }

    write(&repo.join("outside.txt"), "not mine\n");
    commit_all(&repo, "touch a path outside owns");
    let failed = runner.evaluate(&context, 0, &Gate::OwnsRespected(true)).unwrap();
    assert!(!failed.passed);
    match &failed.evidence {
        Evidence::OwnsRespected { changed, violations } => {
            assert!(changed.iter().any(|path| path == "outside.txt"), "changed: {changed:?}");
            assert_eq!(violations, &["outside.txt".to_owned()]);
        }
        other => panic!("expected owns evidence, got {other:?}"),
    }
}

#[test]
fn owns_respected_allows_always_allowed_cargo_lock() {
    let tmp = tempdir().unwrap();
    let repo = repo_with_branch(tmp.path(), "job/owns-lock", &[("Cargo.lock", "# lock\n")]);
    let (deps, job, store) = own_job(tmp.path(), "owns-lock", &["src/**"], &repo, "job/owns-lock");
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &repo, &repo);

    let result = runner.evaluate(&context, 0, &Gate::OwnsRespected(true)).unwrap();

    assert!(result.passed, "Cargo.lock is always allowed: {:?}", result.evidence);
    match &result.evidence {
        Evidence::OwnsRespected { changed, violations } => {
            assert!(changed.iter().any(|path| path == "Cargo.lock"), "changed: {changed:?}");
            assert!(violations.is_empty());
        }
        other => panic!("expected owns evidence, got {other:?}"),
    }
}

#[test]
fn pushed_gate_against_local_bare_remote() {
    let tmp = tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    init_bare(&bare);
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    git_ok(&repo, &["remote", "add", "origin", &bare.display().to_string()]);
    git_ok(&repo, &["push", "-u", "origin", "main"]);
    git_ok(&repo, &["checkout", "-b", "job/pushed-test"]);
    write(&repo.join("src/lib.rs"), "pub fn work() {}\n");
    commit_all(&repo, "job work");
    git_ok(&repo, &["push", "-u", "origin", "job/pushed-test"]);
    let local_sha = git_ok(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

    let node = tmp.path().join("node");
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "pushed-test", JobKind::Build, &["src/**"], Vec::new());
    let job = set_workspace(&store, &job, "job/pushed-test", Some(&repo));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &repo, &repo);

    let result = runner
        .evaluate(&context, 0, &Gate::Pushed(PushedGate::default()))
        .unwrap();

    assert!(result.passed, "evidence: {:?}", result.evidence);
    match &result.evidence {
        Evidence::Pushed {
            remote,
            branch,
            ls_remote,
            local_sha: evidence_sha,
        } => {
            assert_eq!(remote, "origin");
            assert_eq!(branch, "job/pushed-test");
            assert!(ls_remote.contains(&local_sha), "ls-remote: {ls_remote:?}");
            assert_eq!(evidence_sha.as_deref(), Some(local_sha.as_str()));
        }
        other => panic!("expected pushed evidence, got {other:?}"),
    }
}

#[test]
fn pushed_gate_fails_when_branch_absent() {
    let tmp = tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    init_bare(&bare);
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    git_ok(&repo, &["remote", "add", "origin", &bare.display().to_string()]);
    git_ok(&repo, &["push", "-u", "origin", "main"]);

    let node = tmp.path().join("node");
    let store = test_store(&node);
    let project = insert_project(&store);
    let job = insert_job(&store, &project, "pushed-absent", JobKind::Build, &[], Vec::new());
    let job = set_workspace(&store, &job, "job/pushed-absent", Some(&repo));
    let deps = test_deps(&node, VowpConfig::with_defaults_for_tests());
    let runner = GateRunner::new(deps.clone());
    let context = ctx(&deps, &job, &store, &repo, &repo);

    let gate = Gate::Pushed(PushedGate {
        remote: "origin".to_owned(),
        branch: Some("job/pushed-absent".to_owned()),
    });
    let result = runner.evaluate(&context, 0, &gate).unwrap();

    assert!(!result.passed);
    match &result.evidence {
        Evidence::Pushed {
            ls_remote, local_sha, ..
        } => {
            assert!(
                ls_remote.trim().is_empty(),
                "remote must have no such branch: {ls_remote:?}"
            );
            assert!(local_sha.is_none(), "the local branch must not exist either");
        }
        other => panic!("expected pushed evidence, got {other:?}"),
    }
}
