//! Mac-side worktree and result-return primitives for offloaded runs.
//!
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3 steps 4–8, §6.4, §6.5 and §6.6. The
//! module owns three things the executor calls after `AssignRun`:
//!
//! - [`PathMap`] resolves a project-relative [`PathRef`] against one [`AgentPathMap`] without
//!   escaping the mapped roots.
//! - [`WorktreeManager`] creates, reuses and cleans exactly one worktree per job at
//!   `<worktree_root>/<job-id>` on branch `job/<id>`.
//! - [`Worktree::push_job`] returns only that branch to a verified project origin.
//!
//! It deliberately adds no frame loop, no harness start and no policy decision. Git runs through
//! the small [`GitRunner`] trait; the default [`SystemGit`] uses `std::process::Command`, while
//! tests pass a recording wrapper around the real binary. Absolute paths never leave this module.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use vowp_protocol::{AgentPathMap, JobId, MAX_PATH_REF_LEN, PathRef, ProjectName, RunRejectReason};

/// Default free-space reserve below the worktree root: 5 GiB.
pub const DEFAULT_DISK_RESERVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Prefix of the one branch a job may push.
pub const JOB_BRANCH_PREFIX: &str = "job/";

/// Maximum number of stderr bytes kept in a typed git error.
pub const STDERR_TAIL_BYTES: usize = 2048;

/// Marker file written in a worktree's private git directory.
const OWNERSHIP_MARKER: &str = "vowp-job";

/// Directory below the git common directory holding one ownership record per job.
const OWNERSHIP_DIR: &str = "vowp/worktrees";

/// Everything the worktree primitives can refuse or fail to do.
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// A local filesystem operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON ownership record could not be encoded or decoded.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The path ref belongs to another project.
    #[error("path ref project {actual} does not match project {expected}")]
    ProjectMismatch {
        /// Project the caller asked for.
        expected: ProjectName,
        /// Project named by the path ref.
        actual: ProjectName,
    },
    /// A path ref carries something the resolver will not follow.
    #[error("invalid path ref: {detail}")]
    InvalidPathRef {
        /// Human-readable reason; never a secret value.
        detail: String,
    },
    /// A path map root or allow entry is invalid.
    #[error("invalid path map entry {entry}: {detail}")]
    InvalidPathMapEntry {
        /// The entry as supplied by the home.
        entry: String,
        /// Human-readable reason.
        detail: String,
    },
    /// A mapped root or requested path does not exist.
    #[error("path {path} is missing")]
    PathMissing {
        /// Missing path.
        path: PathBuf,
    },
    /// A mapped root or requested path is not writable.
    #[error("path {path} is not writable")]
    PathNotWritable {
        /// Unwritable path.
        path: PathBuf,
    },
    /// A mapped root or requested path is not a directory.
    #[error("path {path} is not a directory")]
    NotADirectory {
        /// Path that is not a directory.
        path: PathBuf,
    },
    /// A resolved path is outside the root it must stay below.
    #[error("path {path} is outside {root}")]
    PathOutsideRoot {
        /// Resolved path.
        path: PathBuf,
        /// Root the path must stay below.
        root: PathBuf,
    },
    /// A symlink resolves outside the root it must stay below.
    #[error("symlink {path} resolves outside {root}")]
    SymlinkEscape {
        /// First symlink in the requested path.
        path: PathBuf,
        /// Root the path must stay below.
        root: PathBuf,
    },
    /// A path component matches an on-disk entry only by case.
    #[error("case-only alias: {requested} resolves to {resolved}")]
    CaseOnlyEscape {
        /// Path as requested.
        requested: PathBuf,
        /// Path as the filesystem resolved it.
        resolved: PathBuf,
    },
    /// A requested subdirectory is outside the project's allow list.
    #[error("path {path} is outside the project allow list")]
    OutsideAllow {
        /// Rejected canonical path.
        path: PathBuf,
    },
    /// A project checkout, worktree root or git directory is not private to the agent uid.
    #[error("unsafe checkout {path}: {detail}; it must be owned by the current user and not group/world-writable")]
    UnsafeCheckout {
        /// Offending canonical path.
        path: PathBuf,
        /// What was found: the owner or the mode.
        detail: String,
    },
    /// The mapped local root is not a git checkout.
    #[error("not a git checkout: {path}")]
    NotAGitCheckout {
        /// Root that was expected to be a checkout.
        path: PathBuf,
    },
    /// The supplied base commit is not a full object name.
    #[error("invalid base commit {commit}")]
    InvalidCommit {
        /// Rejected commit value.
        commit: String,
    },
    /// The supplied remote name is not a plain git remote name.
    #[error("invalid git remote {remote}")]
    InvalidRemote {
        /// Rejected remote value.
        remote: String,
    },
    /// The base commit is absent locally and could not be fetched.
    #[error("base commit {commit} is not available from {remote}: {detail}")]
    BaseCommitMissing {
        /// Commit the home supplied.
        commit: String,
        /// Remote that was asked.
        remote: String,
        /// Last git stderr tail, when any.
        detail: String,
    },
    /// Free space is below the configured reserve.
    #[error("free disk {free} bytes is below the {reserve}-byte reserve")]
    DiskLow {
        /// Free bytes measured below the worktree root.
        free: u64,
        /// Reserve the caller supplied.
        reserve: u64,
    },
    /// A path exists that VOWP does not own or cannot reuse.
    #[error("worktree conflict at {path}: {detail}")]
    WorktreeConflict {
        /// Conflicting path.
        path: PathBuf,
        /// Human-readable reason.
        detail: String,
    },
    /// A local fetch or push URL does not match the project remote.
    #[error("origin {actual} does not match project remote {expected}")]
    RemoteMismatch {
        /// Home-supplied project remote URL.
        expected: String,
        /// Mismatching push URL, with credentials stripped.
        actual: String,
    },
    /// A git command other than the result push failed.
    #[error("git {command} failed: {stderr}")]
    GitFailed {
        /// Command that failed.
        command: String,
        /// Last [`STDERR_TAIL_BYTES`] of stderr.
        stderr: String,
    },
    /// The result push failed; the worktree is kept for inspection.
    #[error("git push failed: {command}: {stderr}")]
    GitPushFailed {
        /// Push command that failed.
        command: String,
        /// Last [`STDERR_TAIL_BYTES`] of stderr.
        stderr: String,
    },
    /// Cleanup refused to touch something VOWP did not create.
    #[error("refused: {detail}")]
    Refused {
        /// Human-readable reason.
        detail: String,
    },
}

