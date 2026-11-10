//! Foundation tests: command runner, git runner, dependency wiring and error mapping.
//! Spec: `docs/design/crates/vowp-core.md` §2.

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use support::fake_adapter::FakeAdapter;
use support::fake_git::FakeGit;
use tempfile::tempdir;
use vowp_core::{
    AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, CoreError, Deps, FixedRng, GitOps, RebaseResult, Rng,
    SystemGit, SystemRunner,
};
use vowp_protocol::{
    AdapterError, ErrorKind, Executor, FakeClock, HarnessConfig, HarnessKind, LimitEvent, LimitKind, MemoryLog,
    NodeName, ProtocolError, ServiceError, StartInput, Timestamp, VowpConfig,
};
use vowp_store::NodePaths;
use vowp_tmux::TmuxError;
use vowp_tmux::fake::FakeWindows;

/// Builds hermetic dependencies rooted at `root`.
fn test_deps(root: &Path) -> Deps {
    Deps {
        paths: NodePaths::new(root),
        config: Arc::new(VowpConfig::with_defaults_for_tests()),
        node: NodeName::parse("local").unwrap(),
        clock: Arc::new(FakeClock::at(Timestamp::parse("2026-03-10T19:00:00Z").unwrap())),
        log: Arc::new(MemoryLog::new()),
        adapters: Arc::new(AdapterRegistry::new()),
        windows: Arc::new(FakeWindows::new()),
        git: Arc::new(FakeGit::new()),
        cmd: Arc::new(SystemRunner),
        rng: Arc::new(FixedRng(0)),
    }
}

/// Returns the hermetic environment for direct `git` setup commands.
fn git_env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
    ]
}

