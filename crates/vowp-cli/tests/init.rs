//! `vowp init` integration tests.
//!
//! Every test runs the real binary in a temporary directory with a temporary node home.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use vowp_protocol::{ProjectName, VowpConfig};
use vowp_store::{NodePaths, Store};

/// Runs `vowp` in `cwd` with `--home` and `HOME` pointed at the temp node home.
fn run_vowp(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run vowp")
}

/// Creates a directory that looks like the root of a git checkout.
fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("temp repo");
    std::fs::create_dir(repo.path().join(".git")).expect(".git dir");
    repo
}

#[test]
fn init_creates_files_and_registers_project() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    let output = run_vowp(repo.path(), home.path(), &["init", "--name", "demo", "--json"]);
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let canonical_repo = std::fs::canonicalize(repo.path()).expect("canonical repo path");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("init prints JSON");
    assert_eq!(value["project"], "demo");
    assert_eq!(
        value["path"].as_str().expect("path"),
        canonical_repo.display().to_string()
    );
    let written: Vec<&str> = value["written"]
        .as_array()
        .expect("written array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    for expected in [
        "vowp.toml",
        "AGENTS.md",
        "CLAUDE.md",
        ".vowp/templates/brief.md",
        ".vowp/templates/report.md",
        ".claude/skills/vowp-worker/SKILL.md",
        ".claude/skills/vowp-planner/SKILL.md",
        ".claude/skills/vowp-verifier/SKILL.md",
        ".claude/skills/vowp-keeper/SKILL.md",
        "opencode.jsonc",
        ".gitignore",
    ] {
        assert!(written.contains(&expected), "{expected} not in {written:?}");
        assert!(repo.path().join(expected).is_file(), "{expected} missing on disk");
    }
    assert_eq!(
        std::fs::read_to_string(repo.path().join("CLAUDE.md")).expect("CLAUDE.md"),
        "@AGENTS.md\n"
    );
    assert!(
        std::fs::read_to_string(repo.path().join(".gitignore"))
            .expect(".gitignore")
            .contains(".vowp/tmp/")
    );
    assert!(repo.path().join(".vowp/jobs").is_dir());
    assert!(repo.path().join(".vowp/checkpoints").is_dir());

    let stored = std::fs::read_to_string(repo.path().join("vowp.toml")).expect("vowp.toml");
    let config = vowp_protocol::VowpConfig::from_toml(&stored).expect("config validates");
    assert_eq!(config.project.name.as_str(), "demo");
    for role in ["planner", "keeper", "worker", "verifier"] {
        assert!(config.profiles.contains_key(role), "missing profile {role}");
    }

    let store = Store::open(&NodePaths::new(home.path())).expect("store opens");
    let project = store
        .project(&vowp_protocol::ProjectName::parse("demo").expect("slug"))
        .expect("project lookup")
        .expect("project row");
    assert_eq!(project.path, canonical_repo.display().to_string());
    let node = store.local_node().expect("node lookup").expect("local node");
    assert!(node.is_local);
    let loops = store.list_loops().expect("loops");
    assert_eq!(loops.len(), 9, "nine default loops");
    let intervals: Vec<(String, u64)> = loops
        .into_iter()
        .map(|loop_| (loop_.name, loop_.interval_secs))
        .collect();
    assert!(intervals.contains(&("l1-supervisor".to_owned(), 60)));
    assert!(intervals.contains(&("l4-planner".to_owned(), 1800)));
    assert!(intervals.contains(&("l8-watchdog".to_owned(), 300)));
}

#[test]
fn init_refuses_overwrite_without_force() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    assert!(
        run_vowp(repo.path(), home.path(), &["init", "--name", "demo"])
            .status
            .success()
    );

    let second = run_vowp(repo.path(), home.path(), &["init", "--name", "demo"]);
    assert_eq!(second.status.code(), Some(4), "second init must be a conflict");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("--force"),
        "conflict must mention --force: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    std::fs::write(repo.path().join("AGENTS.md"), "custom rules\n").expect("edit AGENTS.md");
    let forced = run_vowp(repo.path(), home.path(), &["init", "--name", "demo", "--force"]);
    assert!(forced.status.success(), "--force init failed");
    let restored = std::fs::read_to_string(repo.path().join("AGENTS.md")).expect("AGENTS.md");
    assert_ne!(restored, "custom rules\n");
    assert!(restored.contains("AGENTS.md"));
}