impl WorktreeError {
    /// Maps the error to the protocol's assignment refusal vocabulary.
    pub fn reject_reason(&self) -> RunRejectReason {
        match self {
            Self::PathMissing { .. } => RunRejectReason::PathMissing,
            Self::PathNotWritable { .. } | Self::NotADirectory { .. } => RunRejectReason::PathNotWritable,
            Self::OutsideAllow { .. } => RunRejectReason::CwdOutsideAllow,
            Self::DiskLow { .. } => RunRejectReason::DiskLow,
            Self::WorktreeConflict { .. } => RunRejectReason::WorktreeConflict,
            Self::RemoteMismatch { .. } => RunRejectReason::OriginMismatch,
            Self::Io(_)
            | Self::Json(_)
            | Self::ProjectMismatch { .. }
            | Self::InvalidPathRef { .. }
            | Self::InvalidPathMapEntry { .. }
            | Self::PathOutsideRoot { .. }
            | Self::SymlinkEscape { .. }
            | Self::CaseOnlyEscape { .. }
            | Self::UnsafeCheckout { .. }
            | Self::NotAGitCheckout { .. }
            | Self::InvalidCommit { .. }
            | Self::InvalidRemote { .. }
            | Self::BaseCommitMissing { .. }
            | Self::GitFailed { .. }
            | Self::GitPushFailed { .. }
            | Self::Refused { .. } => RunRejectReason::Other,
        }
    }
}

/// One completed git invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    /// Process exit code; `None` when git was killed by a signal.
    pub status: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

impl GitOutput {
    /// Returns true when git exited with status 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// Returns the last [`STDERR_TAIL_BYTES`] of stderr, trimmed at the end.
    pub fn stderr_tail(&self) -> String {
        stderr_tail(&self.stderr)
    }
}

/// Runs git commands for the worktree lifecycle.
///
/// The executor and tests both use this boundary. Implementations must be Send + Sync because
/// one manager can be shared by a worker thread.
pub trait GitRunner: Send + Sync {
    /// Runs `git` in `cwd` with `args` and captures its output.
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError>;
}

/// The real `git` binary, run through `std::process::Command`.
#[derive(Debug, Clone)]
pub struct SystemGit {
    binary: String,
}

impl SystemGit {
    /// Uses the `git` found on `PATH`.
    pub fn new() -> Self {
        Self {
            binary: "git".to_owned(),
        }
    }

    /// Uses an explicit git binary.
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self { binary: binary.into() }
    }

    /// Returns the configured binary name.
    pub fn binary(&self) -> &str {
        &self.binary
    }
}

impl Default for SystemGit {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner for SystemGit {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError> {
        let output = Command::new(&self.binary)
            .arg("-C")
            .arg(cwd)
            .args(args)
            // Never wait for a credential prompt on a background run, and keep git's messages
            // parseable regardless of the owner's locale.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()?;
        Ok(GitOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// A validated, local-only path map for one project.
///
/// Construction canonicalises both roots and checks that they exist, are directories, carry a
/// write bit, are owned by the agent uid and are not group- or world-writable. Resolution never
/// returns a path outside those roots.
#[derive(Debug, Clone)]
pub struct PathMap {
    project: ProjectName,
    local_root: PathBuf,
    worktree_root: PathBuf,
    allow: Vec<Vec<String>>,
}

impl PathMap {
    /// Validates one node-supplied [`AgentPathMap`].
    ///
    /// `worktree_root` defaults to `<local_root>/.vowp/worktrees` when the map omits it. A
    /// missing root is [`WorktreeError::PathMissing`]; a root without a write bit is
    /// [`WorktreeError::PathNotWritable`]. A root owned by another uid or group- or
    /// world-writable is [`WorktreeError::UnsafeCheckout`]. Group/world read and execute are
    /// allowed, so a read-only shared mirror may be group-readable. Neither panics.
    pub fn new(map: &AgentPathMap) -> Result<Self, WorktreeError> {
        let local_root = validate_root(Path::new(&map.local_root), "local_root")?;
        let worktree_root = match &map.worktree_root {
            Some(root) => validate_root(Path::new(root), "worktree_root")?,
            None => {
                let default = local_root.join(".vowp").join("worktrees");
                validate_root(&default, "worktree_root")?
            }
        };
        let allow = validate_allow(&map.allow, &local_root)?;
        let paths = Self {
            project: map.project.clone(),
            local_root,
            worktree_root,
            allow,
        };
        paths.check_private_checkout()?;
        Ok(paths)
    }

    /// Rechecks that both roots are private to the agent uid.
    ///
    /// [`Self::new`] runs this check when the map is built; [`WorktreeManager::create`] runs it
    /// again at run start, because a checkout's owner or mode can change after startup. The
    /// roots must be directories owned by the effective uid with no group or world write bit
    /// (`mode & 0o022 == 0`). Group/world read and execute are allowed.
    pub fn check_private_checkout(&self) -> Result<(), WorktreeError> {
        let uid = effective_uid()?;
        check_private_dir(&self.local_root, uid)?;
        check_private_dir(&self.worktree_root, uid)
    }

    /// Returns the project this map belongs to.
    pub fn project(&self) -> &ProjectName {
        &self.project
    }

    /// Returns the canonical main checkout.
    pub fn local_root(&self) -> &Path {
        &self.local_root
    }

    /// Returns the canonical worktree root.
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    /// Resolves a home-supplied [`PathRef`] to an absolute local path.
    ///
    /// A ref for another project is [`WorktreeError::ProjectMismatch`]. A subdirectory keeps
    /// every requested component normalised and canonicalises the result, so `..`, an absolute
    /// path, a NUL, a backslash, a symlink escape and a case-only alias are all refused.
    pub fn resolve(&self, project: &ProjectName, path_ref: &PathRef) -> Result<PathBuf, WorktreeError> {
        if project != &self.project {
            return Err(WorktreeError::ProjectMismatch {
                expected: project.clone(),
                actual: self.project.clone(),
            });
        }
        match path_ref {
            PathRef::Project { project: ref_project } => {
                self.check_ref_project(ref_project)?;
                Ok(self.local_root.clone())
            }
            PathRef::Worktree {
                project: ref_project,
                job,
            } => {
                self.check_ref_project(ref_project)?;
                self.worktree_path(job)
            }
            PathRef::Subdir {
                project: ref_project,
                rel,
            } => {
                self.check_ref_project(ref_project)?;
                self.resolve_subdir(rel)
            }
        }
    }

    /// Returns `<worktree_root>/<job>` without requiring the leaf to exist yet.
    ///
    /// An existing leaf must be a real directory below the worktree root. A symlink at the leaf
    /// is refused: [`WorktreeError::SymlinkEscape`] when it leaves the root and
    /// [`WorktreeError::WorktreeConflict`] otherwise. A case-only alias of the job directory is
    /// [`WorktreeError::CaseOnlyEscape`].
    pub fn worktree_path(&self, job: &JobId) -> Result<PathBuf, WorktreeError> {
        let name = job.as_str();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
        {
            return Err(WorktreeError::InvalidPathRef {
                detail: format!("invalid job directory {name:?}"),
            });
        }
        let requested = self.worktree_root.join(name);
        match fs::symlink_metadata(&requested) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(match fs::canonicalize(&requested) {
                        Ok(canonical) if !is_within(&canonical, &self.worktree_root) => WorktreeError::SymlinkEscape {
                            path: requested,
                            root: self.worktree_root.clone(),
                        },
                        Ok(_) => WorktreeError::WorktreeConflict {
                            path: requested,
                            detail: "worktree path is a symlink".to_owned(),
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorktreeError::WorktreeConflict {
                            path: requested,
                            detail: "worktree path is a broken symlink".to_owned(),
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                            WorktreeError::PathNotWritable { path: requested }
                        }
                        Err(error) => WorktreeError::Io(error),
                    });
                }
                if !metadata.is_dir() {
                    return Err(WorktreeError::NotADirectory { path: requested });
                }
                let canonical = fs::canonicalize(&requested)?;
                if !is_within(&canonical, &self.worktree_root) {
                    return Err(WorktreeError::PathOutsideRoot {
                        path: canonical,
                        root: self.worktree_root.clone(),
                    });
                }
                let leaf = canonical.file_name().and_then(|value| value.to_str()).unwrap_or("");
                if is_case_only(leaf, name) {
                    return Err(WorktreeError::CaseOnlyEscape {
                        requested: requested.clone(),
                        resolved: canonical,
                    });
                }
                Ok(canonical)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(requested),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(WorktreeError::PathNotWritable { path: requested })
            }
            Err(error) => Err(WorktreeError::Io(error)),
        }
    }

