//! Process and state-directory isolation checks for the service and run modes.
//!
//! Spec: `docs/design/v0.4/hardening.md` §3.4, §3.6 and `docs/design/v0.4/local-orchestrator.md`
//! §10.3. The agent never runs as root, its state root is owned by the agent uid and is not
//! group- or world-accessible, and a foreign owner or a loose mode is a refusal that names the
//! path and the expected owner/mode. The checks never follow a symlink out of the state root.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The required mode of the state root and its private directories.
pub const STATE_DIR_MODE: u32 = 0o700;
/// The required mode of a private file.
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// A refusal from the isolation checks.
#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    /// A path is owned by another uid.
    #[error("{path} is owned by uid {found}; the agent runs as uid {expected}")]
    ForeignOwner {
        /// Offending path.
        path: PathBuf,
        /// Expected owner.
        expected: u32,
        /// Owner found on disk.
        found: u32,
    },
    /// A directory grants group or world access.
    #[error("{path} is mode {mode:03o}; it must be {expected:03o} (no group or world access)")]
    LooseMode {
        /// Offending path.
        path: PathBuf,
        /// Required mode.
        expected: u32,
        /// Mode found on disk.
        mode: u32,
    },
    /// A private file grants group or world access.
    #[error("{path} is mode {mode:03o}; a private file must be {expected:03o}")]
    LooseFile {
        /// Offending path.
        path: PathBuf,
        /// Required mode.
        expected: u32,
        /// Mode found on disk.
        mode: u32,
    },
    /// The checked path is a symbolic link and the check refuses to follow it.
    #[error("{path} must be a real directory, not a symbolic link")]
    Symlink {
        /// Offending path.
        path: PathBuf,
    },
    /// The checked path exists but is not a directory.
    #[error("{path} is not a directory")]
    NotADirectory {
        /// Offending path.
        path: PathBuf,
    },
    /// The state directory has not been created yet.
    #[error("the agent state directory is missing: {0}")]
    Missing(PathBuf),
    /// The process is root.
    #[error("the agent must not run as root; use a standard user account")]
    Root,
    /// A filesystem or process lookup failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The current uid could not be determined.
    #[error("{0}")]
    Config(String),
}

impl IsolationError {
    /// Returns the documented CLI exit code for this refusal.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Missing(_) | Self::Config(_) => 2,
            Self::ForeignOwner { .. } | Self::LooseMode { .. } | Self::LooseFile { .. } | Self::Symlink { .. } => 5,
            Self::NotADirectory { .. } | Self::Root => 5,
            Self::Io(_) => 1,
        }
    }
}

/// Refuses uid 0. This is a pure predicate so the root test needs no root process.
pub fn ensure_not_root(uid: u32) -> Result<(), IsolationError> {
    if uid == 0 { Err(IsolationError::Root) } else { Ok(()) }
}

/// Returns the effective uid of this process.
///
/// Linux reads `/proc/self/status`; macOS and other Unixes ask `id -u`. Neither reads a secret.
pub fn current_uid() -> Result<u32, IsolationError> {
    #[cfg(target_os = "linux")]
    if let Ok(text) = fs::read_to_string("/proc/self/status") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(field) = rest.split_whitespace().next()
            {
                return field
                    .parse::<u32>()
                    .map_err(|_| IsolationError::Config("the process uid is not an integer".to_owned()));
            }
        }
    }
    let output = std::process::Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(IsolationError::Config(
            "could not determine the current uid with `id -u`".to_owned(),
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|_| IsolationError::Config("`id -u` did not return an integer".to_owned()))
}

/// Checks that `path` is a real directory owned by `uid` with no group or world access.
pub fn check_state_dir(path: &Path, uid: u32) -> Result<(), IsolationError> {
    check_dir(path, uid, STATE_DIR_MODE)?;
    Ok(())
}

