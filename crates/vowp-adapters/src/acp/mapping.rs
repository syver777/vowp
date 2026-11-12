//! Maps ACP `session/update` notifications and the `session/prompt` result to canonical events.
//! Spec: `docs/design/adapters.md` §10 and `docs/design/v0.2/acp-adapter.md`.
//!
//! The first slice accumulates agent message chunks into one `assistant.message` at turn end,
//! emits tool calls/results and file changes as they complete, maps plan updates, and reports
//! token usage without inventing a price (`CostSource::Unknown`). Permission requests become a
//! note plus a non-retryable error; this module never selects an approval option.

use std::collections::BTreeMap;

use serde_json::Value;
use vowp_protocol::{
    Actor, AssistantMessage, Cost, CostEvent, CostSource, ErrorEvent, ErrorKind, EventBody, Executor, FileChange,
    FileOp, HarnessEvent, LimitEvent, LimitKind, Note, PlanEvent, PlanItem, PlanStatus, RunOutcome, RunSummary,
    StopReason, Timestamp, Tokens, ToolCall, ToolResult, TurnEnded,
};

use crate::classify::{Classified, classify_text, extract_http_status, redact};

/// Serialised `raw` values larger than this are replaced by a truncated marker.
const RAW_CAP_BYTES: usize = 16 * 1024;

/// Bytes of the original payload kept in that marker.
const RAW_HEAD_BYTES: usize = 2 * 1024;

/// Bytes of non-JSON stdout kept for classification.
const NOISE_CAP_BYTES: usize = 4 * 1024;

/// How a conversation ended, used to choose the run outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndKind {
    /// The `session/prompt` response arrived.
    Result,
    /// The server asked for permission; VOWP refused and ended the turn.
    Permission,
    /// A JSON-RPC error response arrived.
    RpcError,
    /// The server violated the protocol (malformed JSON, unknown method, missing session id).
    Protocol,
    /// The server closed stdout without a prompt result.
    Eof,
}

/// One tool call the server has announced.
struct ToolState {
    name: String,
    input: Value,
    done: bool,
}

/// The first protocol/runtime failure, kept as classifier input.
struct Failure {
    text: String,
    http_status: Option<u16>,
    raw: Option<Value>,
}

/// The stop reason reported by the server.
struct PromptResult {
    stop_reason: String,
}

/// State for one ACP prompt turn.
pub(crate) struct AcpTurn {
    executor: Executor,
    now: Timestamp,
    tz_offset_minutes: i32,
    session_id: Option<String>,
    message: String,
    tools: BTreeMap<String, ToolState>,
    tokens: Tokens,
    saw_usage: bool,
    result: Option<PromptResult>,
    permission: Option<String>,
    failure: Option<Failure>,
    error_emitted: bool,
    noise: String,
}

impl AcpTurn {
    /// Creates a turn mapper for one run.
    pub(crate) fn new(executor: Executor, now: Timestamp, tz_offset_minutes: i32) -> Self {
        Self {
            executor,
            now,
            tz_offset_minutes,
            session_id: None,
            message: String::new(),
            tools: BTreeMap::new(),
            tokens: Tokens::default(),
            saw_usage: false,
            result: None,
            permission: None,
            failure: None,
            error_emitted: false,
            noise: String::new(),
        }
    }

