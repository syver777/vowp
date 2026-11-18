//! `vowp capacity` CLI tests: JSON output and the exit-5 owner refusal.
//!
//! The file is a module of the registered `vowp-core` `capacity_admit` test target, so the
//! tests run under `cargo test --workspace` even though the root manifest has no separate
//! `[[test]] name = "capacity"` entry (the manifest is outside this job's owned paths).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use tempfile::TempDir;
use vowp_protocol::{HostSample, Node, NodeName, Project, SampleSource, Timestamp, VowpConfig};
use vowp_store::{LedgerFilter, NodePaths, Store};

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

    /// Reads the project's current `vowp.toml`.
    fn config(&self) -> VowpConfig {
        let text = std::fs::read_to_string(self.project.path().join("vowp.toml")).expect("read config");
        VowpConfig::from_toml(&text).expect("parse config")
    }

    /// Sets one provider's `max_concurrent` in the project config.
    fn set_provider_cap(&self, cap: u32) {
        let mut config = self.config();
        config
            .providers
            .entry("deepseek".to_owned())
            .or_default()
            .max_concurrent = cap;
        std::fs::write(
            self.project.path().join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config toml"),
        )
        .expect("write vowp.toml");
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
fn capacity_reload_check_reports_without_writing() {
    let env = CliEnv::new();
    env.set_provider_cap(3);

    let (code, value, stderr) = env.run_json(&["capacity", "reload", "--check"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["checked"], true);
    assert_eq!(value["applied"], false);
    assert_eq!(value["pending"], false);
    assert_eq!(value["provider_caps"]["deepseek"], 3);
    assert_eq!(
        value["target"]["target_workers"], 3,
        "with no sample, the bootstrap target is capped by the configured provider"
    );
    assert_eq!(value["target"]["mode"], "auto");

    let project = env.config().project.name;
    let key = format!("capacity:config:{project}");
    assert!(
        env.store().kv_get(&key).expect("kv").is_none(),
        "--check must not publish reload state"
    );
    assert!(
        env.store().latest_capacity_target().expect("target query").is_none(),
        "--check must not write a target row"
    );
}

#[test]
fn capacity_report_shows_provider_caps_and_reload_state() {
    let env = CliEnv::new();
    env.set_provider_cap(2);

    let (code, value, stderr) = env.run_json(&["capacity", "report"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["provider_caps"]["deepseek"], 2);
    assert_eq!(value["reload_pending"], false);

    let (code, stdout, stderr) = env.run(&["capacity", "report"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("providers: deepseek=2"), "{stdout}");
    assert!(stdout.contains("reload: current"), "{stdout}");
}

#[test]
fn capacity_reload_refuses_when_no_supervisor_runs() {
    let env = CliEnv::new();
    env.set_provider_cap(2);
    let project = env.config().project.name;

    let (code, value, stderr) = env.run_json(&["capacity", "reload"]);
    assert_eq!(code, 5, "{stderr}");
    assert_eq!(value["error"]["kind"], "guard");
    let message = value["error"]["message"].as_str().expect("message");
    assert!(message.contains("supervisor is not running"), "{message}");
    assert!(
        message.contains("vowp up"),
        "the refusal includes a start hint: {message}"
    );

    let store = env.store();
    assert!(
        store
            .kv_prefix("capacity:reload_request:")
            .expect("requests")
            .is_empty(),
        "no request is left when the supervisor is absent"
    );
    assert!(
        store
            .kv_get(&format!("capacity:config:{project}"))
            .expect("kv")
            .is_none(),
        "the CLI does not write reload state itself"
    );
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

/// Seeds one raw 5-minute bucket and rolls it up.
fn seed_rollup(store: &Store, bucket: Timestamp, mem_available: u64, load1: f64, disk_free: u64) {
    for (offset, memory) in [(0_u64, mem_available), (60, mem_available / 2), (120, mem_available)] {
        store
            .insert_host_sample(&HostSample {
                at: bucket.plus(Duration::from_secs(offset)),
                interval_secs: 5,
                mem_total_bytes: 8_000_000_000,
                mem_available_bytes: memory,
                swap_used_bytes: 0,
                load1,
                load5: load1,
                load15: load1,
                cpu_count: 4,
                cpu_quota_cores: None,
                disk_free_home_bytes: disk_free,
                disk_free_worktrees_bytes: None,
                pressure_cpu_avg10: None,
                pressure_mem_avg10: None,
                pressure_io_avg10: None,
                source: SampleSource::Proc,
            })
            .expect("host sample");
    }
    assert!(
        store.rollup_host_samples(bucket, 300).expect("rollup"),
        "the seeded bucket has samples"
    );
}

/// Floors an instant to the start of a five-minute bucket.
fn bucket_start(at: Timestamp) -> Timestamp {
    Timestamp::from_unix_secs(at.unix_secs().div_euclid(300) * 300)
}

#[test]
fn capacity_history_json_is_the_capacity_history_document() {
    let env = CliEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let hour = Timestamp::from_unix_secs(now.unix_secs().div_euclid(3600) * 3600);
    let first = hour.minus(Duration::from_secs(3600));
    let second = first.plus(Duration::from_secs(300));
    seed_rollup(&store, first, 4_000_000_000, 0.5, 10_000_000_000);
    seed_rollup(&store, second, 3_000_000_000, 1.5, 9_000_000_000);

    let (code, value, stderr) = env.run_json(&["capacity", "history", "--since", "3h", "--bucket", "5m"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["bucket_secs"], 300);
    assert_eq!(value["retention_secs"], 2_592_000);
    let points = value["points"].as_array().expect("points array");
    assert_eq!(points.len(), 2, "{value}");
    assert_eq!(points[0]["at"], first.as_rfc3339());
    assert_eq!(points[0]["samples"], 3);
    assert_eq!(points[0]["mem_available"]["min"], 2_000_000_000_u64);
    assert_eq!(points[0]["mem_available"]["max"], 4_000_000_000_u64);
    assert_eq!(points[0]["load1_avg"], 0.5);
    assert_eq!(points[0]["disk_free_home_min"], 10_000_000_000_u64);
    assert_eq!(points[0]["source"], "proc");

    let (code, human, stderr) = env.run(&["capacity", "history", "--since", "3h"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        human.contains("AT") && human.contains("SAMPLES") && human.contains("MEM_MIN/AVG/MAX"),
        "{human}"
    );
    assert!(
        human.contains("LOAD1") && human.contains("DISK_MIN") && human.contains("SOURCE"),
        "{human}"
    );
    assert!(human.contains(&first.as_rfc3339()), "{human}");
    assert!(human.contains("cost: vowp cost --by day"), "{human}");
}

#[test]
fn capacity_history_aggregates_read_time_hour_buckets() {
    let env = CliEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let hour = Timestamp::from_unix_secs(now.unix_secs().div_euclid(3600) * 3600);
    let first = hour.minus(Duration::from_secs(3600));
    let second = first.plus(Duration::from_secs(300));
    seed_rollup(&store, first, 4_000_000_000, 0.5, 10_000_000_000);
    seed_rollup(&store, second, 2_000_000_000, 1.5, 9_000_000_000);

    let (code, value, stderr) = env.run_json(&["capacity", "history", "--bucket", "1h"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["bucket_secs"], 3600);
    let points = value["points"].as_array().expect("points array");
    assert_eq!(points.len(), 1, "{value}");
    assert_eq!(points[0]["samples"], 6);
    assert_eq!(points[0]["mem_available"]["min"], 1_000_000_000_u64);
    assert_eq!(points[0]["mem_available"]["max"], 4_000_000_000_u64);
}

#[test]
fn capacity_history_unknown_bucket_exits_two_with_the_accepted_values() {
    let env = CliEnv::new();
    let (code, _stdout, stderr) = env.run(&["capacity", "history", "--bucket", "30m"]);
    assert_eq!(code, 2, "an unknown bucket is invalid input (code {code}): {stderr}");
    assert!(
        stderr.contains("5m") && stderr.contains("1h") && stderr.contains("1d"),
        "the refusal names the accepted values: {stderr}"
    );
}

#[test]
fn capacity_history_clamps_since_to_the_retained_window_and_says_so() {
    let env = CliEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let old = bucket_start(now.minus(Duration::from_secs(40 * 24 * 60 * 60)));
    seed_rollup(&store, old, 4_000_000_000, 0.5, 10_000_000_000);

    let (code, value, stderr) = env.run_json(&["capacity", "history", "--since", "60d"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        value["points"].as_array().expect("points").len(),
        0,
        "the 40-day-old rollup is outside the 30-day window"
    );
    let since = Timestamp::parse(value["since"].as_str().expect("since")).expect("timestamp");
    let retained = now.minus(Duration::from_secs(30 * 24 * 60 * 60));
    assert!(
        since <= retained && since >= retained.minus(Duration::from_secs(300)),
        "the window is clamped to the retained start: {}",
        since.as_rfc3339()
    );
    assert!(stderr.contains("retained window"), "{stderr}");
}

#[test]
fn capacity_history_is_read_only() {
    let env = CliEnv::new();
    let store = env.store();
    let now = store.clock().now();
    let bucket = bucket_start(now.minus(Duration::from_secs(10 * 60)));
    seed_rollup(&store, bucket, 4_000_000_000, 0.5, 10_000_000_000);
    let before_ledger = store
        .ledger_since(&LedgerFilter::default())
        .expect("ledger before")
        .len();
    let before_target = store.latest_capacity_target().expect("target before");

    let (code, _value, stderr) = env.run_json(&["capacity", "history", "--since", "1h"]);
    assert_eq!(code, 0, "{stderr}");

    let store = env.store();
    let after_ledger = store
        .ledger_since(&LedgerFilter::default())
        .expect("ledger after")
        .len();
    assert_eq!(after_ledger, before_ledger, "history writes no ledger row");
    assert_eq!(
        store.latest_capacity_target().expect("target after").is_some(),
        before_target.is_some(),
        "history writes no capacity target row"
    );
}
