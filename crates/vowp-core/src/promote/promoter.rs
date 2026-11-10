//! The promoter: guards, serving bank, candidate diff, approval, deploy, probe and record.
//! Spec: `docs/design/crates/vowp-core.md` §8, `docs/design/plan.md` §10 and
//! `docs/design/concepts/work-to-production.md`.
//!
//! A promotion is intentionally not idempotent across terminal releases: a refused or failed
//! release stays refused/failed until the operator fixes the cause and promotes again. Calls
//! for the same change while a release is `banked` resume that release (after an inbox
//! approval answer), and a release that is already `live` is returned unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use vowp_protocol::{
    Approval, EnvConfig, GuardResult, InboxId, InboxItem, InboxKind, InboxState, JobId, LedgerEntry, LedgerKind,
    ProjectName, Release, ReleaseId, ReleaseState,
};
use vowp_store::Store;

use crate::CoreError;
use crate::deps::{CommandOutput, CommandSpec, Deps};

use super::{bank, guards, releases_log};

/// Wall-clock limit for one deploy command.
const DEPLOY_TIMEOUT_SECS: u64 = 1800;

/// Wall-clock limit for the post-deploy probe.
const PROBE_TIMEOUT_SECS: u64 = 600;

/// Options for [`Promoter::promote`] and [`Promoter::rollback`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromoteOpts {
    /// Run guards and the candidate diff only; never bank, deploy, probe or ask for approval.
    pub dry_run: bool,
    /// Approve a `policy` environment explicitly. Never bypasses a human approval.
    pub yes: bool,
    /// Operator identity recorded when `yes` approves a policy environment.
    pub approved_by: Option<String>,
}

/// Guarded promotion and rollback of finished work.
pub struct Promoter {
    deps: Deps,
}

/// What an approval decision said.
enum ApprovalOutcome {
    /// Approved by the named operator (or `policy`).
    Approved(String),
    /// Waiting for a human answer.
    Waiting,
    /// Rejected or dismissed by a human.
    Rejected(String),
}