    /// Records the native session id from `session/new`.
    pub(crate) fn set_session_id(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    /// Returns true when a prompt result was recorded.
    pub(crate) fn has_result(&self) -> bool {
        self.result.is_some()
    }

    /// Returns true when the server asked for permission and VOWP refused.
    pub(crate) fn permission_refused(&self) -> bool {
        self.permission.is_some()
    }

    /// Maps one `session/update` payload.
    pub(crate) fn on_update(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let Some(update) = params.get("update") else {
            return Vec::new();
        };
        match update.get("sessionUpdate").and_then(Value::as_str).unwrap_or_default() {
            "agent_message_chunk" => {
                self.append_message(update);
                Vec::new()
            }
            "tool_call" | "tool_call_update" => self.tool_events(update),
            "plan" => self.plan_event(update),
            // Reasoning, available commands, modes and context usage are outside the first
            // slice; ignoring them keeps the log small and the capabilities honest.
            _ => Vec::new(),
        }
    }

    /// Maps one `session/request_permission` payload to a note and a refusal.
    ///
    /// The driver answers the JSON-RPC request with `cancelled`; this method never produces an
    /// approval option.
    pub(crate) fn on_permission(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let title = params
            .pointer("/toolCall/title")
            .and_then(Value::as_str)
            .or_else(|| params.pointer("/toolCall/kind").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .unwrap_or("a tool call");
        self.permission = Some(title.to_owned());
        vec![
            event(
                Actor::System,
                EventBody::Note(Note {
                    text: format!("ACP permission requested for {title}; VOWP does not answer permission dialogs"),
                }),
                None,
            ),
            event(
                Actor::System,
                EventBody::Error(ErrorEvent {
                    kind: ErrorKind::Refused,
                    message: format!("ACP permission request for {title} was refused; the turn ended without approval"),
                    retryable: false,
                }),
                raw_value(params),
            ),
        ]
    }

    /// Records a JSON-RPC error response and emits its error event.
    pub(crate) fn on_rpc_error(&mut self, error: &Value) -> Vec<HarnessEvent> {
        let text = rpc_error_text(error);
        let http_status = extract_http_status(&text).or_else(|| extract_http_status(&error.to_string()));
        self.record_failure(&text, http_status, raw_value(error))
    }

    /// Records malformed JSON and emits its error event.
    pub(crate) fn on_malformed(&mut self, line: &str, detail: &str) -> Vec<HarnessEvent> {
        self.push_noise(line);
        let text = format!("ACP server sent malformed JSON: {detail}");
        self.record_failure(&text, None, Some(serde_json::json!({"line": redact(line, 500)})))
    }

    /// Records an unknown JSON-RPC method and emits its error event.
    pub(crate) fn on_unknown_method(&mut self, method: &str, message: &Value) -> Vec<HarnessEvent> {
        let text = format!("ACP server sent an unknown method: {method}");
        self.record_failure(&text, None, raw_value(message))
    }

    /// Records a protocol violation such as a missing session id.
    pub(crate) fn on_protocol(&mut self, text: &str, raw: Option<Value>) -> Vec<HarnessEvent> {
        self.record_failure(text, None, raw)
    }

    /// Records the `session/prompt` result and its token usage.
    pub(crate) fn on_prompt_result(&mut self, result: &Value) -> Vec<HarnessEvent> {
        if let Some(usage) = result.get("usage") {
            self.tokens = tokens_from(usage);
            self.saw_usage = true;
        }
        let Some(stop_reason) = result.get("stopReason").and_then(Value::as_str) else {
            return self.record_failure("ACP prompt result has no stopReason", None, raw_value(result));
        };
        if !matches!(
            stop_reason,
            "end_turn" | "max_tokens" | "max_turn_requests" | "refusal" | "cancelled"
        ) {
            let text = format!("ACP prompt result has an unknown stopReason: {stop_reason}");
            return self.record_failure(&text, None, raw_value(result));
        }
        self.result = Some(PromptResult {
            stop_reason: stop_reason.to_owned(),
        });
        Vec::new()
    }

    /// Builds the terminal events and the classified summary.
    pub(crate) fn finish(
        self,
        end: EndKind,
        killed: bool,
        exit: Option<i32>,
        stderr_tail: &str,
    ) -> (Vec<HarnessEvent>, RunSummary) {
        let mut terminal = Vec::new();
        if !self.message.is_empty() {
            terminal.push(event(
                Actor::Assistant,
                EventBody::AssistantMessage(AssistantMessage {
                    text: self.message.clone(),
                }),
                None,
            ));
        }
        if self.saw_usage && self.tokens.total() > 0 {
            terminal.push(event(
                Actor::System,
                EventBody::Cost(CostEvent {
                    cost: Cost {
                        usd: None,
                        tokens: self.tokens,
                        source: CostSource::Unknown,
                    },
                    run_id: None,
                }),
                None,
            ));
        }

        let classification = if !killed && matches!(end, EndKind::RpcError | EndKind::Protocol | EndKind::Eof) {
            self.best_classification(stderr_tail)
        } else {
            None
        };

        let (outcome, stop_reason, limit) = if killed {
            (RunOutcome::Killed, StopReason::Interrupted, None)
        } else if end == EndKind::Permission {
            (RunOutcome::Error, StopReason::Error, None)
        } else if let Some(classified) = classification {
            let limit = LimitEvent {
                kind: classified.kind,
                provider: self.executor.provider(),
                resets_at: classified.resets_at,
                message: classified.message.clone(),
                http_status: classified.http_status,
            };
            (outcome_for_limit(classified.kind), StopReason::Limit, Some(limit))
        } else {
            match end {
                EndKind::Result => match self.result.as_ref().map(|result| result.stop_reason.as_str()) {
                    Some("end_turn") => (RunOutcome::Ok, StopReason::EndTurn, None),
                    Some("max_tokens" | "max_turn_requests") => (RunOutcome::Ok, StopReason::MaxTurns, None),
                    Some("refusal") => (RunOutcome::Ok, StopReason::EndTurn, None),
                    Some("cancelled") => (RunOutcome::Killed, StopReason::Interrupted, None),
                    _ => (RunOutcome::Error, StopReason::Crash, None),
                },
                EndKind::RpcError => (RunOutcome::Error, StopReason::Error, None),
                EndKind::Protocol | EndKind::Eof => (RunOutcome::Error, StopReason::Crash, None),
                EndKind::Permission => (RunOutcome::Error, StopReason::Error, None),
            }
        };

        if let Some(limit) = &limit {
            let raw = self.failure.as_ref().and_then(|failure| failure.raw.clone());
            terminal.push(event(Actor::System, EventBody::Limit(limit.clone()), raw));
        }
        if !killed
            && end != EndKind::Permission
            && limit.is_none()
            && !self.error_emitted
            && !matches!(outcome, RunOutcome::Ok | RunOutcome::Killed)
        {
            let message = self
                .failure
                .as_ref()
                .map(|failure| failure.text.as_str())
                .unwrap_or("ACP server exited without a prompt result");
            let raw = self.failure.as_ref().and_then(|failure| failure.raw.clone());
            terminal.push(event(
                Actor::System,
                EventBody::Error(ErrorEvent {
                    kind: ErrorKind::AdapterFailed,
                    message: redact(message, 300),
                    retryable: false,
                }),
                raw,
            ));
        }
        terminal.push(event(
            Actor::System,
            EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason,
                duration_ms: None,
            }),
            None,
        ));

        let summary = RunSummary {
            exit_code: exit,
            outcome,
            native_session_id: self.session_id.clone(),
            cost: if self.saw_usage {
                Cost {
                    usd: None,
                    tokens: self.tokens,
                    source: CostSource::Unknown,
                }
            } else {
                Cost::unknown()
            },
            limit,
            result_text: (!self.message.is_empty()).then(|| self.message.clone()),
            stop_reason,
        };
        (terminal, summary)
    }

