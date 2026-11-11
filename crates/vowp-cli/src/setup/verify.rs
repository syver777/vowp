//! `vowp setup verify`: the fixture checks run after `setup home` and `setup control`.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §11.2. On the home, verify runs:
//!
//! 1. a read-only gc dry run;
//! 2. a fixture project through one fake harness turn, its gate and the real merge queue;
//! 3. a fresh `snapshot.json` written from the home's own store;
//! 4. an orchestrator chat session that answers through `vowp chat`.
//!
//! On a control machine the same checks run on the home over SSH, using the installed
//! `vowp setup verify --json`, and the control machine then fetches the home's fresh
//! snapshot and writes its cache. The fixture project is registered in the real home
//! under a unique archived name, uses the real home database and git, and is removed on
//! every exit path. Only tmux windows are faked; the fixture never attaches to a tmux
//! server.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vowp_core::Deps;
use vowp_core::lifecycle::{GcOptions, gc};
use vowp_core::loops::Supervisor;
use vowp_core::surface::orchestrator::{NewOrchestrator, Orchestrator};
use vowp_protocol::{
    EventBody, Gate, HarnessConfig, JobId, JobKind, JobState, NewJob, Node, OrchestratorApp, Project, ProjectName,
    ProjectState, Role, SNAPSHOT_FORMAT, SNAPSHOT_VERSION, Snapshot, Timestamp, VowpConfig,
};
use vowp_store::session_log::SessionLog;
use vowp_store::{JobFilter, NodePaths, SessionFilter, Store};
use vowp_tmux::fake::FakeWindows;

use crate::cmd::setup::SetupVerifyArgs;
use crate::node_ctx::{ControlHome, NodeRole};
use crate::setup::commands::{SshRunner, probe};
use crate::{CliError, Ctx, IoStreams};

use super::{SetupCheck, SetupOutcome, as_user, print_outcome};
use crate::setup::SetupRunner;

/// The fake harness reply used by the chat check.
const CHAT_REPLY: &str = "setup verify orchestrated answer";

/// Prefix for the fixture project and job ids. Each run adds a unique suffix so a
/// retained session index row can never be reused by a later run or by a real job.
const FIXTURE_JOB_PREFIX: &str = "setup-verify";

/// How long one fixture job may take to merge. A loaded CI runner can starve the real
/// supervisor and merge queue for much longer than a quiet machine; the check still fails
/// on a genuinely stuck job, only after a wider margin.
const FIXTURE_MERGE_TIMEOUT: Duration = Duration::from_secs(300);

