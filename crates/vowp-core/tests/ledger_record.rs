//! Ledger record, price-table and cost-total tests.
//! Spec: `docs/design/crates/vowp-core.md` §5.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use support::fake_git::FakeGit;
use tempfile::{TempDir, tempdir};
use vowp_core::ledger::{Ledger, price};
use vowp_core::{AdapterRegistry, Deps, FixedRng, SystemRunner};
use vowp_protocol::{
    Cost, CostSource, Executor, FakeClock, HarnessKind, Job, JobId, JobKind, LedgerEntry, LedgerKind, NewJob, NodeName,
    PriceEntry, Project, ProviderConfig, Role, Run, RunId, RunOutcome, RunPurpose, RunRole, RunSummary, StopReason,
    Timestamp, Tokens, VowpConfig,
};
use vowp_store::{CostGroup, CostQuery, LedgerFilter, NodePaths, Store};
use vowp_tmux::fake::FakeWindows;

/// A hermetic node with a file-backed store and injected clock.
struct Fixture {
    _tmp: TempDir,
    deps: Deps,
    store: Store,
    ledger: Ledger,
}

impl Fixture {
    fn new(config: VowpConfig) -> Self {
        let tmp = tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(timestamp("2026-11-08T19:00:00Z"));
        let store = Store::open(&paths).expect("store").with_clock(Arc::new(clock.clone()));
        let deps = Deps {
            paths: paths.clone(),
            config: Arc::new(config.clone()),
            node: NodeName::parse("local").expect("node"),
            clock: Arc::new(clock),
            log: Arc::new(vowp_protocol::MemoryLog::new()),
            adapters: Arc::new(AdapterRegistry::new()),
            windows: Arc::new(FakeWindows::new()),
            git: Arc::new(FakeGit::new()),
            cmd: Arc::new(SystemRunner),
            rng: Arc::new(FixedRng(0)),
        };
        let ledger = Ledger::new(deps.clone());
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
            ledger,
        }
    }

    fn config(&self) -> &VowpConfig {
        &self.deps.config
    }

    fn seed_job(&self, id: &str) -> Job {
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

    fn run(&self, job: &Job, executor: Option<Executor>, role: RunRole) -> Run {
        Run {
            id: RunId::new(),
            job_id: Some(job.id.clone()),
            session_id: None,
            role,
            purpose: RunPurpose::Start,
            profile: None,
            rung: 0,
            executor,
            provider: None,
            node: NodeName::parse("local").expect("node"),
            native_session_id: None,
            pid: None,
            window: None,
            cwd: None,
            started_at: self.store.clock().now(),
            ended_at: None,
            exit_code: None,
            outcome: None,
            limit_kind: None,
            cost: Cost::unknown(),
            detail: String::new(),
        }
    }
}

/// Parses a timestamp, failing the test on bad input.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

/// Builds a summary with the given cost.
fn summary(cost: Cost) -> RunSummary {
    RunSummary {
        exit_code: Some(0),
        outcome: RunOutcome::Ok,
        native_session_id: None,
        cost,
        limit: None,
        result_text: None,
        stop_reason: StopReason::EndTurn,
    }
}

/// Builds a token count.
fn tokens(input: u64, output: u64, reasoning: u64, cache_read: u64, cache_write: u64) -> Tokens {
    Tokens {
        input,
        output,
        reasoning,
        cache_read,
        cache_write,
    }
}

/// Returns a config with a deepseek price entry but no claude entry.
fn priced_config() -> VowpConfig {
    let mut config = VowpConfig::with_defaults_for_tests();
    let mut prices = BTreeMap::new();
    prices.insert(
        "deepseek-flash".to_owned(),
        PriceEntry {
            input_per_m: 0.15,
            output_per_m: 0.60,
            cache_read_per_m: Some(0.003),
            cache_write_per_m: Some(0.30),
        },
    );
    config.providers.insert(
        "deepseek".to_owned(),
        ProviderConfig {
            max_concurrent: 4,
            on_credits_exhausted: Default::default(),
            prices,
        },
    );
    config
}

