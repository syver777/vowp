//! Job, edge and patch tests. Spec: `docs/design/types.md` §7 and
//! `docs/design/crates/vowp-protocol.md` §6.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use vowp_protocol::job::{Edge, EdgeKind, Job, JobLoop, JobPatch, JobState, LoopUntil, OnExhausted, ParkedReason};
use vowp_protocol::{JobId, JobKind, Role};

#[test]
fn job_patch_json_roundtrip_with_loop_and_defaults() {
    let empty: JobPatch = serde_json::from_str("{}").expect("an empty patch is all defaults");
    assert_eq!(empty, JobPatch::default());
    assert_eq!(serde_json::to_value(&empty).expect("serialisable"), json!({}));
    assert!(empty.add_needs.is_empty());
    assert!(empty.remove_needs.is_empty());

    let raw = json!({
        "loop": {"until": "gates", "max_attempts": 2, "on_exhausted": "park"}
    });
    let patch: JobPatch = serde_json::from_value(raw).expect("loop key is accepted");
    assert_eq!(
        patch.loop_,
        Some(JobLoop {
            until: LoopUntil::Gates,
            max_attempts: 2,
            on_exhausted: OnExhausted::Park,
        })
    );
    assert!(patch.add_needs.is_empty());
    assert!(patch.remove_needs.is_empty());

    let encoded = serde_json::to_value(&patch).expect("serialisable");
    assert_eq!(
        encoded["loop"],
        json!({"until": "gates", "max_attempts": 2, "on_exhausted": "park"})
    );
    let decoded: JobPatch = serde_json::from_value(encoded).expect("roundtrip");
    assert_eq!(decoded, patch);

    let with_needs: JobPatch = serde_json::from_value(json!({
        "add_needs": ["sample-dependency"],
        "remove_needs": ["other-dependency"],
        "title": "new title"
    }))
    .expect("needs lists decode");
    assert_eq!(with_needs.add_needs, vec![JobId::parse("sample-dependency").unwrap()]);
    assert_eq!(with_needs.remove_needs, vec![JobId::parse("other-dependency").unwrap()]);
    assert_eq!(with_needs.title.as_deref(), Some("new title"));
}

#[test]
fn job_row_serde_roundtrips_fixture() {
    let raw: Value = serde_json::from_str(include_str!("fixtures/job_example.json")).expect("fixture is JSON");
    let job: Job = serde_json::from_value(raw.clone()).expect("fixture decodes");
    assert_eq!(job.id.as_str(), "sample-job");
    assert_eq!(job.kind, JobKind::Build);
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.role, Role::Worker);
    assert_eq!(job.loop_.max_attempts, 3);
    assert_eq!(job.parked_reason, None);
    assert_eq!(job.gates.len(), 4);

    let encoded = serde_json::to_value(&job).expect("job is serialisable");
    assert_eq!(encoded["loop"]["max_attempts"], 3);
    assert_eq!(encoded["gates"][1]["cmd"]["timeout_secs"], 1800);
    let reparsed: Job = serde_json::from_value(encoded).expect("job roundtrips");
    assert_eq!(reparsed, job);
}

#[test]
fn job_state_predicates_match_the_spec() {
    for state in [JobState::Done, JobState::Replanned, JobState::Cancelled] {
        assert!(state.is_terminal(), "{state:?} is terminal");
    }
    for state in [
        JobState::Pending,
        JobState::Ready,
        JobState::Running,
        JobState::Stalled,
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
        JobState::Failed,
        JobState::Parked,
    ] {
        assert!(!state.is_terminal(), "{state:?} is not terminal");
    }
    for state in [
        JobState::Running,
        JobState::Stalled,
        JobState::Gating,
        JobState::Reviewing,
        JobState::Fixing,
        JobState::Merging,
    ] {
        assert!(state.holds_lock(), "{state:?} holds the lock");
    }
    for state in [
        JobState::Pending,
        JobState::Ready,
        JobState::Done,
        JobState::Failed,
        JobState::Parked,
        JobState::Replanned,
        JobState::Cancelled,
    ] {
        assert!(!state.holds_lock(), "{state:?} does not hold the lock");
    }
    assert_eq!(JobState::Gating.as_str(), "gating");
    assert_eq!(JobState::Cancelled.as_str(), "cancelled");
    assert_eq!(ParkedReason::ProviderBlocked, ParkedReason::ProviderBlocked);
}

#[test]
fn edge_kind_only_fixes_forms_a_loop() {
    let needs = Edge {
        src: JobId::parse("sample-job").unwrap(),
        dst: JobId::parse("sample-need").unwrap(),
        kind: EdgeKind::Needs,
    };
    assert!(!needs.kind.is_loop_edge());
    assert!(EdgeKind::Fixes.is_loop_edge());
    assert!(!EdgeKind::Verifies.is_loop_edge());
    assert!(!EdgeKind::Escalates.is_loop_edge());
    assert!(!EdgeKind::Blocks.is_loop_edge());

    let encoded = serde_json::to_value(&needs).expect("edge is serialisable");
    assert_eq!(
        encoded,
        json!({"src": "sample-job", "dst": "sample-need", "kind": "needs"})
    );
    let decoded: Edge = serde_json::from_value(encoded).expect("roundtrip");
    assert_eq!(decoded, needs);
}
