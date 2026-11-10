//! Board snapshot and night-report tests.
//! Spec: `docs/design/crates/vowp-core.md` §5.

mod support;

use std::sync::Arc;

use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::ledger::{BoardSnapshot, report};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    Cost, CostSource, Executor, FakeClock, HarnessKind, InboxItem, InboxKind, InboxState, Job, JobId, JobKind,
    JobState, LedgerEntry, LedgerKind, Limit, LimitRowKind, NewJob, NodeName, Project, Role, Run, RunId, RunPurpose,
    RunRole, Timestamp, Tokens, VowpConfig, Worker, WorkerKind, WorkerState,
};
use vowp_store::{NodePaths, StateChange, Store};
use vowp_tmux::fake::FakeWindows;

/// A hermetic node with a file-backed store and injected clock.
struct Fixture {
    _tmp: TempDir,
    deps: Deps,
    store: Store,
    clock: FakeClock,
}

impl Fixture {
    fn new(config: VowpConfig) -> Self {
        let tmp = tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(timestamp("2026-11-08T19:00:00Z"));
        let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock.clone()));
        let deps = Deps {
            paths,
            config: Arc::new(config.clone()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock.clone()),
            log: Arc::new(vowp_protocol::MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(FakeGit::new()),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
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
            deps,
            store,
            clock,
        }
    }

    fn config(&self) -> &VowpConfig {
        &self.deps.config
    }

    fn job(&self, id: &str) -> Job {
        let new = NewJob {
            id: JobId::parse(id).expect("job id"),
            title: format!("job {id}"),
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
        };
        self.store
            .insert_job(&self.config().project.name, &new, "test")
            .expect("insert job")
    }

    fn set_state(&self, job: &Job, state: JobState, note: Option<&str>) {
        self.store
            .set_job_state(
                &job.id,
                state,
                &StateChange {
                    note: note.map(str::to_owned),
                    parked_reason: None,
                },
            )
            .expect("state");
    }

    fn cost(&self, provider: &str, cost: Cost, at: &str) {
        let entry = LedgerEntry {
            id: None,
            ts: timestamp(at),
            kind: LedgerKind::Cost,
            project: Some(self.config().project.name.clone()),
            job_id: None,
            run_id: None,
            session_id: None,
            role: Some(RunRole::Worker),
            provider: Some(provider.to_owned()),
            harness: Some(HarnessKind::Opencode),
            model: Some("deepseek-flash".to_owned()),
            cost: Some(cost),
            summary: format!("cost {provider}"),
            detail: serde_json::json!({}),
        };
        self.store.append_ledger(&entry).expect("cost row");
    }
}

/// Parses a timestamp, failing the test on bad input.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

/// Builds a token count.
fn tokens(input: u64, output: u64) -> Tokens {
    Tokens {
        input,
        output,
        ..Tokens::default()
    }
}

