//! Shared harness for the loop tests and drills.
//!
//! Everything is hermetic: a temporary node directory, a `FakeClock`, a fake adapter that
//! replays scripted events and writes scripted files, and (for merge tests) a real git repository
//! under the temporary directory. No network, no real `vowp`/`claude`/`opencode` binary.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tempfile::{TempDir, tempdir};
use vowp_core::deps::{AdapterRegistry, FixedRng, SystemRunner};
use vowp_core::loops::Supervisor;
use vowp_core::session::service::SessionService;
use vowp_core::{Deps, GitOps, SystemGit};
use vowp_protocol::{
    Actor, Adapter, AdapterCapabilities, AdapterError, AdapterRun, AssistantMessage, Cost, EventBody, FakeClock,
    HarnessEvent, HarnessKind, Job, JobId, JobKind, JobState, MemoryLog, NewJob, NodeName, ProbeResult, Project,
    ProjectName, Role, RunOutcome, RunPurpose, RunRole, RunSummary, StartInput, StopReason, Timestamp, VowpConfig,
};
use vowp_store::{NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

#[path = "support/fake_git.rs"]
mod fake_git;
use fake_git::FakeGit;

/// Fixed instant every test clock starts at.
pub fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp")
}

/// One scripted harness start.
#[derive(Clone, Default)]
pub struct ScriptAction {
    /// Events emitted to the session layer.
    pub events: Vec<EventBody>,
    /// Summary returned by `wait`.
    pub summary: Option<RunSummary>,
    /// Files written under the start cwd before events are emitted.
    pub files: Vec<(String, String)>,
    /// Commands run with the start cwd before events are emitted.
    pub commands: Vec<Vec<String>>,
    /// Delay before the run reports its summary (simulates a live run).
    pub delay: Option<Duration>,
    /// Holds the run open until the test releases it.
    pub hold: Option<ScriptHold>,
}

impl ScriptAction {
    /// A successful run that prints one assistant message.
    pub fn ok(text: &str) -> Self {
        Self {
            events: vec![EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() })],
            summary: Some(ok_summary()),
            ..Self::default()
        }
    }

    /// A crashed run.
    pub fn crash(text: &str) -> Self {
        Self {
            events: vec![EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() })],
            summary: Some(crash_summary()),
            ..Self::default()
        }
    }

    /// Adds one file written under the start cwd.
    pub fn with_file(mut self, path: impl Into<String>, content: impl Into<String>) -> Self {
        self.files.push((path.into(), content.into()));
        self
    }

    /// Adds one command run under the start cwd.
    pub fn with_command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.commands.push(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Makes the run stay open for `delay`.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Holds the run open until the test releases `hold`.
    ///
    /// The adapter marks the hold as started in [`Adapter::start`], after recording the
    /// start input, and the run blocks in [`AdapterRun::wait`] until [`ScriptHold::release`].
    pub fn with_hold(mut self, hold: ScriptHold) -> Self {
        self.hold = Some(hold);
        self
    }
}

/// How often a condition-based hold re-checks its condition.
const HOLD_POLL: Duration = Duration::from_millis(10);

/// Upper bound for a condition-based hold, so a broken condition cannot block a runner forever.
const HOLD_MAX_WAIT: Duration = Duration::from_secs(120);