    fn check_ref_project(&self, project: &ProjectName) -> Result<(), WorktreeError> {
        if project == &self.project {
            Ok(())
        } else {
            Err(WorktreeError::ProjectMismatch {
                expected: self.project.clone(),
                actual: project.clone(),
            })
        }
    }

    fn resolve_subdir(&self, rel: &str) -> Result<PathBuf, WorktreeError> {
        let requested_components = rel_components(rel)?;
        let requested = self.local_root.join(requested_components.join("/"));
        let canonical = match fs::canonicalize(&requested) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(WorktreeError::PathMissing { path: requested });
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(WorktreeError::PathNotWritable { path: requested });
            }
            Err(error) => return Err(WorktreeError::Io(error)),
        };
        if !is_within(&canonical, &self.local_root) {
            return Err(match has_symlink(&self.local_root, &requested_components) {
                Some(path) => WorktreeError::SymlinkEscape {
                    path,
                    root: self.local_root.clone(),
                },
                None => WorktreeError::PathOutsideRoot {
                    path: canonical,
                    root: self.local_root.clone(),
                },
            });
        }
        let canonical_rel = canonical
            .strip_prefix(&self.local_root)
            .map_err(|_| WorktreeError::PathOutsideRoot {
                path: canonical.clone(),
                root: self.local_root.clone(),
            })
            .map(path_components)??;
        for (requested_component, actual) in requested_components.iter().zip(canonical_rel.iter()) {
            if is_case_only(requested_component, actual) {
                return Err(WorktreeError::CaseOnlyEscape {
                    requested: requested.clone(),
                    resolved: canonical,
                });
            }
        }
        if !self.is_allowed(&canonical_rel) {
            return Err(WorktreeError::OutsideAllow { path: canonical });
        }
        let metadata = fs::metadata(&canonical)?;
        if !metadata.is_dir() {
            return Err(WorktreeError::NotADirectory { path: canonical });
        }
        Ok(canonical)
    }

    fn is_allowed(&self, rel: &[String]) -> bool {
        self.allow.is_empty() || self.allow.iter().any(|entry| rel.starts_with(entry))
    }
}

/// Options for one worktree create-or-reuse call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSpec {
    /// Job whose worktree is created.
    pub job: JobId,
    /// Full 40- or 64-character base object name supplied by the home.
    pub base_commit: String,
    /// Remote to fetch a missing base from and to push the job branch to.
    pub remote: String,
    /// Free-space reserve in bytes checked before anything is touched.
    pub reserve_bytes: u64,
}

impl WorktreeSpec {
    /// Builds a spec with the `origin` remote and the default 5 GiB reserve.
    pub fn new(job: JobId, base_commit: impl Into<String>) -> Self {
        Self {
            job,
            base_commit: base_commit.into(),
            remote: "origin".to_owned(),
            reserve_bytes: DEFAULT_DISK_RESERVE_BYTES,
        }
    }

    /// Replaces the git remote name.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
        self
    }

    /// Replaces the free-space reserve.
    pub fn with_reserve(mut self, reserve_bytes: u64) -> Self {
        self.reserve_bytes = reserve_bytes;
        self
    }
}

/// The result of a successful `job/<id>` push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    /// Pushed branch, always `job/<id>`.
    pub branch: String,
    /// Remote the branch was pushed to.
    pub remote: String,
    /// Head commit the node reported to the home.
    pub head_commit: String,
}

/// One `base..head` git bundle created from a job worktree.
///
/// The bundle is a local staging artifact: its path never crosses the agent channel. Only
/// [`Self::branch`], [`Self::head`], [`Self::bytes`] and [`Self::sha256`] are sent in
/// `BundleStart`/`BundleEnd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobBundle {
    /// Absolute bundle file path on the node.
    pub path: PathBuf,
    /// The bundle's single branch, `job/<id>`.
    pub branch: String,
    /// Committed head the bundle returns.
    pub head: String,
    /// Bundle size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the bundle file.
    pub sha256: String,
}

impl JobBundle {
    /// Returns the bundle size in bytes.
    pub fn len(&self) -> u64 {
        self.bytes
    }

    /// Returns whether the bundle has no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
}

/// Creates exactly one `base..job/<id>` bundle in `dest_dir`.
///
/// The command is `git bundle create <file> <base>..job/<id>` in the job worktree: one
/// ref, no tags and no `--all`. Only committed objects are in the bundle, so a dirty
/// worktree is allowed and uncommitted files are not returned. The file is written 0600 in
/// a 0700 directory and hashed as it is read back.
pub fn create_job_bundle(
    git: &dyn GitRunner,
    worktree: &Path,
    job: &JobId,
    base: &str,
    dest_dir: &Path,
) -> Result<JobBundle, WorktreeError> {
    validate_commit(base)?;
    fs::create_dir_all(dest_dir)?;
    fs::set_permissions(dest_dir, fs::Permissions::from_mode(0o700))?;
    let path = dest_dir.join("result.gitbundle");
    remove_if_exists(&path)?;
    let branch = job_branch(job);
    let range = format!("{base}..{branch}");
    let Some(path_text) = path.to_str() else {
        return Err(WorktreeError::Refused {
            detail: "the bundle path is not valid UTF-8".to_owned(),
        });
    };
    let output = git.run(worktree, &["bundle", "create", path_text, range.as_str()])?;
    if !output.success() {
        return Err(WorktreeError::GitFailed {
            command: format!("git bundle create {range}"),
            stderr: output.stderr_tail(),
        });
    }
    let head = run_checked(
        git,
        worktree,
        &["rev-parse", &format!("refs/heads/{branch}")],
        "git rev-parse",
    )?;
    let head = head.stdout.trim().to_ascii_lowercase();
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorktreeError::Refused {
            detail: "the job branch did not resolve to a commit".to_owned(),
        });
    }
    let (bytes, sha256) = hash_file(&path)?;
    Ok(JobBundle {
        path,
        branch,
        head,
        bytes,
        sha256,
    })
}

