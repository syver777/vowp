//! JSON Schema export and validation of the `types.md` examples.
//! Spec: `docs/design/types.md` §14–15 and `docs/design/crates/vowp-protocol.md` §6.
//!
//! The examples are read from `docs/design/types.md` itself, which is the canonical source
//! for the fixture files listed in the crate spec. The structural checker verifies the
//! required properties and, for `deny_unknown_fields` types (whose schema says
//! `additionalProperties: false`), that no unknown top-level key is present.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;

use vowp_protocol::capsule::Capsule;
use vowp_protocol::cost::Cost;
use vowp_protocol::event::EventEnvelope;
use vowp_protocol::executor::Profile;
use vowp_protocol::gate::GateResult;
use vowp_protocol::inbox::InboxItem;
use vowp_protocol::job::{Edge, Job};
use vowp_protocol::limit::Limit;
use vowp_protocol::loops::{Loop, Schedule};
use vowp_protocol::release::Release;
use vowp_protocol::run::{Run, Worker};
use vowp_protocol::schema::{all_schemas, schema_for_name};
use vowp_protocol::session::{Session, Switch};

/// The type names rule 12 of the crate spec requires.
const LISTED_TYPES: [&str; 27] = [
    "EventEnvelope",
    "Capsule",
    "CapsuleDraft",
    "Job",
    "NewJob",
    "JobPatch",
    "Gate",
    "Evidence",
    "GateResult",
    "ReportFrontMatter",
    "BriefFrontMatter",
    "VowpConfig",
    "Schedule",
    "Release",
    "InboxItem",
    "Run",
    "Worker",
    "Session",
    "Limit",
    "Cost",
    "Switch",
    "Loop",
    "Node",
    "Project",
    "Profile",
    "Edge",
    "LedgerEntry",
];

/// Decodes one JSON value into a concrete type.
type Decoder = fn(&Value) -> Result<(), String>;

/// One example fixture: its name, schema name, decoder and parsed JSON.
type Case = (&'static str, &'static str, Decoder, Value);

/// Decodes a JSON value into `T`.
fn decodes<T: DeserializeOwned>(value: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(value.clone())
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Returns the text from `heading` to `next` (exclusive).
fn section<'a>(doc: &'a str, heading: &str, next: &str) -> &'a str {
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("types.md is missing heading {heading:?}"));
    let rest = &doc[start + heading.len()..];
    let end = rest
        .find(next)
        .unwrap_or_else(|| panic!("types.md is missing {next:?} after {heading:?}"));
    &rest[..end]
}

/// Returns the text from `heading` to the end of the document.
fn section_to_end<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc
        .find(heading)
        .unwrap_or_else(|| panic!("types.md is missing heading {heading:?}"));
    &doc[start + heading.len()..]
}

/// Returns every fenced ```json block in `section`, in order.
fn json_blocks(section: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = section;
    while let Some((_, after)) = rest.split_once("```json\n") {
        let (block, tail) = after
            .split_once("\n```")
            .unwrap_or_else(|| panic!("unterminated json block in types.md"));
        blocks.push(block);
        rest = tail;
    }
    blocks
}

/// Parses every JSON value in `text`, including several concatenated values.
fn json_values(text: &str) -> Vec<Value> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|err| panic!("types.md example is not JSON: {err}\n{text}"))
}

/// Parses the first json block of `section`.
fn first_values(section: &str) -> Vec<Value> {
    let blocks = json_blocks(section);
    assert_eq!(blocks.len(), 1, "expected exactly one json block");
    json_values(blocks[0])
}

#[test]
fn schema_export_covers_listed_types() {
    let schemas = all_schemas();
    assert_eq!(
        schemas.len(),
        LISTED_TYPES.len(),
        "all_schemas must export exactly the listed types"
    );
    for name in LISTED_TYPES {
        let schema = schemas
            .get(name)
            .unwrap_or_else(|| panic!("all_schemas is missing {name}"));
        let object = schema
            .as_object()
            .unwrap_or_else(|| panic!("{name} schema must be a JSON object"));
        assert!(
            object.contains_key("$schema") || object.contains_key("type") || object.contains_key("definitions"),
            "{name} schema must look like a schema: {schema}"
        );
    }

    assert_eq!(all_schemas(), all_schemas(), "schema export must be deterministic");

    let cost = schema_for_name("Cost").expect("Cost is exported");
    assert_eq!(cost["title"], "Cost");
    assert!(schema_for_name("NotAType").is_none());
}

