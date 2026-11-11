//! `vowp capacity` CLI tests: JSON output and the exit-5 owner refusal.
//!
//! The file is a module of the registered `vowp-core` `capacity_admit` test target, so the
//! tests run under `cargo test --workspace` even though the root manifest has no separate
//! `[[test]] name = "capacity"` entry (the manifest is outside this job's owned paths).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;
use vowp_protocol::{Node, NodeName, Project, VowpConfig};
use vowp_store::{NodePaths, Store};

/// The resolved CLI binary, built at most once per test process.
static CLI_BINARY: OnceLock<PathBuf> = OnceLock::new();

/// One temporary node home and project, plus the private tmux environment.
struct CliEnv {
    home: TempDir,
    project: TempDir,
}

impl CliEnv {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let project = tempfile::tempdir().expect("temp project");
        let config = VowpConfig::with_defaults_for_tests();
        std::fs::write(
            project.path().join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config serialises"),
        )
        .expect("write vowp.toml");
        std::fs::create_dir_all(project.path().join(".vowp/jobs")).expect("jobs dir");

        let paths = NodePaths::new(home.path());
        let store = Store::open(&paths).expect("store opens");
        let now = store.clock().now();
        store
            .upsert_node(&Node {
                name: NodeName::parse("local").expect("local slug"),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({}),
                last_seen_at: Some(now),
                created_at: now,
            })
            .expect("node row");
        store
            .upsert_project(&Project {
                name: config.project.name.clone(),
                path: project.path().display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        Self { home, project }
    }

    /// Runs one command against this temporary home and returns `(code, stdout, stderr)`.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let tmux = self.home.path().join("tmux");
        std::fs::create_dir_all(&tmux).expect("private tmux dir");
        let output = Command::new(cli_binary())
            .arg("--home")
            .arg(self.home.path())
            .arg("--project")
            .arg(self.project.path())
            .args(args)
            .env("HOME", self.home.path())
            .env("TMUX_TMPDIR", &tmux)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("VOWP_TMUX_SOCKET")
            .output()
            .unwrap_or_else(|error| panic!("run vowp: {error}"));
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Runs one command with `--json` and parses its stdout document.
    fn run_json(&self, args: &[&str]) -> (i32, serde_json::Value, String) {
        let mut full = vec!["--json"];
        full.extend(args.iter().copied());
        let (code, stdout, stderr) = self.run(&full);
        let value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
        (code, value, stderr)
    }

    /// Opens a fresh store connection to the temporary home.
    fn store(&self) -> Store {
        Store::open(&NodePaths::new(self.home.path())).expect("store opens")
    }
}

/// Returns the `vowp` binary, building it once when the core test target runs alone.
fn cli_binary() -> PathBuf {
    CLI_BINARY.get_or_init(resolve_cli_binary).clone()
}

/// Resolves or builds the CLI binary exactly once.
fn resolve_cli_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vowp") {
        return PathBuf::from(path);
    }
    let current = std::env::current_exe().expect("current test executable");
    let profile = current
        .parent()
        .and_then(Path::parent)
        .expect("target profile directory");
    let binary = profile.join(if cfg!(windows) { "vowp.exe" } else { "vowp" });
    if binary.is_file() {
        return binary;
    }
    let target = profile.parent().expect("target directory");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace_root())
        .args(["build", "--quiet", "-p", "vowp-cli", "--bin", "vowp", "--target-dir"])
        .arg(target)
        .status()
        .unwrap_or_else(|error| panic!("build vowp CLI: {error}"));
    assert!(status.success(), "cargo build -p vowp-cli --bin vowp failed");
    assert!(binary.is_file(), "CLI binary missing at {}", binary.display());
    binary
}

/// Finds the workspace root from the package manifest directory.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .find(|directory| {
            let cargo = directory.join("Cargo.toml");
            cargo.is_file() && std::fs::read_to_string(cargo).is_ok_and(|text| text.contains("[workspace]"))
        })
        .map(Path::to_path_buf)
        .expect("workspace root")
}

#[test]
fn capacity_set_json_writes_the_target() {
    let env = CliEnv::new();
    let (code, value, stderr) = env.run_json(&["capacity", "set", "--target", "2", "--reason", "two workers"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["mode"], "fixed");
    assert_eq!(value["target_workers"], 2);
    assert_eq!(value["reason"], "two workers");
    assert_eq!(value["set_by"], "owner");

    let latest = env
        .store()
        .latest_capacity_target()
        .expect("target query")
        .expect("a target row");
    assert_eq!(latest.target_workers, 2);
    assert_eq!(latest.reason, "two workers");
}

#[test]
fn capacity_set_above_advice_refuses_with_exit_5() {
    let env = CliEnv::new();
    let (code, stdout, stderr) = env.run(&["capacity", "set", "--target", "9"]);
    assert_eq!(
        code, 5,
        "an above-advice target must be refused: stdout {stdout}; stderr {stderr}"
    );
    assert!(
        stderr.contains("above the advised"),
        "the refusal names the advice: {stderr}"
    );
    assert!(
        env.store().latest_capacity_target().expect("target query").is_none(),
        "a refused write must not change the target"
    );

    let (code, value, stderr) = env.run_json(&["capacity", "set", "--target", "9", "--override"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["target_workers"], 9);
    assert_eq!(value["is_override"], true);
}
