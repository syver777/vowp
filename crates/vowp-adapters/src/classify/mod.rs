//! Classification of harness error text, reset times and secret stripping.
//! Spec: `docs/design/adapters.md` §5 and `docs/design/adapters.md` §2.4.
//!
//! [`classify_text`] is pure: callers decide whether an exit code or error signal makes a
//! matched limit count (adapters.md §5.5). This module never talks to a harness itself.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use vowp_protocol::{LimitKind, Timestamp};

/// Transient rate-limit backoff: a rate limit resets ten minutes after it was seen.
pub const TRANSIENT_BACKOFF_MIN: u32 = 10;

/// Fallback quota window when the harness gave no reset time: one hour.
pub const QUOTA_FALLBACK_MIN: u32 = 60;

/// What a piece of error text means.
#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    /// Kind of limit the text describes.
    pub kind: LimitKind,
    /// When the limit lifts, when it can be derived.
    pub resets_at: Option<Timestamp>,
    /// Redacted excerpt of the input, at most 300 characters.
    pub message: String,
    /// HTTP status that carried the limit, when the caller knew one.
    pub http_status: Option<u16>,
}

const CREDITS_PATTERN: &str = r"(?i)insufficient[ _](?:balance|credits?|funds)|payment required|credit balance (?:is )?too low|\b402\b|out of credits";
const AUTH_PATTERN: &str = r"(?i)invalid api key|incorrect api key|unauthorized|\b401\b|please run /login|not logged in|oauth token (?:has )?(?:expired|revoked)|authentication[_ ](?:error|failed)|permission_denied";
const QUOTA_PATTERN: &str = r"(?i)usage limit|hit your (?:\w+[ -])*limit|limit (?:reached|exceeded)|out of (?:extra )?usage|quota|limit will reset|resets? (?:at )?\d|usage_limit|weekly limit|session limit|5-hour limit";
const RATE_PATTERN: &str = r"(?i)rate[_ ]limit|\b429\b|\b529\b|overloaded|too many requests|\bbusy\b";
const CONTEXT_PATTERN: &str = r"(?i)prompt is too long|context (?:window|length) (?:exceeded|overflow)|maximum context length|context_length_exceeded|input is too long|exceeds the context";

/// Fixed secret patterns from adapters.md §5.4, compiled once.
const SECRET_PATTERN: &str = concat!(
    r"sk-[A-Za-z0-9_-]{20,}",
    r"|sb_secret_[A-Za-z0-9_-]{10,}",
    r"|ghp_[A-Za-z0-9]{20,}",
    r"|github_pat_[A-Za-z0-9_]{20,}",
    r"|AKIA[0-9A-Z]{16}",
    r"|-----BEGIN [A-Z ]*PRIVATE KEY-----",
    r"|xox[abp]-[A-Za-z0-9-]{10,}",
    r"|Bearer [A-Za-z0-9._~+/-]{20,}",
    r#"|(?i)(api[_-]?key|token|secret|password)["']?\s*[:=]\s*["']?[^\s"']{8,}"#,
);

/// HTTP status spellings seen in harness output.
const STATUS_PATTERN: &str = r#"(?i)(?:status(?:Code)?\s*"?\s*:\s*"?|HTTP\s+|API\s+Error\s*:\s*)(\d{3})"#;

const RESET_UNIX_PATTERN: &str = r"\|(\d{10})\b";
const RESET_CLOCK_PATTERN: &str = r"(?i)resets?\s+(?:at\s+)?(?:(?P<mon>jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+(?P<day>\d{1,2}),?\s+)?(?P<h>\d{1,2})(?::(?P<m>\d{2}))?\s*(?P<ap>am|pm)?";

struct Patterns {
    credits: Regex,
    auth: Regex,
    quota: Regex,
    rate: Regex,
    context: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        credits: Regex::new(CREDITS_PATTERN).expect("invariant: credits regex literal is valid"),
        auth: Regex::new(AUTH_PATTERN).expect("invariant: auth regex literal is valid"),
        quota: Regex::new(QUOTA_PATTERN).expect("invariant: quota regex literal is valid"),
        rate: Regex::new(RATE_PATTERN).expect("invariant: rate regex literal is valid"),
        context: Regex::new(CONTEXT_PATTERN).expect("invariant: context regex literal is valid"),
    })
}

fn secret_regex() -> &'static Regex {
    static SECRET: OnceLock<Regex> = OnceLock::new();
    SECRET.get_or_init(|| Regex::new(SECRET_PATTERN).expect("invariant: secret regex literal is valid"))
}

fn status_regex() -> &'static Regex {
    static STATUS: OnceLock<Regex> = OnceLock::new();
    STATUS.get_or_init(|| Regex::new(STATUS_PATTERN).expect("invariant: status regex literal is valid"))
}