/// A test-controlled gate that holds one scripted run open.
///
/// [`ScriptHold::wait_started`] proves the adapter has actually recorded the start, not
/// merely that a run row is open; wall-clock delays cannot provide that ordering. A hold can
/// also release itself when a test condition becomes true ([`ScriptHold::release_when`]), so a
/// run stays open until the supervisor has applied the state or counter change the test needs
/// instead of racing a sleep.
#[derive(Clone, Default)]
pub struct ScriptHold {
    started: Arc<(Mutex<bool>, Condvar)>,
    released: Arc<(Mutex<bool>, Condvar)>,
    condition: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl ScriptHold {
    /// Creates a hold that is closed until [`ScriptHold::release`] is called.
    pub fn new() -> Self {
        Self::default()
    }

    /// Releases the held run as soon as `condition` returns true.
    ///
    /// The condition is polled while the run waits. This keeps a run alive across supervisor
    /// ticks without assuming the machine will schedule the test thread within a fixed delay,
    /// and it releases the run when the supervisor applies the transition (for example a
    /// stall counter or a terminal state) that the test is waiting for.
    pub fn release_when(mut self, condition: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.condition = Some(Arc::new(condition));
        self
    }

    /// Waits until the adapter has started the held run, or `timeout` elapses.
    pub fn wait_started(&self, timeout: Duration) -> bool {
        Self::wait_flag(&self.started, timeout)
    }

    /// Releases the held run. The gate stays open for later runs.
    pub fn release(&self) {
        let (lock, condvar) = &*self.released;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condvar.notify_all();
    }

    fn mark_started(&self) {
        let (lock, condvar) = &*self.started;
        *lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        condvar.notify_all();
    }

    fn wait_released(&self) {
        let (lock, condvar) = &*self.released;
        let mut released = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(condition) = &self.condition else {
            while !*released {
                released = condvar
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            return;
        };
        let deadline = Instant::now() + HOLD_MAX_WAIT;
        loop {
            if *released || condition() {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let (guard, _) = condvar
                .wait_timeout(released, HOLD_POLL.min(deadline - now))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            released = guard;
        }
    }

    fn wait_flag(flag: &Arc<(Mutex<bool>, Condvar)>, timeout: Duration) -> bool {
        let (lock, condvar) = &**flag;
        let mut set = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + timeout;
        while !*set {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = condvar
                .wait_timeout(set, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            set = guard;
        }
        true
    }
}

/// Script callback: receives the start input and returns the next action.
pub type ScriptFn = Arc<dyn Fn(&StartInput) -> ScriptAction + Send + Sync>;

/// An adapter that scripts per-start behaviour.
pub struct ScriptAdapter {
    kind: HarnessKind,
    script: ScriptFn,
    starts: Arc<Mutex<Vec<StartInput>>>,
}

impl ScriptAdapter {
    /// Creates a scripted adapter.
    pub fn new(kind: HarnessKind, script: ScriptFn) -> Self {
        Self {
            kind,
            script,
            starts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Creates an adapter that always returns `action`.
    pub fn always(kind: HarnessKind, action: ScriptAction) -> Self {
        Self::new(kind, Arc::new(move |_| action.clone()))
    }

    /// Returns every start input in order.
    pub fn starts(&self) -> Vec<StartInput> {
        self.starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Adapter for ScriptAdapter {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn probe(&self) -> ProbeResult {
        ProbeResult {
            ok: true,
            version: Some("scripted-1.0".to_owned()),
            path: Some("scripted".to_owned()),
            detail: "scripted adapter".to_owned(),
        }
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_resume_native: true,
            ..AdapterCapabilities::default()
        }
    }

    fn start(&self, input: StartInput) -> Result<Box<dyn AdapterRun>, AdapterError> {
        self.starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input.clone());
        let action = (self.script)(&input);
        if let Some(hold) = &action.hold {
            hold.mark_started();
        }
        for (path, content) in &action.files {
            let path = input.cwd.join(path);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(AdapterError::Io)?;
            }
            std::fs::write(&path, content).map_err(AdapterError::Io)?;
        }
        for argv in &action.commands {
            if argv.is_empty() {
                continue;
            }
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).current_dir(&input.cwd);
            let _ = command.status();
        }
        let (sender, receiver) = channel();
        for body in &action.events {
            let _ = sender.send(HarnessEvent {
                ts: None,
                actor: Actor::Assistant,
                body: body.clone(),
                raw: None,
            });
        }
        drop(sender);
        Ok(Box::new(ScriptedRun {
            receiver,
            summary: action.summary.unwrap_or_else(ok_summary),
            delay: action.delay,
            hold: action.hold,
            interrupted: false,
        }))
    }
}

struct ScriptedRun {
    receiver: Receiver<HarnessEvent>,
    summary: RunSummary,
    delay: Option<Duration>,
    hold: Option<ScriptHold>,
    interrupted: bool,
}

impl AdapterRun for ScriptedRun {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn events(&self) -> &Receiver<HarnessEvent> {
        &self.receiver
    }

    fn send(&mut self, _text: &str) -> Result<(), AdapterError> {
        Err(AdapterError::Unsupported("scripted adapter has no live input"))
    }

    fn interrupt(&mut self) -> Result<(), AdapterError> {
        self.interrupted = true;
        self.delay = Some(Duration::from_millis(1));
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AdapterError> {
        self.delay = Some(Duration::from_millis(1));
        Ok(())
    }

    fn wait(&mut self) -> Result<RunSummary, AdapterError> {
        if let Some(hold) = &self.hold {
            hold.wait_released();
        }
        if let Some(delay) = self.delay {
            let deadline = Instant::now() + delay;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Ok(self.summary.clone())
    }
}

/// A successful run summary.
pub fn ok_summary() -> RunSummary {
    RunSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: None,
        stop_reason: StopReason::EndTurn,
    }
}

/// A crashed run summary.
pub fn crash_summary() -> RunSummary {
    RunSummary {
        exit_code: Some(137),
        outcome: RunOutcome::Crash,
        native_session_id: None,
        cost: Cost::unknown(),
        limit: None,
        result_text: None,
        stop_reason: StopReason::Crash,
    }
}

/// A summary carrying a native session id, for resume tests.
pub fn summary_with_native(native: &str) -> RunSummary {
    let mut summary = ok_summary();
    summary.native_session_id = Some(native.to_owned());
    summary
}

/// One hermetic project + node + store + supervisor harness.
pub struct TestNode {
    pub tmp: TempDir,
    pub project: ProjectName,
    pub deps: Deps,
    pub clock: FakeClock,
    pub windows: Arc<FakeWindows>,
    pub adapter: Arc<ScriptAdapter>,
    pub log: Arc<MemoryLog>,
    pub real_git: bool,
    pub main_repo: PathBuf,
}

impl Default for TestNode {
    fn default() -> Self {
        Self::new()
    }
}

impl TestNode {
    /// Creates a node with a fake git implementation.
    pub fn new() -> Self {
        Self::with_adapter(
            false,
            Arc::new(ScriptAdapter::always(HarnessKind::Opencode, ScriptAction::ok("ok"))),
        )
    }

    /// Creates a node with a real git repository and bare origin.
    pub fn new_real_git() -> Self {
        Self::with_adapter(
            true,
            Arc::new(ScriptAdapter::always(HarnessKind::Opencode, ScriptAction::ok("ok"))),
        )
    }

    /// Creates a node with a specific scripted adapter.
    pub fn with_adapter(real_git: bool, adapter: Arc<ScriptAdapter>) -> Self {
        Self::with_adapter_config(real_git, adapter, |_| {})
    }

    /// Creates a node with a scripted adapter and a callback that edits the config.
    pub fn with_adapter_config<F>(real_git: bool, adapter: Arc<ScriptAdapter>, configure: F) -> Self
    where
        F: FnOnce(&mut VowpConfig),
    {
        let tmp = tempdir().expect("tempdir");
        let project = ProjectName::parse("test").expect("project");
        let main_repo = tmp.path().join("project");
        if real_git {
            init_real_repo(&main_repo, &tmp.path().join("origin.git"));
        } else {
            std::fs::create_dir_all(&main_repo).expect("project dir");
        }
        let worktree_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&worktree_root).expect("worktree root");

        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.path = Some(main_repo.to_string_lossy().into_owned());
        config.project.worktree_root = Some(worktree_root.to_string_lossy().into_owned());
        config.project.name = project.clone();
        config.project.remote = "origin".to_owned();
        config.project.default_branch = "main".to_owned();
        config.node.tick_secs = 60;
        configure(&mut config);

        let paths = NodePaths::new(tmp.path().join("node"));
        paths.ensure().expect("node paths");
        let clock = FakeClock::at(ts());
        let windows = Arc::new(FakeWindows::new());
        let git: Arc<dyn GitOps> = if real_git {
            Arc::new(SystemGit::new(Arc::new(SystemRunner)))
        } else {
            Arc::new(FakeGit::new())
        };
        let log = Arc::new(MemoryLog::new());
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: log.clone(),
            adapters: Arc::new(AdapterRegistry::new().with("opencode", adapter.clone())),
            windows: windows.clone(),
            git,
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        let store = store(&deps);
        store
            .upsert_project(&Project {
                name: project.clone(),
                path: main_repo.to_string_lossy().into_owned(),
                remote_url: Some(tmp.path().join("origin.git").to_string_lossy().into_owned()),
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: ts(),
                updated_at: ts(),
            })
            .expect("project row");
        Self {
            tmp,
            project,
            deps,
            clock,
            windows,
            adapter,
            log,
            real_git,
            main_repo,
        }
    }

    /// Opens a store with this node's clock.
    pub fn store(&self) -> Store {
        store(&self.deps)
    }

    /// Creates a supervisor over this node.
    pub fn supervisor(&self) -> Supervisor {
        Supervisor::new(self.deps.clone(), self.project.clone()).expect("supervisor")
    }

    /// Inserts one pending job.
    pub fn insert_job(&self, id: &str, kind: JobKind, role: Role, gates: Option<Vec<vowp_protocol::Gate>>) -> Job {
        let store = self.store();
        let new = NewJob {
            id: JobId::parse(id).expect("job id"),
            title: format!("job {id}"),
            kind,
            role,
            profile: Some(match role {
                Role::Verifier => "verifier".to_owned(),
                Role::Planner => "planner".to_owned(),
                Role::Keeper => "keeper".to_owned(),
                Role::Worker => "worker".to_owned(),
            }),
            needs: Vec::new(),
            owns: vec!["src/**".to_owned()],
            reads: Vec::new(),
            gates,
            priority: None,
            loop_: None,
            brief_text: None,
        };
        store.insert_job(&self.project, &new, "test").expect("insert job")
    }

    /// Forces a job into a state without going through the graph.
    pub fn force_state(&self, job: &Job, state: JobState) {
        self.store()
            .set_job_state(
                &job.id,
                state,
                &StateChange {
                    note: None,
                    parked_reason: None,
                },
            )
            .expect("force state");
    }

    /// Waits until no run row is open, up to ten seconds.
    pub fn wait_runs(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let open = self.store().open_runs().expect("open runs");
            if open.is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "runs did not finish: {open:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ticks until `predicate` holds or the attempt limit is reached.
    pub fn tick_until<F>(&self, supervisor: &mut Supervisor, attempts: usize, mut predicate: F)
    where
        F: FnMut(&Store) -> bool,
    {
        for _ in 0..attempts {
            self.wait_runs();
            supervisor.tick().expect("tick");
            if predicate(&self.store()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("condition not reached after {attempts} ticks");
    }

    /// Runs one tick after any live runner has finished.
    pub fn tick_settled(&self, supervisor: &mut Supervisor) -> vowp_core::loops::TickReport {
        self.wait_runs();
        supervisor.tick().expect("tick")
    }

    /// Advances the fake clock.
    pub fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
    }

    /// Creates a real worktree for `job` with one committed file.
    pub fn worktree_with_file(&self, job: &Job, file: &str, content: &str) -> PathBuf {
        self.worktree_with_files(job, &[(file, content)])
    }

    /// Creates a real worktree for `job` with committed files.
    pub fn worktree_with_files(&self, job: &Job, files: &[(&str, &str)]) -> PathBuf {
        assert!(self.real_git, "worktree_with_file needs real git");
        let path = self.tmp.path().join("worktrees").join(job.id.as_str());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("worktree parent");
        }
        let branch = format!("job/{}", job.id);
        git(
            &self.main_repo,
            &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "main"],
        );
        for (file, content) in files {
            let path_file = path.join(file);
            if let Some(dir) = path_file.parent() {
                std::fs::create_dir_all(dir).expect("file dir");
            }
            std::fs::write(&path_file, content).expect("write file");
        }
        git(&path, &["add", "-A"]);
        git(
            &path,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "work",
            ],
        );
        self.store()
            .set_job_workspace(&job.id, Some(&branch), Some(&path.to_string_lossy()))
            .expect("workspace");
        path
    }

    /// Adds one commit on an existing worktree.
    pub fn commit_file(&self, worktree: &Path, file: &str, content: &str) {
        let path = worktree.join(file);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("file dir");
        }
        std::fs::write(&path, content).expect("write file");
        git(worktree, &["add", "-A"]);
        git(
            worktree,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "change",
            ],
        );
    }

    /// Runs a git command in `cwd` and asserts success.
    pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
        git(cwd, args)
    }
}

