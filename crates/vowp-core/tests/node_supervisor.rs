//! Node supervisor tests: one supervisor, every active project, round-robin.
//!
//! Spec: `docs/design/v0.2/home-and-surfaces.md` §4.2–§4.3. Everything is hermetic: a
//! temporary node home, per-project `vowp.toml` files, a fake clock, a fake window manager
//! and no harness process.

use std::sync::Arc;

use tempfile::TempDir;
use vowp_core::deps::{AdapterRegistry, FixedRng, SystemRunner};
use vowp_core::loops::NodeSupervisor;
use vowp_core::requests::{Request, push};
use vowp_core::{Deps, SystemGit};
use vowp_protocol::{
    ActiveState, Cost, FakeClock, InboxState, Job, JobId, JobKind, JobState, MemoryLog, NewJob, NodeName, Project,
    ProjectName, ProjectState, Role, Run, RunId, RunPurpose, RunRole, Session, SessionRole, SessionState, Timestamp,
    VowpConfig,
};
use vowp_store::{MigrationReport, NodePaths, RunFinish, Store};
use vowp_tmux::fake::FakeWindows;

/// Fixed instant every test clock starts at.
fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("timestamp")
}

/// One temporary node with a global store and fake dependencies.
struct NodeEnv {
    tmp: TempDir,
    paths: NodePaths,
    deps: Deps,
    clock: FakeClock,
    log: Arc<MemoryLog>,
}

