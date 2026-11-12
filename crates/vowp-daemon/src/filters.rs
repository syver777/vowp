//! Query-string parsing for the list and event endpoints.
//!
//! The API accepts repeated and comma-separated `state` values, for example
//! `?state=running&state=ready` or `?state=running,ready`. Unknown keys are ignored so a
//! newer client can send parameters an older daemon does not understand. Every rejected
//! value produces a message the handler returns as a `400` [`vowp_protocol::api::ApiError`].

use serde::de::DeserializeOwned;
use serde_json::Value;
use time::macros::format_description;
use vowp_protocol::{InboxState, JobId, JobKind, JobState, ProjectName, Role, SessionState, Timestamp};
use vowp_store::{CostGroup, JobFilter, SessionFilter};

/// Default number of events returned by one request.
pub const DEFAULT_EVENT_LIMIT: u32 = 200;

/// Largest number of events one request may return.
pub const MAX_EVENT_LIMIT: u32 = 1000;

/// The grouping and bounds from a cost request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CostQuerySpec {
    /// Dimension to group by.
    pub group: CostGroup,
    /// Its snake_case wire name, echoed in the report's `by` field.
    pub by: &'static str,
    /// Include entries at or after this instant.
    pub since: Option<Timestamp>,
    /// Include entries at or before this instant.
    pub until: Option<Timestamp>,
}

/// The event cursor and page size from a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventQuery {
    /// Return events whose `seq` is greater than this value.
    pub after_seq: u64,
    /// Maximum number of events to return.
    pub limit: u32,
}

/// Parses the job list filters.
pub(crate) fn job_filter(raw: Option<&str>) -> Result<JobFilter, String> {
    let pairs = pairs(raw);
    let states = collect_values(&pairs, "state")
        .into_iter()
        .map(|value| parse_enum::<JobState>(&value, "job state"))
        .collect::<Result<Vec<_>, _>>()?;
    let kind = single_value(&pairs, "kind")
        .map(|value| parse_enum::<JobKind>(value, "job kind"))
        .transpose()?;
    let role = single_value(&pairs, "role")
        .map(|value| parse_enum::<Role>(value, "role"))
        .transpose()?;
    Ok(JobFilter {
        project: None,
        states,
        kind,
        role,
        id_prefix: None,
        limit: None,
    })
}

/// Parses the session list filters.
pub(crate) fn session_filter(raw: Option<&str>) -> Result<SessionFilter, String> {
    let pairs = pairs(raw);
    let project = single_value(&pairs, "project")
        .map(|value| ProjectName::parse(value).map_err(|_| format!("invalid project name {value:?}")))
        .transpose()?;
    let job_id = single_value(&pairs, "job")
        .map(|value| JobId::parse(value).map_err(|_| format!("invalid job id {value:?}")))
        .transpose()?;
    let state = single_value(&pairs, "state")
        .map(|value| parse_enum::<SessionState>(value, "session state"))
        .transpose()?;
    Ok(SessionFilter {
        project,
        job_id,
        state,
        limit: None,
    })
}

/// Parses the inbox state filter, defaulting to `open`.
pub(crate) fn inbox_state(raw: Option<&str>) -> Result<InboxState, String> {
    let pairs = pairs(raw);
    match single_value(&pairs, "state").map(str::trim) {
        Some(value) if !value.is_empty() => parse_enum(value, "inbox state"),
        _ => Ok(InboxState::Open),
    }
}

/// Parses the cost grouping and time bounds.
pub(crate) fn cost_query(raw: Option<&str>) -> Result<CostQuerySpec, String> {
    let pairs = pairs(raw);
    let (group, by) = match single_value(&pairs, "by")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("job")
    {
        "job" => (CostGroup::Job, "job"),
        "role" => (CostGroup::Role, "role"),
        "provider" => (CostGroup::Provider, "provider"),
        "model" => (CostGroup::Model, "model"),
        "day" => (CostGroup::Day, "day"),
        other => {
            return Err(format!(
                "unknown cost dimension {other:?} (job, role, provider, model or day)"
            ));
        }
    };
    let since = time_value(&pairs, "since")?;
    let until = time_value(&pairs, "until")?;
    Ok(CostQuerySpec {
        group,
        by,
        since,
        until,
    })
}