    /// Appends text blocks from one `agent_message_chunk`.
    fn append_message(&mut self, update: &Value) {
        let Some(content) = update.get("content") else {
            return;
        };
        let text = content_text(content);
        if !text.is_empty() {
            self.message.push_str(&text);
        }
    }

    /// Maps one tool call or tool call update.
    fn tool_events(&mut self, update: &Value) -> Vec<HarnessEvent> {
        let Some(call_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return Vec::new();
        };
        let title = update.get("title").and_then(Value::as_str).unwrap_or_default();
        let kind = update.get("kind").and_then(Value::as_str).unwrap_or_default();
        let first = !self.tools.contains_key(call_id);
        let state = self.tools.entry(call_id.to_owned()).or_insert_with(|| ToolState {
            name: if !kind.is_empty() {
                kind.to_owned()
            } else if !title.is_empty() {
                title.to_owned()
            } else {
                "tool".to_owned()
            },
            input: Value::Null,
            done: false,
        });
        if !kind.is_empty() {
            state.name = kind.to_owned();
        } else if !title.is_empty() && state.name == "tool" {
            state.name = title.to_owned();
        }
        if let Some(input) = update.get("rawInput") {
            state.input = input.clone();
        }

        let mut events = Vec::new();
        if first {
            events.push(event(
                Actor::Assistant,
                EventBody::ToolCall(ToolCall {
                    call_id: call_id.to_owned(),
                    name: state.name.clone(),
                    input: state.input.clone(),
                }),
                raw_value(update),
            ));
        }

        let status = update.get("status").and_then(Value::as_str).unwrap_or_default();
        if matches!(status, "completed" | "failed") && !state.done {
            state.done = true;
            let ok = status == "completed";
            let output = tool_output(update);
            let exit_code = tool_exit_code(update);
            events.push(event(
                Actor::Tool,
                EventBody::ToolResult(ToolResult {
                    call_id: call_id.to_owned(),
                    ok,
                    output: Some(output),
                    output_ref: None,
                    exit_code,
                }),
                raw_value(update),
            ));
            if let Some((path, op)) = file_change(update, &state.name) {
                events.push(event(
                    Actor::Tool,
                    EventBody::FileChange(FileChange {
                        path,
                        op,
                        diff_ref: None,
                    }),
                    None,
                ));
            }
        }
        events
    }

