//! Tests for the v0.3 loopback API documents in `vowp_protocol::api`.
//! Spec: `docs/design/v0.3/http-api.md`.
//!
//! These pin the wire shape the daemon serves: the health document, the generic page,
//! the concrete page aliases and the `vowp --json` error shape.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use vowp_protocol::api::{
    ApiError, ApiErrorBody, Board, BoardLimit, BoardRun, CostReport, CostRow, CostTotal, EventPage, Health,
    InboxAnswerRequest, InboxPage, JobPage, NodePage, Page, SessionDetail, SessionPage, SessionSwitchRequest,
};
use vowp_protocol::schema::schema_for_name;
use vowp_protocol::{
    ErrorKind, InboxId, InboxItem, InboxKind, InboxState, JobId, LimitRowKind, NodeName, ProjectName, Session,
    SessionId, SessionState, SwitchReason, Timestamp, Tokens,
};

/// Fixed test instant.
fn epoch() -> Timestamp {
    Timestamp::parse("2026-01-01T00:00:00.000Z").expect("epoch")
}

#[test]
fn health_roundtrips_with_and_without_a_heartbeat() {
    let health = Health {
        version: "0.2.0".to_owned(),
        node: NodeName::parse("local").expect("node"),
        project: ProjectName::parse("vowp").expect("project"),
        schema_version: 1,
        heartbeat: None,
    };
    let value = serde_json::to_value(&health).expect("health JSON");
    assert_eq!(
        value,
        json!({
            "version": "0.2.0",
            "node": "local",
            "project": "vowp",
            "schema_version": 1,
            "heartbeat": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<Health>(value).expect("health roundtrip"),
        health
    );

    let with_heartbeat = Health {
        heartbeat: Some(epoch()),
        ..health
    };
    let value = serde_json::to_value(&with_heartbeat).expect("health JSON");
    assert_eq!(value["heartbeat"], json!("2026-01-01T00:00:00.000Z"));
    assert_eq!(
        serde_json::from_value::<Health>(value).expect("health roundtrip"),
        with_heartbeat
    );
}

#[test]
fn api_error_matches_the_cli_json_shape() {
    let error = ApiError {
        error: ApiErrorBody {
            kind: ErrorKind::NotFound,
            message: "session 01ARZ3NDEKTSV4RRFFQ69G5FAV not found".to_owned(),
            hint: Some("vowp session ls".to_owned()),
        },
    };
    let value = serde_json::to_value(&error).expect("error JSON");
    assert_eq!(
        value,
        json!({
            "error": {
                "kind": "not_found",
                "message": "session 01ARZ3NDEKTSV4RRFFQ69G5FAV not found",
                "hint": "vowp session ls",
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<ApiError>(value).expect("error roundtrip"),
        error
    );
}

#[test]
fn page_json_and_schema_have_the_documented_fields() {
    let page: Page<String> = Page {
        items: vec!["first".to_owned()],
        next: Some(7),
        total: 1,
    };
    let value = serde_json::to_value(&page).expect("page JSON");
    assert_eq!(value, json!({ "items": ["first"], "next": 7, "total": 1 }));
    assert_eq!(
        serde_json::from_value::<Page<String>>(value).expect("page roundtrip"),
        page
    );

    let schema = schema_for_name("Page").expect("Page is exported");
    for field in ["items", "next", "total"] {
        assert!(
            schema["properties"].get(field).is_some(),
            "Page schema has no {field} property: {schema}"
        );
    }
    let required = schema["required"].as_array().expect("required array");
    assert!(required.contains(&json!("items")), "items must be required");
    assert!(required.contains(&json!("total")), "total must be required");
}

#[test]
fn concrete_page_aliases_serialise_their_item_type() {
    let jobs: JobPage = Page {
        items: Vec::new(),
        next: None,
        total: 0,
    };
    let sessions: SessionPage = Page {
        items: Vec::new(),
        next: None,
        total: 0,
    };
    let nodes: NodePage = Page {
        items: Vec::new(),
        next: None,
        total: 0,
    };
    let events: EventPage = Page {
        items: Vec::new(),
        next: None,
        total: 0,
    };
    let inbox: InboxPage = Page {
        items: Vec::new(),
        next: None,
        total: 0,
    };
    for page in [
        serde_json::to_value(jobs).expect("job page"),
        serde_json::to_value(sessions).expect("session page"),
        serde_json::to_value(nodes).expect("node page"),
        serde_json::to_value(events).expect("event page"),
        serde_json::to_value(inbox).expect("inbox page"),
    ] {
        assert_eq!(page, json!({ "items": [], "next": null, "total": 0 }));
    }
}

#[test]
fn session_detail_serialises_metadata_capsule_and_lineage() {
    let parent = SessionId::new();
    let root = SessionId::new();
    let session = Session {
        id: SessionId::new(),
        project: Some(ProjectName::parse("vowp").expect("project")),
        job_id: None,
        role: None,
        title: "detail session".to_owned(),
        parent_session_id: Some(parent.clone()),
        node: NodeName::parse("local").expect("node"),
        cwd: "/work/vowp".to_owned(),
        state: SessionState::Idle,
        executor: None,
        last_seq: 0,
        last_capsule_id: None,
        log_bytes: 0,
        created_at: epoch(),
        updated_at: epoch(),
    };
    let detail = SessionDetail {
        session: session.clone(),
        latest_capsule: None,
        lineage: vec![parent.clone(), root.clone()],
    };
    let value = serde_json::to_value(&detail).expect("session detail JSON");
    assert_eq!(value["session"]["id"], json!(session.id.as_str()));
    assert_eq!(value["session"]["parent_session_id"], json!(parent.as_str()));
    assert_eq!(value["latest_capsule"], Value::Null);
    assert_eq!(value["lineage"], json!([parent.as_str(), root.as_str()]));
    assert_eq!(
        serde_json::from_value::<SessionDetail>(value.clone()).expect("session detail roundtrip"),
        detail
    );

    let schema = schema_for_name("SessionDetail").expect("SessionDetail is exported");
    for field in ["session", "latest_capsule", "lineage"] {
        assert!(
            schema["properties"].get(field).is_some(),
            "SessionDetail schema has no {field} property: {schema}"
        );
    }
    for field in ["session", "lineage"] {
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!(field))),
            "SessionDetail schema does not require {field}: {schema}"
        );
    }
}

#[test]
fn board_matches_the_cli_json_shape() {
    let board = Board {
        at: epoch(),
        project: ProjectName::parse("vowp").expect("project"),
        jobs: BTreeMap::from([("pending".to_owned(), 2), ("running".to_owned(), 1)]),
        running: vec![BoardRun {
            job: JobId::parse("job-a").expect("job id"),
            executor: Some("claude-code:model-a@medium".to_owned()),
            age_secs: 90,
            last_event_age_secs: Some(30),
        }],
        slots_busy: 1,
        slots_total: 6,
        planner: Some("claude-code:model-a@medium".to_owned()),
        keeper: None,
        limits: vec![BoardLimit {
            provider: "claude".to_owned(),
            kind: LimitRowKind::Quota,
            resets_at: Some(Timestamp::parse("2026-01-01T01:00:00.000Z").expect("reset")),
        }],
        cost_today_usd: Some(1.25),
        inbox_open: 2,
        last_checkpoint: Some(Timestamp::parse("2025-12-31T23:00:00.000Z").expect("checkpoint")),
    };
    let value = serde_json::to_value(&board).expect("board JSON");
    assert_eq!(
        value,
        json!({
            "at": "2026-01-01T00:00:00.000Z",
            "project": "vowp",
            "jobs": { "pending": 2, "running": 1 },
            "running": [{
                "job": "job-a",
                "executor": "claude-code:model-a@medium",
                "age_secs": 90,
                "last_event_age_secs": 30,
            }],
            "slots_busy": 1,
            "slots_total": 6,
            "planner": "claude-code:model-a@medium",
            "keeper": null,
            "limits": [{
                "provider": "claude",
                "kind": "quota",
                "resets_at": "2026-01-01T01:00:00.000Z",
            }],
            "cost_today_usd": 1.25,
            "inbox_open": 2,
            "last_checkpoint": "2025-12-31T23:00:00.000Z",
        })
    );
    assert_eq!(serde_json::from_value::<Board>(value).expect("board roundtrip"), board);
}

#[test]
fn board_keeps_unknown_cost_and_unavailable_limits_null_or_named() {
    let board = Board {
        at: epoch(),
        project: ProjectName::parse("vowp").expect("project"),
        jobs: BTreeMap::new(),
        running: vec![BoardRun {
            job: JobId::parse("job-a").expect("job id"),
            executor: None,
            age_secs: 0,
            last_event_age_secs: None,
        }],
        slots_busy: 0,
        slots_total: 0,
        planner: None,
        keeper: None,
        limits: vec![BoardLimit {
            provider: "local".to_owned(),
            kind: LimitRowKind::Unavailable,
            resets_at: None,
        }],
        cost_today_usd: None,
        inbox_open: 0,
        last_checkpoint: None,
    };
    let value = serde_json::to_value(&board).expect("board JSON");
    assert_eq!(value["cost_today_usd"], Value::Null);
    assert_eq!(value["limits"][0]["kind"], json!("unavailable"));
    assert_eq!(value["limits"][0]["resets_at"], Value::Null);
    assert_eq!(value["running"][0]["executor"], Value::Null);
    assert_eq!(value["running"][0]["last_event_age_secs"], Value::Null);
    assert_eq!(serde_json::from_value::<Board>(value).expect("board roundtrip"), board);
}

#[test]
fn board_schemas_have_the_documented_fields() {
    let board = schema_for_name("Board").expect("Board is exported");
    for field in [
        "at",
        "project",
        "jobs",
        "running",
        "slots_busy",
        "slots_total",
        "planner",
        "keeper",
        "limits",
        "cost_today_usd",
        "inbox_open",
        "last_checkpoint",
    ] {
        assert!(
            board["properties"].get(field).is_some(),
            "Board schema has no {field} property: {board}"
        );
    }
    for field in [
        "at",
        "project",
        "jobs",
        "running",
        "slots_busy",
        "slots_total",
        "limits",
        "inbox_open",
    ] {
        assert!(
            board["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!(field))),
            "Board schema does not require {field}: {board}"
        );
    }

    let run = schema_for_name("BoardRun").expect("BoardRun is exported");
    for field in ["job", "executor", "age_secs", "last_event_age_secs"] {
        assert!(run["properties"].get(field).is_some(), "BoardRun has no {field}: {run}");
    }

    let limit = schema_for_name("BoardLimit").expect("BoardLimit is exported");
    for field in ["provider", "kind", "resets_at"] {
        assert!(
            limit["properties"].get(field).is_some(),
            "BoardLimit has no {field}: {limit}"
        );
    }
    let mut kinds = Vec::new();
    for one_of in [
        limit["properties"]["kind"]["oneOf"].as_array(),
        limit["$defs"]["LimitRowKind"]["oneOf"].as_array(),
    ]
    .into_iter()
    .flatten()
    {
        kinds.extend(
            one_of
                .iter()
                .filter_map(|variant| variant["const"].as_str())
                .collect::<Vec<_>>(),
        );
    }
    assert!(!kinds.is_empty(), "BoardLimit kind has no variants: {limit}");
    for kind in ["quota", "credits", "rate", "context", "auth", "unavailable"] {
        assert!(kinds.contains(&kind), "BoardLimit kind is missing {kind}: {limit}");
    }
}

#[test]
fn inbox_page_carries_items_and_the_page_envelope() {
    let item = InboxItem {
        id: InboxId::new(),
        project: Some(ProjectName::parse("vowp").expect("project")),
        job_id: Some(JobId::parse("job-a").expect("job id")),
        kind: InboxKind::Question,
        title: "Which way?".to_owned(),
        body: "Pick one".to_owned(),
        options: vec!["left".to_owned(), "right".to_owned()],
        state: InboxState::Open,
        answer: None,
        answered_by: None,
        created_at: epoch(),
        answered_at: None,
    };
    let page: InboxPage = Page {
        items: vec![item.clone()],
        next: None,
        total: 1,
    };
    let value = serde_json::to_value(&page).expect("inbox page JSON");
    assert_eq!(value["total"], 1);
    assert_eq!(value["next"], Value::Null);
    assert_eq!(value["items"][0]["id"], json!(item.id.as_str()));
    assert_eq!(value["items"][0]["state"], json!("open"));
    assert_eq!(value["items"][0]["created_at"], json!("2026-01-01T00:00:00.000Z"));
    assert_eq!(
        serde_json::from_value::<InboxPage>(value).expect("inbox page roundtrip"),
        page
    );
}

#[test]
fn inbox_answer_request_has_one_required_answer_field() {
    let request = InboxAnswerRequest {
        answer: "left".to_owned(),
    };
    let value = serde_json::to_value(&request).expect("answer request JSON");
    assert_eq!(value, json!({ "answer": "left" }));
    assert_eq!(
        serde_json::from_value::<InboxAnswerRequest>(value).expect("answer request roundtrip"),
        request
    );

    let schema = schema_for_name("InboxAnswerRequest").expect("InboxAnswerRequest is exported");
    assert!(
        schema["properties"].get("answer").is_some(),
        "InboxAnswerRequest schema has no answer property: {schema}"
    );
    assert!(
        schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("answer"))),
        "InboxAnswerRequest schema does not require answer: {schema}"
    );
}

#[test]
fn session_switch_request_defaults_reason_and_force_and_has_a_schema() {
    let minimal = serde_json::from_value::<SessionSwitchRequest>(json!({
        "to": "opencode:deepseek/deepseek-flash#max"
    }))
    .expect("minimal switch request");
    assert_eq!(minimal.to, "opencode:deepseek/deepseek-flash#max");
    assert_eq!(minimal.reason, SwitchReason::User);
    assert!(!minimal.force);

    let request = SessionSwitchRequest {
        to: "claude-code:claude-sonnet-5-5@medium".to_owned(),
        reason: SwitchReason::Context,
        force: true,
    };
    let value = serde_json::to_value(&request).expect("switch request JSON");
    assert_eq!(
        value,
        json!({
            "to": "claude-code:claude-sonnet-5-5@medium",
            "reason": "context",
            "force": true,
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionSwitchRequest>(value).expect("switch request roundtrip"),
        request
    );

    let schema = schema_for_name("SessionSwitchRequest").expect("SessionSwitchRequest is exported");
    for field in ["to", "reason", "force"] {
        assert!(
            schema["properties"].get(field).is_some(),
            "SessionSwitchRequest schema has no {field} property: {schema}"
        );
    }
    assert!(
        schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("to"))),
        "SessionSwitchRequest schema does not require to: {schema}"
    );
    assert!(
        !schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("reason")) || required.contains(&json!("force"))),
        "reason and force must be optional: {schema}"
    );
    // The generated schema carries the default reason so a client can see it.
    let reason = schema["properties"]["reason"].to_string();
    assert!(
        reason.contains("user"),
        "SessionSwitchRequest reason schema does not show the user default: {schema}"
    );
}

