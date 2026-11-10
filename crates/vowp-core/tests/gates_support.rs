//! Shared hermetic helpers for the `gates_*` integration tests.
//!
//! Every test gets a temporary node directory, a temporary worktree and, where it needs one,
//! a real git repository with a local identity. Nothing reads `$HOME` or the network.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use vowp_core::gates::GateCtx;
use vowp_core::{AdapterRegistry, CommandRunner, CommandSpec, Deps, FixedRng, GitOps, SystemGit, SystemRunner};
use vowp_protocol::{
    FakeClock, Gate, Job, JobId, JobKind, MemoryLog, NewJob, NodeName, Project, ProjectName, Role, Timestamp,
    VowpConfig,
};
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

/// Inserts the default test project row.
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

/// Inserts one job with explicit owns patterns and gates.
pub fn insert_job(
    store: &Store,
    project: &ProjectName,
    id: &str,
    kind: JobKind,
    owns: &[&str],
    gates: Vec<Gate>,
) -> Job {
    let role = match kind {
        JobKind::Verify => Role::Verifier,
        _ => Role::Worker,
    };
    let new = NewJob {
        id: JobId::parse(id).unwrap(),
        title: format!("job {id}"),
        kind,
        role,
        profile: None,
        needs: Vec::new(),
        owns: owns.iter().map(|pattern| (*pattern).to_owned()).collect(),
        reads: Vec::new(),
        gates: Some(gates),
        priority: None,
        loop_: None,
        brief_text: None,
    };
    store.insert_job(project, &new, "test").unwrap()
}

/// Gives a job a branch and optional worktree path, returning the refreshed row.
pub fn set_workspace(store: &Store, job: &Job, branch: &str, worktree: Option<&Path>) -> Job {
    let worktree = worktree.map(|path| path.display().to_string());
    store
        .set_job_workspace(&job.id, Some(branch), worktree.as_deref())
        .unwrap();
    store.require_job(&job.id).unwrap()
}

/// Builds hermetic dependencies rooted at `root` with an injected config and real git.
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

/// Creates and returns `<root>/worktree`.
pub fn worktree(root: &Path) -> PathBuf {
    let path = root.join("worktree");
    fs::create_dir_all(&path).unwrap();
    path
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

/// Runs git and returns its exit code without asserting success.
pub fn git_status(cwd: &Path, args: &[&str]) -> Option<i32> {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    SystemRunner
        .run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: git_env(),
            timeout: Duration::from_secs(60),
        })
        .unwrap()
        .exit_code
}

/// Creates a real repo on `main` with one initial commit.
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

/// Commits every change in `repo` with `message`.
pub fn commit_all(repo: &Path, message: &str) -> String {
    git_ok(repo, &["add", "."]);
    git_ok(repo, &["commit", "-m", message]);
    git_ok(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Builds a gate context borrowing the test's dependencies, job and store.
pub fn ctx<'a>(deps: &'a Deps, job: &'a Job, store: &'a Store, worktree: &'a Path, repo: &'a Path) -> GateCtx<'a> {
    GateCtx {
        deps,
        job,
        run_id: None,
        worktree,
        repo,
        store,
        attempt: job.attempts,
    }
}