    /// Maps one plan update.
    fn plan_event(&self, update: &Value) -> Vec<HarnessEvent> {
        let Some(entries) = update.get("entries").and_then(Value::as_array) else {
            return Vec::new();
        };
        let items: Vec<PlanItem> = entries
            .iter()
            .filter_map(|entry| {
                let text = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| entry.get("text").and_then(Value::as_str))?;
                let status = match entry.get("status").and_then(Value::as_str) {
                    Some("completed") => PlanStatus::Done,
                    Some("in_progress") => PlanStatus::InProgress,
                    _ => PlanStatus::Pending,
                };
                Some(PlanItem {
                    text: text.to_owned(),
                    status,
                })
            })
            .collect();
        if items.is_empty() {
            return Vec::new();
        }
        vec![event(Actor::Assistant, EventBody::Plan(PlanEvent { items }), None)]
    }

    /// Records a failure and emits its error event.
    fn record_failure(&mut self, text: &str, http_status: Option<u16>, raw: Option<Value>) -> Vec<HarnessEvent> {
        if self.failure.is_none() {
            self.failure = Some(Failure {
                text: text.to_owned(),
                http_status,
                raw: raw.clone(),
            });
        }
        let provider = self.executor.provider();
        let (kind, message, retryable) =
            match classify_text(&provider, text, http_status, self.now, self.tz_offset_minutes) {
                Some(classified) => {
                    let kind = match classified.kind {
                        LimitKind::Auth => ErrorKind::Auth,
                        LimitKind::Credits | LimitKind::Quota | LimitKind::Rate | LimitKind::Context => {
                            ErrorKind::Limit
                        }
                    };
                    (kind, classified.message, classified.kind == LimitKind::Rate)
                }
                None => (ErrorKind::AdapterFailed, redact(text, 300), false),
            };
        self.error_emitted = true;
        vec![event(
            Actor::System,
            EventBody::Error(ErrorEvent {
                kind,
                message,
                retryable,
            }),
            raw,
        )]
    }

    /// Appends a non-JSON line to the bounded noise buffer.
    fn push_noise(&mut self, line: &str) {
        if self.noise.len() >= NOISE_CAP_BYTES {
            return;
        }
        self.noise.push_str(line);
        self.noise.push('\n');
        cap_string(&mut self.noise, NOISE_CAP_BYTES);
    }

    /// Classifies the recorded failure, stderr and noise, keeping the strongest class.
    fn best_classification(&self, stderr_tail: &str) -> Option<Classified> {
        let provider = self.executor.provider();
        let now = self.now;
        let tz_offset_minutes = self.tz_offset_minutes;
        let mut best: Option<Classified> = None;
        let mut consider = |text: &str, http_status: Option<u16>| {
            let Some(classified) = classify_text(&provider, text, http_status, now, tz_offset_minutes) else {
                return;
            };
            if best
                .as_ref()
                .is_none_or(|current| classification_rank(classified.kind) > classification_rank(current.kind))
            {
                best = Some(classified);
            }
        };
        if let Some(failure) = &self.failure {
            consider(&failure.text, failure.http_status);
        }
        if !stderr_tail.trim().is_empty() {
            consider(stderr_tail, extract_http_status(stderr_tail));
        }
        if !self.noise.trim().is_empty() {
            consider(&self.noise, extract_http_status(&self.noise));
        }
        best
    }
}