/// Opens a store over the node paths with the injected clock.
pub fn store(deps: &Deps) -> Store {
    Store::open(&deps.paths).expect("store").with_clock(deps.clock.clone())
}

/// Creates a session directly, used by tests that need a prior native session id.
pub fn create_session(node: &TestNode, job: &Job) -> vowp_protocol::SessionId {
    SessionService::new(node.deps.clone())
        .create_session(vowp_protocol::NewSession {
            project: Some(job.project.clone()),
            job_id: Some(job.id.clone()),
            role: vowp_protocol::SessionRole::Worker,
            title: job.id.as_str().to_owned(),
            cwd: PathBuf::from(
                job.worktree
                    .clone()
                    .unwrap_or_else(|| node.main_repo.to_string_lossy().into_owned()),
            ),
            parent: None,
            executor: Some(vowp_protocol::Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        })
        .expect("create session")
}

/// Renders a report Markdown file with the report front matter.
pub fn report_markdown(job: &str, branch: &str, status: &str, verdict: Option<&str>, findings: &[&str]) -> String {
    let mut text = format!(
        "---\njob: {job}\nstatus: {status}\nbranch: {branch}\ncommits: []\ntests: \"cargo test: pass\"\nfiles_changed: []\nopen_items: []\nquestions_for_owner: []\n"
    );
    if let Some(verdict) = verdict {
        text.push_str(&format!("verdict: {verdict}\n"));
    }
    if findings.is_empty() {
        text.push_str("findings: []\n");
    } else {
        text.push_str("findings:\n");
        for finding in findings {
            text.push_str(&format!("- {}\n", finding.replace('\n', " ")));
        }
    }
    text.push_str("---\n\nReport body.\n");
    text
}

/// Returns the id of the verify job for `original` at `attempt`.
pub fn verify_id(original: &str, attempt: u32) -> String {
    format!("{original}-verify-{attempt}")
}

/// Returns the id of the fix/merge job for `original` at `attempt`.
pub fn child_id(original: &str, kind: &str, attempt: u32) -> String {
    format!("{original}-{kind}-{attempt}")
}

/// Runs `git` in `cwd`, asserting a zero exit, and returns stdout.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Creates a bare origin and a clone at `main_repo` with one commit on `main`.
fn init_real_repo(main_repo: &Path, origin: &Path) {
    std::fs::create_dir_all(origin).expect("origin dir");
    git(origin, &["init", "--bare"]);
    git(origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    std::fs::create_dir_all(main_repo).expect("main dir");
    git(main_repo, &["init"]);
    git(main_repo, &["checkout", "-b", "main"]);
    git(main_repo, &["config", "user.name", "test"]);
    git(main_repo, &["config", "user.email", "test@example.com"]);
    git(main_repo, &["remote", "add", "origin", &origin.to_string_lossy()]);
    std::fs::write(main_repo.join("README.md"), "initial\n").expect("readme");
    std::fs::write(main_repo.join(".gitignore"), ".vowp/\n").expect("gitignore");
    git(main_repo, &["add", "-A"]);
    git(main_repo, &["commit", "-m", "initial"]);
    git(main_repo, &["push", "-u", "origin", "main"]);
}

/// Drains a `VecDeque` script in order.
pub fn script_queue(actions: Vec<ScriptAction>) -> Arc<Mutex<VecDeque<ScriptAction>>> {
    Arc::new(Mutex::new(actions.into()))
}

/// Pops the next action, or returns a successful default.
pub fn next_action(queue: &Arc<Mutex<VecDeque<ScriptAction>>>) -> ScriptAction {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop_front()
        .unwrap_or_else(|| ScriptAction::ok("default"))
}

/// Returns the job id encoded in a verify prompt's final line.
pub fn verify_prompt_job(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("Job: ").map(str::to_owned))
}

/// Returns the fix round encoded in a fix prompt.
pub fn fix_round(prompt: &str) -> Option<u32> {
    let start = prompt.find("(round ")? + "(round ".len();
    let rest = &prompt[start..];
    rest.split_whitespace().next()?.parse().ok()
}

/// Ensures a run purpose name for assertions.
pub fn purpose_name(purpose: RunPurpose) -> &'static str {
    match purpose {
        RunPurpose::Start => "start",
        RunPurpose::Continue => "continue",
        RunPurpose::Fix => "fix",
        RunPurpose::Verify => "verify",
        RunPurpose::Plan => "plan",
        RunPurpose::Keep => "keep",
        RunPurpose::Capsule => "capsule",
        RunPurpose::Probe => "probe",
        RunPurpose::Merge => "merge",
        RunPurpose::Gate => "gate",
    }
}

/// Returns run-role name for assertions.
pub fn role_name(role: RunRole) -> &'static str {
    match role {
        RunRole::Planner => "planner",
        RunRole::Keeper => "keeper",
        RunRole::Worker => "worker",
        RunRole::Verifier => "verifier",
        RunRole::Capsule => "capsule",
        RunRole::Probe => "probe",
        RunRole::Merge => "merge",
        RunRole::Gate => "gate",
    }
}
