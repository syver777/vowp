//! Ledger and cost-aggregation repository tests for `vowp-store`.
//! Spec: `docs/design/crates/vowp-store.md` §6.

use std::sync::Arc;

use rusqlite::Connection;
use tempfile::TempDir;
use vowp_protocol::{
    Clock, Cost, CostSource, FakeClock, HarnessKind, JobId, LedgerEntry, LedgerKind, ProjectName, RunRole, Timestamp,
    Tokens,
};
use vowp_store::{CostGroup, CostQuery, CostRow, LedgerFilter, NodePaths, Store};

const EPOCH: &str = "2026-01-01T00:00:00.000Z";
const DAY_ONE: &str = "2026-01-01T10:00:00.000Z";
const DAY_TWO: &str = "2026-01-02T10:00:00.000Z";

/// A file-backed store with a controllable clock.
struct Fixture {
    _tmp: TempDir,
    paths: NodePaths,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        Self::with_project("proj")
    }

    fn with_project(name: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = NodePaths::new(tmp.path());
        let clock = FakeClock::at(Timestamp::parse(EPOCH).expect("epoch"));
        let store = Store::open(&paths)
            .expect("open store")
            .with_clock(Arc::new(clock.clone()));
        let project = ProjectName::parse(name).expect("project");
        let now = clock.now();
        store
            .upsert_project(&vowp_protocol::Project {
                name: project,
                path: format!("/tmp/{name}"),
                remote_url: None,
                default_branch: "main".to_owned(),
                config: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            })
            .expect("insert project");
        Self {
            _tmp: tmp,
            paths,
            store,
        }
    }
}

/// Builds a cost ledger entry.
fn cost_entry(
    ts: &str,
    project: &str,
    job: &str,
    role: RunRole,
    provider: &str,
    model: &str,
    cost: Cost,
) -> LedgerEntry {
    LedgerEntry {
        id: None,
        ts: Timestamp::parse(ts).expect("timestamp"),
        kind: LedgerKind::Cost,
        project: Some(ProjectName::parse(project).expect("project")),
        job_id: Some(JobId::parse(job).expect("job")),
        run_id: None,
        session_id: None,
        role: Some(role),
        provider: Some(provider.to_owned()),
        harness: Some(HarnessKind::Opencode),
        model: Some(model.to_owned()),
        cost: Some(cost),
        summary: format!("cost {job}"),
        detail: serde_json::json!({}),
    }
}

/// Builds a token count with only input and output set.
fn tokens(input: u64, output: u64) -> Tokens {
    Tokens {
        input,
        output,
        ..Tokens::default()
    }
}

/// Builds a price-table cost.
fn priced(usd: f64, tokens: Tokens) -> Cost {
    Cost {
        usd: Some(usd),
        tokens,
        source: CostSource::PriceTable,
    }
}

/// Builds an unknown cost.
fn unknown(tokens: Tokens) -> Cost {
    Cost {
        usd: None,
        tokens,
        source: CostSource::Unknown,
    }
}

/// Builds one expected cost-total row.
fn expected(key: &str, usd: f64, receipt: u64, price: u64, absent: u64, tokens: Tokens, entries: u64) -> CostRow {
    CostRow {
        key: key.to_owned(),
        usd,
        receipt_entries: receipt,
        price_table_entries: price,
        unknown_entries: absent,
        tokens,
        entries,
    }
}