impl Promoter {
    /// Creates a promoter over the injected dependencies.
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Runs the promotion pipeline and returns the release in its final state for this call.
    ///
    /// The pipeline is guards → serving bank → candidate diff → approval → deploy → probe →
    /// record. A guard or diff failure stops at `refused`; a deploy or probe failure stops at
    /// `failed` with the rollback command still recorded. A human approval stops at `banked`
    /// until the inbox item is answered and the call is repeated. `dry_run` stops at `guarded`.
    pub fn promote(
        &self,
        store: &Store,
        project: &ProjectName,
        env: &str,
        change: &str,
        opts: PromoteOpts,
    ) -> Result<Release, CoreError> {
        let env_config = self.env_config(env)?.clone();
        let checkout = PathBuf::from(&env_config.checkout);
        let repo = self.project_repo(&env_config);

        if let Some(existing) = self.existing_release(store, project, env, change)? {
            match existing.state {
                ReleaseState::Live => return Ok(existing),
                ReleaseState::Banked if opts.dry_run => {
                    // A dry run never deploys, probes or asks for approval, even when a
                    // previously banked release is waiting for a human answer.
                    return Ok(existing);
                }
                ReleaseState::Banked => {
                    return self.continue_after_bank(store, existing, &env_config, &checkout, &opts);
                }
                _ => {}
            }
        }

        let to_commit = resolve_change(&self.deps, store, &repo, change)?;
        let from_commit = self.deps.git.head_sha(&checkout, "HEAD").ok();
        let mut release = Release {
            id: ReleaseId::new(),
            project: project.clone(),
            env: env.to_owned(),
            change_ref: change.to_owned(),
            from_commit,
            to_commit: Some(to_commit.clone()),
            state: ReleaseState::Planned,
            guards: Vec::new(),
            snapshot_path: None,
            rollback_cmd: None,
            probe: Value::Null,
            approved_by: None,
            rolled_back_by: None,
            note: String::new(),
            created_at: self.deps.clock.now(),
            finished_at: None,
        };
        store.insert_release(&release)?;

        for name in &env_config.guards {
            let result = self.run_guard(&env_config, name, &checkout);
            let failed = !result.passed;
            release.guards.push(result);
            if failed {
                let guard = release.guards.last().cloned().unwrap_or_else(|| GuardResult {
                    name: name.clone(),
                    passed: false,
                    detail: String::new(),
                });
                return self.refuse(store, release, format!("guard {} failed: {}", guard.name, guard.detail));
            }
        }
        release.state = ReleaseState::Guarded;
        store.update_release(&release)?;

        if opts.dry_run {
            let dry_from = self.deps.git.head_sha(&checkout, "HEAD").ok();
            match self.unexpected_files(&repo, &checkout, dry_from.as_deref(), &to_commit) {
                Ok(files) if !files.is_empty() => {
                    return self.refuse(
                        store,
                        release,
                        format!(
                            "candidate differs from the serving copy outside the intended change: {}",
                            files.join(", ")
                        ),
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    return self.refuse(
                        store,
                        release,
                        format!("could not compare candidate with the serving copy: {err}"),
                    );
                }
            }
            self.record_ledger(store, &release, LedgerKind::Promote)?;
            return Ok(release);
        }

        let bank_path = self.bank_path(&release);
        let banked = match bank::bank_serving(&self.deps, &env_config, env, &bank_path) {
            Ok(banked) => banked,
            Err(err) => {
                return self.refuse(store, release, format!("serving bank failed: {err}"));
            }
        };
        if banked.commit.is_some() {
            release.from_commit = banked.commit.clone();
        }
        release.snapshot_path = banked.path.exists().then(|| banked.path.display().to_string());
        release.rollback_cmd = (!banked.rollback_cmd.trim().is_empty()).then(|| banked.rollback_cmd.clone());
        release.state = ReleaseState::Banked;
        store.update_release(&release)?;

        match self.unexpected_files(&repo, &checkout, release.from_commit.as_deref(), &to_commit) {
            Ok(files) if !files.is_empty() => {
                return self.refuse(
                    store,
                    release,
                    format!(
                        "candidate differs from the banked serving copy outside the intended change: {}",
                        files.join(", ")
                    ),
                );
            }
            Ok(_) => {}
            Err(err) => {
                return self.refuse(
                    store,
                    release,
                    format!("could not compare candidate with the banked serving copy: {err}"),
                );
            }
        }

        self.continue_after_bank(store, release, &env_config, &checkout, &opts)
    }

    /// Redeploys the banked serving copy of `release` as a new rollback release.
    ///
    /// The original release becomes `rolled_back` and records the new release id in
    /// `rolled_back_by` only when the rollback command succeeds. A failed rollback leaves the
    /// original release untouched and returns the new release in `failed`.
    pub fn rollback(&self, store: &Store, release: &ReleaseId, opts: PromoteOpts) -> Result<Release, CoreError> {
        let original = store
            .release(release)?
            .ok_or_else(|| CoreError::NotFound(format!("release {release}")))?;
        let rollback_cmd = original
            .rollback_cmd
            .clone()
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| CoreError::Invalid(format!("release {release} has no rollback command")))?;
        let env_config = self.env_config(&original.env)?.clone();
        let checkout = PathBuf::from(&env_config.checkout);
        let now = self.deps.clock.now();
        let mut rollback_release = Release {
            id: ReleaseId::new(),
            project: original.project.clone(),
            env: original.env.clone(),
            change_ref: format!("rollback {}", original.id),
            from_commit: original.to_commit.clone(),
            to_commit: original.from_commit.clone(),
            state: ReleaseState::Failed,
            guards: Vec::new(),
            snapshot_path: original.snapshot_path.clone(),
            rollback_cmd: Some(rollback_cmd.clone()),
            probe: Value::Null,
            approved_by: opts.approved_by.clone(),
            rolled_back_by: None,
            note: format!("rolls back {}", original.id),
            created_at: now,
            finished_at: Some(now),
        };
        store.insert_release(&rollback_release)?;

        match self.run_command(&rollback_cmd, &checkout, DEPLOY_TIMEOUT_SECS) {
            Ok(output) if !output.timed_out && output.exit_code == Some(0) => {
                rollback_release.state = ReleaseState::RolledBack;
                rollback_release.note = format!(
                    "restored {} from {}",
                    original.id,
                    rollback_release.snapshot_path.as_deref().unwrap_or("-")
                );
                store.update_release(&rollback_release)?;

                let mut restored = original;
                restored.state = ReleaseState::RolledBack;
                restored.rolled_back_by = Some(rollback_release.id.clone());
                restored.finished_at = Some(now);
                restored.note = format!("rolled back by {}", rollback_release.id);
                store.update_release(&restored)?;

                self.record_ledger(store, &rollback_release, LedgerKind::Rollback)?;
                releases_log::append(&self.deps, &restored)?;
                releases_log::append(&self.deps, &rollback_release)?;
                Ok(rollback_release)
            }
            Ok(output) => {
                rollback_release.note = format!(
                    "rollback command failed: exit {:?}{}",
                    output.exit_code,
                    if output.timed_out { " (timed out)" } else { "" }
                );
                store.update_release(&rollback_release)?;
                self.record_ledger(store, &rollback_release, LedgerKind::Rollback)?;
                releases_log::append(&self.deps, &rollback_release)?;
                Ok(rollback_release)
            }
            Err(err) => {
                rollback_release.note = format!("rollback command could not run: {err}");
                store.update_release(&rollback_release)?;
                self.record_ledger(store, &rollback_release, LedgerKind::Rollback)?;
                releases_log::append(&self.deps, &rollback_release)?;
                Ok(rollback_release)
            }
        }
    }

