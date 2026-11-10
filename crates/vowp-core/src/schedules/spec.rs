//! Parsed schedule specs: five-field cron expressions and `every <n><unit>` intervals.
//! Spec: `docs/design/crates/vowp-core.md` §6 and `docs/design/loops.md` §11.
//!
//! Cron expressions are evaluated in the schedule's fixed UTC offset and converted back to
//! UTC. `next_after` is strictly after the given instant, so an occurrence exactly at `t`
//! does not fire twice. Day-of-week follows standard cron numbering (`0`/`7` Sunday, `1`
//! Monday); the `cron` crate numbers Sunday as 1, so that one field is matched here.
//!
//! Interval occurrences are aligned to the Unix epoch (`anchor + k·period` with anchor 0),
//! because [`ScheduleSpec::parse`] receives no `created_at`. This is the one deviation from
//! loops.md §11's `anchor = created_at`; see the job report.

use std::str::FromStr;

use cron::{Schedule, TimeUnitSpec};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use vowp_protocol::{ScheduleSpecKind, Timestamp};

use crate::CoreError;

/// Retry-free horizon for a cron search; the `cron` crate's year range ends at 2100.
const LAST_CRON_YEAR: i32 = 2100;

/// A parsed schedule spec that can compute its next occurrence.
#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    inner: Spec,
}

/// The two supported schedule dialects.
#[derive(Debug, Clone)]
enum Spec {
    /// A cron expression plus the fixed offset it is evaluated in and the standard day-of-week
    /// set (the `cron` crate numbers Sunday as 1; this engine uses standard cron numbering).
    Cron {
        schedule: Box<Schedule>,
        offset_minutes: i32,
        days: DayOfWeekSpec,
    },
    /// A fixed period in seconds.
    Interval { period_secs: u64 },
}

impl ScheduleSpec {
    /// Parses `spec` for `kind` evaluated in `tz`.
    ///
    /// Cron accepts exactly five fields (`min hour dom mon dow`) and prefixes the seconds
    /// field required by the `cron` crate. Interval accepts `every <n><s|m|h|d>` or the bare
    /// `<n><unit>` form used by `[[schedules]].every`, with a minimum period of 60 seconds.
    /// `tz` is `UTC` or a fixed `±HH:MM` offset.
    pub fn parse(kind: ScheduleSpecKind, spec: &str, tz: &str) -> Result<Self, CoreError> {
        let offset_minutes = parse_tz(tz)?;
        let inner = match kind {
            ScheduleSpecKind::Cron => {
                let text = spec.trim();
                let fields: Vec<&str> = text.split_whitespace().collect();
                if fields.len() != 5 {
                    return Err(CoreError::Invalid(format!(
                        "cron {spec:?} must have exactly 5 fields, found {}",
                        fields.len()
                    )));
                }
                // Standard cron numbers Sunday as 0 or 7 and Monday as 1; the `cron` crate
                // numbers Sunday as 1. Parse the DOW field here and let the crate match only
                // the remaining four fields.
                let days = DayOfWeekSpec::parse(fields[4])?;
                let expression = format!("0 {} {} {} {} *", fields[0], fields[1], fields[2], fields[3]);
                let schedule = Schedule::from_str(&expression)
                    .map_err(|error| CoreError::Invalid(format!("invalid cron {spec:?}: {error}")))?;
                Spec::Cron {
                    schedule: Box::new(schedule),
                    offset_minutes,
                    days,
                }
            }
            ScheduleSpecKind::Interval => Spec::Interval {
                period_secs: parse_interval(spec)?,
            },
        };
        Ok(Self { inner })
    }

    /// Returns the first occurrence strictly after `after`, when one exists.
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        match &self.inner {
            Spec::Cron {
                schedule,
                offset_minutes,
                days,
            } => cron_next_after(schedule.as_ref(), *offset_minutes, days, after),
            Spec::Interval { period_secs } => interval_next_after(*period_secs, after),
        }
    }
}

