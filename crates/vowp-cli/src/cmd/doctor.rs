//! `vowp doctor`: one screen of health checks.
//!
//! Spec: `docs/design/cli.md` §2. Checks are `ok|warn|fail`; the command exits 0 only when no
//! check failed. Secret values are never read or printed: provider checks report environment
//! variable **names** only and never touch the network.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use vowp_core::schedules::ScheduleSpec;
use vowp_core::{AdapterRegistry, CommandRunner, CommandSpec, SystemRunner};
use vowp_protocol::{Executor, HarnessKind, ScheduleSpecKind, VowpConfig};
use vowp_store::Store;

use crate::cli::{DoctorArgs, Global};
use crate::ctx;
use crate::{CliError, IoStreams};

/// One check's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    /// Returns the lowercase status word.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One health check.
#[derive(Debug, Serialize)]
struct Check {
    name: String,
    status: CheckStatus,
    detail: String,
}

/// The JSON document `vowp doctor --json` prints.
#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<Check>,
}

/// Accumulates checks.
#[derive(Default)]
struct Checks {
    items: Vec<Check>,
}

impl Checks {
    /// Records one check.
    fn push(&mut self, name: &str, status: CheckStatus, detail: impl Into<String>) {
        self.items.push(Check {
            name: name.to_owned(),
            status,
            detail: detail.into(),
        });
    }

    /// Returns true when no check failed.
    fn healthy(&self) -> bool {
        !self.items.iter().any(|check| check.status == CheckStatus::Fail)
    }
}

/// Runs `vowp doctor`.
pub fn run(global: &Global, io: &mut dyn IoStreams, _args: &DoctorArgs) -> Result<i32, CliError> {
    let paths = ctx::node_paths(global)?;
    let root = paths.root();
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemRunner);
    let mut checks = Checks::default();

    check_home(root, &mut checks);
    check_db(&paths, &mut checks);
    check_version(
        runner.as_ref(),
        "git",
        &["git", "--version"],
        CheckStatus::Fail,
        &mut checks,
    );
    check_version(runner.as_ref(), "tmux", &["tmux", "-V"], CheckStatus::Warn, &mut checks);

    let project_root = resolve_project(global, &paths);
    let config = check_config(project_root.as_deref(), &paths, &mut checks);
    check_schedules(config.as_ref(), &mut checks);

    let registry_config = config.clone().unwrap_or_else(VowpConfig::with_defaults_for_tests);
    let registry = AdapterRegistry::from_config(&registry_config);
    let registry = match registry {
        Ok(registry) => Some(registry),
        Err(error) => {
            checks.push("harnesses", CheckStatus::Fail, error.to_string());
            None
        }
    };

    if let Some(registry) = &registry {
        check_harness(registry, HarnessKind::ClaudeCode, "claude-code", &mut checks);
        check_harness(registry, HarnessKind::Opencode, "opencode", &mut checks);
    } else {
        checks.push("harness:claude-code", CheckStatus::Fail, "adapter registry unavailable");
        checks.push("harness:opencode", CheckStatus::Fail, "adapter registry unavailable");
    }

    check_profiles(registry.as_ref(), config.as_ref(), &mut checks);
    check_providers(config.as_ref(), &mut checks);
    check_systemd(&mut checks);
    check_watchdog(&mut checks);
    check_supervisor(&paths, &mut checks);
    check_disk(runner.as_ref(), root, &mut checks);

    let ok = checks.healthy();
    if global.json {
        let report = DoctorReport {
            ok,
            checks: checks.items,
        };
        crate::output::print_json(io, &report)?;
    } else {
        for check in &checks.items {
            writeln!(
                io.out(),
                "{:<4} {:<20} {}",
                check.status.as_str(),
                check.name,
                check.detail
            )?;
        }
        writeln!(io.out(), "{}", if ok { "doctor: ok" } else { "doctor: failures found" })?;
    }
    Ok(if ok { 0 } else { 1 })
}

