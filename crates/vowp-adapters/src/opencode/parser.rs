//! Streaming parser for opencode `run --format json` output.
//! Spec: `docs/design/adapters.md` §3.2–§3.4 and `docs/design/crates/vowp-adapters.md` §4.2.
//!
//! Every stdout line is one JSON object with a top-level `type`, `timestamp` (Unix
//! milliseconds), `sessionID` and a `part`. The parser is pure: it records the session id,
//! maps content parts to events, sums per-step cost receipts and classifies error text in
//! [`StreamParser::finish`].

use serde_json::Value;
use vowp_protocol::{
    Actor, AssistantMessage, AssistantReasoning, Cost, CostEvent, ErrorEvent, ErrorKind, EventBody, Executor,
    FileChange, FileOp, HarnessEvent, LimitEvent, LimitKind, RunOutcome, RunSummary, StopReason, Timestamp, Tokens,
    ToolCall, ToolResult, TurnEnded,
};

use crate::classify::{Classified, classify_text, extract_http_status, redact};
use crate::stream::StreamParser;

/// Serialised `raw` values larger than this are replaced by a truncated marker.
const RAW_CAP_BYTES: usize = 16 * 1024;

/// Bytes of the original raw payload kept in that marker.
const RAW_HEAD_BYTES: usize = 2 * 1024;

/// Bytes of non-JSON stdout kept for classification.
const NOISE_CAP_BYTES: usize = 4 * 1024;

/// Error text collected during the run, with its structured status when one was seen.
struct ErrorSource {
    text: String,
    http_status: Option<u16>,
    raw: Option<Value>,
    ts: Option<Timestamp>,
    from_error_line: bool,
}

/// The strongest classification found in the error sources, with its provenance.
struct Candidate {
    classified: Classified,
    raw: Option<Value>,
    ts: Option<Timestamp>,
    from_error_line: bool,
}

/// Parser for opencode `--format json` lines.
pub struct OpencodeParser {
    now: Timestamp,
    tz_offset_minutes: i32,
    executor: Executor,
    session_id: Option<String>,
    cost: Cost,
    saw_step_finish: bool,
    saw_output: bool,
    result_text: Option<String>,
    max_tokens: bool,
    failed_step: bool,
    error_sources: Vec<ErrorSource>,
    noise: String,
}

impl OpencodeParser {
    /// Creates a parser for one run; `now` anchors rate-limit reset times.
    pub fn new(now: Timestamp, tz_offset_minutes: i32, executor: Executor) -> Self {
        Self {
            now,
            tz_offset_minutes,
            executor,
            session_id: None,
            // A zero receipt is the additive identity for step costs; when no step finish is
            // seen `finish` reports `Cost::unknown()` instead.
            cost: Cost::receipt(0.0, Tokens::default()),
            saw_step_finish: false,
            saw_output: false,
            result_text: None,
            max_tokens: false,
            failed_step: false,
            error_sources: Vec::new(),
            noise: String::new(),
        }
    }

    /// Handles one parsed line.
    fn line_events(&mut self, line: &Value) -> Vec<HarnessEvent> {
        if let Some(session_id) = line.get("sessionID").and_then(Value::as_str) {
            if self.session_id.is_none() {
                self.session_id = Some(session_id.to_owned());
            }
        }

        match line.get("type").and_then(Value::as_str).unwrap_or_default() {
            "step_start" => Vec::new(),
            "tool_use" => self.tool_events(line),
            "text" => self.text_event(line),
            "reasoning" => self.reasoning_event(line),
            "step_finish" => self.step_finish_event(line),
            "error" => self.error_event(line),
            _ => Vec::new(),
        }
    }

