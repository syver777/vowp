//! Seed prompt rendering tests: exact handoff text, bounded tail and no native tool replay.
//! Spec: `docs/design/sessions.md` §4 and `docs/design/crates/vowp-core.md` §3 named tests.

use vowp_core::session::seed::seed_prompt;
use vowp_protocol::{
    Actor, AssistantMessage, Capsule, CapsuleId, CapsuleState, CapsuleTrigger, EventBody, EventEnvelope, EventSource,
    Executor, ExecutorRef, FileChange, FileOp, HarnessKind, NodeName, SessionId, Timestamp, ToolCall, ToolResult,
    TurnEnded, TurnStarted, UserMessage,
};

fn ts() -> Timestamp {
    Timestamp::parse("2026-11-08T18:00:00Z").expect("test timestamp")
}

fn executor() -> Executor {
    Executor::parse("claude-code:claude-sonnet-5-5@medium").expect("executor")
}

fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::ClaudeCode,
        model: Some("claude-sonnet-5-5".to_owned()),
        node: Some(NodeName::parse("local").expect("node")),
        native_session_id: Some("native-1".to_owned()),
    }
}

fn event(session: &SessionId, seq: u64, body: EventBody) -> EventEnvelope {
    EventEnvelope::new(session.clone(), seq, ts(), Actor::System, source(), &body)
}

fn turn(session: &SessionId, start: u64, user: &str, assistant: &str) -> Vec<EventEnvelope> {
    vec![
        event(session, start, EventBody::TurnStarted(TurnStarted { turn: 1 })),
        event(
            session,
            start + 1,
            EventBody::UserMessage(UserMessage {
                text: user.to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
        ),
        event(
            session,
            start + 2,
            EventBody::AssistantMessage(AssistantMessage {
                text: assistant.to_owned(),
            }),
        ),
        event(
            session,
            start + 3,
            EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: vowp_protocol::StopReason::EndTurn,
                duration_ms: Some(10),
            }),
        ),
    ]
}

fn capsule(session: &SessionId) -> Capsule {
    Capsule {
        version: vowp_protocol::CAPSULE_VERSION,
        capsule_id: CapsuleId::new(),
        session_id: session.clone(),
        created_at: ts(),
        at_seq: 4,
        trigger: CapsuleTrigger::TurnEnd,
        from: ExecutorRef {
            executor: executor(),
            node: Some(NodeName::parse("local").expect("node")),
        },
        intent: "Finish the session service".to_owned(),
        constraints: vec!["no public actions".to_owned()],
        decisions: Vec::new(),
        state: CapsuleState {
            done: vec!["foundation".to_owned()],
            in_progress: vec!["capsule writer".to_owned()],
            blocked: Vec::new(),
            next_steps: vec!["run tests".to_owned()],
        },
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        telemetry: Default::default(),
    }
}

#[test]
fn seed_prompt_golden_with_two_turn_tail() {
    let session = SessionId::new();
    let capsule = capsule(&session);
    let mut tail = Vec::new();
    tail.extend(turn(&session, 1, "first question", "first answer"));
    let mut second = turn(&session, 5, "second question", "second answer");
    second.insert(
        2,
        event(
            &session,
            7,
            EventBody::ToolCall(ToolCall {
                call_id: "call_1".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
        ),
    );
    tail.extend(second);
    let third = turn(&session, 9, "third question", "third answer");
    tail.extend(third);

    let prompt = seed_prompt(&capsule, &tail, 2, "please continue");

    let expected = format!(
        "[VOWP handoff] You are continuing session {}, started on claude-code:claude-sonnet-5-5@medium. Prior work summary follows.\nTreat it as background; do not restate it. Continue the work and respond to the user's next message below.\n\n{}\n\n## Last turns (plain text)\nuser: second question\n(ran: shell …)\nassistant: second answer\n\nuser: third question\nassistant: third answer\n\n## Next message\nplease continue",
        session,
        capsule.render_markdown().trim_end()
    );
    assert_eq!(prompt, expected);
    assert!(
        !prompt.contains("first question"),
        "only the last two turns are rendered"
    );
}

#[test]
fn seed_prompt_never_contains_tool_calls_as_native() {
    let session = SessionId::new();
    let capsule = capsule(&session);
    let tail = vec![
        event(&session, 1, EventBody::TurnStarted(TurnStarted { turn: 1 })),
        event(
            &session,
            2,
            EventBody::UserMessage(UserMessage {
                text: "inspect".to_owned(),
                attachments: Vec::new(),
                note: None,
            }),
        ),
        event(
            &session,
            3,
            EventBody::ToolCall(ToolCall {
                call_id: "call_secret_id".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"command": "cat secret.txt", "token": "do-not-copy"}),
            }),
        ),
        event(
            &session,
            4,
            EventBody::ToolResult(ToolResult {
                call_id: "call_secret_id".to_owned(),
                ok: true,
                output: Some("SECRET-OUTPUT".to_owned()),
                output_ref: None,
                exit_code: Some(0),
            }),
        ),
        event(
            &session,
            5,
            EventBody::FileChange(FileChange {
                path: "secret.txt".to_owned(),
                op: FileOp::Modify,
                diff_ref: None,
            }),
        ),
        event(
            &session,
            6,
            EventBody::AssistantMessage(AssistantMessage {
                text: "done inspecting".to_owned(),
            }),
        ),
        event(
            &session,
            7,
            EventBody::TurnEnded(TurnEnded {
                turn: 1,
                stop_reason: vowp_protocol::StopReason::EndTurn,
                duration_ms: Some(10),
            }),
        ),
    ];

    let prompt = seed_prompt(&capsule, &tail, 2, "next");
    assert!(prompt.contains("(ran: shell …)"));
    assert!(!prompt.contains("call_secret_id"), "call ids are never replayed");
    assert!(!prompt.contains("do-not-copy"), "tool input is never replayed");
    assert!(!prompt.contains("SECRET-OUTPUT"), "tool output is never replayed");
    assert!(!prompt.contains("tool_calls"));
    assert!(!prompt.contains("\"command\""));
}

#[test]
fn seed_prompt_handles_zero_tail_turns_and_empty_tail() {
    let session = SessionId::new();
    let capsule = capsule(&session);
    let prompt = seed_prompt(&capsule, &[], 0, "go");
    assert!(prompt.contains("## Last turns (plain text)\n(no earlier turns)"));
    assert!(prompt.ends_with("## Next message\ngo"));
}