/// Returns the size and lowercase hex SHA-256 of one file.
fn hash_file(path: &Path) -> Result<(u64, String), WorktreeError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        sha256.push_str(&format!("{byte:02x}"));
    }
    Ok((total, sha256))
}

/// Creates, reuses and cleans one worktree per job.
pub struct WorktreeManager {
    map: PathMap,
    git: Arc<dyn GitRunner>,
}

impl WorktreeManager {
    /// Builds a manager that uses the system git and a validated path map.
    pub fn new(map: PathMap) -> Self {
        Self {
            map,
            git: Arc::new(SystemGit::new()),
        }
    }

    /// Builds a manager around an injected git runner.
    pub fn with_git(map: PathMap, git: Arc<dyn GitRunner>) -> Self {
        Self { map, git }
    }

    /// Returns the manager's path map.
    pub fn map(&self) -> &PathMap {
        &self.map
    }

    /// Creates or safely reuses `<worktree_root>/<job-id>` on branch `job/<id>`.
    ///
    /// The disk reserve is checked before any existing worktree is inspected or removed, so a
    /// low disk never destroys work to make room. A missing base commit is fetched from the
    /// supplied remote; the main checkout is never checked out, reset or cleaned. An existing
    /// worktree is reused only when VOWP owns it, it is registered with the main checkout, it
    /// is on `job/<id>`, it is clean, and its head is exactly the base commit. Any other owned
    /// worktree is removed and recreated.
    pub fn create(&self, spec: &WorktreeSpec) -> Result<Worktree, WorktreeError> {
        validate_commit(&spec.base_commit)?;
        validate_remote(&spec.remote)?;
        self.map.check_private_checkout()?;
        let path = self.map.worktree_path(&spec.job)?;
        let branch = job_branch(&spec.job);

        let free = disk_free_bytes(&self.map.worktree_root);
        if free < spec.reserve_bytes {
            return Err(WorktreeError::DiskLow {
                free,
                reserve: spec.reserve_bytes,
            });
        }

        let common_dir = self.common_dir()?;
        // A group-writable `.git` is part of a shared checkout even when the working tree
        // itself looks private. Check it after git resolves the path and before any fetch,
        // inspect or create step.
        check_private_dir(&common_dir, effective_uid()?)?;
        let record_path = ownership_record_path(&common_dir, &spec.job);
        // Git prints object names in lowercase; normalise an uppercase home input so a clean
        // worktree at the same object is recognised for reuse.
        let base = self.ensure_commit(&spec.base_commit.to_ascii_lowercase(), &spec.remote)?;

        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(match fs::canonicalize(&path) {
                        Ok(canonical) if !is_within(&canonical, &self.map.worktree_root) => {
                            WorktreeError::SymlinkEscape {
                                path,
                                root: self.map.worktree_root.clone(),
                            }
                        }
                        Ok(_) => WorktreeError::WorktreeConflict {
                            path,
                            detail: "worktree path is a symlink".to_owned(),
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorktreeError::WorktreeConflict {
                            path,
                            detail: "worktree path is a broken symlink".to_owned(),
                        },
                        Err(error) => WorktreeError::Io(error),
                    });
                }
                if !metadata.is_dir() {
                    return Err(WorktreeError::WorktreeConflict {
                        path,
                        detail: "path exists and is not a directory".to_owned(),
                    });
                }
                let record = read_record(&record_path)?.ok_or_else(|| WorktreeError::WorktreeConflict {
                    path: path.clone(),
                    detail: "directory was not created by VOWP".to_owned(),
                })?;
                self.check_record(&record, spec, &path, &branch)?;
                if self.worktree_marker(&path)?.as_deref() != Some(spec.job.as_str()) {
                    return Err(WorktreeError::WorktreeConflict {
                        path,
                        detail: "worktree marker does not match this job".to_owned(),
                    });
                }
                if self.is_registered_worktree(&path, &common_dir)?
                    && self.current_branch(&path)?.as_deref() == Some(branch.as_str())
                    && self.head_at(&path)? == base
                    && self.is_clean_at(&path)?
                {
                    return Ok(self.worktree(spec, path, branch, common_dir, record_path));
                }
                self.remove_owned_worktree(&path, &common_dir)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if record_path.exists() {
                    // A previous create left an ownership record but no directory; the
                    // registration, if any, is ours, so it is safe to prune it.
                    remove_if_exists(&record_path)?;
                    self.prune_worktrees()?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(WorktreeError::PathNotWritable { path });
            }
            Err(error) => return Err(WorktreeError::Io(error)),
        }

        if fs::symlink_metadata(&path).is_ok() {
            return Err(WorktreeError::WorktreeConflict {
                path,
                detail: "path appeared during create".to_owned(),
            });
        }

