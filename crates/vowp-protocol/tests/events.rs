//! Event envelope, payload, type-name and summary tests.
//! Spec: `docs/design/crates/vowp-protocol.md` §6 and `docs/design/types.md` §5.

use std::collections::BTreeSet;

use pretty_assertions::assert_eq;
use serde_json::json;
use vowp_protocol::prelude::*;

fn source() -> EventSource {
    EventSource {
        harness: HarnessKind::Vowp,
        model: None,
        node: None,
        native_session_id: None,
    }
}

fn sample_time() -> Timestamp {
    Timestamp::parse("2026-03-10T19:41:09.000Z").expect("sample timestamp must parse")
}

fn sample_node(name: &str) -> NodeName {
    NodeName::parse(name).expect("sample node must parse")
}

fn sample_executor(text: &str) -> Executor {
    Executor::parse(text).expect("sample executor must parse")
}

/// One sample payload for every [`EventType`], in declaration order.
fn sample_bodies() -> Vec<EventBody> {
    vec![
        EventBody::SessionCreated(SessionCreated {
            title: "sample-job".to_owned(),
            cwd: "/home/user/project".to_owned(),
            project: Some(ProjectName::parse("vowp").expect("sample project must parse")),
            job_id: Some(JobId::parse("sample-job").expect("sample job must parse")),
            role: Some(SessionRole::Worker),
            parent_session_id: None,
        }),
        EventBody::SessionExecutor(SessionExecutor {
            action: AttachAction::Attach,
            executor: sample_executor("opencode:example/model#max"),
            node: sample_node("vps"),
            native_session_id: Some("ses_example000000000000000".to_owned()),
            run_id: Some(RunId::new()),
            reason: Some("resume".to_owned()),
        }),
        EventBody::TurnStarted(TurnStarted { turn: 3 }),
        EventBody::TurnEnded(TurnEnded {
            turn: 3,
            stop_reason: StopReason::EndTurn,
            duration_ms: Some(1_234),
        }),
        EventBody::UserMessage(UserMessage {
            text: "implement the events module".to_owned(),
            attachments: vec![BlobRef {
                sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned(),
                bytes: 12,
                preview: "hello world\n".to_owned(),
            }],
            note: Some("handoff".to_owned()),
        }),
        EventBody::AssistantMessage(AssistantMessage {
            text: "done".to_owned(),
        }),
        EventBody::AssistantReasoning(AssistantReasoning {
            text: "consider the roundtrip".to_owned(),
        }),
        EventBody::ToolCall(ToolCall {
            call_id: "call_01_2ncr".to_owned(),
            name: "shell".to_owned(),
            input: json!({"command": "cargo test", "timeout_ms": 120000}),
        }),
        EventBody::ToolResult(ToolResult {
            call_id: "call_01_2ncr".to_owned(),
            ok: true,
            output: Some("all tests passed".to_owned()),
            output_ref: None,
            exit_code: Some(0),
        }),
        EventBody::FileChange(FileChange {
            path: "crates/vowp-protocol/src/event.rs".to_owned(),
            op: FileOp::Modify,
            diff_ref: Some(BlobRef {
                sha256: "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_owned(),
                bytes: 4096,
                preview: "+ new event payloads\n".to_owned(),
            }),
        }),
        EventBody::Plan(PlanEvent {
            items: vec![
                PlanItem {
                    text: "write payloads".to_owned(),
                    status: PlanStatus::Done,
                },
                PlanItem {
                    text: "write tests".to_owned(),
                    status: PlanStatus::InProgress,
                },
                PlanItem {
                    text: "run gates".to_owned(),
                    status: PlanStatus::Pending,
                },
            ],
        }),
        EventBody::Limit(LimitEvent {
            kind: LimitKind::Quota,
            provider: "claude".to_owned(),
            resets_at: Some(sample_time()),
            message: "You've hit your limit · resets 11pm (Europe/Oslo)".to_owned(),
            http_status: Some(429),
        }),
        EventBody::Cost(CostEvent {
            cost: Cost::receipt(
                0.21,
                Tokens {
                    input: 84_000,
                    output: 9_100,
                    ..Tokens::default()
                },
            ),
            run_id: Some(RunId::new()),
        }),
        EventBody::Switch(SwitchEvent {
            switch_id: SwitchId::new(),
            from: Some(ExecutorRef {
                executor: sample_executor("claude-code:example-model@medium"),
                node: Some(sample_node("vps")),
            }),
            to: ExecutorRef {
                executor: sample_executor("opencode:example/model#max"),
                node: Some(sample_node("local")),
            },
            capsule_id: Some(CapsuleId::new()),
            reason: SwitchReason::Limit,
        }),
        EventBody::Capsule(CapsuleEvent {
            capsule_id: CapsuleId::new(),
            version: 1,
            trigger: CapsuleTrigger::Switch,
            at_seq: 42,
        }),
        EventBody::Error(ErrorEvent {
            kind: ErrorKind::Limit,
            message: "quota exhausted".to_owned(),
            retryable: true,
        }),
        EventBody::Note(Note {
            text: "gate output looks clean".to_owned(),
        }),
    ]
}

