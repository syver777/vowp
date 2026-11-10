//! JSON-pointer, stream parsing and classification for the generic CLI adapter.
//! Spec: `docs/design/adapters.md` §4.3 and `docs/design/crates/vowp-adapters.md` §4.3.
//!
//! [`CliParser`] turns one harness run into canonical events without touching the process:
//! the shared `proc::RunHandle` reads stdout and calls [`CliParser::feed_line`]. On exit it
//! emits at most one cost event, one limit or error event and exactly one `turn.ended`.

use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};
use vowp_protocol::{
    Actor, AdapterError, AssistantMessage, Cost, CostEvent, ErrorEvent, ErrorKind, EventBody, EventType, HarnessEvent,
    LimitEvent, LimitKind, Note, RunOutcome, RunSummary, StopReason, Timestamp, Tokens, TurnEnded,
};

use crate::classify::{extract_http_status, parse_reset_time, redact};
use crate::stream::StreamParser;

use super::mapping::{MapRule, Mapping, OutputFormat, ResetKind, allowed_emit, required_fields};

/// Maximum number of bytes kept from non-JSON stdout for classification.
const NOISE_TAIL_BYTES: usize = 4096;

/// Serialised `raw` values larger than this are replaced by a truncated marker.
const RAW_CAP_BYTES: usize = 16 * 1024;

/// Bytes of the original raw payload kept in that marker.
const RAW_HEAD_BYTES: usize = 2 * 1024;

/// Evaluates an RFC 6901 JSON pointer (`"/a/0/b"`; `""` = the whole value).
///
/// Returns `None` for a missing object key, an out-of-range or non-numeric array index,
/// a pointer that does not start with `/`, or a scalar reached before the pointer ends.
pub fn pointer_get<'a>(value: &'a Value, pointer: &str) -> Option<&'a Value> {
    if pointer.is_empty() {
        return Some(value);
    }
    let rest = pointer.strip_prefix('/')?;
    let mut current = value;
    for token in rest.split('/') {
        let token = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items) => {
                if token == "-" {
                    return None;
                }
                let index: usize = token.parse().ok()?;
                items.get(index)?
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => return None,
        };
    }
    Some(current)
}

/// One parsed limit classification.
struct Classification {
    kind: LimitKind,
    resets_at: Option<Timestamp>,
    message: String,
}

/// Compiled limit patterns and reset extractor for one mapping.
struct CompiledLimits {
    credits: Vec<Regex>,
    auth: Vec<Regex>,
    quota: Vec<Regex>,
    rate: Vec<Regex>,
    context: Vec<Regex>,
    reset: Option<(Regex, ResetKind)>,
    tz_offset_minutes: i32,
}

/// Parser for a mapped CLI harness.
pub struct CliParser {
    mapping: Mapping,
    now: Timestamp,
    rule_regexes: Vec<Option<Regex>>,
    limits: CompiledLimits,
    chunk_lines: usize,
    text_lines: Vec<String>,
    native_session_id: Option<String>,
    last_assistant: Option<String>,
    usd: Option<f64>,
    tokens: Tokens,
    noise: String,
    error_messages: Vec<String>,
    saw_error: bool,
    last_json: Option<Value>,
}