/// A unique resident of the home's temporary verification area.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates a uniquely named directory under the home.
    fn new(root: &Path, label: &str) -> Result<Self, CliError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| CliError::Invalid(format!("system clock before 1970: {error}")))?
            .as_nanos();
        let path = root.join(format!(
            "setup-verify-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        // Remove the shared `setup-verify/` parent too when this was its last run.
        if let Some(parent) = self.path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

/// Removes one directory when it goes out of scope.
struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One fixture project registered in the configured home.
///
/// The project is archived while it exists so a running node supervisor never ticks it.
/// The fixture's own `Supervisor` drives it through the real project store and merge
/// queue. The project and job ids are unique per run, so a session index row retained
/// by the store can never be picked up by a later verification or by a real job.
/// Dropping the fixture removes its session directories, project row, database and
/// checkout; the global session index rows keep the run-unique job id.
struct Fixture {
    _scratch: Scratch,
    _cleanup: FixtureCleanup,
    paths: NodePaths,
    project: ProjectName,
    job: JobId,
    root: PathBuf,
    repo: PathBuf,
    deps: Deps,
}

/// Removes a fixture's files and its project row when it goes out of scope.
///
/// `Fixture::new` builds this before registering anything, so a failure part-way through
/// (for example `upsert_project` inserting the row before creating its database fails)
/// still removes the row, database, session directories and checkout.
struct FixtureCleanup {
    paths: NodePaths,
    project: ProjectName,
    root: PathBuf,
}

impl FixtureCleanup {
    /// Builds a guard for `project`, whose files live below `root`.
    fn new(paths: NodePaths, project: ProjectName, root: PathBuf) -> Self {
        Self { paths, project, root }
    }
}

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if let Ok(store) = Store::open_with_project(&self.paths, &self.project)
            && let Ok(sessions) = store.list_sessions(&SessionFilter {
                project: Some(self.project.clone()),
                ..SessionFilter::default()
            })
        {
            for session in sessions {
                let _ = fs::remove_dir_all(self.paths.session_dir(&session.id));
            }
        }
        // The store has no delete-session API, so every fixture run leaves one global
        // session-index row. It keeps the run-unique job id, so neither a later verify nor
        // a real job can reuse it; only the row's session directory is removed.
        if let Ok(global) = Store::open_global(&self.paths) {
            let _ = global.remove_project_record(&self.project);
        }
        let _ = fs::remove_dir_all(self.paths.project_dir(&self.project));
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Registers a temporary project in `base`'s home and builds its dependencies.
    fn new(base: &Deps, label: &str, reply: &str, commit_file: bool) -> Result<Self, CliError> {
        let scratch = Scratch::new(&base.paths.root().join("setup-verify"), label)?;
        let suffix = unique_suffix();
        let project = ProjectName::parse(&format!("{FIXTURE_JOB_PREFIX}-{label}-{suffix}"))?;
        let job = JobId::parse(&format!("{FIXTURE_JOB_PREFIX}-{label}-{suffix}"))?;
        let root = scratch.path.clone();
        let cleanup = FixtureCleanup::new(base.paths.clone(), project.clone(), root.clone());
        let repo = root.join("project");
        let origin = root.join("origin.git");
        fs::create_dir_all(&repo)?;
        git(&repo, &["init", "--quiet"])?;
        git(&repo, &["checkout", "--quiet", "-b", "main"])?;
        git(
            &repo,
            &[
                "-c",
                "user.name=setup verify",
                "-c",
                "user.email=setup@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture base",
            ],
        )?;
        git(&root, &["init", "--quiet", "--bare", "origin.git"])?;
        git(&repo, &["remote", "add", "origin", &origin.display().to_string()])?;
        git(&repo, &["push", "--quiet", "-u", "origin", "main"])?;

        let harness = root.join("fake-opencode");
        write_exec(&harness, &harness_script(&job, reply, commit_file))?;

        let mut config = VowpConfig::with_defaults_for_tests();
        config.node.name = base.node.clone();
        config.node.tick_secs = 1;
        config.node.max_workers = 1;
        config.node.max_verifiers = 1;
        config.project.name = project.clone();
        config.project.path = Some(repo.display().to_string());
        config.project.remote = "origin".to_owned();
        config.project.default_branch = "main".to_owned();
        config.project.push = false;
        config.project.worktree_root = Some(root.join("worktrees").display().to_string());
        config.node.cwd_allow.push(root.display().to_string());
        config.harnesses.insert(
            "opencode".to_owned(),
            HarnessConfig {
                binary: Some(harness.display().to_string()),
                ..HarnessConfig::default()
            },
        );
        config.validate()?;
        fs::write(
            repo.join("vowp.toml"),
            toml::to_string_pretty(&config)
                .map_err(|error| CliError::Invalid(format!("fixture vowp.toml: {error}")))?,
        )?;
        fs::write(repo.join(".gitignore"), ".vowp/\n")?;
        git(&repo, &["add", "vowp.toml", ".gitignore"])?;
        git(
            &repo,
            &[
                "-c",
                "user.name=setup verify",
                "-c",
                "user.email=setup@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture config",
            ],
        )?;
        git(&repo, &["push", "--quiet", "origin", "main"])?;

        let now = base.clock.now();
        let global = Store::open_global(&base.paths)?;
        if global.local_node()?.is_none() {
            global.upsert_node(&Node {
                name: base.node.clone(),
                ssh_target: None,
                is_local: true,
                capabilities: serde_json::json!({"vowp": env!("CARGO_PKG_VERSION")}),
                last_seen_at: Some(now),
                created_at: now,
            })?;
        }
        global.upsert_project(&Project {
            name: project.clone(),
            path: repo.display().to_string(),
            remote_url: Some(origin.display().to_string()),
            default_branch: "main".to_owned(),
            config: serde_json::to_value(&config).map_err(vowp_protocol::ProtocolError::from)?,
            created_at: now,
            updated_at: now,
        })?;
        global.set_project_state(&project, ProjectState::Archived, "")?;
        Store::create_project_db(&base.paths, &project)?;
        drop(global);

        let adapters = vowp_core::AdapterRegistry::from_config(&config)?;
        let deps = Deps {
            paths: base.paths.clone(),
            config: Arc::new(config),
            node: base.node.clone(),
            clock: base.clock.clone(),
            log: base.log.clone(),
            adapters: Arc::new(adapters),
            windows: Arc::new(FakeWindows::new()),
            git: base.git.clone(),
            cmd: base.cmd.clone(),
            rng: base.rng.clone(),
        };
        Ok(Self {
            _scratch: scratch,
            _cleanup: cleanup,
            paths: base.paths.clone(),
            project,
            job,
            root,
            repo,
            deps,
        })
    }
}

/// Runs `vowp setup verify` and returns 1 when any check failed.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, _args: &SetupVerifyArgs) -> Result<i32, CliError> {
    let checks = if let Some(control) = ctx.control() {
        control_checks(control)
    } else if ctx.role() == NodeRole::Home {
        home_checks(ctx)
    } else {
        vec![check(
            "home.checks",
            "fail",
            "vowp home is not initialised; run `vowp setup home` and `vowp setup control` first",
        )]
    };

    let failed = checks.iter().any(|check| check.status == "fail");
    let outcome = SetupOutcome {
        status: if failed { "failed" } else { "ok" }.to_owned(),
        phase: "verify".to_owned(),
        changed: Vec::new(),
        handoff: None,
        checks,
    };
    if ctx.json() {
        crate::output::print_json(io, &outcome)?;
    } else {
        print_outcome(io, &outcome)?;
    }
    Ok(if failed { 1 } else { 0 })
}

