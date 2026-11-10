//! `vowp cost` integration tests.

mod common;

use vowp_protocol::{Cost, CostSource, JobId, LedgerEntry, LedgerKind, Tokens};

/// Appends one cost ledger row.
fn cost_row(env: &common::TestEnv, job: &str, cost: Cost) {
    let store = env.store();
    let job_id = JobId::parse(job).expect("job id");
    store
        .append_ledger(&LedgerEntry {
            id: None,
            ts: store.clock().now(),
            kind: LedgerKind::Cost,
            project: Some(env.config.project.name.clone()),
            job_id: Some(job_id),
            run_id: None,
            session_id: None,
            role: None,
            provider: Some("deepseek".to_owned()),
            harness: None,
            model: Some("deepseek/deepseek-flash".to_owned()),
            cost: Some(cost),
            summary: "run cost".to_owned(),
            detail: serde_json::json!({}),
        })
        .expect("append cost");
}

#[test]
fn cost_by_job_marks_unknown_not_zero() {
    let env = common::TestEnv::new();
    cost_row(
        &env,
        "job-unknown",
        Cost {
            usd: None,
            tokens: Tokens {
                input: 12,
                output: 3,
                ..Tokens::default()
            },
            source: CostSource::Unknown,
        },
    );
    cost_row(
        &env,
        "job-known",
        Cost {
            usd: Some(0.5),
            tokens: Tokens {
                input: 100,
                output: 20,
                cache_read: 5,
                ..Tokens::default()
            },
            source: CostSource::Receipt,
        },
    );

    let (code, value, stderr) = env.run_json(&["cost", "--by", "job"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(value["by"], "job");
    let rows = value["rows"].as_array().expect("rows");
    let unknown = rows
        .iter()
        .find(|row| row["key"] == "job-unknown")
        .expect("unknown row");
    assert!(unknown["usd"].is_null(), "unknown cost must not be zero: {unknown}");
    assert_eq!(unknown["unknown_entries"], 1);
    assert_eq!(unknown["source"], "unknown");
    assert_eq!(unknown["tokens"]["input"], 12);

    let known = rows.iter().find(|row| row["key"] == "job-known").expect("known row");
    assert_eq!(known["usd"], 0.5);
    assert_eq!(known["source"], "receipt");

    let total = &value["total"];
    assert_eq!(total["usd"], 0.5);
    assert_eq!(total["unknown_entries"], 1);
    assert_eq!(total["source"], "mixed");
}

#[test]
fn cost_since_and_source_derivation() {
    let env = common::TestEnv::new();
    cost_row(
        &env,
        "job-price",
        Cost {
            usd: Some(0.25),
            tokens: Tokens::default(),
            source: CostSource::PriceTable,
        },
    );
    let (code, value, stderr) = env.run_json(&["cost", "--by", "job", "--since", "7d"]);
    assert_eq!(code, 0, "{stderr}");
    let row = value["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["key"] == "job-price")
        .expect("row");
    assert_eq!(row["source"], "price-table");
    assert_eq!(value["total"]["source"], "price-table");

    let (code, _, _) = env.run(&["cost", "--since", "not-a-date"]);
    assert_eq!(code, 2);
}
