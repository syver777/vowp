//! Worktree, path-map and result-return tests for the Mac-side agent.
//!
//! Every test uses temporary git repositories: a temporary bare origin, a temporary clone, a
//! temporary worktree root and real git. No network, real home directory, harness or tmux
//! server is touched.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use vowp_agent::{
    GitOutput, GitRunner, PathMap, SystemGit, Worktree, WorktreeError, WorktreeManager, WorktreeSpec,
    strip_remote_credentials,
};
use vowp_protocol::{AgentPathMap, JobId, PathRef, ProjectName};

const PROJECT: &str = "vowp";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git {:?} in {} failed: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git_output(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("run git")
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    origin: PathBuf,
    seed: PathBuf,
    main: PathBuf,
    worktrees: PathBuf,
    base: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = fs::canonicalize(dir.path()).expect("canonical temp root");
        let origin = root.join("origin.git");
        git(&root, &["init", "-q", "--bare", origin.to_str().expect("origin")]);
        let seed = root.join("seed");
        git(&root, &["init", "-q", "-b", "main", seed.to_str().expect("seed")]);
        git(&seed, &["config", "user.email", "test@example.invalid"]);
        git(&seed, &["config", "user.name", "VOWP Test"]);
        fs::write(seed.join("a.txt"), "base\n").expect("write base file");
        git(&seed, &["add", "a.txt"]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        let base = git(&seed, &["rev-parse", "HEAD"]).trim().to_owned();
        git(
            &seed,
            &[
                "push",
                "-q",
                origin.to_str().expect("origin"),
                "refs/heads/main:refs/heads/main",
            ],
        );
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        let main = root.join("main");
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().expect("origin"),
                main.to_str().expect("main"),
            ],
        );
        git(&main, &["config", "user.email", "test@example.invalid"]);
        git(&main, &["config", "user.name", "VOWP Test"]);
        let worktrees = root.join("worktrees");
        fs::create_dir_all(&worktrees).expect("worktree root");
        // The checkout shape the agent supports, independent of the runner's umask: group and
        // world may read and execute, and the private worktree root is 0700.
        fs::set_permissions(&main, fs::Permissions::from_mode(0o755)).expect("main mode");
        fs::set_permissions(main.join(".git"), fs::Permissions::from_mode(0o755)).expect("git dir mode");
        fs::set_permissions(&worktrees, fs::Permissions::from_mode(0o700)).expect("worktree root mode");
        Self {
            _dir: dir,
            root,
            origin,
            seed,
            main,
            worktrees,
            base,
        }
    }

    fn project(&self) -> ProjectName {
        ProjectName::parse(PROJECT).expect("project")
    }

    fn job(&self, name: &str) -> JobId {
        JobId::parse(name).expect("job id")
    }

    fn map(&self) -> AgentPathMap {
        AgentPathMap {
            project: self.project(),
            local_root: self.main.to_str().expect("main path").to_owned(),
            worktree_root: Some(self.worktrees.to_str().expect("worktree path").to_owned()),
            allow: Vec::new(),
        }
    }

    fn path_map(&self) -> PathMap {
        PathMap::new(&self.map()).expect("path map")
    }

    fn spec(&self, job: &JobId) -> WorktreeSpec {
        self.spec_at(job, &self.base)
    }

    fn spec_at(&self, job: &JobId, base: &str) -> WorktreeSpec {
        WorktreeSpec::new(job.clone(), base.to_owned()).with_reserve(0)
    }

    fn manager(&self) -> WorktreeManager {
        WorktreeManager::new(self.path_map())
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        git(cwd, args)
    }

    fn head(&self, repo: &Path) -> String {
        self.git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn status(&self, repo: &Path) -> String {
        self.git(repo, &["status", "--porcelain"])
    }

    fn ref_head(&self, repo: &Path, reference: &str) -> Option<String> {
        let output = git_output(repo, &["rev-parse", "--verify", "--quiet", reference]);
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn branch_head(&self, branch: &str) -> Option<String> {
        self.ref_head(&self.main, &format!("refs/heads/{branch}"))
    }

    fn origin_branch_head(&self, branch: &str) -> Option<String> {
        self.ref_head(&self.origin, &format!("refs/heads/{branch}"))
    }

    fn commit_and_push_to_origin(&self, name: &str, contents: &str) -> String {
        fs::write(self.seed.join(name), contents).expect("write seed file");
        self.git(&self.seed, &["add", name]);
        self.git(&self.seed, &["commit", "-q", "-m", "newer"]);
        let head = self.head(&self.seed);
        self.git(
            &self.seed,
            &[
                "push",
                "-q",
                self.origin.to_str().expect("origin"),
                "refs/heads/main:refs/heads/main",
            ],
        );
        head
    }

    fn add_file(&self, worktree: &Worktree, name: &str, contents: &str) {
        fs::write(worktree.path().join(name), contents).expect("write worktree file");
        self.git(worktree.path(), &["add", name]);
        self.git(worktree.path(), &["commit", "-q", "-m", "work"]);
    }
}

#[derive(Default)]
struct RecordingGit {
    inner: SystemGit,
    calls: Mutex<Vec<Vec<String>>>,
}

impl RecordingGit {
    fn worktree_adds(&self) -> usize {
        self.calls
            .lock()
            .expect("recording lock")
            .iter()
            .filter(|args| args.len() >= 2 && args[0] == "worktree" && args[1] == "add")
            .count()
    }
}

impl GitRunner for RecordingGit {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError> {
        self.calls
            .lock()
            .expect("recording lock")
            .push(args.iter().map(|arg| (*arg).to_owned()).collect());
        self.inner.run(cwd, args)
    }
}

#[test]
fn worktree_is_created_from_the_base_commit_without_touching_the_main_checkout() {
    let fixture = Fixture::new();
    let job = fixture.job("job-1");
    let manager = fixture.manager();

    // Move the main checkout past the base without moving the base object.
    fs::write(fixture.main.join("b.txt"), "newer\n").expect("write newer file");
    fixture.git(&fixture.main, &["add", "b.txt"]);
    fixture.git(&fixture.main, &["commit", "-q", "-m", "newer"]);
    let newer = fixture.head(&fixture.main);
    assert_ne!(newer, fixture.base);
    let status_before = fixture.status(&fixture.main);
    assert!(status_before.trim().is_empty());

    let worktree = manager.create(&fixture.spec(&job)).expect("create");
    assert_eq!(worktree.path(), fixture.worktrees.join(job.as_str()));
    assert_eq!(worktree.branch(), "job/job-1");
    assert_eq!(worktree.head_commit().expect("head"), fixture.base);
    assert!(worktree.is_clean().expect("clean"));
    assert_eq!(fixture.branch_head("job/job-1").as_deref(), Some(fixture.base.as_str()));
    assert_eq!(
        fs::read_to_string(worktree.path().join("a.txt")).expect("base file"),
        "base\n"
    );
    assert!(!worktree.path().join("b.txt").exists());

    // The main checkout still has its newer commit, its only worktree is the job's, and
    // its working tree is untouched.
    assert_eq!(fixture.head(&fixture.main), newer);
    assert_eq!(fixture.status(&fixture.main), status_before);
    assert!(fixture.main.join("b.txt").exists());
    let list = fixture.git(&fixture.main, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains(worktree.path().to_str().expect("worktree path")),
        "worktree list: {list}"
    );
}

#[test]
fn worktree_leaves_a_dirty_main_checkout_untouched() {
    let fixture = Fixture::new();
    let job = fixture.job("job-2");
    fs::write(fixture.main.join("a.txt"), "dirty working tree\n").expect("dirty tracked file");
    fs::write(fixture.main.join("untracked.txt"), "keep me\n").expect("dirty untracked file");
    let status_before = fixture.status(&fixture.main);
    assert!(status_before.contains("a.txt"));
    assert!(status_before.contains("untracked.txt"));
    let head_before = fixture.head(&fixture.main);

    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");

    assert_eq!(fixture.status(&fixture.main), status_before);
    assert_eq!(fixture.head(&fixture.main), head_before);
    assert_eq!(
        fs::read_to_string(fixture.main.join("a.txt")).expect("dirty file kept"),
        "dirty working tree\n"
    );
    assert!(fixture.main.join("untracked.txt").exists());
    // The worktree came from the base commit, not the dirty working tree.
    assert_eq!(
        fs::read_to_string(worktree.path().join("a.txt")).expect("base file"),
        "base\n"
    );
    assert!(!worktree.path().join("untracked.txt").exists());
}

#[test]
fn worktree_reuses_a_clean_branch_at_the_expected_base_and_recreates_a_dirty_one() {
    let fixture = Fixture::new();
    let job = fixture.job("job-3");
    let recorder = Arc::new(RecordingGit::default());
    let manager = WorktreeManager::with_git(fixture.path_map(), recorder.clone());
    let spec = fixture.spec(&job);

    let first = manager.create(&spec).expect("first create");
    assert_eq!(recorder.worktree_adds(), 1);

    let reused = manager.create(&spec).expect("clean reuse");
    assert_eq!(
        recorder.worktree_adds(),
        1,
        "a clean worktree at the base must be reused"
    );
    assert_eq!(reused.path(), first.path());
    assert_eq!(reused.head_commit().expect("head"), fixture.base);
    assert!(reused.is_clean().expect("clean"));

    fs::write(reused.path().join("dirty.txt"), "dirty\n").expect("dirty worktree file");
    let recreated = manager.create(&spec).expect("recreate dirty");
    assert_eq!(recorder.worktree_adds(), 2, "a dirty worktree must be recreated");
    assert!(!recreated.path().join("dirty.txt").exists());
    assert_eq!(recreated.head_commit().expect("head"), fixture.base);
    assert!(recreated.is_clean().expect("clean"));

    fixture.git(recreated.path(), &["commit", "--allow-empty", "-q", "-m", "stale"]);
    let stale = fixture.head(recreated.path());
    assert_ne!(stale, fixture.base);
    let reset = manager.create(&spec).expect("recreate stale branch");
    assert_eq!(
        recorder.worktree_adds(),
        3,
        "a branch head away from the base must be recreated"
    );
    assert_eq!(reset.head_commit().expect("head"), fixture.base);
    assert!(reset.is_clean().expect("clean"));
}

#[test]
fn worktree_cleanup_removes_only_the_vowp_worktree_and_branch() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let job = fixture.job("job-4");
    let worktree = manager.create(&fixture.spec(&job)).expect("create");
    let worktree_path = worktree.path().to_path_buf();
    assert!(worktree_path.exists());

    // Things VOWP must not touch.
    let keep_dir = fixture.worktrees.join("keep");
    fs::create_dir_all(&keep_dir).expect("keep dir");
    fs::write(keep_dir.join("file.txt"), "keep\n").expect("keep file");
    fixture.git(&fixture.main, &["branch", "keep-branch", &fixture.base]);
    let main_head = fixture.head(&fixture.main);

    worktree.cleanup().expect("cleanup");
    assert!(!worktree_path.exists());
    assert!(fixture.branch_head("job/job-4").is_none());
    assert!(keep_dir.join("file.txt").exists());
    assert_eq!(
        fixture.branch_head("keep-branch").as_deref(),
        Some(fixture.base.as_str())
    );
    assert_eq!(fixture.head(&fixture.main), main_head);

    // Cleanup is idempotent.
    manager.cleanup(&job).expect("idempotent cleanup");

    // A plain directory and a branch VOWP did not create are refused and kept.
    let foreign = fixture.job("job-foreign");
    let foreign_path = fixture.worktrees.join(foreign.as_str());
    fs::create_dir_all(&foreign_path).expect("foreign dir");
    fs::write(foreign_path.join("owner.txt"), "keep\n").expect("foreign file");
    let foreign_branch = format!("job/{foreign}");
    fixture.git(&fixture.main, &["branch", &foreign_branch, &fixture.base]);
    let error = manager
        .cleanup(&foreign)
        .expect_err("foreign directory must be refused");
    assert!(matches!(error, WorktreeError::Refused { .. }), "{error}");
    assert!(foreign_path.join("owner.txt").exists());
    assert!(fixture.branch_head(&foreign_branch).is_some());

    // A registered worktree created by the owner, without VOWP's ownership record, is also
    // refused and kept.
    let owned = fixture.job("job-owner-worktree");
    let owned_path = fixture.worktrees.join(owned.as_str());
    let owned_branch = format!("job/{owned}");
    fixture.git(
        &fixture.main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &owned_branch,
            owned_path.to_str().expect("owned path"),
            &fixture.base,
        ],
    );
    let error = manager.cleanup(&owned).expect_err("owner worktree must be refused");
    assert!(matches!(error, WorktreeError::Refused { .. }), "{error}");
    assert!(owned_path.exists());
    assert!(fixture.branch_head(&owned_branch).is_some());

    // Put the temporary checkout back in order for the fixture's drop.
    fixture.git(
        &fixture.main,
        &[
            "worktree",
            "remove",
            "--force",
            owned_path.to_str().expect("owned path"),
        ],
    );
    fixture.git(&fixture.main, &["branch", "-D", &owned_branch]);
    fixture.git(&fixture.main, &["branch", "-D", &foreign_branch]);
}