#[test]
fn ledger_is_append_only() {
    let fixture = Fixture::new();
    let first = cost_entry(
        DAY_ONE,
        "proj",
        "job-a",
        RunRole::Worker,
        "claude",
        "m1",
        Cost::receipt(1.0, tokens(10, 1)),
    );
    let second = cost_entry(
        DAY_TWO,
        "proj",
        "job-b",
        RunRole::Verifier,
        "deepseek",
        "m2",
        priced(2.0, tokens(20, 2)),
    );
    let first_id = fixture.store.append_ledger(&first).expect("append first");
    let second_id = fixture.store.append_ledger(&second).expect("append second");
    assert!(first_id < second_id, "append_ledger returns increasing ids");

    let all = fixture.store.ledger_since(&LedgerFilter::default()).expect("ledger");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, Some(first_id));
    assert_eq!(all[1].id, Some(second_id));
    assert_eq!(all[0].ts, first.ts);
    assert_eq!(all[0].kind, LedgerKind::Cost);
    assert_eq!(all[0].project, first.project);
    assert_eq!(all[0].job_id, first.job_id);
    assert_eq!(all[0].role, Some(RunRole::Worker));
    assert_eq!(all[0].provider.as_deref(), Some("claude"));
    assert_eq!(all[0].harness, Some(HarnessKind::Opencode));
    assert_eq!(all[0].model.as_deref(), Some("m1"));
    assert_eq!(all[0].cost, first.cost);
    assert_eq!(all[0].summary, "cost job-a");
    assert_eq!(all[0].detail, serde_json::json!({}));

    let only_cost = fixture
        .store
        .ledger_since(&LedgerFilter {
            kind: Some(LedgerKind::Cost),
            ..LedgerFilter::default()
        })
        .expect("cost ledger");
    assert_eq!(only_cost.len(), 2);
    let only_job = fixture
        .store
        .ledger_since(&LedgerFilter {
            job_id: Some(JobId::parse("job-a").expect("job id")),
            ..LedgerFilter::default()
        })
        .expect("job ledger");
    assert_eq!(only_job.len(), 1);
    assert_eq!(only_job[0].id, Some(first_id));
    let limited = fixture
        .store
        .ledger_since(&LedgerFilter {
            limit: Some(1),
            ..LedgerFilter::default()
        })
        .expect("limited ledger");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, Some(first_id));

    let project = ProjectName::parse("proj").expect("project");
    let connection = Connection::open(fixture.paths.project_db_path(&project)).expect("raw connection");
    let error = connection
        .execute("UPDATE ledger SET summary = 'tampered' WHERE id = ?1", [first_id])
        .expect_err("UPDATE must be rejected");
    assert!(error.to_string().contains("ledger is append-only"), "got {error}");
    let error = connection
        .execute("DELETE FROM ledger WHERE id = ?1", [first_id])
        .expect_err("DELETE must be rejected");
    assert!(error.to_string().contains("ledger is append-only"), "got {error}");
    assert_eq!(
        fixture
            .store
            .ledger_since(&LedgerFilter::default())
            .expect("ledger after rejected writes")
            .len(),
        2
    );
}

#[test]
fn cost_totals_group_by_each_dimension() {
    let fixture = Fixture::new();
    let entries = [
        cost_entry(
            DAY_ONE,
            "proj",
            "job-a",
            RunRole::Worker,
            "claude",
            "m1",
            Cost::receipt(1.0, tokens(10, 1)),
        ),
        cost_entry(
            DAY_ONE,
            "proj",
            "job-a",
            RunRole::Worker,
            "claude",
            "m2",
            priced(2.0, tokens(20, 2)),
        ),
        cost_entry(
            DAY_ONE,
            "proj",
            "job-b",
            RunRole::Verifier,
            "deepseek",
            "m1",
            unknown(tokens(30, 3)),
        ),
        cost_entry(
            DAY_TWO,
            "proj",
            "job-a",
            RunRole::Worker,
            "claude",
            "m1",
            Cost::receipt(0.5, tokens(40, 4)),
        ),
        cost_entry(
            DAY_TWO,
            "proj",
            "job-b",
            RunRole::Verifier,
            "deepseek",
            "m2",
            priced(0.25, tokens(50, 5)),
        ),
        cost_entry(
            DAY_TWO,
            "proj",
            "job-b",
            RunRole::Verifier,
            "deepseek",
            "m2",
            unknown(tokens(60, 6)),
        ),
    ];
    for entry in &entries {
        fixture.store.append_ledger(entry).expect("append cost");
    }

    let query = CostQuery::default();
    let by_job = fixture.store.cost_totals(&query, CostGroup::Job).expect("job totals");
    assert_eq!(
        by_job,
        vec![
            expected("job-a", 3.5, 2, 1, 0, tokens(70, 7), 3),
            expected("job-b", 0.25, 0, 1, 2, tokens(140, 14), 3),
        ]
    );

    let by_role = fixture.store.cost_totals(&query, CostGroup::Role).expect("role totals");
    assert_eq!(
        by_role,
        vec![
            expected("worker", 3.5, 2, 1, 0, tokens(70, 7), 3),
            expected("verifier", 0.25, 0, 1, 2, tokens(140, 14), 3),
        ]
    );

    let by_provider = fixture
        .store
        .cost_totals(&query, CostGroup::Provider)
        .expect("provider totals");
    assert_eq!(
        by_provider,
        vec![
            expected("claude", 3.5, 2, 1, 0, tokens(70, 7), 3),
            expected("deepseek", 0.25, 0, 1, 2, tokens(140, 14), 3),
        ]
    );

    let by_model = fixture
        .store
        .cost_totals(&query, CostGroup::Model)
        .expect("model totals");
    assert_eq!(
        by_model,
        vec![
            expected("m2", 2.25, 0, 2, 1, tokens(130, 13), 3),
            expected("m1", 1.5, 2, 0, 1, tokens(80, 8), 3),
        ]
    );

    let by_day = fixture.store.cost_totals(&query, CostGroup::Day).expect("day totals");
    assert_eq!(
        by_day,
        vec![
            expected("2026-01-01", 3.0, 1, 1, 1, tokens(60, 6), 3),
            expected("2026-01-02", 0.75, 1, 1, 1, tokens(150, 15), 3),
        ]
    );

    for rows in [&by_job, &by_role, &by_provider, &by_model, &by_day] {
        for row in rows {
            assert_eq!(
                row.entries,
                row.receipt_entries + row.price_table_entries + row.unknown_entries,
                "source counts partition the entries for {}",
                row.key
            );
            assert_eq!(row.tokens.total(), row.tokens.input + row.tokens.output);
        }
    }
}

