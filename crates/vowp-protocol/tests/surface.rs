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
use vowp_protocol::surface::{
    EndpointAccess, JoinInstructions, OrchestratorApp, OrchestratorAttach, OrchestratorAuth, OrchestratorCard,
    OrchestratorEndpoint, OrchestratorLocation, OrchestratorNodeState, OwnershipMode, OwnershipState, SessionOwner,
    SurfaceActivity, SurfaceId, SurfaceKind,
};
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
fn orchestrator_endpoint_round_trips_home_and_node_documents() {
    // Assemble the marker at runtime so no secret-shaped literal is committed.
    let password = format!("vowp-endpoint-{}", "R".repeat(24));
    let session = SessionId::new();
    let home = OrchestratorEndpoint {
        name: "home-main".to_owned(),
        app: OrchestratorApp::Opencode,
        session_id: session.clone(),
        location: OrchestratorLocation::Home,
        node: None,
        state: OrchestratorNodeState::Running,
        url: Some("http://127.0.0.1:54321".to_owned()),
        native_session_id: Some("ses_native_1".to_owned()),
        auth: Some(OrchestratorAuth {
            header: "Authorization".to_owned(),
            scheme: "Basic".to_owned(),
            username: "opencode".to_owned(),
            password: password.clone(),
        }),
        owner: None,
        access: EndpointAccess::ReadOnly,
    };
    let node = OrchestratorEndpoint {
        name: "mac-main".to_owned(),
        app: OrchestratorApp::Opencode,
        session_id: session,
        location: OrchestratorLocation::Node,
        node: Some(NodeName::parse("mac-studio").expect("node name")),
        state: OrchestratorNodeState::Running,
        url: None,
        native_session_id: None,
        auth: None,
        owner: None,
        access: EndpointAccess::ReadOnly,
    };

    for (endpoint, location) in [(&home, "home"), (&node, "node")] {
        let value = serde_json::to_value(endpoint).expect("orchestrator endpoint JSON");
        assert_eq!(value["name"], endpoint.name, "name for {location}");
        assert_eq!(value["location"], location, "location wire name");
        assert_eq!(
            value["session_id"],
            serde_json::to_value(&endpoint.session_id).expect("session id JSON")
        );
        let decoded: OrchestratorEndpoint = serde_json::from_value(value).expect("orchestrator endpoint back");
        assert_eq!(&decoded, endpoint, "round trip for {location}");
    }

    let home_value = serde_json::to_value(&home).expect("home endpoint JSON");
    assert_eq!(home_value["url"], "http://127.0.0.1:54321");
    assert_eq!(home_value["auth"]["header"], "Authorization");
    assert_eq!(home_value["auth"]["scheme"], "Basic");
    assert_eq!(home_value["auth"]["username"], "opencode");
    assert_eq!(home_value["auth"]["password"], password);

    let node_value = serde_json::to_value(&node).expect("node endpoint JSON");
    assert_eq!(node_value["node"], "mac-studio");
    assert!(node_value["url"].is_null(), "a node document has no url: {node_value}");
    assert!(
        node_value["native_session_id"].is_null(),
        "a node document has no native id: {node_value}"
    );
    assert!(
        node_value["auth"].is_null(),
        "a node document has no auth: {node_value}"
    );
}