#[test]
fn worktree_path_ref_stays_inside_the_worktree_root() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let job = fixture.job("job-5");
    let map = fixture.path_map();

    let resolved = map
        .resolve(
            &project,
            &PathRef::Worktree {
                project: project.clone(),
                job: job.clone(),
            },
        )
        .expect("resolve before create");
    assert_eq!(resolved, fixture.worktrees.join(job.as_str()));
    assert!(resolved.starts_with(fixture.worktrees.as_path()));

    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    assert_eq!(
        map.resolve(
            &project,
            &PathRef::Worktree {
                project: project.clone(),
                job: job.clone(),
            },
        )
        .expect("resolve after create"),
        worktree.path()
    );

    // A symlink at the job path that resolves outside the worktree root is refused.
    let escape = fixture.job("job-escape");
    let outside = fixture.root.join("outside-worktrees");
    fs::create_dir_all(&outside).expect("outside dir");
    symlink(&outside, fixture.worktrees.join(escape.as_str())).expect("escape symlink");
    let error = map
        .resolve(
            &project,
            &PathRef::Worktree {
                project: project.clone(),
                job: escape.clone(),
            },
        )
        .expect_err("symlink escape");
    assert!(matches!(error, WorktreeError::SymlinkEscape { .. }), "{error}");
    fs::remove_file(fixture.worktrees.join(escape.as_str())).expect("remove symlink");

    // A ref for another project is refused even when the map is for this project.
    let docs = ProjectName::parse("docs").expect("docs project");
    let error = map
        .resolve(&project, &PathRef::Worktree { project: docs, job })
        .expect_err("project mismatch");
    assert!(matches!(error, WorktreeError::ProjectMismatch { .. }), "{error}");
}