/// Runs the checks on the home that holds the store.
fn home_checks(ctx: &Ctx) -> Vec<SetupCheck> {
    let mut checks = Vec::new();
    push_check(&mut checks, "gc.dry_run", gc_dry_run(ctx));
    push_check(&mut checks, "fixture.job_merge", fixture_job_merges(ctx));
    push_check(&mut checks, "control.snapshot", snapshot_check(ctx));
    push_check(&mut checks, "orchestrator.chat", orchestrator_answers_chat(ctx));
    checks
}

/// Runs the home checks over SSH and adds the live control-side snapshot.
fn control_checks(control: &ControlHome) -> Vec<SetupCheck> {
    let mut checks = Vec::new();
    let checked_after = Timestamp::now();
    let mut remote = match remote_home_checks(control) {
        Ok(outcome) => outcome.checks,
        Err(error) => {
            let detail = error.to_string();
            for name in ["gc.dry_run", "fixture.job_merge", "orchestrator.chat"] {
                checks.push(check(name, "fail", format!("home checks failed: {detail}")));
            }
            checks.push(check(
                "control.snapshot",
                "fail",
                format!("home checks failed: {detail}"),
            ));
            return checks;
        }
    };
    // Keep the home's own snapshot result: it proves the home just wrote a fresh
    // `snapshot.json`. Fetching the file separately can otherwise turn a failed write
    // followed by an older valid file into a false `ok`.
    let home_snapshot = take_check(&mut remote, "control.snapshot");
    for name in ["gc.dry_run", "fixture.job_merge"] {
        checks.push(take_check(&mut remote, name));
    }
    push_check(
        &mut checks,
        "control.snapshot",
        snapshot_from_control(control, home_snapshot, checked_after),
    );
    checks.push(take_check(&mut remote, "orchestrator.chat"));
    checks
}

/// Removes one named check, or returns a failing placeholder when it is missing.
fn take_check(checks: &mut Vec<SetupCheck>, name: &str) -> SetupCheck {
    let Some(index) = checks.iter().position(|check| check.name == name) else {
        return check(name, "fail", "the home did not report this check");
    };
    checks.remove(index)
}