        let path_string = path_string(&path)?;
        self.run_checked(
            &self.map.local_root,
            &["worktree", "add", "--detach", &path_string, &base],
            "git worktree add --detach",
        )?;
        let marker = (|| -> Result<(), WorktreeError> {
            let git_dir = absolute_git_dir(self.git.as_ref(), &path)?;
            write_owner_marker(&git_dir, &spec.job)?;
            write_record(
                &record_path,
                &OwnershipRecord {
                    project: self.map.project().as_str().to_owned(),
                    job: spec.job.as_str().to_owned(),
                    branch: branch.clone(),
                    path: path_string.clone(),
                    base_commit: base.clone(),
                },
            )?;
            self.run_checked(&path, &["checkout", "-B", &branch, &base], "git checkout -B")?;
            Ok(())
        })();
        if let Err(error) = marker {
            // The worktree is ours (we just added it); remove it before returning the original
            // failure. A secondary removal failure must not hide that failure.
            let _ = self.remove_owned_worktree(&path, &common_dir);
            return Err(error);
        }
        Ok(self.worktree(spec, path, branch, common_dir, record_path))
    }

    /// Removes only the worktree and `job/<id>` branch VOWP created for `job`.
    ///
    /// Cleanup is idempotent. A directory or branch without the ownership record, or with a
    /// mismatched marker, is [`WorktreeError::Refused`] and is left untouched.
    pub fn cleanup(&self, job: &JobId) -> Result<(), WorktreeError> {
        let path = self.map.worktree_path(job)?;
        let branch = job_branch(job);
        let common_dir = self.common_dir()?;
        let record_path = ownership_record_path(&common_dir, job);
        let record = read_record(&record_path)?;
        let path_exists = fs::symlink_metadata(&path).is_ok();
        let branch_exists = self.branch_exists(&branch)?;
        if !path_exists && !branch_exists {
            if let Some(record) = &record {
                if record.project != self.map.project().as_str() || record.job != job.as_str() {
                    return Err(WorktreeError::Refused {
                        detail: format!("ownership record for job {job} belongs to another project"),
                    });
                }
            }
            remove_if_exists(&record_path)?;
            return Ok(());
        }
        let Some(record) = record else {
            return Err(WorktreeError::Refused {
                detail: format!("worktree or branch for job {job} was not created by VOWP"),
            });
        };
        if record.project != self.map.project().as_str()
            || record.job != job.as_str()
            || record.branch != branch
            || record.path != path_string(&path)?
        {
            return Err(WorktreeError::Refused {
                detail: format!("ownership record for job {job} does not match"),
            });
        }
        cleanup_owned(
            self.git.as_ref(),
            &CleanupTarget {
                repo: &self.map.local_root,
                common_dir: &common_dir,
                path: &path,
                branch: &branch,
                project: self.map.project(),
                job,
                record_path: &record_path,
                path_exists,
                branch_exists,
            },
        )
    }

    fn worktree(
        &self,
        spec: &WorktreeSpec,
        path: PathBuf,
        branch: String,
        common_dir: PathBuf,
        record_path: PathBuf,
    ) -> Worktree {
        Worktree {
            project: self.map.project.clone(),
            job: spec.job.clone(),
            path,
            branch,
            remote: spec.remote.clone(),
            repo: self.map.local_root.clone(),
            common_dir,
            record_path,
            git: Arc::clone(&self.git),
        }
    }

    fn check_record(
        &self,
        record: &OwnershipRecord,
        spec: &WorktreeSpec,
        path: &Path,
        branch: &str,
    ) -> Result<(), WorktreeError> {
        if record.project != self.map.project().as_str()
            || record.job != spec.job.as_str()
            || record.branch != branch
            || record.path != path_string(path)?
        {
            return Err(WorktreeError::WorktreeConflict {
                path: path.to_path_buf(),
                detail: "ownership record does not match this job".to_owned(),
            });
        }
        Ok(())
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, WorktreeError> {
        self.git.run(cwd, args)
    }

    fn run_checked(&self, cwd: &Path, args: &[&str], command: &str) -> Result<GitOutput, WorktreeError> {
        let output = self.run(cwd, args)?;
        if output.success() {
            Ok(output)
        } else {
            Err(WorktreeError::GitFailed {
                command: command.to_owned(),
                stderr: output.stderr_tail(),
            })
        }
    }

    fn common_dir(&self) -> Result<PathBuf, WorktreeError> {
        let inside = self.run(&self.map.local_root, &["rev-parse", "--is-inside-work-tree"])?;
        if !inside.success() || inside.stdout.trim() != "true" {
            return Err(WorktreeError::NotAGitCheckout {
                path: self.map.local_root.clone(),
            });
        }
        let output = self.run(&self.map.local_root, &["rev-parse", "--git-common-dir"])?;
        if !output.success() {
            return Err(WorktreeError::NotAGitCheckout {
                path: self.map.local_root.clone(),
            });
        }
        let raw = output.stdout.trim();
        if raw.is_empty() {
            return Err(WorktreeError::NotAGitCheckout {
                path: self.map.local_root.clone(),
            });
        }
        let path = Path::new(raw);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.map.local_root.join(path)
        };
        canonical_dir(&absolute)
    }

    fn ensure_commit(&self, commit: &str, remote: &str) -> Result<String, WorktreeError> {
        let spec = format!("{commit}^{{commit}}");
        if self.has_commit(&spec)? {
            return Ok(commit.to_owned());
        }
        let mut detail = String::new();
        let output = self.run(&self.map.local_root, &["fetch", "--no-tags", remote, commit])?;
        if !output.success() {
            detail = output.stderr_tail();
        }
        if !self.has_commit(&spec)? {
            let fallback = self.run(&self.map.local_root, &["fetch", "--no-tags", remote])?;
            if !fallback.success() && detail.is_empty() {
                detail = fallback.stderr_tail();
            }
        }
        if self.has_commit(&spec)? {
            Ok(commit.to_owned())
        } else {
            Err(WorktreeError::BaseCommitMissing {
                commit: commit.to_owned(),
                remote: remote.to_owned(),
                detail,
            })
        }
    }

    fn has_commit(&self, spec: &str) -> Result<bool, WorktreeError> {
        let output = self.run(&self.map.local_root, &["cat-file", "-e", spec])?;
        Ok(output.success())
    }

    fn is_registered_worktree(&self, path: &Path, common_dir: &Path) -> Result<bool, WorktreeError> {
        is_registered_worktree(self.git.as_ref(), path, common_dir)
    }

    fn current_branch(&self, path: &Path) -> Result<Option<String>, WorktreeError> {
        let output = self.run(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        if !output.success() {
            return Ok(None);
        }
        let branch = output.stdout.trim();
        if branch.is_empty() {
            Ok(None)
        } else {
            Ok(Some(branch.to_owned()))
        }
    }

    fn head_at(&self, path: &Path) -> Result<String, WorktreeError> {
        head_commit(self.git.as_ref(), path)
    }

    fn is_clean_at(&self, path: &Path) -> Result<bool, WorktreeError> {
        is_clean(self.git.as_ref(), path)
    }

    fn worktree_marker(&self, path: &Path) -> Result<Option<String>, WorktreeError> {
        worktree_marker(self.git.as_ref(), path)
    }

    fn remove_owned_worktree(&self, path: &Path, common_dir: &Path) -> Result<(), WorktreeError> {
        if fs::symlink_metadata(path).is_ok() {
            if self.is_registered_worktree(path, common_dir)? {
                let path_string = path_string(path)?;
                self.run_checked(
                    &self.map.local_root,
                    &["worktree", "remove", "--force", &path_string],
                    "git worktree remove --force",
                )?;
            } else {
                fs::remove_dir_all(path)?;
            }
        }
        self.prune_worktrees()
    }

    fn prune_worktrees(&self) -> Result<(), WorktreeError> {
        let output = self.run(&self.map.local_root, &["worktree", "prune"])?;
        if output.success() {
            Ok(())
        } else {
            Err(WorktreeError::GitFailed {
                command: "git worktree prune".to_owned(),
                stderr: output.stderr_tail(),
            })
        }
    }

    fn branch_exists(&self, branch: &str) -> Result<bool, WorktreeError> {
        let reference = format!("refs/heads/{branch}");
        let output = self.run(&self.map.local_root, &["show-ref", "--verify", "--quiet", &reference])?;
        Ok(output.success())
    }
}

