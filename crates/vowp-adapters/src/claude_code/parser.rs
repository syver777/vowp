//! `stream-json` line parser for the Claude Code adapter.
//! Spec: `docs/design/adapters.md` §2.2–§2.4, §5 and §7.
//!
//! The parser is pure: it reads no I/O and no clock beyond the `now` value its
//! constructor was given. It maps each stdout line to canonical [`HarnessEvent`]s and
//! classifies limits only from error sources (result error text, stderr and non-JSON
//! noise), never from assistant prose.

use serde_json::Value;
use vowp_protocol::{
    Actor, Cost, CostEvent, ErrorEvent, ErrorKind, EventBody, HarnessEvent, LimitEvent, LimitKind, Note, RunOutcome,
    RunSummary, StopReason, Timestamp,
};

use crate::classify::{Classified, classify_text, extract_http_status, redact, reset_from_rate_limit_info};
use crate::stream::StreamParser;

use super::mapping::{assistant_events, cost_from_result, push_tail, raw_json, system_events, turn_ended, user_events};

/// Serialised `raw` values larger than this are replaced by a truncation marker.
pub(super) const RAW_CAP_BYTES: usize = 16 * 1024;

/// Number of leading bytes kept in the truncation marker's `head`.
pub(super) const RAW_HEAD_BYTES: usize = 2 * 1024;

/// Maximum bytes of non-JSON stdout/stderr kept for classification.
const NOISE_CAP_BYTES: usize = 4 * 1024;

/// Parser for Claude Code `stream-json` lines.
///
/// One instance owns one run. [`StreamParser::finish`] consumes it and returns the
/// terminal events plus the classified [`RunSummary`].
pub struct ClaudeParser {
    now: Timestamp,
    tz_offset_minutes: i32,
    provider: String,
    native_session_id: Option<String>,
    pending_limit: Option<LimitEvent>,
    result: Option<ResultLine>,
    cost: Cost,
    noise: String,
    stderr_noise: String,
}

/// The fields of the final `result` line the adapter uses.
#[derive(Debug, Clone)]
struct ResultLine {
    subtype: Option<String>,
    is_error: bool,
    api_error_status: Option<u16>,
    result_text: Option<String>,
    duration_ms: Option<u64>,
    raw: Value,
}

impl ClaudeParser {
    /// Creates a parser for one run.
    pub fn new(now: Timestamp, tz_offset_minutes: i32, executor: vowp_protocol::Executor) -> Self {
        Self {
            now,
            tz_offset_minutes,
            provider: executor.provider(),
            native_session_id: None,
            pending_limit: None,
            result: None,
            cost: Cost::unknown(),
            noise: String::new(),
            stderr_noise: String::new(),
        }
    }

    /// Handles one parsed `stream-json` object.
    fn handle_value(&mut self, value: Value) -> Vec<HarnessEvent> {
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|text| Timestamp::parse(text).ok());