impl NodeEnv {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path().join("node"));
        paths.ensure().expect("node paths");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = ProjectName::parse("base").expect("project");
        config.project.path = Some(tmp.path().join("base").display().to_string());
        let clock = FakeClock::at(ts());
        let log = Arc::new(MemoryLog::new());
        let windows = Arc::new(FakeWindows::new());
        let cmd = Arc::new(SystemRunner);
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: log.clone(),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: windows.clone(),
            git: Arc::new(SystemGit::new(cmd.clone())),
            cmd,
            rng: Arc::new(FixedRng(0)),
        };
        Self {
            tmp,
            paths,
            deps,
            clock,
            log,
        }
    }

    /// Returns a global store handle.
    fn global(&self) -> Store {
        Store::open_global(&self.paths)
            .expect("global store")
            .with_clock(Arc::new(self.clock.clone()))
    }

    /// Writes a project checkout with `vowp.toml` and registers it.
    fn add_project<F>(&self, name: &str, configure: F) -> ProjectName
    where
        F: FnOnce(&mut VowpConfig),
    {
        let name = ProjectName::parse(name).expect("project name");
        let root = self.tmp.path().join(name.as_str());
        std::fs::create_dir_all(root.join(".vowp/jobs")).expect("project dir");
        let mut config = VowpConfig::with_defaults_for_tests();
        config.project.name = name.clone();
        config.project.path = Some(root.display().to_string());
        config.project.worktree_root = Some(self.tmp.path().join("worktrees").display().to_string());
        configure(&mut config);
        std::fs::write(
            root.join("vowp.toml"),
            toml::to_string_pretty(&config).expect("config toml"),
        )
        .expect("write vowp.toml");

        let store = self.global();
        let now = ts();
        store
            .upsert_project(&Project {
                name: name.clone(),
                path: root.display().to_string(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::to_value(&config).expect("config json"),
                created_at: now,
                updated_at: now,
            })
            .expect("project row");
        name
    }

    /// Inserts one job into one project's database.
    fn insert_job(&self, project: &ProjectName, id: &str, state: JobState) -> Job {
        let store = Store::open_with_project(&self.paths, project)
            .expect("project store")
            .with_clock(Arc::new(self.clock.clone()));
        let job = store
            .insert_job(
                project,
                &NewJob {
                    id: vowp_protocol::JobId::parse(id).expect("job id"),
                    title: format!("job {id}"),
                    kind: JobKind::Build,
                    role: Role::Worker,
                    profile: Some("worker".to_owned()),
                    needs: Vec::new(),
                    owns: Vec::new(),
                    reads: Vec::new(),
                    gates: None,
                    priority: None,
                    loop_: None,
                    brief_text: Some("Do the work.\n".to_owned()),
                },
                "test",
            )
            .expect("insert job");
        if job.state != state {
            store
                .set_job_state(
                    &job.id,
                    state,
                    &vowp_store::StateChange {
                        note: None,
                        parked_reason: None,
                    },
                )
                .expect("state");
        }
        store.require_job(&job.id).expect("job")
    }

    /// Inserts one session into a project's attached global database.
    fn insert_session(&self, project: &ProjectName, state: SessionState) -> vowp_protocol::SessionId {
        let store = self.global();
        let id = vowp_protocol::SessionId::new();
        store
            .upsert_session(&Session {
                id: id.clone(),
                project: Some(project.clone()),
                job_id: None,
                role: Some(SessionRole::Worker),
                title: format!("{project} session"),
                parent_session_id: None,
                node: self.deps.node.clone(),
                cwd: self.tmp.path().display().to_string(),
                state,
                executor: None,
                last_seq: 0,
                last_capsule_id: None,
                log_bytes: 0,
                created_at: ts(),
                updated_at: ts(),
            })
            .expect("session row");
        id
    }

    /// Inserts one run row, optionally finishing it.
    fn insert_run(
        &self,
        project: &ProjectName,
        job: Option<&JobId>,
        session: Option<&vowp_protocol::SessionId>,
        started_at: Timestamp,
        finish: Option<vowp_protocol::RunOutcome>,
    ) -> Run {
        let store = Store::open_with_project(&self.paths, project)
            .expect("project store")
            .with_clock(Arc::new(self.clock.clone()));
        let run = Run {
            id: RunId::new(),
            job_id: job.cloned(),
            session_id: session.cloned(),
            role: RunRole::Worker,
            purpose: RunPurpose::Start,
            profile: None,
            rung: 0,
            executor: None,
            provider: None,
            node: self.deps.node.clone(),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at,
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        };
        store.insert_run(&run).expect("run row");
        if let Some(outcome) = finish {
            store
                .finish_run(
                    &run.id,
                    &RunFinish {
                        outcome,
                        exit_code: Some(0),
                        limit_kind: None,
                        cost: Cost::unknown(),
                        native_session_id: None,
                        detail: "test run finished".to_owned(),
                    },
                )
                .expect("finish run");
        }
        run
    }

    /// Returns the project names named by the `projects` step, in tick order.
    fn tick_order(report: &vowp_core::loops::NodeTickReport) -> Vec<String> {
        let detail = &report
            .steps
            .iter()
            .find(|step| step.name == "projects")
            .expect("projects step")
            .detail;
        let names = detail.split_once(": ").map(|(_, names)| names).unwrap_or_default();
        names
            .split(", ")
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

impl std::ops::Deref for NodeEnv {
    type Target = Deps;

    fn deref(&self) -> &Deps {
        &self.deps
    }
}

#[test]
fn new_registers_the_reserved_node_project_but_does_not_tick_it() {
    let env = NodeEnv::new();
    let node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    assert_eq!(node.projects().count(), 0, "only _node exists");
    let global = env.global();
    let reserved = ProjectName::parse("_node").expect("reserved");
    assert!(global.project(&reserved).expect("lookup").is_some());
    assert!(env.paths.project_db_path(&reserved).is_file());
    assert_eq!(node.order(), Vec::<ProjectName>::new());
}

#[test]
fn one_supervisor_round_robins_every_active_project() {
    let env = NodeEnv::new();
    for name in ["alpha", "beta", "gamma"] {
        env.add_project(name, |_| {});
    }
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");

    let first = node.tick().expect("tick 1");
    assert_eq!(NodeEnv::tick_order(&first), ["alpha", "beta", "gamma"]);
    assert_eq!(first.projects.len(), 3);
    assert!(first.skipped.is_empty(), "{:?}", first.skipped);

    let second = node.tick().expect("tick 2");
    assert_eq!(NodeEnv::tick_order(&second), ["beta", "gamma", "alpha"]);
    let third = node.tick().expect("tick 3");
    assert_eq!(NodeEnv::tick_order(&third), ["gamma", "alpha", "beta"]);
    assert_eq!(
        node.order()
            .into_iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );
}

#[test]
fn archived_projects_are_skipped_and_removed_supervisors_stop() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let archived = env.add_project("archived", |_| {});
    vowp_core::loops::registry::set_project_state(&env.global(), &archived, ProjectState::Archived, "owner")
        .expect("archive");
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    assert!(report.projects.contains_key(&alpha));
    assert!(!report.projects.contains_key(&archived));
    assert!(
        report.skipped.iter().any(|(name, _)| name == &archived),
        "{:?}",
        report.skipped
    );
}

#[test]
fn broken_project_parks_alone_after_three_failed_ticks() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let broken = env.add_project("broken", |_| {});
    // A needs cycle makes `Graph::new` fail on every tick, so the project's `states`
    // step (and dispatch) fails deterministically while every other project is healthy.
    let first = env.insert_job(&broken, "cycle-a", JobState::Pending);
    let second = env.insert_job(&broken, "cycle-b", JobState::Pending);
    let broken_store = Store::open_with_project(&env.paths, &broken).expect("store");
    for (src, dst) in [(&first.id, &second.id), (&second.id, &first.id)] {
        broken_store
            .add_edge(&vowp_protocol::Edge {
                src: src.clone(),
                dst: dst.clone(),
                kind: vowp_protocol::EdgeKind::Needs,
            })
            .expect("cycle edge");
    }

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    node.startup().expect("startup");
    for _ in 0..3 {
        let report = node.tick().expect("tick");
        assert!(report.projects.contains_key(&alpha), "alpha keeps ticking");
        assert!(
            report.projects.contains_key(&broken),
            "broken still reports its failure"
        );
        assert!(
            report.projects[&broken].steps.iter().any(|step| !step.ok),
            "the broken project must report a failed step: {:?}",
            report.projects[&broken].steps
        );
        assert!(
            report.projects[&alpha].steps.iter().all(|step| step.ok),
            "the healthy project must stay green"
        );
    }

    let global = env.global();
    let entry = vowp_core::loops::registry::project_entry(&global, &broken)
        .expect("lookup")
        .expect("entry");
    assert_eq!(entry.state, ProjectState::Paused);
    assert!(entry.paused_reason.starts_with("broken:"), "{}", entry.paused_reason);
    let broken_store = Store::open_with_project(&env.paths, &broken).expect("store");
    let loop_ = broken_store.loop_("l1-supervisor").expect("loop").expect("row");
    assert_eq!(loop_.fail_streak, 3, "three consecutive failures");
    let open = global.list_inbox(Some(InboxState::Open)).expect("inbox");
    assert!(
        open.iter().any(|item| {
            item.project.as_ref() == Some(&broken)
                && item.kind == vowp_protocol::InboxKind::Notice
                && item.title == "project broken paused"
        }),
        "{open:?}"
    );

    let fourth = node.tick().expect("tick 4");
    assert!(fourth.projects.contains_key(&alpha));
    assert!(!fourth.projects.contains_key(&broken));
    assert!(
        fourth
            .skipped
            .iter()
            .any(|(name, reason)| name == &broken && reason.starts_with("broken:")),
        "{:?}",
        fourth.skipped
    );

    // `vowp project resume` clears the reason; the next tick tries the project again.
    vowp_core::loops::registry::set_project_state(&global, &broken, ProjectState::Active, "").expect("resume");
    let fifth = node.tick().expect("tick 5");
    assert!(fifth.projects.contains_key(&broken), "resumed project ticks again");
    assert!(
        fifth.projects[&broken].steps.iter().any(|step| !step.ok),
        "the underlying failure is still visible"
    );
}