/// Runs `vowp setup verify --json` on the configured home.
fn remote_home_checks(control: &ControlHome) -> Result<SetupOutcome, CliError> {
    let mut runner = SshRunner::new(control.config.ssh_target.clone());
    let probe = probe(&mut runner, &[])?;
    let binary = remote_vowp_binary(&control.config.remote_home);
    let argv = as_user(
        &probe.user,
        probe.effective_user(),
        &probe.effective_home(),
        vec![
            binary,
            "--home".to_owned(),
            control.config.remote_home.clone(),
            "setup".to_owned(),
            "verify".to_owned(),
            "--json".to_owned(),
        ],
    );
    let output = runner.run(&argv, None)?;
    // A home with a failed check exits 1 but still prints its complete JSON document, so
    // parse the document first and use its per-check results. A truly failed command has
    // no document; its stderr is the useful part and must not be discarded.
    serde_json::from_str(&output.stdout).map_err(|error| {
        CliError::Remote(format!(
            "the home returned an invalid setup verify document (exit {:?}): {error}; {}",
            output.status,
            output.detail()
        ))
    })
}

/// Returns the per-user binary `setup home` installs on the home.
fn remote_vowp_binary(remote_home: &str) -> String {
    let trimmed = remote_home.trim_end_matches('/');
    let home = trimmed.strip_suffix("/.vowp").unwrap_or(trimmed);
    format!("{home}/.local/bin/vowp")
}

/// Runs the read-only gc report.
pub fn gc_dry_run(ctx: &Ctx) -> Result<SetupCheck, CliError> {
    let store = ctx.fleet_store()?;
    let report = gc(ctx.deps(), &store, &GcOptions::default())?;
    let files: u64 = report.classes.iter().map(|class| class.files).sum();
    let detail = if report.classes.is_empty() {
        "dry run found no lifecycle files to collect".to_owned()
    } else {
        format!(
            "dry run lists {} file(s) in {} class(es); nothing was deleted",
            files,
            report.classes.len()
        )
    };
    Ok(check(
        "gc.dry_run",
        if report.errors.is_empty() { "ok" } else { "warn" },
        if report.errors.is_empty() {
            detail
        } else {
            format!("{detail}; warnings: {}", report.errors.join("; "))
        },
    ))
}