        if self.native_session_id.is_none() {
            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                self.native_session_id = Some(session_id.to_owned());
            }
        }

        match value.get("type").and_then(Value::as_str) {
            Some("system") => system_events(&value, ts),
            Some("assistant") => assistant_events(&value, ts),
            Some("user") => user_events(&value, ts),
            Some("rate_limit_event") => self.rate_limit_events(&value, ts),
            Some("result") => self.result_events(value, ts),
            // `stream_event` deltas and unknown types are ignored; the raw line is teed
            // by the process layer before parsing.
            _ => Vec::new(),
        }
    }

    fn rate_limit_events(&mut self, value: &Value, ts: Option<Timestamp>) -> Vec<HarnessEvent> {
        let info = value.get("rate_limit_info").cloned().unwrap_or(Value::Null);
        match info.get("status").and_then(Value::as_str) {
            Some("rejected") => {
                let rate_type = info.get("rateLimitType").and_then(Value::as_str).unwrap_or("unknown");
                let limit = LimitEvent {
                    kind: LimitKind::Quota,
                    provider: self.provider.clone(),
                    resets_at: reset_from_rate_limit_info(&info),
                    message: redact(&format!("rate_limit_event rejected: {rate_type}"), 300),
                    http_status: None,
                };
                self.pending_limit = Some(limit.clone());
                vec![HarnessEvent {
                    ts,
                    actor: Actor::System,
                    body: EventBody::Limit(limit),
                    raw: Some(raw_json(value)),
                }]
            }
            Some("allowed_warning") => {
                let rate_type = info.get("rateLimitType").and_then(Value::as_str).unwrap_or("unknown");
                let utilization = info
                    .get("utilization")
                    .and_then(Value::as_f64)
                    .or_else(|| first_window_utilization(&info))
                    .unwrap_or(0.0);
                vec![HarnessEvent {
                    ts,
                    actor: Actor::System,
                    body: EventBody::Note(Note {
                        text: format!("claude usage warning: {rate_type} utilization {utilization}"),
                    }),
                    raw: None,
                }]
            }
            // `allowed` and unknown statuses produce no event.
            _ => Vec::new(),
        }
    }

    fn result_events(&mut self, value: Value, ts: Option<Timestamp>) -> Vec<HarnessEvent> {
        let cost = cost_from_result(&value);
        self.cost = cost.clone();
        self.result = Some(ResultLine {
            subtype: value.get("subtype").and_then(Value::as_str).map(str::to_owned),
            is_error: value.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            api_error_status: status_from_value(value.get("api_error_status")),
            result_text: value.get("result").and_then(Value::as_str).map(str::to_owned),
            duration_ms: value.get("duration_ms").and_then(Value::as_u64),
            raw: value,
        });
        vec![HarnessEvent {
            ts,
            actor: Actor::System,
            body: EventBody::Cost(CostEvent { cost, run_id: None }),
            raw: None,
        }]
    }

    /// Chooses the limit event for an error path, applying the documented precedence.
    fn final_limit(&self, classified: Option<Classified>) -> Option<LimitEvent> {
        let mut chosen = match (classified, self.pending_limit.as_ref()) {
            (Some(class), Some(pending)) => match class.kind {
                // Quota is outranked only by Credits and Auth; the structured reset wins.
                LimitKind::Credits | LimitKind::Auth => Some(limit_from_class(class, &self.provider, None)),
                LimitKind::Quota => Some(limit_from_class(class, &self.provider, pending.resets_at)),
                LimitKind::Rate | LimitKind::Context => Some(pending.clone()),
            },
            (Some(class), None) => Some(limit_from_class(class, &self.provider, None)),
            (None, Some(pending)) => Some(pending.clone()),
            (None, None) => None,
        };
        if let Some(limit) = chosen.as_mut() {
            if limit.kind == LimitKind::Quota && limit.resets_at.is_none() {
                limit.resets_at = Some(self.now.plus(std::time::Duration::from_secs(
                    u64::from(crate::classify::QUOTA_FALLBACK_MIN) * 60,
                )));
            }
        }
        chosen
    }

    /// Builds the terminal events and summary for one finished process.
    fn finish_run(self, exit: Option<i32>, killed: bool, stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary) {
        if killed {
            let summary = RunSummary {
                exit_code: exit,
                outcome: RunOutcome::Killed,
                native_session_id: self.native_session_id.clone(),
                cost: self.cost.clone(),
                limit: None,
                result_text: self.result.as_ref().and_then(|line| line.result_text.clone()),
                stop_reason: StopReason::Interrupted,
            };
            return (vec![turn_ended(StopReason::Interrupted, None)], summary);
        }

        let mut events = Vec::new();
        let mut limit: Option<LimitEvent> = None;
        let (outcome, stop_reason, detail) = match &self.result {
            None => match exit {
                Some(0) => (RunOutcome::Error, StopReason::Crash, "no result line".to_owned()),
                _ => {
                    let stderr = if stderr_tail.trim().is_empty() {
                        self.stderr_noise.as_str()
                    } else {
                        stderr_tail
                    };
                    let text = classification_text(&[self.noise.as_str(), stderr]);
                    let status = extract_http_status(&text);
                    match classify_text(&self.provider, &text, status, self.now, self.tz_offset_minutes) {
                        Some(class) => {
                            let chosen = self.final_limit(Some(class));
                            let detail = chosen
                                .as_ref()
                                .map_or_else(|| "no result line".to_owned(), |event| event.message.clone());
                            limit = chosen;
                            let outcome = limit.as_ref().map_or(RunOutcome::Crash, outcome_for_kind);
                            (outcome, stop_reason_for(outcome), detail)
                        }
                        None => {
                            let detail = if text.trim().is_empty() {
                                "no result line".to_owned()
                            } else {
                                redact(&text, 300)
                            };
                            (RunOutcome::Crash, StopReason::Crash, detail)
                        }
                    }
                }
            },
            Some(result) => {
                if result.subtype.as_deref() == Some("error_max_turns") {
                    (
                        RunOutcome::Ok,
                        StopReason::MaxTurns,
                        result.result_text.clone().unwrap_or_default(),
                    )
                } else if !result.is_error
                    && result.subtype.as_deref().is_none_or(|subtype| subtype == "success")
                    && self.pending_limit.is_none()
                {
                    (
                        RunOutcome::Ok,
                        StopReason::EndTurn,
                        result.result_text.clone().unwrap_or_default(),
                    )
                } else {
                    let stderr = if stderr_tail.trim().is_empty() {
                        self.stderr_noise.as_str()
                    } else {
                        stderr_tail
                    };
                    let text = classification_text(&[
                        result.result_text.as_deref().unwrap_or_default(),
                        self.noise.as_str(),
                        stderr,
                    ]);
                    let status = result.api_error_status.or_else(|| extract_http_status(&text));
                    let classified = classify_text(&self.provider, &text, status, self.now, self.tz_offset_minutes);
                    match self.final_limit(classified) {
                        Some(chosen) => {
                            let outcome = outcome_for_kind(&chosen);
                            limit = Some(chosen.clone());
                            (outcome, stop_reason_for(outcome), chosen.message)
                        }
                        None => {
                            let detail = result
                                .result_text
                                .clone()
                                .filter(|text| !text.trim().is_empty())
                                .or_else(|| result.subtype.clone())
                                .unwrap_or_else(|| "harness reported an error".to_owned());
                            (RunOutcome::Error, StopReason::Error, redact(&detail, 300))
                        }
                    }
                }
            }
        };

        if let Some(chosen) = &limit {
            let already_emitted = self
                .pending_limit
                .as_ref()
                .is_some_and(|pending| same_limit(pending, chosen));
            if !already_emitted {
                events.push(HarnessEvent {
                    ts: None,
                    actor: Actor::System,
                    body: EventBody::Limit(chosen.clone()),
                    raw: self.result.as_ref().map(|line| raw_json(&line.raw)),
                });
            }
        }

        if !matches!(outcome, RunOutcome::Ok) {
            let kind = match outcome {
                RunOutcome::Auth => ErrorKind::Auth,
                RunOutcome::Limit | RunOutcome::Rate | RunOutcome::Credits | RunOutcome::Context => ErrorKind::Limit,
                _ => ErrorKind::AdapterFailed,
            };
            events.push(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::Error(ErrorEvent {
                    kind,
                    message: redact(&detail, 300),
                    retryable: matches!(
                        outcome,
                        RunOutcome::Limit | RunOutcome::Rate | RunOutcome::Credits | RunOutcome::Context
                    ),
                }),
                raw: self.result.as_ref().map(|line| raw_json(&line.raw)),
            });
        }

        let duration_ms = self.result.as_ref().and_then(|line| line.duration_ms);
        events.push(turn_ended(stop_reason, duration_ms));

        let result_text = self
            .result
            .as_ref()
            .and_then(|line| line.result_text.clone())
            .or_else(|| (!detail.is_empty()).then(|| redact(&detail, 300)));

        let summary = RunSummary {
            exit_code: exit,
            outcome,
            native_session_id: self.native_session_id.clone(),
            cost: self.cost.clone(),
            limit,
            result_text,
            stop_reason,
        };
        (events, summary)
    }
}