#[test]
fn a_project_added_after_startup_reconciles_cross_db_before_its_first_tick() {
    let env = NodeEnv::new();
    env.add_project("alpha", |_| {});
    let mut supervisor = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    supervisor.startup().expect("startup");

    // A project registered while the supervisor runs gets its first tick on the next
    // pass. That first tick must run the same cross-db reconcile as `startup()`.
    let beta = env.add_project("beta", |_| {});
    let store = Store::open_with_project(&env.paths, &beta)
        .expect("beta store")
        .with_clock(Arc::new(env.clock.clone()));
    let finished = Run {
        id: RunId::new(),
        job_id: None,
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: None,
        provider: None,
        node: env.deps.node.clone(),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at: ts(),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    store.insert_run(&finished).expect("run");
    store
        .finish_run(
            &finished.id,
            &vowp_store::RunFinish {
                outcome: vowp_protocol::RunOutcome::Crash,
                exit_code: None,
                limit_kind: None,
                cost: Cost::unknown(),
                native_session_id: None,
                detail: "test".to_owned(),
            },
        )
        .expect("finish run");

    let report = supervisor.tick().expect("tick");
    assert!(
        report.projects.contains_key(&beta),
        "the new project is ticked: {report:?}"
    );

    // Cross-db reconcile step 4 adds the missing cost note. Without the reconcile in the
    // first tick of a project added after `startup()`, the note stays absent.
    let store = Store::open_with_project(&env.paths, &beta).expect("beta store");
    let repairs = store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Note),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger rows");
    assert!(
        repairs
            .iter()
            .any(|entry| entry.run_id.as_ref() == Some(&finished.id) && entry.summary == "cost missing after crash"),
        "the cross-db repair note is written before the first L1 tick: {repairs:?}"
    );
}