#[test]
fn init_skips_existing_files_without_force() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    std::fs::write(repo.path().join("AGENTS.md"), "custom rules\n").expect("custom AGENTS.md");
    let output = run_vowp(repo.path(), home.path(), &["init", "--name", "demo", "--json"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let skipped: Vec<&str> = value["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(skipped.contains(&"AGENTS.md"), "skipped: {skipped:?}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("AGENTS.md")).expect("AGENTS.md"),
        "custom rules\n"
    );
}

#[test]
fn init_no_framework_writes_only_project_state() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    let output = run_vowp(repo.path(), home.path(), &["init", "--name", "demo", "--no-framework"]);
    assert!(output.status.success());
    assert!(repo.path().join("vowp.toml").is_file());
    assert!(!repo.path().join("AGENTS.md").exists());
    assert!(!repo.path().join("opencode.jsonc").exists());
    assert!(Store::open(&NodePaths::new(home.path())).is_ok());
}

/// Creates a fake PATH directory from `(name, script body)` pairs.
fn fake_bin(files: &[(&str, &str)]) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("fake bin");
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("fake script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    dir
}

/// Runs `vowp doctor` with an isolated PATH and environment.
fn run_doctor(repo: &Path, home: &Path, bin: &Path, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vowp"));
    command
        .current_dir(repo)
        .env("HOME", home)
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env("PATH", bin)
        .env_remove("NO_COLOR")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .arg("--home")
        .arg(home)
        .arg("--project")
        .arg(repo)
        .args(["--json", "doctor"]);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run vowp doctor")
}

/// The version/probe scripts every fake PATH needs besides the two harnesses.
const FAKE_TOOLS: &[(&str, &str)] = &[
    ("git", "#!/bin/sh\necho 'git version 9.9.9'\nexit 0\n"),
    ("tmux", "#!/bin/sh\necho 'tmux 9.9'\nexit 0\n"),
    (
        "df",
        "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho '/dev/fake 9999999 1 8000000 1% /'\nexit 0\n",
    ),
];

#[test]
fn doctor_reports_missing_harness_as_fail_with_fake_path() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    assert!(
        run_vowp(repo.path(), home.path(), &["init", "--name", "demo"])
            .status
            .success()
    );

    let bin = fake_bin(FAKE_TOOLS);
    let output = run_doctor(repo.path(), home.path(), bin.path(), &[]);
    assert_eq!(output.status.code(), Some(1), "doctor must fail");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("doctor prints JSON");
    assert_eq!(value["ok"], false);
    let checks = value["checks"].as_array().expect("checks");
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing check {name}: {checks:?}"))
    };
    assert_eq!(find("harness:claude-code")["status"], "fail");
    assert_eq!(find("harness:opencode")["status"], "fail");
    assert_eq!(find("git")["status"], "ok");
    assert_eq!(find("tmux")["status"], "ok");
}

/// Runs one git command in `dir` and asserts success.
fn git_at(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a real git repository at `path` with one commit on `main`.
fn real_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("repo directory");
    git_at(path, &["init", "-b", "main"]);
    git_at(path, &["config", "user.email", "test@example.com"]);
    git_at(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("README.md"), "hello\n").expect("README");
    git_at(path, &["add", "."]);
    git_at(path, &["commit", "-m", "initial"]);
}

/// Writes a valid `vowp.toml` named `name` into `repo`.
fn write_named_project_config(repo: &Path, name: &str) {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = ProjectName::parse(name).expect("project name");
    config.project.path = Some(repo.display().to_string());
    std::fs::write(
        repo.join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config toml"),
    )
    .expect("vowp.toml");
}

/// One initialised temporary home with a `seed` project, nested under a writable parent.
///
/// The nesting is the point: `project add --git` derives its default clone target from
/// the home's parent, so a bare `/tmp` home would put test checkouts in a shared
/// directory. `root` is both that parent and the working directory for the commands.
struct BootstrapEnv {
    root: tempfile::TempDir,
    home: PathBuf,
}