/// Runs a fixture job through one fake turn, its gate and the real merge queue on `ctx`'s home.
pub fn fixture_job_merges(ctx: &Ctx) -> Result<SetupCheck, CliError> {
    let fixture = Fixture::new(ctx.deps(), "job", "setup verify fixture answered", true)?;
    let store = fixture.deps.open_store()?;
    let job_id = fixture.job.clone();
    let mut supervisor = Supervisor::new(fixture.deps.clone(), fixture.project.clone())?;
    supervisor.startup()?;
    store.insert_job(
        &fixture.project,
        &NewJob {
            id: job_id.clone(),
            title: "setup verify fixture".to_owned(),
            kind: JobKind::Build,
            role: Role::Worker,
            profile: Some("worker".to_owned()),
            needs: Vec::new(),
            owns: vec!["setup-verify.txt".to_owned(), ".vowp/**".to_owned()],
            reads: Vec::new(),
            gates: Some(vec![Gate::OwnsRespected(true)]),
            priority: None,
            loop_: None,
            brief_text: None,
        },
        "setup verify",
    )?;

    let deadline = Instant::now() + FIXTURE_MERGE_TIMEOUT;
    loop {
        let _ = supervisor.tick();
        let job = store.require_job(&job_id)?;
        match job.state {
            JobState::Done => break,
            JobState::Failed | JobState::Parked | JobState::Cancelled | JobState::Replanned => {
                let last_run = store.last_run_of_job(&job_id)?;
                let jobs = store.list_jobs(&JobFilter {
                    project: Some(fixture.project.clone()),
                    ..JobFilter::default()
                })?;
                let states = jobs
                    .iter()
                    .map(|job| format!("{}={}", job.id, job.state.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let gates = store.gate_results_for(&job_id, job.attempts)?;
                let failed_gates = gates
                    .iter()
                    .filter(|gate| !gate.passed)
                    .map(|gate| format!("{:?} {:?}", gate.gate, gate.evidence))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Ok(check(
                    "fixture.job_merge",
                    "fail",
                    format!(
                        "fixture job ended in {}{}; note {:?}; jobs {}; failed gates {}; last run {}",
                        job.state.as_str(),
                        job.parked_reason
                            .as_ref()
                            .map(|reason| format!(" ({reason:?})"))
                            .unwrap_or_default(),
                        job.note,
                        states,
                        failed_gates,
                        last_run
                            .as_ref()
                            .map(|run| format!("{:?}: {}", run.outcome, first_line(&run.detail, "")))
                            .unwrap_or_else(|| "none".to_owned())
                    ),
                ));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            let last_run = store.last_run_of_job(&job_id)?;
            let jobs = store.list_jobs(&JobFilter {
                project: Some(fixture.project.clone()),
                ..JobFilter::default()
            })?;
            let states = jobs
                .iter()
                .map(|job| format!("{}={}", job.id, job.state.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(check(
                "fixture.job_merge",
                "fail",
                format!(
                    "fixture job did not merge within {}s (last state {}; continues {}; jobs {}; last run {})",
                    FIXTURE_MERGE_TIMEOUT.as_secs(),
                    job.state.as_str(),
                    job.continues,
                    states,
                    last_run
                        .as_ref()
                        .map(|run| format!("{:?}: {}", run.outcome, first_line(&run.detail, "")))
                        .unwrap_or_else(|| "none".to_owned())
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let merged = fixture.repo.join("setup-verify.txt");
    let content = fs::read_to_string(&merged).unwrap_or_default();
    if !content.contains("setup verify fixture turn") {
        return Ok(check(
            "fixture.job_merge",
            "fail",
            format!(
                "fixture job reached done but {} is missing or empty after the merge",
                merged.display()
            ),
        ));
    }
    Ok(check(
        "fixture.job_merge",
        "ok",
        format!(
            "fixture job {} merged into main on home {} (project {})",
            job_id,
            fixture.paths.root().display(),
            fixture.project
        ),
    ))
}

/// Creates an orchestrator chat session on `ctx`'s home and sends it one turn through `vowp chat`.
pub fn orchestrator_answers_chat(ctx: &Ctx) -> Result<SetupCheck, CliError> {
    let fixture = Fixture::new(ctx.deps(), "chat", CHAT_REPLY, false)?;
    let orchestrator = Orchestrator::new(fixture.deps.clone());
    let info = orchestrator.create(&NewOrchestrator {
        name: format!("setup-verify-{}", unique_suffix()),
        project: Some(fixture.project.clone()),
        app: OrchestratorApp::Chat,
        executor: None,
        force: false,
    })?;
    let _orchestrator_dir = RemoveDir(info.dir.clone());

    let binary = vowp_binary()?;
    let mut command = Command::new(&binary);
    command
        .arg("--home")
        .arg(fixture.paths.root())
        .arg("--project")
        .arg(&fixture.repo)
        .arg("--no-color")
        .arg("chat")
        .arg(info.session.as_str())
        .current_dir(fixture.root.as_path())
        .env("HOME", &fixture.root)
        .env("TMUX_TMPDIR", fixture.root.join("tmux"))
        .env("NO_COLOR", "1")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("VOWP_CONTROL_HOME");
    fs::create_dir_all(fixture.root.join("tmux"))?;
    let output = run_with_input(command, "hello\n/quit\n")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Ok(check(
            "orchestrator.chat",
            "fail",
            format!(
                "vowp chat exited {:?}: {}",
                output.status.code(),
                first_line(&stderr, &stdout)
            ),
        ));
    }
    if !stdout.contains(CHAT_REPLY) {
        return Ok(check(
            "orchestrator.chat",
            "fail",
            format!(
                "vowp chat did not print the orchestrator's answer: {}",
                first_line(&stdout, &stderr)
            ),
        ));
    }

    let events = SessionLog::open_read(&fixture.paths, &info.session)?.read_all()?;
    let answered = events.iter().any(|event| {
        matches!(
            event.body(),
            Ok(EventBody::AssistantMessage(message)) if message.text.contains(CHAT_REPLY)
        )
    });
    if !answered {
        return Ok(check(
            "orchestrator.chat",
            "fail",
            format!("orchestrator session {} has no assistant answer event", info.session),
        ));
    }
    Ok(check(
        "orchestrator.chat",
        "ok",
        format!(
            "orchestrator {} answered through `vowp chat` in session {} on home {}",
            info.card.name,
            info.session,
            fixture.paths.root().display()
        ),
    ))
}

/// Writes a fresh home snapshot, then checks that a control machine can read one.
pub fn snapshot_check(ctx: &Ctx) -> Result<SetupCheck, CliError> {
    let store = ctx.fleet_store()?;
    vowp_core::fleet::write_snapshot(ctx.deps(), &store, ctx.clock().now())?;
    let snapshot = vowp_core::fleet::read_snapshot(ctx.paths())?;
    let mut result = validate_snapshot(&snapshot, Timestamp::now(), "home snapshot")?;
    if result.status == "ok" {
        result.status = "warn".to_owned();
        result.detail = format!(
            "{}; no control machine is configured on this machine, so readability from a control machine was not verified",
            result.detail
        );
    }
    Ok(result)
}

/// How far the control machine's clock may run ahead of the home's before a snapshot is
/// treated as stale. NTP keeps real machines much closer; the allowance only avoids a
/// false failure from ordinary clock skew.
const SNAPSHOT_CLOCK_SKEW: Duration = Duration::from_secs(60);

/// Fetches the home snapshot over SSH and writes the control cache.
fn snapshot_from_control(
    control: &ControlHome,
    home_check: SetupCheck,
    checked_after: Timestamp,
) -> Result<SetupCheck, CliError> {
    // The home reports whether its own fresh write succeeded. Preserve a failure instead
    // of replacing it with whatever valid `snapshot.json` happens to be left on disk.
    if home_check.status == "fail" {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!("the home's own snapshot check failed: {}", home_check.detail),
        ));
    }
    let mut runner = SshRunner::new(control.config.ssh_target.clone());
    let probe = probe(&mut runner, &[])?;
    let path = format!("{}/snapshot.json", control.config.remote_home.trim_end_matches('/'));
    let argv = as_user(
        &probe.user,
        probe.effective_user(),
        &probe.effective_home(),
        vec!["cat".to_owned(), path],
    );
    let output = runner.run(&argv, None)?;
    if !output.success() {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!(
                "the home snapshot is not readable from the control machine: {}",
                first_line(&output.stderr, &output.stdout)
            ),
        ));
    }
    let snapshot: Snapshot = serde_json::from_str(&output.stdout)
        .map_err(|error| CliError::Remote(format!("the home returned an invalid snapshot: {error}")))?;
    // Even when the home's check passed, refuse a file written before this verification
    // started. This catches a stale file if an older home reports a check it did not run.
    let oldest_fresh = checked_after.minus(SNAPSHOT_CLOCK_SKEW);
    if snapshot.generated_at < oldest_fresh {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!(
                "the home snapshot was generated at {} before this check started at {}; refusing a stale snapshot",
                snapshot.generated_at.as_rfc3339(),
                checked_after.as_rfc3339()
            ),
        ));
    }
    let fetched_at = Timestamp::now();
    let snapshot_bytes = serde_json::to_vec_pretty(&snapshot).map_err(vowp_protocol::ProtocolError::from)?;
    crate::remote::write_private(&control.root.join("snapshot.json"), &snapshot_bytes)?;
    let meta = serde_json::json!({"fetched_at": fetched_at, "source": "remote"});
    let meta_bytes = serde_json::to_vec_pretty(&meta).map_err(vowp_protocol::ProtocolError::from)?;
    crate::remote::write_private(&control.root.join("snapshot.meta.json"), &meta_bytes)?;
    // Validate the cache we just wrote, including that it names the configured home.
    snapshot_readable_from_control(&control.root)
}

/// Reads the snapshot cache written by `setup control --apply`.
pub fn snapshot_readable_from_control(root: &Path) -> Result<SetupCheck, CliError> {
    let Some(cached) = crate::remote::cached_snapshot(root)? else {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!(
                "no readable snapshot cache in {}; run `vowp setup control --apply` or `vowp fleet status --refresh`",
                root.join("snapshot.json").display()
            ),
        ));
    };
    if let Some(config) = crate::remote::read_control_config(root)?
        && config.home != cached.snapshot.home.as_str()
    {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!(
                "cached snapshot names home {} but control.json names {}",
                cached.snapshot.home, config.home
            ),
        ));
    }
    validate_snapshot(&cached.snapshot, cached.fetched_at, &cached.source)
}