#[test]
fn types_md_examples_validate() {
    let types_md = include_str!("../../../docs/design/types.md");

    let cost_and_limit = first_values(section(types_md, "## 4. Cost and limits", "## 5. Events"));
    assert_eq!(cost_and_limit.len(), 2, "cost and limit examples");
    let events = first_values(section(types_md, "### 5.3 Example log lines", "## 6. Capsule"));
    assert_eq!(events.len(), 3, "three event log lines");
    let capsule = first_values(section(types_md, "## 6. Capsule", "## 7. Jobs"));
    assert_eq!(capsule.len(), 1, "one capsule example");
    let job = first_values(section(types_md, "## 7. Jobs, edges, states", "## 8. Gates"));
    assert_eq!(job.len(), 1, "one job example");

    let more = json_blocks(section_to_end(types_md, "## 15. More JSON examples (fixtures)"));
    assert_eq!(more.len(), 10, "ten json blocks in §15");
    let mut more_values = Vec::new();
    for block in more {
        more_values.extend(json_values(block));
    }
    assert_eq!(more_values.len(), 11, "profile, edge and nine single examples");
    let names = [
        "profile.json",
        "edge.json",
        "run.json",
        "session.json",
        "inbox_item.json",
        "loop.json",
        "release.json",
        "schedule.json",
        "gate_result.json",
        "worker.json",
        "switch.json",
    ];

    let mut cases: Vec<Case> = vec![
        ("cost.json", "Cost", decodes::<Cost>, cost_and_limit[0].clone()),
        ("limit.json", "Limit", decodes::<Limit>, cost_and_limit[1].clone()),
        ("capsule.json", "Capsule", decodes::<Capsule>, capsule[0].clone()),
        ("job.json", "Job", decodes::<Job>, job[0].clone()),
    ];
    for value in events {
        // The three lines share one fixture name; the decoder names the line on failure.
        cases.push(("events.jsonl", "EventEnvelope", decodes::<EventEnvelope>, value));
    }
    let decoders: [Decoder; 11] = [
        decodes::<Profile>,
        decodes::<Edge>,
        decodes::<Run>,
        decodes::<Session>,
        decodes::<InboxItem>,
        decodes::<Loop>,
        decodes::<Release>,
        decodes::<Schedule>,
        decodes::<GateResult>,
        decodes::<Worker>,
        decodes::<Switch>,
    ];
    for ((name, value), decode) in names.into_iter().zip(more_values).zip(decoders) {
        let schema_name = match name {
            "profile.json" => "Profile",
            "edge.json" => "Edge",
            "run.json" => "Run",
            "session.json" => "Session",
            "inbox_item.json" => "InboxItem",
            "loop.json" => "Loop",
            "release.json" => "Release",
            "schedule.json" => "Schedule",
            "gate_result.json" => "GateResult",
            "worker.json" => "Worker",
            "switch.json" => "Switch",
            other => panic!("unexpected fixture {other}"),
        };
        cases.push((name, schema_name, decode, value));
    }

    assert_eq!(cases.len(), 18, "all named example fixtures are covered");

    let schemas: BTreeMap<&str, Value> = all_schemas();
    for (fixture, schema_name, decode, value) in cases {
        decode(&value).unwrap_or_else(|err| panic!("{fixture} does not decode into {schema_name}: {err}"));
        let schema = schemas
            .get(schema_name)
            .unwrap_or_else(|| panic!("{schema_name} is not exported"));
        check_structure(fixture, schema, &value);
    }
}

/// Checks required properties and unknown top-level keys against one schema.
fn check_structure(fixture: &str, schema: &Value, value: &Value) {
    let schema_object = schema.as_object().expect("schema is an object");
    let properties = schema_object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{fixture}: schema has no properties"));
    let value_object = value.as_object().expect("fixture is a JSON object");

    if let Some(required) = schema_object.get("required").and_then(Value::as_array) {
        for name in required {
            let name = name.as_str().expect("required entries are strings");
            assert!(
                value_object.contains_key(name),
                "{fixture}: required property {name:?} is missing"
            );
        }
    }

    if schema_object.get("additionalProperties") == Some(&Value::Bool(false)) {
        for key in value_object.keys() {
            assert!(
                properties.contains_key(key),
                "{fixture}: unknown property {key:?} is not in the schema"
            );
        }
    }
}
