//! Environment guards that must pass before anything is deployed.
//! Spec: `docs/design/crates/vowp-core.md` §8 and `docs/design/concepts/work-to-production.md`.
//!
//! A guard either refuses the release ([`GuardResult::passed`] is false) or passes. A guard
//! never works around a failed check: a refusal is recorded and the promotion stops. Names of
//! the form `name:command` run that custom command in the environment checkout; the built-in
//! names are listed in [`BUILTIN`].

use std::path::Path;
use std::time::Duration;

use vowp_protocol::{CmdGate, EnvConfig, Gate, GuardResult, JobState};
use vowp_store::JobFilter;

use crate::CoreError;
use crate::deps::{CommandSpec, Deps};
use crate::gates;

/// The built-in guard names. Every name also accepts a `:command` suffix.
pub const BUILTIN: &[&str] = &[
    "clean_tree",
    "head_equals_remote",
    "build",
    "no_active_jobs",
    "upload_set",
];

/// Wall-clock limit for one guard or custom guard command.
const GUARD_TIMEOUT_SECS: u64 = 1800;

/// The states in which a job still holds its worktree and ownership lock.
const ACTIVE_STATES: [JobState; 6] = [
    JobState::Running,
    JobState::Stalled,
    JobState::Gating,
    JobState::Reviewing,
    JobState::Fixing,
    JobState::Merging,
];

/// Runs one named guard in `checkout`.
///
/// `name` is the configured guard string. A `name:command` form runs `command` with `sh -c`
/// in the checkout and passes on exit 0; the custom command overrides the built-in check.
/// Ordinary check failures are returned as a failed [`GuardResult`], not as an error, so the
/// caller records the refusal. [`CoreError`] is reserved for infrastructure failures the
/// caller cannot turn into evidence (for example an unreadable database).
pub fn run_guard(deps: &Deps, _env: &EnvConfig, name: &str, checkout: &Path) -> Result<GuardResult, CoreError> {
    let (base, custom) = match name.split_once(':') {
        Some((base, command)) => (base, Some(command)),
        None => (name, None),
    };

    if let Some(command) = custom {
        return Ok(run_custom(deps, name, command, checkout));
    }

    let result = match base {
        "clean_tree" => clean_tree(deps, checkout),
        "head_equals_remote" => head_equals_remote(deps, checkout),
        "build" => build(deps, checkout),
        "no_active_jobs" => no_active_jobs(deps)?,
        "upload_set" => failed(
            name,
            "upload_set requires a custom command: upload_set:<command> (for example a .vercelignore check)",
        ),
        other => failed(name, format!("unknown guard: {other}")),
    };
    Ok(result)
}

/// Passes when the checkout has no staged, unstaged or untracked changes.
fn clean_tree(deps: &Deps, checkout: &Path) -> GuardResult {
    match deps.git.status_clean(checkout) {
        Ok(true) => passed("clean_tree", "working tree is clean"),
        Ok(false) => failed("clean_tree", "working tree has uncommitted or untracked changes"),
        Err(err) => failed("clean_tree", format!("could not inspect the working tree: {err}")),
    }
}

/// Passes when the checkout's HEAD equals the remote default branch.
///
/// The remote and branch come from `[project]`; the checkout is fetched first so a stale
/// remote-tracking ref cannot make a behind checkout look current.
fn head_equals_remote(deps: &Deps, checkout: &Path) -> GuardResult {
    let remote = &deps.config.project.remote;
    let branch = &deps.config.project.default_branch;

    if let Err(err) = deps.git.fetch(checkout, remote) {
        return failed("head_equals_remote", format!("could not fetch remote {remote}: {err}"));
    }
    let local = match deps.git.head_sha(checkout, "HEAD") {
        Ok(sha) => sha,
        Err(err) => return failed("head_equals_remote", format!("could not resolve HEAD: {err}")),
    };
    let remote_output = match deps.git.ls_remote(checkout, remote, branch) {
        Ok(output) => output,
        Err(err) => {
            return failed("head_equals_remote", format!("could not read {remote}/{branch}: {err}"));
        }
    };
    let remote_sha = remote_output
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_default();
    if remote_sha.is_empty() {
        return failed("head_equals_remote", format!("remote {remote} has no branch {branch}"));
    }
    if local == remote_sha {
        passed("head_equals_remote", short(&local))
    } else {
        failed(
            "head_equals_remote",
            format!("HEAD {} is not {remote}/{branch} {}", short(&local), short(remote_sha)),
        )
    }
}

