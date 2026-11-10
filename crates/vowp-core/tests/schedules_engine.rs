//! Schedule engine tests: ticks, missed-run policies and the three actions.
//! Spec: `docs/design/crates/vowp-core.md` §6 and `docs/design/loops.md` §11.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::schedules::{DueRun, FireOutcome, ScheduleEngine};
use vowp_core::{AdapterRegistry, CommandOutput, CommandRunner, CommandSpec, CoreError, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    ActiveState, Clock, FakeClock, Job, JobId, JobKind, LedgerKind, MissedPolicy, NewJob, NodeName, Project, Role,
    Schedule, ScheduleAction, ScheduleId, ScheduleSpecKind, Timestamp, VowpConfig,
};
use vowp_store::{LedgerFilter, NodePaths, Store};
use vowp_tmux::fake::FakeWindows;

/// A hermetic node with a file-backed store, injected clock and optional custom runner.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    deps: Deps,
    store: Store,
    clock: FakeClock,
    engine: ScheduleEngine,
}

impl Fixture {
    fn new(config: VowpConfig) -> Self {
        Self::with_runner(config, Arc::new(SystemRunner))
    }

    fn with_runner(config: VowpConfig, cmd: Arc<dyn CommandRunner>) -> Self {
        let tmp = tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(timestamp("2026-11-09T10:00:00Z"));
        let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock.clone()));
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config.clone()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: Arc::new(vowp_protocol::MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(FakeGit::new()),
            cmd,
            rng: Arc::new(FixedRng(0)),
        };
        let engine = ScheduleEngine::new(deps.clone());
        let now = store.clock().now();
        store
            .upsert_project(&Project {
                name: config.project.name.clone(),
                path: "/home/user/project".to_owned(),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("project");
        Self {
            _tmp: tmp,
            paths,
            deps,
            store,
            clock,
            engine,
        }
    }

    fn config(&self) -> &VowpConfig {
        &self.deps.config
    }

    fn dep(&self, config: VowpConfig) -> Deps {
        let mut deps = self.deps.clone();
        deps.config = Arc::new(config);
        deps
    }

    fn schedule(
        &self,
        name: &str,
        spec: &str,
        action: ScheduleAction,
        missed: MissedPolicy,
        next_run_at: Option<Timestamp>,
    ) -> Schedule {
        let schedule = Schedule {
            id: ScheduleId::new(),
            project: Some(self.config().project.name.clone()),
            name: name.to_owned(),
            spec_kind: ScheduleSpecKind::Interval,
            spec: spec.to_owned(),
            tz: "UTC".to_owned(),
            action,
            missed_policy: missed,
            state: ActiveState::Active,
            last_run_at: None,
            next_run_at,
            last_status: String::new(),
            created_at: self.store.clock().now(),
        };
        self.store.insert_schedule(&schedule).expect("schedule");
        schedule
    }

    fn job_template(&self, id: &str) -> ScheduleAction {
        ScheduleAction::JobTemplate {
            template: NewJob {
                id: JobId::parse(id).expect("job id"),
                title: format!("scheduled {id}"),
                kind: JobKind::Build,
                role: Role::Worker,
                profile: None,
                needs: Vec::new(),
                owns: vec!["crates/vowp-core/**".to_owned()],
                reads: Vec::new(),
                gates: None,
                priority: None,
                loop_: None,
                brief_text: None,
            },
        }
    }

    fn jobs(&self) -> Vec<Job> {
        self.store
            .list_jobs(&vowp_store::JobFilter {
                project: Some(self.config().project.name.clone()),
                ..vowp_store::JobFilter::default()
            })
            .expect("jobs")
    }

    fn status(&self, name: &str) -> String {
        self.store
            .schedule(name)
            .expect("schedule lookup")
            .expect("schedule")
            .last_status
    }
}

/// Parses a timestamp, failing the test on bad input.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

/// A runner that records the schedule row as the action sees it.
struct SnapshotRunner {
    paths: NodePaths,
    name: String,
    seen: Arc<Mutex<Option<Schedule>>>,
}

impl CommandRunner for SnapshotRunner {
    fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, CoreError> {
        let store = Store::open(&self.paths)?;
        let schedule = store
            .schedule(&self.name)?
            .ok_or_else(|| CoreError::NotFound(format!("schedule {}", self.name)))?;
        *self.seen.lock().expect("lock") = Some(schedule);
        Ok(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            output: b"checked".to_vec(),
            duration: Duration::from_millis(1),
        })
    }
}

