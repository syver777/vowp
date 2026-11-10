//! Shared fake git implementation for `vowp-core` integration tests.
//!
//! Every call is recorded and returns a configurable default, so a loop test can assert the
//! exact git sequence without touching a real repo.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use vowp_core::{CoreError, GitOps, RebaseResult};
use vowp_protocol::Timestamp;

/// A fake [`GitOps`] with recorded calls and configurable results.
#[derive(Debug, Clone)]
pub struct FakeGit {
    inner: std::sync::Arc<Mutex<State>>,
}

#[derive(Debug)]
struct State {
    calls: Vec<String>,
    rebase_results: VecDeque<RebaseResult>,
    conflicted_files: Vec<String>,
    status_clean: bool,
    diff_names: Vec<String>,
    diff_text: String,
    commits: Vec<String>,
    head_sha: String,
    ls_remote: String,
    last_commit_time: Option<Timestamp>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            rebase_results: VecDeque::new(),
            conflicted_files: Vec::new(),
            status_clean: true,
            diff_names: Vec::new(),
            diff_text: String::new(),
            commits: Vec::new(),
            head_sha: "0000000000000000000000000000000000000000".to_owned(),
            ls_remote: String::new(),
            last_commit_time: None,
        }
    }
}

impl FakeGit {
    /// Creates a fake whose operations succeed and whose worktree is clean.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(State::default())),
        }
    }

    /// Returns the recorded calls, oldest first, formatted as `name argument…`.
    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }

    /// Sets whether `status_clean` returns true.
    pub fn with_status_clean(self, clean: bool) -> Self {
        self.state().status_clean = clean;
        self
    }

    /// Sets the value returned by `diff_names`.
    pub fn with_diff_names(self, names: Vec<String>) -> Self {
        self.state().diff_names = names;
        self
    }

    /// Sets the value returned by `diff_text`.
    pub fn with_diff_text(self, text: impl Into<String>) -> Self {
        self.state().diff_text = text.into();
        self
    }

    /// Sets the value returned by `commits_between`.
    pub fn with_commits(self, commits: Vec<String>) -> Self {
        self.state().commits = commits;
        self
    }

    /// Sets the value returned by `head_sha`.
    pub fn with_head_sha(self, sha: impl Into<String>) -> Self {
        self.state().head_sha = sha.into();
        self
    }

    /// Sets the raw value returned by `ls_remote`.
    pub fn with_ls_remote(self, output: impl Into<String>) -> Self {
        self.state().ls_remote = output.into();
        self
    }

    /// Sets the files returned by `conflicted_files`.
    pub fn with_conflicted_files(self, files: Vec<String>) -> Self {
        self.state().conflicted_files = files;
        self
    }

    /// Queues the results of successive `rebase` and `rebase_continue` calls.
    pub fn with_rebase_results(self, results: Vec<RebaseResult>) -> Self {
        self.state().rebase_results = results.into();
        self
    }

    /// Sets the value returned by `last_commit_time`.
    pub fn with_last_commit_time(self, at: Option<Timestamp>) -> Self {
        self.state().last_commit_time = at;
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, call: String) {
        self.state().calls.push(call);
    }

    fn next_rebase(&self) -> RebaseResult {
        self.state().rebase_results.pop_front().unwrap_or(RebaseResult::Clean)
    }
}

impl Default for FakeGit {
    fn default() -> Self {
        Self::new()
    }
}

impl GitOps for FakeGit {
    fn worktree_add(&self, repo: &Path, path: &Path, branch: &str, base: &str) -> Result<(), CoreError> {
        self.record(format!(
            "worktree_add {} {} {branch} {base}",
            repo.display(),
            path.display()
        ));
        Ok(())
    }

    fn worktree_remove(&self, repo: &Path, path: &Path) -> Result<(), CoreError> {
        self.record(format!("worktree_remove {} {}", repo.display(), path.display()));
        Ok(())
    }