#[test]
fn path_map_rejects_traversal_a_symlink_escape_and_a_case_only_escape() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let map = fixture.path_map();
    fs::create_dir_all(fixture.main.join("src")).expect("src dir");
    fs::write(fixture.main.join("src/file.txt"), "ok\n").expect("src file");
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("data.txt"), "outside\n").expect("outside file");
    symlink(&outside, fixture.main.join("link")).expect("escape symlink");

    let traversal = PathRef::Subdir {
        project: project.clone(),
        rel: "../outside/data.txt".to_owned(),
    };
    let error = map.resolve(&project, &traversal).expect_err("traversal");
    assert!(matches!(error, WorktreeError::InvalidPathRef { .. }), "{error}");

    for rel in ["/etc/passwd", "a\0b", "a\\b", "a//b", "./a", "a/../b", "a/"] {
        let path_ref = PathRef::Subdir {
            project: project.clone(),
            rel: rel.to_owned(),
        };
        let error = map.resolve(&project, &path_ref).expect_err(rel);
        assert!(matches!(error, WorktreeError::InvalidPathRef { .. }), "{rel}: {error}");
    }

    let escaped = PathRef::Subdir {
        project: project.clone(),
        rel: "link/data.txt".to_owned(),
    };
    let error = map.resolve(&project, &escaped).expect_err("symlink escape");
    assert!(matches!(error, WorktreeError::SymlinkEscape { .. }), "{error}");

    // A component that matches an on-disk directory only by case is refused.
    fs::create_dir_all(fixture.main.join("CaseDir")).expect("case dir");
    fs::write(fixture.main.join("CaseDir/file.txt"), "ok\n").expect("case file");
    if fs::canonicalize(fixture.main.join("casedir")).is_ok() {
        // The real case-insensitive filesystem lets the wrong-case spelling resolve.
        let path_ref = PathRef::Subdir {
            project: project.clone(),
            rel: "casedir/file.txt".to_owned(),
        };
        let error = map.resolve(&project, &path_ref).expect_err("case-only escape");
        assert!(matches!(error, WorktreeError::CaseOnlyEscape { .. }), "{error}");
    } else {
        // A case-sensitive temporary filesystem cannot hold both spellings, so a symlink with
        // a case-only different name produces the same requested/canonical component pair.
        eprintln!(
            "note: temporary filesystem is case-sensitive; the SRC symlink below asserts the \
             same case-only rule"
        );
        assert!(fs::canonicalize(fixture.main.join("casedir")).is_err());
        symlink("src", fixture.main.join("SRC")).expect("case-only symlink");
        let case_only = PathRef::Subdir {
            project: project.clone(),
            rel: "SRC/file.txt".to_owned(),
        };
        let error = map.resolve(&project, &case_only).expect_err("case-only alias");
        assert!(matches!(error, WorktreeError::CaseOnlyEscape { .. }), "{error}");
    }
}