impl CliParser {
    /// Creates a parser from a validated mapping.
    ///
    /// Regexes are compiled here; an invalid pattern is an [`AdapterError::Mapping`].
    pub fn new(mapping: Mapping, now: Timestamp) -> Result<Self, AdapterError> {
        let rule_regexes = mapping
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| match &rule.match_.regex {
                Some(pattern) => RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map(Some)
                    .map_err(|error| {
                        mapping_error(format!(
                            "map rule {} match.regex {pattern:?} does not compile: {error}",
                            index + 1
                        ))
                    }),
                None => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let limits = CompiledLimits {
            credits: compile_list("limits.credits", &mapping.limits.credits)?,
            auth: compile_list("limits.auth", &mapping.limits.auth)?,
            quota: compile_list("limits.quota", &mapping.limits.quota)?,
            rate: compile_list("limits.rate", &mapping.limits.rate)?,
            context: compile_list("limits.context", &mapping.limits.context)?,
            reset: match &mapping.limits.resets_at {
                Some(spec) => {
                    let regex = RegexBuilder::new(&spec.regex)
                        .case_insensitive(true)
                        .build()
                        .map_err(|error| {
                            mapping_error(format!(
                                "limits.resets_at.regex {:?} does not compile: {error}",
                                spec.regex
                            ))
                        })?;
                    Some((regex, spec.kind))
                }
                None => None,
            },
            tz_offset_minutes: mapping.limits.tz_offset_minutes,
        };

        Ok(Self {
            chunk_lines: mapping.output.text_chunk_lines.max(1) as usize,
            mapping,
            now,
            rule_regexes,
            limits,
            text_lines: Vec::new(),
            native_session_id: None,
            last_assistant: None,
            usd: None,
            tokens: Tokens::default(),
            noise: String::new(),
            error_messages: Vec::new(),
            saw_error: false,
            last_json: None,
        })
    }

    /// Feeds one JSON line: records the native id and cost, then fires every matching rule.
    fn feed_json(&mut self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.push_noise(line);
            return Vec::new();
        };
        self.last_json = Some(value.clone());

        self.update_native_id(&value);
        self.update_cost(&value);

        let mut events = Vec::new();
        for (index, rule) in self.mapping.rules.iter().enumerate() {
            if !rule_matches(rule, self.rule_regexes[index].as_ref(), &value) {
                continue;
            }
            match build_event(rule, &value) {
                Ok(event) => events.push(event),
                Err(RuleError::Missing(field)) => {
                    events.push(note(format!("mapping: rule {} missing {field}", index + 1)))
                }
                Err(RuleError::Invalid(message)) => {
                    events.push(note(format!("mapping: rule {} invalid: {message}", index + 1)))
                }
            }
        }

        for event in &events {
            match &event.body {
                EventBody::Error(error) => {
                    self.saw_error = true;
                    self.error_messages.push(error.message.clone());
                }
                EventBody::AssistantMessage(message) => {
                    self.last_assistant = Some(message.text.clone());
                }
                _ => {}
            }
        }
        events
    }

    /// Records the first string (or number) found at the native-session pointer.
    fn update_native_id(&mut self, value: &Value) {
        if self.native_session_id.is_some() {
            return;
        }
        let Some(pointer) = self.mapping.output.native_session_pointer.as_deref() else {
            return;
        };
        if let Some(found) = pointer_get(value, pointer) {
            self.native_session_id = match found {
                Value::String(text) if !text.is_empty() => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            };
        }
    }

    /// Applies every cost pointer; the last value seen wins.
    fn update_cost(&mut self, value: &Value) {
        if let Some(pointer) = &self.mapping.cost.usd_pointer {
            if let Some(usd) = pointer_get(value, pointer).and_then(Value::as_f64) {
                self.usd = Some(usd);
            }
        }
        update_tokens(value, &self.mapping.cost.input_pointer, &mut self.tokens.input);
        update_tokens(value, &self.mapping.cost.output_pointer, &mut self.tokens.output);
        update_tokens(value, &self.mapping.cost.reasoning_pointer, &mut self.tokens.reasoning);
        update_tokens(
            value,
            &self.mapping.cost.cache_read_pointer,
            &mut self.tokens.cache_read,
        );
        update_tokens(
            value,
            &self.mapping.cost.cache_write_pointer,
            &mut self.tokens.cache_write,
        );
    }

    /// Appends a non-JSON line to the bounded noise tail.
    fn push_noise(&mut self, line: &str) {
        self.noise.push_str(line);
        self.noise.push('\n');
        if self.noise.len() > NOISE_TAIL_BYTES {
            let mut start = self.noise.len() - NOISE_TAIL_BYTES;
            while !self.noise.is_char_boundary(start) {
                start += 1;
            }
            self.noise.drain(..start);
        }
    }

    /// Emits the buffered text lines as one assistant message.
    fn flush_text(&mut self) -> Vec<HarnessEvent> {
        if self.text_lines.is_empty() {
            return Vec::new();
        }
        let text = self.text_lines.join("\n");
        self.text_lines.clear();
        self.last_assistant = Some(text.clone());
        vec![assistant_message(text)]
    }