impl BootstrapEnv {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("bootstrap root");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).expect("home directory");
        let seed = root.path().join("seed");
        real_git_repo(&seed);
        let output = run_vowp(&seed, &home, &["init", "--name", "seed", "--no-framework"]);
        assert!(
            output.status.success(),
            "seed init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { root, home }
    }

    fn parent(&self) -> &Path {
        self.root.path()
    }

    fn home_path(&self) -> &Path {
        &self.home
    }

    fn run(&self, args: &[&str]) -> Output {
        run_vowp(self.root.path(), &self.home, args)
    }

    fn run_json(&self, args: &[&str]) -> (i32, serde_json::Value, String) {
        let mut full = vec!["--json"];
        full.extend(args.iter().copied());
        let output = self.run(&full);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
        (output.status.code().unwrap_or(-1), value, stderr)
    }
}

/// Reads one project row from the real registry.
fn registry_project(home: &Path, name: &str) -> Option<vowp_protocol::Project> {
    Store::open(&NodePaths::new(home))
        .expect("store")
        .project(&ProjectName::parse(name).expect("project name"))
        .expect("project lookup")
}

/// Reads one orchestrator's persisted state file.
fn orchestrator_state(home: &Path, name: &str) -> Option<serde_json::Value> {
    let path = home.join("orchestrators").join(name).join("orchestrator.json");
    if !path.is_file() {
        return None;
    }
    Some(serde_json::from_slice(&std::fs::read(path).expect("read orchestrator state")).expect("state JSON"))
}