#[test]
fn board_snapshot_counts_and_render_golden() {
    let project_dir = tempdir().expect("project dir");
    checkpoints_dir(project_dir.path());
    std::fs::write(
        project_dir
            .path()
            .join(".vowp")
            .join("checkpoints")
            .join("2026-11-08T17:00:00.000Z.md"),
        "# old checkpoint\n",
    )
    .expect("checkpoint file");

    let mut config = VowpConfig::with_defaults_for_tests();
    config.project.path = Some(project_dir.path().display().to_string());
    let fixture = Fixture::new(config);
    let now = fixture.store.clock().now();

    let done = fixture.job("n01-done");
    fixture.set_state(&done, JobState::Done, None);
    let ready = fixture.job("n02-ready");
    fixture.set_state(&ready, JobState::Ready, None);
    let running = fixture.job("n03-run");
    fixture.set_state(&running, JobState::Running, None);

    let run = Run {
        id: RunId::new(),
        job_id: Some(running.id.clone()),
        session_id: None,
        role: RunRole::Worker,
        purpose: RunPurpose::Start,
        profile: None,
        rung: 0,
        executor: Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        provider: Some("deepseek".to_owned()),
        node: NodeName::parse("local").expect("node"),
        native_session_id: None,
        pid: None,
        window: None,
        cwd: None,
        started_at: timestamp("2026-11-08T18:58:00Z"),
        ended_at: None,
        exit_code: None,
        outcome: None,
        limit_kind: None,
        cost: Cost::unknown(),
        detail: String::new(),
    };
    fixture.store.insert_run(&run).expect("run");

    fixture
        .store
        .upsert_worker(&Worker {
            node: NodeName::parse("local").expect("node"),
            slot: 1,
            kind: WorkerKind::Worker,
            state: WorkerState::Busy,
            run_id: Some(run.id.clone()),
            job_id: Some(running.id.clone()),
            window: None,
            updated_at: now,
        })
        .expect("busy worker");
    fixture
        .store
        .upsert_worker(&Worker {
            node: NodeName::parse("local").expect("node"),
            slot: 2,
            kind: WorkerKind::Worker,
            state: WorkerState::Idle,
            run_id: None,
            job_id: None,
            window: None,
            updated_at: now,
        })
        .expect("idle worker");

    fixture
        .store
        .upsert_limit(&Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: LimitRowKind::Quota,
            resets_at: Some(timestamp("2026-11-08T21:00:00Z")),
            detail: "quota window".to_owned(),
            since: timestamp("2026-11-08T18:00:00Z"),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");

    fixture
        .store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.config().project.name.clone()),
            job_id: Some(running.id.clone()),
            kind: InboxKind::Question,
            title: "Which branch?".to_owned(),
            body: String::new(),
            options: vec!["main".to_owned()],
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: now,
            answered_at: None,
        })
        .expect("inbox");

    fixture.cost(
        "claude",
        Cost::receipt(1.25, tokens(1_000, 100)),
        "2026-11-08T18:10:00Z",
    );
    fixture.cost(
        "deepseek",
        Cost {
            usd: Some(0.50),
            tokens: tokens(2_000, 200),
            source: CostSource::PriceTable,
        },
        "2026-11-08T18:20:00Z",
    );

    let snapshot = BoardSnapshot::collect(&fixture.store, fixture.config(), now).expect("snapshot");
    assert_eq!(snapshot.project.as_str(), "test");
    assert_eq!(snapshot.at, now);
    assert_eq!(snapshot.jobs_by_state.get("done"), Some(&1));
    assert_eq!(snapshot.jobs_by_state.get("ready"), Some(&1));
    assert_eq!(snapshot.jobs_by_state.get("running"), Some(&1));
    assert_eq!(snapshot.running.len(), 1);
    assert_eq!(snapshot.running[0].job, running.id);
    assert_eq!(snapshot.running[0].age_secs, 120);
    assert_eq!(snapshot.running[0].last_event_age_secs, None);
    assert_eq!(snapshot.slots_busy, 1);
    assert_eq!(snapshot.slots_total, 6);
    assert_eq!(
        snapshot.planner_executor,
        Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor"))
    );
    assert_eq!(
        snapshot.keeper_executor,
        Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor"))
    );
    assert_eq!(snapshot.limits.len(), 1);
    assert_eq!(snapshot.cost_today.usd, Some(1.75));
    assert_eq!(snapshot.cost_today.source, CostSource::PriceTable);
    assert_eq!(snapshot.inbox_open, 1);
    assert_eq!(snapshot.last_checkpoint, Some(timestamp("2026-11-08T17:00:00Z")));

    let expected = "\
test @ 2026-11-08T19:00:00.000Z
jobs: done 1, ready 1, running 1
running:
  n03-run  opencode:deepseek/deepseek-flash  120s  -
slots: 1/6
planner: opencode:deepseek/deepseek-flash
keeper: opencode:deepseek/deepseek-flash
limits:
  claude quota resets 2026-11-08T21:00:00.000Z
cost today: 1.75 USD
inbox: 1 open
checkpoint: 2026-11-08T17:00:00.000Z
";
    assert_eq!(snapshot.render_text(false), expected);
    let colored = snapshot.render_text(true);
    assert!(colored.contains("\u{1b}[1m"), "header should be bold");
    assert!(colored.contains("\u{1b}[32m"), "running job should be green");
}