#[test]
fn tick_fires_due_and_advances_next_before_fire() {
    let seen = Arc::new(Mutex::new(None));
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let runner = Arc::new(SnapshotRunner {
        paths: fixture.paths.clone(),
        name: "probe".to_owned(),
        seen: seen.clone(),
    });
    let deps = {
        let mut deps = fixture.deps.clone();
        deps.cmd = runner;
        deps
    };
    let engine = ScheduleEngine::new(deps);
    let now = fixture.clock.now();

    fixture.schedule(
        "probe",
        "every 30m",
        ScheduleAction::Command {
            argv: vec!["sh".to_owned(), "-c".to_owned(), "printf hello".to_owned()],
            cwd: None,
            timeout_secs: 5,
        },
        MissedPolicy::RunOnce,
        Some(timestamp("2026-11-09T10:00:00Z")),
    );

    let due = engine.tick(&fixture.store, now).expect("tick");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].due_at, timestamp("2026-11-09T10:00:00Z"));
    let advanced = fixture.store.schedule("probe").expect("lookup").expect("schedule");
    assert_eq!(advanced.last_run_at, Some(now));
    assert_eq!(advanced.next_run_at, Some(timestamp("2026-11-09T10:30:00Z")));

    let outcome = engine.fire(&fixture.store, &due[0]).expect("fire");
    assert_eq!(outcome, FireOutcome::CommandRan { exit_code: Some(0) });
    let inside = seen.lock().expect("lock").clone().expect("command saw the schedule");
    assert_eq!(
        inside.next_run_at,
        Some(timestamp("2026-11-09T10:30:00Z")),
        "the next run must be advanced before the action runs"
    );
    assert_eq!(fixture.status("probe"), "ok");

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Schedule),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(
        rows[0].detail["key"],
        format!("{}@2026-11-09T10:00:00.000Z", due[0].schedule.id)
    );
}

#[test]
fn double_tick_same_instant_fires_once() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = fixture.clock.now();
    let schedule = fixture.schedule(
        "nightly",
        "every 30m",
        fixture.job_template("nightly"),
        MissedPolicy::RunOnce,
        Some(now),
    );

    let first = fixture.engine.tick(&fixture.store, now).expect("tick");
    assert_eq!(first.len(), 1);
    fixture.engine.fire(&fixture.store, &first[0]).expect("fire");
    assert_eq!(fixture.jobs().len(), 1);

    let second = fixture.engine.tick(&fixture.store, now).expect("second tick");
    assert!(second.is_empty(), "the same instant must not fire twice");
    assert_eq!(fixture.jobs().len(), 1);

    // A crash before the schedule row advanced leaves the key in the ledger; re-arming the
    // row must still not fire again.
    fixture
        .store
        .record_schedule_run(&schedule.id, now, Some(now), "due")
        .expect("re-arm");
    let third = fixture.engine.tick(&fixture.store, now).expect("third tick");
    assert!(third.is_empty(), "the ledger key must suppress a double fire");
    assert_eq!(fixture.jobs().len(), 1);
}

#[test]
fn catch_up_run_once_after_three_missed_fires_once() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = timestamp("2026-11-09T13:00:00Z");
    fixture.clock.set(now);
    fixture.schedule(
        "nightly",
        "every 30m",
        fixture.job_template("nightly"),
        MissedPolicy::RunOnce,
        Some(timestamp("2026-11-09T10:00:00Z")),
    );

    let due = fixture.engine.catch_up(&fixture.store, now).expect("catch up");
    assert_eq!(due.len(), 1, "run_once fires exactly once for the whole gap");
    assert_eq!(due[0].due_at, timestamp("2026-11-09T10:00:00Z"));
    let rearmed = fixture.store.schedule("nightly").expect("lookup").expect("schedule");
    assert_eq!(rearmed.last_run_at, Some(now));
    assert_eq!(rearmed.next_run_at, Some(timestamp("2026-11-09T13:30:00Z")));
    assert!(rearmed.next_run_at.expect("next") > now);

    fixture.engine.fire(&fixture.store, &due[0]).expect("fire");
    assert_eq!(
        fixture.jobs().len(),
        1,
        "six missed occurrences must not create six jobs"
    );

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Schedule),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let key = format!("{}@2026-11-09T10:00:00.000Z", rearmed.id);
    assert!(
        rows.iter().any(|row| row.detail["key"] == key),
        "idempotency key must be recorded"
    );
}

