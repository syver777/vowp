//! v0.4 local app-surface frames: attach/detach, mirrored events, turns and MCP tool calls.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §5.3, §5.4 and §8.3.

use pretty_assertions::assert_eq;
use serde_json::json;

use vowp_protocol::agent::AgentFrame;
use vowp_protocol::agent::surface::{
    SurfaceAttach, SurfaceDetach, SurfaceEvent, SurfaceTurn, SurfaceTurnState, ToolCall, ToolError, ToolResult,
};
use vowp_protocol::event::{AssistantMessage, EventBody};
use vowp_protocol::ids::{NodeName, SessionId};
use vowp_protocol::surface::{JoinInstructions, OrchestratorApp, OrchestratorCard, OrchestratorNodeState, SurfaceId};
use vowp_protocol::time::Timestamp;

/// A fixed timestamp for frame fixtures.
fn at() -> Timestamp {
    Timestamp::from_unix_secs(1_700_000_000)
}

#[test]
fn agent_frame_round_trips_surface_attach_and_detach() {
    let surface = SurfaceId::new();
    let session = SessionId::new();
    let frames = vec![
        (
            "surface_attach",
            AgentFrame::SurfaceAttach(SurfaceAttach {
                surface: surface.clone(),
                session: session.clone(),
            }),
        ),
        (
            "surface_detach",
            AgentFrame::SurfaceDetach(SurfaceDetach {
                surface: surface.clone(),
                session: session.clone(),
            }),
        ),
    ];

    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("surface attach/detach frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        assert_eq!(value["surface"], json!(surface.0), "surface id for {wire}");
        assert!(value.get("lease_token").is_none(), "the frame carries no lease token");
        let decoded: AgentFrame = serde_json::from_value(value).expect("surface attach/detach frame back");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn surface_event_round_trips_with_a_native_id() {
    let event = SurfaceEvent {
        surface: SurfaceId::new(),
        session: SessionId::new(),
        native_id: "msg_01HZY4Q3V7J8K9M0N1P2Q3R4S5".to_owned(),
        event: EventBody::AssistantMessage(AssistantMessage {
            text: "mirrored once from the app stream".to_owned(),
        }),
    };
    let frame = AgentFrame::SurfaceEvent(event.clone());

    let value = serde_json::to_value(&frame).expect("surface event JSON");
    assert_eq!(value["type"], "surface_event");
    assert_eq!(value["native_id"], event.native_id);
    assert_eq!(value["event"]["type"], "assistant.message");
    assert_eq!(value["event"]["text"], "mirrored once from the app stream");
    assert!(value.get("seq").is_none(), "the agent never sends the home's seq");
    assert!(
        value["event"].get("seq").is_none(),
        "the event body never carries the home's seq"
    );

    let decoded: AgentFrame = serde_json::from_value(value).expect("surface event back");
    assert_eq!(decoded, frame);
    let AgentFrame::SurfaceEvent(decoded) = decoded else {
        panic!("expected a SurfaceEvent frame");
    };
    assert_eq!(decoded.native_id, event.native_id);
    assert_eq!(decoded.event, event.event);

    let direct: SurfaceEvent =
        serde_json::from_value(serde_json::to_value(&event).expect("event JSON")).expect("surface event payload back");
    assert_eq!(direct, event);
}

#[test]
fn surface_turn_round_trips_started_and_ended() {
    for (state, wire) in [
        (SurfaceTurnState::Started, "started"),
        (SurfaceTurnState::Ended, "ended"),
    ] {
        assert_eq!(
            serde_json::to_value(state).expect("turn state JSON"),
            json!(wire),
            "wire name for {state:?}"
        );

        let turn = SurfaceTurn {
            surface: SurfaceId::new(),
            session: SessionId::new(),
            native_id: "turn_7".to_owned(),
            state,
            at: at(),
        };
        let frame = AgentFrame::SurfaceTurn(turn.clone());
        let value = serde_json::to_value(&frame).expect("surface turn JSON");
        assert_eq!(value["type"], "surface_turn");
        assert_eq!(value["state"], wire);
        assert_eq!(value["at"], "2023-11-14T22:13:20.000Z");
        let decoded: AgentFrame = serde_json::from_value(value).expect("surface turn back");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn tool_call_and_result_round_trip() {
    let surface = SurfaceId::new();
    let session = SessionId::new();
    let call = ToolCall {
        request_id: "req-01".to_owned(),
        surface: surface.clone(),
        session: session.clone(),
        tool: "session_tail".to_owned(),
        arguments: json!({"session": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "limit": 5}),
    };
    let call_frame = AgentFrame::ToolCall(call.clone());
    let value = serde_json::to_value(&call_frame).expect("tool call JSON");
    assert_eq!(value["type"], "tool_call");
    assert_eq!(value["request_id"], "req-01");
    assert_eq!(value["tool"], "session_tail");
    assert_eq!(value["arguments"]["limit"], 5);
    let text = serde_json::to_string(&value).expect("tool call text");
    assert!(
        !text.contains("/Users/"),
        "no absolute local path in a tool call: {text}"
    );
    let decoded: AgentFrame = serde_json::from_value(value).expect("tool call back");
    assert_eq!(decoded, call_frame);

    let result = ToolResult {
        request_id: call.request_id.clone(),
        result: Some(json!({"events": [], "as_of": "2023-11-14T22:13:20.000Z"})),
        error: None,
    };
    let result_frame = AgentFrame::ToolResult(result.clone());
    let value = serde_json::to_value(&result_frame).expect("tool result JSON");
    assert_eq!(value["type"], "tool_result");
    assert_eq!(value["request_id"], "req-01");
    assert_eq!(value["result"]["events"], json!([]));
    assert!(value.get("error").is_none(), "the success result carries no error");
    let decoded: AgentFrame = serde_json::from_value(value).expect("tool result back");
    assert_eq!(decoded, result_frame);
}

#[test]
fn tool_result_error_is_typed() {
    let error = ToolError {
        code: "lease_revoked".to_owned(),
        message: "another surface holds the session".to_owned(),
        retryable: false,
    };
    let result = ToolResult {
        request_id: "req-02".to_owned(),
        result: None,
        error: Some(error.clone()),
    };

    let value = serde_json::to_value(AgentFrame::ToolResult(result.clone())).expect("tool error JSON");
    assert!(value.get("result").is_none(), "the error result carries no result");
    assert!(value["error"].is_object(), "the error is an object, not a free string");
    assert_eq!(value["error"]["code"], "lease_revoked");
    assert_eq!(value["error"]["message"], "another surface holds the session");
    assert_eq!(value["error"]["retryable"], false);

    let decoded: AgentFrame = serde_json::from_value(value).expect("tool error back");
    assert_eq!(decoded, AgentFrame::ToolResult(result));

    let bad = serde_json::from_value::<AgentFrame>(json!({
        "type": "tool_result",
        "request_id": "req-03",
        "error": "free string errors are not allowed",
    }))
    .expect_err("an untyped tool error must fail");
    assert!(
        bad.to_string().contains("invalid type") || bad.to_string().contains("expected struct"),
        "error explains the typed shape: {bad}"
    );
}

#[test]
fn orchestrator_card_carries_its_node_and_typed_state() {
    let card = OrchestratorCard {
        name: "mac".to_owned(),
        session_id: SessionId::new(),
        project: None,
        app: OrchestratorApp::Opencode,
        state: OrchestratorNodeState::NodeOffline,
        node: Some(NodeName::parse("mac-studio").expect("node name")),
        last_error: Some("node_offline: node mac-studio is offline".to_owned()),
        join: JoinInstructions {
            desktop: "vowp orchestrator resume mac".to_owned(),
            phone: "vowp chat <session>".to_owned(),
            chat: "vowp chat --agent --session <session>".to_owned(),
        },
    };

    let value = serde_json::to_value(&card).expect("orchestrator card JSON");
    assert_eq!(value["state"], "node_offline");
    assert_eq!(value["node"], "mac-studio");
    assert_eq!(value["last_error"], "node_offline: node mac-studio is offline");
    let text = value.to_string();
    for forbidden in ["/", "token", "secret", "password", "127.0.0.1"] {
        assert!(!text.contains(forbidden), "card carries {forbidden:?}: {text}");
    }
    let decoded: OrchestratorCard = serde_json::from_value(value).expect("orchestrator card back");
    assert_eq!(decoded, card);
}

#[test]
fn orchestrator_card_decodes_a_v0_2_row_without_a_node() {
    let value = json!({
        "name": "home",
        "session_id": SessionId::new(),
        "project": null,
        "app": "chat",
        "state": "running",
        "join": {
            "desktop": "vowp chat <session>",
            "phone": "vowp chat <session>",
            "chat": "vowp chat <session>",
        },
    });
    let card: OrchestratorCard = serde_json::from_value(value).expect("v0.2 card back");
    assert_eq!(card.state, OrchestratorNodeState::Running);
    assert_eq!(card.node, None);
}

#[test]
fn unknown_surface_frame_type_is_rejected() {
    let err = serde_json::from_value::<AgentFrame>(json!({"type": "surface_teleport"}))
        .expect_err("unknown surface frame type must fail");
    assert!(
        err.to_string().contains("surface_teleport"),
        "error names the type: {err}"
    );

    let missing = serde_json::from_value::<AgentFrame>(json!({"type": "surface_attach"}))
        .expect_err("missing surface attach fields must fail");
    assert!(
        missing.to_string().contains("missing field"),
        "error names the missing field: {missing}"
    );

    let bad_state = serde_json::from_value::<AgentFrame>(json!({
        "type": "surface_turn",
        "surface": SurfaceId::new(),
        "session": SessionId::new(),
        "native_id": "turn_1",
        "state": "paused",
        "at": at(),
    }))
    .expect_err("unknown turn state must fail");
    assert!(
        bad_state.to_string().contains("paused"),
        "error names the state: {bad_state}"
    );
}