#[test]
fn projects_do_not_shadow_each_others_databases() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let beta = env.add_project("beta", |_| {});
    let alpha_job = env.insert_job(&alpha, "alpha-job", JobState::Pending);
    let beta_job = env.insert_job(&beta, "beta-job", JobState::Pending);

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    node.tick().expect("tick");

    let alpha_store = Store::open_with_project(&env.paths, &alpha).expect("alpha store");
    let beta_store = Store::open_with_project(&env.paths, &beta).expect("beta store");
    assert_ne!(
        alpha_store.require_job(&alpha_job.id).expect("alpha job").state,
        JobState::Pending,
        "the node tick touched alpha's job"
    );
    assert_ne!(
        beta_store.require_job(&beta_job.id).expect("beta job").state,
        JobState::Pending,
        "the node tick touched beta's job"
    );
    assert!(
        alpha_store.job(&beta_job.id).expect("lookup").is_none(),
        "a project store never sees another project's jobs"
    );
    assert!(
        beta_store.job(&alpha_job.id).expect("lookup").is_none(),
        "a project store never sees another project's jobs"
    );
}

#[test]
fn node_tick_updates_every_projects_l8_watchdog_row() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let beta = env.add_project("beta", |_| {});
    let mut supervisor = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    supervisor.startup().expect("startup");

    supervisor.tick().expect("tick");

    for project in [&alpha, &beta] {
        let store = Store::open_with_project(&env.paths, project).expect("project store");
        let row = store.loop_("l8-watchdog").expect("loop lookup").expect("loop row");
        assert!(
            row.last_run_at.is_some(),
            "{project}'s L8 row records the node heartbeat: {row:?}"
        );
        assert_eq!(row.last_status, "heartbeat written", "{project}: {row:?}");
    }
}

#[test]
fn one_active_project_keeps_the_v01_board_and_node_heartbeat() {
    let env = NodeEnv::new();
    env.add_project("solo", |_| {});
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    assert_eq!(report.projects.len(), 1);
    assert!(env.paths.root().join("heartbeat").is_file());
    let board: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths.root().join("board.json")).expect("board"))
            .expect("board json");
    assert_eq!(board["project"], "solo");
    let store = Store::open_with_project(&env.paths, &ProjectName::parse("solo").expect("name")).expect("store");
    let loop_ = store.loop_("l1-supervisor").expect("loop").expect("row");
    assert_eq!(loop_.state, ActiveState::Active);
    assert_eq!(loop_.last_status, "ok");
}