#[test]
fn catch_up_skip_fires_none_but_rearms() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = timestamp("2026-11-09T13:00:00Z");
    fixture.clock.set(now);
    fixture.schedule(
        "nightly",
        "every 30m",
        fixture.job_template("nightly"),
        MissedPolicy::Skip,
        Some(timestamp("2026-11-09T10:00:00Z")),
    );

    let due = fixture.engine.catch_up(&fixture.store, now).expect("catch up");
    assert!(due.is_empty(), "skip must not fire");
    let rearmed = fixture.store.schedule("nightly").expect("lookup").expect("schedule");
    assert_eq!(rearmed.last_run_at, Some(now));
    assert_eq!(rearmed.next_run_at, Some(timestamp("2026-11-09T13:30:00Z")));
    assert_eq!(rearmed.last_status, "skipped missed run");
    assert!(fixture.jobs().is_empty());

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Schedule),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary, "skipped missed run");
    assert_eq!(rows[0].detail["skipped"], true);
}

#[test]
fn job_template_creates_job_with_suffix() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let due_at = timestamp("2026-11-09T07:00:00Z");
    fixture.schedule(
        "nightly",
        "every 1d",
        fixture.job_template("nightly"),
        MissedPolicy::RunOnce,
        Some(due_at),
    );
    let schedule = fixture.store.schedule("nightly").expect("lookup").expect("schedule");
    let due = DueRun {
        schedule: schedule.clone(),
        due_at,
    };

    let outcome = fixture.engine.fire(&fixture.store, &due).expect("fire");
    let job_id = JobId::parse("nightly-202611090700").expect("job id");
    assert_eq!(outcome, FireOutcome::JobCreated(job_id.clone()));
    let job = fixture.store.job(&job_id).expect("lookup").expect("job");
    assert_eq!(job.id, job_id);
    assert_eq!(job.created_by, format!("schedule:{}", schedule.id));
    assert_eq!(job.project, fixture.config().project.name);

    let again = fixture.engine.fire(&fixture.store, &due).expect("second fire");
    assert_eq!(again, FireOutcome::Skipped("already fired".to_owned()));
    assert_eq!(fixture.jobs().len(), 1);
}

#[test]
fn command_action_runs_with_timeout() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = fixture.clock.now();
    fixture.schedule(
        "hello",
        "every 1h",
        ScheduleAction::Command {
            argv: vec!["sh".to_owned(), "-c".to_owned(), "printf hello".to_owned()],
            cwd: None,
            timeout_secs: 5,
        },
        MissedPolicy::RunOnce,
        Some(now),
    );
    let schedule = fixture.store.schedule("hello").expect("lookup").expect("schedule");
    let outcome = fixture
        .engine
        .fire(
            &fixture.store,
            &DueRun {
                schedule: schedule.clone(),
                due_at: now,
            },
        )
        .expect("fire");
    assert_eq!(outcome, FireOutcome::CommandRan { exit_code: Some(0) });
    assert_eq!(fixture.status("hello"), "ok");

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Schedule),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    let result = rows
        .iter()
        .find(|row| row.detail["result"] == true)
        .expect("command result row");
    assert!(
        result.detail["tail"].as_str().expect("tail").contains("hello"),
        "tail must capture stdout: {result:?}"
    );

    fixture.clock.set(now.plus(Duration::from_secs(3600)));
    fixture.schedule(
        "slow",
        "every 1h",
        ScheduleAction::Command {
            argv: vec!["sh".to_owned(), "-c".to_owned(), "sleep 5".to_owned()],
            cwd: None,
            timeout_secs: 1,
        },
        MissedPolicy::RunOnce,
        Some(now),
    );
    let slow = fixture.store.schedule("slow").expect("lookup").expect("schedule");
    let timed_out = fixture
        .engine
        .fire(
            &fixture.store,
            &DueRun {
                schedule: slow,
                due_at: now,
            },
        )
        .expect("timeout fire");
    assert_eq!(timed_out, FireOutcome::CommandRan { exit_code: None });
    assert_eq!(fixture.status("slow"), "failed: timeout");
}