/// Computes the next cron occurrence in `offset_minutes`, converted back to UTC.
fn cron_next_after(
    schedule: &Schedule,
    offset_minutes: i32,
    days: &DayOfWeekSpec,
    after: Timestamp,
) -> Option<Timestamp> {
    let offset = UtcOffset::from_whole_seconds(offset_minutes.checked_mul(60)?).ok()?;
    let after_utc = OffsetDateTime::from_unix_timestamp(after.unix_secs()).ok()?;
    let local = after_utc.to_offset(offset);

    let mut date = if local.year() < 1970 {
        Date::from_calendar_date(1970, Month::January, 1).ok()?
    } else {
        local.date()
    };
    if date.year() > LAST_CRON_YEAR {
        return None;
    }

    while date.year() <= LAST_CRON_YEAR {
        let day_of_month = schedule.days_of_month().includes(u32::from(date.day()));
        let day_of_week = days.includes(date.weekday());
        // Standard cron: when both day fields are restricted, either may match.
        let day_matches = if schedule.days_of_month().is_all() || days.is_all() {
            day_of_month && day_of_week
        } else {
            day_of_month || day_of_week
        };
        let day_matches = schedule.years().includes(date.year() as u32)
            && schedule.months().includes(u32::from(u8::from(date.month())))
            && day_matches;
        if day_matches {
            for hour in schedule.hours().iter() {
                for minute in schedule.minutes().iter() {
                    let (Ok(hour), Ok(minute)) = (u8::try_from(hour), u8::try_from(minute)) else {
                        continue;
                    };
                    let Ok(time) = Time::from_hms(hour, minute, 0) else {
                        continue;
                    };
                    let candidate = PrimitiveDateTime::new(date, time)
                        .assume_offset(offset)
                        .to_offset(UtcOffset::UTC);
                    let candidate = Timestamp::from_unix_secs(candidate.unix_timestamp());
                    if candidate > after {
                        return Some(candidate);
                    }
                }
            }
        }
        date = date.next_day()?;
    }
    None
}

/// Computes the next epoch-aligned interval occurrence strictly after `after`.
fn interval_next_after(period_secs: u64, after: Timestamp) -> Option<Timestamp> {
    let period = i64::try_from(period_secs).ok()?;
    if period <= 0 {
        return None;
    }
    let next = (after.unix_secs().div_euclid(period) + 1).checked_mul(period)?;
    Some(Timestamp::from_unix_secs(next))
}