#[test]
fn two_active_projects_defer_the_node_board_but_still_heartbeat() {
    let env = NodeEnv::new();
    env.add_project("one", |_| {});
    env.add_project("two", |_| {});
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    assert_eq!(report.projects.len(), 2);
    assert!(
        !env.paths.root().join("board.json").exists(),
        "the node board is deferred while projects are active"
    );
    assert!(env.paths.root().join("heartbeat").is_file());
    assert!(!env.log.contains("board failed"), "{:?}", env.log.lines());
}

#[test]
fn a_project_config_that_disagrees_with_the_registry_parks_that_project_only() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let wrong = env.add_project("wrong", |_| {});
    // Rewrite the checkout's config so it names another project.
    let root = env.tmp.path().join("wrong");
    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.name = ProjectName::parse("other").expect("name");
    config.project.path = Some(root.display().to_string());
    std::fs::write(root.join("vowp.toml"), toml::to_string_pretty(&config).expect("config")).expect("rewrite");

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    assert!(report.projects.contains_key(&alpha));
    assert!(
        report
            .skipped
            .iter()
            .any(|(name, reason)| name == &wrong && reason.contains("registry entry")),
        "{:?}",
        report.skipped
    );
    let entry = vowp_core::loops::registry::project_entry(&env.global(), &wrong)
        .expect("lookup")
        .expect("entry");
    assert_eq!(entry.state, ProjectState::Paused);
    assert!(entry.paused_reason.starts_with("broken:"));
}

#[test]
fn node_tick_drains_node_scoped_requests_before_any_project() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let beta = env.add_project("beta", |_| {});
    let job = env.insert_job(&beta, "node-request-job", JobState::Parked);
    assert!(
        env.insert_job(&alpha, "alpha-job", JobState::Parked).state == JobState::Parked,
        "alpha exists so the node request has a wrong project to land on first"
    );
    // Keep beta's graph from dispatching the resolved job, so the assertion below sees
    // the request's own state change rather than the dispatch that follows it.
    Store::open_with_project(&env.paths, &beta)
        .expect("beta store")
        .kv_set("paused", "1")
        .expect("pause beta dispatch");

    // An unbound handle writes `req:node:<id>`. With two projects, a project-level drain
    // would apply it to whichever project ticks first (alpha) and drop it because alpha
    // does not own the job. Only the node's own step can route it to beta.
    let request = push(&env.global(), &Request::ResolveJob { job: job.id.clone() }).expect("node request");
    assert_eq!(env.global().kv_prefix("req:node:").expect("queue").len(), 1);

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    let global_step = report
        .steps
        .iter()
        .find(|step| step.name == "global")
        .expect("global request step");
    assert!(global_step.ok, "{global_step:?}");
    assert!(
        global_step.detail.contains("1 node request(s) applied"),
        "{global_step:?}"
    );
    assert!(env.global().kv_prefix("req:node:").expect("queue").is_empty());
    assert_eq!(
        Store::open_with_project(&env.paths, &beta)
            .expect("beta store")
            .require_job(&job.id)
            .expect("job")
            .state,
        JobState::Ready,
        "the node routed the request to the project that owns the job"
    );
    assert!(
        Store::open_with_project(&env.paths, &alpha)
            .expect("alpha store")
            .job(&job.id)
            .expect("lookup")
            .is_none(),
        "the first project never saw another project's job"
    );
    let _ = request;
}