#[test]
fn worktree_refuses_a_remote_that_does_not_match_the_project() {
    let fixture = Fixture::new();
    let job = fixture.job("job-6");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");
    let head = worktree.head_commit().expect("head");

    let fork = fixture.root.join("fork.git");
    fixture.git(&fixture.root, &["init", "-q", "--bare", fork.to_str().expect("fork")]);
    fixture.git(
        &fixture.main,
        &["remote", "set-url", "origin", fork.to_str().expect("fork")],
    );

    let error = worktree
        .push_job(fixture.origin.to_str().expect("origin"))
        .expect_err("fork must be refused");
    match error {
        WorktreeError::RemoteMismatch { expected, actual } => {
            assert_eq!(expected, fixture.origin.to_str().expect("origin"));
            assert_eq!(actual, fork.to_str().expect("fork"));
        }
        other => panic!("expected remote_mismatch, got {other}"),
    }
    assert!(
        fixture
            .git(&fork, &["for-each-ref", "--format=%(refname)"])
            .trim()
            .is_empty()
    );
    assert!(
        fixture
            .git(
                &fixture.origin,
                &["for-each-ref", "--format=%(refname)", "refs/heads/job/job-6"],
            )
            .trim()
            .is_empty()
    );
    assert!(worktree.path().exists());
    assert_eq!(worktree.head_commit().expect("head"), head);
    assert_eq!(fixture.branch_head("job/job-6").as_deref(), Some(head.as_str()));

    let credential_url = format!("https://owner:{}@example.com/owner/repo.git", "A".repeat(16));
    assert_eq!(
        strip_remote_credentials(&credential_url),
        "https://example.com/owner/repo.git"
    );
    assert_eq!(
        strip_remote_credentials("git@example.com:owner/repo.git"),
        "example.com:owner/repo.git"
    );
}