/// Parses `every <n><s|m|h|d>` (or `<n><unit>`) into seconds, rejecting periods under 60 s.
fn parse_interval(spec: &str) -> Result<u64, CoreError> {
    let text = spec.trim();
    let text = match text.strip_prefix("every") {
        Some(rest) => rest.trim_start(),
        None => text,
    };
    let invalid = || CoreError::Invalid(format!("interval {spec:?} must be `every <n><s|m|h|d>`"));
    let (digits, multiplier) = if let Some(digits) = text.strip_suffix('s') {
        (digits, 1_u64)
    } else if let Some(digits) = text.strip_suffix('m') {
        (digits, 60_u64)
    } else if let Some(digits) = text.strip_suffix('h') {
        (digits, 3_600_u64)
    } else if let Some(digits) = text.strip_suffix('d') {
        (digits, 86_400_u64)
    } else {
        return Err(invalid());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let count: u64 = digits.parse().map_err(|_| invalid())?;
    let seconds = count.checked_mul(multiplier).ok_or_else(invalid)?;
    if seconds < 60 {
        return Err(CoreError::Invalid(format!(
            "interval {spec:?} is below the 60 s minimum"
        )));
    }
    Ok(seconds)
}

/// Standard day-of-week matching for a five-field cron expression.
///
/// Standard cron numbers Sunday as `0` (or `7`), Monday as `1`, … Saturday as `6`; names are
/// accepted too. The `cron` crate numbers Sunday as `1`, so this field is matched here rather
/// than in the crate.
#[derive(Debug, Clone)]
struct DayOfWeekSpec {
    /// `days[0]` is Sunday … `days[6]` is Saturday.
    days: [bool; 7],
}

impl DayOfWeekSpec {
    /// Parses `*`, `?`, numbers `0..=7`, names, lists, ranges and `/step` suffixes.
    fn parse(text: &str) -> Result<Self, CoreError> {
        let mut days = [false; 7];
        for item in text.split(',') {
            apply_dow_item(text, item, &mut days)?;
        }
        if !days.iter().any(|day| *day) {
            return Err(CoreError::Invalid(format!("day-of-week field {text:?} matches no day")));
        }
        Ok(Self { days })
    }

    /// Returns true when this set includes `weekday`.
    fn includes(&self, weekday: time::Weekday) -> bool {
        self.days[usize::from(weekday.number_days_from_sunday())]
    }

    /// Returns true when every weekday is included.
    fn is_all(&self) -> bool {
        self.days.iter().all(|day| *day)
    }
}

/// Applies one comma-separated day-of-week item to `days`.
fn apply_dow_item(text: &str, item: &str, days: &mut [bool; 7]) -> Result<(), CoreError> {
    let invalid = |detail: &str| CoreError::Invalid(format!("invalid day-of-week field {text:?}: {detail}"));
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => {
            let step: u32 = step.parse().map_err(|_| invalid("step is not a number"))?;
            if step == 0 {
                return Err(invalid("step must be greater than zero"));
            }
            (base, step)
        }
        None => (item, 1),
    };
    let (start, end) = if base == "*" || base == "?" {
        (0, 7)
    } else if let Some((start, end)) = base.split_once('-') {
        let start = dow_value(text, start)?;
        let end = dow_value(text, end)?;
        if start > end {
            return Err(invalid("range runs backwards"));
        }
        (start, end)
    } else {
        let value = dow_value(text, base)?;
        (value, value)
    };
    let mut value = start;
    while value <= end {
        days[(value % 7) as usize] = true;
        value += step;
    }
    Ok(())
}

/// Parses one numeric or named day-of-week value into standard cron numbering (`0..=7`).
fn dow_value(text: &str, value: &str) -> Result<u32, CoreError> {
    let invalid = |detail: &str| CoreError::Invalid(format!("invalid day-of-week field {text:?}: {detail}"));
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u32>()
            .ok()
            .filter(|value| *value <= 7)
            .ok_or_else(|| invalid("day must be between 0 and 7"));
    }
    match value.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => Ok(0),
        "mon" | "monday" => Ok(1),
        "tue" | "tues" | "tuesday" => Ok(2),
        "wed" | "wednesday" => Ok(3),
        "thu" | "thurs" | "thursday" => Ok(4),
        "fri" | "friday" => Ok(5),
        "sat" | "saturday" => Ok(6),
        _ => Err(invalid("unknown day name")),
    }
}

/// Parses `UTC` or a fixed `±HH:MM` offset into whole minutes.
fn parse_tz(tz: &str) -> Result<i32, CoreError> {
    if tz == "UTC" {
        return Ok(0);
    }
    let invalid = || CoreError::Invalid(format!("timezone {tz:?} must be UTC or ±HH:MM"));
    let mut chars = tz.chars();
    let Some(sign) = chars.next() else {
        return Err(invalid());
    };
    if !matches!(sign, '+' | '-') {
        return Err(invalid());
    }
    let rest = chars.as_str();
    let mut parts = rest.split(':');
    let (Some(hours), Some(minutes), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(invalid());
    };
    if hours.len() != 2
        || minutes.len() != 2
        || !hours.bytes().all(|byte| byte.is_ascii_digit())
        || !minutes.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    let hours: i32 = hours.parse().map_err(|_| invalid())?;
    let minutes: i32 = minutes.parse().map_err(|_| invalid())?;
    if hours > 23 || minutes > 59 {
        return Err(invalid());
    }
    let offset = hours * 60 + minutes;
    Ok(if sign == '-' { -offset } else { offset })
}