/// Parses one optional `since`/`until` as RFC 3339 or a `YYYY-MM-DD` UTC midnight.
fn time_value(pairs: &[(String, String)], name: &str) -> Result<Option<Timestamp>, String> {
    match single_value(pairs, name).map(str::trim) {
        Some(value) if !value.is_empty() => parse_cost_time(value, name).map(Some),
        _ => Ok(None),
    }
}

/// Parses an RFC 3339 timestamp or a calendar date at UTC midnight.
fn parse_cost_time(value: &str, name: &str) -> Result<Timestamp, String> {
    if let Ok(date) = time::Date::parse(value, format_description!("[year]-[month]-[day]")) {
        return Ok(Timestamp::from_unix_secs(date.midnight().assume_utc().unix_timestamp()));
    }
    Timestamp::parse(value).map_err(|_| format!("invalid {name} {value:?}; use RFC 3339 or YYYY-MM-DD"))
}

/// Parses the per-session event cursor and page size.
pub(crate) fn event_query(raw: Option<&str>) -> Result<EventQuery, String> {
    let pairs = pairs(raw);
    let after_seq = single_value(&pairs, "after_seq")
        .map(|value| value.parse::<u64>().map_err(|_| format!("invalid after_seq {value:?}")))
        .transpose()?
        .unwrap_or(0);
    let limit = single_value(&pairs, "limit")
        .map(|value| value.parse::<u64>().map_err(|_| format!("invalid limit {value:?}")))
        .transpose()?
        .map_or(DEFAULT_EVENT_LIMIT, |value| {
            u32::try_from(value.min(u64::from(MAX_EVENT_LIMIT))).unwrap_or(MAX_EVENT_LIMIT)
        });
    Ok(EventQuery { after_seq, limit })
}

/// Parses the per-session SSE cursor, defaulting to 0.
pub(crate) fn event_stream_query(raw: Option<&str>) -> Result<u64, String> {
    let pairs = pairs(raw);
    single_value(&pairs, "after_seq")
        .map(|value| value.parse::<u64>().map_err(|_| format!("invalid after_seq {value:?}")))
        .transpose()
        .map(|after_seq| after_seq.unwrap_or(0))
}

/// Parses one JSON-schema enum from its snake_case wire string.
fn parse_enum<T: DeserializeOwned>(value: &str, what: &str) -> Result<T, String> {
    serde_json::from_value(Value::String(value.to_owned())).map_err(|_| format!("unknown {what} {value:?}"))
}

