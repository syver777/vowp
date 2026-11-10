//! Private row/value conversions between SQLite TEXT/INTEGER and protocol types.
//! Spec: `docs/design/crates/vowp-store.md` §4.4 rules 10–13.
//!
//! Every enum is stored under its serde wire name, every timestamp under
//! [`Timestamp::as_rfc3339`], and every JSON column under `serde_json`. A malformed value read
//! back from the database becomes [`StoreError::Corrupt`], never a silent default.

use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use vowp_protocol::{Executor, HarnessKind, ProtocolError, Timestamp};

use crate::error::StoreError;

/// Builds a corrupt-data error naming `what` and the underlying parse failure.
pub(crate) fn corrupt(what: &str, reason: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt {
        path: what.to_owned(),
        line: None,
        reason: reason.to_string(),
    }
}

/// Formats a timestamp in its canonical RFC 3339 millisecond form.
pub(crate) fn ts_to_text(ts: Timestamp) -> String {
    ts.as_rfc3339()
}

/// Formats an optional timestamp for a nullable TEXT column.
pub(crate) fn opt_ts_to_text(ts: Option<Timestamp>) -> Option<String> {
    ts.map(ts_to_text)
}

/// Parses one canonical timestamp column value.
pub(crate) fn ts_from_text(text: &str, what: &str) -> Result<Timestamp, StoreError> {
    Timestamp::parse(text).map_err(|err| corrupt(what, err))
}

/// Parses one nullable timestamp column value.
pub(crate) fn opt_ts_from_text(text: Option<&str>, what: &str) -> Result<Option<Timestamp>, StoreError> {
    text.map(|value| ts_from_text(value, what)).transpose()
}

/// Returns the serde wire name of an enum value.
pub(crate) fn enum_to_text<T: Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(name) => Ok(name),
        other => Err(corrupt(
            "enum column",
            format!("expected a string wire name, got {other}"),
        )),
    }
}

/// Parses an enum value from its serde wire name.
pub(crate) fn enum_from_text<T: DeserializeOwned>(text: &str, what: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(|err| corrupt(what, err))
}

/// Serialises a protocol value for a JSON column.
pub(crate) fn json_to_text<T: Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

/// Parses a JSON column into a protocol value.
pub(crate) fn json_from_text<T: DeserializeOwned>(text: &str, what: &str) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|err| corrupt(what, err))
}

/// Parses an id column.
#[allow(dead_code)] // used by the repository and log modules
pub(crate) fn id_from_text<T>(text: &str, what: &str) -> Result<T, StoreError>
where
    T: FromStr<Err = ProtocolError>,
{
    text.parse().map_err(|err| corrupt(what, err))
}

/// Converts a boolean into SQLite's 0/1 integer form.
pub(crate) fn bool_to_int(value: bool) -> i64 {
    i64::from(value)
}

/// Reads SQLite's 0/1 integer form as a boolean.
pub(crate) fn bool_from_int(value: i64) -> bool {
    value != 0
}

/// Converts a stored integer to `u32`, reporting the column on overflow.
pub(crate) fn u32_from_i64(value: i64, what: &str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|err| corrupt(what, err))
}

/// Converts a stored integer to `u64`, reporting the column on overflow.
#[allow(dead_code)] // used by the repository and log modules
pub(crate) fn u64_from_i64(value: i64, what: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|err| corrupt(what, err))
}

/// Converts a stored integer to `i32`, reporting the column on overflow.
#[allow(dead_code)] // used by the repository and log modules
pub(crate) fn i32_from_i64(value: i64, what: &str) -> Result<i32, StoreError> {
    i32::try_from(value).map_err(|err| corrupt(what, err))
}

/// Splits an executor into the `runs.harness`, `runs.model` and `runs.effort` columns.
///
/// Shell and gate runs have no executor; they are recorded with harness `vowp` and no model.
pub(crate) fn executor_to_parts(executor: Option<&Executor>) -> (String, Option<String>, Option<String>) {
    match executor {
        Some(executor) => {
            let modifier = match executor.harness {
                HarnessKind::ClaudeCode => executor.effort.clone(),
                HarnessKind::Opencode => executor.variant.clone(),
                HarnessKind::Cli | HarnessKind::Vowp => None,
            };
            (
                executor.harness.as_str().to_owned(),
                Some(executor.model.clone()),
                modifier,
            )
        }
        None => (HarnessKind::Vowp.as_str().to_owned(), None, None),
    }
}

/// Rebuilds an optional executor from the `runs.harness`, `runs.model` and `runs.effort` columns.
pub(crate) fn executor_from_parts(
    harness: &str,
    model: Option<&str>,
    effort: Option<&str>,
    what: &str,
) -> Result<Option<Executor>, StoreError> {
    let harness: HarnessKind = enum_from_text(harness, what)?;
    match harness {
        HarnessKind::Vowp => Ok(None),
        other => {
            let model = model
                .ok_or_else(|| corrupt(what, "harness row has no model"))
                .map(str::to_owned)?;
            Ok(Some(Executor {
                harness: other,
                model,
                effort: if other == HarnessKind::ClaudeCode {
                    effort.map(str::to_owned)
                } else {
                    None
                },
                variant: if other == HarnessKind::Opencode {
                    effort.map(str::to_owned)
                } else {
                    None
                },
            }))
        }
    }
}
