//! v0.4 agent protocol: path refs, frames, node cards and schema coverage.
//! Spec: `docs/design/v0.4/local-orchestrator.md` §8.3 and §11.3.

use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use vowp_protocol::agent::{
    AGENT_PROTOCOL, AgentCapabilities, AgentCapacity, AgentFrame, AgentPathMap, AgentRole, LeaseMode, MAX_PATH_REF_LEN,
    NodeCard, NodeKind, NodePresence, NodeStatus, PathRef, SurfaceInfo, SurfaceScope, SurfaceState, TurnSummary,
};
use vowp_protocol::cost::Cost;
use vowp_protocol::error::ErrorKind;
use vowp_protocol::event::StopReason;
use vowp_protocol::executor::Executor;
use vowp_protocol::ids::{EventId, JobId, NodeName, ProjectName, RunId, SessionId};
use vowp_protocol::offload::{AssignmentPayload, RemoteRunState, RunRejectReason};
use vowp_protocol::run::{RunOutcome, RunPurpose};
use vowp_protocol::schema::{all_schemas, schema_for_name};
use vowp_protocol::surface::{
    OrchestratorApp, OrchestratorCommand, OrchestratorError, OrchestratorNodeState, OrchestratorResult, RemoteLine,
    SurfaceId, SurfaceKind,
};
use vowp_protocol::time::Timestamp;
use vowp_protocol::vault::SecretName;

/// Parses a project name in test fixtures.
fn project(name: &str) -> ProjectName {
    ProjectName::parse(name).expect("valid project name")
}

/// Parses a job id in test fixtures.
fn job(name: &str) -> JobId {
    JobId::parse(name).expect("valid job id")
}

/// A fixed timestamp for frame fixtures.
fn at() -> Timestamp {
    Timestamp::from_unix_secs(1_700_000_000)
}

/// Builds capabilities with names only.
fn capabilities() -> AgentCapabilities {
    let mut harnesses = BTreeMap::new();
    harnesses.insert("opencode".to_owned(), "1.2.3".to_owned());
    AgentCapabilities {
        os: "darwin".to_owned(),
        arch: "arm64".to_owned(),
        agent_version: "0.4.0".to_owned(),
        harnesses,
        git_version: Some("git version 2.45.0".to_owned()),
        labels: vec!["darwin-arm64".to_owned()],
        secret_names: vec![SecretName::parse("HOME_GITHUB_TOKEN").expect("secret name")],
        projects: vec![project("vowp")],
    }
}

/// Builds a capacity snapshot.
fn capacity() -> AgentCapacity {
    AgentCapacity {
        cpu_cores: 12,
        load1: 1.5,
        mem_total_bytes: 36 * 1024 * 1024 * 1024,
        mem_free_bytes: 18 * 1024 * 1024 * 1024,
        disk_free_bytes: 200 * 1024 * 1024 * 1024,
        busy_slots: 1,
        max_slots: 4,
        on_battery: Some(true),
        thermal_state: Some("nominal".to_owned()),
    }
}

/// Builds one path map entry.
fn path_map() -> AgentPathMap {
    AgentPathMap {
        project: project("vowp"),
        local_root: "/Users/me/vowp".to_owned(),
        worktree_root: Some("/Users/me/vowp-wt".to_owned()),
        allow: vec!["src".to_owned(), "docs".to_owned()],
    }
}

/// Builds a token-free assignment payload.
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
        result_transport: vowp_protocol::config::ResultTransport::Push,
        env_names: vec!["CI".to_owned()],
        secret_names: vec![SecretName::parse("HOME_GITHUB_TOKEN").expect("secret name")],
        epoch: 1,
        deadline: Some(at()),
        spool_cap_bytes: 512 * 1024 * 1024,
    }
}