    /// Lists a project's releases, newest first, optionally restricted to one environment.
    pub fn releases(
        &self,
        store: &Store,
        project: &ProjectName,
        env: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Release>, CoreError> {
        let mut releases = store.list_releases(project, env)?;
        releases.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(releases)
    }

    /// Returns the configured environment, or a typed not-found error.
    fn env_config(&self, env: &str) -> Result<&EnvConfig, CoreError> {
        self.deps
            .config
            .env
            .get(env)
            .ok_or_else(|| CoreError::NotFound(format!("environment {env}")))
    }

    /// Returns the repository used to resolve commits and intended-change files.
    fn project_repo(&self, env: &EnvConfig) -> PathBuf {
        self.deps
            .config
            .project
            .path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&env.checkout))
    }

    /// Returns the newest `banked` (resumable) or `live` release for this change.
    fn existing_release(
        &self,
        store: &Store,
        project: &ProjectName,
        env: &str,
        change: &str,
    ) -> Result<Option<Release>, CoreError> {
        Ok(store.list_releases(project, Some(env))?.into_iter().find(|release| {
            release.change_ref == change && matches!(release.state, ReleaseState::Banked | ReleaseState::Live)
        }))
    }

    /// Returns the serving-bank destination for one release.
    fn bank_path(&self, release: &Release) -> PathBuf {
        let env = release.env.replace(['/', '\\'], "_");
        self.deps
            .paths
            .root()
            .join("banks")
            .join(format!("{env}-{}.tar", release.id.as_str()))
    }

    /// Runs one configured guard, turning infrastructure errors into failed evidence.
    fn run_guard(&self, env: &EnvConfig, name: &str, checkout: &Path) -> GuardResult {
        match guards::run_guard(&self.deps, env, name, checkout) {
            Ok(result) => result,
            Err(err) => GuardResult {
                name: name.to_owned(),
                passed: false,
                detail: err.to_string(),
            },
        }
    }

    /// Resolves approval for a banked release and continues to deploy when approved.
    fn continue_after_bank(
        &self,
        store: &Store,
        mut release: Release,
        env_config: &EnvConfig,
        checkout: &Path,
        opts: &PromoteOpts,
    ) -> Result<Release, CoreError> {
        match self.resolve_approval(store, &release, env_config, opts)? {
            ApprovalOutcome::Approved(by) => {
                release.approved_by = Some(by);
                release.state = ReleaseState::Approved;
                store.update_release(&release)?;
                self.deploy_and_probe(store, release, env_config, checkout)
            }
            ApprovalOutcome::Waiting => Ok(release),
            ApprovalOutcome::Rejected(by) => self.refuse(store, release, format!("approval rejected by {by}")),
        }
    }

    /// Decides whether a banked release may deploy.
    fn resolve_approval(
        &self,
        store: &Store,
        release: &Release,
        env_config: &EnvConfig,
        opts: &PromoteOpts,
    ) -> Result<ApprovalOutcome, CoreError> {
        if env_config.approval == Approval::Policy && opts.yes {
            let by = opts.approved_by.clone().unwrap_or_else(|| "policy".to_owned());
            return Ok(ApprovalOutcome::Approved(by));
        }
        match self.approval_item(store, release)? {
            Some(item) => match item.state {
                InboxState::Answered => {
                    let answer = item.answer.as_deref().unwrap_or_default();
                    match verdict(answer) {
                        Some(true) => {
                            let by = item
                                .answered_by
                                .clone()
                                .or_else(|| opts.approved_by.clone())
                                .unwrap_or_else(|| "unknown".to_owned());
                            Ok(ApprovalOutcome::Approved(by))
                        }
                        Some(false) => Ok(ApprovalOutcome::Rejected(
                            item.answered_by.clone().unwrap_or_else(|| "unknown".to_owned()),
                        )),
                        None => Ok(ApprovalOutcome::Waiting),
                    }
                }
                InboxState::Dismissed => Ok(ApprovalOutcome::Rejected("dismissed".to_owned())),
                InboxState::Open => Ok(ApprovalOutcome::Waiting),
            },
            None => {
                self.ask_approval(store, release)?;
                Ok(ApprovalOutcome::Waiting)
            }
        }
    }

    /// Creates the approval inbox item when it does not exist yet.
    fn ask_approval(&self, store: &Store, release: &Release) -> Result<(), CoreError> {
        if self.approval_item(store, release)?.is_some() {
            return Ok(());
        }
        let item = InboxItem {
            id: InboxId::new(),
            project: Some(release.project.clone()),
            job_id: None,
            kind: InboxKind::Approval,
            title: format!("Promote {} to {}?", release.change_ref, release.env),
            body: approval_body(release),
            options: vec!["approve".to_owned(), "reject".to_owned()],
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: self.deps.clock.now(),
            answered_at: None,
        };
        store.insert_inbox(&item)?;
        Ok(())
    }

    /// Finds the inbox item that belongs to `release`.
    fn approval_item(&self, store: &Store, release: &Release) -> Result<Option<InboxItem>, CoreError> {
        let marker = approval_marker(release);
        let mut open = None;
        let mut dismissed = None;
        for item in store.list_inbox(None)? {
            if !item.body.contains(&marker) {
                continue;
            }
            match item.state {
                InboxState::Answered => return Ok(Some(item)),
                InboxState::Open => open = Some(item),
                InboxState::Dismissed => dismissed = Some(item),
            }
        }
        Ok(open.or(dismissed))
    }

    /// Deploys the candidate and runs the optional probe, recording `failed` or `live`.
    fn deploy_and_probe(
        &self,
        store: &Store,
        mut release: Release,
        env_config: &EnvConfig,
        checkout: &Path,
    ) -> Result<Release, CoreError> {
        release.state = ReleaseState::Deploying;
        store.update_release(&release)?;
        for command in &env_config.deploy {
            match self.run_command(command, checkout, DEPLOY_TIMEOUT_SECS) {
                Ok(output) if !output.timed_out && output.exit_code == Some(0) => {}
                Ok(output) => {
                    return self.fail(
                        store,
                        release,
                        format!(
                            "deploy command {command:?} failed: exit {:?}{}",
                            output.exit_code,
                            if output.timed_out { " (timed out)" } else { "" }
                        ),
                    );
                }
                Err(err) => {
                    return self.fail(
                        store,
                        release,
                        format!("deploy command {command:?} could not run: {err}"),
                    );
                }
            }
        }

        release.state = ReleaseState::Probing;
        store.update_release(&release)?;
        match &env_config.probe {
            Some(command) => match self.run_command(command, checkout, PROBE_TIMEOUT_SECS) {
                Ok(output) => {
                    let passed = !output.timed_out && output.exit_code == Some(0);
                    release.probe = json!({
                        "command": command,
                        "exit_code": output.exit_code,
                        "timed_out": output.timed_out,
                        "passed": passed,
                        "tail": output.tail_lines(40),
                    });
                    if !passed {
                        return self.fail(
                            store,
                            release,
                            format!(
                                "probe {command:?} failed: exit {:?}{}",
                                output.exit_code,
                                if output.timed_out { " (timed out)" } else { "" }
                            ),
                        );
                    }
                }
                Err(err) => {
                    release.probe = json!({
                        "command": command,
                        "passed": false,
                        "error": err.to_string(),
                    });
                    return self.fail(store, release, format!("probe {command:?} could not run: {err}"));
                }
            },
            None => {
                release.probe = json!({"passed": true, "skipped": true});
            }
        }

        release.state = ReleaseState::Live;
        release.finished_at = Some(self.deps.clock.now());
        release.note = format!("deployed {} to {}", release.change_ref, release.env);
        store.update_release(&release)?;
        self.record_ledger(store, &release, LedgerKind::Promote)?;
        releases_log::append(&self.deps, &release)?;
        Ok(release)
    }

    /// Marks a release `refused` and records the terminal state.
    fn refuse(&self, store: &Store, mut release: Release, note: String) -> Result<Release, CoreError> {
        release.state = ReleaseState::Refused;
        release.note = note;
        release.finished_at = Some(self.deps.clock.now());
        store.update_release(&release)?;
        self.record_ledger(store, &release, LedgerKind::Promote)?;
        releases_log::append(&self.deps, &release)?;
        Ok(release)
    }

    /// Marks a release `failed` and records the terminal state.
    fn fail(&self, store: &Store, mut release: Release, note: String) -> Result<Release, CoreError> {
        release.state = ReleaseState::Failed;
        release.note = note;
        release.finished_at = Some(self.deps.clock.now());
        store.update_release(&release)?;
        self.record_ledger(store, &release, LedgerKind::Promote)?;
        releases_log::append(&self.deps, &release)?;
        Ok(release)
    }

    /// Appends one ledger row for a completed release.
    fn record_ledger(&self, store: &Store, release: &Release, kind: LedgerKind) -> Result<(), CoreError> {
        let state = serde_json::to_value(release.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let entry = LedgerEntry {
            id: None,
            ts: self.deps.clock.now(),
            kind,
            project: Some(release.project.clone()),
            job_id: None,
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: format!("{} {} {}: {}", kind_label(kind), release.env, release.change_ref, state),
            detail: serde_json::to_value(release).unwrap_or(Value::Null),
        };
        store.append_ledger(&entry)?;
        Ok(())
    }

    /// Returns files that differ from the candidate but are not part of the intended change.
    ///
    /// The environment checkout was just banked and is the serving copy at this instant.
    /// `git diff <to>` lists tracked differences from the candidate; untracked files are
    /// always drift. A path in the intended change may legitimately differ while the deploy
    /// brings it forward.
    fn unexpected_files(
        &self,
        repo: &Path,
        checkout: &Path,
        from_commit: Option<&str>,
        to_commit: &str,
    ) -> Result<Vec<String>, CoreError> {
        let intended: BTreeSet<String> = match from_commit {
            Some(from) => self.deps.git.diff_names(repo, from, to_commit)?.into_iter().collect(),
            None => BTreeSet::new(),
        };
        let mut changed = self.git_lines(checkout, &["diff", "--name-only", to_commit])?;
        changed.extend(self.git_lines(checkout, &["ls-files", "--others", "--exclude-standard"])?);
        let mut unexpected: Vec<String> = changed.into_iter().filter(|path| !intended.contains(path)).collect();
        unexpected.sort();
        unexpected.dedup();
        Ok(unexpected)
    }

    /// Runs one shell command in `cwd`.
    fn run_command(&self, command: &str, cwd: &Path, timeout_secs: u64) -> Result<CommandOutput, CoreError> {
        self.deps.cmd.run(&CommandSpec {
            shell: Some(command.to_owned()),
            argv: Vec::new(),
            cwd: Some(cwd.to_path_buf()),
            env: Vec::new(),
            timeout: Duration::from_secs(timeout_secs.max(1)),
        })
    }

    /// Runs `git` in `cwd` and returns its non-empty output lines.
    fn git_lines(&self, cwd: &Path, args: &[&str]) -> Result<Vec<String>, CoreError> {
        let mut argv = vec!["git".to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let output = self.deps.cmd.run(&CommandSpec {
            shell: None,
            argv,
            cwd: Some(cwd.to_path_buf()),
            env: git_env(),
            timeout: Duration::from_secs(120),
        })?;
        if output.exit_code != Some(0) {
            return Err(CoreError::Command(format!(
                "git {} failed: exit {:?}\n{}",
                args.join(" "),
                output.exit_code,
                output.tail_lines(20)
            )));
        }
        Ok(output
            .text()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// Resolves `change` (a commit, ref or job id) to a full commit sha.
fn resolve_change(deps: &Deps, store: &Store, repo: &Path, change: &str) -> Result<String, CoreError> {
    if let Ok(sha) = deps.git.head_sha(repo, change)
        && !sha.trim().is_empty()
    {
        return Ok(sha);
    }
    if let Ok(job_id) = JobId::parse(change)
        && let Some(job) = store.job(&job_id)?
        && let Some(branch) = job.branch
        && let Ok(sha) = deps.git.head_sha(repo, &branch)
        && !sha.trim().is_empty()
    {
        return Ok(sha);
    }
    Err(CoreError::Invalid(format!("unknown change: {change}")))
}

/// Builds the approval item body, including the marker used to find it again.
fn approval_body(release: &Release) -> String {
    let guards = release
        .guards
        .iter()
        .map(|guard| format!("{}={}", guard.name, guard.passed))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}project: {}\nenv: {}\nchange: {}\nfrom: {} to: {}\nrollback: {}\nguards: {}",
        approval_marker(release),
        release.project,
        release.env,
        release.change_ref,
        release.from_commit.as_deref().unwrap_or("-"),
        release.to_commit.as_deref().unwrap_or("-"),
        release.rollback_cmd.as_deref().unwrap_or("-"),
        guards
    )
}

/// The stable marker that links an inbox item to its release.
fn approval_marker(release: &Release) -> String {
    format!("release: {}\n", release.id)
}

/// Interprets an inbox answer.
fn verdict(answer: &str) -> Option<bool> {
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "yes" || answer.starts_with("approve") {
        Some(true)
    } else if answer == "no" || answer.starts_with("reject") {
        Some(false)
    } else {
        None
    }
}

/// Returns a short label for a ledger kind.
fn kind_label(kind: LedgerKind) -> &'static str {
    match kind {
        LedgerKind::Promote => "promote",
        LedgerKind::Rollback => "rollback",
        _ => "release",
    }
}

/// Environment that keeps git hermetic: no user or system config, no prompts.
fn git_env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
        ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use super::verdict;

    #[test]
    fn approval_verdict_accepts_only_explicit_answers() {
        assert_eq!(verdict("approve"), Some(true));
        assert_eq!(verdict("Approved"), Some(true));
        assert_eq!(verdict("yes"), Some(true));
        assert_eq!(verdict("reject"), Some(false));
        assert_eq!(verdict("No"), Some(false));
        assert_eq!(verdict("maybe"), None);
    }
}