/// Runs `git` in `cwd` and asserts that it succeeded.
fn git_ok(runner: &SystemRunner, cwd: &Path, args: &[&str]) -> String {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    let output = runner
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

/// Runs `git` and returns its exit code without asserting success.
fn git_status(runner: &SystemRunner, cwd: &Path, args: &[&str]) -> Option<i32> {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    runner
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

/// Writes `text` to `path`.
fn write(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
}

/// Reads `path` as UTF-8.
fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn system_runner_captures_output_shell_argv_cwd_and_env() {
    let tmp = tempdir().unwrap();
    let cwd = tmp.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let runner = SystemRunner;

    let shell = runner
        .run(&CommandSpec {
            shell: Some("printf 'out'; printf 'err' >&2; printf \"$VOWP_TEST_ENV\"".to_owned()),
            argv: Vec::new(),
            cwd: Some(cwd.clone()),
            env: vec![("VOWP_TEST_ENV".to_owned(), "42".to_owned())],
            timeout: Duration::from_secs(5),
        })
        .unwrap();
    assert_eq!(shell.exit_code, Some(0));
    assert!(!shell.timed_out);
    let text = shell.text();
    assert!(text.contains("out"), "stdout missing: {text:?}");
    assert!(text.contains("err"), "stderr missing: {text:?}");
    assert!(text.contains("42"), "env missing: {text:?}");

    let argv = runner
        .run(&CommandSpec {
            shell: None,
            argv: vec!["sh".to_owned(), "-c".to_owned(), "pwd".to_owned()],
            cwd: Some(cwd.clone()),
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        })
        .unwrap();
    assert_eq!(argv.exit_code, Some(0));
    let printed = Path::new(argv.text().trim()).to_path_buf();
    assert_eq!(fs::canonicalize(printed).unwrap(), fs::canonicalize(&cwd).unwrap());

    let failed = runner
        .run(&CommandSpec {
            shell: None,
            argv: vec!["sh".to_owned(), "-c".to_owned(), "exit 7".to_owned()],
            cwd: Some(cwd),
            env: Vec::new(),
            timeout: Duration::from_secs(5),
        })
        .unwrap();
    assert_eq!(failed.exit_code, Some(7));
    assert!(!failed.timed_out);

    let empty = runner.run(&CommandSpec {
        shell: None,
        argv: Vec::new(),
        cwd: None,
        env: Vec::new(),
        timeout: Duration::from_secs(1),
    });
    assert!(matches!(empty, Err(CoreError::Command(_))));

    let capped = CommandOutput {
        output: b"a\nb\nc\n".to_vec(),
        ..CommandOutput::default()
    };
    assert_eq!(capped.text(), "a\nb\nc\n");
    assert_eq!(capped.tail_lines(2), "b\nc");
    assert_eq!(capped.tail_lines(0), "");
}

#[test]
fn system_runner_times_out_and_kills_process_group() {
    let tmp = tempdir().unwrap();
    let started = tmp.path().join("started");
    let survived = tmp.path().join("survived");
    let script = format!(
        "printf started > '{}'; (sleep 3; printf survived > '{}') & wait",
        started.display(),
        survived.display()
    );

    let output = SystemRunner
        .run(&CommandSpec {
            shell: Some(script),
            argv: Vec::new(),
            cwd: Some(tmp.path().to_path_buf()),
            env: Vec::new(),
            timeout: Duration::from_secs(1),
        })
        .unwrap();

    assert!(output.timed_out, "runner must report the timeout");
    assert_eq!(output.exit_code, None, "a killed process has no exit code");
    assert!(
        output.duration < Duration::from_secs(5),
        "timeout should be prompt, took {:?}",
        output.duration
    );
    assert!(started.exists(), "the command never started");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !survived.exists(),
        "the background child survived; the process group was not killed"
    );
}

#[test]
fn system_git_roundtrip_in_temp_repo() {
    let tmp = tempdir().unwrap();
    let runner = SystemRunner;
    let origin = tmp.path().join("origin.git");
    let repo = tmp.path().join("repo");

    git_ok(&runner, tmp.path(), &["init", "--bare", origin.to_str().unwrap()]);
    git_ok(&runner, tmp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
    git_ok(&runner, &repo, &["config", "user.name", "VOWP Test"]);
    git_ok(&runner, &repo, &["config", "user.email", "test@example.invalid"]);
    write(&repo.join("file.txt"), "one\n");
    git_ok(&runner, &repo, &["add", "--", "file.txt"]);
    git_ok(&runner, &repo, &["commit", "-m", "initial"]);
    git_ok(&runner, &repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git_ok(&runner, &repo, &["push", "-u", "origin", "main"]);

    let git = SystemGit::new(Arc::new(SystemRunner));
    assert!(git.status_clean(&repo).unwrap());
    let main_sha = git.head_sha(&repo, "main").unwrap();
    assert_eq!(main_sha.len(), 40);
    let remote = git.ls_remote(&repo, "origin", "main").unwrap();
    assert!(remote.contains(&main_sha), "ls-remote: {remote:?}");

    let worktree = tmp.path().join("worktree");
    git.worktree_add(&repo, &worktree, "job/demo", "main").unwrap();
    assert_eq!(read(&worktree.join("file.txt")), "one\n");
    assert_eq!(git.head_sha(&worktree, "HEAD").unwrap(), main_sha);
    // Adding the same worktree twice is idempotent.
    git.worktree_add(&repo, &worktree, "job/demo", "main").unwrap();

    write(&worktree.join("file.txt"), "two\n");
    git.add(&worktree, &["file.txt".to_owned()]).unwrap();
    let short = git
        .commit(&worktree, "demo: change file\n\nAI-Assisted: test via VOWP")
        .unwrap();
    assert!(short.len() >= 7);
    assert!(git.head_sha(&worktree, "HEAD").unwrap().starts_with(&short));
    assert_eq!(
        git.diff_names(&worktree, "main", "HEAD").unwrap(),
        vec!["file.txt".to_owned()]
    );
    assert!(git.diff_text(&worktree, "main", "HEAD").unwrap().contains("+two"));
    assert_eq!(git.commits_between(&worktree, "main", "HEAD").unwrap().len(), 1);
    assert!(git.status_clean(&worktree).unwrap());

    // checkout_path restores a path from a revision; reset_hard throws the staged change away.
    write(&worktree.join("file.txt"), "three\n");
    git.checkout_path(&worktree, "main", "file.txt").unwrap();
    assert_eq!(read(&worktree.join("file.txt")), "one\n");
    git.reset_hard(&worktree, "HEAD").unwrap();
    assert_eq!(read(&worktree.join("file.txt")), "two\n");
    assert!(git.status_clean(&worktree).unwrap());

    // A clean rebase: main only adds an unrelated file.
    write(&repo.join("other.txt"), "other\n");
    git.add(&repo, &["other.txt".to_owned()]).unwrap();
    git.commit(&repo, "main: other").unwrap();
    assert_eq!(git.rebase(&worktree, "main").unwrap(), RebaseResult::Clean);
    assert!(worktree.join("other.txt").exists());
    // Bring main to the rebased branch so the conflict below has a single commit to replay.
    git.merge_ff_only(&repo, "job/demo").unwrap();

    // A conflicting rebase, then abort.
    write(&repo.join("file.txt"), "main\n");
    git.add(&repo, &["file.txt".to_owned()]).unwrap();
    git.commit(&repo, "main: file").unwrap();
    write(&worktree.join("file.txt"), "branch\n");
    git.add(&worktree, &["file.txt".to_owned()]).unwrap();
    git.commit(&worktree, "branch: file").unwrap();
    assert_eq!(
        git.rebase(&worktree, "main").unwrap(),
        RebaseResult::Conflict {
            files: vec!["file.txt".to_owned()]
        }
    );
    assert_eq!(git.conflicted_files(&worktree).unwrap(), vec!["file.txt".to_owned()]);
    git.rebase_abort(&worktree).unwrap();
    assert!(git.status_clean(&worktree).unwrap());

    // Rebase again, resolve the conflict and continue.
    git.rebase(&worktree, "main").unwrap();
    write(&worktree.join("file.txt"), "resolved\n");
    git.add(&worktree, &["file.txt".to_owned()]).unwrap();
    let continued = git.rebase_continue(&worktree).unwrap();
    assert_eq!(continued, RebaseResult::Clean);
    assert!(git.conflicted_files(&worktree).unwrap().is_empty());
    assert_eq!(read(&worktree.join("file.txt")), "resolved\n");

    // Fast-forward main to the job branch, push it, and read it back.
    let branch_sha = git.head_sha(&worktree, "HEAD").unwrap();
    git.merge_ff_only(&repo, "job/demo").unwrap();
    assert_eq!(git.head_sha(&repo, "main").unwrap(), branch_sha);
    git.push(&repo, "origin", "job/demo").unwrap();
    git.fetch(&repo, "origin").unwrap();
    let pushed = git.ls_remote(&repo, "origin", "job/demo").unwrap();
    assert!(pushed.contains(&branch_sha), "pushed ref: {pushed:?}");

    let committed_at = git.last_commit_time(&repo, "HEAD").unwrap().unwrap();
    assert!(committed_at.unix_secs() > 0);

    // Cleanup: remove the worktree, then the merged branch.
    git.worktree_remove(&repo, &worktree).unwrap();
    assert!(!worktree.exists());
    git.worktree_remove(&repo, &worktree).unwrap();
    git.branch_delete(&repo, "job/demo").unwrap();
    assert_eq!(
        git_status(
            &runner,
            &repo,
            &["rev-parse", "--verify", "--quiet", "refs/heads/job/demo"]
        ),
        Some(1)
    );
}

#[test]
fn adapter_registry_lookup_and_fake_injection() {
    let tmp = tempdir().unwrap();
    let mapping = tmp.path().join("codex.toml");
    write(&mapping, "name = \"codex\"\n[command]\nargv = [\"codex\"]\n");
    let mut config = VowpConfig::with_defaults_for_tests();
    config.harnesses.insert(
        "codex".to_owned(),
        HarnessConfig {
            mapping: Some(mapping.to_string_lossy().into_owned()),
            ..HarnessConfig::default()
        },
    );

    let registry = AdapterRegistry::from_config(&config).unwrap();
    assert_eq!(
        registry
            .get(&Executor::parse("claude-code:example-model").unwrap())
            .unwrap()
            .kind(),
        HarnessKind::ClaudeCode
    );
    assert_eq!(
        registry
            .get(&Executor::parse("opencode:example/model").unwrap())
            .unwrap()
            .kind(),
        HarnessKind::Opencode
    );
    let codex = Executor::parse("cli:codex").unwrap();
    assert_eq!(registry.get(&codex).unwrap().kind(), HarnessKind::Cli);
    assert!(registry.get(&Executor::parse("cli:missing").unwrap()).is_none());

    let fake = Arc::new(FakeAdapter::new(HarnessKind::Opencode, Vec::new()));
    let injected = AdapterRegistry::new().with("opencode", fake.clone());
    let executor = Executor::parse("opencode:example/model").unwrap();
    let adapter = injected.get(&executor).unwrap();
    assert_eq!(adapter.kind(), HarnessKind::Opencode);
    adapter
        .start(StartInput {
            session_id: vowp_protocol::SessionId::new(),
            executor: executor.clone(),
            cwd: tmp.path().to_path_buf(),
            prompt: "hello".to_owned(),
            resume: None,
            new_native_id: None,
            env: Vec::new(),
            permission_mode: None,
            system_note: None,
            event_log_hint: None,
            timeout: None,
        })
        .unwrap();
    assert_eq!(fake.starts().len(), 1);
    assert_eq!(fake.starts()[0].prompt, "hello");
}

#[test]
fn deps_open_store_creates_the_node_database() {
    let tmp = tempdir().unwrap();
    let deps = test_deps(tmp.path());
    let store = deps.open_store().unwrap();
    assert_eq!(store.schema_version().unwrap(), 1);
    assert!(tmp.path().join("vowp.db").is_file());
}

#[test]
fn deps_open_store_uses_injected_clock() {
    let tmp = tempdir().unwrap();
    let fake_time = Timestamp::parse("2026-12-24T05:06:07Z").unwrap();
    let mut deps = test_deps(tmp.path());
    deps.clock = Arc::new(FakeClock::at(fake_time));
    let store = deps.open_store().unwrap();

    assert_eq!(store.clock().now(), fake_time);
    store.push_request("wake", &serde_json::json!({})).unwrap();
    let drained = store.take_requests().unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(
        drained[0].created_at, fake_time,
        "store rows must be stamped by the injected clock"
    );
}

#[test]
fn core_error_kind_mapping() {
    assert_eq!(CoreError::NotFound("job".to_owned()).kind(), ErrorKind::NotFound);
    assert_eq!(CoreError::Invalid("x".to_owned()).kind(), ErrorKind::InvalidInput);
    assert_eq!(CoreError::Refused("x".to_owned()).kind(), ErrorKind::Refused);
    assert_eq!(
        CoreError::Guard {
            name: "owned".to_owned(),
            detail: "x".to_owned()
        }
        .kind(),
        ErrorKind::Guard
    );
    assert_eq!(CoreError::Cancelled.kind(), ErrorKind::Conflict);
    assert_eq!(CoreError::Command("x".to_owned()).kind(), ErrorKind::Internal);
    assert_eq!(
        CoreError::Io {
            path: "x".to_owned(),
            source: std::io::Error::other("x")
        }
        .kind(),
        ErrorKind::Io
    );
    assert_eq!(
        CoreError::Git {
            args: vec!["status".to_owned()],
            code: Some(1),
            stderr: "x".to_owned()
        }
        .kind(),
        ErrorKind::Io
    );
    let limit = LimitEvent {
        kind: LimitKind::Quota,
        provider: "claude".to_owned(),
        resets_at: None,
        message: "limited".to_owned(),
        http_status: None,
    };
    assert_eq!(CoreError::Limited(limit.clone()).kind(), ErrorKind::Limit);
    assert_eq!(
        CoreError::Protocol(ProtocolError::InvalidTimestamp("x".to_owned())).kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        CoreError::Adapter(AdapterError::Unavailable("x".to_owned())).kind(),
        ErrorKind::AdapterUnavailable
    );
    assert_eq!(
        CoreError::Adapter(AdapterError::Unsupported("x")).kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        CoreError::Tmux(TmuxError::Timeout(Duration::from_secs(1))).kind(),
        ErrorKind::Timeout
    );
    assert_eq!(
        CoreError::Tmux(TmuxError::ForeignSession("other".to_owned())).kind(),
        ErrorKind::InvalidInput
    );

    let limited: ServiceError = CoreError::Limited(limit).into();
    assert!(matches!(limited, ServiceError::Limited(_)));
    let cancelled: ServiceError = CoreError::Cancelled.into();
    assert!(matches!(cancelled, ServiceError::Cancelled));
    let missing: ServiceError = CoreError::NotFound("session".to_owned()).into();
    assert!(matches!(missing, ServiceError::NotFound(ref what) if what == "session"));
    let failed: ServiceError = CoreError::Invalid("bad".to_owned()).into();
    assert!(matches!(failed, ServiceError::Failed(ref message) if message.contains("bad")));
}

#[test]
fn fixed_and_system_rng_stay_in_bounds() {
    assert_eq!(FixedRng(7).jitter_secs(5), 5);
    assert_eq!(FixedRng(3).jitter_secs(5), 3);
    let system = vowp_core::deps::SystemRng::new();
    for _ in 0..64 {
        assert!(system.jitter_secs(9) <= 9);
    }
}
