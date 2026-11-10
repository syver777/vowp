//! Everything the loops need from the outside world, injected so tests can use fakes.
//! Spec: `docs/design/crates/vowp-core.md` §2.
//!
//! [`Deps`] is cheap to clone: every dependency is an `Arc`. [`SystemRunner`] runs shell or
//! argv commands in their own process group and kills the whole group on timeout.
//! [`SystemGit`] runs the real `git` binary against a temporary repository; [`AdapterRegistry`]
//! maps an [`Executor`] to the adapter that can run it.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vowp_adapters::claude_code::{ClaudeCodeAdapter, ClaudeConfig};
use vowp_adapters::cli::CliAdapter;
use vowp_adapters::opencode::{OpencodeAdapter, OpencodeConfig};
use vowp_adapters::proc::{Signal, signal_group};
use vowp_protocol::{Adapter, Clock, Executor, HarnessKind, Log, NodeName, Timestamp, VowpConfig};
use vowp_store::{NodePaths, Store};

use crate::CoreError;

/// Bytes of merged command output kept by [`SystemRunner`]; older bytes are dropped first.
const OUTPUT_CAP_BYTES: usize = 4 * 1024 * 1024;

/// How long one `git` command may run before its process group is killed.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything a loop or service needs from the outside world.
///
/// Fields are injected so tests use a temporary node directory, a fake clock, a memory log,
/// fake adapters and a fake window manager instead of the real machine.
#[derive(Clone)]
pub struct Deps {
    /// On-disk node layout.
    pub paths: NodePaths,
    /// Parsed `vowp.toml`.
    pub config: Arc<VowpConfig>,
    /// Name of this node.
    pub node: NodeName,
    /// Injectable clock; loops never call [`Timestamp::now`] themselves.
    pub clock: Arc<dyn Clock>,
    /// Supervisor log sink.
    pub log: Arc<dyn Log>,
    /// Adapter lookup keyed by harness.
    pub adapters: Arc<AdapterRegistry>,
    /// tmux window manager.
    pub windows: Arc<dyn vowp_tmux::WindowManager>,
    /// Git operations.
    pub git: Arc<dyn GitOps>,
    /// Command runner for gates, schedules and probes.
    pub cmd: Arc<dyn CommandRunner>,
    /// Jitter source for retry backoff.
    pub rng: Arc<dyn Rng>,
}

impl Deps {
    /// Opens a fresh store connection for the current thread.
    ///
    /// Each thread owns its connection: SQLite connections are not shared across threads.
    /// The store stamps rows with this node's injected clock.
    pub fn open_store(&self) -> Result<Store, CoreError> {
        Ok(Store::open(&self.paths)?.with_clock(self.clock.clone()))
    }
}

/// A source of bounded random jitter, injected so tests are deterministic.
pub trait Rng: Send + Sync {
    /// Returns a random number in `0..=max` (`0` when `max` is `0`).
    fn jitter_secs(&self, max: u64) -> u64;
}

/// A time-seeded xorshift generator.
#[derive(Debug)]
pub struct SystemRng {
    state: Mutex<u64>,
}

impl SystemRng {
    /// Creates a generator seeded from the wall clock and the process id.
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        let seed = nanos ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491_4F6C_DD1D;
        Self {
            state: Mutex::new(if seed == 0 { 1 } else { seed }),
        }
    }
}

impl Default for SystemRng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng for SystemRng {
    fn jitter_secs(&self, max: u64) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut value = *state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        *state = value;
        value % max.saturating_add(1)
    }
}

/// A jitter source that always returns the same value, clamped to the requested maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedRng(pub u64);

impl Rng for FixedRng {
    fn jitter_secs(&self, max: u64) -> u64 {
        self.0.min(max)
    }
}

/// How to run one command.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Shell program run with `sh -c`; takes precedence over [`CommandSpec::argv`].
    pub shell: Option<String>,
    /// Program and arguments used when [`CommandSpec::shell`] is `None`.
    pub argv: Vec<String>,
    /// Working directory; `None` inherits the supervisor's directory.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables on top of the inherited environment.
    pub env: Vec<(String, String)>,
    /// Wall-clock limit; the whole process group is killed when it expires.
    pub timeout: Duration,
}

