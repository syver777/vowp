//! Classification, reset-time, redaction and HTTP-status tests.
//! Spec: `docs/design/crates/vowp-adapters.md` §7 and `docs/design/adapters.md` §5.

use std::path::Path;

use vowp_adapters::classify::{
    QUOTA_FALLBACK_MIN, TRANSIENT_BACKOFF_MIN, classify_text, extract_http_status, parse_reset_time, redact,
    reset_from_rate_limit_info,
};
use vowp_protocol::{LimitKind, Timestamp};

fn now() -> Timestamp {
    Timestamp::parse("2026-03-10T18:10:00Z").expect("test timestamp is valid")
}

fn ts(value: &str) -> Timestamp {
    Timestamp::parse(value).expect("test timestamp is valid")
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Returns the `rate_limit_info` object of the first `rate_limit_event` in a fixture.
fn rate_limit_info_from_fixture(name: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(fixture_path(name)).expect("fixture is readable");
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("fixture line is JSON");
        if value.get("type").and_then(serde_json::Value::as_str) == Some("rate_limit_event") {
            return value
                .get("rate_limit_info")
                .cloned()
                .expect("rate_limit_event carries rate_limit_info");
        }
    }
    panic!("fixture {name} has no rate_limit_event");
}

#[test]
fn classify_claude_usage_limit_with_unix_suffix() {
    let classified = classify_text("claude", "Claude AI usage limit reached|1773169200", None, now(), 0)
        .expect("usage limit text classifies");
    assert_eq!(classified.kind, LimitKind::Quota);
    assert_eq!(classified.resets_at, Some(ts("2026-03-10T19:00:00Z")));
    assert!(classified.message.contains("claude"));
    assert!(classified.message.len() <= 300);
}

#[test]
fn classify_claude_hit_limit_clock_time_with_tz_offset() {
    let classified = classify_text(
        "claude",
        "You've hit your limit · resets 11pm (Europe/Oslo)",
        None,
        now(),
        60,
    )
    .expect("hit-limit text classifies");
    assert_eq!(classified.kind, LimitKind::Quota);
    assert_eq!(classified.resets_at, Some(ts("2026-03-10T22:00:00Z")));
}

#[test]
fn classify_clock_time_rolls_to_next_day() {
    let classified =
        classify_text("claude", "quota reached, resets 3pm", None, now(), 60).expect("quota text classifies");
    assert_eq!(classified.kind, LimitKind::Quota);
    assert_eq!(classified.resets_at, Some(ts("2026-03-11T14:00:00Z")));
}

#[test]
fn classify_five_hour_and_weekly_and_minutes() {
    let five_hour = classify_text("claude", "5-hour limit reached ∙ resets 3pm", None, now(), 60)
        .expect("five-hour text classifies");
    assert_eq!(five_hour.kind, LimitKind::Quota);
    assert_eq!(five_hour.resets_at, Some(ts("2026-03-11T14:00:00Z")));

    let weekly = classify_text(
        "claude",
        "You've hit your weekly limit · resets Mar 15, 9am",
        None,
        now(),
        60,
    )
    .expect("weekly text classifies");
    assert_eq!(weekly.kind, LimitKind::Quota);
    assert_eq!(weekly.resets_at, Some(ts("2026-03-15T08:00:00Z")));

    let minutes = classify_text("claude", "Session limit reached ∙ resets 4:30 am", None, now(), 60)
        .expect("minute text classifies");
    assert_eq!(minutes.kind, LimitKind::Quota);
    assert_eq!(minutes.resets_at, Some(ts("2026-03-11T03:30:00Z")));
}

#[test]
fn classify_rate_529_429_overloaded() {
    let expected = now().plus(std::time::Duration::from_secs(u64::from(TRANSIENT_BACKOFF_MIN) * 60));
    for (text, status) in [
        (
            "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
            None,
        ),
        ("API Error: 529 Overloaded", None),
        ("overloaded_error", None),
        ("too many requests", None),
        ("", Some(429)),
        ("", Some(529)),
    ] {
        let classified =
            classify_text("claude", text, status, now(), 0).unwrap_or_else(|| panic!("{text:?} should classify"));
        assert_eq!(classified.kind, LimitKind::Rate, "text {text:?}");
        assert_eq!(classified.resets_at, Some(expected), "text {text:?}");
    }
}

