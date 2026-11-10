//! Seed text handed to a harness that has no native session for this conversation.
//! Spec: `docs/design/sessions.md` §4.
//!
//! A seed is plain text: the capsule markdown followed by the last complete turns (only
//! user/assistant text and tool *names*), then the next user message. Native tool calls are
//! never replayed.

use vowp_protocol::{Capsule, EventBody, EventEnvelope, EventType};

/// Maximum characters of one rendered turn.
const TURN_MAX_CHARS: usize = 3000;

/// Renders the handoff seeded into a new harness.
pub fn seed_prompt(capsule: &Capsule, tail: &[EventEnvelope], tail_turns: u32, next_input: &str) -> String {
    let turns = render_turns(tail, tail_turns);
    format!(
        "[VOWP handoff] You are continuing session {}, started on {}. Prior work summary follows.\nTreat it as background; do not restate it. Continue the work and respond to the user's next message below.\n\n{}\n\n## Last turns (plain text)\n{}\n\n## Next message\n{}",
        capsule.session_id,
        capsule.from.executor,
        capsule.render_markdown().trim_end(),
        turns,
        next_input
    )
}

/// Renders the last `tail_turns` complete turns from `tail`.
fn render_turns(tail: &[EventEnvelope], tail_turns: u32) -> String {
    if tail_turns == 0 {
        return "(no earlier turns)".to_owned();
    }
    let mut turns: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut in_turn = false;
    let mut complete: Vec<Vec<String>> = Vec::new();
    for event in tail {
        match event.body() {
            Ok(EventBody::TurnStarted(_)) => {
                if in_turn && !current.is_empty() {
                    turns.push(std::mem::take(&mut current));
                }
                current.clear();
                in_turn = true;
            }
            Ok(EventBody::TurnEnded(_)) => {
                if in_turn {
                    if !current.is_empty() {
                        complete.push(std::mem::take(&mut current));
                    }
                    current.clear();
                    in_turn = false;
                }
            }
            Ok(EventBody::UserMessage(payload)) => {
                if payload.note.as_deref() == Some("handoff") {
                    continue;
                }
                if in_turn {
                    current.push(format!("user: {}", clip(&payload.text, TURN_MAX_CHARS)));
                } else if event.event_type == EventType::UserMessage {
                    // A tail without turn boundaries is still rendered as one turn.
                    current.push(format!("user: {}", clip(&payload.text, TURN_MAX_CHARS)));
                    in_turn = true;
                }
            }
            Ok(EventBody::AssistantMessage(payload)) => {
                if in_turn {
                    current.push(format!("assistant: {}", clip(&payload.text, TURN_MAX_CHARS)));
                }
            }
            Ok(EventBody::ToolCall(payload)) if in_turn => {
                current.push(format!("(ran: {} …)", payload.name));
            }
            _ => {}
        }
    }
    if in_turn && !current.is_empty() {
        turns.push(current);
    }
    let mut all = complete;
    all.extend(turns);
    let start = all.len().saturating_sub(tail_turns as usize);
    let rendered: Vec<String> = all[start..]
        .iter()
        .map(|lines| {
            let mut text = lines.join("\n");
            if text.chars().count() > TURN_MAX_CHARS {
                text = clip(&text, TURN_MAX_CHARS);
            }
            text
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    if rendered.is_empty() {
        "(no earlier turns)".to_owned()
    } else {
        rendered.join("\n\n")
    }
}

/// Clips `text` to `max` characters on a character boundary.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}