/// One finished command.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// Exit code, or `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
    /// True when the timeout expired and the process group was killed.
    pub timed_out: bool,
    /// Merged stdout and stderr, capped at 4 MiB (oldest bytes dropped first).
    pub output: Vec<u8>,
    /// Wall-clock duration of the command.
    pub duration: Duration,
}

impl CommandOutput {
    /// Returns the last `n` lines of output, joined by newlines without a trailing newline.
    pub fn tail_lines(&self, n: usize) -> String {
        if n == 0 {
            return String::new();
        }
        let text = self.text();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    /// Returns the merged output decoded as UTF-8, replacing invalid sequences.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

/// Runs commands for gates, schedules and probes.
pub trait CommandRunner: Send + Sync {
    /// Runs `spec` to completion or until its timeout expires.
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CoreError>;
}

/// The real command runner: `std::process::Command`, one process group per command.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CoreError> {
        let start = Instant::now();
        let program_label = match (&spec.shell, spec.argv.first()) {
            (Some(shell), _) => format!("sh -c {shell}"),
            (None, Some(program)) => program.clone(),
            (None, None) => return Err(CoreError::Command("empty command spec".to_owned())),
        };

        let mut command = match &spec.shell {
            Some(shell) => {
                let mut command = Command::new("sh");
                command.arg("-c").arg(shell);
                command
            }
            None => {
                let mut components = spec.argv.iter();
                let Some(program) = components.next() else {
                    return Err(CoreError::Command("empty command spec".to_owned()));
                };
                let mut command = Command::new(program);
                command.args(components);
                command
            }
        };
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command
            .envs(spec.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = command.spawn().map_err(|source| CoreError::Io {
            path: program_label.clone(),
            source,
        })?;
        let pid = child.id();
        let shared = Arc::new(Mutex::new(Vec::new()));
        let Some(stdout) = child.stdout.take() else {
            let _ = signal_group(pid, Signal::Kill);
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreError::Command("child stdout was not piped".to_owned()));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = signal_group(pid, Signal::Kill);
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreError::Command("child stderr was not piped".to_owned()));
        };
        let stdout_reader = spawn_reader(stdout, Arc::clone(&shared));
        let stderr_reader = spawn_reader(stderr, Arc::clone(&shared));

        let mut timed_out = false;
        let exit_code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) => {
                    if start.elapsed() >= spec.timeout {
                        timed_out = true;
                        let _ = signal_group(pid, Signal::Kill);
                        let _ = child.kill();
                        let status = child.wait().map_err(|source| CoreError::Io {
                            path: program_label.clone(),
                            source,
                        })?;
                        break status.code();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(source) => {
                    let _ = signal_group(pid, Signal::Kill);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CoreError::Io {
                        path: program_label,
                        source,
                    });
                }
            }
        };

        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        let output = match shared.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Ok(CommandOutput {
            exit_code,
            timed_out,
            output,
            duration: start.elapsed(),
        })
    }
}

/// Reads `reader` in chunks and appends them to the shared, capped output buffer.
fn spawn_reader(mut reader: impl Read + Send + 'static, shared: Arc<Mutex<Vec<u8>>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let mut buffer = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.len() > OUTPUT_CAP_BYTES {
                        let overflow = buffer.len() - OUTPUT_CAP_BYTES;
                        buffer.drain(..overflow);
                    }
                }
                Err(_) => break,
            }
        }
    })
}

/// The outcome of a rebase attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseResult {
    /// The branch rebased without conflicts.
    Clean,
    /// The rebase stopped on conflicting files.
    Conflict {
        /// Conflicted paths, relative to the worktree.
        files: Vec<String>,
    },
}

