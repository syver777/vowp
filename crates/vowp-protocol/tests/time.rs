//! Timestamp, clock and log-line tests. Spec: `docs/design/crates/vowp-protocol.md` §6.

use std::time::Duration;

use pretty_assertions::assert_eq;
use proptest::prelude::*;
use vowp_protocol::prelude::*;

#[test]
fn timestamp_roundtrip_and_ms_truncation() {
    let parsed = Timestamp::parse("2026-03-10T19:25:47.123456Z").unwrap();
    assert_eq!(parsed.as_rfc3339(), "2026-03-10T19:25:47.123Z");

    // Truncation, not rounding.
    let parsed = Timestamp::parse("2026-03-10T19:25:47.999999Z").unwrap();
    assert_eq!(parsed.as_rfc3339(), "2026-03-10T19:25:47.999Z");

    let ms = 1_760_000_000_123_i64;
    assert_eq!(Timestamp::from_unix_millis(ms).unix_millis(), ms);
    assert_eq!(Timestamp::from_unix_millis(ms).unix_secs(), 1_760_000_000);
    assert_eq!(Timestamp::from_unix_secs(1_760_000_000).unix_secs(), 1_760_000_000);

    let negative = -1_500_i64;
    assert_eq!(Timestamp::from_unix_millis(negative).unix_millis(), negative);

    let ts = Timestamp::parse("2026-03-10T19:25:47.000Z").unwrap();
    assert_eq!(ts.to_string(), "2026-03-10T19:25:47.000Z");
    assert_eq!(String::from(ts), "2026-03-10T19:25:47.000Z");

    // Serde uses the canonical string form and validates on the way in.
    let json = serde_json::to_string(&ts).unwrap();
    assert_eq!(json, "\"2026-03-10T19:25:47.000Z\"");
    assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), ts);
    assert!(serde_json::from_str::<Timestamp>("\"not a timestamp\"").is_err());
    assert!(Timestamp::parse("2026-13-08T19:25:47Z").is_err());
    assert!(Timestamp::parse("2026-03-10T19:25:47.1234567890Z").is_err());
}

#[test]
fn timestamp_normalises_offsets() {
    let with_offset = Timestamp::parse("2026-03-10T21:25:47+02:00").unwrap();
    let utc = Timestamp::parse("2026-03-10T19:25:47.000Z").unwrap();
    assert_eq!(with_offset, utc);
    assert_eq!(with_offset.as_rfc3339(), "2026-03-10T19:25:47.000Z");

    let negative_offset = Timestamp::parse("2026-03-10T14:25:47-05:00").unwrap();
    assert_eq!(negative_offset, utc);
}

#[test]
fn timestamp_plus_minus_keep_millisecond_precision() {
    let start = Timestamp::parse("2026-03-10T19:25:47.123Z").unwrap();

    for delta in [
        Duration::from_nanos(1),
        Duration::from_nanos(999_999),
        Duration::from_micros(500),
    ] {
        let later = start.plus(delta);
        assert_eq!(later, start, "sub-millisecond addition changed the instant");
        assert_eq!(later.as_rfc3339(), "2026-03-10T19:25:47.123Z");
        assert_eq!(later.unix_millis(), start.unix_millis());

        // Subtraction floors the truncated instant, matching "truncate, never round".
        let earlier = start.minus(delta);
        assert_eq!(earlier.as_rfc3339(), "2026-03-10T19:25:47.122Z");
        assert_eq!(earlier.unix_millis(), start.unix_millis() - 1);
        assert!(earlier < start);
    }

    let one_ms_later = start.plus(Duration::from_millis(1));
    assert!(start < one_ms_later);
    assert_eq!(one_ms_later.as_rfc3339(), "2026-03-10T19:25:47.124Z");
    assert_eq!(one_ms_later.minus(Duration::from_millis(1)), start);
    assert_eq!(
        start.cmp(&one_ms_later),
        start.as_rfc3339().cmp(&one_ms_later.as_rfc3339())
    );

    // FakeClock::advance uses the same arithmetic.
    let clock = FakeClock::at(start);
    clock.advance(Duration::from_nanos(1));
    assert_eq!(clock.now(), start);
    clock.advance(Duration::from_millis(1));
    assert_eq!(clock.now(), one_ms_later);

    // Saturating in both directions stays inside the canonical year range.
    assert_eq!(start.plus(Duration::MAX), Timestamp::from_unix_secs(i64::MAX));
    assert_eq!(start.minus(Duration::MAX), Timestamp::from_unix_secs(i64::MIN));
}

