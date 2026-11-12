//! Cron, interval and timezone tests for `ScheduleSpec`.
//! Spec: `docs/design/crates/vowp-core.md` §6 and `docs/design/loops.md` §11.

use proptest::prelude::*;
use vowp_core::CoreError;
use vowp_core::schedules::ScheduleSpec;
use vowp_protocol::{ScheduleSpecKind, Timestamp};

/// Parses a timestamp, failing the test on bad input.
fn timestamp(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("timestamp")
}

/// Parses a cron spec in UTC.
fn cron(expression: &str) -> ScheduleSpec {
    ScheduleSpec::parse(ScheduleSpecKind::Cron, expression, "UTC").expect("cron")
}

#[test]
fn cron_five_field_next_runs_golden() {
    let schedule = cron("30 9 * * 1-5");
    let first = timestamp("2026-11-09T09:15:00Z");
    let next = schedule.next_after(first).expect("next");
    assert_eq!(next, timestamp("2026-11-09T09:30:00Z"));
    // The same instant does not match again: next_after is strictly after.
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T09:30:00Z")),
        Some(timestamp("2026-11-10T09:30:00Z"))
    );
    assert_eq!(schedule.next_after(next), Some(timestamp("2026-11-10T09:30:00Z")));
    assert_eq!(
        schedule.next_after(timestamp("2026-11-10T09:30:00Z")),
        Some(timestamp("2026-11-11T09:30:00Z"))
    );
}

#[test]
fn cron_weekday_window() {
    let schedule = cron("0 7 * * 1-5");
    // Friday after 07:00 → next weekday is Monday.
    assert_eq!(
        schedule.next_after(timestamp("2026-11-13T08:00:00Z")),
        Some(timestamp("2026-11-16T07:00:00Z"))
    );
    // Monday before 07:00 → same day.
    assert_eq!(
        schedule.next_after(timestamp("2026-11-16T06:59:00Z")),
        Some(timestamp("2026-11-16T07:00:00Z"))
    );
    // Saturday → Monday.
    assert_eq!(
        schedule.next_after(timestamp("2026-11-14T12:00:00Z")),
        Some(timestamp("2026-11-16T07:00:00Z"))
    );
}

#[test]
fn interval_every_30m() {
    let anchor = timestamp("2026-11-09T10:07:00Z");
    let schedule = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "every 30m", "UTC", anchor).expect("interval");
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:07:00Z")),
        Some(timestamp("2026-11-09T10:37:00Z"))
    );
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:37:00Z")),
        Some(timestamp("2026-11-09T11:07:00Z"))
    );
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T23:50:00Z")),
        Some(timestamp("2026-11-10T00:07:00Z"))
    );

    let bare = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "30m", "UTC", anchor).expect("bare interval");
    assert_eq!(
        bare.next_after(timestamp("2026-11-09T10:07:00Z")),
        schedule.next_after(timestamp("2026-11-09T10:07:00Z"))
    );

    let too_short = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "every 30s", "UTC", anchor);
    assert!(matches!(too_short, Err(CoreError::Invalid(_))), "got {too_short:?}");
    let zero = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "every 0m", "UTC", anchor);
    assert!(matches!(zero, Err(CoreError::Invalid(_))), "got {zero:?}");
    let nonsense = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "often", "UTC", anchor);
    assert!(matches!(nonsense, Err(CoreError::Invalid(_))), "got {nonsense:?}");
}

#[test]
fn interval_preserves_anchor_seconds() {
    let anchor = timestamp("2026-11-09T10:07:13Z");
    let schedule = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "every 30m", "UTC", anchor).expect("interval");
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:07:13Z")),
        Some(timestamp("2026-11-09T10:37:13Z"))
    );
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:37:13Z")),
        Some(timestamp("2026-11-09T11:07:13Z"))
    );
    // One second before the phase still lands on the phase, not the previous occurrence.
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:37:12Z")),
        Some(timestamp("2026-11-09T10:37:13Z"))
    );
}

