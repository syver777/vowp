//! Snapshot documents: the session card's additive `owner` field.
//! Spec: `docs/design/v0.5.1/session-ownership.md` §2.4.

use serde_json::json;

use vowp_protocol::ids::{NodeName, SessionId};
use vowp_protocol::snapshot::SessionSurfaceCard;
use vowp_protocol::surface::{OwnershipMode, OwnershipState, SessionOwner, SurfaceActivity, SurfaceId, SurfaceKind};
use vowp_protocol::time::Timestamp;

fn card_json(id: &SessionId) -> serde_json::Value {
    json!({
        "id": id.to_string(),
        "project": null,
        "job_id": null,
        "role": "orchestrator",
        "title": "main",
        "state": "idle",
        "executor": null,
        "node": "home",
        "active_surface": null,
        "last_event_at": "2023-11-14T22:13:20Z"
    })
}

#[test]
fn session_card_without_owner_still_parses() {
    let id = SessionId::new();
    let card: SessionSurfaceCard = serde_json::from_value(card_json(&id)).expect("a v0.5 card parses");
    assert_eq!(card.owner, None);
    assert_eq!(card.active_surface, None);
}

#[test]
fn session_card_round_trips_an_owner_without_a_token_field() {
    let id = SessionId::new();
    let mut value = card_json(&id);
    let owner = SessionOwner {
        session_id: id.clone(),
        surface_id: Some(SurfaceId::new()),
        kind: SurfaceKind::DesktopUi,
        label: Some("StackSpirit · home".to_owned()),
        holder: "stackspirit-1".to_owned(),
        node: NodeName::parse("home").expect("node"),
        mode: OwnershipMode::Interactive,
        state: OwnershipState::Active,
        activity: SurfaceActivity::Running,
        epoch: 3,
        acquired_at: Timestamp::from_unix_secs(1_700_000_000),
        heartbeat_at: Timestamp::from_unix_secs(1_700_000_004),
        expires_at: None,
    };
    value["owner"] = serde_json::to_value(&owner).expect("owner JSON");
    let text = value.to_string();
    for forbidden in ["token", "password", "hash"] {
        assert!(
            !text.contains(forbidden),
            "owner JSON must not mention {forbidden}: {text}"
        );
    }
    let card: SessionSurfaceCard = serde_json::from_value(value).expect("card with owner parses");
    assert_eq!(card.owner, Some(owner));
}

#[test]
fn queue_and_running_cards_omit_new_fields_when_absent_and_round_trip_them() {
    use vowp_protocol::snapshot::{QueueJobCard, RunningJobCard};
    let queue = json!({
        "job": "a-job", "title": "t", "kind": "build", "role": "worker", "priority": 1,
        "state": "ready", "age_secs": 3, "waiting_reason": null
    });
    let card: QueueJobCard = serde_json::from_value(queue).expect("old queue card parses");
    assert_eq!(card.planned_executor, None);
    assert!(
        !serde_json::to_value(&card)
            .expect("json")
            .as_object()
            .expect("object")
            .contains_key("planned_executor")
    );
    let mut card = card;
    card.planned_executor = Some("opencode:deepseek/deepseek-flash".to_owned());
    let back: QueueJobCard = serde_json::from_value(serde_json::to_value(&card).expect("json")).expect("parse");
    assert_eq!(
        back.planned_executor.as_deref(),
        Some("opencode:deepseek/deepseek-flash")
    );

    let running = json!({
        "job": "a-job", "kind": "build", "role": "worker", "executor": null, "node": "home",
        "age_secs": 1, "last_event_age_secs": null, "session_id": null
    });
    let card: RunningJobCard = serde_json::from_value(running).expect("old running card parses");
    assert_eq!(card.tokens_input, None);
    assert_eq!(card.billing_mode, None);
    let value = serde_json::to_value(&card).expect("json");
    for key in [
        "tokens_input",
        "tokens_output",
        "tokens_cache_read",
        "context_window",
        "cost_usd",
        "billing_mode",
    ] {
        assert!(
            !value.as_object().expect("object").contains_key(key),
            "{key} must be omitted"
        );
    }
}