#[test]
fn paused_project_keeps_its_checkpoint_and_board_fresh() {
    let env = NodeEnv::new();
    let active = env.add_project("alpha", |_| {});
    let paused = env.add_project("paused", |_| {});
    vowp_core::loops::registry::set_project_state(&env.global(), &paused, ProjectState::Paused, "owner")
        .expect("pause");

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    let paused_report = report.projects.get(&paused).expect("paused project ticked");
    let board = paused_report
        .steps
        .iter()
        .find(|step| step.name == "board")
        .expect("paused project has the maintenance board step");
    assert!(board.ok, "{board:?}");
    assert!(board.detail.contains("checkpoint"), "{board:?}");
    assert!(
        !paused_report.steps.iter().any(|step| step.name == "dispatch"),
        "a paused project starts no new work: {paused_report:?}"
    );
    let checkpoints = env.tmp.path().join("paused/.vowp/checkpoints");
    let written = std::fs::read_dir(&checkpoints)
        .expect("checkpoint dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("checkpoint entries");
    assert_eq!(written.len(), 1, "the paused project wrote one checkpoint: {written:?}");
    let board: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths.root().join("board.json")).expect("board"))
            .expect("board json");
    assert_eq!(
        board["project"], "alpha",
        "the one active project keeps the v0.1 compatibility board"
    );

    // Once the active project is archived, the paused project is the only one left: the
    // v0.1 compatibility board must be written for it instead of being deferred forever.
    vowp_core::loops::registry::set_project_state(&env.global(), &active, ProjectState::Archived, "owner")
        .expect("archive");
    let report = node.tick().expect("second tick");
    assert!(report.projects.contains_key(&paused));
    let board: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(env.paths.root().join("board.json")).expect("board"))
            .expect("board json");
    assert_eq!(board["project"], "paused", "{board}");
}

#[test]
fn project_pause_lets_running_work_finish() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let paused = env.add_project("paused", |_| {});
    let job = env.insert_job(&alpha, "finish-me", JobState::Running);
    let queued = env.insert_job(&alpha, "do-not-start", JobState::Pending);

    // The run ended before the tick, but the job is still `running`: this is exactly the
    // work a pause must let the maintenance harvest advance.
    let worktree = env.tmp.path().join("alpha-worktree");
    let report_path = worktree.join(&job.report);
    std::fs::create_dir_all(report_path.parent().expect("report parent")).expect("report dir");
    std::fs::write(
        &report_path,
        format!(
            "---\njob: {}\nstatus: done\nbranch: job/{}\ncommits: []\ntests: \"cargo test: pass\"\n\
             files_changed: []\nopen_items: []\nquestions_for_owner: []\n---\n\nDone.\n",
            job.id, job.id
        ),
    )
    .expect("report");
    Store::open_with_project(&env.paths, &alpha)
        .expect("alpha store")
        .set_job_workspace(
            &job.id,
            Some(&format!("job/{}", job.id)),
            Some(&worktree.display().to_string()),
        )
        .expect("workspace");
    let run = env.insert_run(
        &alpha,
        Some(&job.id),
        None,
        Timestamp::from_unix_secs(0),
        Some(vowp_protocol::RunOutcome::Ok),
    );

    vowp_core::loops::registry::set_project_state(&env.global(), &paused, ProjectState::Paused, "owner")
        .expect("pause peer");
    vowp_core::loops::registry::set_project_state(&env.global(), &alpha, ProjectState::Paused, "owner")
        .expect("pause alpha");

    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    let report = node.tick().expect("tick");
    let tick = report.projects.get(&alpha).expect("paused project ticked");
    assert!(
        tick.transitions
            .iter()
            .any(|(id, from, to)| id == &job.id && *from == JobState::Running && *to == JobState::Gating),
        "the finished run advanced while paused: {:?}",
        tick.transitions
    );
    assert!(
        !tick.steps.iter().any(|step| step.name == "dispatch"),
        "the paused project dispatched nothing: {tick:?}"
    );
    let store = Store::open_with_project(&env.paths, &alpha).expect("alpha store");
    assert_eq!(
        store.require_job(&job.id).expect("job").state,
        JobState::Gating,
        "running work finished into its next state"
    );
    assert_eq!(
        store.require_job(&queued.id).expect("queued job").state,
        JobState::Ready,
        "queued work stays ready but was not started"
    );
    assert_eq!(
        store.require_run(&run.id).expect("run").outcome,
        Some(vowp_protocol::RunOutcome::Ok)
    );
}