/// The run outcome implied by a classified limit.
fn outcome_for_limit(kind: LimitKind) -> RunOutcome {
    match kind {
        LimitKind::Quota => RunOutcome::Limit,
        LimitKind::Credits => RunOutcome::Credits,
        LimitKind::Rate => RunOutcome::Rate,
        LimitKind::Context => RunOutcome::Context,
        LimitKind::Auth => RunOutcome::Auth,
    }
}

/// Classification precedence: credits > auth > quota > rate > context.
fn classification_rank(kind: LimitKind) -> u8 {
    match kind {
        LimitKind::Credits => 5,
        LimitKind::Auth => 4,
        LimitKind::Quota => 3,
        LimitKind::Rate => 2,
        LimitKind::Context => 1,
    }
}

/// Builds one harness event; ACP updates carry no timestamps.
fn event(actor: Actor, body: EventBody, raw: Option<Value>) -> HarnessEvent {
    HarnessEvent {
        ts: None,
        actor,
        body,
        raw,
    }
}

/// Extracts text blocks from an ACP content block (or an array of them).
fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(content_text).collect::<Vec<_>>().join(""),
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("text") {
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Renders a tool output: strings verbatim, other JSON as its compact text.
fn output_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Extracts a tool output from `rawOutput` or the structured content blocks.
fn tool_output(update: &Value) -> String {
    if let Some(raw) = update.get("rawOutput") {
        return output_text(raw);
    }
    let Some(content) = update.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("content") => item.get("content").map(output_text).filter(|text| !text.is_empty()),
            Some("diff") => item.get("newText").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reads a shell exit code from a tool's raw output, when the server reported one.
fn tool_exit_code(update: &Value) -> Option<i32> {
    update
        .pointer("/rawOutput/exitCode")
        .or_else(|| update.pointer("/rawOutput/exit_code"))
        .and_then(value_i64)
        .and_then(|code| i32::try_from(code).ok())
}

/// Recognises a file change for edit-like ACP tool kinds.
fn file_change(update: &Value, name: &str) -> Option<(String, FileOp)> {
    let op = match name {
        "edit" => FileOp::Modify,
        "delete" => FileOp::Delete,
        "move" => FileOp::Rename,
        _ => return None,
    };
    let path = update
        .pointer("/locations/0/path")
        .and_then(Value::as_str)
        .or_else(|| update.pointer("/rawInput/path").and_then(Value::as_str))
        .or_else(|| update.pointer("/rawInput/filePath").and_then(Value::as_str))
        .or_else(|| update.pointer("/rawInput/file_path").and_then(Value::as_str))?;
    Some((path.to_owned(), op))
}

/// Reads ACP's camelCase token usage into canonical counts.
fn tokens_from(usage: &Value) -> Tokens {
    Tokens {
        input: token_count(usage.get("inputTokens")),
        output: token_count(usage.get("outputTokens")),
        reasoning: token_count(usage.get("thoughtTokens")).max(token_count(usage.get("reasoningTokens"))),
        cache_read: token_count(usage.get("cachedReadTokens")),
        cache_write: token_count(usage.get("cachedWriteTokens")),
    }
}

/// Reads an unsigned count, treating absent or negative values as zero.
fn token_count(value: Option<&Value>) -> u64 {
    value
        .and_then(value_i64)
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0)
}

/// Reads an integer from a JSON number or numeric string.
fn value_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    value.as_str()?.parse().ok()
}