#[test]
fn classify_credits_402_insufficient_balance() {
    let balance =
        classify_text("example", "Insufficient Balance", Some(402), now(), 0).expect("insufficient balance classifies");
    assert_eq!(balance.kind, LimitKind::Credits);
    assert_eq!(balance.resets_at, None);
    assert_eq!(balance.http_status, Some(402));

    let low = classify_text(
        "claude",
        "Your credit balance is too low to access the Anthropic API",
        None,
        now(),
        0,
    )
    .expect("credit balance text classifies");
    assert_eq!(low.kind, LimitKind::Credits);
    assert_eq!(low.resets_at, None);

    let out = classify_text("claude", "You are out of credits", None, now(), 0).expect("out of credits classifies");
    assert_eq!(out.kind, LimitKind::Credits);

    // Credits beats both rate and quota when several classes match.
    let both =
        classify_text("claude", "402 rate limit quota exceeded", None, now(), 0).expect("multi-class text classifies");
    assert_eq!(both.kind, LimitKind::Credits);
}

#[test]
fn classify_auth_texts() {
    for (text, status) in [
        ("Invalid API key · Please run /login", None),
        ("Not logged in", None),
        ("OAuth token has expired", None),
        ("something else entirely", Some(401)),
        ("something else entirely", Some(403)),
    ] {
        let classified =
            classify_text("claude", text, status, now(), 0).unwrap_or_else(|| panic!("{text:?} should classify"));
        assert_eq!(classified.kind, LimitKind::Auth, "text {text:?}");
        assert_eq!(classified.resets_at, None, "text {text:?}");
    }
}

#[test]
fn classify_context_texts() {
    for text in [
        "prompt is too long: 210000 tokens > 200000 maximum",
        "maximum context length exceeded",
        "context_length_exceeded",
        "input is too long",
        "exceeds the context window",
    ] {
        let classified = classify_text("", text, None, now(), 0).unwrap_or_else(|| panic!("{text:?} should classify"));
        assert_eq!(classified.kind, LimitKind::Context, "text {text:?}");
        assert_eq!(classified.resets_at, None);
    }
}

#[test]
fn classify_precedence_credits_auth_quota_rate_context() {
    let cases: &[(&str, Option<u16>, LimitKind)] = &[
        (
            "insufficient_balance 401 rate limit resets 5pm",
            None,
            LimitKind::Credits,
        ),
        ("payment required unauthorized", None, LimitKind::Credits),
        ("quota exceeded", Some(402), LimitKind::Credits),
        ("unauthorized quota exceeded", None, LimitKind::Auth),
        ("quota exceeded", Some(401), LimitKind::Auth),
        ("rate_limit_error resets 5pm", None, LimitKind::Quota),
        ("5-hour limit reached resets 3pm", None, LimitKind::Quota),
        ("429 too many requests", None, LimitKind::Rate),
        ("context window exceeded", Some(429), LimitKind::Rate),
        ("context window exceeded", None, LimitKind::Context),
    ];

    for (text, status, expected) in cases {
        let classified =
            classify_text("claude", text, *status, now(), 0).unwrap_or_else(|| panic!("{text:?} should classify"));
        assert_eq!(classified.kind, *expected, "text {text:?} status {status:?}");
    }
}

#[test]
fn classify_unrelated_text_is_none() {
    assert_eq!(classify_text("", "all tests passed", None, now(), 0), None);
    assert_eq!(classify_text("claude", "the weather is fine", None, now(), 0), None);
}