    fn branch_delete(&self, repo: &Path, branch: &str) -> Result<(), CoreError> {
        self.record(format!("branch_delete {} {branch}", repo.display()));
        Ok(())
    }

    fn fetch(&self, repo: &Path, remote: &str) -> Result<(), CoreError> {
        self.record(format!("fetch {} {remote}", repo.display()));
        Ok(())
    }

    fn diff_names(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError> {
        self.record(format!("diff_names {} {base} {head}", repo.display()));
        Ok(self.state().diff_names.clone())
    }

    fn diff_text(&self, repo: &Path, base: &str, head: &str) -> Result<String, CoreError> {
        self.record(format!("diff_text {} {base} {head}", repo.display()));
        Ok(self.state().diff_text.clone())
    }

    fn commits_between(&self, repo: &Path, base: &str, head: &str) -> Result<Vec<String>, CoreError> {
        self.record(format!("commits_between {} {base} {head}", repo.display()));
        Ok(self.state().commits.clone())
    }

    fn head_sha(&self, repo: &Path, rev: &str) -> Result<String, CoreError> {
        self.record(format!("head_sha {} {rev}", repo.display()));
        Ok(self.state().head_sha.clone())
    }

    fn ls_remote(&self, repo: &Path, remote: &str, branch: &str) -> Result<String, CoreError> {
        self.record(format!("ls_remote {} {remote} {branch}", repo.display()));
        Ok(self.state().ls_remote.clone())
    }

    fn status_clean(&self, repo: &Path) -> Result<bool, CoreError> {
        self.record(format!("status_clean {}", repo.display()));
        Ok(self.state().status_clean)
    }

    fn rebase(&self, worktree: &Path, onto: &str) -> Result<RebaseResult, CoreError> {
        self.record(format!("rebase {} {onto}", worktree.display()));
        Ok(self.next_rebase())
    }

    fn rebase_abort(&self, worktree: &Path) -> Result<(), CoreError> {
        self.record(format!("rebase_abort {}", worktree.display()));
        Ok(())
    }

    fn rebase_continue(&self, worktree: &Path) -> Result<RebaseResult, CoreError> {
        self.record(format!("rebase_continue {}", worktree.display()));
        Ok(self.next_rebase())
    }

    fn add(&self, worktree: &Path, paths: &[String]) -> Result<(), CoreError> {
        self.record(format!("add {} {}", worktree.display(), paths.join(",")));
        Ok(())
    }

    fn checkout_path(&self, worktree: &Path, rev: &str, path: &str) -> Result<(), CoreError> {
        self.record(format!("checkout_path {} {rev} {path}", worktree.display()));
        Ok(())
    }

    fn commit(&self, worktree: &Path, message: &str) -> Result<String, CoreError> {
        self.record(format!("commit {} {message}", worktree.display()));
        Ok("deadbee".to_owned())
    }

    fn reset_hard(&self, repo: &Path, rev: &str) -> Result<(), CoreError> {
        self.record(format!("reset_hard {} {rev}", repo.display()));
        Ok(())
    }

    fn conflicted_files(&self, worktree: &Path) -> Result<Vec<String>, CoreError> {
        self.record(format!("conflicted_files {}", worktree.display()));
        Ok(self.state().conflicted_files.clone())
    }

    fn merge_ff_only(&self, repo: &Path, branch: &str) -> Result<(), CoreError> {
        self.record(format!("merge_ff_only {} {branch}", repo.display()));
        Ok(())
    }

    fn push(&self, repo: &Path, remote: &str, branch: &str) -> Result<(), CoreError> {
        self.record(format!("push {} {remote} {branch}", repo.display()));
        Ok(())
    }

    fn last_commit_time(&self, repo: &Path, rev: &str) -> Result<Option<Timestamp>, CoreError> {
        self.record(format!("last_commit_time {} {rev}", repo.display()));
        Ok(self.state().last_commit_time)
    }
}