#[test]
fn path_ref_parses_project_worktree_and_subdir() {
    let project_ref = PathRef::parse("project:vowp").expect("project ref");
    assert_eq!(
        project_ref,
        PathRef::Project {
            project: project("vowp")
        }
    );
    assert_eq!(project_ref.to_string(), "project:vowp");

    let worktree_ref = PathRef::parse("worktree:vowp/graph-engine").expect("worktree ref");
    assert_eq!(
        worktree_ref,
        PathRef::Worktree {
            project: project("vowp"),
            job: job("graph-engine"),
        }
    );
    assert_eq!(worktree_ref.to_string(), "worktree:vowp/graph-engine");

    let subdir_ref = PathRef::parse("subdir:vowp/src/lib").expect("subdir ref");
    assert_eq!(
        subdir_ref,
        PathRef::Subdir {
            project: project("vowp"),
            rel: "src/lib".to_owned(),
        }
    );
    assert_eq!(subdir_ref.to_string(), "subdir:vowp/src/lib");

    let normalised = PathRef::parse("subdir:vowp/src//./lib").expect("normalised ref");
    assert_eq!(normalised.to_string(), "subdir:vowp/src/lib");
    assert_eq!("project:vowp".parse::<PathRef>().expect("from_str"), project_ref);
}

#[test]
fn path_ref_rejects_traversal_absolute_backslash_and_nul() {
    let too_long = format!("subdir:vowp/{}", "a".repeat(MAX_PATH_REF_LEN));
    let bad = [
        "subdir:vowp/../etc",
        "subdir:vowp/src/../../etc",
        "worktree:vowp/../job",
        "subdir:vowp//etc",
        "project:/vowp",
        "subdir:vowp/src\\lib",
        "subdir:vowp/src\0lib",
        "project:",
        "worktree:vowp/",
        "subdir:vowp/",
        "subdir:vowp/./.",
        "unknown:vowp",
        too_long.as_str(),
    ];
    for value in bad {
        let err = PathRef::parse(value).expect_err("invalid path ref must fail");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "error kind for {value:?}");
    }
}

#[test]
fn path_ref_round_trips_through_json() {
    for wire in ["project:vowp", "worktree:vowp/graph-engine", "subdir:vowp/src/lib"] {
        let parsed = PathRef::parse(wire).expect("path ref");
        let encoded = serde_json::to_string(&parsed).expect("path ref JSON");
        assert_eq!(encoded, format!("{wire:?}"));
        let decoded: PathRef = serde_json::from_str(&encoded).expect("path ref back");
        assert_eq!(decoded, parsed);
    }

    let err = serde_json::from_str::<PathRef>("\"subdir:vowp/../etc\"").expect_err("traversal JSON must fail");
    assert!(err.to_string().contains("path_ref"));
}