impl StreamParser for ClaudeParser {
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.handle_value(value),
            Err(_) => {
                push_tail(&mut self.noise, line, NOISE_CAP_BYTES);
                Vec::new()
            }
        }
    }

    fn feed_stderr(&mut self, line: &str) {
        push_tail(&mut self.stderr_noise, line, NOISE_CAP_BYTES);
    }

    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary) {
        self.finish_run(exit, killed, stderr_tail)
    }
}

/// Returns the outcome implied by a limit kind.
fn outcome_for_kind(limit: &LimitEvent) -> RunOutcome {
    match limit.kind {
        LimitKind::Quota => RunOutcome::Limit,
        LimitKind::Credits => RunOutcome::Credits,
        LimitKind::Rate => RunOutcome::Rate,
        LimitKind::Context => RunOutcome::Context,
        LimitKind::Auth => RunOutcome::Auth,
    }
}

/// Returns the stop reason implied by a run outcome.
fn stop_reason_for(outcome: RunOutcome) -> StopReason {
    match outcome {
        RunOutcome::Ok => StopReason::EndTurn,
        RunOutcome::Limit | RunOutcome::Rate => StopReason::Limit,
        RunOutcome::Crash => StopReason::Crash,
        RunOutcome::Killed => StopReason::Interrupted,
        RunOutcome::Error | RunOutcome::Credits | RunOutcome::Auth | RunOutcome::Context | RunOutcome::Stalled => {
            StopReason::Error
        }
    }
}

/// Builds a redacted limit event from classification output.
fn limit_from_class(class: Classified, provider: &str, resets_at: Option<Timestamp>) -> LimitEvent {
    LimitEvent {
        kind: class.kind,
        provider: provider.to_owned(),
        resets_at: resets_at.or(class.resets_at),
        message: class.message,
        http_status: class.http_status,
    }
}

/// Returns true when two limit events carry the same classification and reset.
fn same_limit(left: &LimitEvent, right: &LimitEvent) -> bool {
    left.kind == right.kind && left.resets_at == right.resets_at && left.message == right.message
}

/// Joins classifier inputs with a newline; empty parts are skipped.
fn classification_text(parts: &[&str]) -> String {
    let mut text = String::new();
    for part in parts {
        if part.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
    }
    text
}

/// Picks the first window utilization from a `rate_limit_info` object.
fn first_window_utilization(info: &Value) -> Option<f64> {
    let windows = info.get("unifiedWindows")?.as_object()?;
    windows
        .values()
        .find_map(|window| window.get("utilization").and_then(Value::as_f64))
}

/// Reads an HTTP status from a JSON number or numeric string.
fn status_from_value(value: Option<&Value>) -> Option<u16> {
    match value? {
        Value::Number(number) => number.as_u64().and_then(|status| u16::try_from(status).ok()),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}