/// Validates the snapshot document and returns an `ok` check with its `as of` time.
fn validate_snapshot(snapshot: &Snapshot, fetched_at: Timestamp, source: &str) -> Result<SetupCheck, CliError> {
    if snapshot.schema != SNAPSHOT_FORMAT || snapshot.version != SNAPSHOT_VERSION {
        return Ok(check(
            "control.snapshot",
            "fail",
            format!(
                "unsupported snapshot {} version {}; this build reads {} version {}",
                snapshot.schema, snapshot.version, SNAPSHOT_FORMAT, SNAPSHOT_VERSION
            ),
        ));
    }
    Ok(check(
        "control.snapshot",
        "ok",
        format!(
            "snapshot as of {} (home {}, project(s) {}, source {source})",
            fetched_at.as_rfc3339(),
            snapshot.home,
            snapshot.projects.len()
        ),
    ))
}

/// Runs one check and turns a setup error into a failed check instead of aborting.
fn push_check(checks: &mut Vec<SetupCheck>, name: &str, result: Result<SetupCheck, CliError>) {
    match result {
        Ok(check) => checks.push(check),
        Err(error) => checks.push(SetupCheck {
            name: name.to_owned(),
            status: "fail".to_owned(),
            detail: error.to_string(),
        }),
    }
}