#[test]
fn worktree_refuses_a_fork_push_url() {
    let fixture = Fixture::new();
    let job = fixture.job("job-15");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");

    let fork = fixture.root.join("fork.git");
    fixture.git(&fixture.root, &["init", "-q", "--bare", fork.to_str().expect("fork")]);
    let fork_url = fork.to_str().expect("fork");
    // Keep the fetch URL on the project origin and redirect only the push.
    fixture.git(&fixture.main, &["config", "remote.origin.pushurl", fork_url]);
    assert_eq!(
        fixture
            .git(&fixture.main, &["remote", "get-url", "--push", "origin"])
            .trim(),
        fork_url
    );

    let error = worktree
        .push_job(fixture.origin.to_str().expect("origin"))
        .expect_err("a fork push url must be refused");
    match error {
        WorktreeError::RemoteMismatch { expected, actual } => {
            assert_eq!(expected, fixture.origin.to_str().expect("origin"));
            assert_eq!(actual, fork_url);
        }
        other => panic!("expected remote_mismatch, got {other}"),
    }
    assert!(
        fixture
            .git(&fork, &["for-each-ref", "--format=%(refname)"])
            .trim()
            .is_empty(),
        "the fork must not receive the job branch"
    );
    assert!(
        fixture
            .git(
                &fixture.origin,
                &["for-each-ref", "--format=%(refname)", "refs/heads/job/job-15"],
            )
            .trim()
            .is_empty(),
        "the project origin must not receive the job branch"
    );
    assert!(worktree.path().exists());
}

#[test]
fn worktree_refuses_a_push_instead_of_rewrite() {
    let fixture = Fixture::new();
    let job = fixture.job("job-16");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");

    let fork = fixture.root.join("fork.git");
    fixture.git(&fixture.root, &["init", "-q", "--bare", fork.to_str().expect("fork")]);
    let fork_url = fork.to_str().expect("fork");
    let origin_url = fixture.origin.to_str().expect("origin");
    // `git push origin` rewrites the origin URL to the fork through pushInsteadOf.
    let rewrite = format!("url.{fork_url}.pushInsteadOf");
    fixture.git(&fixture.main, &["config", &rewrite, origin_url]);
    assert_eq!(
        fixture
            .git(&fixture.main, &["remote", "get-url", "--push", "origin"])
            .trim(),
        fork_url
    );

    let error = worktree
        .push_job(origin_url)
        .expect_err("a pushInsteadOf rewrite must be refused");
    match error {
        WorktreeError::RemoteMismatch { expected, actual } => {
            assert_eq!(expected, origin_url);
            assert_eq!(actual, fork_url);
        }
        other => panic!("expected remote_mismatch, got {other}"),
    }
    assert!(
        fixture
            .git(&fork, &["for-each-ref", "--format=%(refname)"])
            .trim()
            .is_empty(),
        "the rewritten push destination must not receive the job branch"
    );
    assert!(
        fixture
            .git(
                &fixture.origin,
                &["for-each-ref", "--format=%(refname)", "refs/heads/job/job-16"],
            )
            .trim()
            .is_empty(),
        "the project origin must not receive the job branch"
    );
    assert!(worktree.path().exists());
}

#[test]
fn worktree_remote_mismatch_strips_credentials_from_the_actual_url() {
    let fixture = Fixture::new();
    let job = fixture.job("job-17");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");

    let token = "FAKE-TOKEN-fix-round-1";
    let credential_url = format!("https://owner:{token}@example.invalid/owner/repo.git");
    fixture.git(&fixture.main, &["remote", "set-url", "origin", &credential_url]);

    let expected = "https://example.invalid/owner/other.git";
    let error = worktree
        .push_job(expected)
        .expect_err("a different remote must be refused");
    let display = error.to_string();
    assert!(
        !display.contains(token),
        "the error display leaked credentials: {display}"
    );
    match error {
        WorktreeError::RemoteMismatch {
            expected: expected_url,
            actual,
        } => {
            assert_eq!(expected_url, expected);
            assert_eq!(actual, "https://example.invalid/owner/repo.git");
            assert!(!actual.contains(token), "the actual url leaked credentials: {actual}");
        }
        other => panic!("expected remote_mismatch, got {other}"),
    }
}