#[test]
fn record_run_cost_receipt_vs_price_table_vs_unknown() {
    let fixture = Fixture::new(priced_config());
    let job = fixture.seed_job("n01-ledger");
    let ledger = &fixture.ledger;

    let receipt_run = fixture.run(
        &job,
        Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&receipt_run).expect("run");
    let receipt = ledger
        .record_run_cost(
            &fixture.store,
            &receipt_run,
            &summary(Cost::receipt(0.25, tokens(1_000, 100, 0, 0, 0))),
        )
        .expect("receipt cost");
    assert_eq!(receipt.source, CostSource::Receipt);
    assert_eq!(receipt.usd, Some(0.25));

    let priced_run = fixture.run(
        &job,
        Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&priced_run).expect("run");
    let priced = ledger
        .record_run_cost(
            &fixture.store,
            &priced_run,
            &summary(Cost {
                usd: None,
                tokens: tokens(1_000_000, 1_000_000, 0, 0, 0),
                source: CostSource::Unknown,
            }),
        )
        .expect("price-table cost");
    assert_eq!(priced.source, CostSource::PriceTable);
    assert!((priced.usd.expect("usd") - 0.75).abs() < 1e-9, "got {priced:?}");

    let unknown_run = fixture.run(
        &job,
        Some(Executor::parse("cli:local-tool").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&unknown_run).expect("run");
    let unknown = ledger
        .record_run_cost(
            &fixture.store,
            &unknown_run,
            &summary(Cost {
                usd: None,
                tokens: tokens(10, 20, 0, 0, 0),
                source: CostSource::Unknown,
            }),
        )
        .expect("unknown cost");
    assert_eq!(unknown.source, CostSource::Unknown);
    assert_eq!(unknown.usd, None);
    assert_eq!(unknown.tokens.total(), 30);

    let rows = fixture
        .store
        .cost_totals(&CostQuery::default(), CostGroup::Role)
        .expect("totals");
    let worker = rows.iter().find(|row| row.key == "worker").expect("worker row");
    assert_eq!(worker.entries, 3);
    assert_eq!(worker.receipt_entries, 1);
    assert_eq!(worker.price_table_entries, 1);
    assert_eq!(worker.unknown_entries, 1);
    assert!((worker.usd - 1.0).abs() < 1e-9, "known usd only: {worker:?}");
}

#[test]
fn price_estimate_math_per_million() {
    let fixture = Fixture::new(priced_config());
    let executor = Executor::parse("opencode:deepseek/deepseek-flash").expect("executor");
    let cost = price::estimate(
        fixture.config(),
        &executor,
        &tokens(2_000_000, 500_000, 100_000, 1_000_000, 200_000),
    );
    assert_eq!(cost.source, CostSource::PriceTable);
    // 2*0.15 + 0.6*0.60 (output+reasoning) + 1*0.003 + 0.2*0.30 = 0.723
    assert!((cost.usd.expect("usd") - 0.723).abs() < 1e-9, "got {cost:?}");
    assert_eq!(cost.tokens.total(), 3_800_000);

    let claude = Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor");
    let missing = price::estimate(fixture.config(), &claude, &tokens(1_000, 1_000, 0, 0, 0));
    assert_eq!(missing.source, CostSource::Unknown);
    assert_eq!(missing.usd, None);

    let no_tokens = price::estimate(fixture.config(), &executor, &Tokens::default());
    assert_eq!(no_tokens.source, CostSource::Unknown);
    assert_eq!(no_tokens.usd, None);
}

#[test]
fn claude_cost_marked_not_billed() {
    let fixture = Fixture::new(priced_config());
    let job = fixture.seed_job("n02-claude");
    let run = fixture.run(
        &job,
        Some(Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&run).expect("run");
    let cost = fixture
        .ledger
        .record_run_cost(
            &fixture.store,
            &run,
            &summary(Cost::receipt(1.25, tokens(500, 50, 0, 0, 0))),
        )
        .expect("cost");
    assert_eq!(cost.source, CostSource::Receipt);
    assert_eq!(cost.usd, Some(1.25));

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Cost),
            ..LedgerFilter::default()
        })
        .expect("ledger");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].run_id.as_ref(), Some(&run.id));
    assert_eq!(rows[0].harness, Some(HarnessKind::ClaudeCode));
    assert_eq!(rows[0].detail, serde_json::json!({"billed": false, "basis": "list"}));
    assert_eq!(rows[0].cost.as_ref().expect("cost").usd, Some(1.25));
}

#[test]
fn ledger_rows_are_append_only_via_store_trigger() {
    let fixture = Fixture::new(priced_config());
    let job = fixture.seed_job("n03-append");
    let first = LedgerEntry {
        id: None,
        ts: fixture.store.clock().now(),
        kind: LedgerKind::Note,
        project: Some(fixture.config().project.name.clone()),
        job_id: Some(job.id.clone()),
        run_id: None,
        session_id: None,
        role: None,
        provider: None,
        harness: None,
        model: None,
        cost: None,
        summary: "first".to_owned(),
        detail: serde_json::json!({"n": 1}),
    };
    let second = LedgerEntry {
        summary: "second".to_owned(),
        detail: serde_json::json!({"n": 2}),
        ..first.clone()
    };
    let first_id = fixture.ledger.record(&fixture.store, first).expect("first row");
    let second_id = fixture.ledger.record(&fixture.store, second).expect("second row");
    assert!(second_id > first_id, "{second_id} should follow {first_id}");

    let rows = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Note),
            ..LedgerFilter::default()
        })
        .expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].summary, "first");
    assert_eq!(rows[0].detail, serde_json::json!({"n": 1}));
    assert_eq!(rows[1].summary, "second");
}

#[test]
fn cost_totals_never_count_unknown_as_zero() {
    let fixture = Fixture::new(priced_config());
    let job = fixture.seed_job("n04-unknown");
    let known_run = fixture.run(
        &job,
        Some(Executor::parse("opencode:deepseek/deepseek-flash").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&known_run).expect("run");
    fixture
        .ledger
        .record_run_cost(
            &fixture.store,
            &known_run,
            &summary(Cost {
                usd: None,
                tokens: tokens(1_000_000, 0, 0, 0, 0),
                source: CostSource::Unknown,
            }),
        )
        .expect("known cost");

    let unknown_run = fixture.run(
        &job,
        Some(Executor::parse("cli:mystery").expect("executor")),
        RunRole::Worker,
    );
    fixture.store.insert_run(&unknown_run).expect("run");
    fixture
        .ledger
        .record_run_cost(&fixture.store, &unknown_run, &summary(Cost::unknown()))
        .expect("unknown cost");

    let rows = fixture
        .store
        .cost_totals(&CostQuery::default(), CostGroup::Job)
        .expect("totals");
    let row = rows.iter().find(|row| row.key == job.id.as_str()).expect("job row");
    assert_eq!(row.entries, 2);
    assert_eq!(row.unknown_entries, 1);
    assert_eq!(row.price_table_entries, 1);
    assert_eq!(row.receipt_entries, 0);
    assert!(
        (row.usd - 0.15).abs() < 1e-9,
        "unknown must not add zero-valued cost: {row:?}"
    );
}