#[test]
fn node_supervisor_restart_reconciles_cross_db() {
    let env = NodeEnv::new();
    let alpha = env.add_project("alpha", |_| {});
    let beta = env.add_project("beta", |_| {});
    let mut first = NodeSupervisor::new(env.deps.clone()).expect("first supervisor");
    first.startup().expect("first startup");
    first.tick().expect("first tick");
    drop(first);

    // Crash residue in two project databases: a running session with no open run
    // (alpha) and a closed run that never got its turn.ended/cost rows (beta).
    let running = env.insert_session(&alpha, SessionState::Running);
    let idle = env.insert_session(&beta, SessionState::Idle);
    let run = env.insert_run(&beta, None, Some(&idle), ts(), Some(vowp_protocol::RunOutcome::Crash));

    let mut restarted = NodeSupervisor::new(env.deps.clone()).expect("restarted supervisor");
    let report = restarted.startup().expect("restart startup");
    assert_eq!(report.projects, 2, "both projects reconciled");
    assert!(
        report.cross_db.sessions_repaired >= 1,
        "alpha's running session without a run is repaired: {:?}",
        report.cross_db
    );
    assert!(
        report.cross_db.terminal_events_repaired >= 1,
        "beta's closed run gets its terminal state: {:?}",
        report.cross_db
    );
    assert!(
        report.cross_db.ledgers_repaired >= 1,
        "beta's missing cost row is repaired: {:?}",
        report.cross_db
    );

    let alpha_store = Store::open_with_project(&env.paths, &alpha).expect("alpha store");
    assert_eq!(
        alpha_store.session(&running).expect("session").expect("row").state,
        SessionState::Idle
    );
    let beta_store = Store::open_with_project(&env.paths, &beta).expect("beta store");
    let notes = beta_store
        .ledger_since(&vowp_store::LedgerFilter {
            kind: Some(vowp_protocol::LedgerKind::Note),
            ..vowp_store::LedgerFilter::default()
        })
        .expect("ledger");
    assert!(
        notes
            .iter()
            .any(|entry| entry.run_id.as_ref() == Some(&run.id) && entry.summary == "cost missing after crash"),
        "{notes:?}"
    );
}

#[test]
fn startup_returns_the_migration_it_was_given() {
    let env = NodeEnv::new();
    env.add_project("alpha", |_| {});
    let migration = MigrationReport {
        from_version: 1,
        to_version: 2,
        backup: env.tmp.path().join("backups/pre-0.2.db").display().to_string(),
        projects: vec![ProjectName::parse("alpha").expect("project")],
        verified: true,
        ..MigrationReport::default()
    };
    let mut node = NodeSupervisor::new(env.deps.clone()).expect("supervisor");
    node.record_migration(migration.clone());
    let report = node.startup().expect("startup");
    assert_eq!(report.migration, Some(migration));
}

#[test]
fn the_node_systemd_unit_matches_the_home_supervisor_contract() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let installed = std::fs::read_to_string(root.join("scripts/vowp.service")).expect("installed unit");
    let template =
        std::fs::read_to_string(root.join("framework/templates/scripts/vowp.service")).expect("unit template");
    assert_eq!(installed, template, "the two unit copies must not drift");
    for line in [
        "Environment=VOWP_HOME=%h/.vowp",
        "ExecStart=%h/.local/bin/vowp up --foreground",
        "Restart=on-failure",
        "RestartSec=5",
        "KillMode=mixed",
        "WantedBy=default.target",
    ] {
        assert!(installed.contains(line), "unit is missing {line:?}:\n{installed}");
    }
    assert!(
        !installed.contains("Restart=always"),
        "always would restart the supervisor after `vowp down` exits 0"
    );

    // The constant `vowp init` embeds must not drift back to `Restart=always` either.
    let template = vowp_core::loops::l8::SYSTEMD_UNIT;
    for line in [
        "Environment=VOWP_HOME=%h/.vowp",
        "ExecStart=%h/.local/bin/vowp up --foreground",
        "Restart=on-failure",
        "RestartSec=5",
        "KillMode=mixed",
        "WantedBy=default.target",
    ] {
        assert!(template.contains(line), "SYSTEMD_UNIT is missing {line:?}:\n{template}");
    }
    assert!(
        !template.contains("Restart=always"),
        "SYSTEMD_UNIT must not restart after a clean `vowp down`"
    );
}