#[test]
fn interval_before_anchor_returns_anchor_plus_period() {
    let anchor = timestamp("2026-11-09T10:07:00Z");
    let schedule = ScheduleSpec::parse_at(ScheduleSpecKind::Interval, "every 30m", "UTC", anchor).expect("interval");
    assert_eq!(
        schedule.next_after(timestamp("2026-11-09T10:00:00Z")),
        Some(timestamp("2026-11-09T10:37:00Z"))
    );
    assert_eq!(
        schedule.next_after(timestamp("2026-11-08T10:00:00Z")),
        Some(timestamp("2026-11-09T10:37:00Z"))
    );
}

#[test]
fn cron_sunday_is_zero_or_seven() {
    let sunday_zero = cron("0 7 * * 0");
    let sunday_seven = cron("0 7 * * 7");
    let saturday = cron("0 7 * * 6");
    let after = timestamp("2026-11-14T12:00:00Z"); // Saturday
    assert_eq!(sunday_zero.next_after(after), Some(timestamp("2026-11-15T07:00:00Z")));
    assert_eq!(sunday_seven.next_after(after), Some(timestamp("2026-11-15T07:00:00Z")));
    let friday = timestamp("2026-11-13T12:00:00Z");
    assert_eq!(saturday.next_after(friday), Some(timestamp("2026-11-14T07:00:00Z")));
}

#[test]
fn fixed_offset_tz_shifts_next_run() {
    let utc = cron("0 7 * * *");
    let plus_two = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 7 * * *", "+02:00").expect("tz +02");
    let minus_five = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 7 * * *", "-05:00").expect("tz -05");
    let after = timestamp("2026-11-08T19:00:00Z");

    assert_eq!(utc.next_after(after), Some(timestamp("2026-11-09T07:00:00Z")));
    // 07:00 at +02:00 is 05:00 UTC.
    assert_eq!(plus_two.next_after(after), Some(timestamp("2026-11-09T05:00:00Z")));
    // 07:00 at -05:00 is 12:00 UTC.
    assert_eq!(minus_five.next_after(after), Some(timestamp("2026-11-09T12:00:00Z")));

    let bad_tz = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 7 * * *", "Europe/Oslo");
    assert!(matches!(bad_tz, Err(CoreError::Invalid(_))), "got {bad_tz:?}");
    let bad_offset = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 7 * * *", "+2:00");
    assert!(matches!(bad_offset, Err(CoreError::Invalid(_))), "got {bad_offset:?}");
}

#[test]
fn bad_cron_rejected() {
    let text = ScheduleSpec::parse(ScheduleSpecKind::Cron, "not a cron", "UTC");
    assert!(matches!(text, Err(CoreError::Invalid(_))), "got {text:?}");
    let minutes = ScheduleSpec::parse(ScheduleSpecKind::Cron, "61 9 * * *", "UTC");
    assert!(matches!(minutes, Err(CoreError::Invalid(_))), "got {minutes:?}");
    let month = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 9 * 13 *", "UTC");
    assert!(matches!(month, Err(CoreError::Invalid(_))), "got {month:?}");
}

#[test]
fn six_field_cron_rejected() {
    let six = ScheduleSpec::parse(ScheduleSpecKind::Cron, "0 30 9 * * 1-5", "UTC");
    assert!(matches!(six, Err(CoreError::Invalid(_))), "got {six:?}");
    let four = ScheduleSpec::parse(ScheduleSpecKind::Cron, "30 9 * *", "UTC");
    assert!(matches!(four, Err(CoreError::Invalid(_))), "got {four:?}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn proptest_next_after_is_strictly_greater_and_matches_cron(minute in 0u8..60, hour in 0u8..24, secs in 0i64..2_000_000_000i64) {
        let expression = format!("{minute} {hour} * * *");
        let schedule = cron(&expression);
        let after = Timestamp::from_unix_secs(secs);
        let next = schedule.next_after(after).expect("next occurrence");
        prop_assert!(next > after);

        let day = secs.div_euclid(86_400);
        let second_of_day = secs.rem_euclid(86_400);
        let candidate = i64::from(hour) * 3_600 + i64::from(minute) * 60;
        let expected_secs = if candidate > second_of_day {
            day * 86_400 + candidate
        } else {
            (day + 1) * 86_400 + candidate
        };
        prop_assert_eq!(next, Timestamp::from_unix_secs(expected_secs), "expression {}", expression);
    }
}
