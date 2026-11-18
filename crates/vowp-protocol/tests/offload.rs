//! v0.4 offload wire format: placement, waiting reasons, assignments and resume records.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §6.3, §7.1, §7.2, §7.5 and §11.3.

use pretty_assertions::assert_eq;
use serde_json::json;

use vowp_protocol::agent::{AgentFrame, AgentPathMap, MAX_BUNDLE_CHUNK_BYTES, PathRef, is_bundle_chunk_data};
use vowp_protocol::config::ResultTransport;
use vowp_protocol::error::ErrorKind;
use vowp_protocol::executor::Executor;
use vowp_protocol::ids::{JobId, NodeName, ProjectName, RunId, SessionId};
use vowp_protocol::offload::{
    AssignmentPayload, NodeFallback, Placement, RemoteRunState, RunRejectReason, RunResume, RunResumeResult,
    SurfaceResume, SurfaceResumeResult, WaitingReason,
};
use vowp_protocol::run::RunPurpose;
use vowp_protocol::surface::SurfaceId;
use vowp_protocol::time::Timestamp;
use vowp_protocol::vault::SecretName;

/// A fixed timestamp for fixtures.
fn at() -> Timestamp {
    Timestamp::from_unix_secs(1_700_000_000)
}

/// Parses a project name in fixtures.
fn project(name: &str) -> ProjectName {
    ProjectName::parse(name).expect("valid project name")
}

/// Parses a job id in fixtures.
fn job(name: &str) -> JobId {
    JobId::parse(name).expect("valid job id")
}

/// Parses a node name in fixtures.
fn node(name: &str) -> NodeName {
    NodeName::parse(name).expect("valid node name")
}

/// A fixed assignment payload with names only, no token and no secret value.
fn assignment() -> AssignmentPayload {
    AssignmentPayload {
        assignment_id: "assign-01".to_owned(),
        run_id: RunId::new(),
        session_id: SessionId::new(),
        project: project("vowp"),
        job: job("graph-engine"),
        executor: Executor::parse("opencode:deepseek/deepseek-flash#max").expect("executor"),
        purpose: RunPurpose::Start,
        prompt: "Implement the offload wire format.".to_owned(),
        cwd_ref: PathRef::Worktree {
            project: project("vowp"),
            job: job("graph-engine"),
        },
        base_commit: "abc1234".to_owned(),
        remote: "origin".to_owned(),
        remote_url: "https://example.invalid/owner/repo.git".to_owned(),
        result_transport: ResultTransport::Push,
        env_names: vec!["CI".to_owned()],
        secret_names: vec![SecretName::parse("HOME_GITHUB_TOKEN").expect("secret name")],
        epoch: 1,
        deadline: Some(at()),
        spool_cap_bytes: 512 * 1024 * 1024,
    }
}

#[test]
fn placement_round_trips_home_any_and_node() {
    let cases = [
        (Placement::Home, "home"),
        (Placement::Any, "any"),
        (
            Placement::Node {
                node: node("mac-studio"),
                fallback: None,
            },
            "node:mac-studio",
        ),
    ];
    for (placement, wire) in cases {
        assert_eq!(placement.to_string(), wire);
        assert_eq!(Placement::parse(wire).expect("placement parses"), placement);
        assert_eq!(wire.parse::<Placement>().expect("placement from_str"), placement);
        let value = serde_json::to_value(&placement).expect("placement JSON");
        assert_eq!(value, json!(wire));
        let decoded: Placement = serde_json::from_value(value).expect("placement back");
        assert_eq!(decoded, placement);
    }
    assert_eq!(Placement::default(), Placement::Home);
}

#[test]
fn placement_with_home_fallback_round_trips() {
    let placement = Placement::Node {
        node: node("mac-studio"),
        fallback: Some(NodeFallback::Home),
    };
    assert_eq!(placement.to_string(), "node:mac-studio+home");
    assert_eq!(
        Placement::parse("node:mac-studio+home").expect("placement parses"),
        placement
    );

    let value = serde_json::to_value(&placement).expect("placement JSON");
    assert_eq!(value, json!("node:mac-studio+home"));
    let decoded: Placement = serde_json::from_value(value).expect("placement back");
    assert_eq!(decoded, placement);

    assert_eq!(
        serde_json::to_value(NodeFallback::Home).expect("fallback JSON"),
        json!("home")
    );
}