/// Builds one check row.
fn check(name: &str, status: &str, detail: impl Into<String>) -> SetupCheck {
    SetupCheck {
        name: name.to_owned(),
        status: status.to_owned(),
        detail: detail.into(),
    }
}

/// Returns the first non-empty line of a command result, for a short check detail.
fn first_line(primary: &str, fallback: &str) -> String {
    let text = if primary.trim().is_empty() {
        fallback.trim()
    } else {
        primary.trim()
    };
    text.lines().next().unwrap_or_default().chars().take(300).collect()
}

/// Writes an executable shell script.
fn write_exec(path: &Path, body: &str) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Returns a unique lower-case slug suffix.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let value = u64::try_from(nanos).unwrap_or(u64::MAX);
    format!(
        "{:x}-{}",
        value ^ u64::from(std::process::id()),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Returns the fake opencode script for `job`.
///
/// The report path is taken from the prompt, so a fix child (whose id differs from the
/// fixture job's id) writes its own report instead of leaving its run without one. A
/// child without a report stalls forever and keeps the fixture job in `fixing` until the
/// merge deadline.
fn harness_script(job: &JobId, reply: &str, commit_file: bool) -> String {
    let commit = if commit_file {
        format!(
            r#"sleep 1
prompt=""
for arg in "$@"; do prompt="$arg"; done
report=$(printf '%s\n' "$prompt" | grep -o '\.vowp/jobs/[^[:space:]]*/report\.md' | head -n 1)
if [ -z "$report" ]; then
  report=".vowp/jobs/{job}/report.md"
fi
report_job=$(basename "$(dirname "$report")")
branch=$(git symbolic-ref --short HEAD)
mkdir -p "$(dirname "$report")"
cat > "$report" <<VOWP_REPORT_EOF
---
job: $report_job
status: done
branch: $branch
---
setup verify fixture turn
VOWP_REPORT_EOF
printf '%s\n' 'setup verify fixture turn' > setup-verify.txt
git add "$report" setup-verify.txt >/dev/null 2>&1
git -c user.name='setup verify' -c user.email='setup@example.invalid' commit -m 'setup verify fixture turn' >/dev/null 2>&1
"#
        )
    } else {
        String::new()
    };
    let event = serde_json::json!({
        "type": "text",
        "timestamp": 1_794_168_421_621_i64,
        "sessionID": "ses_setup_verify",
        "part": {"text": reply}
    });
    let finish = serde_json::json!({
        "type": "step_finish",
        "timestamp": 1_794_168_421_622_i64,
        "sessionID": "ses_setup_verify",
        "part": {"cost": 0.0, "tokens": {"input": 1, "output": 1}, "reason": "stop"}
    });
    format!(
        "#!/bin/sh\n\
         if [ \"${{1:-}}\" = \"--version\" ]; then printf 'opencode 2.0.28\\n'; exit 0; fi\n\
         {commit}\
         printf '%s\\n' '{event}'\n\
         printf '%s\\n' '{finish}'\n"
    )
}

/// Runs one git command in `cwd` and returns its stdout.
fn git(cwd: &Path, args: &[&str]) -> Result<String, CliError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !output.status.success() {
        return Err(CliError::Remote(format!(
            "git {} failed: {}",
            args.join(" "),
            first_line(
                &String::from_utf8_lossy(&output.stderr),
                &String::from_utf8_lossy(&output.stdout)
            )
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Locates the `vowp` binary built for this checkout.
fn vowp_binary() -> Result<PathBuf, CliError> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_vowp") {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe()?;
    if current.file_name().is_some_and(|name| name == "vowp") {
        return Ok(current);
    }
    if let Some(debug) = current.parent().and_then(Path::parent) {
        let candidate = debug.join("vowp");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from("vowp"))
}

/// Runs a child with `input` on stdin and captures both streams.
fn run_with_input(mut command: Command, input: &str) -> Result<Output, CliError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(input.as_bytes())
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(CliError::Io(error));
    }
    child.wait_with_output().map_err(CliError::Io)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use tempfile::TempDir;
    use vowp_protocol::JobId;

    use super::harness_script;

    /// Runs the freshly written fake harness, retrying the ETXTBSY/EINTR races that a
    /// parallel test can cause by briefly holding a write descriptor or interrupting
    /// `exec`. Any other error, and a real harness failure, is returned unchanged.
    fn run_harness(script: &Path, prompt: &str, repo: &Path) -> std::process::Output {
        for _ in 0..19 {
            match Command::new(script).args(["run", prompt]).current_dir(repo).output() {
                Ok(output) => return output,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ExecutableFileBusy | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("run harness: {error}"),
            }
        }
        Command::new(script)
            .args(["run", prompt])
            .current_dir(repo)
            .output()
            .expect("run harness")
    }

    #[test]
    fn fixture_harness_writes_the_report_for_the_running_child_job() {
        let temp = TempDir::new().expect("temp fixture");
        let repo = temp.path();
        for args in [
            vec!["init", "--quiet"],
            vec!["checkout", "--quiet", "-b", "main"],
            vec![
                "-c",
                "user.name=setup verify",
                "-c",
                "user.email=setup@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture base",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }

        let script = repo.join("fake-opencode");
        let root = JobId::parse("setup-verify-job-root").expect("root job id");
        fs::write(&script, harness_script(&root, "fixture answer", true)).expect("write harness");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod harness");

        let child = "setup-verify-job-root-fix-1";
        let prompt = format!(
            "Fix exactly these problems in the same worktree and update {}/.vowp/jobs/{child}/report.md. \
             Do not widen scope.",
            repo.display()
        );
        let output = run_harness(&script, &prompt, repo);
        assert!(
            output.status.success(),
            "harness failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !repo.join(".vowp/jobs/setup-verify-job-root/report.md").exists(),
            "the root report must not satisfy a child run"
        );
        let report = fs::read_to_string(repo.join(format!(".vowp/jobs/{child}/report.md"))).expect("child report");
        assert!(report.contains(&format!("job: {child}")), "{report}");
        assert!(report.contains("status: done"), "{report}");
        assert!(report.contains("branch: main"), "{report}");
        assert!(
            repo.join("setup-verify.txt").is_file(),
            "the fixture commit must include its file"
        );
    }
}