/// Checks that the node home exists and accepts a write.
fn check_home(root: &Path, checks: &mut Checks) {
    if !root.is_dir() {
        checks.push(
            "home",
            CheckStatus::Warn,
            format!("{} is not initialised; run `vowp init`", root.display()),
        );
        return;
    }
    let probe = root.join(format!(".doctor-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            checks.push("home", CheckStatus::Ok, format!("{} is writable", root.display()));
        }
        Err(error) => checks.push("home", CheckStatus::Fail, format!("{}: {error}", root.display())),
    }
}

/// Opens the database, runs the integrity checks and reports the schema version.
fn check_db(paths: &vowp_store::NodePaths, checks: &mut Checks) {
    if !paths.db_path().is_file() {
        checks.push(
            "db",
            CheckStatus::Warn,
            format!("{} is missing; run `vowp init`", paths.db_path().display()),
        );
        return;
    }
    match Store::open(paths) {
        Ok(store) => match store.integrity_check().and_then(|()| store.schema_version()) {
            Ok(version) => checks.push(
                "db",
                CheckStatus::Ok,
                format!("{} opens, integrity ok, schema {version}", paths.db_path().display()),
            ),
            Err(error) => checks.push("db", CheckStatus::Fail, error.to_string()),
        },
        Err(error) => checks.push("db", CheckStatus::Fail, error.to_string()),
    }
}

/// Runs a `--version` style command and records its first line.
fn check_version(runner: &dyn CommandRunner, name: &str, argv: &[&str], missing: CheckStatus, checks: &mut Checks) {
    let spec = CommandSpec {
        shell: None,
        argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        cwd: None,
        env: Vec::new(),
        timeout: Duration::from_secs(10),
    };
    match runner.run(&spec) {
        Ok(output) if output.exit_code == Some(0) => {
            let text = output.text();
            let line = text.lines().next().unwrap_or("").trim();
            checks.push(name, CheckStatus::Ok, if line.is_empty() { "installed" } else { line });
        }
        Ok(output) => {
            let detail = format!("`{}` exited with {:?}", argv.join(" "), output.exit_code);
            checks.push(name, missing, detail);
        }
        Err(error) => checks.push(name, missing, format!("{} not found: {error}", argv[0])),
    }
}

/// Resolves the project root for the config check.
fn resolve_project(global: &Global, paths: &vowp_store::NodePaths) -> Option<PathBuf> {
    if let Some(value) = &global.project {
        let candidate = PathBuf::from(value);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !paths.db_path().is_file() {
            return None;
        }
        let name = vowp_protocol::ProjectName::parse(value).ok()?;
        let store = Store::open(paths).ok()?;
        let project = store.project(&name).ok()??;
        return Some(PathBuf::from(project.path));
    }
    let cwd = std::env::current_dir().ok()?;
    if let Some(root) = ctx::find_project_root(&cwd) {
        return Some(root);
    }
    if !paths.db_path().is_file() {
        return None;
    }
    let store = Store::open(paths).ok()?;
    let projects = store.list_projects().ok()?;
    match projects.as_slice() {
        [project] => Some(PathBuf::from(&project.path)),
        _ => None,
    }
}

/// Parses and validates `vowp.toml` when one exists.
fn check_config(project_root: Option<&Path>, paths: &vowp_store::NodePaths, checks: &mut Checks) -> Option<VowpConfig> {
    let Some(root) = project_root else {
        checks.push("config", CheckStatus::Warn, "no vowp.toml found");
        return None;
    };
    let config_path = root.join("vowp.toml");
    match ctx::load_project_config(&config_path, paths) {
        Ok(config) => {
            checks.push(
                "config",
                CheckStatus::Ok,
                format!("{} parses and validates", config_path.display()),
            );
            Some(config)
        }
        Err(error) => {
            checks.push("config", CheckStatus::Fail, error.to_string());
            None
        }
    }
}