#[test]
fn placement_rejects_an_unknown_or_malformed_selector() {
    let bad = [
        "",
        "Home",
        "ANY",
        "node",
        "node:",
        "node:Mac-Studio",
        "node:mac studio",
        "node: mac-studio",
        "node:mac/studio",
        "node:mac:studio",
        "node:mac-studio+",
        "node:mac-studio+Home",
        "node:mac-studio+laptop",
        "node:+home",
        "node:mac-studio+home+home",
        " node:mac-studio",
        "any+home",
    ];
    for value in bad {
        let err = Placement::parse(value).expect_err("invalid placement must fail");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "error kind for {value:?}");
        assert!(err.to_string().contains("placement"), "error names placement: {err}");
    }

    let err = serde_json::from_str::<Placement>("\"node:mac/studio\"").expect_err("invalid placement JSON");
    assert!(
        err.to_string().contains("placement"),
        "JSON error names placement: {err}"
    );
}

#[test]
fn waiting_reason_matches_the_documented_wire_values() {
    let cases = [
        (WaitingReason::NodeOffline, "node_offline"),
        (WaitingReason::NodeRevoked, "node_revoked"),
        (WaitingReason::NodeBusy, "node_busy"),
        (WaitingReason::NodeBattery, "node_battery"),
        (WaitingReason::NodeDiskLow, "node_disk_low"),
        (WaitingReason::NodeLimit, "node_limit"),
        (WaitingReason::PathUnmapped, "path_unmapped"),
        (WaitingReason::PathMissing, "path_missing"),
        (WaitingReason::MissingSecret, "missing_secret"),
        (WaitingReason::HomeOnlySecret, "home_only_secret"),
        (WaitingReason::KindNotAllowed, "kind_not_allowed"),
        (WaitingReason::ProjectHomeOnly, "project_home_only"),
        (WaitingReason::RemoteMismatch, "remote_mismatch"),
        (WaitingReason::NoEligibleNode, "no_eligible_node"),
    ];
    assert_eq!(cases.len(), 14, "every §7.5 waiting reason is covered");
    for (reason, wire) in cases {
        assert_eq!(serde_json::to_value(reason).expect("reason JSON"), json!(wire));
        let decoded: WaitingReason = serde_json::from_value(json!(wire)).expect("reason back");
        assert_eq!(decoded, reason);
    }

    let err = serde_json::from_value::<WaitingReason>(json!("node_asleep")).expect_err("unknown reason");
    assert!(err.to_string().contains("node_asleep"), "error names the value: {err}");
}

#[test]
fn assignment_payload_round_trips_without_a_lease_token() {
    let payload = assignment();
    let value = serde_json::to_value(&payload).expect("payload JSON");
    let text = serde_json::to_string(&value).expect("payload text");

    assert!(value.get("lease_token").is_none(), "payload has no lease token: {text}");
    assert!(
        !text.contains("lease_token"),
        "payload text names no lease token: {text}"
    );
    assert!(!text.contains("/Users/"), "payload has no absolute local path: {text}");
    assert_eq!(value["cwd_ref"], json!("worktree:vowp/graph-engine"));

    let decoded: AssignmentPayload = serde_json::from_value(value).expect("payload back");
    assert_eq!(decoded, payload);
}

#[test]
fn assignment_payload_carries_the_expected_remote_url() {
    let payload = assignment();
    assert_eq!(payload.remote, "origin", "remote stays the git remote name");
    assert_eq!(
        payload.remote_url, "https://example.invalid/owner/repo.git",
        "the payload carries the home project's remote URL"
    );
    assert!(
        !payload.remote_url.contains('@'),
        "the fixture URL carries no userinfo: {}",
        payload.remote_url
    );

    let value = serde_json::to_value(&payload).expect("payload JSON");
    assert_eq!(value["remote"], json!("origin"));
    assert_eq!(value["remote_url"], json!("https://example.invalid/owner/repo.git"));

    let decoded: AssignmentPayload = serde_json::from_value(value).expect("payload back");
    assert_eq!(decoded.remote_url, payload.remote_url);
}