/// Checks that `path` is a private file (0600) owned by `uid`.
pub fn check_private_file(path: &Path, uid: u32) -> Result<(), IsolationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(IsolationError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(IsolationError::Config(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(IsolationError::LooseFile {
            path: path.to_path_buf(),
            expected: PRIVATE_FILE_MODE,
            mode,
        });
    }
    if metadata.uid() != uid {
        return Err(IsolationError::ForeignOwner {
            path: path.to_path_buf(),
            expected: uid,
            found: metadata.uid(),
        });
    }
    Ok(())
}

fn check_dir(path: &Path, uid: u32, expected: u32) -> Result<(), IsolationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(IsolationError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_dir() {
        return Err(IsolationError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(IsolationError::LooseMode {
            path: path.to_path_buf(),
            expected,
            mode,
        });
    }
    if metadata.uid() != uid {
        return Err(IsolationError::ForeignOwner {
            path: path.to_path_buf(),
            expected: uid,
            found: metadata.uid(),
        });
    }
    Ok(())
}

/// Creates the state root 0700 if it is missing, then checks it.
///
/// The parent (`$HOME`) is not rewritten. A state root that is a symlink or another owner is
/// refused before anything is written below it.
pub fn ensure_state_dir(path: &Path, uid: u32) -> Result<(), IsolationError> {
    match fs::symlink_metadata(path) {
        Ok(_) => check_state_dir(path, uid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(STATE_DIR_MODE))?;
            check_state_dir(path, uid)
        }
        Err(error) => Err(IsolationError::Io(error)),
    }
}

/// Creates and checks `<state_dir>/logs` 0700.
pub fn ensure_log_dir(state_dir: &Path, uid: u32) -> Result<PathBuf, IsolationError> {
    ensure_state_dir(state_dir, uid)?;
    let logs = state_dir.join("logs");
    fs::create_dir_all(&logs)?;
    fs::set_permissions(&logs, fs::Permissions::from_mode(STATE_DIR_MODE))?;
    check_dir(&logs, uid, STATE_DIR_MODE)?;
    Ok(logs)
}

/// Checks that a parent directory (for example `~/Library/LaunchAgents`) exists and is owned
/// by `uid`; it may be group-readable because the OS owns the convention, but never by another
/// user.
pub fn check_unit_parent(path: &Path, uid: u32) -> Result<(), IsolationError> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(IsolationError::Symlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_dir() {
        return Err(IsolationError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    if metadata.uid() != uid {
        return Err(IsolationError::ForeignOwner {
            path: path.to_path_buf(),
            expected: uid,
            found: metadata.uid(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_predicate_is_pure_and_refuses_uid_zero() {
        assert!(matches!(ensure_not_root(0), Err(IsolationError::Root)));
        ensure_not_root(501).expect("a standard uid is allowed");
        ensure_not_root(u32::MAX).expect("a high uid is allowed");
    }

    #[test]
    fn a_loose_state_dir_is_refused_with_its_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let uid = current_uid().expect("uid");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("mode");
        let error = check_state_dir(dir.path(), uid).expect_err("a loose mode is refused");
        match error {
            IsolationError::LooseMode { expected, mode, .. } => {
                assert_eq!(expected, STATE_DIR_MODE);
                assert_eq!(mode, 0o755);
            }
            other => panic!("expected LooseMode, got {other:?}"),
        }
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        check_state_dir(dir.path(), uid).expect("0700 is accepted");
    }

    #[test]
    fn a_foreign_owner_is_refused_with_the_expected_uid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let uid = current_uid().expect("uid");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("mode");
        let other = uid.wrapping_add(1);
        let error = check_state_dir(dir.path(), other).expect_err("a foreign owner is refused");
        assert!(error.to_string().contains(&format!("uid {uid}")), "{error}");
        assert!(error.to_string().contains(&format!("uid {other}")), "{error}");
    }

    #[test]
    fn a_symlink_state_dir_is_refused() {
        let target = tempfile::tempdir().expect("temp dir");
        let holder = tempfile::tempdir().expect("temp dir");
        let link = holder.path().join("agent");
        std::os::unix::fs::symlink(target.path(), &link).expect("symlink");
        let error = check_state_dir(&link, current_uid().expect("uid")).expect_err("a symlink is refused");
        assert!(matches!(error, IsolationError::Symlink { .. }), "{error:?}");
    }

    #[test]
    fn ensure_state_dir_creates_0700_and_keeps_a_private_file_0600() {
        let holder = tempfile::tempdir().expect("temp dir");
        let state = holder.path().join("agent");
        let uid = current_uid().expect("uid");
        ensure_state_dir(&state, uid).expect("create state dir");
        let mode = fs::metadata(&state).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, STATE_DIR_MODE);
        let file = state.join("run");
        fs::write(&file, b"").expect("marker");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("mode");
        check_private_file(&file, uid).expect("0600 file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(matches!(
            check_private_file(&file, uid),
            Err(IsolationError::LooseFile { .. })
        ));
    }
}