#[test]
fn worktree_push_sends_only_the_job_branch() {
    let fixture = Fixture::new();
    let job = fixture.job("job-7");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");
    fixture.git(worktree.path(), &["tag", "v1"]);
    let head = worktree.head_commit().expect("head");

    let pushed = worktree
        .push_job(fixture.origin.to_str().expect("origin"))
        .expect("push");
    assert_eq!(pushed.branch, "job/job-7");
    assert_eq!(pushed.remote, "origin");
    assert_eq!(pushed.head_commit, head);
    assert_eq!(fixture.origin_branch_head("job/job-7").as_deref(), Some(head.as_str()));
    assert_eq!(
        fixture.origin_branch_head("main").as_deref(),
        Some(fixture.base.as_str())
    );

    let mut refs: Vec<String> = fixture
        .git(&fixture.origin, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    refs.sort();
    assert_eq!(refs, vec!["refs/heads/job/job-7", "refs/heads/main"]);
    assert!(fixture.git(&fixture.origin, &["tag", "-l"]).trim().is_empty());
}

#[test]
fn worktree_push_fails_cleanly_and_keeps_the_worktree() {
    let fixture = Fixture::new();
    let job = fixture.job("job-8");
    let worktree = fixture.manager().create(&fixture.spec(&job)).expect("create");
    fixture.add_file(&worktree, "change.txt", "change\n");
    let head = worktree.head_commit().expect("head");
    let hook = fixture.origin.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\necho 'vowp test hook rejects the push' >&2\nexit 1\n").expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("hook mode");

    let error = worktree
        .push_job(fixture.origin.to_str().expect("origin"))
        .expect_err("the pre-receive hook must reject the push");
    match error {
        WorktreeError::GitPushFailed { command, stderr } => {
            assert!(stderr.contains("vowp test hook rejects the push"), "stderr: {stderr}");
            assert!(command.contains("refs/heads/job/job-8"), "command: {command}");
        }
        other => panic!("expected git_push_failed, got {other}"),
    }
    assert!(worktree.path().exists());
    assert_eq!(worktree.head_commit().expect("head"), head);
    assert_eq!(fixture.branch_head("job/job-8").as_deref(), Some(head.as_str()));
    assert!(
        fixture
            .git(
                &fixture.origin,
                &["for-each-ref", "--format=%(refname)", "refs/heads/job/job-8"],
            )
            .trim()
            .is_empty()
    );
}

#[test]
fn worktree_refuses_a_create_below_the_disk_reserve() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let job = fixture.job("job-9");
    let worktree = manager.create(&fixture.spec(&job)).expect("create");

    let low = fixture.spec(&job).with_reserve(u64::MAX);
    let error = manager.create(&low).expect_err("disk low must refuse");
    match error {
        WorktreeError::DiskLow { reserve, .. } => assert_eq!(reserve, u64::MAX),
        other => panic!("expected disk_low, got {other}"),
    }
    // Never delete an existing worktree to make room.
    assert!(worktree.path().exists());
    assert!(fixture.branch_head("job/job-9").is_some());
    assert_eq!(worktree.head_commit().expect("head"), fixture.base);

    // A fresh job is refused before its directory or branch exists.
    let fresh = fixture.job("job-10");
    let error = manager
        .create(&fixture.spec(&fresh).with_reserve(u64::MAX))
        .expect_err("disk low must refuse a fresh job");
    assert!(matches!(error, WorktreeError::DiskLow { .. }), "{error}");
    assert!(!fixture.worktrees.join(fresh.as_str()).exists());
    assert!(fixture.branch_head("job/job-10").is_none());
}

#[test]
fn worktree_create_fetches_a_missing_base_from_origin() {
    let fixture = Fixture::new();
    let newer = fixture.commit_and_push_to_origin("b.txt", "newer\n");
    assert!(
        !git_output(&fixture.main, &["cat-file", "-e", &format!("{newer}^{{commit}}")],)
            .status
            .success(),
        "the clone must not have the newer commit before the fetch"
    );

    let job = fixture.job("job-11");
    let worktree = fixture
        .manager()
        .create(&fixture.spec_at(&job, &newer))
        .expect("fetch the missing base");
    assert_eq!(worktree.head_commit().expect("head"), newer);
    assert_eq!(
        fs::read_to_string(worktree.path().join("b.txt")).expect("fetched file"),
        "newer\n"
    );
}

