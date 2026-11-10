//! `vowp init` integration tests.
//!
//! Every test runs the real binary in a temporary directory with a temporary node home.

use std::path::Path;
use std::process::{Command, Output};

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