/// Git operations used by sessions, gates and the merge queue.
pub trait GitOps: Send + Sync {
    /// Adds a worktree at `path` on `branch` based on `base`; an existing worktree/branch is reused.
    fn worktree_add(&self, repo: &Path, path: &Path, branch: &str, base: &str) -> Result<(), CoreError>;
    /// Removes the worktree at `path`; absent worktrees are a no-op.
    fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<(), CoreError>;
    /// Deletes a local branch; absent branches are a no-op.
    fn branch_delete(&self, repo: &Path, branch: &str) -> Result<(), CoreError>;
    /// Fetches one remote.
    fn fetch(&self, repo: &Path, remote: &str) -> Result<(), CoreError>;
    /// Lists files changed by `git diff --name-only base...head`.
    fn diff_names(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError>;
    /// Returns the full `git diff base...head` text.
    fn diff_text(&self, repo: &Path, base: &str, head: &str) -> Result<String, CoreError>;
    /// Returns the short shas of commits in `base..head`.
    fn commits_between(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError>;
    /// Resolves `rev` to a full sha.
    fn head_sha(&self, repo: &Path, rev: &str) -> Result<String, CoreError>;
    /// Returns the raw `git ls-remote <remote> refs/heads/<branch>` output.
    fn ls_remote(&self, repo: &Path, remote: &str, branch: &str) -> Result<String, CoreError>;
    /// Returns true when `git status --porcelain` prints nothing.
    fn status_clean(&self, repo: &Path) -> Result<bool, CoreError>;
    /// Rebases the worktree onto `onto`.
    fn rebase(&self, worktree: &Path, onto: &str) -> Result<RebaseResult, CoreError>;
    /// Aborts an in-progress rebase.
    fn rebase_abort(&self, worktree: &Path) -> Result<(), CoreError>;
    /// Continues an in-progress rebase with `GIT_EDITOR=true`.
    fn rebase_continue(&self, worktree: &Path) -> Result<RebaseResult, CoreError>;
    /// Stages `paths` (`git add -- <paths>`); an empty list is a no-op.
    fn add(&self, worktree: &Path, paths: &[String]) -> Result<(), CoreError>;
    /// Restores one path from `rev` and stages it (`git checkout <rev> -- <path>`).
    fn checkout_path(&self, worktree: &Path, rev: &str, path: &str) -> Result<(), CoreError>;
    /// Commits the staged tree and returns the new short sha.
    fn commit(&self, worktree: &Path, message: &str) -> Result<String, CoreError>;
    /// Resets the tree and index to `rev` (`git reset --hard <rev>`).
    fn reset_hard(&self, repo: &Path, rev: &str) -> Result<(), CoreError>;
    /// Lists unmerged paths (`git diff --name-only --diff-filter=U`).
    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>, CoreError>;
    /// Fast-forwards `repo`'s current branch to `branch`.
    fn merge_ff_only(&self, repo: &Path, branch: &str) -> Result<(), CoreError>;
    /// Pushes `branch` to `remote`.
    fn push(&self, repo: &Path, remote: &str, branch: &str) -> Result<(), CoreError>;
    /// Returns the committer time of `rev`, or `None` when the revision has no commit.
    fn last_commit_time(&self, repo: &Path, rev: &str) -> Result<Option<Timestamp>, CoreError>;
}

/// The real git implementation, driven through a [`CommandRunner`].
#[derive(Clone)]
pub struct SystemGit {
    cmd: Arc<dyn CommandRunner>,
}

impl SystemGit {
    /// Creates a git implementation that runs commands through `cmd`.
    pub fn new(cmd: Arc<dyn CommandRunner>) -> Self {
        Self { cmd }
    }

    /// Runs `git` with `args` in `cwd` and adds `env` to the inherited environment.
    fn raw(&self, cwd: &Path, args: &[String], env: &[(&str, &str)]) -> Result<CommandOutput, CoreError> {
        let mut spec_env = vec![
            ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
            ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ];
        spec_env.extend(
            env.iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("git".to_owned());
        argv.extend(args.iter().cloned());
        self.cmd.run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: spec_env,
            timeout: GIT_TIMEOUT,
        })
    }

    /// Runs `git` and turns a non-zero exit into [`CoreError::Git`].
    fn ok(&self, cwd: &Path, args: &[String], env: &[(&str, &str)]) -> Result<CommandOutput, CoreError> {
        let output = self.raw(cwd, args, env)?;
        if output.exit_code == Some(0) {
            Ok(output)
        } else {
            Err(git_error(args, &output))
        }
    }

    /// Convenience wrapper for static argument lists.
    fn text(&self, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, CoreError> {
        let owned = owned_args(args);
        self.ok(cwd, &owned, env).map(|output| output.text())
    }

    /// Returns true when the worktree at `path` is already registered in `repo`.
    fn worktree_registered(&self, repo: &Path, path: &Path) -> Result<bool, CoreError> {
        let list = self.text(repo, &["worktree", "list", "--porcelain"], &[])?;
        Ok(list.lines().any(|line| match line.strip_prefix("worktree ") {
            Some(candidate) => same_path(candidate, path),
            None => false,
        }))
    }

    /// Returns true when the revision resolves to a commit.
    fn rev_exists(&self, repo: &Path, rev: &str) -> Result<bool, CoreError> {
        let owned = owned_args(&["rev-parse", "--verify", "--quiet", rev]);
        let output = self.raw(repo, &owned, &[])?;
        Ok(output.exit_code == Some(0))
    }

    /// Classifies a failed rebase: conflicts when unmerged files remain, otherwise a git error.
    fn rebase_outcome(
        &self,
        worktree: &Path,
        args: &[String],
        output: &CommandOutput,
    ) -> Result<RebaseResult, CoreError> {
        let files = self.conflicted_files(worktree)?;
        if files.is_empty() {
            Err(git_error(args, output))
        } else {
            Ok(RebaseResult::Conflict { files })
        }
    }
}

impl Default for SystemGit {
    fn default() -> Self {
        Self::new(Arc::new(SystemRunner))
    }
}

impl GitOps for SystemGit {
    fn worktree_add(&self, repo: &Path, path: &Path, branch: &str, base: &str) -> Result<(), CoreError> {
        if self.worktree_registered(repo, path)? {
            return Ok(());
        }
        let path_text = path.to_string_lossy().into_owned();
        let branch_ref = format!("refs/heads/{branch}");
        let mut args = vec!["worktree".to_owned(), "add".to_owned()];
        if self.rev_exists(repo, &branch_ref)? {
            args.push(path_text);
            args.push(branch.to_owned());
        } else {
            args.push("-b".to_owned());
            args.push(branch.to_owned());
            args.push(path_text);
            args.push(base.to_owned());
        }
        self.ok(repo, &args, &[]).map(|_| ())
    }

    fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<(), CoreError> {
        if !self.worktree_registered(repo, path)? {
            return Ok(());
        }
        let args = vec![
            "worktree".to_owned(),
            "remove".to_owned(),
            "--force".to_owned(),
            path.to_string_lossy().into_owned(),
        ];
        self.ok(repo, &args, &[]).map(|_| ())
    }

    fn branch_delete(&self, repo: &Path, branch: &str) -> Result<(), CoreError> {
        let reference = format!("refs/heads/{branch}");
        if !self.rev_exists(repo, &reference)? {
            return Ok(());
        }
        let args = vec!["branch".to_owned(), "-D".to_owned(), branch.to_owned()];
        self.ok(repo, &args, &[]).map(|_| ())
    }

    fn fetch(&self, repo: &Path, remote: &str) -> Result<(), CoreError> {
        self.ok(repo, &owned_args(&["fetch", remote]), &[]).map(|_| ())
    }

    fn diff_names(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError> {
        let range = format!("{base}...{head}");
        let output = self.text(repo, &["diff", "--name-only", &range], &[])?;
        Ok(non_empty_lines(&output))
    }

    fn diff_text(&self, repo: &Path, base: &str, head: &str) -> Result<String, CoreError> {
        let range = format!("{base}...{head}");
        self.text(repo, &["diff", &range], &[])
    }

    fn commits_between(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError> {
        let range = format!("{base}..{head}");
        let output = self.text(repo, &["log", "--format=%h", &range], &[])?;
        Ok(non_empty_lines(&output))
    }

    fn head_sha(&self, repo: &Path, rev: &str) -> Result<String, CoreError> {
        let output = self.text(repo, &["rev-parse", rev], &[])?;
        Ok(output.trim().to_owned())
    }

    fn ls_remote(&self, repo: &Path, remote: &str, branch: &str) -> Result<String, CoreError> {
        let reference = format!("refs/heads/{branch}");
        self.text(repo, &["ls-remote", remote, &reference], &[])
    }

    fn status_clean(&self, repo: &Path) -> Result<bool, CoreError> {
        let output = self.text(repo, &["status", "--porcelain"], &[])?;
        Ok(output.trim().is_empty())
    }

    fn rebase(&self, worktree: &Path, onto: &str) -> Result<RebaseResult, CoreError> {
        let args = owned_args(&["rebase", onto]);
        let output = self.raw(worktree, &args, &[])?;
        if output.exit_code == Some(0) {
            Ok(RebaseResult::Clean)
        } else {
            self.rebase_outcome(worktree, &args, &output)
        }
    }

    fn rebase_abort(&self, worktree: &Path) -> Result<(), CoreError> {
        self.ok(worktree, &owned_args(&["rebase", "--abort"]), &[]).map(|_| ())
    }

    fn rebase_continue(&self, worktree: &Path) -> Result<RebaseResult, CoreError> {
        let args = owned_args(&["rebase", "--continue"]);
        let output = self.raw(worktree, &args, &[("GIT_EDITOR", "true")])?;
        if output.exit_code == Some(0) {
            Ok(RebaseResult::Clean)
        } else {
            self.rebase_outcome(worktree, &args, &output)
        }
    }

    fn add(&self, worktree: &Path, paths: &[String]) -> Result<(), CoreError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add".to_owned(), "--".to_owned()];
        args.extend(paths.iter().cloned());
        self.ok(worktree, &args, &[]).map(|_| ())
    }

    fn checkout_path(&self, worktree: &Path, rev: &str, path: &str) -> Result<(), CoreError> {
        let args = owned_args(&["checkout", rev, "--", path]);
        self.ok(worktree, &args, &[]).map(|_| ())
    }

    fn commit(&self, worktree: &Path, message: &str) -> Result<String, CoreError> {
        self.ok(worktree, &owned_args(&["commit", "-m", message]), &[])?;
        let output = self.text(worktree, &["rev-parse", "--short", "HEAD"], &[])?;
        Ok(output.trim().to_owned())
    }

    fn reset_hard(&self, repo: &Path, rev: &str) -> Result<(), CoreError> {
        self.ok(repo, &owned_args(&["reset", "--hard", rev]), &[]).map(|_| ())
    }

    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>, CoreError> {
        let output = self.text(worktree, &["diff", "--name-only", "--diff-filter=U"], &[])?;
        Ok(non_empty_lines(&output))
    }

    fn merge_ff_only(&self, repo: &Path, branch: &str) -> Result<(), CoreError> {
        self.ok(repo, &owned_args(&["merge", "--ff-only", branch]), &[])
            .map(|_| ())
    }

    fn push(&self, repo: &Path, remote: &str, branch: &str) -> Result<(), CoreError> {
        self.ok(repo, &owned_args(&["push", remote, branch]), &[]).map(|_| ())
    }

    fn last_commit_time(&self, repo: &Path, rev: &str) -> Result<Option<Timestamp>, CoreError> {
        let args = owned_args(&["log", "-1", "--format=%ct", rev]);
        let output = self.raw(repo, &args, &[])?;
        if output.exit_code != Some(0) {
            let stderr = output.text();
            if stderr.contains("does not have any commits")
                || stderr.contains("unknown revision")
                || stderr.contains("bad revision")
            {
                return Ok(None);
            }
            return Err(git_error(&args, &output));
        }
        let text = output.text();
        let line = text.trim().lines().next().unwrap_or_default();
        if line.is_empty() {
            return Ok(None);
        }
        let seconds = line
            .parse::<i64>()
            .map_err(|_| CoreError::Command(format!("git log returned non-numeric time {line:?}")))?;
        Ok(Some(Timestamp::from_unix_secs(seconds)))
    }
}

/// Maps an [`Executor`] to the adapter that can run it.
///
/// Keys are `claude-code`, `opencode` and `cli:<name>`; tests inject fakes with
/// [`AdapterRegistry::with`].
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the registry from `[harnesses.*]` and `[node].tz_offset_minutes`.
    ///
    /// `claude-code` and `opencode` always exist, with defaults when their table is absent.
    /// Every other harness entry must name a `mapping` file and becomes `cli:<name>`.
    pub fn from_config(config: &VowpConfig) -> Result<Self, CoreError> {
        let tz = config.node.tz_offset_minutes;
        let claude = config.harnesses.get("claude-code").cloned().unwrap_or_default();
        let opencode = config.harnesses.get("opencode").cloned().unwrap_or_default();

        let mut registry = Self::new()
            .with(
                "claude-code",
                Arc::new(ClaudeCodeAdapter::new(ClaudeConfig {
                    binary: claude.binary,
                    extra_args: claude.extra_args,
                    permission_mode: claude.permission_mode,
                    tz_offset_minutes: tz,
                    ..ClaudeConfig::default()
                })),
            )
            .with(
                "opencode",
                Arc::new(OpencodeAdapter::new(OpencodeConfig {
                    binary: opencode.binary,
                    extra_args: opencode.extra_args,
                    tz_offset_minutes: tz,
                    ..OpencodeConfig::default()
                })),
            );

        for (name, harness) in &config.harnesses {
            if matches!(name.as_str(), "claude-code" | "opencode" | "vowp") {
                continue;
            }
            let Some(mapping) = &harness.mapping else {
                return Err(CoreError::Invalid(format!(
                    "harness {name:?} has no mapping file; only claude-code and opencode are built in"
                )));
            };
            let path = mapping_path(config, mapping);
            let adapter = CliAdapter::from_mapping_file(&path)?;
            registry = registry.with(&format!("cli:{name}"), Arc::new(adapter));
        }
        Ok(registry)
    }

    /// Inserts or replaces the adapter for `key`.
    pub fn with(mut self, key: &str, adapter: Arc<dyn Adapter>) -> Self {
        self.adapters.insert(key.to_owned(), adapter);
        self
    }

    /// Returns the adapter for `exec`, or `None` when that harness has no adapter.
    pub fn get(&self, exec: &Executor) -> Option<Arc<dyn Adapter>> {
        self.adapters.get(&registry_key(exec)).cloned()
    }
}

/// Returns the registry key for one executor.
fn registry_key(exec: &Executor) -> String {
    match exec.harness {
        HarnessKind::ClaudeCode => "claude-code".to_owned(),
        HarnessKind::Opencode => "opencode".to_owned(),
        HarnessKind::Cli => format!("cli:{}", exec.model),
        HarnessKind::Vowp => "vowp".to_owned(),
    }
}

/// Resolves a mapping path relative to the project path when it is not absolute.
fn mapping_path(config: &VowpConfig, mapping: &str) -> PathBuf {
    let path = PathBuf::from(mapping);
    if path.is_absolute() {
        return path;
    }
    match &config.project.path {
        Some(project_path) => Path::new(project_path).join(path),
        None => path,
    }
}

/// Converts a static argument list to owned strings.
fn owned_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Splits command output into non-empty trimmed lines.
fn non_empty_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds the typed git error for a failed invocation.
fn git_error(args: &[String], output: &CommandOutput) -> CoreError {
    CoreError::Git {
        args: args.to_vec(),
        code: output.exit_code,
        stderr: output.tail_lines(40),
    }
}

/// Compares two paths, resolving both when they exist.
fn same_path(candidate: &str, path: &Path) -> bool {
    let candidate_path = Path::new(candidate);
    if candidate_path == path {
        return true;
    }
    match (std::fs::canonicalize(candidate_path), std::fs::canonicalize(path)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