#[test]
fn reset_from_rate_limit_info_picks_max_rejected_window() {
    let one_rejected = serde_json::json!({
        "status": "rejected",
        "resetsAt": 1773169200,
        "rateLimitType": "five_hour",
        "unifiedWindows": [
            {"type": "five_hour", "status": "rejected", "resetsAt": 1773169200},
            {"type": "seven_day", "status": "allowed", "resetsAt": 1773172800}
        ]
    });
    assert_eq!(
        reset_from_rate_limit_info(&one_rejected),
        Some(ts("2026-03-10T19:00:00Z"))
    );

    let both_rejected = serde_json::json!({
        "status": "rejected",
        "resetsAt": 1773169200,
        "unifiedWindows": [
            {"type": "five_hour", "status": "rejected", "resetsAt": 1773169200},
            {"type": "seven_day", "status": "rejected", "resetsAt": 1773176400}
        ]
    });
    assert_eq!(
        reset_from_rate_limit_info(&both_rejected),
        Some(ts("2026-03-10T21:00:00Z"))
    );

    let top_level_only = serde_json::json!({"status": "rejected", "resetsAt": 1773169200});
    assert_eq!(
        reset_from_rate_limit_info(&top_level_only),
        Some(ts("2026-03-10T19:00:00Z"))
    );

    let allowed = serde_json::json!({
        "status": "allowed",
        "resetsAt": 1773169200,
        "unifiedWindows": [{"type": "five_hour", "status": "allowed", "resetsAt": 1773169200}]
    });
    assert_eq!(reset_from_rate_limit_info(&allowed), None);
}

#[test]
fn reset_from_rate_limit_info_object_shape_real_fixture() {
    let info = rate_limit_info_from_fixture("claude_rate_limit_rejected.jsonl");

    let reset = reset_from_rate_limit_info(&info).expect("rejected rate limit has a reset");
    assert_eq!(reset.unix_secs(), 1773169200);
    assert_eq!(reset, ts("2026-03-10T19:00:00Z"));

    // The real stream uses the object map. With the top-level status ignored, the
    // rejected five_hour window itself must supply the reset (the array-only parser
    // returned None here).
    let mut windows_only = info;
    windows_only["status"] = serde_json::json!("allowed");
    assert_eq!(
        reset_from_rate_limit_info(&windows_only),
        Some(ts("2026-03-10T19:00:00Z"))
    );
}

#[test]
fn reset_from_rate_limit_info_array_shape() {
    let info = serde_json::json!({
        "status": "allowed",
        "resetsAt": 1773565200,
        "unifiedWindows": [
            {"type": "five_hour", "status": "allowed", "resetsAt": 1773565200},
            {"type": "seven_day", "status": "rejected", "resetsAt": 1773169200}
        ]
    });
    assert_eq!(reset_from_rate_limit_info(&info), Some(ts("2026-03-10T19:00:00Z")));
}

#[test]
fn reset_picks_latest_rejected_window() {
    let object = serde_json::json!({
        "status": "rejected",
        "resetsAt": 1773169200,
        "unifiedWindows": {
            "five_hour": {"utilization": 1.0, "status": "rejected", "resetsAt": 1773169200},
            "seven_day": {"utilization": 1.0, "status": "rejected", "resetsAt": 1773176400},
            "overage": {"utilization": 0.4, "status": "allowed", "resetsAt": 1773565200}
        }
    });
    assert_eq!(reset_from_rate_limit_info(&object), Some(ts("2026-03-10T21:00:00Z")));

    // The same rule applies to the array shape, independent of window order.
    let array = serde_json::json!({
        "status": "allowed",
        "unifiedWindows": [
            {"type": "seven_day", "status": "allowed", "resetsAt": 1773565200},
            {"type": "seven_day", "status": "rejected", "resetsAt": 1773176400},
            {"type": "five_hour", "status": "rejected", "resetsAt": 1773169200}
        ]
    });
    assert_eq!(reset_from_rate_limit_info(&array), Some(ts("2026-03-10T21:00:00Z")));
}

