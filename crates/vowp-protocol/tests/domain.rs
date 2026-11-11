//! Serde round-trips for the remaining domain rows: run, worker, session, switch, inbox,
//! loop, schedule, ledger, node, project and release. Spec: `docs/design/types.md` §9–10 and
//! the examples in §15.

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use vowp_protocol::inbox::{InboxItem, InboxKind, InboxState};
use vowp_protocol::job::Edge;
use vowp_protocol::loops::{ActiveState, Loop, LoopKind, Schedule, ScheduleAction, ScheduleSpecKind};
use vowp_protocol::release::{Release, ReleaseState};
use vowp_protocol::run::{Run, RunOutcome, RunPurpose, RunRole, Worker, WorkerKind, WorkerState};
use vowp_protocol::session::{
    LedgerEntry, LedgerKind, Node, Project, Session, SessionRole, SessionState, Switch, SwitchState,
};
use vowp_protocol::{Executor, Timestamp};

fn roundtrip<T>(raw: Value) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let value: T = serde_json::from_value(raw.clone()).expect("fixture decodes");
    let encoded = serde_json::to_value(&value).expect("value is serialisable");
    assert_eq!(encoded, raw, "serialised value equals the fixture");
    let again: T = serde_json::from_value(encoded).expect("roundtrip");
    assert_eq!(again, value);
    value
}

#[test]
fn domain_run_and_worker_rows_serde() {
    let run: Run = roundtrip(json!({
        "id": "01KA1V30E5H8K2M4N6P8Q0R2S4",
        "job_id": "sample-job",
        "session_id": "01KA1V0Z7A0000000000000000",
        "role": "worker",
        "purpose": "start",
        "profile": "worker",
        "rung": 0,
        "executor": "opencode:example/model#max",
        "provider": "example",
        "node": "vps",
        "native_session_id": "ses_example000000000000000",
        "pid": 4242,
        "window": "job-sample-job",
        "cwd": "/home/user/project",
        "started_at": "2026-03-10T20:01:00.000Z",
        "ended_at": "2026-03-10T20:19:30.000Z",
        "exit_code": 0,
        "outcome": "ok",
        "limit_kind": null,
        "cost": {"usd": 0.42, "tokens": {"input": 81000, "output": 9100, "reasoning": 0, "cache_read": 240000, "cache_write": 0}, "source": "receipt"},
        "detail": ""
    }));
    assert_eq!(run.role, RunRole::Worker);
    assert_eq!(run.purpose, RunPurpose::Start);
    assert_eq!(run.outcome, Some(RunOutcome::Ok));
    assert_eq!(
        run.executor.as_ref().map(Executor::to_string).as_deref(),
        Some("opencode:example/model#max")
    );

    roundtrip::<Worker>(json!({
        "node": "vps",
        "slot": 1,
        "kind": "worker",
        "state": "busy",
        "run_id": "01KA1V30E5H8K2M4N6P8Q0R2S4",
        "job_id": "sample-job",
        "window": "job-sample-job",
        "updated_at": "2026-03-10T20:05:00.000Z"
    }));
    assert_eq!(WorkerKind::Verifier, WorkerKind::Verifier);
    assert_eq!(WorkerState::Draining, WorkerState::Draining);
}

#[test]
fn domain_session_and_switch_rows_serde() {
    let session: Session = roundtrip(json!({
        "id": "01KA1V0Z7A0000000000000000",
        "project": "vowp",
        "job_id": null,
        "role": "user",
        "title": "design chat",
        "parent_session_id": null,
        "node": "local",
        "cwd": "/home/user/project",
        "state": "idle",
        "executor": "claude-code:example-model@medium",
        "last_seq": 42,
        "last_capsule_id": null,
        "log_bytes": 18311,
        "created_at": "2026-03-10T19:30:00.000Z",
        "updated_at": "2026-03-10T19:41:02.000Z"
    }));
    assert_eq!(session.role, Some(SessionRole::User));
    assert_eq!(session.state, SessionState::Idle);

    let switch: Switch = roundtrip(json!({
        "id": "01KA1V70J9N3Q6R8S0T2V4W6X8",
        "session_id": "01KA1V0Z7A0000000000000000",
        "from_executor": "claude-code:example-model@medium",
        "to_executor": "opencode:example/model#max",
        "from_node": "local",
        "to_node": "vps",
        "capsule_id": "cap_01KA1V2B000000000000000000",
        "reason": "limit",
        "state": "completed",
        "error": null,
        "started_at": "2026-03-10T19:41:03.000Z",
        "ended_at": "2026-03-10T19:41:08.000Z",
        "at_seq": 142
    }));
    assert_eq!(switch.state, SwitchState::Completed);
    assert_eq!(switch.reason, vowp_protocol::SwitchReason::Limit);
}