#[test]
fn path_map_resolves_project_worktree_and_subdir_and_enforces_allow() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let job = fixture.job("job-13");
    fs::create_dir_all(fixture.main.join("src")).expect("src dir");
    fs::write(fixture.main.join("src/file.txt"), "ok\n").expect("src file");
    fs::create_dir_all(fixture.main.join("other")).expect("other dir");
    let map = fixture.path_map();

    assert_eq!(
        map.resolve(
            &project,
            &PathRef::Project {
                project: project.clone()
            }
        )
        .expect("project"),
        fixture.main
    );
    assert_eq!(
        map.resolve(
            &project,
            &PathRef::Worktree {
                project: project.clone(),
                job: job.clone(),
            },
        )
        .expect("worktree"),
        fixture.worktrees.join(job.as_str())
    );
    assert_eq!(
        map.resolve(
            &project,
            &PathRef::Subdir {
                project: project.clone(),
                rel: "src".to_owned(),
            },
        )
        .expect("subdir"),
        fs::canonicalize(fixture.main.join("src")).expect("canonical src")
    );
    let error = map
        .resolve(
            &project,
            &PathRef::Subdir {
                project: project.clone(),
                rel: "src/file.txt".to_owned(),
            },
        )
        .expect_err("a file is not a subdirectory");
    assert!(matches!(error, WorktreeError::NotADirectory { .. }), "{error}");

    let mut allowed_map = fixture.map();
    allowed_map.allow = vec!["src".to_owned()];
    let allowed = PathMap::new(&allowed_map).expect("allowed map");
    assert!(
        allowed
            .resolve(
                &project,
                &PathRef::Subdir {
                    project: project.clone(),
                    rel: "src".to_owned(),
                },
            )
            .is_ok()
    );
    let error = allowed
        .resolve(
            &project,
            &PathRef::Subdir {
                project: project.clone(),
                rel: "other".to_owned(),
            },
        )
        .expect_err("outside allow");
    assert!(matches!(error, WorktreeError::OutsideAllow { .. }), "{error}");

    let docs = ProjectName::parse("docs").expect("docs");
    let error = map
        .resolve(&project, &PathRef::Project { project: docs })
        .expect_err("project mismatch");
    assert!(matches!(error, WorktreeError::ProjectMismatch { .. }), "{error}");

    // An omitted worktree root defaults to <local_root>/.vowp/worktrees.
    let default_root = fixture.main.join(".vowp").join("worktrees");
    fs::create_dir_all(&default_root).expect("default worktree root");
    fs::set_permissions(&default_root, fs::Permissions::from_mode(0o755)).expect("default root mode");
    let mut map_with_default = fixture.map();
    map_with_default.worktree_root = None;
    let default_map = PathMap::new(&map_with_default).expect("default worktree root map");
    assert_eq!(
        default_map.worktree_root(),
        fs::canonicalize(&default_root).expect("canonical default root")
    );
}

#[test]
fn path_map_reports_missing_and_unwritable_roots() {
    let fixture = Fixture::new();
    let project = fixture.project();

    let missing = fixture.root.join("missing-checkout");
    let map = AgentPathMap {
        project: project.clone(),
        local_root: missing.to_str().expect("missing").to_owned(),
        worktree_root: Some(fixture.worktrees.to_str().expect("worktrees").to_owned()),
        allow: Vec::new(),
    };
    let error = PathMap::new(&map).expect_err("missing local root");
    assert!(matches!(error, WorktreeError::PathMissing { .. }), "{error}");

    let missing_worktrees = fixture.root.join("missing-worktrees");
    let map = AgentPathMap {
        project: project.clone(),
        local_root: fixture.main.to_str().expect("main").to_owned(),
        worktree_root: Some(missing_worktrees.to_str().expect("missing").to_owned()),
        allow: Vec::new(),
    };
    let error = PathMap::new(&map).expect_err("missing worktree root");
    assert!(matches!(error, WorktreeError::PathMissing { .. }), "{error}");

    let map = AgentPathMap {
        project: project.clone(),
        local_root: fixture.main.to_str().expect("main").to_owned(),
        worktree_root: None,
        allow: Vec::new(),
    };
    let error = PathMap::new(&map).expect_err("missing default worktree root");
    assert!(matches!(error, WorktreeError::PathMissing { .. }), "{error}");

    let read_only = fixture.root.join("read-only-worktrees");
    fs::create_dir_all(&read_only).expect("read-only dir");
    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o555)).expect("read-only mode");
    let map = AgentPathMap {
        project: project.clone(),
        local_root: fixture.main.to_str().expect("main").to_owned(),
        worktree_root: Some(read_only.to_str().expect("read-only").to_owned()),
        allow: Vec::new(),
    };
    let error = PathMap::new(&map).expect_err("read-only worktree root");
    assert!(matches!(error, WorktreeError::PathNotWritable { .. }), "{error}");
    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o755)).expect("restore mode");

    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).expect("plain dir");
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o755)).expect("plain mode");
    let map = AgentPathMap {
        project,
        local_root: plain.to_str().expect("plain").to_owned(),
        worktree_root: Some(fixture.worktrees.to_str().expect("worktrees").to_owned()),
        allow: Vec::new(),
    };
    let manager = WorktreeManager::new(PathMap::new(&map).expect("plain map"));
    let error = manager
        .create(&WorktreeSpec::new(fixture.job("job-12"), fixture.base.clone()).with_reserve(0))
        .expect_err("plain directory is not a checkout");
    assert!(matches!(error, WorktreeError::NotAGitCheckout { .. }), "{error}");
}