#[test]
fn timestamp_clamps_to_four_digit_years() {
    let min = Timestamp::from_unix_secs(-62_230_291_200);
    assert_eq!(min.as_rfc3339(), "0000-01-01T00:00:00.000Z");
    assert_eq!(min, Timestamp::from_unix_secs(i64::MIN));
    assert_eq!(min, Timestamp::parse("0000-01-01T00:00:00.000Z").unwrap());
    // One millisecond before year 0000 also clamps to the first representable instant.
    assert_eq!(min, Timestamp::from_unix_millis(-62_167_219_200_001));

    let max = Timestamp::from_unix_secs(253_402_300_800);
    assert_eq!(max.as_rfc3339(), "9999-12-31T23:59:59.999Z");
    assert_eq!(max, Timestamp::from_unix_secs(i64::MAX));
    assert_eq!(max, Timestamp::parse("9999-12-31T23:59:59.999Z").unwrap());
    assert_eq!(max, Timestamp::from_unix_millis(253_402_300_800_000));

    assert!(min < max);
    assert!(min.as_rfc3339() < max.as_rfc3339());
    for ts in [min, max] {
        assert_eq!(ts.as_rfc3339().len(), 24);
        assert!(ts.as_rfc3339().starts_with(|c: char| c.is_ascii_digit()));
    }
}

proptest! {
    #[test]
    fn timestamp_ordering_matches_strings(a in 0_i64..4_000_000_000_000, b in 0_i64..4_000_000_000_000) {
        let left = Timestamp::from_unix_millis(a);
        let right = Timestamp::from_unix_millis(b);
        prop_assert_eq!(left.cmp(&right), left.as_rfc3339().cmp(&right.as_rfc3339()));
        prop_assert_eq!(left == right, left.as_rfc3339() == right.as_rfc3339());
        prop_assert_eq!(left.as_rfc3339().len(), 24);
    }
}

#[test]
fn fake_clock_advances() {
    let start = Timestamp::parse("2026-03-10T19:25:47.123Z").unwrap();
    let clock = FakeClock::at(start);
    assert_eq!(clock.now(), start);

    let clone = clock.clone();
    clone.advance(Duration::from_secs(90));
    assert_eq!(clock.now(), start.plus(Duration::from_secs(90)));
    assert_eq!(clone.now(), start.plus(Duration::from_secs(90)));

    let later = start.plus(Duration::from_secs(3600));
    clock.set(later);
    assert_eq!(clone.now(), later);

    assert_eq!(start.since(start), Duration::ZERO);
    assert_eq!(later.since(start), Duration::from_secs(3600));
    assert_eq!(start.since(later), Duration::ZERO);
}

#[test]
fn log_format_line() {
    let ts = Timestamp::parse("2026-03-10T19:25:47.123Z").unwrap();
    assert_eq!(
        format_log_line(ts, Level::Info, "l1", "job sample-job: running -> gating"),
        "2026-03-10T19:25:47.123Z INFO l1 job sample-job: running -> gating"
    );
    assert_eq!(Level::Debug.as_str(), "DEBUG");
    assert_eq!(Level::Info.as_str(), "INFO");
    assert_eq!(Level::Warn.as_str(), "WARN");
    assert_eq!(Level::Error.as_str(), "ERROR");

    let log = MemoryLog::new();
    log.log(Level::Info, "l1", "running -> gating");
    log.log(Level::Warn, "l6", "probe failed");
    let copy = log.clone();
    copy.log(Level::Error, "l1", "parked");
    assert_eq!(
        log.lines(),
        vec![
            (Level::Info, "l1".to_owned(), "running -> gating".to_owned()),
            (Level::Warn, "l6".to_owned(), "probe failed".to_owned()),
            (Level::Error, "l1".to_owned(), "parked".to_owned()),
        ]
    );
    assert!(log.contains("probe failed"));
    assert!(log.contains("l6"));
    assert!(!log.contains("credits"));

    let null = NullLog;
    null.log(Level::Info, "l1", "discarded");
}