#[test]
fn cost_report_matches_the_cli_json_shape() {
    let report = CostReport {
        by: "job".to_owned(),
        rows: vec![
            CostRow {
                key: "job-a".to_owned(),
                runs: 2,
                usd: Some(0.5),
                tokens: Tokens {
                    input: 100,
                    output: 10,
                    ..Tokens::default()
                },
                source: "mixed".to_owned(),
                unknown_entries: 1,
            },
            CostRow {
                key: "job-b".to_owned(),
                runs: 1,
                usd: None,
                tokens: Tokens {
                    input: 12,
                    output: 3,
                    ..Tokens::default()
                },
                source: "unknown".to_owned(),
                unknown_entries: 1,
            },
        ],
        total: CostTotal {
            runs: 3,
            usd: Some(0.5),
            tokens: Tokens {
                input: 112,
                output: 13,
                ..Tokens::default()
            },
            source: "mixed".to_owned(),
            unknown_entries: 2,
        },
    };
    let value = serde_json::to_value(&report).expect("cost report JSON");
    assert_eq!(
        value,
        json!({
            "by": "job",
            "rows": [
                {
                    "key": "job-a",
                    "runs": 2,
                    "usd": 0.5,
                    "tokens": {
                        "input": 100,
                        "output": 10,
                        "reasoning": 0,
                        "cache_read": 0,
                        "cache_write": 0,
                    },
                    "source": "mixed",
                    "unknown_entries": 1,
                },
                {
                    "key": "job-b",
                    "runs": 1,
                    "usd": null,
                    "tokens": {
                        "input": 12,
                        "output": 3,
                        "reasoning": 0,
                        "cache_read": 0,
                        "cache_write": 0,
                    },
                    "source": "unknown",
                    "unknown_entries": 1,
                },
            ],
            "total": {
                "runs": 3,
                "usd": 0.5,
                "tokens": {
                    "input": 112,
                    "output": 13,
                    "reasoning": 0,
                    "cache_read": 0,
                    "cache_write": 0,
                },
                "source": "mixed",
                "unknown_entries": 2,
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<CostReport>(value).expect("cost report roundtrip"),
        report
    );
}

#[test]
fn inbox_and_cost_schemas_have_the_documented_fields() {
    let inbox = schema_for_name("InboxPage").expect("InboxPage is exported");
    for field in ["items", "next", "total"] {
        assert!(
            inbox["properties"].get(field).is_some(),
            "InboxPage schema has no {field} property: {inbox}"
        );
    }

    let report = schema_for_name("CostReport").expect("CostReport is exported");
    for field in ["by", "rows", "total"] {
        assert!(
            report["properties"].get(field).is_some(),
            "CostReport schema has no {field} property: {report}"
        );
        assert!(
            report["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!(field))),
            "CostReport schema does not require {field}: {report}"
        );
    }

    let row = schema_for_name("CostRow").expect("CostRow is exported");
    for field in ["key", "runs", "usd", "tokens", "source", "unknown_entries"] {
        assert!(
            row["properties"].get(field).is_some(),
            "CostRow schema has no {field} property: {row}"
        );
    }
    for field in ["key", "runs", "tokens", "source", "unknown_entries"] {
        assert!(
            row["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!(field))),
            "CostRow schema does not require {field}: {row}"
        );
    }

    let total = schema_for_name("CostTotal").expect("CostTotal is exported");
    for field in ["runs", "usd", "tokens", "source", "unknown_entries"] {
        assert!(
            total["properties"].get(field).is_some(),
            "CostTotal schema has no {field} property: {total}"
        );
    }
    for field in ["runs", "tokens", "source", "unknown_entries"] {
        assert!(
            total["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!(field))),
            "CostTotal schema does not require {field}: {total}"
        );
    }
}

#[test]
fn api_documents_are_exported_schemas() {
    for name in [
        "Health",
        "Board",
        "BoardRun",
        "BoardLimit",
        "Page",
        "JobPage",
        "SessionPage",
        "NodePage",
        "SessionDetail",
        "EventPage",
        "InboxAnswerRequest",
        "SessionSwitchRequest",
        "InboxPage",
        "CostReport",
        "CostRow",
        "CostTotal",
        "ApiError",
        "ApiErrorBody",
    ] {
        let schema: Value = schema_for_name(name).unwrap_or_else(|| panic!("{name} is not exported"));
        assert!(schema.is_object(), "{name} schema must be a JSON object");
    }
}
