//! Stream-json block mapping and raw/cost helpers for the Claude parser.
//! Spec: `docs/design/adapters.md` §2.2, §6 and §7.
//!
//! Pure functions that turn one JSON object or content block into canonical events,
//! build the cost receipt, and bound/redact the display-only `raw` value.

use serde_json::{Map, Value};
use vowp_protocol::{
    Actor, AssistantMessage, AssistantReasoning, Cost, EventBody, HarnessEvent, Note, Timestamp, Tokens, ToolCall,
    ToolResult, TurnEnded,
};

use super::parser::{RAW_CAP_BYTES, RAW_HEAD_BYTES};
use crate::classify::redact;

/// Maps one `system` line to a note, or nothing for framing/noisy subtypes.
pub(super) fn system_events(value: &Value, ts: Option<Timestamp>) -> Vec<HarnessEvent> {
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or_default();
    if subtype.is_empty() || subtype == "init" || matches!(subtype, "hook_started" | "hook_response") {
        return Vec::new();
    }
    vec![HarnessEvent {
        ts,
        actor: Actor::System,
        body: EventBody::Note(Note {
            text: format!("system:{subtype}"),
        }),
        raw: None,
    }]
}

/// Maps one `assistant` line's content blocks to canonical events.
pub(super) fn assistant_events(value: &Value, ts: Option<Timestamp>) -> Vec<HarnessEvent> {
    let Some(content) = value.pointer("/message/content") else {
        return Vec::new();
    };
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return Vec::new();
        }
        return vec![HarnessEvent {
            ts,
            actor: Actor::Assistant,
            body: EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() }),
            raw: None,
        }];
    }
    let Value::Array(blocks) = content else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(HarnessEvent {
                            ts,
                            actor: Actor::Assistant,
                            body: EventBody::AssistantMessage(AssistantMessage { text: text.to_owned() }),
                            raw: None,
                        });
                    }
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(HarnessEvent {
                            ts,
                            actor: Actor::Assistant,
                            body: EventBody::AssistantReasoning(AssistantReasoning { text: text.to_owned() }),
                            raw: None,
                        });
                    }
                }
            }
            Some("tool_use") => {
                let Some(call_id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                events.push(HarnessEvent {
                    ts,
                    actor: Actor::Assistant,
                    body: EventBody::ToolCall(ToolCall {
                        call_id: call_id.to_owned(),
                        name,
                        input,
                    }),
                    raw: Some(raw_json(block)),
                });
            }
            // Unknown block types are ignored (opaque signatures are never stored).
            _ => {}
        }
    }
    events
}

/// Maps one `user` line's `tool_result` blocks to canonical events.
pub(super) fn user_events(value: &Value, ts: Option<Timestamp>) -> Vec<HarnessEvent> {
    let Some(Value::Array(blocks)) = value.pointer("/message/content") else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(call_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let ok = !block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let output = match block.get("content") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Array(parts)) => {
                let texts = parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>();
                Some(texts.join("\n"))
            }
            _ => None,
        };
        events.push(HarnessEvent {
            ts,
            actor: Actor::Tool,
            body: EventBody::ToolResult(ToolResult {
                call_id: call_id.to_owned(),
                ok,
                output,
                output_ref: None,
                exit_code: None,
            }),
            raw: Some(raw_json(block)),
        });
    }
    events
}

/// Builds a terminal `turn.ended` event.
pub(super) fn turn_ended(stop_reason: vowp_protocol::StopReason, duration_ms: Option<u64>) -> HarnessEvent {
    HarnessEvent {
        ts: None,
        actor: Actor::System,
        body: EventBody::TurnEnded(TurnEnded {
            turn: 1,
            stop_reason,
            duration_ms,
        }),
        raw: None,
    }
}

/// Builds the cost receipt from a `result` line (adapters.md §6).
pub(super) fn cost_from_result(value: &Value) -> Cost {
    let usage = value.get("usage");
    let thinking = usage
        .and_then(|usage| usage.pointer("/output_tokens_details/thinking_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens = Tokens {
        input: usage
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: output_tokens.saturating_sub(thinking),
        reasoning: thinking,
        cache_read: usage
            .and_then(|usage| usage.get("cache_read_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write: usage
            .and_then(|usage| usage.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    match value.get("total_cost_usd").and_then(Value::as_f64) {
        Some(usd) => Cost::receipt(usd, tokens),
        None => Cost {
            usd: None,
            tokens,
            source: vowp_protocol::CostSource::Unknown,
        },
    }
}

/// Appends a line to a bounded noise buffer, keeping the last `cap` bytes on a char boundary.
pub(super) fn push_tail(buffer: &mut String, line: &str, cap: usize) {
    buffer.push_str(line);
    buffer.push('\n');
    if buffer.len() <= cap {
        return;
    }
    let mut cut = buffer.len() - cap;
    while cut < buffer.len() && !buffer.is_char_boundary(cut) {
        cut += 1;
    }
    buffer.drain(..cut);
}

/// Redacts secret-looking string leaves and caps the serialised `raw` value at 16 KiB.
pub(super) fn raw_json(value: &Value) -> Value {
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