    /// Classifies `text` with the mapping's lists, in precedence order.
    fn classify(&self, text: &str) -> Option<Classification> {
        let lists = [
            (&self.limits.credits, LimitKind::Credits),
            (&self.limits.auth, LimitKind::Auth),
            (&self.limits.quota, LimitKind::Quota),
            (&self.limits.rate, LimitKind::Rate),
            (&self.limits.context, LimitKind::Context),
        ];
        for (patterns, kind) in lists {
            if patterns.iter().any(|pattern| pattern.is_match(text)) {
                return Some(Classification {
                    kind,
                    resets_at: self.reset_at(text),
                    message: self.limit_message(text),
                });
            }
        }
        None
    }

    /// Extracts a reset time from `text` with the optional `[limits.resets_at]` regex.
    fn reset_at(&self, text: &str) -> Option<Timestamp> {
        let (regex, kind) = self.limits.reset.as_ref()?;
        let captures = regex.captures(text)?;
        let raw = captures
            .get(1)
            .or_else(|| captures.get(0))
            .map(|matched| matched.as_str())?;
        match kind {
            ResetKind::Unix => raw.trim().parse::<i64>().ok().map(Timestamp::from_unix_secs),
            ResetKind::Clock => parse_reset_time(raw, self.now, self.limits.tz_offset_minutes),
        }
    }

    /// Builds the redacted provider-prefixed message for a limit event.
    fn limit_message(&self, text: &str) -> String {
        let provider = self.provider();
        redact(&format!("{provider}: {text}"), 300)
    }

    /// Returns the provider key: the configured one, else the mapping name.
    fn provider(&self) -> &str {
        self.mapping.provider.as_deref().unwrap_or(&self.mapping.name)
    }

    /// Builds the diagnostic text for an error event.
    fn error_detail(&self, exit: Option<i32>, stderr_tail: &str) -> String {
        let mut parts = Vec::new();
        if !self.error_messages.is_empty() {
            parts.push(self.error_messages.join("; "));
        }
        if !stderr_tail.trim().is_empty() {
            parts.push(stderr_tail.trim().to_owned());
        }
        if !self.noise.trim().is_empty() {
            parts.push(self.noise.trim().to_owned());
        }
        if parts.is_empty() {
            parts.push(match exit {
                Some(code) => format!("exit code {code}"),
                None => "process terminated by signal".to_owned(),
            });
        }
        redact(&parts.join("; "), 300)
    }
}