#[test]
fn night_report_lists_limits_and_switches_golden() {
    let fixture = Fixture::new(VowpConfig::with_defaults_for_tests());
    let now = timestamp("2026-11-09T06:00:00Z");
    fixture.clock.set(now);

    let built = fixture.job("n01-built");
    fixture.set_state(&built, JobState::Done, None);
    let failed = fixture.job("n02-bad");
    fixture.set_state(&failed, JobState::Failed, Some("compile failed"));
    let running = fixture.job("n03-run");
    fixture.set_state(&running, JobState::Running, None);

    fixture.cost(
        "claude",
        Cost::receipt(1.25, tokens(1_000, 100)),
        "2026-11-09T01:00:00Z",
    );
    fixture.cost(
        "deepseek",
        Cost {
            usd: Some(0.75),
            tokens: tokens(2_000, 200),
            source: CostSource::PriceTable,
        },
        "2026-11-09T02:00:00Z",
    );

    fixture
        .store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: timestamp("2026-11-09T00:30:00Z"),
            kind: LedgerKind::Limit,
            project: Some(fixture.config().project.name.clone()),
            job_id: Some(built.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: Some("claude".to_owned()),
            harness: Some(HarnessKind::ClaudeCode),
            model: None,
            cost: None,
            summary: "claude quota".to_owned(),
            detail: serde_json::json!({"provider": "claude", "kind": "quota"}),
        })
        .expect("limit row");
    fixture
        .store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: timestamp("2026-11-09T05:00:00Z"),
            kind: LedgerKind::Switch,
            project: Some(fixture.config().project.name.clone()),
            job_id: Some(built.id.clone()),
            run_id: None,
            session_id: None,
            role: None,
            provider: None,
            harness: None,
            model: None,
            cost: None,
            summary: "n01-built: claude-code -> opencode".to_owned(),
            detail: serde_json::json!({"reason": "quota"}),
        })
        .expect("switch row");

    fixture
        .store
        .upsert_limit(&Limit {
            provider: "claude".to_owned(),
            harness: Some(HarnessKind::ClaudeCode),
            kind: LimitRowKind::Quota,
            resets_at: Some(timestamp("2026-11-09T09:00:00Z")),
            detail: "quota window".to_owned(),
            since: timestamp("2026-11-09T00:30:00Z"),
            last_probe_at: None,
            probe_fail_streak: 0,
            concurrency_cap: None,
            cap_until: None,
        })
        .expect("limit");

    fixture
        .store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.config().project.name.clone()),
            job_id: Some(built.id.clone()),
            kind: InboxKind::Question,
            title: "Need a decision".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Open,
            answer: None,
            answered_by: None,
            created_at: timestamp("2026-11-09T04:00:00Z"),
            answered_at: None,
        })
        .expect("open inbox");
    fixture
        .store
        .insert_inbox(&InboxItem {
            id: vowp_protocol::InboxId::new(),
            project: Some(fixture.config().project.name.clone()),
            job_id: None,
            kind: InboxKind::Approval,
            title: "Deploy?".to_owned(),
            body: String::new(),
            options: Vec::new(),
            state: InboxState::Answered,
            answer: Some("approve".to_owned()),
            answered_by: Some("owner".to_owned()),
            created_at: timestamp("2026-11-09T03:00:00Z"),
            answered_at: Some(timestamp("2026-11-09T05:30:00Z")),
        })
        .expect("answered inbox");

    let text = report::night_report(&fixture.store, fixture.config(), now).expect("report");
    let expected = "\
# Night report — test

Generated 2026-11-09T06:00:00.000Z (since 2026-11-09T00:00:00.000Z)

## Built
- n01-built: job n01-built

## Failed
- n02-bad: job n02-bad — compile failed

## Cost
- claude: 1.25 USD receipt=1 price-table=0 unknown=0
- deepseek: 0.75 USD receipt=0 price-table=1 unknown=0
Total: 2.00 USD receipt=1 price-table=1 unknown=0

## Limits and failover
- limit events: 1
- failover switches: 1
- claude quota until 2026-11-09T09:00:00.000Z — quota window

## Switches
- 2026-11-09T05:00:00.000Z: n01-built: claude-code -> opencode

## Inbox
Open: 1
- [question] n01-built: Need a decision
Answered: 1
";
    assert_eq!(text, expected);

    let checkpoint = report::checkpoint(&fixture.store, fixture.config(), now).expect("checkpoint");
    let expected_checkpoint = "\
# Checkpoint — test
At 2026-11-09T06:00:00.000Z
done 1 · failed 1 · running 1 · limits 1 · inbox 1 open · cost 2.00 USD receipt=1 price-table=1 unknown=0
";
    assert_eq!(checkpoint, expected_checkpoint);
}

/// Creates `<project>/.vowp/checkpoints`.
fn checkpoints_dir(project: &std::path::Path) {
    std::fs::create_dir_all(project.join(".vowp").join("checkpoints")).expect("checkpoint dir");
}