#[test]
fn paused_schedule_never_fires() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = fixture.clock.now();
    let schedule = fixture.schedule(
        "paused",
        "every 30m",
        fixture.job_template("paused"),
        MissedPolicy::RunOnce,
        Some(now),
    );
    fixture
        .store
        .set_schedule_state(&schedule.id, ActiveState::Paused)
        .expect("pause");

    assert!(fixture.engine.tick(&fixture.store, now).expect("tick").is_empty());
    assert!(
        fixture
            .engine
            .catch_up(&fixture.store, now.plus(Duration::from_secs(3600)))
            .expect("catch up")
            .is_empty()
    );
    let paused = fixture.store.schedule("paused").expect("lookup").expect("schedule");
    assert_eq!(paused.next_run_at, Some(now));
    assert_eq!(paused.last_run_at, None);
    assert!(fixture.jobs().is_empty());

    fixture
        .store
        .set_schedule_state(&schedule.id, ActiveState::Active)
        .expect("resume");
    assert_eq!(fixture.engine.tick(&fixture.store, now).expect("tick").len(), 1);
}

#[test]
fn tick_arms_schedule_without_next_run() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = fixture.clock.now();
    fixture.schedule(
        "unarmed",
        "every 30m",
        fixture.job_template("unarmed"),
        MissedPolicy::RunOnce,
        None,
    );
    assert!(fixture.engine.tick(&fixture.store, now).expect("tick").is_empty());
    let armed = fixture.store.schedule("unarmed").expect("lookup").expect("schedule");
    assert_eq!(armed.next_run_at, Some(timestamp("2026-11-09T10:30:00Z")));
    assert_eq!(armed.last_status, "");
}

#[test]
fn loop_action_pushes_run_loop_request() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = fixture.clock.now();
    fixture.schedule(
        "planner",
        "every 30m",
        ScheduleAction::Loop {
            loop_name: "l4-planner".to_owned(),
        },
        MissedPolicy::RunOnce,
        Some(now),
    );
    let schedule = fixture.store.schedule("planner").expect("lookup").expect("schedule");
    let outcome = fixture
        .engine
        .fire(&fixture.store, &DueRun { schedule, due_at: now })
        .expect("fire");
    assert_eq!(outcome, FireOutcome::LoopTriggered("l4-planner".to_owned()));
    let requests = fixture.store.take_requests().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].kind, "run_loop");
    assert_eq!(requests[0].payload["name"], "l4-planner");
}

#[test]
fn sync_from_config_upserts_by_name() {
    let mut config = VowpConfig::with_defaults_for_tests();
    config.schedules = vec![vowp_protocol::ScheduleConfig {
        name: "morning".to_owned(),
        cron: Some("0 7 * * *".to_owned()),
        every: None,
        tz: Some("+02:00".to_owned()),
        missed: Some(MissedPolicy::Skip),
        action: ScheduleAction::Loop {
            loop_name: "l4-planner".to_owned(),
        },
    }];
    let fixture = Fixture::new(config.clone());
    let now = fixture.clock.now();

    fixture.engine.sync_from_config(&fixture.store, &config).expect("sync");
    let rows = fixture.store.list_schedules().expect("schedules");
    assert_eq!(rows.len(), 1);
    let first = rows[0].clone();
    assert_eq!(first.name, "morning");
    assert_eq!(first.spec_kind, ScheduleSpecKind::Cron);
    assert_eq!(first.tz, "+02:00");
    assert_eq!(first.missed_policy, MissedPolicy::Skip);
    assert_eq!(first.next_run_at, Some(timestamp("2026-11-10T05:00:00Z")));

    fixture
        .store
        .set_schedule_state(&first.id, ActiveState::Paused)
        .expect("pause");
    fixture
        .engine
        .sync_from_config(&fixture.store, &config)
        .expect("second sync");
    let rows = fixture.store.list_schedules().expect("schedules");
    assert_eq!(rows.len(), 1, "sync must not duplicate by name");
    assert_eq!(rows[0].id, first.id);
    assert_eq!(rows[0].state, ActiveState::Paused, "user state survives a sync");

    config.schedules[0].action = ScheduleAction::Loop {
        loop_name: "l5-keeper".to_owned(),
    };
    let engine = ScheduleEngine::new(fixture.dep(config.clone()));
    engine.sync_from_config(&fixture.store, &config).expect("changed sync");
    let updated = fixture.store.schedule("morning").expect("lookup").expect("schedule");
    assert_eq!(updated.id, first.id);
    assert_eq!(
        updated.action,
        ScheduleAction::Loop {
            loop_name: "l5-keeper".to_owned()
        }
    );
    assert_eq!(updated.state, ActiveState::Paused);
    let _ = now;
}