/// Parses every configured schedule with the real cron/interval parser.
///
/// `VowpConfig::validate` only checks the structure (five cron fields, interval shape, timezone
/// shape); this catches expressions such as `61 9 * * *` that it lets through.
fn check_schedules(config: Option<&VowpConfig>, checks: &mut Checks) {
    let Some(config) = config else {
        checks.push("schedules", CheckStatus::Warn, "no config to check");
        return;
    };
    if config.schedules.is_empty() {
        checks.push("schedules", CheckStatus::Ok, "no schedules configured");
        return;
    }
    let mut problems = Vec::new();
    for schedule in &config.schedules {
        let tz = schedule.tz.as_deref().unwrap_or("UTC");
        let parsed = match (&schedule.cron, &schedule.every) {
            (Some(cron), None) => ScheduleSpec::parse(ScheduleSpecKind::Cron, cron, tz),
            (None, Some(every)) => ScheduleSpec::parse(ScheduleSpecKind::Interval, every, tz),
            _ => {
                problems.push(format!("{}: exactly one of cron and every is required", schedule.name));
                continue;
            }
        };
        if let Err(error) = parsed {
            problems.push(format!("{}: {error}", schedule.name));
        }
    }
    if problems.is_empty() {
        let count = config.schedules.len();
        let noun = if count == 1 { "schedule" } else { "schedules" };
        checks.push("schedules", CheckStatus::Ok, format!("{count} {noun} parse"));
    } else {
        checks.push("schedules", CheckStatus::Fail, problems.join("; "));
    }
}

/// Probes one harness binary through its adapter.
fn check_harness(registry: &AdapterRegistry, harness: HarnessKind, name: &str, checks: &mut Checks) {
    let executor = Executor {
        harness,
        model: "probe".to_owned(),
        effort: None,
        variant: None,
    };
    let Some(adapter) = registry.get(&executor) else {
        checks.push(
            &format!("harness:{name}"),
            CheckStatus::Fail,
            "no adapter is registered for this harness",
        );
        return;
    };
    let probe = adapter.probe();
    let detail = match &probe.version {
        Some(version) => format!("{version} ({})", probe.detail),
        None => probe.detail.clone(),
    };
    checks.push(
        &format!("harness:{name}"),
        if probe.ok { CheckStatus::Ok } else { CheckStatus::Fail },
        detail,
    );
}

/// Checks that every profile rung resolves to an installed harness.
fn check_profiles(registry: Option<&AdapterRegistry>, config: Option<&VowpConfig>, checks: &mut Checks) {
    let Some(config) = config else {
        checks.push("profiles", CheckStatus::Warn, "no config to check");
        return;
    };
    let Some(registry) = registry else {
        checks.push("profiles", CheckStatus::Fail, "adapter registry unavailable");
        return;
    };
    let mut problems = Vec::new();
    let mut rungs = 0_u32;
    for role in ["planner", "keeper", "worker", "verifier"] {
        let Some(profile) = config.profiles.get(role) else {
            problems.push(format!("missing profile {role}"));
            continue;
        };
        for executor in &profile.ladder {
            rungs += 1;
            match registry.get(executor) {
                Some(adapter) if adapter.probe().ok => {}
                Some(_) => problems.push(format!("{executor}: harness not runnable")),
                None => problems.push(format!("{executor}: no adapter")),
            }
        }
    }
    if problems.is_empty() {
        checks.push(
            "profiles",
            CheckStatus::Ok,
            format!("{rungs} rungs resolve to installed harnesses"),
        );
    } else {
        checks.push("profiles", CheckStatus::Fail, problems.join("; "));
    }
}