#[test]
fn cost_totals_respects_filters() {
    let alpha = Fixture::with_project("alpha");
    let beta = Fixture::with_project("beta");
    let alpha_entries = [
        cost_entry(
            DAY_ONE,
            "alpha",
            "job-a",
            RunRole::Worker,
            "claude",
            "m1",
            Cost::receipt(1.0, tokens(10, 1)),
        ),
        cost_entry(
            DAY_TWO,
            "alpha",
            "job-b",
            RunRole::Worker,
            "claude",
            "m1",
            Cost::receipt(2.0, tokens(20, 2)),
        ),
    ];
    let beta_entries = [
        cost_entry(
            DAY_ONE,
            "beta",
            "job-a",
            RunRole::Worker,
            "claude",
            "m1",
            Cost::receipt(4.0, tokens(40, 4)),
        ),
        cost_entry(
            DAY_TWO,
            "beta",
            "job-b",
            RunRole::Worker,
            "claude",
            "m1",
            priced(8.0, tokens(80, 8)),
        ),
    ];
    for entry in &alpha_entries {
        alpha.store.append_ledger(entry).expect("append alpha cost");
    }
    for entry in &beta_entries {
        beta.store.append_ledger(entry).expect("append beta cost");
    }

    let since = Timestamp::parse(DAY_TWO).expect("day two");
    let totals = alpha
        .store
        .cost_totals(
            &CostQuery {
                since: Some(since),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect("since totals");
    assert_eq!(totals, vec![expected("job-b", 2.0, 1, 0, 0, tokens(20, 2), 1)]);

    let until = Timestamp::parse("2026-01-01T23:59:59.999Z").expect("end of day one");
    let totals = alpha
        .store
        .cost_totals(
            &CostQuery {
                until: Some(until),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect("until totals");
    assert_eq!(totals, vec![expected("job-a", 1.0, 1, 0, 0, tokens(10, 1), 1)]);

    let alpha_name = ProjectName::parse("alpha").expect("project");
    let totals = alpha
        .store
        .cost_totals(
            &CostQuery {
                project: Some(alpha_name.clone()),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect("project totals");
    assert_eq!(
        totals,
        vec![
            expected("job-b", 2.0, 1, 0, 0, tokens(20, 2), 1),
            expected("job-a", 1.0, 1, 0, 0, tokens(10, 1), 1),
        ]
    );
    let wrong = alpha
        .store
        .cost_totals(
            &CostQuery {
                project: Some(ProjectName::parse("beta").expect("project")),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect_err("wrong project");
    assert!(
        matches!(wrong, vowp_store::StoreError::WrongProject { .. }),
        "got {wrong:?}"
    );

    let job_a = JobId::parse("job-a").expect("job");
    let totals = alpha
        .store
        .cost_totals(
            &CostQuery {
                job_id: Some(job_a),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect("job totals");
    assert_eq!(totals, vec![expected("job-a", 1.0, 1, 0, 0, tokens(10, 1), 1)]);

    let totals = beta
        .store
        .cost_totals(
            &CostQuery {
                project: Some(ProjectName::parse("beta").expect("project")),
                job_id: Some(JobId::parse("job-b").expect("job")),
                ..CostQuery::default()
            },
            CostGroup::Job,
        )
        .expect("combined totals");
    assert_eq!(totals, vec![expected("job-b", 8.0, 0, 1, 0, tokens(80, 8), 1)]);

    let totals = alpha
        .store
        .cost_totals(
            &CostQuery {
                project: Some(alpha_name),
                ..CostQuery::default()
            },
            CostGroup::Day,
        )
        .expect("project day totals");
    assert_eq!(
        totals,
        vec![
            expected("2026-01-02", 2.0, 1, 0, 0, tokens(20, 2), 1),
            expected("2026-01-01", 1.0, 1, 0, 0, tokens(10, 1), 1),
        ]
    );
}