/// One job worktree, returned by [`WorktreeManager::create`].
pub struct Worktree {
    project: ProjectName,
    job: JobId,
    path: PathBuf,
    branch: String,
    remote: String,
    repo: PathBuf,
    common_dir: PathBuf,
    record_path: PathBuf,
    git: Arc<dyn GitRunner>,
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worktree")
            .field("project", &self.project)
            .field("job", &self.job)
            .field("path", &self.path)
            .field("branch", &self.branch)
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

impl Worktree {
    /// Returns the project the worktree belongs to.
    pub fn project(&self) -> &ProjectName {
        &self.project
    }

    /// Returns the job the worktree belongs to.
    pub fn job(&self) -> &JobId {
        &self.job
    }

    /// Returns the absolute worktree path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the `job/<id>` branch.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Returns the remote used for fetch and push.
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Returns the current worktree head commit.
    pub fn head_commit(&self) -> Result<String, WorktreeError> {
        head_commit(self.git.as_ref(), &self.path)
    }

    /// Returns whether the worktree has no modified, staged or untracked files.
    pub fn is_clean(&self) -> Result<bool, WorktreeError> {
        is_clean(self.git.as_ref(), &self.path)
    }

    /// Pushes only `job/<id>` and reports the head commit.
    ///
    /// Every push URL configured for the remote must match `expected_remote_url` after
    /// credentials are stripped. The check reads the URLs git will actually push to, so
    /// `remote.<name>.pushurl`, `url.<base>.pushInsteadOf` and `url.<base>.insteadOf` rewrites
    /// are all covered. A fork or a different push remote is
    /// [`WorktreeError::RemoteMismatch`] and no push is attempted; the error reports the
    /// actual push URL with credentials stripped. A git failure is
    /// [`WorktreeError::GitPushFailed`] with the stderr tail; the worktree stays in place for
    /// inspection.
    pub fn push_job(&self, expected_remote_url: &str) -> Result<PushResult, WorktreeError> {
        let actual_urls = self.push_urls()?;
        if actual_urls.is_empty() {
            return Err(WorktreeError::RemoteMismatch {
                expected: expected_remote_url.to_owned(),
                actual: format!("<missing or empty remote {}>", self.remote),
            });
        }
        let expected = comparable_remote(expected_remote_url);
        if let Some(actual) = actual_urls.iter().find(|actual| comparable_remote(actual) != expected) {
            return Err(WorktreeError::RemoteMismatch {
                expected: expected_remote_url.to_owned(),
                actual: strip_remote_credentials(actual),
            });
        }
        let head = self.head_commit()?;
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", self.branch);
        let output = self.git.run(
            &self.path,
            &["-c", "push.followTags=false", "push", &self.remote, &refspec],
        )?;
        if !output.success() {
            return Err(WorktreeError::GitPushFailed {
                command: format!("git push {} {refspec}", self.remote),
                stderr: output.stderr_tail(),
            });
        }
        Ok(PushResult {
            branch: self.branch.clone(),
            remote: self.remote.clone(),
            head_commit: head,
        })
    }

    /// Creates exactly one `base..job/<id>` bundle in `dest_dir`.
    ///
    /// The slice starts at `base` and carries only this worktree's committed job branch; a
    /// dirty worktree is allowed, but uncommitted files are not in the bundle.
    pub fn create_bundle(&self, base: &str, dest_dir: &Path) -> Result<JobBundle, WorktreeError> {
        create_job_bundle(self.git.as_ref(), &self.path, &self.job, base, dest_dir)
    }

    /// Removes this worktree and its `job/<id>` branch.
    ///
    /// The same ownership checks as [`WorktreeManager::cleanup`] apply; a worktree that no
    /// longer carries the matching marker is refused and kept.
    pub fn cleanup(self) -> Result<(), WorktreeError> {
        let path_exists = fs::symlink_metadata(&self.path).is_ok();
        let branch_exists = {
            let reference = format!("refs/heads/{}", self.branch);
            let output = self
                .git
                .run(&self.repo, &["show-ref", "--verify", "--quiet", &reference])?;
            output.success()
        };
        if !path_exists && !branch_exists {
            if let Some(record) = read_record(&self.record_path)? {
                if record.project != self.project.as_str() || record.job != self.job.as_str() {
                    return Err(WorktreeError::Refused {
                        detail: format!("ownership record for job {} belongs to another project", self.job),
                    });
                }
            }
            remove_if_exists(&self.record_path)?;
            return Ok(());
        }
        cleanup_owned(
            self.git.as_ref(),
            &CleanupTarget {
                repo: &self.repo,
                common_dir: &self.common_dir,
                path: &self.path,
                branch: &self.branch,
                project: &self.project,
                job: &self.job,
                record_path: &self.record_path,
                path_exists,
                branch_exists,
            },
        )
    }