/// Splits a raw query string into decoded key/value pairs.
fn pairs(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (decode(key), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Returns the last value for `name`, if any.
fn single_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
        .find(|(key, _)| key.as_str() == name)
        .map(|(_, value)| value.as_str())
}

/// Returns every value for `name`, splitting comma-separated lists and dropping blanks.
fn collect_values(pairs: &[(String, String)], name: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(key, _)| key.as_str() == name)
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Decodes one `application/x-www-form-urlencoded` component.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    index += 3;
                }
                _ => {
                    out.push(b'%');
                    index += 1;
                }
            },
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the value of one hexadecimal digit.
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_states_accept_repeated_and_comma_separated_values() {
        let filter = job_filter(Some("state=running&state=ready,done")).expect("filter");
        assert_eq!(filter.states, vec![JobState::Running, JobState::Ready, JobState::Done]);
    }

    #[test]
    fn job_filters_parse_kind_and_role() {
        let filter = job_filter(Some("kind=build&role=worker")).expect("filter");
        assert_eq!(filter.kind, Some(JobKind::Build));
        assert_eq!(filter.role, Some(Role::Worker));
    }

    #[test]
    fn job_filters_reject_unknown_values() {
        let error = job_filter(Some("state=not-a-state")).expect_err("bad state");
        assert!(error.contains("unknown job state"), "{error}");
        assert!(job_filter(Some("kind=not-a-kind")).is_err());
        assert!(job_filter(Some("role=not-a-role")).is_err());
    }

    #[test]
    fn empty_state_value_means_no_state_filter() {
        let filter = job_filter(Some("state=")).expect("filter");
        assert!(filter.states.is_empty());
    }

    #[test]
    fn session_filters_parse_every_field() {
        let filter = session_filter(Some("project=proj&job=job-a&state=idle")).expect("filter");
        assert_eq!(filter.project.expect("project").as_str(), "proj");
        assert_eq!(filter.job_id.expect("job").as_str(), "job-a");
        assert_eq!(filter.state, Some(SessionState::Idle));
    }

    #[test]
    fn session_filters_reject_bad_ids_and_states() {
        assert!(session_filter(Some("project=Not_Valid")).is_err());
        assert!(session_filter(Some("job=Not_Valid")).is_err());
        assert!(session_filter(Some("state=nope")).is_err());
    }

    #[test]
    fn event_query_defaults_and_caps_the_limit() {
        let query = event_query(None).expect("defaults");
        assert_eq!(query.after_seq, 0);
        assert_eq!(query.limit, DEFAULT_EVENT_LIMIT);

        let query = event_query(Some("after_seq=7&limit=5")).expect("explicit");
        assert_eq!(query.after_seq, 7);
        assert_eq!(query.limit, 5);

        let query = event_query(Some("limit=999999")).expect("capped");
        assert_eq!(query.limit, MAX_EVENT_LIMIT);
    }

    #[test]
    fn event_query_rejects_bad_numbers() {
        assert!(event_query(Some("after_seq=nope")).is_err());
        assert!(event_query(Some("limit=-1")).is_err());
        assert!(event_query(Some("after_seq=18446744073709551616")).is_err());
    }

    #[test]
    fn event_stream_query_defaults_and_rejects_bad_cursors() {
        assert_eq!(event_stream_query(None).expect("default"), 0);
        assert_eq!(event_stream_query(Some("after_seq=7")).expect("cursor"), 7);
        assert_eq!(event_stream_query(Some("after_seq=0")).expect("zero"), 0);
        assert!(event_stream_query(Some("after_seq=nope")).is_err());
        assert!(event_stream_query(Some("after_seq=-1")).is_err());
        assert!(event_stream_query(Some("after_seq=18446744073709551616")).is_err());
    }

    #[test]
    fn query_decoding_handles_percent_and_plus() {
        let filter = job_filter(Some("state=run%6Eing&role=worker")).expect("filter");
        assert_eq!(filter.states, vec![JobState::Running]);
        assert_eq!(decode("a+b%2Fc"), "a b/c");
    }

    #[test]
    fn unknown_query_keys_are_ignored() {
        let filter = job_filter(Some("future=1&state=pending")).expect("filter");
        assert_eq!(filter.states, vec![JobState::Pending]);
    }

    #[test]
    fn inbox_state_defaults_to_open_and_rejects_unknown_values() {
        assert_eq!(inbox_state(None).expect("default"), InboxState::Open);
        assert_eq!(inbox_state(Some("state=")).expect("empty"), InboxState::Open);
        assert_eq!(
            inbox_state(Some("state=answered")).expect("answered"),
            InboxState::Answered
        );
        assert_eq!(
            inbox_state(Some("state=dismissed")).expect("dismissed"),
            InboxState::Dismissed
        );
        assert!(inbox_state(Some("state=nope")).is_err());
    }

    #[test]
    fn cost_query_defaults_and_parses_groups_and_bounds() {
        let query = cost_query(None).expect("defaults");
        assert_eq!(query.group, CostGroup::Job);
        assert_eq!(query.by, "job");
        assert_eq!(query.since, None);
        assert_eq!(query.until, None);

        let query = cost_query(Some("by=role&since=2026-01-03&until=2026-01-04T12:00:00.000Z")).expect("explicit");
        assert_eq!(query.group, CostGroup::Role);
        assert_eq!(query.by, "role");
        assert_eq!(
            query.since,
            Some(Timestamp::parse("2026-01-03T00:00:00.000Z").expect("since"))
        );
        assert_eq!(
            query.until,
            Some(Timestamp::parse("2026-01-04T12:00:00.000Z").expect("until"))
        );

        assert_eq!(cost_query(Some("by=")).expect("empty by").group, CostGroup::Job);
        assert!(cost_query(Some("by=nope")).is_err());
        assert!(cost_query(Some("since=nope")).is_err());
        assert!(cost_query(Some("until=2026-13-01")).is_err());
    }
}
