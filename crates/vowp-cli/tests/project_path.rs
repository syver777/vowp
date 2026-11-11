// `[project].path` defaults and relative harness mappings.
//
// `vowp.toml` may omit `[project].path`; the documented default is the directory
// containing the file. These tests cover the config default and the real CLI resolving a
// relative harness `mapping` from that default when the caller stands elsewhere.

mod common;

use std::path::{Path, PathBuf};

use common::TestEnv;
use vowp_cli::ctx::load_project_config;
use vowp_protocol::HarnessConfig;
use vowp_store::NodePaths;

/// Writes a valid config whose `[project]` table omits `path`.
fn write_config_without_path(dir: &Path) -> PathBuf {
    write_config(dir, "")
}

/// Writes a valid config whose `[project]` table carries `extra` after the name.
fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let text = format!(
        r#"[node]
name = "local"

[project]
name = "test"
{extra}
[profiles.planner]
ladder = ["opencode:test/model"]

[profiles.keeper]
ladder = ["opencode:test/model"]

[profiles.worker]
ladder = ["opencode:test/model"]

[profiles.verifier]
ladder = ["opencode:test/model"]
"#
    );
    let path = dir.join("vowp.toml");
    std::fs::write(&path, text).expect("write vowp.toml");
    path
}

#[test]
fn load_project_config_defaults_project_path() {
    let home = tempfile::tempdir().expect("temp home");
    let dir = tempfile::tempdir().expect("temp config dir");
    let paths = NodePaths::new(home.path());

    let file = write_config_without_path(dir.path());
    let config = load_project_config(&file, &paths).expect("load config without path");
    assert_eq!(
        config.project.path,
        Some(dir.path().display().to_string()),
        "an omitted [project].path must default to the directory containing vowp.toml"
    );

    let file = write_config(dir.path(), "path = \"/somewhere\"\n");
    let config = load_project_config(&file, &paths).expect("load config with explicit path");
    assert_eq!(
        config.project.path.as_deref(),
        Some("/somewhere"),
        "an explicit [project].path must be kept verbatim"
    );
}

#[test]
fn cli_project_name_resolves_relative_mapping_outside_checkout() {
    let env = TestEnv::new();
    let mut config = env.config.clone();
    config.project.path = None;
    config.harnesses.insert(
        "fakecli".to_owned(),
        HarnessConfig {
            mapping: Some("fakecli.toml".to_owned()),
            ..HarnessConfig::default()
        },
    );
    std::fs::write(
        env.project_path().join("vowp.toml"),
        toml::to_string_pretty(&config).expect("config serialises"),
    )
    .expect("write vowp.toml");
    std::fs::write(
        env.project_path().join("fakecli.toml"),
        "name = \"fakecli\"\n[command]\nargv = [\"fake\"]\n",
    )
    .expect("write mapping");

    // Stand somewhere unrelated so a cwd-relative mapping cannot be found by accident.
    let elsewhere = tempfile::tempdir().expect("unrelated dir");
    let output = env
        .command(env!("CARGO_BIN_EXE_vowp"))
        .current_dir(elsewhere.path())
        .arg("--home")
        .arg(env.home_path())
        .arg("--project")
        .arg("test")
        .args(["--json", "job", "ls"])
        .output()
        .expect("run vowp");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vowp --project test must resolve fakecli.toml under the project directory; exit {:?}; stdout: {stdout}; stderr: {stderr}",
        output.status.code()
    );
    assert!(
        !stderr.contains("No such file or directory"),
        "relative mapping must not be resolved from the caller's cwd; stderr: {stderr}"
    );
    assert!(
        !stdout.contains("No such file or directory"),
        "relative mapping must not be resolved from the caller's cwd; stdout: {stdout}"
    );
    let jobs: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout:?}; stderr: {stderr:?}"));
    assert!(jobs.is_array(), "job ls must print a JSON array: {jobs}");
}