    /// Returns every URL `git push <remote>` will use, in git's order.
    ///
    /// `--push` makes git apply `pushurl`, `pushInsteadOf` and `insteadOf`, so the result is
    /// the destination set of the later push rather than the fetch URL. A missing remote or an
    /// empty URL list is an empty vec.
    fn push_urls(&self) -> Result<Vec<String>, WorktreeError> {
        let output = self
            .git
            .run(&self.path, &["remote", "get-url", "--push", "--all", &self.remote])?;
        if !output.success() {
            return Ok(Vec::new());
        }
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

/// Strips credentials from a git remote URL so two spellings can be compared.
///
/// URL userinfo (`https://user:token@host/...`) and the scp-like `user@` prefix are removed;
/// the scheme, host, port and path are preserved. A local path is returned unchanged.
pub fn strip_remote_credentials(url: &str) -> String {
    let trimmed = url.trim();
    if let Some(scheme_end) = trimmed.find("://") {
        let (scheme, rest) = trimmed.split_at(scheme_end + 3);
        let slash = rest.find('/').unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(slash);
        let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        return format!("{scheme}{host}{tail}");
    }
    match trimmed.split_once('@') {
        Some((userinfo, host_path))
            if !userinfo.contains('/') && !userinfo.contains(':') && host_path.contains(':') =>
        {
            host_path.to_owned()
        }
        _ => trimmed.to_owned(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnershipRecord {
    project: String,
    job: String,
    branch: String,
    path: String,
    base_commit: String,
}

struct CleanupTarget<'a> {
    repo: &'a Path,
    common_dir: &'a Path,
    path: &'a Path,
    branch: &'a str,
    project: &'a ProjectName,
    job: &'a JobId,
    record_path: &'a Path,
    path_exists: bool,
    branch_exists: bool,
}

fn cleanup_owned(git: &dyn GitRunner, target: &CleanupTarget<'_>) -> Result<(), WorktreeError> {
    let Some(record) = read_record(target.record_path)? else {
        return Err(WorktreeError::Refused {
            detail: format!("worktree or branch for job {} was not created by VOWP", target.job),
        });
    };
    if record.project != target.project.as_str()
        || record.job != target.job.as_str()
        || record.branch != target.branch
        || record.path != path_string(target.path)?
    {
        return Err(WorktreeError::Refused {
            detail: format!("ownership record for job {} does not match", target.job),
        });
    }
    if target.path_exists {
        let metadata = fs::symlink_metadata(target.path)?;
        if metadata.file_type().is_symlink() {
            return Err(WorktreeError::Refused {
                detail: format!("worktree path {} is a symlink", target.path.display()),
            });
        }
        let marker = worktree_marker(git, target.path)?;
        if marker.as_deref() != Some(target.job.as_str()) {
            return Err(WorktreeError::Refused {
                detail: format!("worktree marker for job {} does not match", target.job),
            });
        }
        if is_registered_worktree(git, target.path, target.common_dir)? {
            let path_string = path_string(target.path)?;
            run_checked(
                git,
                target.repo,
                &["worktree", "remove", "--force", &path_string],
                "git worktree remove --force",
            )?;
        } else {
            fs::remove_dir_all(target.path)?;
        }
    }
    if target.branch_exists {
        run_checked(git, target.repo, &["branch", "-D", target.branch], "git branch -D")?;
    }
    remove_if_exists(target.record_path)?;
    Ok(())
}

fn head_commit(git: &dyn GitRunner, path: &Path) -> Result<String, WorktreeError> {
    let output = run_checked(git, path, &["rev-parse", "HEAD"], "git rev-parse HEAD")?;
    let head = output.stdout.trim();
    if head.is_empty() {
        return Err(WorktreeError::GitFailed {
            command: "git rev-parse HEAD".to_owned(),
            stderr: String::new(),
        });
    }
    Ok(head.to_owned())
}

fn is_clean(git: &dyn GitRunner, path: &Path) -> Result<bool, WorktreeError> {
    let output = run_checked(
        git,
        path,
        &["status", "--porcelain", "--untracked-files=normal"],
        "git status --porcelain",
    )?;
    Ok(output.stdout.trim().is_empty())
}

fn run_checked(git: &dyn GitRunner, cwd: &Path, args: &[&str], command: &str) -> Result<GitOutput, WorktreeError> {
    let output = git.run(cwd, args)?;
    if output.success() {
        Ok(output)
    } else {
        Err(WorktreeError::GitFailed {
            command: command.to_owned(),
            stderr: output.stderr_tail(),
        })
    }
}

fn absolute_git_dir(git: &dyn GitRunner, path: &Path) -> Result<PathBuf, WorktreeError> {
    let output = run_checked(
        git,
        path,
        &["rev-parse", "--absolute-git-dir"],
        "git rev-parse --absolute-git-dir",
    )?;
    let raw = output.stdout.trim();
    if raw.is_empty() {
        return Err(WorktreeError::GitFailed {
            command: "git rev-parse --absolute-git-dir".to_owned(),
            stderr: String::new(),
        });
    }
    Ok(PathBuf::from(raw))
}

fn worktree_marker(git: &dyn GitRunner, path: &Path) -> Result<Option<String>, WorktreeError> {
    let dir = match absolute_git_dir(git, path) {
        Ok(dir) => dir,
        Err(_) => return Ok(None),
    };
    match fs::read_to_string(dir.join(OWNERSHIP_MARKER)) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WorktreeError::Io(error)),
    }
}

fn is_registered_worktree(git: &dyn GitRunner, path: &Path, common_dir: &Path) -> Result<bool, WorktreeError> {
    let inside = git.run(path, &["rev-parse", "--is-inside-work-tree"])?;
    if !inside.success() || inside.stdout.trim() != "true" {
        return Ok(false);
    }
    let top = git.run(path, &["rev-parse", "--show-toplevel"])?;
    if !top.success() {
        return Ok(false);
    }
    if canonical_dir(Path::new(top.stdout.trim()))? != canonical_dir(path)? {
        return Ok(false);
    }
    let common = git.run(path, &["rev-parse", "--git-common-dir"])?;
    if !common.success() {
        return Ok(false);
    }
    let raw = common.stdout.trim();
    let raw_path = Path::new(raw);
    let absolute = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        path.join(raw_path)
    };
    Ok(canonical_dir(&absolute)? == common_dir)
}

fn write_owner_marker(git_dir: &Path, job: &JobId) -> Result<(), WorktreeError> {
    let marker = git_dir.join(OWNERSHIP_MARKER);
    write_private(&marker, format!("{}\n", job.as_str()).as_bytes())
}

fn ownership_record_path(common_dir: &Path, job: &JobId) -> PathBuf {
    common_dir.join(OWNERSHIP_DIR).join(format!("{}.json", job.as_str()))
}

fn read_record(path: &Path) -> Result<Option<OwnershipRecord>, WorktreeError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WorktreeError::Io(error)),
    }
}

fn write_record(path: &Path, record: &OwnershipRecord) -> Result<(), WorktreeError> {
    write_private(path, &serde_json::to_vec_pretty(record)?)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), WorktreeError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Returns the effective uid of this process.
///
/// Linux exposes the real, effective, saved and filesystem ids in `/proc/self/status`; the
/// second `Uid:` field is the effective uid. Other Unixes report the effective uid from
/// `id -u`.
fn effective_uid() -> Result<u32, WorktreeError> {
    #[cfg(target_os = "linux")]
    if let Ok(text) = fs::read_to_string("/proc/self/status") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(field) = rest.split_whitespace().nth(1)
            {
                return field.parse::<u32>().map_err(|_| WorktreeError::Refused {
                    detail: "the process uid is not an integer".to_owned(),
                });
            }
        }
    }
    let output = Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(WorktreeError::Refused {
            detail: "could not determine the effective uid with `id -u`".to_owned(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|_| WorktreeError::Refused {
            detail: "`id -u` did not return an integer".to_owned(),
        })
}

/// Checks that `path` is a real directory owned by `uid` with no group or world write bit.
///
/// Group and world read/execute are allowed so a read-only shared mirror can be
/// group-readable. `symlink_metadata` is used so a symbolic link is refused instead of
/// followed; callers pass an already canonical path.
fn check_private_dir(path: &Path, uid: u32) -> Result<(), WorktreeError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(WorktreeError::UnsafeCheckout {
            path: path.to_path_buf(),
            detail: "it is a symbolic link".to_owned(),
        });
    }
    if !metadata.is_dir() {
        return Err(WorktreeError::NotADirectory {
            path: path.to_path_buf(),
        });
    }
    if metadata.uid() != uid {
        return Err(WorktreeError::UnsafeCheckout {
            path: path.to_path_buf(),
            detail: format!("owned by uid {}, expected uid {uid}", metadata.uid()),
        });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(WorktreeError::UnsafeCheckout {
            path: path.to_path_buf(),
            detail: format!("mode {mode:03o} grants group or world write"),
        });
    }
    Ok(())
}

