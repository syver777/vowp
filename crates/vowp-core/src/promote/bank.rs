//! Snapshot the serving state before a deploy, so every promotion has a rollback.
//! Spec: `docs/design/crates/vowp-core.md` §8 and `docs/design/concepts/work-to-production.md`.
//!
//! The snapshot command is the environment's `serving_bank` command. VOWP chooses the
//! destination path and passes it in `VOWP_BANK_PATH`; `{target}` in the command is replaced
//! with the target name. The command must create that path. The resulting
//! [`BankedServing::rollback_cmd`] appends the path to the environment's `rollback` command,
//! or restores the archive with `tar` when no rollback command is configured.

use std::path::{Path, PathBuf};
use std::time::Duration;

use vowp_protocol::EnvConfig;

use crate::CoreError;
use crate::deps::{CommandSpec, Deps};

/// Wall-clock limit for the serving-bank command.
const BANK_TIMEOUT_SECS: u64 = 1800;

/// The snapshot taken before a deploy and the command that restores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankedServing {
    /// Path chosen for the snapshot; the bank command writes it.
    pub path: PathBuf,
    /// Commit the environment checkout was serving, when it is a git checkout.
    pub commit: Option<String>,
    /// Exact shell command that restores the snapshot.
    pub rollback_cmd: String,
}

/// Runs the environment's `serving_bank` command and records what it captured.
///
/// `target` is the promotion target (the environment name) substituted for `{target}`.
/// `to` is the snapshot path; its parent directory is created. When the environment has no
/// `serving_bank` command the checkout itself is treated as the serving copy and no file is
/// required. A failing, timed-out or silent command is an error: deploying without a rollback
/// is not allowed.
pub fn bank_serving(deps: &Deps, env: &EnvConfig, target: &str, to: &Path) -> Result<BankedServing, CoreError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    if let Some(command) = &env.serving_bank {
        let command = command.replace("{target}", target);
        let output = deps.cmd.run(&CommandSpec {
            shell: Some(command.clone()),
            argv: Vec::new(),
            cwd: Some(PathBuf::from(&env.checkout)),
            env: vec![
                ("VOWP_BANK_PATH".to_owned(), to.display().to_string()),
                ("VOWP_BANK_TARGET".to_owned(), target.to_owned()),
            ],
            timeout: Duration::from_secs(BANK_TIMEOUT_SECS),
        })?;
        if output.timed_out {
            return Err(CoreError::Command(format!(
                "serving_bank timed out after {BANK_TIMEOUT_SECS}s: {command}"
            )));
        }
        if output.exit_code != Some(0) {
            return Err(CoreError::Command(format!(
                "serving_bank exited {:?}: {command}\n{}",
                output.exit_code,
                output.tail_lines(20)
            )));
        }
        if !to.exists() {
            return Err(CoreError::Command(format!(
                "serving_bank did not create {} (set VOWP_BANK_PATH in the script)",
                to.display()
            )));
        }
    }

    let commit = deps
        .git
        .head_sha(Path::new(&env.checkout), "HEAD")
        .ok()
        .filter(|sha| !sha.trim().is_empty());
    let rollback_cmd = if env.serving_bank.is_some() {
        rollback_command(env, target, to)
    } else {
        env.rollback
            .as_ref()
            .map(|command| command.replace("{target}", target))
            .unwrap_or_default()
    };
    Ok(BankedServing {
        path: to.to_path_buf(),
        commit,
        rollback_cmd,
    })
}

/// Builds the command that restores `snapshot`, preferring the environment's rollback command.
fn rollback_command(env: &EnvConfig, target: &str, snapshot: &Path) -> String {
    let snapshot = snapshot.display().to_string();
    match &env.rollback {
        Some(command) => {
            let command = command.replace("{target}", target);
            if command.contains("{path}") {
                command.replace("{path}", &snapshot)
            } else {
                format!("{command} '{}'", snapshot.replace('\'', "'\\''"))
            }
        }
        None => format!(
            "tar -xf '{}' -C '{}'",
            snapshot.replace('\'', "'\\''"),
            env.checkout.replace('\'', "'\\''")
        ),
    }
}