#[test]
fn project_add_git_clones_initializes_registers_and_returns_one_summary() {
    let env = BootstrapEnv::new();
    let source = env.parent().join("origins").join("demo");
    real_git_repo(&source);
    let bare = env.parent().join("origins").join("demo.git");
    let output = Command::new("git")
        .args(["clone", "--bare"])
        .arg(&source)
        .arg(&bare)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git clone --bare");
    assert!(
        output.status.success(),
        "bare clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (code, value, stderr) = env.run_json(&[
        "project",
        "add",
        "--git",
        bare.to_str().expect("bare path"),
        "--init",
        "--orchestrator",
        "chat",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let keys: BTreeSet<&str> = value
        .as_object()
        .expect("one summary object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["project", "path", "initialized", "orchestrator"].into_iter().collect(),
        "bootstrap summary fields changed: {value}"
    );

    let target = env.parent().join("demo");
    assert!(target.is_dir(), "clone target is missing");
    assert!(target.join("vowp.toml").is_file(), "init did not run");
    assert_eq!(value["project"], "demo", "{value}");
    assert_eq!(value["initialized"], true, "{value}");
    assert_eq!(
        Path::new(value["path"].as_str().expect("path"))
            .canonicalize()
            .expect("summary path canonicalises"),
        target.canonicalize().expect("clone target canonicalises"),
        "{value}"
    );

    let card = value["orchestrator"].as_object().expect("orchestrator card");
    assert_eq!(card["name"], "demo", "{value}");
    assert_eq!(card["project"], "demo", "{value}");
    assert_eq!(card["app"], "chat", "{value}");
    let session = card["session_id"].as_str().expect("session id");
    assert!(!session.is_empty(), "{value}");
    let state = orchestrator_state(env.home_path(), "demo").expect("orchestrator state");
    assert_eq!(state["session_id"], session, "{state}");
    assert_eq!(state["project"], "demo", "{state}");

    let row = registry_project(env.home_path(), "demo").expect("registry row");
    assert_eq!(
        Path::new(&row.path)
            .canonicalize()
            .expect("registered path canonicalises"),
        target.canonicalize().expect("clone target canonicalises")
    );
    let text = std::fs::read_to_string(target.join("vowp.toml")).expect("vowp.toml");
    assert!(text.contains("name = \"demo\""), "{text}");
}

#[test]
fn project_add_git_refuses_an_existing_target_and_a_url_with_credentials() {
    let env = BootstrapEnv::new();
    let source = env.parent().join("origins").join("demo");
    real_git_repo(&source);
    let target = env.parent().join("demo");
    std::fs::create_dir(&target).expect("existing target");
    std::fs::write(target.join("keep.txt"), "keep\n").expect("target marker");

    let output = env.run(&[
        "project",
        "add",
        "--git",
        source.to_str().expect("source path"),
        "--json",
    ]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(
        target.join("keep.txt").is_file(),
        "a refused clone must not touch the existing target"
    );
    assert!(
        registry_project(env.home_path(), "demo").is_none(),
        "a refused clone must not register the project"
    );

    // A URL with userinfo is refused before any git process or network access. The
    // user:password pair is a placeholder on a reserved example domain.
    let credential_url = "https://git:git@example.com/demo.git";
    let output = env.run(&["--json", "project", "add", "--git", credential_url]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "a credentialed URL is a guard: {output:?}"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("git:git@") && !combined.contains(credential_url),
        "the credential or raw URL was echoed: {combined}"
    );
    assert!(
        registry_project(env.home_path(), "demo").is_none(),
        "a refused credentialed URL must not register the project"
    );
}

#[test]
fn project_add_init_is_skipped_when_vowp_toml_exists() {
    let env = BootstrapEnv::new();
    let checkout = env.parent().join("kept");
    real_git_repo(&checkout);
    write_named_project_config(&checkout, "kept");
    let config_path = checkout.join("vowp.toml");
    let before = std::fs::read(&config_path).expect("original vowp.toml");

    let (code, value, stderr) = env.run_json(&["project", "add", checkout.to_str().expect("path"), "--init"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["project"], "kept", "{value}");
    assert_eq!(value["initialized"], false, "{value}");
    assert_eq!(
        std::fs::read(&config_path).expect("vowp.toml after add"),
        before,
        "an existing vowp.toml must stay byte-identical"
    );
    assert!(registry_project(env.home_path(), "kept").is_some());
}

#[test]
fn project_add_init_skips_an_initialised_checkout_named_by_a_subdirectory() {
    let env = BootstrapEnv::new();
    let checkout = env.parent().join("kept");
    real_git_repo(&checkout);
    write_named_project_config(&checkout, "kept");
    let config_path = checkout.join("vowp.toml");
    let before = std::fs::read(&config_path).expect("original vowp.toml");
    let subdirectory = checkout.join("sub");
    std::fs::create_dir(&subdirectory).expect("subdirectory");

    let (code, value, stderr) = env.run_json(&[
        "project",
        "add",
        subdirectory.to_str().expect("subdirectory path"),
        "--init",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["project"], "kept", "{value}");
    assert_eq!(value["initialized"], false, "{value}");
    assert_eq!(
        Path::new(value["path"].as_str().expect("path"))
            .canonicalize()
            .expect("summary path canonicalises"),
        checkout.canonicalize().expect("checkout canonicalises"),
        "a subdirectory registers its checkout root: {value}"
    );
    assert_eq!(
        std::fs::read(&config_path).expect("vowp.toml after add"),
        before,
        "an existing vowp.toml must stay byte-identical"
    );
    assert!(registry_project(env.home_path(), "kept").is_some());
}

#[test]
fn project_add_git_failure_leaves_no_partial_checkout_or_registry_row() {
    let env = BootstrapEnv::new();
    let missing = env.parent().join("origins").join("missing-demo.git");
    let output = env.run(&[
        "project",
        "add",
        "--git",
        missing.to_str().expect("missing path"),
        "--init",
        "--json",
    ]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !env.parent().join("missing-demo").exists(),
        "a failed clone left a partial target"
    );
    assert!(
        registry_project(env.home_path(), "missing-demo").is_none(),
        "a failed clone registered a project row"
    );
}

#[test]
fn project_add_orchestrator_reuses_a_matching_existing_card_and_refuses_a_conflict() {
    let env = BootstrapEnv::new();
    let checkout = env.parent().join("second");
    real_git_repo(&checkout);
    write_named_project_config(&checkout, "second");
    let path = checkout.to_str().expect("path");

    let (code, first, stderr) = env.run_json(&["project", "add", path, "--orchestrator", "chat"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(first["orchestrator"]["app"], "chat", "{first}");
    let session = first["orchestrator"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let keeper = env.home_path().join("orchestrators").join("second").join("keeper.pid");
    assert!(!keeper.exists(), "the chat app needs no keeper");

    let (code, second, stderr) = env.run_json(&["project", "add", path, "--orchestrator", "chat"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        second["orchestrator"]["session_id"], session,
        "the matching card is reused, not replaced: {second}"
    );
    assert!(!keeper.exists(), "reuse must not start a keeper for chat");

    let output = env.run(&["project", "add", path, "--orchestrator", "opencode"]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already exists for app chat"), "{stderr}");
    let state = orchestrator_state(env.home_path(), "second").expect("orchestrator state");
    assert_eq!(state["app"], "chat", "{state}");
    assert_eq!(state["session_id"], session, "{state}");
}

#[test]
fn project_add_orchestrator_failure_keeps_the_project_and_names_the_retry() {
    let env = BootstrapEnv::new();
    let output = env.run(&[
        "orchestrator",
        "new",
        "--name",
        "second",
        "--project",
        "seed",
        "--app",
        "chat",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "conflicting orchestrator setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let checkout = env.parent().join("second");
    real_git_repo(&checkout);
    write_named_project_config(&checkout, "second");
    let output = env.run(&[
        "project",
        "add",
        checkout.to_str().expect("path"),
        "--orchestrator",
        "chat",
    ]);
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("vowp orchestrator new --name second --app chat --project second"),
        "the failure must name the exact retry: {stderr}"
    );
    assert!(stderr.contains("project second is registered"), "{stderr}");
    assert!(
        registry_project(env.home_path(), "second").is_some(),
        "the project stays registered after the orchestrator step fails"
    );
    let state = orchestrator_state(env.home_path(), "second").expect("existing card");
    assert_eq!(state["project"], "seed", "the conflicting card is untouched: {state}");
}

#[test]
fn project_add_bootstrap_summary_never_contains_the_url_or_a_secret() {
    let env = BootstrapEnv::new();
    let marker = "bootstrap-url-marker";
    let source = env.parent().join("origins").join(marker).join("demo");
    real_git_repo(&source);
    let url = source.to_str().expect("source path").to_owned();

    let (code, value, stderr) = env.run_json(&["project", "add", "--git", &url, "--init"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["project"], "demo", "{value}");
    assert_eq!(value["initialized"], true, "{value}");
    assert!(value["orchestrator"].is_null(), "{value}");
    let summary = value.to_string();
    assert!(
        !summary.contains(marker),
        "the URL path marker leaked into the summary: {summary}"
    );
    assert!(
        !summary.contains(&url),
        "the raw URL leaked into the summary: {summary}"
    );
    assert!(
        !stderr.contains(marker),
        "the URL path marker leaked to stderr: {stderr}"
    );
    assert!(!stderr.contains(&url), "the raw URL leaked to stderr: {stderr}");
}

#[test]
fn doctor_never_prints_env_values() {
    let repo = git_repo();
    let home = tempfile::tempdir().expect("temp home");
    assert!(
        run_vowp(repo.path(), home.path(), &["init", "--name", "demo"])
            .status
            .success()
    );

    let bin = fake_bin(&[
        ("git", "#!/bin/sh\necho 'git version 9.9.9'\nexit 0\n"),
        ("tmux", "#!/bin/sh\necho 'tmux 9.9'\nexit 0\n"),
        ("df", "#!/bin/sh\necho '/dev/fake 9999999 1 8000000 1% /'\nexit 0\n"),
        (
            "claude",
            "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo '2.1.0 (Claude Code)'; exit 0; fi\nexit 0\n",
        ),
        (
            "opencode",
            "#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo 'opencode 2.0.0'; exit 0; fi\nexit 0\n",
        ),
    ]);
    // Fake values are assembled at run time so the repository contains no secret-shaped
    // literal for the secret-scan gate; the point is that doctor never prints the value.
    let fake_prefix: String = ['s', 'k', '-'].into_iter().collect();
    let deepseek_value = format!("{fake_prefix}deepseek-test-value-0001");
    let claude_value = format!("{fake_prefix}claude-test-value-0002");
    let output = run_doctor(
        repo.path(),
        home.path(),
        bin.path(),
        &[
            ("DEEPSEEK_API_KEY", deepseek_value.as_str()),
            ("CLAUDE_API_KEY", claude_value.as_str()),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(&deepseek_value), "stdout leaked the value: {stdout}");
    assert!(!stderr.contains(&deepseek_value), "stderr leaked the value: {stderr}");
    assert!(!stdout.contains(&claude_value), "stdout leaked the value: {stdout}");
    assert!(!stderr.contains(&claude_value), "stderr leaked the value: {stderr}");
    assert!(stdout.contains("DEEPSEEK_API_KEY"), "{stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(value["ok"], true, "{value}");
}