fn validate_root(path: &Path, label: &str) -> Result<PathBuf, WorktreeError> {
    if !path.is_absolute() {
        return Err(WorktreeError::InvalidPathMapEntry {
            entry: path.display().to_string(),
            detail: format!("{label} must be absolute"),
        });
    }
    if path.to_str().is_none() {
        return Err(WorktreeError::InvalidPathMapEntry {
            entry: path.display().to_string(),
            detail: format!("{label} must be UTF-8"),
        });
    }
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(WorktreeError::PathMissing {
                path: path.to_path_buf(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(WorktreeError::PathNotWritable {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(WorktreeError::Io(error)),
    };
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_dir() {
        return Err(WorktreeError::NotADirectory { path: canonical });
    }
    if metadata.permissions().mode() & 0o222 == 0 {
        return Err(WorktreeError::PathNotWritable { path: canonical });
    }
    Ok(canonical)
}

fn validate_allow(entries: &[String], root: &Path) -> Result<Vec<Vec<String>>, WorktreeError> {
    let mut allow = Vec::new();
    for entry in entries {
        let components = rel_components(entry).map_err(|error| WorktreeError::InvalidPathMapEntry {
            entry: entry.clone(),
            detail: error.to_string(),
        })?;
        let candidate = root.join(components.join("/"));
        let normalised = match fs::canonicalize(&candidate) {
            Ok(canonical) => match canonical.strip_prefix(root) {
                Ok(rel) => path_components(rel)?,
                Err(_) => {
                    return Err(WorktreeError::InvalidPathMapEntry {
                        entry: entry.clone(),
                        detail: "resolves outside local_root".to_owned(),
                    });
                }
            },
            Err(_) => components,
        };
        allow.push(normalised);
    }
    Ok(allow)
}

fn rel_components(rel: &str) -> Result<Vec<String>, WorktreeError> {
    let invalid = |detail: &str| WorktreeError::InvalidPathRef {
        detail: format!("{detail}: {rel:?}"),
    };
    if rel.is_empty() {
        return Err(invalid("empty relative path"));
    }
    if rel.len() > MAX_PATH_REF_LEN {
        return Err(invalid("path over the length cap"));
    }
    if rel.starts_with('/') {
        return Err(invalid("absolute path"));
    }
    if rel.contains('\0') {
        return Err(invalid("NUL byte"));
    }
    if rel.contains('\\') {
        return Err(invalid("backslash"));
    }
    let mut components = Vec::new();
    for part in rel.split('/') {
        if part.is_empty() {
            return Err(invalid("empty component"));
        }
        if part == "." || part == ".." {
            return Err(invalid("traversal or non-normalised component"));
        }
        if part.chars().any(char::is_control) {
            return Err(invalid("control character"));
        }
        components.push(part.to_owned());
    }
    Ok(components)
}

fn path_components(path: &Path) -> Result<Vec<String>, WorktreeError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    return Err(WorktreeError::InvalidPathRef {
                        detail: "non-UTF-8 path component".to_owned(),
                    });
                };
                components.push(value.to_owned());
            }
            _ => {
                return Err(WorktreeError::InvalidPathRef {
                    detail: format!("non-normalised path component in {}", path.display()),
                });
            }
        }
    }
    Ok(components)
}

fn has_symlink(root: &Path, components: &[String]) -> Option<PathBuf> {
    let mut current = root.to_path_buf();
    for component in components {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Some(current),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

fn is_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Returns true when two different strings match only after case folding.
fn is_case_only(left: &str, right: &str) -> bool {
    left != right && left.to_lowercase() == right.to_lowercase()
}

fn canonical_dir(path: &Path) -> Result<PathBuf, WorktreeError> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(WorktreeError::PathMissing {
            path: path.to_path_buf(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(WorktreeError::PathNotWritable {
            path: path.to_path_buf(),
        }),
        Err(error) => Err(WorktreeError::Io(error)),
    }
}

fn path_string(path: &Path) -> Result<String, WorktreeError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| WorktreeError::InvalidPathRef {
            detail: format!("non-UTF-8 path {}", path.display()),
        })
}

fn validate_commit(commit: &str) -> Result<(), WorktreeError> {
    let valid = matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(WorktreeError::InvalidCommit {
            commit: commit.to_owned(),
        })
    }
}

fn validate_remote(remote: &str) -> Result<(), WorktreeError> {
    let valid = !remote.is_empty()
        && remote.len() <= 64
        && !remote.starts_with('-')
        && remote != "."
        && remote != ".."
        && !remote.contains("..")
        && remote
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(WorktreeError::InvalidRemote {
            remote: remote.to_owned(),
        })
    }
}

fn job_branch(job: &JobId) -> String {
    format!("{JOB_BRANCH_PREFIX}{}", job.as_str())
}

fn comparable_remote(url: &str) -> String {
    strip_remote_credentials(url).trim_end_matches('/').to_owned()
}

fn disk_free_bytes(path: &Path) -> u64 {
    let Ok(output) = Command::new("df").arg("-Pk").arg(path).output() else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().nth(1) else {
        return 0;
    };
    line.split_whitespace()
        .nth(3)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .unwrap_or(0)
}

fn stderr_tail(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.len() <= STDERR_TAIL_BYTES {
        return trimmed.to_owned();
    }
    let mut start = trimmed.len() - STDERR_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_owned()
}

fn remove_if_exists(path: &Path) -> Result<(), WorktreeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(WorktreeError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_remote_credentials_removes_url_and_scp_userinfo() {
        let credential_url = format!("https://owner:{}@example.com/owner/repo.git", "A".repeat(16));
        assert_eq!(
            strip_remote_credentials(&credential_url),
            "https://example.com/owner/repo.git"
        );
        assert_eq!(
            strip_remote_credentials("ssh://git@example.com/owner/repo.git"),
            "ssh://example.com/owner/repo.git"
        );
        assert_eq!(
            strip_remote_credentials("git@example.com:owner/repo.git"),
            "example.com:owner/repo.git"
        );
        assert_eq!(strip_remote_credentials("/tmp/vowp/origin.git"), "/tmp/vowp/origin.git");
        assert_eq!(
            strip_remote_credentials("file:///tmp/vowp/origin.git"),
            "file:///tmp/vowp/origin.git"
        );
    }

    #[test]
    fn stderr_tail_keeps_the_end_and_respects_utf8() {
        let short = "one\ntwo\n";
        assert_eq!(stderr_tail(short), "one\ntwo");
        let long = format!("{}\nlast line", "x".repeat(STDERR_TAIL_BYTES + 32));
        let tail = stderr_tail(&long);
        assert!(tail.len() <= STDERR_TAIL_BYTES);
        assert!(tail.ends_with("last line"));
        let unicode = format!("{}{}", "é".repeat(STDERR_TAIL_BYTES), "tail");
        assert!(stderr_tail(&unicode).is_char_boundary(0));
    }
}