/// Reports provider environment-variable names only, never their values.
fn check_providers(config: Option<&VowpConfig>, checks: &mut Checks) {
    let Some(config) = config else {
        checks.push("providers", CheckStatus::Warn, "no config to check");
        return;
    };
    let mut providers = BTreeSet::new();
    for name in config.providers.keys() {
        providers.insert(name.clone());
    }
    for profile in config.profiles.values() {
        for executor in &profile.ladder {
            providers.insert(executor.provider());
        }
    }
    if providers.is_empty() {
        checks.push("providers", CheckStatus::Warn, "no providers configured");
        return;
    }
    let mut parts = Vec::new();
    let mut all_present = true;
    for provider in providers {
        let variable = format!("{}_API_KEY", provider_env_prefix(&provider));
        let present = std::env::var_os(&variable).is_some();
        all_present &= present;
        parts.push(format!("{variable}: {}", if present { "present" } else { "absent" }));
    }
    checks.push(
        "providers",
        if all_present {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        parts.join(", "),
    );
}

/// Converts a provider key into an environment-variable prefix.
fn provider_env_prefix(provider: &str) -> String {
    let mut prefix = String::new();
    for character in provider.chars() {
        if character.is_ascii_alphanumeric() {
            prefix.push(character.to_ascii_uppercase());
        } else {
            prefix.push('_');
        }
    }
    prefix
}

/// Checks for the systemd user unit installed by the ops scripts.
fn check_systemd(checks: &mut Checks) {
    match home_dir() {
        Some(home) => {
            let unit = home.join(".config/systemd/user/vowp.service");
            checks.push(
                "systemd",
                if unit.is_file() {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                },
                format!("{}", unit.display()),
            );
        }
        None => checks.push("systemd", CheckStatus::Warn, "HOME is not set"),
    }
}

/// Checks for the cron watchdog script installed by the ops scripts.
fn check_watchdog(checks: &mut Checks) {
    match home_dir() {
        Some(home) => {
            let script = home.join(".local/bin/vowp-watchdog.sh");
            checks.push(
                "watchdog",
                if script.is_file() {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                },
                format!("{}", script.display()),
            );
        }
        None => checks.push("watchdog", CheckStatus::Warn, "HOME is not set"),
    }
}

/// Reports whether a supervisor holds the node lock.
fn check_supervisor(paths: &vowp_store::NodePaths, checks: &mut Checks) {
    match ctx::supervisor_pid(paths) {
        Some(pid) => checks.push("supervisor", CheckStatus::Ok, format!("running (pid {pid})")),
        None => checks.push("supervisor", CheckStatus::Warn, "not running"),
    }
}

/// Reports free disk space on the home filesystem; fails below 1 GB.
fn check_disk(runner: &dyn CommandRunner, root: &Path, checks: &mut Checks) {
    let path = if root.exists() {
        root.to_path_buf()
    } else {
        root.parent().unwrap_or_else(|| Path::new("/")).to_path_buf()
    };
    let spec = CommandSpec {
        shell: None,
        argv: vec!["df".to_owned(), "-Pk".to_owned(), path.display().to_string()],
        cwd: None,
        env: Vec::new(),
        timeout: Duration::from_secs(10),
    };
    match runner.run(&spec) {
        Ok(output) if output.exit_code == Some(0) => {
            let text = output.text();
            let last = text.lines().rfind(|line| !line.trim().is_empty());
            let available_kb = last
                .and_then(|line| line.split_whitespace().nth(3))
                .and_then(|value| value.parse::<u64>().ok());
            match available_kb {
                Some(kb) if kb >= 1_048_576 => {
                    checks.push("disk", CheckStatus::Ok, format!("{} GB available", kb / 1_048_576))
                }
                Some(kb) => checks.push(
                    "disk",
                    CheckStatus::Fail,
                    format!("{} MB available; at least 1 GB is required", kb / 1024),
                ),
                None => checks.push("disk", CheckStatus::Warn, "could not parse df output"),
            }
        }
        _ => checks.push("disk", CheckStatus::Warn, "df is not available"),
    }
}

/// Returns `$HOME` when it is set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