/// Passes when the project's build/check commands succeed in the checkout.
///
/// `EnvConfig` has no build command of its own; the guard uses `[merge].check_cmds` when set
/// and falls back to `[project].check_cmds`, the same repo-wide checks the merge queue runs.
/// A project that wants a different build command can configure `build:<command>`.
fn build(deps: &Deps, checkout: &Path) -> GuardResult {
    let config = &deps.config;
    let commands = if config.merge.check_cmds.is_empty() {
        &config.project.check_cmds
    } else {
        &config.merge.check_cmds
    };
    if commands.is_empty() {
        return passed("build", "no build commands configured; nothing to run");
    }
    for command in commands {
        match run_shell(deps, command, checkout, GUARD_TIMEOUT_SECS) {
            Ok(output) if !output.timed_out && output.exit_code == Some(0) => {}
            Ok(output) => {
                return failed(
                    "build",
                    format!(
                        "build command {command:?} failed: exit {:?}{}",
                        output.exit_code,
                        if output.timed_out { " (timed out)" } else { "" }
                    ),
                );
            }
            Err(err) => {
                return failed("build", format!("build command {command:?} could not run: {err}"));
            }
        }
    }
    passed("build", format!("{} build command(s) passed", commands.len()))
}

/// Passes when no job of the configured project is running or otherwise holding its worktree.
fn no_active_jobs(deps: &Deps) -> Result<GuardResult, CoreError> {
    let store = deps.open_store()?;
    let filter = JobFilter {
        project: Some(deps.config.project.name.clone()),
        states: ACTIVE_STATES.to_vec(),
        ..JobFilter::default()
    };
    let jobs = store.list_jobs(&filter)?;
    if jobs.is_empty() {
        return Ok(passed("no_active_jobs", "no active jobs"));
    }
    let ids: Vec<&str> = jobs.iter().map(|job| job.id.as_str()).collect();
    Ok(failed("no_active_jobs", format!("active jobs: {}", ids.join(", "))))
}

/// Runs a custom `name:command` guard through the same anti-pattern checks as a command gate.
fn run_custom(deps: &Deps, name: &str, command: &str, checkout: &Path) -> GuardResult {
    let gate = CmdGate {
        run: command.to_owned(),
        ..CmdGate::default()
    };
    if let Err(err) = gates::validate_gate(&Gate::Cmd(gate)) {
        return failed(name, format!("custom guard is not allowed: {err}"));
    }
    match run_shell(deps, command, checkout, GUARD_TIMEOUT_SECS) {
        Ok(output) if !output.timed_out && output.exit_code == Some(0) => passed(name, output.tail_lines(20)),
        Ok(output) => failed(
            name,
            format!(
                "custom guard {command:?} failed: exit {:?}{}\n{}",
                output.exit_code,
                if output.timed_out { " (timed out)" } else { "" },
                output.tail_lines(20)
            ),
        ),
        Err(err) => failed(name, format!("custom guard {command:?} could not run: {err}")),
    }
}

/// Runs one shell command in `cwd`.
fn run_shell(
    deps: &Deps,
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
) -> Result<crate::deps::CommandOutput, CoreError> {
    deps.cmd.run(&CommandSpec {
        shell: Some(command.to_owned()),
        argv: Vec::new(),
        cwd: Some(cwd.to_path_buf()),
        env: Vec::new(),
        timeout: Duration::from_secs(timeout_secs.max(1)),
    })
}

/// Builds a passing result.
fn passed(name: &str, detail: impl Into<String>) -> GuardResult {
    GuardResult {
        name: name.to_owned(),
        passed: true,
        detail: detail.into(),
    }
}

/// Builds a failing result.
fn failed(name: &str, detail: impl Into<String>) -> GuardResult {
    GuardResult {
        name: name.to_owned(),
        passed: false,
        detail: detail.into(),
    }
}

/// Returns a short, display-friendly prefix of a commit sha.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}