#[test]
fn assign_run_frame_carries_the_lease_token_but_the_payload_does_not() {
    const FAKE_TOKEN: &str = "FAKE-LEASE-TOKEN-NOT-A-REAL-SECRET";
    let payload = assignment();
    let frame = AgentFrame::AssignRun {
        assignment: payload.clone(),
        lease_token: FAKE_TOKEN.to_owned(),
        lease_expires_at: at(),
    };

    let value = serde_json::to_value(&frame).expect("assign frame JSON");
    assert_eq!(value["type"], "assign_run");
    assert_eq!(value["lease_token"], json!(FAKE_TOKEN));
    assert!(
        value["assignment"].get("lease_token").is_none(),
        "the nested payload has no lease token: {value}"
    );
    let payload_text = serde_json::to_string(&payload).expect("payload text");
    assert!(
        !payload_text.contains(FAKE_TOKEN),
        "the payload never carries the token"
    );

    let decoded: AgentFrame = serde_json::from_value(value).expect("assign frame back");
    assert_eq!(decoded, frame);
}

#[test]
fn assignment_payload_defaults_an_older_literal_to_push() {
    let mut value = serde_json::to_value(assignment()).expect("payload JSON");
    value
        .as_object_mut()
        .expect("payload object")
        .remove("result_transport");

    let decoded: AssignmentPayload = serde_json::from_value(value).expect("older payload parses");
    assert_eq!(
        decoded.result_transport,
        ResultTransport::Push,
        "an older literal without the field is a push assignment"
    );

    let bundle = AssignmentPayload {
        result_transport: ResultTransport::Bundle,
        ..assignment()
    };
    let value = serde_json::to_value(&bundle).expect("bundle payload JSON");
    assert_eq!(value["result_transport"], json!("bundle"));
    let decoded: AssignmentPayload = serde_json::from_value(value).expect("bundle payload back");
    assert_eq!(decoded.result_transport, ResultTransport::Bundle);
}

#[test]
fn bundle_frames_round_trip_with_run_id_and_epoch() {
    let run_id = RunId::new();
    let frames = [
        AgentFrame::BundleStart {
            run_id: run_id.clone(),
            epoch: 2,
            base: "a".repeat(40),
            head: "b".repeat(40),
            branch: "job/graph-engine".to_owned(),
            bytes: 4096,
            sha256: "c".repeat(64),
        },
        AgentFrame::BundleAccept {
            run_id: run_id.clone(),
            epoch: 2,
            offset: 1024,
        },
        AgentFrame::BundleChunk {
            run_id: run_id.clone(),
            epoch: 2,
            offset: 1024,
            data: "00ff".to_owned(),
        },
        AgentFrame::BundleEnd {
            run_id: run_id.clone(),
            epoch: 2,
            bytes: 4096,
            sha256: "c".repeat(64),
        },
        AgentFrame::BundleAbort {
            run_id: run_id.clone(),
            epoch: 2,
            reason: "bundle_rejected: the head does not descend".to_owned(),
        },
    ];
    let wires = [
        "bundle_start",
        "bundle_accept",
        "bundle_chunk",
        "bundle_end",
        "bundle_abort",
    ];
    for (frame, wire) in frames.iter().zip(wires) {
        let value = serde_json::to_value(frame).expect("bundle frame JSON");
        assert_eq!(value["type"], wire);
        assert_eq!(value["run_id"], run_id.as_str(), "every frame carries run_id");
        assert_eq!(value["epoch"], 2, "every frame carries epoch");
        let decoded: AgentFrame = serde_json::from_value(value).expect("bundle frame back");
        assert_eq!(&decoded, frame);
    }
}

#[test]
fn bundle_chunk_data_is_lowercase_hex_and_capped() {
    assert!(is_bundle_chunk_data(""));
    assert!(is_bundle_chunk_data("00ff"));
    assert!(!is_bundle_chunk_data("00FF"), "uppercase is not the wire spelling");
    assert!(!is_bundle_chunk_data("abc"), "an odd length cannot be hex bytes");
    assert!(!is_bundle_chunk_data("zz"));
    assert!(is_bundle_chunk_data(&"ab".repeat(MAX_BUNDLE_CHUNK_BYTES)));
    assert!(!is_bundle_chunk_data(&"ab".repeat(MAX_BUNDLE_CHUNK_BYTES + 1)));
}

