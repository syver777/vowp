//! Cost, token and limit tests. Spec: `docs/design/crates/vowp-protocol.md` §6.

use pretty_assertions::assert_eq;
use vowp_protocol::prelude::*;

fn tokens(input: u64) -> Tokens {
    Tokens {
        input,
        ..Tokens::default()
    }
}

#[test]
fn cost_add_source_degradation() {
    let mut receipt = Cost::receipt(
        0.5,
        Tokens {
            input: 2,
            output: 4,
            reasoning: 0,
            cache_read: 6,
            cache_write: 4,
        },
    );
    receipt.add(&Cost::receipt(0.25, tokens(1)));
    assert_eq!(receipt.usd, Some(0.75));
    assert_eq!(receipt.source, CostSource::Receipt);
    assert_eq!(receipt.tokens.input, 3);
    assert_eq!(receipt.tokens.output, 4);
    assert_eq!(receipt.tokens.cache_read, 6);

    // Receipt plus unknown: the partial USD amount survives, the source degrades.
    let mut partial = Cost::receipt(0.5, tokens(2));
    partial.add(&Cost::unknown());
    assert_eq!(partial.usd, Some(0.5));
    assert_eq!(partial.source, CostSource::Unknown);
    assert_eq!(partial.tokens.input, 2);

    // Price table plus receipt: the weaker source wins, USD sums.
    let mut estimated = Cost {
        usd: Some(0.25),
        tokens: tokens(3),
        source: CostSource::PriceTable,
    };
    estimated.add(&Cost::receipt(0.5, tokens(4)));
    assert_eq!(estimated.usd, Some(0.75));
    assert_eq!(estimated.source, CostSource::PriceTable);
    assert_eq!(estimated.tokens.input, 7);

    let mut unknown = Cost::unknown();
    unknown.add(&Cost::unknown());
    assert_eq!(unknown.usd, None);
    assert_eq!(unknown.source, CostSource::Unknown);
}

#[test]
fn cost_validate_flags_inconsistent() {
    let bad = Cost {
        usd: Some(1.0),
        tokens: Tokens::default(),
        source: CostSource::Unknown,
    };
    assert!(bad.validate().is_err());

    let bad = Cost {
        usd: None,
        tokens: Tokens::default(),
        source: CostSource::Receipt,
    };
    assert!(bad.validate().is_err());

    let bad = Cost {
        usd: None,
        tokens: Tokens::default(),
        source: CostSource::PriceTable,
    };
    assert!(bad.validate().is_err());

    assert!(Cost::unknown().validate().is_ok());
    assert!(Cost::receipt(0.01, Tokens::default()).validate().is_ok());
    assert!(
        Cost {
            usd: Some(0.1),
            tokens: Tokens::default(),
            source: CostSource::PriceTable,
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn tokens_add_and_total() {
    let mut total = Tokens {
        input: 1,
        output: 2,
        reasoning: 3,
        cache_read: 4,
        cache_write: 5,
    };
    total.add(&Tokens {
        input: 10,
        output: 20,
        reasoning: 30,
        cache_read: 40,
        cache_write: 50,
    });
    assert_eq!(
        total,
        Tokens {
            input: 11,
            output: 22,
            reasoning: 33,
            cache_read: 44,
            cache_write: 55,
        }
    );
    assert_eq!(total.total(), 11 + 22 + 33 + 44 + 55);
    assert_eq!(Tokens::default().total(), 0);
}

#[test]
fn limit_serde_matches_types_example() {
    let json = r#"{"provider": "claude", "harness": "claude-code", "kind": "quota",
        "resets_at": "2026-03-10T22:20:00.000Z", "detail": "five_hour window rejected",
        "since": "2026-03-10T19:41:02.000Z", "last_probe_at": null, "probe_fail_streak": 0,
        "concurrency_cap": null, "cap_until": null}"#;
    let limit: Limit = serde_json::from_str(json).unwrap();
    assert_eq!(limit.provider, "claude");
    assert_eq!(limit.harness, Some(HarnessKind::ClaudeCode));
    assert_eq!(limit.kind, LimitRowKind::Quota);
    assert_eq!(
        limit.resets_at,
        Some(Timestamp::parse("2026-03-10T22:20:00.000Z").unwrap())
    );
    assert_eq!(limit.since, Timestamp::parse("2026-03-10T19:41:02.000Z").unwrap());
    assert_eq!(limit.last_probe_at, None);
    assert_eq!(limit.probe_fail_streak, 0);

    let value = serde_json::to_value(&limit).unwrap();
    assert_eq!(value["kind"], "quota");
    assert_eq!(value["harness"], "claude-code");
    assert_eq!(value["resets_at"], "2026-03-10T22:20:00.000Z");
    assert_eq!(value["last_probe_at"], serde_json::Value::Null);

    assert_eq!(serde_json::to_value(LimitRowKind::Unavailable).unwrap(), "unavailable");
    assert_eq!(serde_json::to_value(LimitKind::Context).unwrap(), "context");
    assert_eq!(serde_json::to_value(LimitKind::Credits).unwrap(), "credits");
}