    /// Maps one `text` line to an assistant message.
    fn text_event(&mut self, line: &Value) -> Vec<HarnessEvent> {
        let Some(text) = line
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }
        self.result_text = Some(text.to_owned());
        vec![event(
            line_timestamp(line),
            Actor::Assistant,
            EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() }),
            None,
        )]
    }

    /// Maps one `reasoning` line to assistant reasoning.
    fn reasoning_event(&mut self, line: &Value) -> Vec<HarnessEvent> {
        let Some(text) = line
            .get("part")
            .and_then(|part| part.get("text"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }
        vec![event(
            line_timestamp(line),
            Actor::Assistant,
            EventBody::AssistantReasoning(AssistantReasoning { text: text.to_owned() }),
            None,
        )]
    }

    /// Maps one `tool_use` line to a call, a result and optionally a file change.
    fn tool_events(&mut self, line: &Value) -> Vec<HarnessEvent> {
        let Some(part) = line.get("part") else {
            return Vec::new();
        };
        let Some(state) = part.get("state") else {
            return Vec::new();
        };
        let status = state.get("status").and_then(Value::as_str).unwrap_or_default();
        if status != "completed" && status != "error" {
            return Vec::new();
        }

        let Some(call_id) = part
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| part.get("partID").and_then(Value::as_str))
        else {
            return Vec::new();
        };
        let name = part.get("tool").and_then(Value::as_str).unwrap_or_default();
        let input = state.get("input").cloned().unwrap_or(Value::Null);

        let start = state
            .pointer("/time/start")
            .and_then(value_i64)
            .map(Timestamp::from_unix_millis)
            .or_else(|| line_timestamp(line));
        let end = state
            .pointer("/time/end")
            .and_then(value_i64)
            .map(Timestamp::from_unix_millis)
            .or_else(|| line_timestamp(line));

        let (ok, output) = if status == "completed" {
            (true, state.get("output").map(output_text).unwrap_or_default())
        } else {
            (false, state.get("error").map(output_text).unwrap_or_default())
        };
        let exit_code = tool_exit_code(part);
        let raw = raw_value(line);

        let mut events = vec![
            event(
                start,
                Actor::Assistant,
                EventBody::ToolCall(ToolCall {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    input,
                }),
                raw.clone(),
            ),
            event(
                end,
                Actor::Tool,
                EventBody::ToolResult(ToolResult {
                    call_id: call_id.to_owned(),
                    ok,
                    output: Some(output.clone()),
                    output_ref: None,
                    exit_code,
                }),
                raw,
            ),
        ];

        if let Some((path, op)) = file_change(name, state, &output) {
            events.push(event(
                end,
                Actor::Tool,
                EventBody::FileChange(FileChange {
                    path,
                    op,
                    diff_ref: None,
                }),
                None,
            ));
        }
        events
    }

    /// Maps one `step_finish` line to a cost receipt and records its stop reason hint.
    fn step_finish_event(&mut self, line: &Value) -> Vec<HarnessEvent> {
        let Some(part) = line.get("part") else {
            return Vec::new();
        };
        let usd = part.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        let tokens = tokens_from(part.get("tokens"));
        let step_cost = Cost::receipt(usd, tokens);
        self.cost.add(&step_cost);
        self.saw_step_finish = true;

        match part.get("reason").and_then(Value::as_str) {
            Some("tool-calls") => {}
            Some("length") => self.max_tokens = true,
            Some("error") => self.failed_step = true,
            _ => {}
        }

        vec![event(
            line_timestamp(line),
            Actor::System,
            EventBody::Cost(CostEvent {
                cost: step_cost,
                run_id: None,
            }),
            None,
        )]
    }

    /// Maps one `error` line to an error event and remembers it for `finish`.
    fn error_event(&mut self, line: &Value) -> Vec<HarnessEvent> {
        let text = error_text(line);
        let status = extract_http_status(&text).or_else(|| extract_http_status(&line.to_string()));
        let raw = raw_value(line);
        let ts = line_timestamp(line);
        self.error_sources.push(ErrorSource {
            text: text.clone(),
            http_status: status,
            raw: raw.clone(),
            ts,
            from_error_line: true,
        });

        let provider = self.executor.provider();
        let classified = classify_text(&provider, &text, status, self.now, self.tz_offset_minutes);
        let (kind, message, retryable) = match classified {
            Some(classified) => {
                let kind = match classified.kind {
                    LimitKind::Auth => ErrorKind::Auth,
                    LimitKind::Credits | LimitKind::Quota | LimitKind::Rate | LimitKind::Context => ErrorKind::Limit,
                };
                let retryable = classified.kind == LimitKind::Rate;
                (kind, classified.message, retryable)
            }
            None => (ErrorKind::AdapterFailed, redact(&text, 300), false),
        };

        vec![event(
            ts,
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

    /// Classifies every error source and returns the strongest match.
    fn best_classification(&self, stderr_tail: &str) -> Option<Candidate> {
        let provider = self.executor.provider();
        let mut best: Option<Candidate> = None;

        for source in &self.error_sources {
            if let Some(classified) = classify_text(
                &provider,
                &source.text,
                source.http_status,
                self.now,
                self.tz_offset_minutes,
            ) {
                consider_candidate(
                    &mut best,
                    classified,
                    source.raw.clone(),
                    source.ts,
                    source.from_error_line,
                );
            }
        }

        let stderr_status = extract_http_status(stderr_tail);
        if !stderr_tail.trim().is_empty() {
            if let Some(classified) =
                classify_text(&provider, stderr_tail, stderr_status, self.now, self.tz_offset_minutes)
            {
                consider_candidate(&mut best, classified, None, None, false);
            }
        }

        if !self.noise.trim().is_empty() {
            if let Some(classified) = classify_text(
                &provider,
                &self.noise,
                extract_http_status(&self.noise),
                self.now,
                self.tz_offset_minutes,
            ) {
                consider_candidate(&mut best, classified, None, None, false);
            }
        }

        best
    }
}

impl StreamParser for OpencodeParser {
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        if line.is_empty() {
            return Vec::new();
        }
        self.saw_output = true;

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.push_noise(line);
            return Vec::new();
        };
        self.line_events(&value)
    }

    fn finish(self: Box<Self>, exit: Option<i32>, killed: bool, stderr_tail: &str) -> (Vec<HarnessEvent>, RunSummary) {
        let parser = *self;
        let best = parser.best_classification(stderr_tail);
        let mut terminal = Vec::new();

        let (outcome, stop_reason, limit) = if killed {
            (RunOutcome::Killed, StopReason::Interrupted, None)
        } else if let Some(candidate) = best {
            let Candidate {
                classified,
                raw,
                ts,
                from_error_line,
            } = candidate;
            let limit = LimitEvent {
                kind: classified.kind,
                provider: parser.executor.provider(),
                resets_at: classified.resets_at,
                message: classified.message.clone(),
                http_status: classified.http_status,
            };
            if classified.kind == LimitKind::Context && !from_error_line {
                terminal.push(event(
                    ts,
                    Actor::System,
                    EventBody::Error(ErrorEvent {
                        kind: ErrorKind::Limit,
                        message: classified.message.clone(),
                        retryable: false,
                    }),
                    raw.clone(),
                ));
            }
            terminal.push(event(ts, Actor::System, EventBody::Limit(limit.clone()), raw));
            (outcome_for_limit(classified.kind), StopReason::Limit, Some(limit))
        } else if exit == Some(0) {
            if !parser.saw_output {
                (RunOutcome::Error, StopReason::Crash, None)
            } else if parser.failed_step {
                (RunOutcome::Error, StopReason::Error, None)
            } else if parser.max_tokens {
                (RunOutcome::Ok, StopReason::MaxTurns, None)
            } else {
                (RunOutcome::Ok, StopReason::EndTurn, None)
            }
        } else if exit.is_none() {
            (RunOutcome::Crash, StopReason::Crash, None)
        } else {
            (RunOutcome::Error, StopReason::Error, None)
        };

        let result_text = parser.result_text.clone().or_else(|| {
            if outcome == RunOutcome::Error && exit == Some(0) && !parser.saw_output {
                Some("no output".to_owned())
            } else {
                None
            }
        });

        terminal.push(event(
            None,
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
            native_session_id: parser.session_id.clone(),
            cost: if parser.saw_step_finish {
                parser.cost.clone()
            } else {
                Cost::unknown()
            },
            limit,
            result_text,
            stop_reason,
        };
        (terminal, summary)
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

/// Keeps the highest-precedence classification seen so far.
fn consider_candidate(
    best: &mut Option<Candidate>,
    classified: Classified,
    raw: Option<Value>,
    ts: Option<Timestamp>,
    from_error_line: bool,
) {
    if best
        .as_ref()
        .is_none_or(|current| classification_rank(classified.kind) > classification_rank(current.classified.kind))
    {
        *best = Some(Candidate {
            classified,
            raw,
            ts,
            from_error_line,
        });
    }
}

/// Builds one harness event.
fn event(ts: Option<Timestamp>, actor: Actor, body: EventBody, raw: Option<Value>) -> HarnessEvent {
    HarnessEvent { ts, actor, body, raw }
}

/// Reads the top-level millisecond timestamp of a line.
fn line_timestamp(line: &Value) -> Option<Timestamp> {
    line.get("timestamp")
        .and_then(value_i64)
        .map(Timestamp::from_unix_millis)
}

/// Reads an integer from a JSON number or numeric string.
fn value_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    value.as_str()?.parse().ok()
}

/// Reads an unsigned token count, saturating at zero for absent or negative values.
fn token_count(value: Option<&Value>) -> u64 {
    value
        .and_then(value_i64)
        .and_then(|count| u64::try_from(count).ok())
        .unwrap_or(0)
}

/// Reads `part.tokens` into canonical token counts.
fn tokens_from(tokens: Option<&Value>) -> Tokens {
    let Some(tokens) = tokens else {
        return Tokens::default();
    };
    Tokens {
        input: token_count(tokens.get("input")),
        output: token_count(tokens.get("output")),
        reasoning: token_count(tokens.get("reasoning")),
        cache_read: token_count(tokens.pointer("/cache/read")),
        cache_write: token_count(tokens.pointer("/cache/write")),
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

/// Extracts the shell exit code from the real nested pointer, then the older shallow one.
fn tool_exit_code(part: &Value) -> Option<i32> {
    part.pointer("/state/metadata/metadata/exit")
        .or_else(|| part.pointer("/state/metadata/exit"))
        .and_then(value_i64)
        .and_then(|code| i32::try_from(code).ok())
}

/// Recognises best-effort file changes for edit-like tools.
fn file_change(name: &str, state: &Value, output: &str) -> Option<(String, FileOp)> {
    if !matches!(name, "edit" | "write" | "patch" | "multiedit") {
        return None;
    }
    let input = state.get("input")?;
    let path = input
        .get("filePath")
        .and_then(Value::as_str)
        .or_else(|| input.get("path").and_then(Value::as_str))?;
    let op = if name == "write" && output.to_ascii_lowercase().contains("created") {
        FileOp::Create
    } else {
        FileOp::Modify
    };
    Some((path.to_owned(), op))
}

/// Extracts human-readable error text from an `error` line.
fn error_text(line: &Value) -> String {
    for pointer in ["/error/message", "/part/error", "/error", "/part/message", "/message"] {
        let Some(value) = line.pointer(pointer) else {
            continue;
        };
        match value {
            Value::String(text) if !text.is_empty() => return text.clone(),
            Value::Object(object) => {
                if let Some(message) = object.get("message").and_then(Value::as_str) {
                    return message.to_owned();
                }
            }
            _ => {}
        }
    }
    line.to_string()
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

    fn parser() -> OpencodeParser {
        OpencodeParser::new(
            Timestamp::from_unix_secs(0),
            0,
            Executor {
                harness: HarnessKind::Opencode,
                model: "deepseek/deepseek-flash".to_owned(),
                effort: None,
                variant: None,
            },
        )
    }

    #[test]
    fn nested_exit_pointer_wins() {
        let line: Value = serde_json::from_str(
            r#"{"type":"tool_use","timestamp":1,"sessionID":"ses_x","part":{"id":"c1","tool":"shell","state":{"status":"completed","input":{},"output":"ok","metadata":{"metadata":{"exit":7}}}}}"#,
        )
        .expect("invariant: test JSON is valid");
        let events = parser().line_events(&line);
        let result = events
            .iter()
            .find_map(|event| match &event.body {
                EventBody::ToolResult(result) => Some(result),
                _ => None,
            })
            .expect("tool result emitted");
        assert_eq!(result.exit_code, Some(7));
    }

    #[test]
    fn non_json_lines_are_bounded_noise() {
        let mut parser = parser();
        for _ in 0..1000 {
            parser.feed_line("this is not json and it is repeated to fill the buffer");
        }
        assert!(parser.noise.len() <= NOISE_CAP_BYTES);
        assert!(!parser.noise.is_empty());
    }
}