#[test]
fn agent_frame_round_trips_every_phase_zero_variant() {
    let frames = vec![
        (
            "agent_hello",
            AgentFrame::AgentHello {
                node: NodeName::parse("mac-studio").expect("node"),
                key_id: "ed25519:abc".to_owned(),
                agent_version: "0.4.0".to_owned(),
                os: "darwin".to_owned(),
                arch: "arm64".to_owned(),
                labels: vec!["darwin-arm64".to_owned()],
                capabilities: capabilities(),
            },
        ),
        (
            "home_challenge",
            AgentFrame::HomeChallenge {
                nonce: "nonce".to_owned(),
                home_id: "home-1".to_owned(),
                connection_id: "conn-1".to_owned(),
                server_time: at(),
                heartbeat_secs: 10,
                max_frame_bytes: 1024 * 1024,
            },
        ),
        (
            "agent_auth",
            AgentFrame::AgentAuth {
                signature: "ab".repeat(64),
            },
        ),
        (
            "home_welcome",
            AgentFrame::HomeWelcome {
                protocol: AGENT_PROTOCOL.to_owned(),
                home_id: "home-1".to_owned(),
                node: NodeName::parse("mac-studio").expect("node"),
                connection_id: "conn-1".to_owned(),
            },
        ),
        (
            "enroll",
            AgentFrame::Enroll {
                code: "one-time-code".to_owned(),
                public_key: "cd".repeat(32),
                agent_version: "0.4.0".to_owned(),
                os: "darwin".to_owned(),
                arch: "arm64".to_owned(),
                labels: vec!["darwin-arm64".to_owned()],
                capabilities: capabilities(),
                paths: vec![path_map()],
            },
        ),
        (
            "enrolled",
            AgentFrame::Enrolled {
                home_id: "home-1".to_owned(),
                node: NodeName::parse("mac-studio").expect("node"),
                protocol: AGENT_PROTOCOL.to_owned(),
                server_time: at(),
                heartbeat_secs: 10,
            },
        ),
        (
            "heartbeat",
            AgentFrame::Heartbeat {
                at: at(),
                capacity: capacity(),
                pending_spool_bytes: 4096,
            },
        ),
        ("ping", AgentFrame::Ping),
        ("pong", AgentFrame::Pong),
        (
            "revoked",
            AgentFrame::Revoked {
                reason: "owner revoked the node".to_owned(),
            },
        ),
        (
            "superseded",
            AgentFrame::Superseded {
                connection_id: "conn-2".to_owned(),
            },
        ),
        (
            "error",
            AgentFrame::Error {
                kind: ErrorKind::Conflict,
                message: "already enrolled".to_owned(),
                retryable: false,
            },
        ),
    ];

    assert_eq!(frames.len(), 12, "every Phase 0 frame variant is covered");
    assert_eq!(serde_json::to_value(AgentRole::Both).expect("role JSON"), json!("both"));
    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        let decoded: AgentFrame = serde_json::from_value(value).expect("frame back");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn agent_frame_round_trips_surface_pair_frames() {
    let surface_id = SurfaceId::new();
    let frames = vec![
        (
            "surface_pair",
            AgentFrame::SurfacePair {
                code: "one-time-pairing-code".to_owned(),
                surface_id: surface_id.clone(),
                session: SessionId::new(),
                app: "opencode".to_owned(),
            },
        ),
        (
            "surface_issued",
            AgentFrame::SurfaceIssued {
                surface_id: surface_id.clone(),
                token: "vowp_surf_abcdef".to_owned(),
                scopes: vec![
                    SurfaceScope::new("session.read:01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                    SurfaceScope::new("fleet.read"),
                ],
                expires_at: at(),
            },
        ),
        (
            "surface_revoked",
            AgentFrame::SurfaceRevoked {
                surface_id: surface_id.clone(),
                reason: "owner revoked the surface".to_owned(),
            },
        ),
    ];

    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("surface frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        let decoded: AgentFrame = serde_json::from_value(value).expect("surface frame back");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn agent_frame_round_trips_lease_frames() {
    let surface_id = SurfaceId::new();
    let session = SessionId::new();
    let frames = vec![
        (
            "lease_acquire",
            AgentFrame::LeaseAcquire {
                surface: surface_id.clone(),
                session: session.clone(),
                surface_token: "vowp_surf_abcdef".to_owned(),
                mode: LeaseMode::Acquire,
            },
        ),
        (
            "lease_heartbeat",
            AgentFrame::LeaseHeartbeat {
                surface: surface_id.clone(),
                session: session.clone(),
                token: "A".repeat(52),
            },
        ),
        (
            "lease_release",
            AgentFrame::LeaseRelease {
                surface: surface_id.clone(),
                session: session.clone(),
                token: "B".repeat(52),
            },
        ),
        (
            "lease_grant",
            AgentFrame::LeaseGrant {
                lease_token: "C".repeat(52),
                expires_at: at(),
                tail_seq: 42,
            },
        ),
        (
            "lease_lost",
            AgentFrame::LeaseLost {
                reason: "lease expired".to_owned(),
            },
        ),
        (
            "lease_revoked",
            AgentFrame::LeaseRevoked {
                reason: "session taken over".to_owned(),
                holder: "mac opencode".to_owned(),
            },
        ),
    ];

    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("lease frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        let decoded: AgentFrame = serde_json::from_value(value).expect("lease frame back");
        assert_eq!(decoded, frame);
    }

    assert_eq!(
        serde_json::to_value(LeaseMode::Acquire).expect("acquire JSON"),
        json!("acquire")
    );
    assert_eq!(
        serde_json::to_value(LeaseMode::Takeover).expect("takeover JSON"),
        json!("takeover")
    );
    let err = serde_json::from_value::<AgentFrame>(json!({
        "type": "lease_acquire",
        "surface": surface_id,
        "session": session,
        "surface_token": "vowp_surf_abcdef",
        "mode": "steal",
    }))
    .expect_err("unknown lease mode must fail");
    assert!(err.to_string().contains("steal"), "error names the mode: {err}");
}

#[test]
fn agent_frame_round_trips_local_chat_frames() {
    let surface_id = SurfaceId::new();
    let session = SessionId::new();
    let run = RunId::new();
    let event_id = EventId::new();
    let frames = vec![
        (
            "turn_begin",
            AgentFrame::TurnBegin {
                surface: surface_id.clone(),
                session: session.clone(),
                lease_token: "D".repeat(52),
                prompt: "summarise the design note".to_owned(),
                executor: "opencode:deepseek/deepseek-flash#max".to_owned(),
                cwd_ref: PathRef::Worktree {
                    project: project("vowp"),
                    job: job("local-chat"),
                },
                job: Some(job("local-chat")),
                purpose: RunPurpose::Continue,
            },
        ),
        (
            "turn_grant",
            AgentFrame::TurnGrant {
                run_id: run.clone(),
                session: session.clone(),
                lease_token: "E".repeat(52),
                expires_at: at(),
                tail_seq: 42,
            },
        ),
        (
            "remote_line",
            AgentFrame::RemoteLine {
                run_id: run.clone(),
                epoch: 3,
                line: RemoteLine::Heartbeat { at: at() },
            },
        ),
        (
            "ingest_ack",
            AgentFrame::IngestAck {
                run_id: run.clone(),
                event_id: Some(event_id.clone()),
                seq: Some(43),
                duplicate: false,
                lease_revoked: None,
                error: None,
            },
        ),
        (
            "credit",
            AgentFrame::Credit {
                run_id: run.clone(),
                count: 64,
            },
        ),
        (
            "turn_end",
            AgentFrame::TurnEnd {
                run_id: run.clone(),
                summary: TurnSummary {
                    exit_code: Some(0),
                    outcome: RunOutcome::Ok,
                    native_session_id: Some("ses_mac".to_owned()),
                    cost: Cost::unknown(),
                    limit: None,
                    result_text: Some("done".to_owned()),
                    stop_reason: StopReason::EndTurn,
                    usage: None,
                },
            },
        ),
        (
            "run_lost",
            AgentFrame::RunLost {
                run_id: run.clone(),
                reason: "session was taken over by phone".to_owned(),
            },
        ),
    ];

    assert_eq!(frames.len(), 7, "every local-chat frame variant is covered");
    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("local chat frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        let decoded: AgentFrame = serde_json::from_value(value).expect("local chat frame back");
        assert_eq!(decoded, frame);
    }

    let turn_begin = serde_json::to_value(AgentFrame::TurnBegin {
        surface: surface_id,
        session,
        lease_token: "D".repeat(52),
        prompt: "prompt".to_owned(),
        executor: "opencode:deepseek/deepseek-flash#max".to_owned(),
        cwd_ref: PathRef::parse("subdir:vowp/src").expect("path ref"),
        job: None,
        purpose: RunPurpose::Start,
    })
    .expect("turn begin JSON");
    assert_eq!(turn_begin["cwd_ref"], json!("subdir:vowp/src"));
    let text = serde_json::to_string(&turn_begin).expect("turn begin text");
    assert!(!text.contains("/Users/"), "no absolute path in a frame: {text}");
}

#[test]
fn local_chat_schema_is_exported() {
    let schemas = all_schemas();
    for name in ["AgentFrame", "TurnSummary"] {
        assert!(schemas.contains_key(name), "all_schemas is missing {name}");
        assert!(schema_for_name(name).is_some(), "schema_for_name is missing {name}");
    }
    let frame = schema_for_name("AgentFrame").expect("AgentFrame schema");
    let text = serde_json::to_string(&frame).expect("schema text");
    for variant in [
        "turn_begin",
        "turn_grant",
        "remote_line",
        "ingest_ack",
        "credit",
        "turn_end",
        "run_lost",
    ] {
        assert!(text.contains(variant), "AgentFrame schema is missing {variant}: {text}");
    }
}

#[test]
fn surface_types_round_trip_and_have_stable_wire_names() {
    for (state, wire) in [
        (SurfaceState::Pending, "pending"),
        (SurfaceState::Attached, "attached"),
        (SurfaceState::Revoked, "revoked"),
    ] {
        assert_eq!(serde_json::to_value(state).expect("state JSON"), json!(wire));
        let decoded: SurfaceState = serde_json::from_value(json!(wire)).expect("state back");
        assert_eq!(decoded, state);
    }

    let scope = SurfaceScope::new("project.read:vowp");
    assert_eq!(
        serde_json::to_value(&scope).expect("scope JSON"),
        json!("project.read:vowp")
    );
    assert_eq!(scope.to_string(), "project.read:vowp");
    let decoded: SurfaceScope = serde_json::from_value(json!("project.read:vowp")).expect("scope back");
    assert_eq!(decoded, scope);

    let info = SurfaceInfo {
        id: SurfaceId::new(),
        kind: SurfaceKind::Agent,
        node: NodeName::parse("mac-studio").expect("node"),
        holder: "mac opencode".to_owned(),
        app: Some("opencode".to_owned()),
        cwd: Some("/Users/me/vowp".to_owned()),
        metadata: json!({}),
        state: SurfaceState::Attached,
        created_at: at(),
        last_seen_at: at(),
        revoked_at: None,
    };
    let value = serde_json::to_value(&info).expect("surface info JSON");
    assert_eq!(value["state"], "attached");
    let decoded: SurfaceInfo = serde_json::from_value(value).expect("surface info back");
    assert_eq!(decoded, info);
}

#[test]
fn agent_frame_rejects_unknown_type() {
    let err = serde_json::from_value::<AgentFrame>(json!({"type": "teleport"})).expect_err("unknown frame type");
    assert!(err.to_string().contains("teleport"), "error names the type: {err}");

    let unknown_surface =
        serde_json::from_value::<AgentFrame>(json!({"type": "surface_teleport"})).expect_err("unknown surface frame");
    assert!(
        unknown_surface.to_string().contains("surface_teleport"),
        "error names the type: {unknown_surface}"
    );

    let missing = serde_json::from_value::<AgentFrame>(json!({"type": "heartbeat"})).expect_err("missing fields");
    assert!(
        missing.to_string().contains("missing field"),
        "error names the field: {missing}"
    );

    let missing_pair =
        serde_json::from_value::<AgentFrame>(json!({"type": "surface_pair"})).expect_err("missing pair fields");
    assert!(
        missing_pair.to_string().contains("missing field"),
        "error names the field: {missing_pair}"
    );
}

#[test]
fn node_card_serializes_without_absolute_paths_or_secret_names() {
    let card = NodeCard {
        name: NodeName::parse("mac-studio").expect("node"),
        kind: NodeKind::Agent,
        status: NodeStatus::Active,
        presence: NodePresence::Online,
        labels: vec!["darwin-arm64".to_owned()],
        capacity: Some(capacity()),
        runs: 1,
        mapped: vec![project("vowp")],
        agent_version: Some("0.4.0".to_owned()),
        protocol: Some(AGENT_PROTOCOL.to_owned()),
        last_error: Some("connection reset".to_owned()),
    };
    let value = serde_json::to_value(&card).expect("node card JSON");
    let text = serde_json::to_string(&value).expect("node card text");

    assert!(!text.contains("/Users/"), "no absolute path: {text}");
    assert!(!text.contains("HOME_GITHUB_TOKEN"), "no secret name: {text}");
    for forbidden in ["local_root", "worktree_root", "secret_names", "paths"] {
        assert!(!text.contains(forbidden), "no {forbidden} field: {text}");
    }
    assert_no_absolute_strings(&value);

    assert_eq!(value["kind"], "agent");
    assert_eq!(value["status"], "active");
    assert_eq!(value["presence"], "online");
}

/// Recursively asserts that no JSON string looks like an absolute path.
fn assert_no_absolute_strings(value: &Value) {
    match value {
        Value::String(text) => assert!(!text.starts_with('/'), "absolute path string: {text}"),
        Value::Array(items) => items.iter().for_each(assert_no_absolute_strings),
        Value::Object(map) => map.values().for_each(assert_no_absolute_strings),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[test]
fn all_schemas_includes_agent_types() {
    let schemas = all_schemas();
    for name in [
        "PathRef",
        "AgentRole",
        "AgentCapabilities",
        "AgentCapacity",
        "AgentPathMap",
        "NodeCard",
        "NodeKind",
        "NodeStatus",
        "NodePresence",
        "SurfaceInfo",
        "SurfaceScope",
        "SurfaceState",
        "LeaseMode",
        "OrchestratorNodeState",
        "OrchestratorCommand",
        "OrchestratorResult",
        "OrchestratorError",
        "AgentFrame",
    ] {
        assert!(schemas.contains_key(name), "all_schemas is missing {name}");
        assert!(schema_for_name(name).is_some(), "schema_for_name is missing {name}");
    }
}

#[test]
fn surface_kind_agent_wire_name_is_agent() {
    assert_eq!(
        serde_json::to_value(SurfaceKind::Agent).expect("surface kind JSON"),
        json!("agent")
    );
    let decoded: SurfaceKind = serde_json::from_value(json!("agent")).expect("surface kind back");
    assert_eq!(decoded, SurfaceKind::Agent);
}

#[test]
fn agent_frame_round_trips_run_assignment_frames() {
    let assignment = assignment();
    let frames = vec![
        (
            "assign_run",
            AgentFrame::AssignRun {
                assignment: assignment.clone(),
                lease_token: "FAKE-LEASE-TOKEN-NOT-A-REAL-SECRET".to_owned(),
                lease_expires_at: at(),
            },
        ),
        (
            "run_accepted",
            AgentFrame::RunAccepted {
                assignment_id: assignment.assignment_id.clone(),
                run_id: assignment.run_id.clone(),
                epoch: assignment.epoch,
            },
        ),
        (
            "run_rejected",
            AgentFrame::RunRejected {
                assignment_id: assignment.assignment_id.clone(),
                run_id: assignment.run_id.clone(),
                epoch: assignment.epoch,
                reason: RunRejectReason::EnvMissing,
                detail: "CI is not set on the node".to_owned(),
            },
        ),
        (
            "run_state",
            AgentFrame::RunState {
                run_id: assignment.run_id.clone(),
                epoch: assignment.epoch,
                state: RemoteRunState::Finishing,
                spool_bytes: 4096,
                head_commit: Some("abc1234".to_owned()),
                detail: "pushed job branch".to_owned(),
            },
        ),
        (
            "cancel_run",
            AgentFrame::CancelRun {
                run_id: assignment.run_id.clone(),
                epoch: assignment.epoch,
                reason: "owner cancelled the job".to_owned(),
            },
        ),
        (
            "cleanup",
            AgentFrame::Cleanup {
                run_id: assignment.run_id.clone(),
                job: assignment.job.clone(),
                worktree: PathRef::Worktree {
                    project: project("vowp"),
                    job: job("graph-engine"),
                },
            },
        ),
    ];

    assert_eq!(frames.len(), 6, "every run assignment frame variant is covered");
    for (wire, frame) in frames {
        let value = serde_json::to_value(&frame).expect("run assignment frame JSON");
        assert_eq!(value["type"], wire, "wire name for {wire}");
        let decoded: AgentFrame = serde_json::from_value(value).expect("run assignment frame back");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn agent_frame_round_trips_assign_run_with_the_remote_url() {
    let payload = assignment();
    assert_eq!(payload.remote_url, "https://example.invalid/owner/repo.git");
    let frame = AgentFrame::AssignRun {
        assignment: payload.clone(),
        lease_token: "FAKE-LEASE-TOKEN-NOT-A-REAL-SECRET".to_owned(),
        lease_expires_at: at(),
    };

    let value = serde_json::to_value(&frame).expect("assign frame JSON");
    assert_eq!(value["type"], "assign_run");
    assert_eq!(value["assignment"]["remote"], json!("origin"));
    assert_eq!(
        value["assignment"]["remote_url"],
        json!("https://example.invalid/owner/repo.git")
    );

    let decoded: AgentFrame = serde_json::from_value(value).expect("assign frame back");
    assert_eq!(decoded, frame);
    let AgentFrame::AssignRun { assignment, .. } = decoded else {
        panic!("expected an AssignRun frame");
    };
    assert_eq!(assignment.remote_url, payload.remote_url);
}

#[test]
fn agent_frame_round_trips_orchestrator_commands_and_results() {
    let commands = [
        (
            "start",
            AgentFrame::OrchestratorCommand(OrchestratorCommand::Start {
                request_id: "req-start".to_owned(),
                name: "mac".to_owned(),
                app: OrchestratorApp::Opencode,
                project: Some(project("vowp")),
            }),
        ),
        (
            "stop",
            AgentFrame::OrchestratorCommand(OrchestratorCommand::Stop {
                request_id: "req-stop".to_owned(),
                name: "mac".to_owned(),
            }),
        ),
        (
            "resume",
            AgentFrame::OrchestratorCommand(OrchestratorCommand::Resume {
                request_id: "req-resume".to_owned(),
                name: "mac".to_owned(),
            }),
        ),
    ];
    for (action, frame) in commands {
        let value = serde_json::to_value(&frame).expect("orchestrator command JSON");
        assert_eq!(value["type"], "orchestrator_command");
        assert_eq!(value["action"], action);
        assert_eq!(value["name"], "mac");
        assert_no_absolute_strings(&value);
        let decoded: AgentFrame = serde_json::from_value(value).expect("orchestrator command back");
        assert_eq!(decoded, frame);
    }

    let accepted = OrchestratorResult::accepted("req-start", OrchestratorNodeState::Running);
    let frame = AgentFrame::OrchestratorResult(accepted.clone());
    let value = serde_json::to_value(&frame).expect("orchestrator result JSON");
    assert_eq!(value["type"], "orchestrator_result");
    assert_eq!(value["request_id"], "req-start");
    assert_eq!(value["accepted"], true);
    assert_eq!(value["state"], "running");
    assert!(value.get("error").is_none(), "{value}");
    assert_no_absolute_strings(&value);
    let decoded: AgentFrame = serde_json::from_value(value).expect("orchestrator result back");
    assert_eq!(decoded, frame);

    let failed = OrchestratorResult::failed(
        "req-start",
        OrchestratorNodeState::Failed,
        OrchestratorError::new("spawn_failed", "the app did not start", true),
    );
    let frame = AgentFrame::OrchestratorResult(failed);
    let value = serde_json::to_value(&frame).expect("failed orchestrator result JSON");
    assert_eq!(value["accepted"], false);
    assert_eq!(value["state"], "failed");
    assert_eq!(value["error"]["code"], "spawn_failed");
    assert_eq!(value["error"]["retryable"], true);
    assert_no_absolute_strings(&value);
    let decoded: AgentFrame = serde_json::from_value(value).expect("failed orchestrator result back");
    assert_eq!(decoded, frame);
}

#[test]
fn orchestrator_node_state_wire_names_are_stable() {
    for state in OrchestratorNodeState::ALL {
        let value = serde_json::to_value(state).expect("state JSON");
        assert_eq!(value, json!(state.as_str()));
        let decoded: OrchestratorNodeState = serde_json::from_value(value).expect("state back");
        assert_eq!(decoded, state);
        assert_eq!(state.to_string(), state.as_str());
    }
    assert_eq!(OrchestratorNodeState::NodeOffline.as_str(), "node_offline");
    assert!(serde_json::from_value::<OrchestratorNodeState>(json!("sleeping")).is_err());
}

#[test]
fn unknown_frame_type_is_rejected() {
    let err = serde_json::from_value::<AgentFrame>(json!({"type": "assign_run_teleport"}))
        .expect_err("unknown frame type must fail");
    assert!(
        err.to_string().contains("assign_run_teleport"),
        "error names the type: {err}"
    );

    let missing = serde_json::from_value::<AgentFrame>(json!({"type": "assign_run"})).expect_err("missing fields");
    assert!(
        missing.to_string().contains("missing field"),
        "error names the field: {missing}"
    );
}