fn reset_unix_regex() -> &'static Regex {
    static RESET: OnceLock<Regex> = OnceLock::new();
    RESET.get_or_init(|| Regex::new(RESET_UNIX_PATTERN).expect("invariant: unix reset regex literal is valid"))
}

fn reset_clock_regex() -> &'static Regex {
    static RESET: OnceLock<Regex> = OnceLock::new();
    RESET.get_or_init(|| Regex::new(RESET_CLOCK_PATTERN).expect("invariant: clock reset regex literal is valid"))
}

/// Classifies harness error text.
///
/// `provider_hint` is informational ("claude", "deepseek", "") and only prefixes the
/// message. `http_status` participates as a shortcut: 402 is credits, 401/403 auth and
/// 429/529 rate. Precedence is credits > auth > quota > rate > context. Returns `None`
/// when nothing matches. This function is pure; callers apply the exit-code gate.
pub fn classify_text(
    provider_hint: &str,
    text: &str,
    http_status: Option<u16>,
    now: Timestamp,
    tz_offset_minutes: i32,
) -> Option<Classified> {
    let patterns = patterns();
    let message = classified_message(provider_hint, text);

    if http_status == Some(402) || patterns.credits.is_match(text) {
        return Some(Classified {
            kind: LimitKind::Credits,
            resets_at: None,
            message,
            http_status,
        });
    }

    if matches!(http_status, Some(401 | 403)) || patterns.auth.is_match(text) {
        return Some(Classified {
            kind: LimitKind::Auth,
            resets_at: None,
            message,
            http_status,
        });
    }

    if patterns.quota.is_match(text) {
        return Some(Classified {
            kind: LimitKind::Quota,
            resets_at: parse_reset_time(text, now, tz_offset_minutes),
            message,
            http_status,
        });
    }

    if matches!(http_status, Some(429 | 529)) || patterns.rate.is_match(text) {
        return Some(Classified {
            kind: LimitKind::Rate,
            resets_at: Some(now.plus(Duration::from_secs(u64::from(TRANSIENT_BACKOFF_MIN) * 60))),
            message,
            http_status,
        });
    }

    if patterns.context.is_match(text) {
        return Some(Classified {
            kind: LimitKind::Context,
            resets_at: None,
            message,
            http_status,
        });
    }

    None
}

/// Builds the redacted, provider-prefixed classifier excerpt.
fn classified_message(provider_hint: &str, text: &str) -> String {
    if provider_hint.is_empty() {
        redact(text, 300)
    } else {
        redact(&format!("{provider_hint}: {text}"), 300)
    }
}

/// Parses a reset time out of limit text (adapters.md §5.2).
///
/// First a `|<10 digits>` Unix suffix is used exactly. Otherwise a clock time such as
/// `resets 11pm`, `resets 4:30 am` or `resets Nov 12, 9am` is interpreted in local time
/// (UTC plus `tz_offset_minutes`) and converted back to UTC. No safety margin is added.
pub fn parse_reset_time(text: &str, now: Timestamp, tz_offset_minutes: i32) -> Option<Timestamp> {
    if let Some(captures) = reset_unix_regex().captures(text) {
        let secs: i64 = captures.get(1)?.as_str().parse().ok()?;
        return Some(Timestamp::from_unix_secs(secs));
    }

    let captures = reset_clock_regex().captures(text)?;
    let hour: u32 = captures.name("h")?.as_str().parse().ok()?;
    let minute: u32 = captures
        .name("m")
        .map_or(Some(0), |value| value.as_str().parse().ok())?;
    if hour > 23 || minute > 59 {
        return None;
    }

    let mut hour = hour;
    if let Some(ap) = captures.name("ap") {
        match ap.as_str().to_ascii_lowercase().as_str() {
            "pm" if hour < 12 => hour += 12,
            "am" if hour == 12 => hour = 0,
            _ => {}
        }
    }

    let offset = UtcOffset::from_whole_seconds(tz_offset_minutes.clamp(-1439, 1439) * 60).ok()?;
    let utc_now = OffsetDateTime::from_unix_timestamp(now.unix_secs()).ok()?;
    let local_now = utc_now.to_offset(offset);
    let time = Time::from_hms(u8::try_from(hour).ok()?, u8::try_from(minute).ok()?, 0).ok()?;

    let month = captures.name("mon").and_then(|value| month_from_name(value.as_str()));
    if let Some(month) = month {
        let day: u8 = captures.name("day")?.as_str().parse().ok()?;
        let year = local_now.year();
        let date = Date::from_calendar_date(year, month, day).ok()?;
        let mut candidate = PrimitiveDateTime::new(date, time).assume_offset(offset);
        let one_day = time::Duration::days(1);
        if candidate.to_offset(UtcOffset::UTC) < utc_now - one_day {
            let date = Date::from_calendar_date(year + 1, month, day).ok()?;
            candidate = PrimitiveDateTime::new(date, time).assume_offset(offset);
        }
        return Some(Timestamp::from_unix_secs(
            candidate.to_offset(UtcOffset::UTC).unix_timestamp(),
        ));
    }

    let date = local_now.date();
    let mut candidate = PrimitiveDateTime::new(date, time).assume_offset(offset);
    if candidate <= local_now {
        let date = date.next_day()?;
        candidate = PrimitiveDateTime::new(date, time).assume_offset(offset);
    }
    Some(Timestamp::from_unix_secs(
        candidate.to_offset(UtcOffset::UTC).unix_timestamp(),
    ))
}