#[test]
fn redact_removes_secret_shapes_and_truncates() {
    // Build the synthetic values from fragments so this test file contains no
    // literal secret-shaped token for the repository's secret scanner.
    let secrets = [
        format!("{}{}{}", "s", "k-", "Ab1_-".repeat(8)),
        format!("{}{}{}", "sb_", "secret_", "a".repeat(20)),
        format!("{}{}{}", "gh", "p_", "a".repeat(30)),
        format!("{}{}{}", "github_", "pat_", "a".repeat(30)),
        format!("{}{}{}", "AK", "IA", "A1".repeat(8)),
        format!("{}{}{}{}", "-----BEGIN", " RSA ", "PRIVATE", " KEY-----"),
        format!("{}{}{}", "xox", "b-", "a".repeat(20)),
        format!("{}{}{}", "Bear", "er ", "a".repeat(30)),
        format!("{}{}{}", "api_", "key=", "abcdefgh"),
    ];

    for secret in secrets {
        let output = redact(&format!("before {secret} after"), 300);
        assert!(!output.contains(&secret), "secret survived: {output}");
        assert!(output.contains("<redacted>"), "no marker for secret: {output}");
        assert!(output.chars().count() <= 300);
    }

    let long = redact(&"x".repeat(1000), 300);
    assert_eq!(long.chars().count(), 300);
    assert!(long.ends_with('…'));

    let multibyte = redact(&"é".repeat(1000), 300);
    assert_eq!(multibyte.chars().count(), 300);
    assert!(multibyte.ends_with('…'));

    assert_eq!(redact("anything", 0), "");
}

#[test]
fn redact_collapses_whitespace() {
    assert_eq!(redact("  one\n\n two\tthree  ", 300), "one two three");
    let assignment = format!("a\n{}{}abcdefgh\nb", "sec", "ret=");
    assert_eq!(redact(&assignment, 300), "a <redacted> b");
}

#[test]
fn extract_http_status_variants() {
    assert_eq!(extract_http_status("\"statusCode\":402"), Some(402));
    assert_eq!(extract_http_status("\"status\": 429"), Some(429));
    assert_eq!(extract_http_status("HTTP 401"), Some(401));
    assert_eq!(extract_http_status("API Error: 529"), Some(529));
    assert_eq!(extract_http_status("HTTP 200 OK"), Some(200));
    assert_eq!(extract_http_status("\"statusCode\":999"), None);
    assert_eq!(extract_http_status("no status here"), None);
}

#[test]
fn parse_reset_time_unix_suffix_is_exact() {
    assert_eq!(
        parse_reset_time("Claude AI usage limit reached|1773169200", now(), 60),
        Some(ts("2026-03-10T19:00:00Z"))
    );
}

#[test]
fn parse_reset_time_clock_uses_tz_offset() {
    assert_eq!(
        parse_reset_time("resets 11pm (Europe/Oslo)", now(), 60),
        Some(ts("2026-03-10T22:00:00Z"))
    );
    assert_eq!(
        parse_reset_time("resets at 11pm", now(), 0),
        Some(ts("2026-03-10T23:00:00Z"))
    );
}

#[test]
fn parse_reset_time_month_day_and_year_rollover() {
    assert_eq!(
        parse_reset_time("resets Dec 31, 11pm", now(), 60),
        Some(ts("2026-12-31T22:00:00Z"))
    );
    assert_eq!(
        parse_reset_time("resets Feb 1, 9am", now(), 60),
        Some(ts("2027-02-01T08:00:00Z"))
    );
}

#[test]
fn parse_reset_time_rejects_invalid_values() {
    assert_eq!(parse_reset_time("resets 25pm", now(), 0), None);
    assert_eq!(parse_reset_time("resets 12:75", now(), 0), None);
    assert_eq!(parse_reset_time("nothing to see here", now(), 0), None);
}

#[test]
fn classify_constants_match_the_spec() {
    assert_eq!(TRANSIENT_BACKOFF_MIN, 10);
    assert_eq!(QUOTA_FALLBACK_MIN, 60);
}