#[test]
fn domain_inbox_loop_schedule_rows_serde() {
    let item: InboxItem = roundtrip(json!({
        "id": "01KA1V40F6J9M3N5P7Q9R1S3T5",
        "project": "vowp",
        "job_id": "sample-release",
        "kind": "approval",
        "title": "Promote abc1234 to prod?",
        "body": "Guards passed; diff equals the intended change.",
        "options": ["approve", "reject"],
        "state": "open",
        "answer": null,
        "answered_by": null,
        "created_at": "2026-03-10T23:10:00.000Z",
        "answered_at": null
    }));
    assert_eq!(item.kind, InboxKind::Approval);
    assert_eq!(item.state, InboxState::Open);

    let lp: Loop = roundtrip(json!({
        "name": "l4-planner",
        "kind": "L4",
        "project": "vowp",
        "interval_secs": 1800,
        "state": "active",
        "bounds": {"same_action_max": 3},
        "last_run_at": "2026-03-10T22:30:00.000Z",
        "next_run_at": "2026-03-10T23:00:00.000Z",
        "last_status": "ok",
        "fail_streak": 0,
        "updated_at": "2026-03-10T22:30:01.000Z"
    }));
    assert_eq!(lp.kind, LoopKind::L4);
    assert_eq!(lp.state, ActiveState::Active);
    assert_eq!(lp.bounds.same_action_max, Some(3));
    assert_eq!(lp.bounds.stall_minutes, None);

    let schedule: Schedule = roundtrip(json!({
        "id": "01KA1V60H8M2P5Q7R9S1T3V5W7",
        "project": "vowp",
        "name": "morning-report",
        "spec_kind": "cron",
        "spec": "0 7 * * 1-5",
        "tz": "+02:00",
        "action": {"kind": "loop", "loop_name": "l4-planner"},
        "missed_policy": "run_once",
        "state": "active",
        "last_run_at": null,
        "next_run_at": "2026-03-11T05:00:00.000Z",
        "last_status": "ok",
        "created_at": "2026-03-10T19:00:00.000Z"
    }));
    assert_eq!(schedule.spec_kind, ScheduleSpecKind::Cron);
    assert_eq!(
        schedule.action,
        ScheduleAction::Loop {
            loop_name: "l4-planner".to_owned()
        }
    );

    let command = ScheduleAction::Command {
        argv: vec!["vowp".to_owned(), "backup".to_owned()],
        cwd: None,
        timeout_secs: 1800,
    };
    assert_eq!(
        serde_json::to_value(&command).unwrap(),
        json!({"kind": "command", "argv": ["vowp", "backup"], "cwd": null, "timeout_secs": 1800})
    );
    assert_eq!(
        serde_json::from_value::<ScheduleAction>(json!({"kind": "command", "argv": ["vowp", "backup"]})).unwrap(),
        command
    );
}

#[test]
fn domain_ledger_node_project_release_rows_serde() {
    let entry: LedgerEntry = roundtrip(json!({
        "id": 7,
        "ts": "2026-03-10T20:05:00.000Z",
        "kind": "cost",
        "project": "vowp",
        "job_id": "sample-job",
        "run_id": "01KA1V30E5H8K2M4N6P8Q0R2S4",
        "session_id": null,
        "role": "worker",
        "provider": "example",
        "harness": "opencode",
        "model": "example/model",
        "cost": {"usd": 0.42, "tokens": {"input": 81000, "output": 9100, "reasoning": 0, "cache_read": 240000, "cache_write": 0}, "source": "receipt"},
        "summary": "worker run cost",
        "detail": {"rung": 0}
    }));
    assert_eq!(entry.kind, LedgerKind::Cost);

    let node: Node = roundtrip(json!({
        "name": "home-node",
        "ssh_target": "vowp@home.example",
        "is_local": false,
        "capabilities": {"gpu": false},
        "last_seen_at": "2026-03-10T20:00:00.000Z",
        "created_at": "2026-03-10T19:00:00.000Z"
    }));
    assert_eq!(node.name.as_str(), "home-node");

    let project: Project = roundtrip(json!({
        "name": "vowp",
        "path": "/home/user/project",
        "remote_url": "git@example.com:user/project.git",
        "default_branch": "main",
        "config": {"node": {"name": "local"}},
        "created_at": "2026-03-10T19:00:00.000Z",
        "updated_at": "2026-03-10T20:00:00.000Z"
    }));
    assert_eq!(project.default_branch, "main");

    let release: Release = roundtrip(json!({
        "id": "01KA1V50G7K1N4P6Q8R0S2T4V6",
        "project": "vowp",
        "env": "prod",
        "change_ref": "abc1234",
        "from_commit": "def5678",
        "to_commit": "abc1234",
        "state": "live",
        "guards": [
            {"name": "clean_tree", "passed": true, "detail": ""},
            {"name": "head_equals_remote", "passed": true, "detail": "abc1234"}
        ],
        "snapshot_path": "/home/user/.vowp/banks/prod-20261108.tar",
        "rollback_cmd": "./scripts/rollback.sh /home/user/.vowp/banks/prod-20261108.tar",
        "probe": {"status": 200},
        "approved_by": "owner",
        "rolled_back_by": null,
        "note": "",
        "created_at": "2026-03-10T23:12:00.000Z",
        "finished_at": "2026-03-10T23:14:00.000Z"
    }));
    assert_eq!(release.state, ReleaseState::Live);
    assert_eq!(release.guards.len(), 2);
}

#[test]
fn domain_edge_and_timestamp_helpers_serde() {
    let edge: Edge = roundtrip(json!({
        "src": "sample-job",
        "dst": "sample-need",
        "kind": "needs"
    }));
    assert_eq!(edge.src.as_str(), "sample-job");

    let ts = Timestamp::parse("2026-03-10T20:03:00.000Z").unwrap();
    assert_eq!(serde_json::to_value(ts).unwrap(), json!("2026-03-10T20:03:00.000Z"));
}