/// Extracts a human-readable message from a JSON-RPC error object.
fn rpc_error_text(error: &Value) -> String {
    let message = error.get("message").and_then(Value::as_str).unwrap_or_default();
    let data = error
        .get("data")
        .map(|data| match data {
            Value::String(text) => text.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .unwrap_or_default();
    match (message.is_empty(), data.is_empty()) {
        (false, false) => format!("{message}: {data}"),
        (false, true) => message.to_owned(),
        (true, false) => data,
        (true, true) => error.to_string(),
    }
}

/// Copies a JSON value with secret-looking string leaves stripped.
fn raw_value(value: &Value) -> Option<Value> {
    let sanitised = redact_value(value);
    let text = serde_json::to_string(&sanitised).ok()?;
    if text.len() <= RAW_CAP_BYTES {
        return Some(sanitised);
    }
    let head = text
        .chars()
        .scan(0usize, |used, ch| {
            *used += ch.len_utf8();
            (*used <= RAW_HEAD_BYTES).then_some(ch)
        })
        .collect::<String>();
    Some(serde_json::json!({
        "truncated": true,
        "bytes": text.len(),
        "head": head,
    }))
}

/// Recursively applies `redact` to string leaves of a JSON value.
fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text, text.chars().count().max(1))),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Truncates a string to at most `max` bytes on a character boundary.
fn cap_string(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vowp_protocol::HarnessKind;

    fn turn() -> AcpTurn {
        AcpTurn::new(
            Executor {
                harness: HarnessKind::Acp,
                model: "fixture".to_owned(),
                effort: None,
                variant: None,
            },
            Timestamp::from_unix_secs(0),
            0,
        )
    }

    fn update(value: Value) -> Value {
        serde_json::json!({"sessionId": "sess_1", "update": value})
    }

    #[test]
    fn message_chunks_accumulate_into_one_message() {
        let mut turn = turn();
        assert!(
            turn.on_update(&update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Hello "}
            })))
            .is_empty()
        );
        assert!(
            turn.on_update(&update(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "ACP"}
            })))
            .is_empty()
        );
        let (events, summary) = turn.finish(EndKind::Result, false, Some(0), "");
        let message = events
            .iter()
            .find_map(|event| match &event.body {
                EventBody::AssistantMessage(message) => Some(message.text.clone()),
                _ => None,
            })
            .expect("one assistant message");
        assert_eq!(message, "Hello ACP");
        assert_eq!(summary.result_text.as_deref(), Some("Hello ACP"));
    }

    #[test]
    fn permission_request_is_a_note_and_a_refusal() {
        let mut turn = turn();
        let events = turn.on_permission(&serde_json::json!({
            "toolCall": {"toolCallId": "t1", "title": "Write a file", "kind": "edit"},
            "options": [{"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"}]
        }));
        assert!(matches!(events[0].body, EventBody::Note(_)));
        let EventBody::Error(error) = &events[1].body else {
            panic!("the permission request is an error");
        };
        assert_eq!(error.kind, ErrorKind::Refused);
        assert!(!error.retryable);
        assert!(turn.permission_refused());
        let (events, summary) = turn.finish(EndKind::Permission, false, Some(0), "");
        assert_eq!(summary.outcome, RunOutcome::Error);
        assert_eq!(summary.stop_reason, StopReason::Error);
        assert!(events.iter().all(|event| !matches!(event.body, EventBody::ToolCall(_))));
    }

    #[test]
    fn plan_entries_map_their_status() {
        let mut turn = turn();
        let events = turn.on_update(&update(serde_json::json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "one", "status": "completed"},
                {"content": "two", "status": "in_progress"},
                {"content": "three", "status": "pending"}
            ]
        })));
        let EventBody::Plan(plan) = &events[0].body else {
            panic!("one plan event");
        };
        let statuses: Vec<PlanStatus> = plan.items.iter().map(|item| item.status).collect();
        assert_eq!(
            statuses,
            vec![PlanStatus::Done, PlanStatus::InProgress, PlanStatus::Pending]
        );
    }
}