#[test]
fn run_reject_reason_covers_the_documented_values() {
    let cases = [
        (RunRejectReason::NodeInactive, "node_inactive"),
        (RunRejectReason::ProjectUnmapped, "project_unmapped"),
        (RunRejectReason::PathMissing, "path_missing"),
        (RunRejectReason::PathNotWritable, "path_not_writable"),
        (RunRejectReason::CwdOutsideAllow, "cwd_outside_allow"),
        (RunRejectReason::HarnessMissing, "harness_missing"),
        (RunRejectReason::EnvMissing, "env_missing"),
        (RunRejectReason::DiskLow, "disk_low"),
        (RunRejectReason::WorktreeConflict, "worktree_conflict"),
        (RunRejectReason::OriginMismatch, "origin_mismatch"),
        (RunRejectReason::Other, "other"),
    ];
    assert_eq!(cases.len(), 11, "every run reject reason is covered");
    for (reason, wire) in cases {
        assert_eq!(serde_json::to_value(reason).expect("reason JSON"), json!(wire));
        let decoded: RunRejectReason = serde_json::from_value(json!(wire)).expect("reason back");
        assert_eq!(decoded, reason);
    }

    let err = serde_json::from_value::<RunRejectReason>(json!("node_asleep")).expect_err("unknown reason");
    assert!(err.to_string().contains("node_asleep"), "error names the value: {err}");
}

#[test]
fn resume_frame_round_trips_runs_and_surfaces() {
    let run = RunResume {
        run_id: RunId::new(),
        epoch: 3,
        last_acked: 41,
    };
    let surface = SurfaceResume {
        surface: SurfaceId::new(),
        last_acked: 17,
    };
    let resume = AgentFrame::Resume {
        runs: vec![run.clone()],
        surfaces: vec![surface.clone()],
    };
    let value = serde_json::to_value(&resume).expect("resume JSON");
    assert_eq!(value["type"], "resume");
    let decoded: AgentFrame = serde_json::from_value(value).expect("resume back");
    assert_eq!(decoded, resume);

    let ok = AgentFrame::ResumeOk {
        runs: vec![RunResumeResult {
            run_id: run.run_id,
            epoch: run.epoch,
            accepted: true,
            reason: None,
            tail_seq: 42,
        }],
        surfaces: vec![SurfaceResumeResult {
            surface: surface.surface,
            accepted: false,
            reason: Some("surface was revoked".to_owned()),
            tail_seq: 18,
        }],
    };
    let value = serde_json::to_value(&ok).expect("resume ok JSON");
    assert_eq!(value["type"], "resume_ok");
    let decoded: AgentFrame = serde_json::from_value(value).expect("resume ok back");
    assert_eq!(decoded, ok);
}

#[test]
fn path_map_update_round_trips_an_agent_path_map() {
    let map = AgentPathMap {
        project: project("vowp"),
        local_root: "/Users/me/vowp".to_owned(),
        worktree_root: Some("/Users/me/vowp-wt".to_owned()),
        allow: vec!["src".to_owned(), "docs".to_owned()],
    };
    let frame = AgentFrame::PathMapUpdate { map: map.clone() };
    let value = serde_json::to_value(&frame).expect("path map frame JSON");
    assert_eq!(value["type"], "path_map_update");
    assert_eq!(value["map"]["project"], "vowp");
    let decoded: AgentFrame = serde_json::from_value(value).expect("path map frame back");
    assert_eq!(decoded, frame);
    assert_eq!(decoded, AgentFrame::PathMapUpdate { map });
}

#[test]
fn run_state_reports_the_remote_state_and_head_commit() {
    let run_id = RunId::new();
    for (state, wire) in [
        (RemoteRunState::Running, "running"),
        (RemoteRunState::Disconnected, "disconnected"),
        (RemoteRunState::Finishing, "finishing"),
    ] {
        let frame = AgentFrame::RunState {
            run_id: run_id.clone(),
            epoch: 2,
            state,
            spool_bytes: 4096,
            head_commit: Some("abc1234".to_owned()),
            detail: "pushed job branch".to_owned(),
        };
        let value = serde_json::to_value(&frame).expect("run state JSON");
        assert_eq!(value["type"], "run_state");
        assert_eq!(value["state"], wire);
        let decoded: AgentFrame = serde_json::from_value(value).expect("run state back");
        assert_eq!(decoded, frame);
    }
}