#[test]
fn worktree_create_rejects_invalid_commit_and_remote() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let job = fixture.job("job-14");

    let error = manager
        .create(&WorktreeSpec::new(job.clone(), "HEAD").with_reserve(0))
        .expect_err("short ref must be refused");
    assert!(matches!(error, WorktreeError::InvalidCommit { .. }), "{error}");

    let error = manager
        .create(&fixture.spec(&job).with_remote("--upload-pack=evil"))
        .expect_err("option-like remote must be refused");
    assert!(matches!(error, WorktreeError::InvalidRemote { .. }), "{error}");
}

#[test]
fn path_map_refuses_a_group_writable_checkout() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.main, fs::Permissions::from_mode(0o775)).expect("group-writable mode");

    let error = PathMap::new(&fixture.map()).expect_err("a group-writable checkout must be refused");
    match &error {
        WorktreeError::UnsafeCheckout { path, detail } => {
            assert_eq!(path, &fixture.main);
            assert!(
                detail.contains("group") || detail.contains("world"),
                "the detail must name the writable class: {detail}"
            );
        }
        other => panic!("expected unsafe_checkout, got {other}"),
    }
    let message = error.to_string();
    assert!(
        message.contains(fixture.main.to_str().expect("main path")),
        "the message must name the checkout: {message}"
    );
    assert!(
        message.contains("owned by the current user and not group/world-writable"),
        "the message must name the expected condition: {message}"
    );
}

#[test]
fn path_map_refuses_a_world_writable_worktree_root() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.worktrees, fs::Permissions::from_mode(0o707)).expect("world-writable mode");

    let error = PathMap::new(&fixture.map()).expect_err("a world-writable worktree root must be refused");
    match &error {
        WorktreeError::UnsafeCheckout { path, .. } => assert_eq!(path, &fixture.worktrees),
        other => panic!("expected unsafe_checkout, got {other}"),
    }
    let message = error.to_string();
    assert!(
        message.contains(fixture.worktrees.to_str().expect("worktree path")),
        "the message must name the worktree root: {message}"
    );
    assert!(
        message.contains("owned by the current user and not group/world-writable"),
        "the message must name the expected condition: {message}"
    );
}

#[test]
fn path_map_accepts_a_group_readable_private_checkout() {
    let fixture = Fixture::new();
    // 0755 has group/world read and execute but no group/world write; 0700 is the private
    // worktree root. Both are supported.
    fs::set_permissions(&fixture.main, fs::Permissions::from_mode(0o755)).expect("group-readable mode");
    fs::set_permissions(&fixture.worktrees, fs::Permissions::from_mode(0o700)).expect("private mode");

    let map = PathMap::new(&fixture.map()).expect("a group-readable private checkout must be accepted");
    assert_eq!(map.local_root(), fixture.main);
    assert_eq!(map.worktree_root(), fixture.worktrees);
    map.check_private_checkout()
        .expect("the recheck accepts the same shape");
}

#[test]
fn worktree_create_rechecks_the_checkout_before_creating() {
    let fixture = Fixture::new();
    let job = fixture.job("job-18");
    let recorder = Arc::new(RecordingGit::default());
    let manager = WorktreeManager::with_git(fixture.path_map(), recorder.clone());
    // The map was valid when it was built; a later chmod must still be caught at run start.
    fs::set_permissions(&fixture.main, fs::Permissions::from_mode(0o775)).expect("group-writable mode");

    let error = manager
        .create(&fixture.spec(&job))
        .expect_err("a checkout made group-writable after startup must be refused");
    assert!(matches!(error, WorktreeError::UnsafeCheckout { .. }), "{error}");
    assert_eq!(recorder.worktree_adds(), 0, "no worktree may be created");
    assert!(!fixture.worktrees.join(job.as_str()).exists());
    assert!(fixture.branch_head("job/job-18").is_none());
}

#[test]
fn worktree_create_refuses_a_group_writable_git_dir() {
    let fixture = Fixture::new();
    let job = fixture.job("job-19");
    let recorder = Arc::new(RecordingGit::default());
    let manager = WorktreeManager::with_git(fixture.path_map(), recorder.clone());
    let git_dir = fixture.main.join(".git");
    fs::set_permissions(&git_dir, fs::Permissions::from_mode(0o775)).expect("group-writable git dir");

    let error = manager
        .create(&fixture.spec(&job))
        .expect_err("a group-writable git directory must be refused");
    match &error {
        WorktreeError::UnsafeCheckout { path, .. } => assert_eq!(path, &git_dir),
        other => panic!("expected unsafe_checkout, got {other}"),
    }
    assert_eq!(recorder.worktree_adds(), 0, "no worktree may be created");
    assert!(!fixture.worktrees.join(job.as_str()).exists());
    assert!(fixture.branch_head("job/job-19").is_none());
}