impl StreamParser for CliParser {
    fn feed_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        match self.mapping.output.format {
            OutputFormat::Text => {
                self.text_lines.push(line.to_owned());
                if self.text_lines.len() >= self.chunk_lines {
                    self.flush_text()
                } else {
                    Vec::new()
                }
            }
            OutputFormat::Jsonl => self.feed_json(line),
        }
    }

    fn finish(
        mut self: Box<Self>,
        exit: Option<i32>,
        killed: bool,
        stderr_tail: &str,
    ) -> (Vec<HarnessEvent>, RunSummary) {
        let mut events = Vec::new();
        if self.mapping.output.format == OutputFormat::Text {
            events.extend(self.flush_text());
        }

        let mut cost = Cost::unknown();
        cost.tokens = self.tokens;
        if let Some(usd) = self.usd {
            cost = Cost::receipt(usd, self.tokens);
        }

        let success = exit.is_some_and(|code| self.mapping.exit.ok.contains(&code));
        let limit_code = exit.is_some_and(|code| self.mapping.exit.limit.contains(&code));
        let stderr_present = !stderr_tail.trim().is_empty();
        let noise_signal = !self.noise.trim().is_empty() && self.classify(&self.noise).is_some();
        let should_classify = limit_code
            || !success
            || self.saw_error
            || noise_signal
            || (self.mapping.exit.error_from_stderr && stderr_present);

        let mut text = String::new();
        if !success || self.mapping.exit.error_from_stderr {
            text.push_str(stderr_tail);
        }
        if !self.noise.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.noise);
        }
        if !self.error_messages.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.error_messages.join("\n"));
        }

        let classified = if should_classify { self.classify(&text) } else { None };
        let limit = if killed {
            None
        } else if limit_code {
            let kind = classified
                .as_ref()
                .map_or(LimitKind::Quota, |classification| classification.kind);
            let resets_at = classified
                .as_ref()
                .and_then(|classification| classification.resets_at)
                .or_else(|| self.reset_at(&text));
            let message = classified.map_or_else(
                || {
                    let code = exit.map_or_else(|| "signal".to_owned(), |code| code.to_string());
                    self.limit_message(&format!("exit code {code}"))
                },
                |classification| classification.message,
            );
            Some(LimitEvent {
                kind,
                provider: self.provider().to_owned(),
                resets_at,
                message,
                http_status: extract_http_status(&text),
            })
        } else {
            classified.map(|classification| LimitEvent {
                kind: classification.kind,
                provider: self.provider().to_owned(),
                resets_at: classification.resets_at,
                message: classification.message,
                http_status: extract_http_status(&text),
            })
        };

        let (outcome, stop_reason) = if killed {
            (RunOutcome::Killed, StopReason::Interrupted)
        } else if let Some(limit) = &limit {
            (outcome_for(limit.kind), StopReason::Limit)
        } else if success {
            (RunOutcome::Ok, StopReason::EndTurn)
        } else if exit.is_none() {
            (RunOutcome::Crash, StopReason::Crash)
        } else {
            (RunOutcome::Error, StopReason::Error)
        };

        let terminal_raw = self.last_json.as_ref().map(raw_json);

        if cost.usd.is_some() {
            events.push(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::Cost(CostEvent {
                    cost: cost.clone(),
                    run_id: None,
                }),
                raw: None,
            });
        }
        if let Some(limit) = &limit {
            if limit.kind == LimitKind::Context && !self.saw_error {
                events.push(HarnessEvent {
                    ts: None,
                    actor: Actor::System,
                    body: EventBody::Error(ErrorEvent {
                        kind: ErrorKind::Limit,
                        message: limit.message.clone(),
                        retryable: false,
                    }),
                    raw: terminal_raw.clone(),
                });
            }
            events.push(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::Limit(limit.clone()),
                raw: terminal_raw.clone(),
            });
        }
        if matches!(outcome, RunOutcome::Error | RunOutcome::Crash) {
            events.push(HarnessEvent {
                ts: None,
                actor: Actor::System,
                body: EventBody::Error(ErrorEvent {
                    kind: ErrorKind::AdapterFailed,
                    message: self.error_detail(exit, stderr_tail),
                    retryable: false,
                }),
                raw: terminal_raw,
            });
        }
        events.push(HarnessEvent {
            ts: None,
            actor: Actor::System,
            body: EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason,
                duration_ms: None,
            }),
            raw: None,
        });

        let summary = RunSummary {
            exit_code: exit,
            outcome,
            native_session_id: self.native_session_id.clone(),
            cost,
            limit,
            result_text: self.last_assistant.clone(),
            stop_reason,
        };
        (events, summary)
    }
}

/// One rule could not produce its event.
enum RuleError {
    /// A required payload field was absent from the line.
    Missing(String),
    /// The payload did not decode as the requested event type.
    Invalid(String),
}

/// Returns true when `rule` selects `value`.
fn rule_matches(rule: &MapRule, regex: Option<&Regex>, value: &Value) -> bool {
    let found = pointer_get(value, &rule.match_.pointer);
    if let Some(expected) = &rule.match_.equals {
        return match found {
            Some(Value::String(actual)) => actual == expected,
            Some(other) => other == &literal_value(expected),
            None => false,
        };
    }
    if let Some(regex) = regex {
        return matches!(found, Some(Value::String(actual)) if regex.is_match(actual));
    }
    if let Some(expected) = rule.match_.exists {
        return found.is_some() == expected;
    }
    false
}