fn all_event_type_strings() -> BTreeSet<&'static str> {
    [
        "session.created",
        "session.executor",
        "turn.started",
        "turn.ended",
        "user.message",
        "assistant.message",
        "assistant.reasoning",
        "tool.call",
        "tool.result",
        "file.change",
        "plan",
        "limit",
        "cost",
        "switch",
        "capsule",
        "error",
        "note",
    ]
    .into()
}

#[test]
fn event_body_roundtrip_every_type() {
    let bodies = sample_bodies();
    assert_eq!(bodies.len(), 17, "one sample per event type");

    let seen: BTreeSet<&'static str> = bodies.iter().map(|body| body.event_type().as_str()).collect();
    assert_eq!(seen, all_event_type_strings());

    for body in &bodies {
        let value = body.to_value();
        let decoded = EventBody::from_parts(body.event_type(), value)
            .unwrap_or_else(|err| panic!("{} must decode: {err}", body.event_type()));
        assert_eq!(&decoded, body, "roundtrip for {}", body.event_type());
        assert_eq!(decoded.event_type(), body.event_type());
    }

    // Extra payload keys are ignored for forward compatibility (rule 1).
    let with_extra = json!({"text": "hello", "future_field": {"nested": true}});
    assert_eq!(
        EventBody::from_parts(EventType::Note, with_extra).expect("extra keys must be ignored"),
        EventBody::Note(Note {
            text: "hello".to_owned()
        })
    );

    // Missing required payload fields are rejected, not defaulted silently.
    assert!(EventBody::from_parts(EventType::UserMessage, json!({})).is_err());
}

#[test]
fn event_type_strings_stable() {
    // The test fails (rather than falling back) when the schema file or its CHECK list is gone.
    let schema = include_str!("../../../docs/design/schema.sql");
    let table_offset = schema
        .find("CREATE TABLE session_events")
        .expect("docs/design/schema.sql must define session_events");
    let table = &schema[table_offset..];
    let check = "CHECK (type IN";
    let check_offset = table.find(check).expect("session_events.type must have a CHECK list");
    let after_check = &table[check_offset + check.len()..];
    let open = after_check
        .find('(')
        .expect("session_events.type CHECK list must be parenthesised");
    let list = &after_check[open + 1..];
    let close = list
        .find(')')
        .expect("session_events.type CHECK list must be terminated");
    let schema_names: BTreeSet<&str> = list[..close]
        .split(',')
        .map(|name| name.trim().trim_matches('\''))
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(
        schema_names,
        all_event_type_strings(),
        "schema.sql session_events.type CHECK list must match EventType"
    );

    for text in all_event_type_strings() {
        let event_type = EventType::parse(text).expect("every dotted name must parse");
        assert_eq!(event_type.as_str(), text);
        assert_eq!(event_type.to_string(), text);
        assert_eq!(
            serde_json::to_value(event_type).expect("event type must serialise"),
            json!(text)
        );
        assert_eq!(
            serde_json::from_value::<EventType>(json!(text)).expect("event type must deserialise"),
            event_type
        );
    }

    assert!(EventType::parse("mystery").is_err());
    assert!(serde_json::from_value::<EventType>(json!("mystery")).is_err());
}

#[test]
fn envelope_new_sets_replayable() {
    let session_id = SessionId::new();
    let ts = sample_time();
    let mut ids = BTreeSet::new();

    for body in sample_bodies() {
        let envelope = EventEnvelope::new(session_id.clone(), 7, ts, Actor::Assistant, source(), &body);
        let expected_replayable = !matches!(body, EventBody::AssistantReasoning(_));
        assert_eq!(envelope.replayable, expected_replayable, "{}", body.event_type());
        assert_eq!(envelope.event_type, body.event_type());
        assert_eq!(envelope.payload, body.to_value());
        assert_eq!(envelope.seq, 7);
        assert_eq!(envelope.ts, ts);
        assert_eq!(envelope.actor, Actor::Assistant);
        assert_eq!(envelope.body().expect("typed body must decode"), body);
        assert!(ids.insert(envelope.id.clone()), "every envelope gets a fresh id");

        let json = serde_json::to_value(&envelope).expect("envelope must serialise");
        assert!(json.get("raw").is_none(), "raw must be omitted when it is None");
        assert_eq!(json["replayable"], json!(expected_replayable));
    }
}