/// Maps an English month prefix to its [`Month`].
fn month_from_name(name: &str) -> Option<Month> {
    match name.get(..3)?.to_ascii_lowercase().as_str() {
        "jan" => Some(Month::January),
        "feb" => Some(Month::February),
        "mar" => Some(Month::March),
        "apr" => Some(Month::April),
        "may" => Some(Month::May),
        "jun" => Some(Month::June),
        "jul" => Some(Month::July),
        "aug" => Some(Month::August),
        "sep" => Some(Month::September),
        "oct" => Some(Month::October),
        "nov" => Some(Month::November),
        "dec" => Some(Month::December),
        _ => None,
    }
}

/// Extracts the latest reset from a Claude `rate_limit_info` object (adapters.md §2.4).
///
/// `unifiedWindows` is accepted in both shapes Claude emits: the object map seen in real
/// streams (`{"five_hour": {…}, "seven_day": {…}}`) and an array of window objects. Uses
/// the maximum `resetsAt` among rejected windows, plus the top-level `resetsAt` when the
/// top-level status is `"rejected"`; allowed windows are ignored. Returns `None` when no
/// window is rejected. No margin is added.
pub fn reset_from_rate_limit_info(info: &serde_json::Value) -> Option<Timestamp> {
    let mut latest: Option<i64> = None;
    let mut consider = |value: Option<&serde_json::Value>| {
        if let Some(secs) = value.and_then(unix_seconds) {
            latest = Some(latest.map_or(secs, |current| current.max(secs)));
        }
    };

    if info.get("status").and_then(serde_json::Value::as_str) == Some("rejected") {
        consider(info.get("resetsAt"));
    }

    let is_rejected =
        |window: &serde_json::Value| window.get("status").and_then(serde_json::Value::as_str) == Some("rejected");

    match info.get("unifiedWindows") {
        Some(serde_json::Value::Array(windows)) => {
            for window in windows {
                if is_rejected(window) {
                    consider(window.get("resetsAt"));
                }
            }
        }
        Some(serde_json::Value::Object(windows)) => {
            for window in windows.values() {
                if is_rejected(window) {
                    consider(window.get("resetsAt"));
                }
            }
        }
        _ => {}
    }

    latest.map(Timestamp::from_unix_secs)
}

/// Reads a Unix-seconds value from a JSON number or numeric string.
fn unix_seconds(value: &serde_json::Value) -> Option<i64> {
    if let Some(secs) = value.as_i64() {
        return Some(secs);
    }
    value.as_str()?.parse().ok()
}

/// Strips secret-looking substrings, collapses whitespace and truncates to `max_chars`.
///
/// Truncation happens on a character boundary and appends a trailing `…`, so the result
/// never exceeds `max_chars` characters. Redaction never fails.
pub fn redact(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let stripped = secret_regex().replace_all(text, "<redacted>");
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut truncated: String = collapsed.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

/// Extracts an HTTP status from text such as `"statusCode":402`, `HTTP 401` or
/// `API Error: 529`. Only three-digit statuses in the 100–599 range are returned.
pub fn extract_http_status(text: &str) -> Option<u16> {
    let captures = status_regex().captures(text)?;
    let status: u16 = captures.get(1)?.as_str().parse().ok()?;
    (100..=599).contains(&status).then_some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::parse("2026-11-08T18:10:00Z").expect("invariant: test timestamp literal is valid")
    }

    #[test]
    fn strip_unix_reset_is_exact() {
        assert_eq!(
            parse_reset_time("resets |1794164400", now(), 0),
            Some(Timestamp::parse("2026-11-08T19:00:00Z").expect("invariant: timestamp literal is valid"))
        );
    }

    #[test]
    fn clock_reset_rolls_forward_when_past() {
        assert_eq!(
            parse_reset_time("resets 3pm", now(), 60),
            Some(Timestamp::parse("2026-11-09T14:00:00Z").expect("invariant: timestamp literal is valid"))
        );
    }

    #[test]
    fn redaction_never_panics_on_multibyte_truncation() {
        let out = redact(&"é".repeat(500), 30);
        assert!(out.chars().count() <= 30);
        assert!(out.ends_with('…'));
    }
}