/// Builds one event from a matching rule.
fn build_event(rule: &MapRule, value: &Value) -> Result<HarnessEvent, RuleError> {
    let Some(event_type) = allowed_emit(&rule.emit) else {
        return Err(RuleError::Invalid(format!("unknown emit {:?}", rule.emit)));
    };

    let mut payload = Map::new();
    for (field, spec) in &rule.fields {
        if let Some(literal) = spec.strip_prefix('=') {
            payload.insert(field.clone(), literal_value(literal));
        } else if let Some(found) = pointer_get(value, spec) {
            payload.insert(field.clone(), found.clone());
        }
    }
    for field in required_fields(event_type) {
        if !payload.contains_key(*field) {
            return Err(RuleError::Missing((*field).to_owned()));
        }
    }
    // `error` mappings only require `message` (adapters.md §4.1); the protocol payload also
    // has a `kind`, so supply the documented generic class when the mapping omits it.
    if event_type == EventType::Error && !payload.contains_key("kind") {
        let kind =
            serde_json::to_value(ErrorKind::AdapterFailed).map_err(|error| RuleError::Invalid(error.to_string()))?;
        payload.insert("kind".to_owned(), kind);
    }

    let body = EventBody::from_parts(event_type, Value::Object(payload))
        .map_err(|error| RuleError::Invalid(error.to_string()))?;
    let raw = matches!(
        event_type,
        EventType::ToolCall | EventType::ToolResult | EventType::Error
    )
    .then(|| raw_json(value));
    Ok(HarnessEvent {
        ts: None,
        actor: actor_for(event_type),
        body,
        raw,
    })
}

/// Redacts secret-looking string leaves and caps the serialised `raw` value at 16 KiB.
fn raw_json(value: &Value) -> Value {
    let redacted = redact_value(value);
    let serialised = serde_json::to_string(&redacted).unwrap_or_default();
    let bytes = serialised.len();
    if bytes <= RAW_CAP_BYTES {
        return redacted;
    }
    let mut end = RAW_HEAD_BYTES.min(serialised.len());
    while end > 0 && !serialised.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({
        "truncated": true,
        "bytes": bytes,
        "head": &serialised[..end],
    })
}

/// Recursively redacts every string leaf of a JSON value.
fn redact_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(text, text.chars().count().max(1))),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), redact_value(item)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// Parses a `=literal` field value as JSON when possible, else as a plain string.
fn literal_value(text: &str) -> Value {
    if text.is_empty() {
        return Value::String(String::new());
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// Chooses the actor for an emitted event type.
fn actor_for(event_type: EventType) -> Actor {
    match event_type {
        EventType::AssistantMessage | EventType::AssistantReasoning | EventType::Plan => Actor::Assistant,
        EventType::ToolCall | EventType::ToolResult | EventType::FileChange => Actor::Tool,
        _ => Actor::System,
    }
}

/// Maps a limit kind to the run outcome it implies.
fn outcome_for(kind: LimitKind) -> RunOutcome {
    match kind {
        LimitKind::Quota => RunOutcome::Limit,
        LimitKind::Credits => RunOutcome::Credits,
        LimitKind::Auth => RunOutcome::Auth,
        LimitKind::Rate => RunOutcome::Rate,
        LimitKind::Context => RunOutcome::Context,
    }
}

/// Creates a system note event.
fn note(text: String) -> HarnessEvent {
    HarnessEvent {
        ts: None,
        actor: Actor::System,
        body: EventBody::Note(Note { text }),
        raw: None,
    }
}

/// Creates an assistant message event.
fn assistant_message(text: String) -> HarnessEvent {
    HarnessEvent {
        ts: None,
        actor: Actor::Assistant,
        body: EventBody::AssistantMessage(AssistantMessage { text }),
        raw: None,
    }
}

/// Updates one token counter from its pointer.
fn update_tokens(value: &Value, pointer: &Option<String>, target: &mut u64) {
    let Some(pointer) = pointer else {
        return;
    };
    if let Some(count) = pointer_get(value, pointer).and_then(json_u64) {
        *target = count;
    }
}

/// Reads a non-negative integer from a JSON number.
fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().map(|number| number.max(0) as u64))
        .or_else(|| {
            value
                .as_f64()
                .filter(|number| *number >= 0.0)
                .map(|number| number as u64)
        })
}

/// Compiles a limit list with the case-insensitive flag applied.
fn compile_list(label: &str, patterns: &[String]) -> Result<Vec<Regex>, AdapterError> {
    patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|error| mapping_error(format!("{label}[{index}] {pattern:?} does not compile: {error}")))
        })
        .collect()
}

/// Builds a mapping error for `CliParser::new`.
fn mapping_error(message: String) -> AdapterError {
    AdapterError::Mapping {
        path: "<mapping>".to_owned(),
        message,
    }
}

/// The `(\d+\.\d+\.\d+)` version pattern shared by probes.
pub(super) fn version_regex() -> &'static Regex {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    VERSION.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+)").expect("invariant: version regex literal is valid"))
}