#[test]
fn orchestrator_endpoint_schema_is_exported_and_auth_debug_redacts_the_password() {
    use vowp_protocol::schema::schema_for_name;

    let password = format!("vowp-endpoint-{}", "S".repeat(24));
    let auth = OrchestratorAuth {
        header: "Authorization".to_owned(),
        scheme: "Basic".to_owned(),
        username: "opencode".to_owned(),
        password: password.clone(),
    };
    let debug = format!("{auth:?}");
    assert!(debug.contains("[REDACTED]"), "debug redacts the password: {debug}");
    assert!(!debug.contains(&password), "debug never carries the password");
    assert!(debug.contains("Authorization") && debug.contains("opencode"), "{debug}");

    let endpoint = OrchestratorEndpoint {
        name: "main".to_owned(),
        app: OrchestratorApp::Opencode,
        session_id: SessionId::new(),
        location: OrchestratorLocation::Home,
        node: None,
        state: OrchestratorNodeState::Running,
        url: Some("http://127.0.0.1:1234".to_owned()),
        native_session_id: None,
        auth: Some(auth),
        owner: None,
        access: EndpointAccess::ReadOnly,
    };
    let debug = format!("{endpoint:?}");
    assert!(!debug.contains(&password), "endpoint debug never carries the password");

    for name in [
        "OrchestratorLocation",
        "OrchestratorAuth",
        "OrchestratorEndpoint",
        "EndpointAccess",
        "OrchestratorAttach",
    ] {
        let schema = schema_for_name(name).unwrap_or_else(|| panic!("{name} is exported"));
        assert_eq!(schema["title"], name, "{name} schema title");
    }
    let auth_schema = schema_for_name("OrchestratorAuth").expect("auth schema");
    let required: Vec<&str> = auth_schema["required"]
        .as_array()
        .expect("required is an array")
        .iter()
        .map(|name| name.as_str().expect("required names are strings"))
        .collect();
    for field in ["header", "scheme", "username", "password"] {
        assert!(required.contains(&field), "the auth schema must require {field}");
    }
    let endpoint_schema = schema_for_name("OrchestratorEndpoint").expect("endpoint schema");
    let properties = endpoint_schema["properties"]
        .as_object()
        .expect("endpoint schema properties");
    for field in [
        "name",
        "app",
        "session_id",
        "location",
        "node",
        "state",
        "url",
        "native_session_id",
        "auth",
        "owner",
        "access",
    ] {
        assert!(
            properties.contains_key(field),
            "the endpoint schema must define {field}"
        );
    }
    let location_schema = schema_for_name("OrchestratorLocation").expect("location schema");
    let variants = location_schema["oneOf"]
        .as_array()
        .expect("location oneOf values")
        .iter()
        .filter_map(|value| value["const"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(variants, vec!["home", "node"]);
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

#[test]
fn surface_kind_desktop_ui_round_trips_as_desktop_ui() {
    use vowp_protocol::surface::SurfaceKind;

    let value = serde_json::to_value(SurfaceKind::DesktopUi).expect("kind JSON");
    assert_eq!(value, json!("desktop_ui"));
    let back: SurfaceKind = serde_json::from_value(json!("desktop_ui")).expect("kind back");
    assert_eq!(back, SurfaceKind::DesktopUi);
    assert_eq!(SurfaceKind::DesktopUi.store_name(), "desktop-ui");
    assert_eq!(SurfaceKind::ClaudeCode.store_name(), "claude-code");
    assert!(
        serde_json::from_value::<SurfaceKind>(json!("desktop-ui")).is_err(),
        "the hyphenated spelling is the store/CLI name, not a wire value"
    );
}

#[test]
fn lease_owner_fields_default_for_an_old_document() {
    use vowp_protocol::surface::{LeaseStatus, OwnershipMode, OwnershipState, SessionOwner, SurfaceActivity};

    let old = json!({
        "held": true,
        "holder": "laptop",
        "kind": "chat",
        "node": "home",
        "acquired_at": "2023-11-14T22:13:20Z",
        "heartbeat_at": "2023-11-14T22:13:20Z",
        "token": null
    });
    let status: LeaseStatus = serde_json::from_value(old).expect("old LeaseStatus parses");
    assert_eq!(status.mode, None);
    assert_eq!(status.state, None);
    assert_eq!(status.epoch, None);
    assert_eq!(status.expires_at, None);
    assert_eq!(status.label, None);
    assert_eq!(status.activity, None);

    let session = SessionId::new();
    let owner: SessionOwner = serde_json::from_value(json!({
        "session_id": session.to_string(),
        "surface_id": null,
        "kind": "agent",
        "holder": "agent",
        "node": "mac",
        "acquired_at": "2023-11-14T22:13:20Z",
        "heartbeat_at": "2023-11-14T22:13:20Z"
    }))
    .expect("an owner document with only the required fields parses");
    assert_eq!(owner.mode, OwnershipMode::Interactive);
    assert_eq!(owner.state, OwnershipState::Active);
    assert_eq!(owner.activity, SurfaceActivity::Idle);
    assert_eq!(owner.epoch, 0);
    assert_eq!(owner.label, None);
    assert_eq!(owner.expires_at, None);
}

#[test]
fn ownership_enums_use_snake_case_wire_names() {
    use vowp_protocol::surface::{OwnershipMode, OwnershipState, SurfaceActivity};

    assert_eq!(serde_json::to_value(OwnershipMode::Job).expect("mode"), json!("job"));
    assert_eq!(
        serde_json::to_value(OwnershipState::Expired).expect("state"),
        json!("expired")
    );
    assert_eq!(
        serde_json::to_value(SurfaceActivity::Waiting).expect("activity"),
        json!("waiting")
    );
    assert_eq!(OwnershipMode::Interactive.to_string(), "interactive");
    assert_eq!(SurfaceActivity::Running.to_string(), "running");
}

#[test]
fn default_surface_labels_are_clean_and_name_the_node() {
    use vowp_protocol::surface::{SurfaceKind, default_surface_label};

    let node = NodeName::parse("macbook").expect("node");
    assert_eq!(
        default_surface_label(SurfaceKind::Opencode, "main", &node),
        "OpenCode · macbook"
    );
    assert_eq!(
        default_surface_label(SurfaceKind::ClaudeCode, "main", &node),
        "Claude Code · macbook"
    );
    assert_eq!(
        default_surface_label(SurfaceKind::DesktopUi, "StackSpirit", &node),
        "StackSpirit · macbook"
    );
    assert_eq!(
        default_surface_label(SurfaceKind::DesktopUi, "  ", &node),
        "Desktop UI · macbook"
    );
}

/// A fixed owner for the attach fixtures.
fn desktop_owner(session: &SessionId, surface: &SurfaceId) -> SessionOwner {
    SessionOwner {
        session_id: session.clone(),
        surface_id: Some(surface.clone()),
        kind: SurfaceKind::DesktopUi,
        label: Some("StackSpirit".to_owned()),
        holder: "opencode-desktop".to_owned(),
        node: NodeName::parse("home-1").expect("node name"),
        mode: OwnershipMode::Interactive,
        state: OwnershipState::Active,
        activity: SurfaceActivity::Idle,
        epoch: 3,
        acquired_at: at(),
        heartbeat_at: at(),
        expires_at: None,
    }
}

#[test]
fn endpoint_access_wire_strings_default_and_display() {
    assert_eq!(EndpointAccess::default(), EndpointAccess::ReadOnly);
    for (access, wire) in [
        (EndpointAccess::Owner, "owner"),
        (EndpointAccess::ReadOnly, "read_only"),
    ] {
        assert_eq!(access.as_str(), wire);
        assert_eq!(access.to_string(), wire);
        assert_eq!(serde_json::to_value(access).expect("access JSON"), json!(wire));
        let back: EndpointAccess = serde_json::from_value(json!(wire)).expect("access back");
        assert_eq!(back, access);
    }
    serde_json::from_value::<EndpointAccess>(json!("writer")).expect_err("an unknown access is refused");
}

#[test]
fn an_endpoint_document_without_owner_or_access_still_parses() {
    // The v0.5.0 document shape: no `owner`, no `access`.
    let old = json!({
        "name": "main",
        "app": "opencode",
        "session_id": SessionId::new(),
        "location": "home",
        "node": null,
        "state": "running",
        "url": "http://127.0.0.1:1234",
        "native_session_id": null,
        "auth": null
    });
    let endpoint: OrchestratorEndpoint = serde_json::from_value(old).expect("an old document parses");
    assert!(endpoint.owner.is_none());
    assert_eq!(endpoint.access, EndpointAccess::ReadOnly);
}

#[test]
fn orchestrator_attach_round_trips_with_expiry_and_owner() {
    let session = SessionId::new();
    let surface = SurfaceId::new();
    let owner = desktop_owner(&session, &surface);
    let endpoint = OrchestratorEndpoint {
        name: "main".to_owned(),
        app: OrchestratorApp::Opencode,
        session_id: session,
        location: OrchestratorLocation::Home,
        node: None,
        state: OrchestratorNodeState::Running,
        url: Some("http://127.0.0.1:1234".to_owned()),
        native_session_id: None,
        auth: None,
        owner: Some(owner.clone()),
        access: EndpointAccess::Owner,
    };
    let attach = OrchestratorAttach {
        surface_id: surface,
        token: "t".repeat(52),
        owner,
        endpoint,
        expires_at: at().plus(std::time::Duration::from_secs(60)),
    };
    let value = serde_json::to_value(&attach).expect("attach JSON");
    assert_eq!(value["token"], "t".repeat(52));
    assert_eq!(value["endpoint"]["access"], "owner");
    assert_eq!(value["owner"]["kind"], "desktop_ui");
    assert_eq!(
        value["expires_at"],
        at().plus(std::time::Duration::from_secs(60)).as_rfc3339()
    );
    let back: OrchestratorAttach = serde_json::from_value(value).expect("attach back");
    assert_eq!(back, attach);
}

#[test]
fn orchestrator_attach_debug_redacts_the_token() {
    let session = SessionId::new();
    let surface = SurfaceId::new();
    // Assemble the marker at runtime so no secret-shaped literal is committed.
    let token = format!("attach-token-{}", "Q".repeat(24));
    let owner = desktop_owner(&session, &surface);
    let attach = OrchestratorAttach {
        surface_id: surface,
        token: token.clone(),
        endpoint: OrchestratorEndpoint {
            name: "main".to_owned(),
            app: OrchestratorApp::Opencode,
            session_id: session,
            location: OrchestratorLocation::Home,
            node: None,
            state: OrchestratorNodeState::Running,
            url: None,
            native_session_id: None,
            auth: None,
            owner: Some(owner.clone()),
            access: EndpointAccess::Owner,
        },
        owner,
        expires_at: at(),
    };
    let debug = format!("{attach:?}");
    assert!(debug.contains("[REDACTED]"), "debug redacts the token: {debug}");
    assert!(!debug.contains(&token), "debug never carries the token: {debug}");
    assert!(debug.contains("opencode-desktop"), "the rest stays readable: {debug}");
}
