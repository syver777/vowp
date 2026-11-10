//! UTC timestamps and the injectable clock abstraction used by loops and gates.
//! Spec: `docs/design/types.md` §1 and `docs/design/crates/vowp-protocol.md` §4.
//!
//! [`Timestamp`] always displays and serialises as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC, exactly
//! three fractional digits). Parsing accepts any RFC 3339 offset with 0–9 fractional digits
//! and truncates (never rounds) to milliseconds.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::macros::{datetime, format_description};
use time::{OffsetDateTime, UtcOffset};

use crate::error::ProtocolError;

/// Canonical wire format: UTC, `T` separator, exactly three fractional digits, `Z` suffix.
const RFC3339_MILLIS: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// First instant that still has the canonical four-digit `YYYY` wire form.
const MIN_UTC: OffsetDateTime = datetime!(0000-01-01 0:00:00 UTC);

/// Last instant that still has the canonical four-digit `YYYY` wire form.
const MAX_UTC: OffsetDateTime = datetime!(9999-12-31 23:59:59.999 UTC);

/// A UTC instant with millisecond precision.
///
/// The wire/DB form is RFC 3339 with a `Z` suffix and exactly three fractional digits,
/// for example `2026-11-08T19:25:47.123Z`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Returns the current instant, truncated to milliseconds.
    pub fn now() -> Self {
        Self::truncated(OffsetDateTime::now_utc())
    }

    /// Builds a timestamp from whole Unix seconds.
    ///
    /// Values outside the canonical year range (0000–9999) are clamped to its nearest bound,
    /// so the wire form always has a four-digit year.
    pub fn from_unix_secs(secs: i64) -> Self {
        let odt = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(if secs < 0 { MIN_UTC } else { MAX_UTC });
        Self::truncated(odt)
    }

    /// Builds a timestamp from Unix milliseconds (negative values are before 1970).
    ///
    /// Values outside the canonical year range (0000–9999) are clamped to its nearest bound.
    pub fn from_unix_millis(ms: i64) -> Self {
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000);
        match OffsetDateTime::from_unix_timestamp(secs) {
            Ok(odt) => {
                let odt = odt.checked_add(time::Duration::milliseconds(millis)).unwrap_or(MAX_UTC);
                Self::truncated(odt)
            }
            Err(_) => {
                if ms < 0 {
                    Self(MIN_UTC)
                } else {
                    Self(MAX_UTC)
                }
            }
        }
    }

    /// Returns whole Unix seconds (rounded towards negative infinity for pre-1970 instants).
    pub fn unix_secs(&self) -> i64 {
        self.0.unix_timestamp()
    }

    /// Returns Unix milliseconds.
    pub fn unix_millis(&self) -> i64 {
        self.0.unix_timestamp() * 1000 + i64::from(self.0.millisecond())
    }

    /// Parses an RFC 3339 timestamp with any offset, normalising to UTC and truncating to ms.
    ///
    /// Up to nine fractional digits are accepted; more are rejected.
    pub fn parse(s: &str) -> Result<Self, ProtocolError> {
        if let Some(dot) = s.find('.') {
            let digits = s[dot + 1..].bytes().take_while(u8::is_ascii_digit).count();
            if digits > 9 {
                return Err(ProtocolError::InvalidTimestamp(format!(
                    "{s}: more than 9 fractional digits"
                )));
            }
        }
        let odt =
            OffsetDateTime::parse(s, &Rfc3339).map_err(|err| ProtocolError::InvalidTimestamp(format!("{s}: {err}")))?;
        Ok(Self::truncated(odt.to_offset(UtcOffset::UTC)))
    }

    /// Returns the canonical RFC 3339 form, for example `2026-11-08T19:25:47.123Z`.
    pub fn as_rfc3339(&self) -> String {
        self.0.format(RFC3339_MILLIS).unwrap_or_default()
    }

    /// Adds a duration, keeping millisecond precision and clamping to the canonical year range.
    pub fn plus(&self, d: Duration) -> Self {
        let d = time::Duration::try_from(d).unwrap_or(time::Duration::MAX);
        match self.0.checked_add(d) {
            Some(odt) => Self::truncated(odt),
            None => Self(MAX_UTC),
        }
    }

    /// Subtracts a duration, keeping millisecond precision and clamping to the canonical year range.
    pub fn minus(&self, d: Duration) -> Self {
        let d = time::Duration::try_from(d).unwrap_or(time::Duration::MAX);
        match self.0.checked_sub(d) {
            Some(odt) => Self::truncated(odt),
            None => Self(MIN_UTC),
        }
    }

    /// Returns the elapsed time since `earlier`, saturating at zero for future instants.
    pub fn since(&self, earlier: Timestamp) -> Duration {
        let delta = self.0 - earlier.0;
        if delta.is_negative() {
            Duration::ZERO
        } else {
            delta.try_into().unwrap_or(Duration::MAX)
        }
    }

    /// Truncates an instant to millisecond precision, converts it to UTC and clamps it to the
    /// canonical four-digit-year range.
    fn truncated(odt: OffsetDateTime) -> Self {
        let odt = odt.to_offset(UtcOffset::UTC).clamp(MIN_UTC, MAX_UTC);
        let nanos = odt.nanosecond();
        let truncated = nanos - nanos % 1_000_000;
        Self(odt.replace_nanosecond(truncated).unwrap_or(odt))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_rfc3339())
    }
}

impl TryFrom<String> for Timestamp {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.as_rfc3339()
    }
}

/// A source of the current time, injected wherever behaviour depends on the clock.
pub trait Clock: Send + Sync {
    /// Returns the current instant.
    fn now(&self) -> Timestamp;
}

/// The real clock, backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A manually advanced clock for tests. Clones share the same underlying instant.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now: Arc<Mutex<Timestamp>>,
}

impl FakeClock {
    /// Creates a clock fixed at `t`.
    pub fn at(t: Timestamp) -> Self {
        Self {
            now: Arc::new(Mutex::new(t)),
        }
    }

    /// Moves the clock forward by `d`.
    pub fn advance(&self, d: Duration) {
        let mut now = self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *now = now.plus(d);
    }

    /// Sets the clock to an exact instant.
    pub fn set(&self, t: Timestamp) {
        let mut now = self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *now = t;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        *self.now.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
