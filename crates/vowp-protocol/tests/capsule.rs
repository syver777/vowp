//! Capsule validation, markdown rendering and draft-schema tests.
//! Spec: `docs/design/crates/vowp-protocol.md` §6 and `docs/design/types.md` §6.

use pretty_assertions::assert_eq;
use serde_json::json;
use vowp_protocol::prelude::*;

fn fixture() -> Capsule {
    serde_json::from_str(include_str!("fixtures/capsule_example.json")).expect("capsule_example.json must decode")
}

fn assert_invalid(capsule: &Capsule, field: &str) {
    let err = capsule.validate().expect_err("capsule must be rejected");
    assert!(
        matches!(err, ProtocolError::Config(_)),
        "unexpected error for {field}: {err:?}"
    );
    assert!(err.to_string().contains(field), "error must name {field}: {err}");
}

/// One invalid-capsule case: the field name the error must mention and the mutation.
type CapsuleMutation = (&'static str, fn(&mut Capsule));

#[test]
fn capsule_validate_rules() {
    let capsule = fixture();
    assert!(capsule.validate().is_ok(), "the types.md example is valid");

    // A minimal capsule with only the required sections is valid.
    let minimal = Capsule {
        intent: "ship it".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        ..capsule.clone()
    };
    assert!(minimal.validate().is_ok());

    let cases: Vec<CapsuleMutation> = vec![
        ("intent", |c: &mut Capsule| c.intent = "   \n\t".to_owned()),
        ("version", |c: &mut Capsule| c.version = 2),
        ("at_seq", |c: &mut Capsule| c.at_seq = 0),
        ("constraints", |c: &mut Capsule| {
            c.constraints = vec!["ok".to_owned(), "  ".to_owned()]
        }),
        ("decisions[0].what", |c: &mut Capsule| {
            c.decisions[0].what = String::new()
        }),
        ("decisions[0].why", |c: &mut Capsule| {
            c.decisions[0].why = "\t".to_owned()
        }),
        ("state.done", |c: &mut Capsule| c.state.done = vec![String::new()]),
        ("state.in_progress", |c: &mut Capsule| {
            c.state.in_progress = vec!["  ".to_owned()]
        }),
        ("state.blocked", |c: &mut Capsule| c.state.blocked = vec![String::new()]),
        ("state.next_steps", |c: &mut Capsule| {
            c.state.next_steps = vec!["".to_owned()]
        }),
        ("files[0].path", |c: &mut Capsule| c.files[0].path = "   ".to_owned()),
        ("gotchas", |c: &mut Capsule| c.gotchas = vec![String::new()]),
        ("open_questions", |c: &mut Capsule| {
            c.open_questions = vec!["\n".to_owned()]
        }),
    ];

    for (field, mutate) in cases {
        let mut broken = capsule.clone();
        mutate(&mut broken);
        assert_invalid(&broken, field);
    }
}

#[test]
fn capsule_render_markdown_golden() {
    let rendered = fixture().render_markdown();
    assert_eq!(rendered, include_str!("fixtures/capsule_golden.md"));

    // Empty sections and empty state subsections are omitted.
    assert!(!rendered.contains("## Open questions"));
    assert!(!rendered.contains("### Blocked"));
    assert!(!rendered.contains("## Telemetry"));

    let empty = Capsule {
        intent: "only the intent".to_owned(),
        constraints: Vec::new(),
        decisions: Vec::new(),
        state: CapsuleState::default(),
        files: Vec::new(),
        gotchas: Vec::new(),
        open_questions: Vec::new(),
        ..fixture()
    };
    assert_eq!(empty.render_markdown(), "## Intent\n\nonly the intent\n");
}

#[test]
fn capsule_draft_json_schema_has_required_fields() {
    let schema = schemars::schema_for!(CapsuleDraft);
    let value = serde_json::to_value(&schema).expect("schema must serialise to JSON");
    let required = value["required"]
        .as_array()
        .expect("CapsuleDraft schema must list required fields");
    let required: Vec<&str> = required.iter().filter_map(|name| name.as_str()).collect();
    assert!(required.contains(&"intent"), "intent must be required: {required:?}");
    assert!(required.contains(&"state"), "state must be required: {required:?}");
    assert_eq!(required.len(), 2, "only intent and state are required: {required:?}");

    let properties = value["properties"]
        .as_object()
        .expect("CapsuleDraft schema must have properties");
    assert!(properties.contains_key("intent"));
    assert!(properties.contains_key("state"));
    assert!(properties.contains_key("constraints"));
    assert!(
        serde_json::from_value::<CapsuleDraft>(json!({
            "intent": "x",
            "state": {}
        }))
        .is_ok(),
        "defaults must let a draft omit every optional list"
    );
}