#[test]
fn envelope_forward_compat() {
    let session_id = SessionId::new();
    let envelope_json = json!({
        "id": EventId::new(),
        "session_id": session_id,
        "seq": 12,
        "ts": "2026-03-10T19:41:09.000Z",
        "type": "error",
        "actor": "system",
        "source": {"harness": "vowp"},
        "payload": {
            "kind": "io",
            "message": "disk full",
            "retryable": true,
            "fields_from_a_later_version": [1, 2, 3]
        },
        "replayable": true,
        "top_level_from_a_later_version": {"ignored": true}
    });

    let envelope: EventEnvelope = serde_json::from_value(envelope_json).expect("extra top-level keys must be ignored");
    assert_eq!(envelope.seq, 12);
    assert_eq!(
        envelope.body().expect("extra payload keys must be ignored"),
        EventBody::Error(ErrorEvent {
            kind: ErrorKind::Io,
            message: "disk full".to_owned(),
            retryable: true,
        })
    );

    let mut unknown_type = serde_json::to_value(&envelope).expect("envelope must serialise");
    unknown_type["type"] = json!("mystery");
    assert!(
        serde_json::from_value::<EventEnvelope>(unknown_type).is_err(),
        "unknown event types must be rejected"
    );
}

#[test]
fn envelope_json_matches_fixture() {
    let fixture = include_str!("fixtures/event_log_lines.jsonl");
    let mut lines = 0;
    for line in fixture.lines().filter(|line| !line.trim().is_empty()) {
        let original: serde_json::Value = serde_json::from_str(line).expect("fixture line must be JSON");
        let envelope: EventEnvelope = serde_json::from_value(original.clone()).expect("fixture line must decode");
        let body = envelope.body().expect("fixture payload must match its type");
        assert_eq!(body.event_type(), envelope.event_type);
        let roundtripped = serde_json::to_value(&envelope).expect("envelope must serialise");
        assert_eq!(roundtripped, original, "fixture line changed: {line}");
        lines += 1;
    }
    assert_eq!(lines, 3, "the spec fixture has three example log lines");
}

#[test]
fn harness_event_summary_len() {
    let multiline = (0..400)
        .map(|index| format!("line {index}: the quick brown fox jumps over the lazy dog\n"))
        .collect::<String>();
    assert!(multiline.len() > 10_000, "sample must be large");

    let event = HarnessEvent {
        ts: Some(sample_time()),
        actor: Actor::Assistant,
        body: EventBody::UserMessage(UserMessage {
            text: multiline.clone(),
            attachments: Vec::new(),
            note: None,
        }),
        raw: None,
    };

    let summaries = [
        event.body.summary(),
        EventBody::AssistantMessage(AssistantMessage {
            text: multiline.clone(),
        })
        .summary(),
        EventBody::AssistantReasoning(AssistantReasoning {
            text: multiline.clone(),
        })
        .summary(),
        EventBody::ToolCall(ToolCall {
            call_id: "call_01_2ncr".to_owned(),
            name: "shell".to_owned(),
            input: json!({"command": multiline}),
        })
        .summary(),
        EventBody::Note(Note {
            text: multiline.clone(),
        })
        .summary(),
    ];

    for summary in summaries {
        assert!(!summary.is_empty());
        assert!(
            summary.chars().count() <= 200,
            "summary is {} chars: {summary}",
            summary.chars().count()
        );
        assert!(!summary.contains('\n'), "summary must be one line: {summary}");
        assert!(!summary.contains('\r'), "summary must be one line: {summary}");
    }

    // Every payload type keeps the same bound, not just the long-text ones above.
    for body in sample_bodies() {
        let summary = body.summary();
        assert!(!summary.is_empty(), "{} has no summary", body.event_type());
        assert!(
            summary.chars().count() <= 200,
            "{} summary is {} chars",
            body.event_type(),
            summary.chars().count()
        );
        assert!(
            !summary.contains('\n') && !summary.contains('\r'),
            "{} summary is not one line: {summary}",
            body.event_type()
        );
    }
}

#[test]
fn session_created_user_role_roundtrips() {
    let payload = SessionCreated {
        title: "interactive".to_owned(),
        cwd: "/home/user/project".to_owned(),
        project: None,
        job_id: None,
        role: Some(SessionRole::User),
        parent_session_id: None,
    };

    let value = serde_json::to_value(&payload).expect("session.created must serialise");
    assert_eq!(value["role"], json!("user"));

    let decoded: SessionCreated = serde_json::from_value(value).expect("session.created must deserialise");
    assert_eq!(decoded, payload);
    assert_eq!(decoded.role, Some(SessionRole::User));

    // The typed event body decodes the same wire form.
    let body = EventBody::SessionCreated(decoded);
    assert_eq!(
        EventBody::from_parts(EventType::SessionCreated, body.to_value()).expect("body must decode"),
        body
    );
}
