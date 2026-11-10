//! Shared hermetic helpers for the `promote_*` integration tests.
//!
//! Every test uses a temporary node directory, temporary git repositories with a local
//! identity and fake shell scripts; nothing reads `$HOME`, the network or a real tmux server.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use vowp_core::{AdapterRegistry, CommandRunner, CommandSpec, Deps, FixedRng, GitOps, SystemGit, SystemRunner};
use vowp_protocol::{Approval, EnvConfig, FakeClock, MemoryLog, NodeName, Project, ProjectName, Timestamp, VowpConfig};
use vowp_store::{NodePaths, Store};
use vowp_tmux::fake::FakeWindows;

/// Fixed instant every test clock starts at.
pub fn now() -> Timestamp {
    Timestamp::parse("2026-11-08T19:00:00Z").unwrap()
}

/// Opens a hermetic store whose clock is fixed at [`now`].
pub fn test_store(root: &Path) -> Store {
    Store::open(&NodePaths::new(root))
        .unwrap()
        .with_clock(Arc::new(FakeClock::at(now())))
}

/// Inserts the default test project row required by the release and ledger foreign keys.
pub fn insert_project(store: &Store) -> ProjectName {
    let project = ProjectName::parse("test").unwrap();
    store
        .upsert_project(&Project {
            name: project.clone(),
            path: "/tmp/test-project".to_owned(),
            remote_url: None,
            default_branch: "main".to_owned(),
            config: serde_json::json!({}),
            created_at: now(),
            updated_at: now(),
        })
        .unwrap();
    project
}

/// Builds hermetic dependencies rooted at `root` with real git and real commands.
pub fn test_deps(root: &Path, config: VowpConfig) -> Deps {
    test_deps_with_git(root, config, Arc::new(SystemGit::default()))
}

/// Builds hermetic dependencies with a chosen [`GitOps`] implementation.
pub fn test_deps_with_git(root: &Path, config: VowpConfig, git: Arc<dyn GitOps>) -> Deps {
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(config),
        node: NodeName::parse("local").unwrap(),
        clock: Arc::new(FakeClock::at(now())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(FakeWindows::new()),
        git,
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

/// A test config whose project checkout is `repo`.
pub fn config_for(repo: &Path) -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.path = Some(repo.display().to_string());
    config
}

/// A minimal environment configuration rooted at `checkout`.
pub fn env_config(checkout: &Path) -> EnvConfig {
    EnvConfig {
        checkout: checkout.display().to_string(),
        deploy: Vec::new(),
        guards: Vec::new(),
        approval: Approval::Human,
        serving_bank: None,
        probe: None,
        rollback: None,
    }
}

/// Writes `text` to `path`, creating parent directories.
pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

/// Reads `path` as UTF-8.
pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// Writes an executable POSIX shell script.
pub fn script(path: &Path, body: &str) {
    let text = if body.starts_with("#!") {
        body.to_owned()
    } else {
        format!("#!/bin/sh\n{body}\n")
    };
    write(path, &text);
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

/// Environment that keeps git hermetic: no user or system config, fixed identity.
pub fn git_env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_AUTHOR_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_AUTHOR_EMAIL".to_owned(), "test@example.invalid".to_owned()),
        ("GIT_COMMITTER_NAME".to_owned(), "VOWP Test".to_owned()),
        ("GIT_COMMITTER_EMAIL".to_owned(), "test@example.invalid".to_owned()),
    ]
}

/// Runs git in `cwd` and asserts success, returning merged output.
pub fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let output = SystemRunner
        .run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: git_env(),
            timeout: Duration::from_secs(60),
        })
        .unwrap();
    assert_eq!(output.exit_code, Some(0), "git {args:?} failed: {}", output.text());
    output.text()
}

/// Creates a real repository on `main` with one initial commit.
pub fn init_repo(repo: &Path) {
    fs::create_dir_all(repo).unwrap();
    git_ok(repo, &["init", "-b", "main"]);
    write(&repo.join("README.md"), "base\n");
    git_ok(repo, &["add", "."]);
    git_ok(repo, &["commit", "-m", "initial"]);
}

/// Creates a bare repository to act as `origin`.
pub fn init_bare(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git_ok(path, &["init", "--bare", "-b", "main"]);
}

/// Commits every change in `repo` with `message` and returns the full sha.
pub fn commit_all(repo: &Path, message: &str) -> String {
    git_ok(repo, &["add", "."]);
    git_ok(repo, &["commit", "-m", message]);
    git_ok(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Clones `source` to `dest` and checks out `rev`.
pub fn clone_at(source: &Path, dest: &Path, rev: &str) {
    let parent = dest.parent().unwrap();
    git_ok(
        parent,
        &[
            "clone",
            "-q",
            &source.display().to_string(),
            &dest.display().to_string(),
        ],
    );
    git_ok(dest, &["checkout", "-q", rev]);
}

/// Returns the full sha of `HEAD` in `repo`.
pub fn head(repo: &Path) -> String {
    git_ok(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}
